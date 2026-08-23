//! Bounded access to the MediaWiki Action API.
//!
//! The transport deliberately performs one bounded API response at a time. Callers
//! persist [`RevisionContinuation`] between calls, which keeps long histories
//! resumable without an unbounded in-memory paginator. A configured [`RetryPolicy`]
//! may repeat the same bounded request, but it has an explicit attempt ceiling and a
//! shared circuit breaker for persistent retryable failures.

mod dump;
mod media;

pub use dump::{
    DumpError, DumpFilter, DumpLimits, DumpNamespace, DumpPage, DumpReader, DumpRevision,
    DumpSiteInfo,
};
pub use media::{
    MAX_REVISION_IMAGE_REFERENCES, RevisionImagePlacement, ThumbnailDownloadError,
    ThumbnailIneligibility, ThumbnailMetadata, ThumbnailMetadataResolution, ThumbnailMimeType,
};

use std::error::Error;
use std::fmt;
use std::io;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
use std::num::NonZeroUsize;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use reqwest::dns::{Addrs, Name, Resolve, Resolving};
use reqwest::{StatusCode, Url, redirect};
use serde::Deserialize;
use serde::de::DeserializeOwned;
use tokio::sync::{Mutex as AsyncMutex, Semaphore};
use wikisync_core::{PageId, PageTitle, RevisionId};

const DEFAULT_RESPONSE_LIMIT: usize = 8 * 1024 * 1024;
const DEFAULT_RUN_DOWNLOAD_LIMIT: usize = 512 * 1024 * 1024;
const DEFAULT_MAX_CONCURRENT_REQUESTS: usize = 4;
const DEFAULT_TITLES_PER_OPERATION: usize = 1_000;
const DEFAULT_TITLES_PER_REQUEST: usize = 50;
const DEFAULT_REVISIONS_PER_REQUEST: usize = 500;
const DEFAULT_CATEGORY_MEMBERS_PER_REQUEST: usize = 500;
const DEFAULT_RETRY_ATTEMPTS: usize = 4;
const DEFAULT_CIRCUIT_FAILURE_THRESHOLD: usize = 3;
const MAX_RESOLVED_DESTINATIONS: usize = 32;

static NEXT_JITTER_SEED: AtomicU64 = AtomicU64::new(0x9e37_79b9_7f4a_7c15);

/// Bounded retry and circuit-breaker policy for one MediaWiki client.
///
/// Backoff uses equal jitter: each retry waits between half and all of its capped
/// exponential delay. A server `Retry-After` value is used as a minimum delay when
/// it is within `maximum_delay`, and is clamped to that safety ceiling otherwise.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RetryPolicy {
    maximum_attempts: NonZeroUsize,
    initial_delay: Duration,
    maximum_delay: Duration,
    circuit_failure_threshold: NonZeroUsize,
    circuit_open_duration: Duration,
}

impl RetryPolicy {
    /// Creates a policy with an explicit total-attempt ceiling and delay bounds.
    ///
    /// `maximum_attempts` includes the first request. Therefore, one disables
    /// request retries while retaining conservative error classification.
    pub fn new(
        maximum_attempts: usize,
        initial_delay: Duration,
        maximum_delay: Duration,
    ) -> Result<Self, ConfigError> {
        let maximum_attempts =
            NonZeroUsize::new(maximum_attempts).ok_or(ConfigError::ZeroLimit {
                name: "maximum retry attempts",
            })?;
        if initial_delay.is_zero() {
            return Err(ConfigError::ZeroLimit {
                name: "initial retry delay",
            });
        }
        if maximum_delay.is_zero() {
            return Err(ConfigError::ZeroLimit {
                name: "maximum retry delay",
            });
        }
        if initial_delay > maximum_delay {
            return Err(ConfigError::InvalidRange {
                smaller: "initial retry delay",
                larger: "maximum retry delay",
            });
        }
        Ok(Self {
            maximum_attempts,
            initial_delay,
            maximum_delay,
            circuit_failure_threshold: nonzero(DEFAULT_CIRCUIT_FAILURE_THRESHOLD),
            circuit_open_duration: Duration::from_secs(60),
        })
    }

    /// Configures how many exhausted retryable operations open the circuit and for
    /// how long new operations are rejected without contacting the source.
    pub fn with_circuit_breaker(
        mut self,
        failure_threshold: usize,
        open_duration: Duration,
    ) -> Result<Self, ConfigError> {
        self.circuit_failure_threshold =
            NonZeroUsize::new(failure_threshold).ok_or(ConfigError::ZeroLimit {
                name: "circuit-breaker failure threshold",
            })?;
        if open_duration.is_zero() {
            return Err(ConfigError::ZeroLimit {
                name: "circuit-breaker open duration",
            });
        }
        self.circuit_open_duration = open_duration;
        Ok(self)
    }
}

impl Default for RetryPolicy {
    fn default() -> Self {
        Self::new(
            DEFAULT_RETRY_ATTEMPTS,
            Duration::from_millis(250),
            Duration::from_secs(30),
        )
        .expect("default retry policy is valid")
    }
}

/// Limits and source identity used by a [`MediaWikiClient`].
#[derive(Clone, Debug)]
pub struct ClientConfig {
    endpoint: Url,
    allowed_source_hosts: Vec<String>,
    destination_policy: DestinationPolicy,
    user_agent: String,
    request_timeout: Duration,
    connect_timeout: Duration,
    max_response_bytes: NonZeroUsize,
    max_downloaded_response_bytes_per_run: NonZeroUsize,
    max_downloaded_response_bytes_per_second: Option<NonZeroUsize>,
    max_concurrent_requests: NonZeroUsize,
    max_redirects: usize,
    max_lag_seconds: u16,
    max_titles_per_operation: NonZeroUsize,
    titles_per_request: NonZeroUsize,
    revisions_per_request: NonZeroUsize,
    category_members_per_request: NonZeroUsize,
    retry_policy: RetryPolicy,
}

impl ClientConfig {
    /// Creates a client configuration for an HTTPS Action API endpoint.
    ///
    /// Plain HTTP is accepted only for loopback hosts so fixture servers can exercise
    /// the real transport without weakening remote-source validation.
    pub fn new(endpoint: &str, user_agent: impl Into<String>) -> Result<Self, ConfigError> {
        let endpoint = Url::parse(endpoint)
            .map_err(|error| ConfigError::InvalidEndpoint(error.to_string()))?;
        validate_endpoint(&endpoint)?;

        let user_agent = user_agent.into();
        if user_agent.trim().is_empty() || user_agent.chars().any(char::is_control) {
            return Err(ConfigError::InvalidUserAgent);
        }

        Ok(Self {
            allowed_source_hosts: vec![
                endpoint
                    .host_str()
                    .expect("validated endpoint has a host")
                    .to_ascii_lowercase(),
            ],
            destination_policy: DestinationPolicy::for_endpoint(&endpoint),
            endpoint,
            user_agent,
            request_timeout: Duration::from_secs(30),
            connect_timeout: Duration::from_secs(10),
            max_response_bytes: nonzero(DEFAULT_RESPONSE_LIMIT),
            max_downloaded_response_bytes_per_run: nonzero(DEFAULT_RUN_DOWNLOAD_LIMIT),
            max_downloaded_response_bytes_per_second: None,
            max_concurrent_requests: nonzero(DEFAULT_MAX_CONCURRENT_REQUESTS),
            max_redirects: 3,
            max_lag_seconds: 5,
            max_titles_per_operation: nonzero(DEFAULT_TITLES_PER_OPERATION),
            titles_per_request: nonzero(DEFAULT_TITLES_PER_REQUEST),
            revisions_per_request: nonzero(DEFAULT_REVISIONS_PER_REQUEST),
            category_members_per_request: nonzero(DEFAULT_CATEGORY_MEMBERS_PER_REQUEST),
            retry_policy: RetryPolicy::default(),
        })
    }

    /// Sets the maximum accepted response body size after decompression.
    pub fn with_max_response_bytes(mut self, bytes: usize) -> Result<Self, ConfigError> {
        self.max_response_bytes = NonZeroUsize::new(bytes).ok_or(ConfigError::ZeroLimit {
            name: "max response bytes",
        })?;
        Ok(self)
    }

    /// Sets the aggregate response-body byte budget shared by this client's clones.
    ///
    /// Constructing a new client establishes a new run boundary. Every received body
    /// chunk, including bodies from retry attempts, consumes this budget.
    pub fn with_max_downloaded_response_bytes_per_run(
        mut self,
        bytes: usize,
    ) -> Result<Self, ConfigError> {
        self.max_downloaded_response_bytes_per_run =
            NonZeroUsize::new(bytes).ok_or(ConfigError::ZeroLimit {
                name: "max downloaded response bytes per run",
            })?;
        Ok(self)
    }

