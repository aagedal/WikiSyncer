//! Bounded discovery and acquisition of revision thumbnails.

use std::fmt;
use std::time::Duration;

use reqwest::Url;
use serde::Deserialize;
use serde::de::{SeqAccess, Visitor};
use wikisync_core::{MediaId, PageId, PageTitle, RevisionId, ThumbnailPolicy};

#[cfg(feature = "fuzzing")]
use super::decode_action_api_json;
use super::{ClientError, MediaWikiClient, deserialize_bounded_vec, redirect_destination_allowed};

const FILE_NAMESPACE: i32 = 6;
const MAX_METADATA_TEXT_BYTES: usize = 16 * 1024;
const MAX_SOURCE_SHA1_BYTES: usize = 128;

/// Maximum raw file references accepted from one exact-revision `parse.images`
/// response before extension filtering and policy selection.
///
/// This deliberately exceeds the stable policy's eligible-image ceiling while
/// keeping a hostile response from amplifying a bounded JSON body into an
/// unbounded vector of heap-allocated strings.
pub const MAX_REVISION_IMAGE_REFERENCES: usize = 4_096;

/// One passive raster reference exposed by parsing an exact selected revision.
///
/// MediaWiki's `parse.images` result does not expose lead semantics, repeated
/// occurrences, captions, or alternative text. Those fields are therefore
/// deliberately not synthesized here.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RevisionImagePlacement {
    /// Zero-based order among eligible JPEG/PNG references as returned by the API.
    pub index: u32,
    /// File title suitable for a subsequent imageinfo query.
    pub file_title: PageTitle,
    /// Caption supplied for this placement, when the upstream API exposes one.
    /// This is currently always `None` for Action API `parse.images` results.
    pub caption: Option<String>,
    /// Alternative text supplied for this placement, when exposed upstream.
    /// This is currently always `None` for Action API `parse.images` results.
    pub alt_text: Option<String>,
}

/// Passive MIME types eligible for stable thumbnail capture.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum ThumbnailMimeType {
    /// JPEG raster bytes.
    Jpeg,
    /// PNG raster bytes. APNG rejection belongs to the content validator.
    Png,
}

impl ThumbnailMimeType {
    /// Returns the canonical MIME spelling expected by the content validator.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Jpeg => "image/jpeg",
            Self::Png => "image/png",
        }
    }

    fn from_api(value: &str) -> Option<Self> {
        match value {
            "image/jpeg" => Some(Self::Jpeg),
            "image/png" => Some(Self::Png),
            _ => None,
        }
    }
}

/// Complete source metadata needed before thumbnail bytes may be catalogued.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ThumbnailMetadata {
    /// Stable MediaWiki file-page identity.
    pub media_id: MediaId,
    /// Canonical file title returned by MediaWiki.
    pub file_title: PageTitle,
    /// Exact upstream SHA-1 for the source file version.
    pub source_sha1: String,
    /// URL of the bounded raster thumbnail.
    pub thumbnail_url: String,
    /// Human-facing file description page.
    pub description_url: String,
    /// Upstream Artist metadata. It remains untrusted presentation text.
    pub artist: String,
    /// Upstream Credit metadata, when supplied. It remains untrusted text.
    pub credit: Option<String>,
    /// Upstream short license name or identifier.
    pub license_short_name: String,
    /// Upstream license URL, when supplied.
    pub license_url: Option<String>,
    /// Width MediaWiki reports for the requested thumbnail.
    pub width: u32,
    /// Height MediaWiki reports for the requested thumbnail.
    pub height: u32,
    /// Exact passive MIME type reported for the source file.
    pub mime_type: ThumbnailMimeType,
}

/// A non-fatal reason why an image reference cannot be acquired under stable-v1.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ThumbnailIneligibility {
    /// The file page or current public imageinfo is unavailable.
    Missing,
    /// The source MIME type is not exact JPEG or PNG.
    UnsupportedMimeType,
    /// Required attribution, license, URL, hash, or dimension metadata is absent.
    IncompleteMetadata,
    /// MediaWiki returned a thumbnail outside the configured edge policy.
    DimensionsOutsidePolicy,
}

