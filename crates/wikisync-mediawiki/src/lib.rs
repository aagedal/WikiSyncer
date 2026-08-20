//! Bounded access to the MediaWiki Action API.
//!
//! The transport deliberately performs one bounded API response at a time. Callers
//! persist [`RevisionContinuation`] between calls, which keeps long histories
//! resumable without an unbounded in-memory paginator or hidden retry loop.

use std::error::Error;
use std::fmt;
use std::net::IpAddr;
use std::num::NonZeroUsize;
use std::time::Duration;

use reqwest::{StatusCode, Url, redirect};
use serde::Deserialize;
use serde::de::DeserializeOwned;
use wikisync_core::{PageId, PageTitle, RevisionId};

const DEFAULT_RESPONSE_LIMIT: usize = 8 * 1024 * 1024;
const DEFAULT_TITLES_PER_OPERATION: usize = 1_000;
const DEFAULT_TITLES_PER_REQUEST: usize = 50;
const DEFAULT_REVISIONS_PER_REQUEST: usize = 500;

/// Limits and source identity used by a [`MediaWikiClient`].
#[derive(Clone, Debug)]
pub struct ClientConfig {
    endpoint: Url,
    user_agent: String,
    request_timeout: Duration,
    connect_timeout: Duration,
    max_response_bytes: NonZeroUsize,
    max_redirects: usize,
    max_lag_seconds: u16,
    max_titles_per_operation: NonZeroUsize,
    titles_per_request: NonZeroUsize,
    revisions_per_request: NonZeroUsize,
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
            endpoint,
            user_agent,
            request_timeout: Duration::from_secs(30),
            connect_timeout: Duration::from_secs(10),
            max_response_bytes: nonzero(DEFAULT_RESPONSE_LIMIT),
            max_redirects: 3,
            max_lag_seconds: 5,
            max_titles_per_operation: nonzero(DEFAULT_TITLES_PER_OPERATION),
            titles_per_request: nonzero(DEFAULT_TITLES_PER_REQUEST),
            revisions_per_request: nonzero(DEFAULT_REVISIONS_PER_REQUEST),
        })
    }

    /// Sets the maximum accepted response body size after decompression.
    pub fn with_max_response_bytes(mut self, bytes: usize) -> Result<Self, ConfigError> {
        self.max_response_bytes = NonZeroUsize::new(bytes).ok_or(ConfigError::ZeroLimit {
            name: "max response bytes",
        })?;
        Ok(self)
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
        "https" => Ok(()),
        "http" if is_loopback(endpoint) => Ok(()),
        _ => Err(ConfigError::HttpsRequired),
    }
}

fn is_loopback(endpoint: &Url) -> bool {
    endpoint.host_str().is_some_and(|host| {
        host.eq_ignore_ascii_case("localhost")
            || host
                .parse::<IpAddr>()
                .is_ok_and(|address| address.is_loopback())
    })
}

/// A client-configuration validation error.
#[derive(Debug)]
pub enum ConfigError {
    /// The endpoint was not a valid URL.
    InvalidEndpoint(String),
    /// The endpoint contained credentials, a query, a fragment, or no host.
    UnsafeEndpoint,
    /// Remote Action API endpoints must use HTTPS.
    HttpsRequired,
    /// The User-Agent was empty or contained a control character.
    InvalidUserAgent,
    /// A configurable bound was zero.
    ZeroLimit {
        /// Human-readable name of the invalid limit.
        name: &'static str,
    },
}

impl fmt::Display for ConfigError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidEndpoint(error) => write!(formatter, "invalid API endpoint: {error}"),
            Self::UnsafeEndpoint => formatter.write_str(
                "API endpoint must be an absolute URL without credentials, query, or fragment",
            ),
            Self::HttpsRequired => {
                formatter.write_str("API endpoint must use HTTPS unless it is loopback-only")
            }
            Self::InvalidUserAgent => {
                formatter.write_str("User-Agent must be non-empty and contain no controls")
            }
            Self::ZeroLimit { name } => write!(formatter, "{name} must be greater than zero"),
        }
    }
}