    /// Sets an optional aggregate response-body rate shared by this client's clones.
    ///
    /// The rate applies to every response-body chunk, including bodies received from
    /// retry attempts. `None` leaves downloads unlimited. A configured limiter keeps
    /// at most one second of unused capacity, so idle time cannot create an unbounded
    /// later burst.
    pub fn with_max_downloaded_response_bytes_per_second(
        mut self,
        bytes_per_second: Option<usize>,
    ) -> Result<Self, ConfigError> {
        self.max_downloaded_response_bytes_per_second = bytes_per_second
            .map(|bytes_per_second| {
                NonZeroUsize::new(bytes_per_second).ok_or(ConfigError::ZeroLimit {
                    name: "max downloaded response bytes per second",
                })
            })
            .transpose()?;
        Ok(self)
    }

    /// Returns the aggregate response-body rate, or `None` when it is unlimited.
    #[must_use]
    pub fn max_downloaded_response_bytes_per_second(&self) -> Option<usize> {
        self.max_downloaded_response_bytes_per_second
            .map(NonZeroUsize::get)
    }

    /// Sets the maximum number of in-flight HTTP requests shared by client clones.
    pub fn with_max_concurrent_requests(mut self, count: usize) -> Result<Self, ConfigError> {
        self.max_concurrent_requests = NonZeroUsize::new(count).ok_or(ConfigError::ZeroLimit {
            name: "max concurrent requests",
        })?;
        Ok(self)
    }

    /// Returns the normalized singleton source-host allowlist.
    ///
    /// Redirects must remain on the endpoint's complete origin in addition to
    /// matching this allowlist; a same-host scheme or port change is rejected.
    pub fn allowed_source_hosts(&self) -> impl ExactSizeIterator<Item = &str> {
        self.allowed_source_hosts.iter().map(String::as_str)
    }

    /// Sets the maximum number of input titles accepted by one operation.
    pub fn with_max_titles_per_operation(mut self, count: usize) -> Result<Self, ConfigError> {
        self.max_titles_per_operation = NonZeroUsize::new(count).ok_or(ConfigError::ZeroLimit {
            name: "max titles per operation",
        })?;
        Ok(self)
    }

    /// Sets the request and connect timeouts.
    pub fn with_timeouts(
        mut self,
        request_timeout: Duration,
        connect_timeout: Duration,
    ) -> Result<Self, ConfigError> {
        if request_timeout.is_zero() {
            return Err(ConfigError::ZeroLimit {
                name: "request timeout",
            });
        }
        if connect_timeout.is_zero() {
            return Err(ConfigError::ZeroLimit {
                name: "connect timeout",
            });
        }
        self.request_timeout = request_timeout;
        self.connect_timeout = connect_timeout;
        Ok(self)
    }

    /// Returns the configured Action API endpoint.
    #[must_use]
    pub fn endpoint(&self) -> &Url {
        &self.endpoint
    }

    /// Sets the number of category members requested in one bounded response.
    ///
    /// MediaWiki caps this at 500 for normal users. A smaller value is useful for
    /// constrained clients and deterministic continuation tests.
    pub fn with_category_members_per_request(mut self, count: usize) -> Result<Self, ConfigError> {
        if !(1..=500).contains(&count) {
            return Err(ConfigError::InvalidLimit {
                name: "category members per request",
                maximum: 500,
            });
        }
        self.category_members_per_request = nonzero(count);
        Ok(self)
    }

    /// Sets the bounded retry and shared circuit-breaker policy.
    #[must_use]
    pub fn with_retry_policy(mut self, policy: RetryPolicy) -> Self {
        self.retry_policy = policy;
        self
    }
}

fn nonzero(value: usize) -> NonZeroUsize {
    NonZeroUsize::new(value).expect("constant limit is non-zero")
}

fn validate_endpoint(endpoint: &Url) -> Result<(), ConfigError> {
    if endpoint.cannot_be_a_base()
        || endpoint.host_str().is_none()
        || !endpoint.username().is_empty()
        || endpoint.password().is_some()
        || endpoint.query().is_some()
        || endpoint.fragment().is_some()
    {
        return Err(ConfigError::UnsafeEndpoint);
    }

    match endpoint.scheme() {
        "https" if endpoint_host_is_statically_unsafe(endpoint) => {
            Err(ConfigError::UnsafeDestination)
        }
        "https" => Ok(()),
        "http" if is_loopback(endpoint) => Ok(()),
        _ => Err(ConfigError::HttpsRequired),
    }
}

fn endpoint_host_is_statically_unsafe(endpoint: &Url) -> bool {
    endpoint.host_str().is_some_and(|host| {
        host.eq_ignore_ascii_case("localhost")
            || parse_endpoint_ip(host).is_some_and(|address| !is_public_destination(address))
    })
}

fn parse_endpoint_ip(host: &str) -> Option<IpAddr> {
    host.parse().ok().or_else(|| {
        host.strip_prefix('[')
            .and_then(|host| host.strip_suffix(']'))
            .and_then(|host| host.parse().ok())
    })
}