/// Result of resolving one file reference to bounded thumbnail metadata.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ThumbnailMetadataResolution {
    /// Complete metadata for an eligible passive thumbnail.
    Eligible(Box<ThumbnailMetadata>),
    /// The reference must be skipped without affecting text capture.
    Ineligible(ThumbnailIneligibility),
}

/// A redacted thumbnail-download failure.
///
/// No variant stores or displays the requested URL or response bytes.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ThumbnailDownloadError {
    /// The URL was malformed or outside the source's explicitly approved origins.
    UrlRejected,
    /// The shared circuit breaker is temporarily open.
    CircuitOpen {
        /// Remaining cool-down before a later operation may probe the source.
        retry_after: Duration,
    },
    /// HTTP transport failed, including a rejected redirect.
    Transport,
    /// The source returned a non-success status.
    HttpStatus {
        /// Numeric status code, which contains no URL or body data.
        status: u16,
        /// Bounded server retry guidance, when present.
        retry_after: Option<Duration>,
    },
    /// The encoded thumbnail exceeded the configured per-image bound.
    ImageBytesExceeded {
        /// Configured per-image byte ceiling.
        limit: u64,
    },
    /// Caller-supplied dimensions exceed the configured thumbnail edge.
    DimensionsOutsidePolicy,
    /// The shared run-wide downloaded-response budget was exhausted.
    RunBudgetExceeded {
        /// Configured run-wide ceiling.
        limit: usize,
    },
}

impl ThumbnailDownloadError {
    /// Returns whether repeating the same request later may succeed.
    #[must_use]
    pub const fn is_retryable(self) -> bool {
        match self {
            Self::CircuitOpen { .. } | Self::Transport => true,
            Self::HttpStatus { status, .. } => matches!(status, 429 | 502 | 503 | 504),
            Self::UrlRejected
            | Self::ImageBytesExceeded { .. }
            | Self::DimensionsOutsidePolicy
            | Self::RunBudgetExceeded { .. } => false,
        }
    }

    /// Returns source-provided retry guidance, when available.
    #[must_use]
    pub const fn retry_after(self) -> Option<Duration> {
        match self {
            Self::CircuitOpen { retry_after } => Some(retry_after),
            Self::HttpStatus { retry_after, .. } => retry_after,
            _ => None,
        }
    }
}

impl fmt::Display for ThumbnailDownloadError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UrlRejected => formatter
                .write_str("thumbnail URL must remain on an approved MediaWiki source origin"),
            Self::CircuitOpen { retry_after } => write!(
                formatter,
                "MediaWiki circuit breaker is open; retry after about {} ms",
                retry_after.as_millis()
            ),
            Self::Transport => formatter.write_str("thumbnail request failed"),
            Self::HttpStatus { status, .. } => {
                write!(formatter, "thumbnail source returned HTTP status {status}")
            }
            Self::ImageBytesExceeded { limit } => {
                write!(formatter, "thumbnail exceeded the {limit}-byte image limit")
            }
            Self::DimensionsOutsidePolicy => {
                formatter.write_str("thumbnail metadata exceeds the configured dimension limit")
            }
            Self::RunBudgetExceeded { limit } => {
                write!(
                    formatter,
                    "MediaWiki run download budget of {limit} bytes was exhausted"
                )
            }
        }
    }
}

impl std::error::Error for ThumbnailDownloadError {}

impl MediaWikiClient {
    /// Discovers a bounded set of JPEG/PNG references for an exact revision.
    ///
    /// The Action API does not provide per-occurrence captions through `parse.images`.
    /// Consequently, a successful empty result or placements with absent captions do
    /// not affect the separately fetched canonical revision text.
    pub async fn revision_image_placements(
        &self,
        page_id: PageId,
        revision_id: RevisionId,
        policy: ThumbnailPolicy,
    ) -> Result<Vec<RevisionImagePlacement>, ClientError> {
        let requested_revision_id = revision_id;
        let revision_id = requested_revision_id.to_string();
        let response: ParseResponse = self
            .get_json(&[
                ("action", "parse"),
                ("oldid", &revision_id),
                ("prop", "images"),
                ("disablelimitreport", "1"),
            ])
            .await?;
        revision_image_placements_from_response(response, page_id, requested_revision_id, policy)
    }

