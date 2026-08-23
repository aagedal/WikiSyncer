#[allow(dead_code)]
mod support;

use std::time::Duration;

use support::{FixtureResponse, FixtureServer};
use wikisync_core::{PageId, RevisionId, ThumbnailPolicy};
use wikisync_mediawiki::{
    ClientConfig, ClientError, MAX_REVISION_IMAGE_REFERENCES, MediaWikiClient, RetryPolicy,
    RevisionImagePlacement, ThumbnailDownloadError, ThumbnailIneligibility, ThumbnailMetadata,
    ThumbnailMetadataResolution, ThumbnailMimeType,
};

const REVISION_IMAGES: &str = include_str!("fixtures/revision-images.json");
const IMAGEINFO: &str = include_str!("fixtures/imageinfo.json");
const IMAGEINFO_SVG: &str = include_str!("fixtures/imageinfo-svg.json");
const IMAGEINFO_INCOMPLETE: &str = include_str!("fixtures/imageinfo-incomplete.json");
const IMAGEINFO_UNRELATED: &str = include_str!("fixtures/imageinfo-unrelated.json");
const IMAGEINFO_NORMALIZED: &str = include_str!("fixtures/imageinfo-normalized.json");
const IMAGEINFO_REDIRECT: &str = include_str!("fixtures/imageinfo-redirect.json");

fn client(server: &FixtureServer) -> MediaWikiClient {
    let config = ClientConfig::new(server.endpoint(), "WikiSyncer/0.1 media-fixtures")
        .expect("fixture config")
        .with_retry_policy(
            RetryPolicy::new(1, Duration::from_millis(1), Duration::from_millis(1))
                .expect("retry policy"),
        );
    MediaWikiClient::new(config).expect("fixture client")
}

fn policy(maximum_images: u32, maximum_bytes: u64) -> ThumbnailPolicy {
    ThumbnailPolicy::new(640, maximum_images, maximum_bytes).expect("thumbnail policy")
}