fn is_loopback(endpoint: &Url) -> bool {
    endpoint.host_str().is_some_and(|host| {
        host.eq_ignore_ascii_case("localhost")
            || parse_endpoint_ip(host).is_some_and(|address| address.is_loopback())
    })
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum DestinationPolicy {
    PublicOnly,
    LoopbackFixture,
}

impl DestinationPolicy {
    fn for_endpoint(endpoint: &Url) -> Self {
        if endpoint.scheme() == "http" && is_loopback(endpoint) {
            Self::LoopbackFixture
        } else {
            Self::PublicOnly
        }
    }

    fn permits(self, address: IpAddr) -> bool {
        match self {
            Self::PublicOnly => is_public_destination(address),
            Self::LoopbackFixture => is_loopback_address(address),
        }
    }
}

fn is_loopback_address(address: IpAddr) -> bool {
    match address {
        IpAddr::V4(address) => address.is_loopback(),
        IpAddr::V6(address) => {
            address.is_loopback()
                || address
                    .to_ipv4_mapped()
                    .is_some_and(|address| address.is_loopback())
        }
    }
}

fn is_public_destination(address: IpAddr) -> bool {
    match address {
        IpAddr::V4(address) => is_public_ipv4(address),
        IpAddr::V6(address) => is_public_ipv6(address),
    }
}

fn is_public_ipv4(address: Ipv4Addr) -> bool {
    let value = u32::from(address);
    ![
        (0x0000_0000, 0xff00_0000), // current host / unspecified
        (0x0a00_0000, 0xff00_0000), // private
        (0x6440_0000, 0xffc0_0000), // shared address space
        (0x7f00_0000, 0xff00_0000), // loopback
        (0xa9fe_0000, 0xffff_0000), // link-local
        (0xac10_0000, 0xfff0_0000), // private
        (0xc000_0000, 0xffff_ff00), // IETF protocol assignments
        (0xc000_0200, 0xffff_ff00), // documentation
        (0xc058_6300, 0xffff_ff00), // deprecated 6to4 relay anycast
        (0xc0a8_0000, 0xffff_0000), // private
        (0xc612_0000, 0xfffe_0000), // benchmarking
        (0xc633_6400, 0xffff_ff00), // documentation
        (0xcb00_7100, 0xffff_ff00), // documentation
        (0xe000_0000, 0xf000_0000), // multicast
        (0xf000_0000, 0xf000_0000), // reserved and broadcast
    ]
    .iter()
    .any(|(network, mask)| value & mask == *network)
}

fn is_public_ipv6(address: Ipv6Addr) -> bool {
    let segments = address.segments();
    if address.is_unspecified()
        || address.is_loopback()
        || segments[0] & 0xfe00 == 0xfc00 // unique-local
        || segments[0] & 0xffc0 == 0xfe80 // link-local
        || segments[0] & 0xffc0 == 0xfec0 // deprecated site-local
        || segments[0] & 0xff00 == 0xff00 // multicast
        || (segments[0] == 0x0100 && segments[1..4] == [0, 0, 0]) // discard-only
        || segments[..4] == [0x0100, 0, 0, 1] // dummy IPv6 prefix
        || (segments[0] == 0x2001 && segments[1] <= 0x01ff) // special-use /23
        || (segments[0] == 0x2001 && segments[1] == 0x0db8) // documentation
        || segments[0] & 0xfff0 == 0x3ff0 // documentation
        || (segments[0] == 0x5f00)
    // segment-routing local-use block
    {
        return false;
    }

    // IPv4-mapped destinations and transition prefixes must not smuggle an unsafe
    // IPv4 target through an otherwise syntactically IPv6 address.
    if let Some(mapped) = address.to_ipv4_mapped() {
        return is_public_ipv4(mapped);
    }
    if segments[..6] == [0, 0, 0, 0, 0, 0] {
        return false;
    }
    if segments[0] == 0x2002 {
        return is_public_ipv4(Ipv4Addr::new(
            (segments[1] >> 8) as u8,
            segments[1] as u8,
            (segments[2] >> 8) as u8,
            segments[2] as u8,
        ));
    }
    if segments[..3] == [0x0064, 0xff9b, 0] {
        return is_public_ipv4(Ipv4Addr::new(
            (segments[6] >> 8) as u8,
            segments[6] as u8,
            (segments[7] >> 8) as u8,
            segments[7] as u8,
        ));
    }
    if segments[..3] == [0x0064, 0xff9b, 1] {
        return false;
    }

    true
}

struct SystemResolver;

impl Resolve for SystemResolver {
    fn resolve(&self, name: Name) -> Resolving {
        let host = name.as_str().to_owned();
        Box::pin(async move {
            let addresses = tokio::net::lookup_host((host, 0)).await?;
            Ok(Box::new(addresses) as Addrs)
        })
    }
}

struct DestinationResolver {
    source_host: String,
    policy: DestinationPolicy,
    inner: Arc<dyn Resolve>,
}

impl DestinationResolver {
    fn system(source_host: String, policy: DestinationPolicy) -> Self {
        Self {
            source_host,
            policy,
            inner: Arc::new(SystemResolver),
        }
    }

    #[cfg(test)]
    fn with_inner(
        source_host: impl Into<String>,
        policy: DestinationPolicy,
        inner: Arc<dyn Resolve>,
    ) -> Self {
        Self {
            source_host: source_host.into(),
            policy,
            inner,
        }
    }
}

impl Resolve for DestinationResolver {
    fn resolve(&self, name: Name) -> Resolving {
        if !name.as_str().eq_ignore_ascii_case(&self.source_host) {
            return Box::pin(std::future::ready(Err(dns_policy_error(
                "refused DNS resolution outside the configured source host",
            ))));
        }

        let resolving = self.inner.resolve(name);
        let policy = self.policy;
        Box::pin(async move {
            let mut approved = Vec::new();
            for address in resolving.await? {
                if approved.len() == MAX_RESOLVED_DESTINATIONS {
                    return Err(dns_policy_error(
                        "source DNS answer exceeded the destination-address limit",
                    ));
                }
                if !policy.permits(address.ip()) {
                    return Err(dns_policy_error(
                        "source DNS answer contained a non-public destination",
                    ));
                }
                approved.push(address);
            }
            if approved.is_empty() {
                return Err(dns_policy_error(
                    "source DNS answer contained no destinations",
                ));
            }
            Ok(Box::new(approved.into_iter()) as Addrs)
        })
    }
}

fn dns_policy_error(message: &'static str) -> Box<dyn Error + Send + Sync> {
    Box::new(io::Error::new(io::ErrorKind::PermissionDenied, message))
}

/// A client-configuration validation error.
#[derive(Debug)]
pub enum ConfigError {
    /// The endpoint was not a valid URL.
    InvalidEndpoint(String),
    /// The endpoint contained credentials, a query, a fragment, or no host.
    UnsafeEndpoint,
    /// A literal endpoint host is not a globally routable destination.
    UnsafeDestination,
    /// Remote Action API endpoints must use HTTPS.
    HttpsRequired,
    /// The User-Agent was empty or contained a control character.
    InvalidUserAgent,
    /// A configurable bound was zero.
    ZeroLimit {
        /// Human-readable name of the invalid limit.
        name: &'static str,
    },
    /// A configurable request limit exceeded MediaWiki's supported maximum.
    InvalidLimit {
        /// Human-readable name of the invalid limit.
        name: &'static str,
        /// Largest accepted value.
        maximum: usize,
    },
    /// A lower retry-policy bound exceeded its corresponding upper bound.
    InvalidRange {
        /// Value required to be no greater than the other value.
        smaller: &'static str,
        /// Inclusive upper bound.
        larger: &'static str,
    },
}

impl fmt::Display for ConfigError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidEndpoint(error) => write!(formatter, "invalid API endpoint: {error}"),
            Self::UnsafeEndpoint => formatter.write_str(
                "API endpoint must be an absolute URL without credentials, query, or fragment",
            ),
            Self::UnsafeDestination => formatter.write_str(
                "API endpoint host must be globally routable (loopback HTTP is fixture-only)",
            ),
            Self::HttpsRequired => {
                formatter.write_str("API endpoint must use HTTPS unless it is loopback-only")
            }
            Self::InvalidUserAgent => {
                formatter.write_str("User-Agent must be non-empty and contain no controls")
            }
            Self::ZeroLimit { name } => write!(formatter, "{name} must be greater than zero"),
            Self::InvalidLimit { name, maximum } => {
                write!(formatter, "{name} must be between 1 and {maximum}")
            }
            Self::InvalidRange { smaller, larger } => {
                write!(formatter, "{smaller} must not exceed {larger}")
            }
        }
    }
}

impl Error for ConfigError {}

/// A bounded asynchronous client for read-only MediaWiki Action API requests.
#[derive(Clone, Debug)]
pub struct MediaWikiClient {
    config: ClientConfig,
    http: reqwest::Client,
    circuit: Arc<CircuitBreaker>,
    transport_limits: Arc<TransportLimits>,
}

#[derive(Debug)]
struct TransportLimits {
    request_slots: Semaphore,
    downloaded_response_bytes: AtomicUsize,
    max_downloaded_response_bytes: usize,
    download_rate_limiter: Option<ByteRateLimiter>,
}

impl TransportLimits {
    fn reserve_response_capacity(&self, bytes: usize) -> Result<ResponseBudget<'_>, ClientError> {
        self.downloaded_response_bytes
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |downloaded| {
                downloaded
                    .checked_add(bytes)
                    .filter(|total| *total <= self.max_downloaded_response_bytes)
            })
            .map(|_| ResponseBudget {
                limits: self,
                reserved: bytes,
                received: 0,
            })
            .map_err(|downloaded| ClientError::DownloadBudgetExceeded {
                limit: self.max_downloaded_response_bytes,
                downloaded,
                next_chunk: bytes,
            })
    }
}

/// A clone-shared token bucket whose credit and sleeps are both explicitly bounded.
///
/// Credit is stored as byte-nanoseconds so refills use integer arithmetic. The
/// bucket holds at most one second of capacity, and large chunks are consumed in
/// at-most-one-second quanta. `Instant` keeps wall-clock adjustments from affecting
/// download shaping.
#[derive(Debug)]
struct ByteRateLimiter {
    bytes_per_second: usize,
    state: AsyncMutex<ByteRateState>,
}

#[derive(Debug)]
struct ByteRateState {
    credit_byte_nanos: u128,
    last_refill: Instant,
}

impl ByteRateLimiter {
    const NANOS_PER_SECOND: u128 = 1_000_000_000;

    fn new(bytes_per_second: NonZeroUsize) -> Self {
        Self {
            bytes_per_second: bytes_per_second.get(),
            state: AsyncMutex::new(ByteRateState {
                credit_byte_nanos: 0,
                last_refill: Instant::now(),
            }),
        }
    }

    async fn consume(&self, bytes: usize) {
        let mut remaining = bytes;
        let rate = self.bytes_per_second as u128;
        let capacity = rate.saturating_mul(Self::NANOS_PER_SECOND);

        while remaining > 0 {
            let quantum = remaining.min(self.bytes_per_second);
            let cost = (quantum as u128).saturating_mul(Self::NANOS_PER_SECOND);
            let mut state = self.state.lock().await;

            loop {
                state.refill(rate, capacity);
                if state.credit_byte_nanos >= cost {
                    state.credit_byte_nanos -= cost;
                    break;
                }

                let missing = cost - state.credit_byte_nanos;
                let wait_nanos = missing.div_ceil(rate);
                let wait_nanos = u64::try_from(wait_nanos)
                    .expect("one rate-limiter quantum waits no more than one second");
                tokio::time::sleep(Duration::from_nanos(wait_nanos)).await;
            }

            drop(state);
            remaining -= quantum;
        }
    }
}

impl ByteRateState {
    fn refill(&mut self, rate: u128, capacity: u128) {
        let now = Instant::now();
        let elapsed_nanos = now.saturating_duration_since(self.last_refill).as_nanos();
        self.last_refill = now;
        self.credit_byte_nanos = self
            .credit_byte_nanos
            .saturating_add(elapsed_nanos.saturating_mul(rate))
            .min(capacity);
    }
}

#[derive(Debug)]
struct ResponseBudget<'a> {
    limits: &'a TransportLimits,
    reserved: usize,
    received: usize,
}