    /// Resolves one passive file reference to current thumbnail and attribution metadata.
    ///
    /// Missing or unsuitable metadata is returned as a typed, non-fatal result so
    /// callers can continue capturing the revision's canonical text.
    pub async fn resolve_thumbnail_metadata(
        &self,
        placement: &RevisionImagePlacement,
        policy: ThumbnailPolicy,
    ) -> Result<ThumbnailMetadataResolution, ClientError> {
        let edge = policy.maximum_edge_pixels().get().to_string();
        let response: ImageInfoResponse = self
            .get_json(&[
                ("action", "query"),
                ("prop", "imageinfo"),
                ("titles", placement.file_title.as_str()),
                ("iiprop", "url|size|mime|sha1|extmetadata"),
                ("iilimit", "1"),
                ("iiurlwidth", &edge),
                ("iiurlheight", &edge),
            ])
            .await?;
        resolve_imageinfo(response, &placement.file_title, policy)
    }

    /// Downloads one eligible thumbnail through the client's shared transport policy.
    ///
    /// The URL is rejected before contact unless it has an exact approved scheme,
    /// host, and effective port. Ordinary HTTPS Wikimedia project endpoints derive
    /// the exact `https://upload.wikimedia.org:443` thumbnail origin; third-party and
    /// loopback fixture endpoints remain restricted to their API origin.
    pub async fn download_thumbnail(
        &self,
        metadata: &ThumbnailMetadata,
        policy: ThumbnailPolicy,
    ) -> Result<Vec<u8>, ThumbnailDownloadError> {
        if metadata.width > policy.maximum_edge_pixels().get()
            || metadata.height > policy.maximum_edge_pixels().get()
        {
            return Err(ThumbnailDownloadError::DimensionsOutsidePolicy);
        }
        let url = Url::parse(&metadata.thumbnail_url)
            .ok()
            .filter(|url| redirect_destination_allowed(url, &self.config.allowed_origins))
            .ok_or(ThumbnailDownloadError::UrlRejected)?;
        self.get_thumbnail_bytes(url, policy.maximum_bytes_per_image().get())
            .await
    }

    async fn get_thumbnail_bytes(
        &self,
        url: Url,
        limit: u64,
    ) -> Result<Vec<u8>, ThumbnailDownloadError> {
        self.circuit
            .before_request()
            .map_err(map_download_client_error)?;
        let retry_policy = self.config.retry_policy;
        for attempt in 1..=retry_policy.maximum_attempts.get() {
            match self.get_thumbnail_bytes_once(url.clone(), limit).await {
                Ok(bytes) => {
                    self.circuit.record_success();
                    return Ok(bytes);
                }
                Err(error)
                    if error.is_retryable() && attempt < retry_policy.maximum_attempts.get() =>
                {
                    let delay = self.retry_delay(attempt, error.retry_after());
                    tokio::time::sleep(delay).await;
                }
                Err(error) => {
                    if error.is_retryable() {
                        self.circuit.record_retryable_failure(retry_policy);
                    } else {
                        self.circuit.record_success();
                    }
                    return Err(error);
                }
            }
        }
        unreachable!("retry policies always contain at least one attempt")
    }

    async fn get_thumbnail_bytes_once(
        &self,
        url: Url,
        limit: u64,
    ) -> Result<Vec<u8>, ThumbnailDownloadError> {
        let _request_permit = self
            .transport_limits
            .request_slots
            .acquire()
            .await
            .expect("MediaWiki request semaphore is never closed");
        let mut response = self.http.get(url).send().await.map_err(|error| {
            if error.is_redirect() {
                ThumbnailDownloadError::UrlRejected
            } else {
                ThumbnailDownloadError::Transport
            }
        })?;
        let status = response.status();
        let retry_after = response
            .headers()
            .get(reqwest::header::RETRY_AFTER)
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.parse::<u64>().ok())
            .map(Duration::from_secs);