fn metadata(url: String) -> ThumbnailMetadata {
    ThumbnailMetadata {
        media_id: 9001_u64.try_into().expect("media ID"),
        file_title: "File:Fixture.jpg".try_into().expect("file title"),
        source_sha1: "abcdef0123456789abcdef0123456789".to_owned(),
        thumbnail_url: url,
        description_url: "https://example.org/wiki/File:Fixture.jpg".to_owned(),
        artist: "Fixture author".to_owned(),
        credit: Some("Fixture credit".to_owned()),
        license_short_name: "CC0".to_owned(),
        license_url: Some("https://creativecommons.org/publicdomain/zero/1.0/".to_owned()),
        width: 2,
        height: 2,
        mime_type: ThumbnailMimeType::Jpeg,
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn discovers_only_bounded_passive_candidates_for_exact_revision() {
    let server = FixtureServer::start(vec![FixtureResponse::json(REVISION_IMAGES)]);
    let client = client(&server);
    let placements = client
        .revision_image_placements(
            PageId::new(42).expect("page ID"),
            RevisionId::new(100).expect("revision ID"),
            policy(2, 1024),
        )
        .await
        .expect("image placements");
    assert_eq!(placements.len(), 2);
    assert_eq!(placements[0].file_title.as_str(), "File:Lead photo.JPG");
    assert_eq!(placements[1].file_title.as_str(), "File:Inline figure.png");
    assert_eq!(placements[0].index, 0);
    assert_eq!(placements[1].index, 1);
    assert_eq!(placements[0].caption, None);

    let requests = server.finish();
    assert_eq!(requests.len(), 1);
    assert!(requests[0].contains("action=parse"));
    assert!(requests[0].contains("oldid=100"));
    assert!(requests[0].contains("prop=images"));
}

#[tokio::test(flavor = "multi_thread")]
async fn rejects_raw_revision_image_reference_overflow_during_deserialization() {
    let images = std::iter::repeat_n("\"candidate.png\"", MAX_REVISION_IMAGE_REFERENCES + 1)
        .collect::<Vec<_>>()
        .join(",");
    let response = format!("{{\"parse\":{{\"pageid\":42,\"revid\":100,\"images\":[{images}]}}}}");
    let response = Box::leak(response.into_boxed_str());
    let server = FixtureServer::start(vec![FixtureResponse::json(response)]);
    let client = client(&server);

    let error = client
        .revision_image_placements(
            PageId::new(42).expect("page ID"),
            RevisionId::new(100).expect("revision ID"),
            policy(1, 1024),
        )
        .await
        .expect_err("raw image reference bound must fail closed");
    assert!(matches!(error, ClientError::Decode(_)));
    assert_eq!(server.finish().len(), 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn resolves_complete_attribution_and_thumbnail_metadata() {
    let server = FixtureServer::start(vec![FixtureResponse::json(IMAGEINFO)]);
    let client = client(&server);
    let placement = RevisionImagePlacement {
        index: 0,
        file_title: "File:Lead photo.JPG".try_into().expect("file title"),
        caption: None,
        alt_text: None,
    };
    let resolution = client
        .resolve_thumbnail_metadata(&placement, policy(2, 1024))
        .await
        .expect("imageinfo");
    let ThumbnailMetadataResolution::Eligible(metadata) = resolution else {
        panic!("fixture must be eligible");
    };
    assert_eq!(metadata.media_id.get(), 9001);
    assert_eq!(metadata.artist, "Fixture photographer");
    assert_eq!(metadata.license_short_name, "CC BY-SA 4.0");
    assert_eq!((metadata.width, metadata.height), (640, 427));
    assert_eq!(metadata.mime_type, ThumbnailMimeType::Jpeg);

    let requests = server.finish();
    assert!(requests[0].contains("prop=imageinfo"));
    assert!(requests[0].contains("iiurlwidth=640"));
    assert!(requests[0].contains("iiurlheight=640"));
    assert!(requests[0].contains("extmetadata"));
    assert!(!requests[0].contains("redirects=1"));
}

#[tokio::test(flavor = "multi_thread")]
async fn accepts_only_explicit_title_normalization_and_rejects_unrelated_or_redirected_files() {
    let server = FixtureServer::start(vec![
        FixtureResponse::json(IMAGEINFO_NORMALIZED),
        FixtureResponse::json(IMAGEINFO_UNRELATED),
        FixtureResponse::json(IMAGEINFO_REDIRECT),
    ]);
    let client = client(&server);
    let mut placement = RevisionImagePlacement {
        index: 0,
        file_title: "File:Lead_photo.JPG".try_into().expect("file title"),
        caption: None,
        alt_text: None,
    };

    let normalized = client
        .resolve_thumbnail_metadata(&placement, policy(2, 1024))
        .await
        .expect("explicit normalization");
    assert!(matches!(
        normalized,
        ThumbnailMetadataResolution::Eligible(metadata)
            if metadata.file_title.as_str() == "File:Lead photo.JPG"
    ));

    placement.file_title = "File:Lead photo.JPG".try_into().expect("file title");
    assert!(matches!(
        client
            .resolve_thumbnail_metadata(&placement, policy(2, 1024))
            .await,
        Err(ClientError::InvalidResponse(
            "imageinfo response returned an unrelated file title"
        ))
    ));
    assert!(matches!(
        client
            .resolve_thumbnail_metadata(&placement, policy(2, 1024))
            .await,
        Err(ClientError::InvalidResponse(
            "imageinfo unexpectedly followed a file redirect"
        ))
    ));
    let requests = server.finish();
    assert!(
        requests
            .iter()
            .all(|request| !request.contains("redirects=1"))
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn treats_active_or_incomplete_metadata_as_nonfatal_ineligibility() {
    let server = FixtureServer::start(vec![
        FixtureResponse::json(IMAGEINFO_SVG),
        FixtureResponse::json(IMAGEINFO_INCOMPLETE),
    ]);
    let client = client(&server);
    let mut placement = RevisionImagePlacement {
        index: 0,
        file_title: "File:Disguised.png".try_into().expect("file title"),
        caption: None,
        alt_text: None,
    };
    assert_eq!(
        client
            .resolve_thumbnail_metadata(&placement, policy(2, 1024))
            .await
            .expect("SVG metadata response"),
        ThumbnailMetadataResolution::Ineligible(ThumbnailIneligibility::UnsupportedMimeType)
    );

    placement.file_title = "File:Incomplete.png".try_into().expect("file title");
    assert_eq!(
        client
            .resolve_thumbnail_metadata(&placement, policy(2, 1024))
            .await
            .expect("incomplete metadata response"),
        ThumbnailMetadataResolution::Ineligible(ThumbnailIneligibility::IncompleteMetadata)
    );
    assert_eq!(server.finish().len(), 2);
}

#[tokio::test(flavor = "multi_thread")]
async fn downloads_same_origin_bytes_through_the_bounded_transport() {
    let server = FixtureServer::start(vec![FixtureResponse::json("fixture-thumbnail")]);
    let thumbnail_url = server.endpoint().replace("/w/api.php", "/thumb.jpg");
    let client = client(&server);
    let bytes = client
        .download_thumbnail(&metadata(thumbnail_url), policy(1, 32))
        .await
        .expect("same-origin download");
    assert_eq!(bytes, b"fixture-thumbnail");
    let requests = server.finish();
    assert!(requests[0].starts_with("GET /thumb.jpg "));
}

#[tokio::test(flavor = "multi_thread")]
async fn rejects_cross_origin_urls_before_contact_and_redacts_the_error() {
    let server = FixtureServer::start(Vec::new());
    let client = client(&server);
    let secret_url = "https://upload.wikimedia.org/thumb.jpg?secret=token";
    let error = client
        .download_thumbnail(&metadata(secret_url.to_owned()), policy(1, 32))
        .await
        .expect_err("cross-origin URL must fail closed");
    assert_eq!(error, ThumbnailDownloadError::UrlRejected);
    assert!(!error.to_string().contains("secret"));
    assert!(server.finish().is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn rejects_cross_origin_redirect_before_contacting_destination() {
    let server = FixtureServer::start(vec![FixtureResponse::redirect(
        "http://localhost:9/private.jpg?secret=token".to_owned(),
    )]);
    let thumbnail_url = server.endpoint().replace("/w/api.php", "/thumb.jpg");
    let client = client(&server);
    let error = client
        .download_thumbnail(&metadata(thumbnail_url), policy(1, 32))
        .await
        .expect_err("redirect must fail closed");
    assert_eq!(error, ThumbnailDownloadError::UrlRejected);
    assert!(!error.is_retryable());
    assert!(!error.to_string().contains("secret"));
    assert_eq!(server.finish().len(), 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn rejects_same_host_redirect_to_a_different_port_before_contact() {
    let server = FixtureServer::start(vec![FixtureResponse::redirect(
        "http://127.0.0.1:9/private.jpg?secret=token".to_owned(),
    )]);
    let thumbnail_url = server.endpoint().replace("/w/api.php", "/thumb.jpg");
    let client = client(&server);
    let error = client
        .download_thumbnail(&metadata(thumbnail_url), policy(1, 32))
        .await
        .expect_err("port-changing redirect must fail closed");
    assert_eq!(error, ThumbnailDownloadError::UrlRejected);
    assert!(!error.to_string().contains("secret"));
    assert_eq!(server.finish().len(), 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn enforces_declared_per_image_and_shared_run_byte_limits() {
    let oversized_server = FixtureServer::start(vec![FixtureResponse::json("0123456789")]);
    let thumbnail_url = oversized_server
        .endpoint()
        .replace("/w/api.php", "/thumb.jpg");
    let client = client(&oversized_server);
    assert_eq!(
        client
            .download_thumbnail(&metadata(thumbnail_url), policy(1, 4))
            .await,
        Err(ThumbnailDownloadError::ImageBytesExceeded { limit: 4 })
    );
    oversized_server.finish();

    let budget_server = FixtureServer::start(vec![FixtureResponse::json("0123456789")]);
    let thumbnail_url = budget_server.endpoint().replace("/w/api.php", "/thumb.jpg");
    let config = ClientConfig::new(budget_server.endpoint(), "WikiSyncer/0.1 media-budget")
        .expect("fixture config")
        .with_max_downloaded_response_bytes_per_run(8)
        .expect("run budget")
        .with_retry_policy(
            RetryPolicy::new(1, Duration::from_millis(1), Duration::from_millis(1))
                .expect("retry policy"),
        );
    let client = MediaWikiClient::new(config).expect("fixture client");
    assert_eq!(
        client
            .download_thumbnail(&metadata(thumbnail_url), policy(1, 16))
            .await,
        Err(ThumbnailDownloadError::RunBudgetExceeded { limit: 8 })
    );
    budget_server.finish();
}