impl ResponseBudget<'_> {
    fn record_chunk(&mut self, bytes: usize) -> Result<(), ClientError> {
        let received = self.received.checked_add(bytes);
        if received.is_some_and(|received| received <= self.reserved) {
            self.received = received.expect("checked above");
            Ok(())
        } else {
            Err(ClientError::DownloadBudgetExceeded {
                limit: self.limits.max_downloaded_response_bytes,
                downloaded: self
                    .limits
                    .downloaded_response_bytes
                    .load(Ordering::Acquire),
                next_chunk: bytes,
            })
        }
    }
}

impl Drop for ResponseBudget<'_> {
    fn drop(&mut self) {
        let unused = self.reserved.saturating_sub(self.received);
        self.limits
            .downloaded_response_bytes
            .fetch_sub(unused, Ordering::AcqRel);
    }
}

#[derive(Debug)]
struct CircuitBreaker {
    state: Mutex<CircuitState>,
}

#[derive(Debug)]
struct CircuitState {
    consecutive_failures: usize,
    open_until: Option<Instant>,
    jitter_state: u64,
}

impl CircuitBreaker {
    fn new() -> Self {
        let seed = NEXT_JITTER_SEED.fetch_add(0x9e37_79b9_7f4a_7c15, Ordering::Relaxed);
        Self {
            state: Mutex::new(CircuitState {
                consecutive_failures: 0,
                open_until: None,
                jitter_state: seed.max(1),
            }),
        }
    }

    fn before_request(&self) -> Result<(), ClientError> {
        let now = Instant::now();
        let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        if let Some(open_until) = state.open_until {
            let remaining = open_until.saturating_duration_since(now);
            if !remaining.is_zero() {
                return Err(ClientError::CircuitOpen {
                    retry_after: remaining,
                });
            }
            state.open_until = None;
            state.consecutive_failures = 0;
        }
        Ok(())
    }

    fn record_success(&self) {
        let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        state.consecutive_failures = 0;
        state.open_until = None;
    }

    fn record_retryable_failure(&self, policy: RetryPolicy) {
        let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        state.consecutive_failures = state.consecutive_failures.saturating_add(1);
        if state.consecutive_failures >= policy.circuit_failure_threshold.get() {
            state.open_until = Instant::now().checked_add(policy.circuit_open_duration);
        }
    }

    fn random_inclusive(&self, maximum: u64) -> u64 {
        if maximum == 0 {
            return 0;
        }
        let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        let mut value = state.jitter_state;
        value ^= value << 13;
        value ^= value >> 7;
        value ^= value << 17;
        state.jitter_state = value.max(1);
        value % maximum.saturating_add(1)
    }
}

impl MediaWikiClient {
    /// Builds a client with redirects, TLS, timeouts, and a fixed User-Agent policy.
    pub fn new(config: ClientConfig) -> Result<Self, ClientError> {
        let https_only = config.endpoint.scheme() == "https";
        let redirect_endpoint = config.endpoint.clone();
        let redirect_allowed_hosts = config.allowed_source_hosts.clone();
        let max_redirects = config.max_redirects;
        let resolver = DestinationResolver::system(
            config
                .endpoint
                .host_str()
                .expect("validated endpoint has a host")
                .to_ascii_lowercase(),
            config.destination_policy,
        );
        let http = reqwest::Client::builder()
            .user_agent(&config.user_agent)
            .timeout(config.request_timeout)
            .connect_timeout(config.connect_timeout)
            // A proxy would resolve/connect the source outside this client's
            // destination policy and would introduce a second source trust boundary.
            .no_proxy()
            .dns_resolver(Arc::new(resolver))
            .redirect(redirect::Policy::custom(move |attempt| {
                if attempt.previous().len() > max_redirects {
                    return attempt.error("MediaWiki redirect limit exceeded");
                }
                if !redirect_destination_allowed(
                    attempt.url(),
                    &redirect_endpoint,
                    &redirect_allowed_hosts,
                ) {
                    return attempt
                        .error("MediaWiki redirect left the explicitly allowed source origin");
                }
                attempt.follow()
            }))
            .https_only(https_only)
            .build()
            .map_err(ClientError::Transport)?;

        let transport_limits = Arc::new(TransportLimits {
            request_slots: Semaphore::new(config.max_concurrent_requests.get()),
            downloaded_response_bytes: AtomicUsize::new(0),
            max_downloaded_response_bytes: config.max_downloaded_response_bytes_per_run.get(),
            download_rate_limiter: config
                .max_downloaded_response_bytes_per_second
                .map(ByteRateLimiter::new),
        });

        Ok(Self {
            config,
            http,
            circuit: Arc::new(CircuitBreaker::new()),
            transport_limits,
        })
    }

    /// Resolves and normalizes a bounded set of titles, following redirects.
    ///
    /// The operation batches requests at MediaWiki's normal-user title limit. It
    /// returns canonical pages, so several aliases may resolve to one result.
    pub async fn resolve_titles(
        &self,
        titles: &[PageTitle],
    ) -> Result<Vec<TitleResolution>, ClientError> {
        if titles.len() > self.config.max_titles_per_operation.get() {
            return Err(ClientError::OperationLimitExceeded {
                operation: "title resolution",
                limit: self.config.max_titles_per_operation.get(),
                actual: titles.len(),
            });
        }

        let mut resolved = Vec::with_capacity(titles.len());
        for chunk in titles.chunks(self.config.titles_per_request.get()) {
            let joined = chunk
                .iter()
                .map(PageTitle::as_str)
                .collect::<Vec<_>>()
                .join("|");
            let response: QueryResponse<PagesQuery> = self
                .get_json(&[
                    ("action", "query"),
                    ("prop", "revisions"),
                    ("titles", &joined),
                    ("redirects", "1"),
                    ("rvprop", "ids|timestamp|size|sha1"),
                    ("rvslots", "main"),
                ])
                .await?;

            for page in response.query.pages {
                resolved.push(page.try_into()?);
            }
        }
        Ok(resolved)
    }

    /// Resolves the current public head for one stable MediaWiki page ID.
    ///
    /// Unlike title resolution, this remains attached to the same page across moves.
    /// A missing response preserves the requested identity so callers can safely mark
    /// the page unavailable without discarding its captured history.
    pub async fn resolve_page_head(
        &self,
        page_id: PageId,
    ) -> Result<PageHeadResolution, ClientError> {
        let page_id_text = page_id.to_string();
        let response: QueryResponse<PagesQuery> = self
            .get_json(&[
                ("action", "query"),
                ("prop", "revisions"),
                ("pageids", &page_id_text),
                ("rvprop", "ids|timestamp|size|sha1"),
                ("rvslots", "main"),
            ])
            .await?;
        let mut pages = response.query.pages.into_iter();
        let page = pages.next().ok_or(ClientError::InvalidResponse(
            "page-head response did not contain the requested page",
        ))?;
        if pages.next().is_some() {
            return Err(ClientError::InvalidResponse(
                "page-head response contained more than one page",
            ));
        }
        if page.missing {
            return Ok(PageHeadResolution::Missing { page_id });
        }
        let resolved = match TitleResolution::try_from(page)? {
            TitleResolution::Found(page) => page,
            TitleResolution::Missing { .. } => {
                return Ok(PageHeadResolution::Missing { page_id });
            }
        };
        if resolved.page_id != page_id {
            return Err(ClientError::InvalidResponse(
                "page-head response returned a different page ID",
            ));
        }
        Ok(PageHeadResolution::Found(Box::new(resolved)))
    }

    /// Fetches one bounded page of revision metadata.
    ///
    /// Pass the returned continuation back to this method to obtain the next page.
    /// Each request returns at most 500 revisions and never includes revision text.
    pub async fn revision_batch(
        &self,
        page_id: PageId,
        order: RevisionOrder,
        continuation: Option<&RevisionContinuation>,
    ) -> Result<RevisionBatch, ClientError> {
        self.revision_batch_from(page_id, None, order, continuation)
            .await
    }