        if response
            .content_length()
            .is_some_and(|length| length > limit)
        {
            return Err(ThumbnailDownloadError::ImageBytesExceeded { limit });
        }
        let capacity = response.content_length().unwrap_or(limit);
        let capacity = usize::try_from(capacity)
            .map_err(|_| ThumbnailDownloadError::ImageBytesExceeded { limit })?;
        let mut response_budget = self
            .transport_limits
            .reserve_response_capacity(capacity)
            .map_err(map_download_client_error)?;
        let initial_capacity = capacity.min(64 * 1024);
        let mut body = Vec::with_capacity(initial_capacity);
        while let Some(chunk) = response
            .chunk()
            .await
            .map_err(|_| ThumbnailDownloadError::Transport)?
        {
            response_budget
                .record_chunk(chunk.len())
                .map_err(map_download_client_error)?;
            let new_length = body
                .len()
                .checked_add(chunk.len())
                .ok_or(ThumbnailDownloadError::ImageBytesExceeded { limit })?;
            if u64::try_from(new_length).unwrap_or(u64::MAX) > limit {
                return Err(ThumbnailDownloadError::ImageBytesExceeded { limit });
            }
            if let Some(limiter) = &self.transport_limits.download_rate_limiter {
                limiter.consume(chunk.len()).await;
            }
            body.extend_from_slice(&chunk);
        }
        if !status.is_success() {
            return Err(ThumbnailDownloadError::HttpStatus {
                status: status.as_u16(),
                retry_after,
            });
        }
        Ok(body)
    }
}

fn map_download_client_error(error: ClientError) -> ThumbnailDownloadError {
    match error {
        ClientError::CircuitOpen { retry_after } => {
            ThumbnailDownloadError::CircuitOpen { retry_after }
        }
        ClientError::DownloadBudgetExceeded { limit, .. } => {
            ThumbnailDownloadError::RunBudgetExceeded { limit }
        }
        _ => ThumbnailDownloadError::Transport,
    }
}

fn passive_file_title(image: &str) -> Result<Option<PageTitle>, ClientError> {
    let name = image
        .split_once(':')
        .filter(|(namespace, _)| namespace.eq_ignore_ascii_case("file"))
        .map_or(image, |(_, name)| name);
    let lowercase = name.to_ascii_lowercase();
    if !lowercase.ends_with(".jpg") && !lowercase.ends_with(".jpeg") && !lowercase.ends_with(".png")
    {
        return Ok(None);
    }
    PageTitle::new(format!("File:{name}"))
        .map(Some)
        .map_err(|_| ClientError::InvalidResponse("MediaWiki returned an invalid image title"))
}

fn revision_image_placements_from_response(
    response: ParseResponse,
    page_id: PageId,
    revision_id: RevisionId,
    policy: ThumbnailPolicy,
) -> Result<Vec<RevisionImagePlacement>, ClientError> {
    if response.parse.page_id != page_id.get() || response.parse.revision_id != revision_id.get() {
        return Err(ClientError::InvalidResponse(
            "revision-image response returned a different page or revision",
        ));
    }

    let maximum = usize::try_from(policy.maximum_images_per_revision().get())
        .expect("u32 always fits usize on supported targets");
    let mut placements = Vec::with_capacity(maximum.min(response.parse.images.len()));
    for image in response.parse.images {
        if placements.len() == maximum {
            break;
        }
        let Some(file_title) = passive_file_title(&image)? else {
            continue;
        };
        placements.push(RevisionImagePlacement {
            index: u32::try_from(placements.len())
                .expect("thumbnail policy count is bounded to u32"),
            file_title,
            caption: None,
            alt_text: None,
        });
    }
    Ok(placements)
}