impl Error for ConfigError {}

/// A bounded asynchronous client for read-only MediaWiki Action API requests.
#[derive(Clone, Debug)]
pub struct MediaWikiClient {
    config: ClientConfig,
    http: reqwest::Client,
}

impl MediaWikiClient {
    /// Builds a client with redirects, TLS, timeouts, and a fixed User-Agent policy.
    pub fn new(config: ClientConfig) -> Result<Self, ClientError> {
        let https_only = config.endpoint.scheme() == "https";
        let http = reqwest::Client::builder()
            .user_agent(&config.user_agent)
            .timeout(config.request_timeout)
            .connect_timeout(config.connect_timeout)
            .redirect(redirect::Policy::limited(config.max_redirects))
            .https_only(https_only)
            .build()
            .map_err(ClientError::Transport)?;

        Ok(Self { config, http })
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

        if let Some(continuation) = continuation {
            params.push(("continue", continuation.generic.as_str()));
            params.push(("rvcontinue", continuation.revisions.as_str()));
        }

        let response: QueryResponse<PagesQuery> = self.get_json(&params).await?;
        let page = response
            .query
            .pages
            .into_iter()
            .find(|page| page.page_id == Some(page_id.get()))
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
            continuation: response.continuation.map(Into::into),
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
            .find(|page| page.page_id == Some(page_id.get()))
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
        let max_lag = self.config.max_lag_seconds.to_string();
        let mut all_params = Vec::with_capacity(params.len() + 3);
        all_params.extend_from_slice(params);
        all_params.push(("format", "json"));
        all_params.push(("formatversion", "2"));
        all_params.push(("maxlag", &max_lag));

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

        let mut body = Vec::new();
        while let Some(chunk) = response.chunk().await.map_err(ClientError::Transport)? {
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
            body.extend_from_slice(&chunk);
        }

        let value: serde_json::Value =
            serde_json::from_slice(&body).map_err(ClientError::Decode)?;
        if let Some(error) = value.get("error") {
            let error: ApiErrorPayload =
                serde_json::from_value(error.clone()).map_err(ClientError::Decode)?;
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

        serde_json::from_value(value).map_err(ClientError::Decode)
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

/// A transport, bound, protocol, or remote API error.
#[derive(Debug)]
pub enum ClientError {
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
            Self::HttpStatus { retry_after, .. } => *retry_after,
            Self::Api(error) => error.retry_after,
            _ => None,
        }
    }
}

impl fmt::Display for ClientError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
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
    rvcontinue: String,
}

impl From<ContinuationPayload> for RevisionContinuation {
    fn from(value: ContinuationPayload) -> Self {
        Self {
            generic: value.generic,
            revisions: value.rvcontinue,
        }
    }
}

#[derive(Debug, Deserialize)]
struct PagesQuery {
    pages: Vec<PagePayload>,
}

#[derive(Debug, Deserialize)]
struct PagePayload {
    #[serde(rename = "pageid")]
    page_id: Option<u64>,
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

        let page_id = page
            .page_id
            .ok_or(ClientError::InvalidResponse("existing page had no page ID"))?
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
struct ApiErrorPayload {
    code: String,
    info: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn endpoint_policy_requires_https_except_on_loopback() {
        assert!(ClientConfig::new("https://en.wikipedia.org/w/api.php", "WikiSyncer/0.1").is_ok());
        assert!(ClientConfig::new("http://127.0.0.1:8080/api.php", "WikiSyncer/0.1").is_ok());
        assert!(matches!(
            ClientConfig::new("http://example.com/api.php", "WikiSyncer/0.1"),
            Err(ConfigError::HttpsRequired)
        ));
        assert!(matches!(
            ClientConfig::new("https://user@example.com/api.php", "WikiSyncer/0.1"),
            Err(ConfigError::UnsafeEndpoint)
        ));
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
}