    /// Fetches one bounded revision-metadata page from an optional inclusive anchor.
    ///
    /// With [`RevisionOrder::OldestFirst`], an anchor lets reconciliation stream
    /// forward from its newest durable revision without retaining the whole gap.
    pub async fn revision_batch_from(
        &self,
        page_id: PageId,
        start_revision: Option<RevisionId>,
        order: RevisionOrder,
        continuation: Option<&RevisionContinuation>,
    ) -> Result<RevisionBatch, ClientError> {
        let page_id_text = page_id.to_string();
        let request_limit = self.config.revisions_per_request.get().min(500).to_string();
        let mut params = vec![
            ("action", "query"),
            ("prop", "revisions"),
            ("pageids", &page_id_text),
            ("rvlimit", &request_limit),
            (
                "rvprop",
                "ids|timestamp|user|userid|comment|flags|size|sha1|contentmodel",
            ),
            ("rvslots", "main"),
            ("rvdir", order.as_api_value()),
        ];

        let start_revision_text = start_revision.map(|revision_id| revision_id.to_string());
        if let Some(start_revision) = start_revision_text.as_deref() {
            params.push(("rvstartid", start_revision));
        }

        if let Some(continuation) = continuation {
            params.push(("continue", continuation.generic.as_str()));
            params.push(("rvcontinue", continuation.revisions.as_str()));
        }

        let response: QueryResponse<PagesQuery> = self.get_json(&params).await?;
        let page = response
            .query
            .pages
            .into_iter()
            .find(|page| page.page_id == i64::try_from(page_id.get()).ok())
            .ok_or(ClientError::InvalidResponse(
                "revision response did not contain the requested page",
            ))?;

        let revisions = page
            .revisions
            .unwrap_or_default()
            .into_iter()
            .map(RevisionMetadata::try_from)
            .collect::<Result<Vec<_>, _>>()?;

        Ok(RevisionBatch {
            revisions,
            continuation: response
                .continuation
                .map(RevisionContinuation::try_from)
                .transpose()?,
        })
    }

    /// Fetches one bounded page of main-namespace pages and subcategories.
    ///
    /// Pass the returned opaque continuation to the next call for the same
    /// category. The source-side namespace filter excludes talk, file, user, and
    /// other namespaces; response validation enforces that contract locally.
    pub async fn category_members_batch(
        &self,
        category: &PageTitle,
        continuation: Option<&CategoryContinuation>,
    ) -> Result<CategoryMembersBatch, ClientError> {
        let request_limit = self.config.category_members_per_request.get().to_string();
        let mut params = vec![
            ("action", "query"),
            ("list", "categorymembers"),
            ("cmtitle", category.as_str()),
            ("cmtype", "page|subcat"),
            ("cmnamespace", "0|14"),
            ("cmlimit", &request_limit),
        ];
        if let Some(continuation) = continuation {
            params.push(("continue", continuation.generic.as_str()));
            params.push(("cmcontinue", continuation.category_members.as_str()));
        }

        let response: QueryResponse<CategoryMembersQuery> = self.get_json(&params).await?;
        let members = response
            .query
            .category_members
            .into_iter()
            .map(CategoryMember::try_from)
            .collect::<Result<Vec<_>, _>>()?;
        let continuation = response
            .continuation
            .map(CategoryContinuation::try_from)
            .transpose()?;
        Ok(CategoryMembersBatch {
            members,
            continuation,
        })
    }

    /// Fetches the canonical main-slot source for one known page revision.
    ///
    /// The caller supplies both identities so a malformed or surprising response
    /// cannot silently attach another page's content to the requested page. The
    /// response remains subject to the configured decompressed JSON body bound.
    pub async fn revision_content(
        &self,
        page_id: PageId,
        revision_id: RevisionId,
    ) -> Result<RevisionContent, ClientError> {
        let page_id_text = page_id.to_string();
        let revision_id_text = revision_id.to_string();
        let response: QueryResponse<PagesQuery> = self
            .get_json(&[
                ("action", "query"),
                ("prop", "revisions"),
                ("pageids", &page_id_text),
                ("rvstartid", &revision_id_text),
                ("rvendid", &revision_id_text),
                (
                    "rvprop",
                    "ids|timestamp|user|userid|comment|flags|size|sha1|contentmodel|content",
                ),
                ("rvslots", "main"),
            ])
            .await?;
        let page = response
            .query
            .pages
            .into_iter()
            .find(|page| page.page_id == i64::try_from(page_id.get()).ok())
            .ok_or(ClientError::InvalidResponse(
                "revision-content response did not contain the requested page",
            ))?;
        let revision = page
            .revisions
            .and_then(|revisions| revisions.into_iter().next())
            .ok_or(ClientError::InvalidResponse(
                "revision-content response did not contain the requested revision",
            ))?;
        if revision.revision_id != revision_id.get() {
            return Err(ClientError::InvalidResponse(
                "revision-content response returned a different revision",
            ));
        }
        let source = revision
            .slots
            .as_ref()
            .and_then(|slots| slots.main.content.as_deref())
            .ok_or(ClientError::InvalidResponse(
                "requested revision has no public main-slot content",
            ))?
            .as_bytes()
            .to_vec();

        Ok(RevisionContent {
            metadata: revision.try_into()?,
            source,
        })
    }

    async fn get_json<T>(&self, params: &[(&str, &str)]) -> Result<T, ClientError>
    where
        T: DeserializeOwned,
    {
        self.circuit.before_request()?;
        let max_lag = self.config.max_lag_seconds.to_string();
        let mut all_params = Vec::with_capacity(params.len() + 3);
        all_params.extend_from_slice(params);
        all_params.push(("format", "json"));
        all_params.push(("formatversion", "2"));
        all_params.push(("maxlag", &max_lag));

        let policy = self.config.retry_policy;
        for attempt in 1..=policy.maximum_attempts.get() {
            match self.get_json_once(&all_params).await {
                Ok(value) => {
                    self.circuit.record_success();
                    return Ok(value);
                }
                Err(error) if error.is_retryable() && attempt < policy.maximum_attempts.get() => {
                    let delay = self.retry_delay(attempt, error.retry_after());
                    tokio::time::sleep(delay).await;
                }
                Err(error) => {
                    if error.is_retryable() {
                        self.circuit.record_retryable_failure(policy);
                    } else {
                        // A completed non-transient response breaks a sequence of
                        // source-availability failures even though this operation
                        // still returns its validation or protocol error.
                        self.circuit.record_success();
                    }
                    return Err(error);
                }
            }
        }
        unreachable!("retry policies always contain at least one attempt")
    }

    async fn get_json_once<T>(&self, all_params: &[(&str, &str)]) -> Result<T, ClientError>
    where
        T: DeserializeOwned,
    {
        let _request_permit = self
            .transport_limits
            .request_slots
            .acquire()
            .await
            .expect("MediaWiki request semaphore is never closed");
        let mut response = self
            .http
            .get(self.config.endpoint.clone())
            .query(&all_params)
            .send()
            .await
            .map_err(ClientError::Transport)?;
        let status = response.status();
        let retry_after = response
            .headers()
            .get(reqwest::header::RETRY_AFTER)
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.parse::<u64>().ok())
            .map(Duration::from_secs);

        if response
            .content_length()
            .is_some_and(|length| length > self.config.max_response_bytes.get() as u64)
        {
            return Err(ClientError::ResponseTooLarge {
                limit: self.config.max_response_bytes.get(),
            });
        }
        let declared_length = response
            .content_length()
            .map(usize::try_from)
            .transpose()
            .map_err(|_| ClientError::ResponseTooLarge {
                limit: self.config.max_response_bytes.get(),
            })?;
        // Atomically reserve exact declared capacity, or the complete per-response
        // ceiling for an unknown/chunked body. This closes aggregate-budget races
        // between cloned clients; unused capacity is returned when the response ends.
        let mut response_budget = self.transport_limits.reserve_response_capacity(
            declared_length.unwrap_or(self.config.max_response_bytes.get()),
        )?;

        let mut body = Vec::new();
        while let Some(chunk) = response.chunk().await.map_err(ClientError::Transport)? {
            response_budget.record_chunk(chunk.len())?;
            let new_length =
                body.len()
                    .checked_add(chunk.len())
                    .ok_or(ClientError::ResponseTooLarge {
                        limit: self.config.max_response_bytes.get(),
                    })?;
            if new_length > self.config.max_response_bytes.get() {
                return Err(ClientError::ResponseTooLarge {
                    limit: self.config.max_response_bytes.get(),
                });
            }
            if let Some(limiter) = &self.transport_limits.download_rate_limiter {
                limiter.consume(chunk.len()).await;
            }
            body.extend_from_slice(&chunk);
        }

        // Probe only the optional top-level error field first. Unknown success fields
        // are traversed as `IgnoredAny`, so bounded typed deserializers can reject an
        // oversized collection before a generic JSON value allocates it.
        let error_envelope: ApiErrorEnvelope =
            serde_json::from_slice(&body).map_err(ClientError::Decode)?;
        if let Some(error) = error_envelope.error {
            return Err(ClientError::Api(ApiError {
                code: error.code,
                info: error.info,
                retry_after,
            }));
        }
        if !status.is_success() {
            return Err(ClientError::HttpStatus {
                status,
                retry_after,
            });
        }