fn resolve_imageinfo(
    response: ImageInfoResponse,
    requested_title: &PageTitle,
    policy: ThumbnailPolicy,
) -> Result<ThumbnailMetadataResolution, ClientError> {
    let ImageInfoQuery {
        pages,
        normalized,
        redirects,
    } = response.query;
    if !redirects.is_empty() {
        return Err(ClientError::InvalidResponse(
            "imageinfo unexpectedly followed a file redirect",
        ));
    }
    let expected_title = match normalized.as_slice() {
        [] => requested_title.as_str(),
        [mapping] if mapping.from == requested_title.as_str() => mapping.to.as_str(),
        _ => {
            return Err(ClientError::InvalidResponse(
                "imageinfo returned an invalid title-normalization mapping",
            ));
        }
    };
    let mut pages = pages.into_iter();
    let Some(page) = pages.next() else {
        return Err(ClientError::InvalidResponse(
            "imageinfo response did not contain a page",
        ));
    };
    if pages.next().is_some() {
        return Err(ClientError::InvalidResponse(
            "imageinfo response contained more than one page",
        ));
    }
    if page.title != expected_title {
        return Err(ClientError::InvalidResponse(
            "imageinfo response returned an unrelated file title",
        ));
    }
    if page.missing {
        return Ok(ThumbnailMetadataResolution::Ineligible(
            ThumbnailIneligibility::Missing,
        ));
    }
    if page.namespace != FILE_NAMESPACE {
        return Err(ClientError::InvalidResponse(
            "imageinfo response returned a non-file page",
        ));
    }
    let Some(info) = page.image_info.and_then(
        |mut infos| {
            if infos.len() == 1 { infos.pop() } else { None }
        },
    ) else {
        return Ok(ThumbnailMetadataResolution::Ineligible(
            ThumbnailIneligibility::Missing,
        ));
    };
    let Some(mime_type) = info
        .mime_type
        .as_deref()
        .and_then(ThumbnailMimeType::from_api)
    else {
        return Ok(ThumbnailMetadataResolution::Ineligible(
            ThumbnailIneligibility::UnsupportedMimeType,
        ));
    };
    let (Some(thumbnail_width), Some(thumbnail_height)) =
        (info.thumbnail_width, info.thumbnail_height)
    else {
        return Ok(incomplete_metadata());
    };
    let edge = policy.maximum_edge_pixels().get();
    if thumbnail_width == 0
        || thumbnail_height == 0
        || thumbnail_width > edge
        || thumbnail_height > edge
    {
        return Ok(ThumbnailMetadataResolution::Ineligible(
            ThumbnailIneligibility::DimensionsOutsidePolicy,
        ));
    }

    let Some(page_id) = page.page_id else {
        return Ok(incomplete_metadata());
    };
    let Ok(media_id) = MediaId::try_from(page_id) else {
        return Ok(incomplete_metadata());
    };
    let Ok(file_title) = PageTitle::new(page.title) else {
        return Ok(incomplete_metadata());
    };
    let metadata = info.ext_metadata;
    let artist = metadata.artist.and_then(bounded_value);
    let credit = metadata.credit.and_then(bounded_value);
    let license_short_name = metadata.license_short_name.and_then(bounded_value);
    let license_url = metadata.license_url.and_then(bounded_value);
    let (Some(source_sha1), Some(thumbnail_url), Some(description_url)) =
        (info.sha1, info.thumbnail_url, info.description_url)
    else {
        return Ok(incomplete_metadata());
    };
    if !bounded_nonempty(&source_sha1, MAX_SOURCE_SHA1_BYTES)
        || !source_sha1.bytes().all(|byte| byte.is_ascii_alphanumeric())
        || !safe_absolute_url(&thumbnail_url)
        || !safe_absolute_url(&description_url)
        || license_url
            .as_deref()
            .is_some_and(|url| !safe_absolute_url(url))
    {
        return Ok(incomplete_metadata());
    }
    let (Some(artist), Some(license_short_name)) = (artist, license_short_name) else {
        return Ok(incomplete_metadata());
    };

    Ok(ThumbnailMetadataResolution::Eligible(Box::new(
        ThumbnailMetadata {
            media_id,
            file_title,
            source_sha1,
            thumbnail_url,
            description_url,
            artist,
            credit,
            license_short_name,
            license_url,
            width: thumbnail_width,
            height: thumbnail_height,
            mime_type,
        },
    )))
}

fn incomplete_metadata() -> ThumbnailMetadataResolution {
    ThumbnailMetadataResolution::Ineligible(ThumbnailIneligibility::IncompleteMetadata)
}

fn bounded_value(field: ExtendedMetadataValue) -> Option<String> {
    bounded_nonempty(&field.value, MAX_METADATA_TEXT_BYTES).then_some(field.value)
}

fn bounded_nonempty(value: &str, maximum: usize) -> bool {
    !value.trim().is_empty()
        && value.len() <= maximum
        && !value.chars().any(|character| character.is_control())
}

fn safe_absolute_url(value: &str) -> bool {
    if !bounded_nonempty(value, MAX_METADATA_TEXT_BYTES) {
        return false;
    }
    Url::parse(value).is_ok_and(|url| {
        matches!(url.scheme(), "http" | "https")
            && url.host_str().is_some()
            && url.username().is_empty()
            && url.password().is_none()
            && url.fragment().is_none()
    })
}

#[cfg(feature = "fuzzing")]
pub(super) fn validate_revision_images_response(data: &[u8]) -> Result<(), ClientError> {
    let response: ParseResponse = decode_action_api_json(data, None)?;
    let page_id = PageId::new(42).expect("the fixed Action API fuzz page ID is valid");
    let revision_id = RevisionId::new(100).expect("the fixed Action API fuzz revision ID is valid");
    let policy = ThumbnailPolicy::new(640, 32, 1024 * 1024)
        .expect("the fixed Action API fuzz thumbnail policy is valid");
    let _ = revision_image_placements_from_response(response, page_id, revision_id, policy)?;
    Ok(())
}

#[cfg(feature = "fuzzing")]
pub(super) fn validate_thumbnail_metadata_response(data: &[u8]) -> Result<(), ClientError> {
    let response: ImageInfoResponse = decode_action_api_json(data, None)?;
    let requested_title =
        PageTitle::new("File:Ferris.png").expect("the fixed Action API fuzz title is valid");
    let policy = ThumbnailPolicy::new(640, 32, 1024 * 1024)
        .expect("the fixed Action API fuzz thumbnail policy is valid");
    let _ = resolve_imageinfo(response, &requested_title, policy)?;
    Ok(())
}

#[derive(Debug, Deserialize)]
struct ParseResponse {
    parse: ParsePayload,
}

#[derive(Debug, Deserialize)]
struct ParsePayload {
    #[serde(rename = "pageid")]
    page_id: u64,
    #[serde(rename = "revid")]
    revision_id: u64,
    #[serde(deserialize_with = "deserialize_bounded_image_references")]
    images: Vec<String>,
}

fn deserialize_bounded_image_references<'de, D>(deserializer: D) -> Result<Vec<String>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    struct BoundedImageReferences;

    impl<'de> Visitor<'de> for BoundedImageReferences {
        type Value = Vec<String>;

        fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
            write!(
                formatter,
                "at most {MAX_REVISION_IMAGE_REFERENCES} revision image references"
            )
        }

        fn visit_seq<A>(self, mut sequence: A) -> Result<Self::Value, A::Error>
        where
            A: SeqAccess<'de>,
        {
            let capacity = sequence
                .size_hint()
                .unwrap_or_default()
                .min(MAX_REVISION_IMAGE_REFERENCES);
            let mut images = Vec::with_capacity(capacity);
            while let Some(image) = sequence.next_element::<String>()? {
                if images.len() == MAX_REVISION_IMAGE_REFERENCES {
                    return Err(serde::de::Error::invalid_length(
                        images.len().saturating_add(1),
                        &self,
                    ));
                }
                images.push(image);
            }
            Ok(images)
        }
    }

    deserializer.deserialize_seq(BoundedImageReferences)
}

#[derive(Debug, Deserialize)]
struct ImageInfoResponse {
    query: ImageInfoQuery,
}

#[derive(Debug, Deserialize)]
struct ImageInfoQuery {
    #[serde(default, deserialize_with = "deserialize_title_mappings")]
    normalized: Vec<TitleMapping>,
    #[serde(default, deserialize_with = "deserialize_title_mappings")]
    redirects: Vec<TitleMapping>,
    #[serde(deserialize_with = "deserialize_imageinfo_pages")]
    pages: Vec<ImageInfoPage>,
}

#[derive(Debug, Deserialize)]
struct TitleMapping {
    from: String,
    to: String,
}

#[derive(Debug, Deserialize)]
struct ImageInfoPage {
    #[serde(rename = "pageid")]
    page_id: Option<u64>,
    #[serde(rename = "ns")]
    namespace: i32,
    title: String,
    #[serde(default)]
    missing: bool,
    #[serde(
        rename = "imageinfo",
        default,
        deserialize_with = "deserialize_optional_imageinfo"
    )]
    image_info: Option<Vec<ImageInfoPayload>>,
}

fn deserialize_title_mappings<'de, D>(deserializer: D) -> Result<Vec<TitleMapping>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    deserialize_bounded_vec::<_, _, 1>(deserializer, "title mappings")
}