        serde_json::from_slice(&body).map_err(ClientError::Decode)
    }

    fn retry_delay(&self, retry_number: usize, retry_after: Option<Duration>) -> Duration {
        let policy = self.config.retry_policy;
        let mut exponential = policy.initial_delay;
        for _ in 1..retry_number {
            exponential = exponential.saturating_mul(2).min(policy.maximum_delay);
        }
        exponential = exponential.min(policy.maximum_delay);

        let exponential_nanos = u64::try_from(exponential.as_nanos()).unwrap_or(u64::MAX);
        let minimum_jitter = exponential_nanos / 2;
        let jitter_span = exponential_nanos.saturating_sub(minimum_jitter);
        let jittered = Duration::from_nanos(
            minimum_jitter.saturating_add(self.circuit.random_inclusive(jitter_span)),
        );
        let requested = retry_after.unwrap_or_default().min(policy.maximum_delay);
        jittered.max(requested)
    }
}

/// Direction in which MediaWiki enumerates a page's revisions.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RevisionOrder {
    /// Newest revision first.
    NewestFirst,
    /// Oldest revision first.
    OldestFirst,
}

impl RevisionOrder {
    const fn as_api_value(self) -> &'static str {
        match self {
            Self::NewestFirst => "older",
            Self::OldestFirst => "newer",
        }
    }
}

/// The result of resolving one canonical title returned by MediaWiki.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum TitleResolution {
    /// A title resolved to a stable remote page identity.
    Found(ResolvedPage),
    /// MediaWiki reported that the normalized title does not exist.
    Missing {
        /// The normalized missing title.
        title: PageTitle,
        /// The namespace inferred by MediaWiki.
        namespace: i32,
    },
}

/// Current public state of one stable MediaWiki page identity.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum PageHeadResolution {
    /// The page remains public, with its current canonical title and revision.
    Found(Box<ResolvedPage>),
    /// The requested stable identity is not currently public.
    Missing {
        /// Stable identity supplied by the caller.
        page_id: PageId,
    },
}

/// Stable page metadata returned during title resolution.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ResolvedPage {
    /// Stable MediaWiki page ID.
    pub page_id: PageId,
    /// MediaWiki namespace number.
    pub namespace: i32,
    /// Current canonical title after normalization and redirect resolution.
    pub title: PageTitle,
    /// Current public revision metadata, when returned by the source.
    pub current_revision: Option<RevisionMetadata>,
}

/// Public metadata for one MediaWiki revision; canonical content is fetched separately.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RevisionMetadata {
    /// Stable revision ID.
    pub revision_id: RevisionId,
    /// Parent revision, absent for the first revision in a page's history.
    pub parent_id: Option<RevisionId>,
    /// MediaWiki's UTC ISO-8601 timestamp.
    pub timestamp: String,
    /// Public author name or IP, absent when hidden.
    pub user: Option<String>,
    /// Public registered-user ID, absent for anonymous or hidden authors.
    pub user_id: Option<u64>,
    /// Public edit comment, absent when hidden.
    pub comment: Option<String>,
    /// Whether MediaWiki marked the edit minor.
    pub minor: bool,
    /// Uncompressed revision size in bytes, when public.
    pub size: Option<u64>,
    /// Upstream MediaWiki SHA-1, when public.
    pub sha1: Option<String>,
    /// Content model for the main slot, when returned.
    pub content_model: Option<String>,
}

/// Canonical public main-slot bytes and metadata for one revision.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RevisionContent {
    /// Public revision metadata returned with the source.
    pub metadata: RevisionMetadata,
    /// Exact UTF-8 bytes obtained by decoding the Action API JSON string.
    pub source: Vec<u8>,
}

/// One response page of revision metadata and its opaque next-page token.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RevisionBatch {
    /// Revisions in the requested order.
    pub revisions: Vec<RevisionMetadata>,
    /// Token for the next request, or `None` at the end of history.
    pub continuation: Option<RevisionContinuation>,
}

/// Opaque MediaWiki continuation values for revision enumeration.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RevisionContinuation {
    generic: String,
    revisions: String,
}

/// One main-namespace page or subcategory returned by category enumeration.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CategoryMember {
    /// Stable MediaWiki page identity.
    pub page_id: PageId,
    /// Current canonical title.
    pub title: PageTitle,
    /// Whether this member is selectable article content or a traversal edge.
    pub kind: CategoryMemberKind,
}

/// The namespace role of a category member.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CategoryMemberKind {
    /// A page in MediaWiki's main namespace (namespace 0).
    Page,
    /// A category in MediaWiki's category namespace (namespace 14).
    Subcategory,
}

/// One response page of category members and its opaque next-page token.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CategoryMembersBatch {
    /// Main-namespace pages and subcategories in source order.
    pub members: Vec<CategoryMember>,
    /// Token for the next request for this category, or `None` at the end.
    pub continuation: Option<CategoryContinuation>,
}

/// Opaque MediaWiki continuation values for category-member enumeration.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CategoryContinuation {
    generic: String,
    category_members: String,
}

/// A transport, bound, protocol, or remote API error.
#[derive(Debug)]
pub enum ClientError {
    /// The shared client circuit is temporarily rejecting source requests after
    /// repeated exhausted retryable operations.
    CircuitOpen {
        /// Remaining cool-down before a later operation may probe the source.
        retry_after: Duration,
    },
    /// The HTTP stack failed before a complete response was available.
    Transport(reqwest::Error),
    /// The response body was not valid for the expected API schema.
    Decode(serde_json::Error),
    /// The server returned a non-success HTTP response without an Action API error.
    HttpStatus {
        /// HTTP status code.
        status: StatusCode,
        /// Server-provided delay, when it was a simple integer number of seconds.
        retry_after: Option<Duration>,
    },
    /// The Action API returned a structured error, which may use HTTP 200.
    Api(ApiError),
    /// The response body exceeded the configured decompressed-byte limit.
    ResponseTooLarge {
        /// Configured byte limit.
        limit: usize,
    },
    /// Response bodies exhausted the aggregate byte budget for this client run.
    DownloadBudgetExceeded {
        /// Configured aggregate byte limit.
        limit: usize,
        /// Bytes downloaded or atomically reserved by responses in this run.
        downloaded: usize,
        /// Size of the declared response or next chunk that would exceed the limit.
        next_chunk: usize,
    },
    /// The caller attempted a larger operation than configured.
    OperationLimitExceeded {
        /// Name of the bounded operation.
        operation: &'static str,
        /// Configured item limit.
        limit: usize,
        /// Requested item count.
        actual: usize,
    },
    /// The remote response was valid JSON but violated an expected invariant.
    InvalidResponse(&'static str),
}

impl ClientError {
    /// Returns whether a later retry may succeed without changing the request.
    #[must_use]
    pub fn is_retryable(&self) -> bool {
        match self {
            Self::CircuitOpen { .. } => true,
            Self::Transport(error) => error.is_timeout() || error.is_connect(),
            Self::HttpStatus { status, .. } => matches!(
                *status,
                StatusCode::TOO_MANY_REQUESTS
                    | StatusCode::BAD_GATEWAY
                    | StatusCode::SERVICE_UNAVAILABLE
                    | StatusCode::GATEWAY_TIMEOUT
            ),
            Self::Api(error) => error.is_retryable(),
            _ => false,
        }
    }

    /// Returns a server-requested minimum retry delay, when present.
    #[must_use]
    pub fn retry_after(&self) -> Option<Duration> {
        match self {
            Self::CircuitOpen { retry_after } => Some(*retry_after),
            Self::HttpStatus { retry_after, .. } => *retry_after,
            Self::Api(error) => error.retry_after,
            _ => None,
        }
    }
}

impl fmt::Display for ClientError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::CircuitOpen { retry_after } => write!(
                formatter,
                "MediaWiki circuit breaker is open; retry after about {} ms",
                retry_after.as_millis()
            ),
            Self::Transport(error) => write!(formatter, "MediaWiki request failed: {error}"),
            Self::Decode(error) => write!(formatter, "invalid MediaWiki JSON response: {error}"),
            Self::HttpStatus { status, .. } => {
                write!(formatter, "MediaWiki returned HTTP status {status}")
            }
            Self::Api(error) => error.fmt(formatter),
            Self::ResponseTooLarge { limit } => {
                write!(
                    formatter,
                    "MediaWiki response exceeded the {limit}-byte limit"
                )
            }
            Self::DownloadBudgetExceeded {
                limit,
                downloaded,
                next_chunk,
            } => write!(
                formatter,
                "MediaWiki run download budget of {limit} bytes was exhausted after {downloaded} bytes before accepting the next {next_chunk} bytes"
            ),
            Self::OperationLimitExceeded {
                operation,
                limit,
                actual,
            } => write!(
                formatter,
                "{operation} requested {actual} items, exceeding the configured limit of {limit}"
            ),
            Self::InvalidResponse(message) => {
                write!(formatter, "invalid MediaWiki response: {message}")
            }
        }
    }
}

fn redirect_destination_allowed(
    destination: &Url,
    endpoint: &Url,
    allowed_source_hosts: &[String],
) -> bool {
    if !destination.username().is_empty()
        || destination.password().is_some()
        || destination.fragment().is_some()
    {
        return false;
    }

    let Some(host) = destination.host_str() else {
        return false;
    };
    if !allowed_source_hosts
        .iter()
        .any(|allowed| allowed.eq_ignore_ascii_case(host))
    {
        return false;
    }
    if destination.scheme() != endpoint.scheme()
        || !endpoint
            .host_str()
            .is_some_and(|endpoint_host| endpoint_host.eq_ignore_ascii_case(host))
        || destination.port_or_known_default() != endpoint.port_or_known_default()
    {
        return false;
    }

    matches!(destination.scheme(), "https")
        || (destination.scheme() == "http" && is_loopback(destination))
}

impl Error for ClientError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Transport(error) => Some(error),
            Self::Decode(error) => Some(error),
            Self::Api(error) => Some(error),
            _ => None,
        }
    }
}

/// A structured MediaWiki Action API error.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ApiError {
    /// Stable API error code.
    pub code: String,
    /// Human-readable context supplied by MediaWiki.
    pub info: String,
    /// Server-provided retry delay, when available.
    pub retry_after: Option<Duration>,
}

impl ApiError {
    /// Returns whether this API condition is expected to be temporary.
    #[must_use]
    pub fn is_retryable(&self) -> bool {
        matches!(
            self.code.as_str(),
            "maxlag" | "ratelimited" | "readonly" | "internal_api_error_DBConnectionError"
        )
    }
}

impl fmt::Display for ApiError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "MediaWiki API error {}: {}",
            self.code, self.info
        )
    }
}

impl Error for ApiError {}

#[derive(Debug, Deserialize)]
struct QueryResponse<T> {
    query: T,
    #[serde(rename = "continue")]
    continuation: Option<ContinuationPayload>,
}

#[derive(Debug, Deserialize)]
struct ContinuationPayload {
    #[serde(rename = "continue")]
    generic: String,
    rvcontinue: Option<String>,
    cmcontinue: Option<String>,
}

impl TryFrom<ContinuationPayload> for RevisionContinuation {
    type Error = ClientError;

    fn try_from(value: ContinuationPayload) -> Result<Self, Self::Error> {
        Ok(Self {
            generic: value.generic,
            revisions: value.rvcontinue.ok_or(ClientError::InvalidResponse(
                "revision continuation omitted rvcontinue",
            ))?,
        })
    }
}

impl TryFrom<ContinuationPayload> for CategoryContinuation {
    type Error = ClientError;

    fn try_from(value: ContinuationPayload) -> Result<Self, Self::Error> {
        Ok(Self {
            generic: value.generic,
            category_members: value.cmcontinue.ok_or(ClientError::InvalidResponse(
                "category continuation omitted cmcontinue",
            ))?,
        })
    }
}

#[derive(Debug, Deserialize)]
struct PagesQuery {
    pages: Vec<PagePayload>,
}

#[derive(Debug, Deserialize)]
struct CategoryMembersQuery {
    #[serde(rename = "categorymembers")]
    category_members: Vec<CategoryMemberPayload>,
}

#[derive(Debug, Deserialize)]
struct CategoryMemberPayload {
    #[serde(rename = "pageid")]
    page_id: u64,
    ns: i32,
    title: String,
}

impl TryFrom<CategoryMemberPayload> for CategoryMember {
    type Error = ClientError;

    fn try_from(member: CategoryMemberPayload) -> Result<Self, Self::Error> {
        let page_id = member
            .page_id
            .try_into()
            .map_err(|_| ClientError::InvalidResponse("category member had a zero page ID"))?;
        let title = PageTitle::new(member.title).map_err(|_| {
            ClientError::InvalidResponse("MediaWiki returned an invalid category-member title")
        })?;
        let kind = match member.ns {
            wikisync_core::MAIN_NAMESPACE => CategoryMemberKind::Page,
            wikisync_core::CATEGORY_NAMESPACE => CategoryMemberKind::Subcategory,
            _ => {
                return Err(ClientError::InvalidResponse(
                    "category response contained a member outside namespaces 0 and 14",
                ));
            }
        };
        Ok(Self {
            page_id,
            title,
            kind,
        })
    }
}

#[derive(Debug, Deserialize)]
struct PagePayload {
    #[serde(rename = "pageid")]
    page_id: Option<i64>,
    ns: i32,
    title: String,
    #[serde(default)]
    missing: bool,
    revisions: Option<Vec<RevisionPayload>>,
}

impl TryFrom<PagePayload> for TitleResolution {
    type Error = ClientError;

    fn try_from(page: PagePayload) -> Result<Self, Self::Error> {
        let title = PageTitle::new(page.title)
            .map_err(|_| ClientError::InvalidResponse("MediaWiki returned an invalid title"))?;
        if page.missing {
            return Ok(Self::Missing {
                title,
                namespace: page.ns,
            });
        }

        let raw_page_id = page
            .page_id
            .ok_or(ClientError::InvalidResponse("existing page had no page ID"))?;
        let page_id = u64::try_from(raw_page_id)
            .map_err(|_| ClientError::InvalidResponse("MediaWiki returned an invalid page ID"))?
            .try_into()
            .map_err(|_| ClientError::InvalidResponse("MediaWiki returned a zero page ID"))?;
        let current_revision = page
            .revisions
            .and_then(|revisions| revisions.into_iter().next())
            .map(RevisionMetadata::try_from)
            .transpose()?;

        Ok(Self::Found(ResolvedPage {
            page_id,
            namespace: page.ns,
            title,
            current_revision,
        }))
    }
}

#[derive(Debug, Deserialize)]
struct RevisionPayload {
    #[serde(rename = "revid")]
    revision_id: u64,
    #[serde(rename = "parentid")]
    parent_id: Option<u64>,
    timestamp: String,
    user: Option<String>,
    #[serde(rename = "userid")]
    user_id: Option<u64>,
    comment: Option<String>,
    #[serde(default)]
    minor: bool,
    size: Option<u64>,
    sha1: Option<String>,
    #[serde(rename = "contentmodel")]
    content_model: Option<String>,
    slots: Option<SlotsPayload>,
}

#[derive(Debug, Deserialize)]
struct SlotsPayload {
    main: MainSlotPayload,
}

#[derive(Debug, Deserialize)]
struct MainSlotPayload {
    #[serde(rename = "contentmodel")]
    content_model: Option<String>,
    content: Option<String>,
}

impl TryFrom<RevisionPayload> for RevisionMetadata {
    type Error = ClientError;

    fn try_from(revision: RevisionPayload) -> Result<Self, Self::Error> {
        let revision_id = revision
            .revision_id
            .try_into()
            .map_err(|_| ClientError::InvalidResponse("MediaWiki returned a zero revision ID"))?;
        let parent_id = match revision.parent_id {
            Some(0) | None => None,
            Some(parent_id) => Some(parent_id.try_into().map_err(|_| {
                ClientError::InvalidResponse("MediaWiki returned an invalid parent revision ID")
            })?),
        };
        let slot_content_model = revision.slots.and_then(|slots| slots.main.content_model);

        Ok(Self {
            revision_id,
            parent_id,
            timestamp: revision.timestamp,
            user: revision.user,
            user_id: revision.user_id,
            comment: revision.comment,
            minor: revision.minor,
            size: revision.size,
            sha1: revision.sha1,
            content_model: slot_content_model.or(revision.content_model),
        })
    }
}

#[derive(Debug, Deserialize)]
struct ApiErrorEnvelope {
    #[serde(default)]
    error: Option<ApiErrorPayload>,
}

#[derive(Debug, Deserialize)]
struct ApiErrorPayload {
    code: String,
    info: String,
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;
    use std::net::SocketAddr;

    use super::*;

    struct ScriptedResolver {
        answers: Mutex<VecDeque<Vec<SocketAddr>>>,
    }

    impl ScriptedResolver {
        fn new(answers: Vec<Vec<SocketAddr>>) -> Self {
            Self {
                answers: Mutex::new(answers.into()),
            }
        }
    }

    impl Resolve for ScriptedResolver {
        fn resolve(&self, _name: Name) -> Resolving {
            let answer = self
                .answers
                .lock()
                .expect("scripted DNS lock")
                .pop_front()
                .expect("scripted DNS answer");
            Box::pin(std::future::ready(
                Ok(Box::new(answer.into_iter()) as Addrs),
            ))
        }
    }