fn deserialize_imageinfo_pages<'de, D>(deserializer: D) -> Result<Vec<ImageInfoPage>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    deserialize_bounded_vec::<_, _, 1>(deserializer, "imageinfo pages")
}

fn deserialize_optional_imageinfo<'de, D>(
    deserializer: D,
) -> Result<Option<Vec<ImageInfoPayload>>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    struct OptionalImageInfo;

    impl<'de> Visitor<'de> for OptionalImageInfo {
        type Value = Option<Vec<ImageInfoPayload>>;

        fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
            formatter.write_str("a null value or one imageinfo record")
        }

        fn visit_none<E>(self) -> Result<Self::Value, E>
        where
            E: serde::de::Error,
        {
            Ok(None)
        }

        fn visit_unit<E>(self) -> Result<Self::Value, E>
        where
            E: serde::de::Error,
        {
            Ok(None)
        }

        fn visit_some<D>(self, deserializer: D) -> Result<Self::Value, D::Error>
        where
            D: serde::Deserializer<'de>,
        {
            deserialize_bounded_vec::<_, _, 1>(deserializer, "imageinfo records").map(Some)
        }
    }

    deserializer.deserialize_option(OptionalImageInfo)
}

#[derive(Debug, Deserialize)]
struct ImageInfoPayload {
    sha1: Option<String>,
    #[serde(rename = "mime")]
    mime_type: Option<String>,
    #[serde(rename = "thumburl")]
    thumbnail_url: Option<String>,
    #[serde(rename = "thumbwidth")]
    thumbnail_width: Option<u32>,
    #[serde(rename = "thumbheight")]
    thumbnail_height: Option<u32>,
    #[serde(rename = "descriptionurl")]
    description_url: Option<String>,
    #[serde(rename = "extmetadata", default)]
    ext_metadata: ExtendedMetadata,
}

#[derive(Debug, Default, Deserialize)]
struct ExtendedMetadata {
    #[serde(rename = "Artist")]
    artist: Option<ExtendedMetadataValue>,
    #[serde(rename = "Credit")]
    credit: Option<ExtendedMetadataValue>,
    #[serde(rename = "LicenseShortName")]
    license_short_name: Option<ExtendedMetadataValue>,
    #[serde(rename = "LicenseUrl")]
    license_url: Option<ExtendedMetadataValue>,
}

#[derive(Debug, Deserialize)]
struct ExtendedMetadataValue {
    value: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn candidate_filter_is_exact_and_canonicalizes_file_namespace() {
        assert_eq!(
            passive_file_title("Photo.JPEG")
                .expect("valid response")
                .expect("eligible")
                .as_str(),
            "File:Photo.JPEG"
        );
        assert!(
            passive_file_title("Diagram.svg")
                .expect("valid response")
                .is_none()
        );
        assert!(
            passive_file_title("not-a-png.txt")
                .expect("valid response")
                .is_none()
        );
    }

    #[test]
    fn redacted_download_errors_never_render_urls() {
        let error = ThumbnailDownloadError::UrlRejected;
        assert!(!error.to_string().contains("https://"));
        assert!(!error.to_string().contains("secret"));
    }

    #[test]
    fn imageinfo_collections_reject_cardinality_amplification() {
        let mapping = serde_json::json!({"from": "File:Fuzz.jpg", "to": "File:Fuzz.jpg"});
        assert!(
            serde_json::from_value::<ImageInfoResponse>(serde_json::json!({
                "query": {
                    "normalized": [mapping.clone(), mapping],
                    "pages": []
                }
            }))
            .is_err()
        );

        let page = serde_json::json!({"pageid": 1, "ns": 6, "title": "File:Fuzz.jpg"});
        assert!(
            serde_json::from_value::<ImageInfoResponse>(serde_json::json!({
                "query": {"pages": [page.clone(), page]}
            }))
            .is_err()
        );

        let info = serde_json::json!({"mime": "image/png"});
        assert!(
            serde_json::from_value::<ImageInfoResponse>(serde_json::json!({
                "query": {"pages": [{
                    "pageid": 1,
                    "ns": 6,
                    "title": "File:Fuzz.jpg",
                    "imageinfo": [info.clone(), info]
                }]}
            }))
            .is_err()
        );
    }
}