    #[test]
    fn endpoint_policy_requires_https_except_on_loopback() {
        let config = ClientConfig::new("https://EN.WIKIPEDIA.ORG/w/api.php", "WikiSyncer/0.1")
            .expect("HTTPS source config");
        assert_eq!(
            config.allowed_source_hosts().collect::<Vec<_>>(),
            ["en.wikipedia.org"]
        );
        assert!(ClientConfig::new("http://127.0.0.1:8080/api.php", "WikiSyncer/0.1").is_ok());
        assert!(matches!(
            ClientConfig::new("http://example.com/api.php", "WikiSyncer/0.1"),
            Err(ConfigError::HttpsRequired)
        ));
        assert!(matches!(
            ClientConfig::new("https://user@example.com/api.php", "WikiSyncer/0.1"),
            Err(ConfigError::UnsafeEndpoint)
        ));
        for endpoint in [
            "https://127.0.0.1/api.php",
            "https://10.0.0.1/api.php",
            "https://[::1]/api.php",
            "https://localhost/api.php",
        ] {
            assert!(matches!(
                ClientConfig::new(endpoint, "WikiSyncer/0.1"),
                Err(ConfigError::UnsafeDestination)
            ));
        }
    }

    #[test]
    fn destination_policy_rejects_private_and_special_use_addresses() {
        for address in [
            "0.0.0.0",
            "10.0.0.1",
            "100.64.0.1",
            "127.0.0.1",
            "169.254.1.1",
            "172.16.0.1",
            "192.0.2.1",
            "192.168.1.1",
            "198.18.0.1",
            "224.0.0.1",
            "255.255.255.255",
            "::",
            "::1",
            "::ffff:127.0.0.1",
            "64:ff9b::a00:1",
            "100::1",
            "2001:2::1",
            "2001:db8::1",
            "2002:ac10:1::",
            "3fff::1",
            "5f00::1",
            "fc00::1",
            "fe80::1",
            "ff02::1",
        ] {
            let address = address.parse::<IpAddr>().expect("test address");
            assert!(!is_public_destination(address), "accepted {address}");
        }
        for address in ["8.8.8.8", "1.1.1.1", "2606:4700:4700::1111"] {
            let address = address.parse::<IpAddr>().expect("test address");
            assert!(is_public_destination(address), "rejected {address}");
        }
    }

    #[tokio::test]
    async fn resolver_revalidates_rebinding_answers_and_rejects_the_unsafe_answer() {
        let inner = Arc::new(ScriptedResolver::new(vec![
            vec!["8.8.8.8:0".parse().expect("public address")],
            vec!["127.0.0.1:0".parse().expect("loopback address")],
        ]));
        let resolver =
            DestinationResolver::with_inner("source.example", DestinationPolicy::PublicOnly, inner);

        let first = resolver
            .resolve("source.example".parse().expect("DNS name"))
            .await
            .expect("first public DNS answer")
            .collect::<Vec<_>>();
        assert_eq!(first, ["8.8.8.8:0".parse::<SocketAddr>().expect("address")]);

        let Err(error) = resolver
            .resolve("source.example".parse().expect("DNS name"))
            .await
        else {
            panic!("rebound loopback destination must fail closed");
        };
        assert!(error.to_string().contains("non-public destination"));
    }

    #[tokio::test]
    async fn resolver_rejects_a_mixed_answer_instead_of_filtering_it() {
        let inner = Arc::new(ScriptedResolver::new(vec![vec![
            "8.8.8.8:0".parse().expect("public address"),
            "192.168.1.1:0".parse().expect("private address"),
        ]]));
        let resolver =
            DestinationResolver::with_inner("source.example", DestinationPolicy::PublicOnly, inner);

        assert!(
            resolver
                .resolve("source.example".parse().expect("DNS name"))
                .await
                .is_err(),
            "mixed DNS answer must fail closed"
        );
    }

    #[tokio::test]
    async fn resolver_rejects_unexpected_hosts_and_oversized_answers() {
        let addresses = (0..=MAX_RESOLVED_DESTINATIONS)
            .map(|index| SocketAddr::from(([8, 8, 8, (index + 1) as u8], 0)))
            .collect();
        let inner = Arc::new(ScriptedResolver::new(vec![addresses]));
        let resolver =
            DestinationResolver::with_inner("source.example", DestinationPolicy::PublicOnly, inner);

        assert!(
            resolver
                .resolve("other.example".parse().expect("DNS name"))
                .await
                .is_err(),
            "resolver must stay bound to the configured source host"
        );
        assert!(
            resolver
                .resolve("source.example".parse().expect("DNS name"))
                .await
                .is_err(),
            "oversized DNS answer must fail closed"
        );
    }

    #[tokio::test]
    async fn resolver_rejects_an_empty_answer() {
        let inner = Arc::new(ScriptedResolver::new(vec![Vec::new()]));
        let resolver =
            DestinationResolver::with_inner("source.example", DestinationPolicy::PublicOnly, inner);

        assert!(
            resolver
                .resolve("source.example".parse().expect("DNS name"))
                .await
                .is_err(),
            "empty DNS answer must fail closed"
        );
    }

    #[tokio::test]
    async fn loopback_fixture_policy_accepts_only_loopback_answers() {
        let inner = Arc::new(ScriptedResolver::new(vec![vec![
            "127.0.0.1:0".parse().expect("loopback address"),
            "[::1]:0".parse().expect("loopback address"),
        ]]));
        let resolver =
            DestinationResolver::with_inner("localhost", DestinationPolicy::LoopbackFixture, inner);

        assert_eq!(
            resolver
                .resolve("localhost".parse().expect("DNS name"))
                .await
                .expect("loopback fixture DNS answer")
                .count(),
            2
        );
    }

    #[test]
    fn retry_policy_is_explicit_and_conservative() {
        let maxlag = ClientError::Api(ApiError {
            code: "maxlag".to_owned(),
            info: "replicas are lagged".to_owned(),
            retry_after: Some(Duration::from_secs(5)),
        });
        assert!(maxlag.is_retryable());
        assert_eq!(maxlag.retry_after(), Some(Duration::from_secs(5)));

        let invalid = ClientError::InvalidResponse("bad identity");
        assert!(!invalid.is_retryable());
    }

    #[test]
    fn retry_backoff_is_exponential_jittered_capped_and_server_aware() {
        let policy = RetryPolicy::new(4, Duration::from_millis(8), Duration::from_millis(20))
            .expect("retry policy");
        let config = ClientConfig::new("http://127.0.0.1:9/api.php", "WikiSyncer/0.1")
            .expect("loopback config")
            .with_retry_policy(policy);
        let client = MediaWikiClient::new(config).expect("client");

        let first = client.retry_delay(1, None);
        assert!((Duration::from_millis(4)..=Duration::from_millis(8)).contains(&first));
        let second = client.retry_delay(2, None);
        assert!((Duration::from_millis(8)..=Duration::from_millis(16)).contains(&second));
        let capped = client.retry_delay(3, None);
        assert!((Duration::from_millis(10)..=Duration::from_millis(20)).contains(&capped));
        assert_eq!(
            client.retry_delay(1, Some(Duration::from_secs(10))),
            Duration::from_millis(20),
            "Retry-After is a minimum delay clamped by the configured safety ceiling"
        );
    }

    #[test]
    fn retry_policy_rejects_zero_and_inverted_bounds() {
        assert!(matches!(
            RetryPolicy::new(0, Duration::from_millis(1), Duration::from_millis(2)),
            Err(ConfigError::ZeroLimit { .. })
        ));
        assert!(matches!(
            RetryPolicy::new(1, Duration::from_millis(2), Duration::from_millis(1)),
            Err(ConfigError::InvalidRange { .. })
        ));
        assert!(matches!(
            RetryPolicy::default().with_circuit_breaker(0, Duration::from_secs(1)),
            Err(ConfigError::ZeroLimit { .. })
        ));
        let config = ClientConfig::new("https://example.com/api.php", "WikiSyncer/0.1")
            .expect("valid config");
        assert!(matches!(
            config.clone().with_max_concurrent_requests(0),
            Err(ConfigError::ZeroLimit { .. })
        ));
        assert!(matches!(
            config.with_max_downloaded_response_bytes_per_run(0),
            Err(ConfigError::ZeroLimit { .. })
        ));
        let config = ClientConfig::new("https://example.com/api.php", "WikiSyncer/0.1")
            .expect("valid config");
        assert_eq!(
            config.max_downloaded_response_bytes_per_second(),
            None,
            "an absent byte-rate policy is unlimited"
        );
        assert!(matches!(
            config
                .clone()
                .with_max_downloaded_response_bytes_per_second(Some(0)),
            Err(ConfigError::ZeroLimit { .. })
        ));
        let configured = config
            .with_max_downloaded_response_bytes_per_second(Some(1_024))
            .expect("positive byte rate");
        assert_eq!(
            configured.max_downloaded_response_bytes_per_second(),
            Some(1_024)
        );
    }
}
