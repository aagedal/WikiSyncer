use std::io::Write;

use bzip2::Compression;
use bzip2::write::BzEncoder;
use wikisync_mediawiki::{DumpError, DumpFilter, DumpLimits, DumpReader};

const CURRENT_PAGES: &[u8] = include_bytes!("../../../fixtures/mediawiki/pages-meta-current.xml");

#[test]
fn reads_concatenated_bzip2_members_and_yields_filtered_current_pages() {
    let compressed = multistream_fixture(CURRENT_PAGES);
    let mut reader =
        DumpReader::new(compressed.as_slice(), DumpLimits::default()).expect("bounded dump reader");

    assert_eq!(reader.site_info().database_name, "enwiki");
    assert_eq!(reader.site_info().language_code, "en");
    assert_eq!(reader.site_info().export_version, "0.11");
    assert_eq!(
        reader.site_info().export_schema,
        "http://www.mediawiki.org/xml/export-0.11/"
    );
    assert_eq!(reader.site_info().case_rule, "first-letter");
    assert_eq!(reader.site_info().namespaces.len(), 2);
    assert_eq!(reader.site_info().namespaces[0].key, 0);
    assert_eq!(reader.site_info().namespaces[1].name, "Talk");

    let first = reader.next().expect("first record").expect("first page");
    assert_eq!(first.page_id.get(), 10);
    assert_eq!(first.namespace, 0);
    assert_eq!(first.title.as_str(), "Alpha");
    assert_eq!(first.revision.metadata.revision_id.get(), 100);
    assert_eq!(first.revision.metadata.parent_id.unwrap().get(), 99);
    assert_eq!(
        first.revision.metadata.user.as_deref(),
        Some("Fixture editor")
    );
    assert_eq!(first.revision.metadata.user_id, Some(42));
    assert_eq!(
        first.revision.metadata.comment.as_deref(),
        Some("Entity & comment")
    );
    assert!(first.revision.metadata.minor);
    assert_eq!(first.revision.metadata.size, Some(12));
    assert_eq!(
        first.revision.metadata.content_model.as_deref(),
        Some("wikitext")
    );
    assert_eq!(first.revision.content_format, "text/x-wiki");
    assert_eq!(
        first.revision.source.as_deref(),
        Some(b"Alpha & beta".as_slice())
    );

    let second = reader.next().expect("second record").expect("second page");
    assert_eq!(second.page_id.get(), 12);
    assert_eq!(second.redirect_title.unwrap().as_str(), "Alpha");
    assert_eq!(second.revision.metadata.user, None);
    assert_eq!(second.revision.metadata.comment, None);
    assert_eq!(second.revision.source, None);

    assert!(reader.next().is_none());
    assert_eq!(reader.pages_examined(), 4);
    assert_eq!(reader.pages_yielded(), 2);
}

#[test]
fn explicit_filter_can_select_other_namespaces_and_content_models() {
    let compressed = multistream_fixture(CURRENT_PAGES);
    let filter = DumpFilter::new([0, 1], ["json"]).expect("filter");
    let pages = DumpReader::with_filter(compressed.as_slice(), DumpLimits::default(), filter)
        .expect("reader")
        .collect::<Result<Vec<_>, _>>()
        .expect("complete scan");

    assert_eq!(pages.len(), 1);
    assert_eq!(pages[0].title.as_str(), "Data model");
    assert_eq!(pages[0].revision.source.as_deref(), Some(b"{}".as_slice()));
}

#[test]
fn page_text_and_count_limits_fail_before_accepting_excess_records() {
    let compressed = multistream_fixture(CURRENT_PAGES);
    let text_limits = DumpLimits {
        max_text_bytes: 5,
        ..DumpLimits::default()
    };
    let mut text_reader =
        DumpReader::new(compressed.as_slice(), text_limits).expect("siteinfo stays within bounds");
    assert!(matches!(
        text_reader.next(),
        Some(Err(DumpError::TextTooLarge { limit: 5 }))
    ));
    assert!(text_reader.next().is_none(), "reader fuses after failure");

    let count_limits = DumpLimits {
        max_pages: 1,
        ..DumpLimits::default()
    };
    let mut count_reader = DumpReader::new(compressed.as_slice(), count_limits).expect("reader");
    assert!(count_reader.next().unwrap().is_ok());
    assert!(matches!(
        count_reader.next(),
        Some(Err(DumpError::PageLimitExceeded { limit: 1 }))
    ));

    let page_limits = DumpLimits {
        max_page_xml_bytes: 300,
        max_text_bytes: 256,
        ..DumpLimits::default()
    };
    let mut page_reader = DumpReader::new(compressed.as_slice(), page_limits).expect("reader");
    assert!(matches!(
        page_reader.next(),
        Some(Err(DumpError::PageTooLarge { limit: 300 }))
    ));
}

#[test]
fn compressed_decompressed_and_declared_size_limits_are_distinct() {
    let compressed = multistream_fixture(CURRENT_PAGES);
    let compressed_limits = DumpLimits {
        max_compressed_bytes: u64::try_from(compressed.len() - 1).unwrap(),
        ..DumpLimits::default()
    };
    let error = drain_error(compressed.as_slice(), compressed_limits);
    assert!(matches!(error, DumpError::CompressedLimitExceeded { .. }));

    let decompressed_limits = DumpLimits {
        max_decompressed_bytes: 700,
        max_siteinfo_bytes: 700,
        ..DumpLimits::default()
    };
    let error = drain_error(compressed.as_slice(), decompressed_limits);
    assert!(matches!(
        error,
        DumpError::DecompressedLimitExceeded { limit: 700 }
            | DumpError::SiteInfoTooLarge { limit: 700 }
    ));

    let mismatched = String::from_utf8(CURRENT_PAGES.to_vec()).unwrap().replacen(
        "<text bytes=\"12\"",
        "<text bytes=\"11\"",
        1,
    );
    let mismatched = multistream_fixture(mismatched.as_bytes());
    let error = drain_error(mismatched.as_slice(), DumpLimits::default());
    assert!(matches!(
        error,
        DumpError::TextSizeMismatch {
            declared: 11,
            actual: 12
        }
    ));
}

#[test]
fn rejects_non_utf8_declarations_and_invalid_utf8_character_data() {
    let non_utf8_declaration = String::from_utf8(CURRENT_PAGES.to_vec()).unwrap().replacen(
        "encoding=\"utf-8\"",
        "encoding=\"iso-8859-1\"",
        1,
    );
    let compressed = multistream_fixture(non_utf8_declaration.as_bytes());
    assert!(matches!(
        DumpReader::new(compressed.as_slice(), DumpLimits::default()),
        Err(DumpError::UnsupportedEncoding)
    ));

    let mut invalid = CURRENT_PAGES.to_vec();
    let offset = invalid
        .windows(b"Alpha &amp; beta".len())
        .position(|window| window == b"Alpha &amp; beta")
        .expect("fixture text");
    invalid[offset] = 0xff;
    let compressed = multistream_fixture(&invalid);
    let error = drain_error(compressed.as_slice(), DumpLimits::default());
    assert!(matches!(error, DumpError::Xml(_)));

    let nested_text = String::from_utf8(CURRENT_PAGES.to_vec()).unwrap().replacen(
        "Alpha &amp; beta",
        "<b>bad</b>",
        1,
    );
    let compressed = multistream_fixture(nested_text.as_bytes());
    let error = drain_error(compressed.as_slice(), DumpLimits::default());
    assert!(matches!(error, DumpError::InvalidStructure(_)));
}

#[test]
fn rejects_duplicate_scalar_metadata_fields() {
    for (original, duplicate) in [
        (
            "<dbname>enwiki</dbname>",
            "<dbname>enwiki</dbname><dbname>enwiki</dbname>",
        ),
        (
            "<title>Alpha</title>",
            "<title>Alpha</title><title>Alpha</title>",
        ),
        ("<id>10</id>", "<id>10</id><id>10</id>"),
        ("<id>100</id>", "<id>100</id><id>100</id>"),
        (
            "<model>wikitext</model>",
            "<model>wikitext</model><model>wikitext</model>",
        ),
    ] {
        let xml = String::from_utf8(CURRENT_PAGES.to_vec())
            .unwrap()
            .replacen(original, duplicate, 1);
        let compressed = multistream_fixture(xml.as_bytes());
        assert!(matches!(
            drain_error(compressed.as_slice(), DumpLimits::default()),
            DumpError::InvalidStructure(_)
        ));
    }
}

#[test]
fn rejects_nested_scalar_markup_but_accepts_chunked_character_data() {
    for (original, nested) in [
        (
            "<dbname>enwiki</dbname>",
            "<dbname>en<em>wiki</em></dbname>",
        ),
        ("<title>Alpha</title>", "<title>Al<em>pha</em></title>"),
        ("<id>10</id>", "<id>1<em>0</em></id>"),
        ("<id>100</id>", "<id>1<em>00</em></id>"),
        (
            "<model>wikitext</model>",
            "<model>wiki<em>text</em></model>",
        ),
    ] {
        let xml = String::from_utf8(CURRENT_PAGES.to_vec())
            .unwrap()
            .replacen(original, nested, 1);
        let compressed = multistream_fixture(xml.as_bytes());
        assert!(matches!(
            drain_error(compressed.as_slice(), DumpLimits::default()),
            DumpError::InvalidStructure(_)
        ));
    }

    let chunked = String::from_utf8(CURRENT_PAGES.to_vec()).unwrap().replacen(
        "<title>Alpha</title>",
        "<title>Al&#112;ha</title>",
        1,
    );
    let compressed = multistream_fixture(chunked.as_bytes());
    let mut reader = DumpReader::new(compressed.as_slice(), DumpLimits::default()).expect("reader");
    let page = reader.next().expect("first record").expect("first page");
    assert_eq!(page.title.as_str(), "Alpha");
}

#[test]
fn deleted_nonempty_contributors_never_expose_identity() {
    let nonempty = String::from_utf8(CURRENT_PAGES.to_vec()).unwrap().replacen(
        "<contributor deleted=\"deleted\" />",
        "<contributor deleted=\"deleted\">\n      </contributor>",
        1,
    );
    let compressed = multistream_fixture(nonempty.as_bytes());
    let pages = DumpReader::new(compressed.as_slice(), DumpLimits::default())
        .expect("reader")
        .collect::<Result<Vec<_>, _>>()
        .expect("complete scan");
    let revision = &pages
        .iter()
        .find(|page| page.title.as_str() == "Redirected")
        .expect("redirected page")
        .revision;
    assert_eq!(revision.metadata.user, None);
    assert_eq!(revision.metadata.user_id, None);

    for identity in [
        "<username>Suppressed</username>",
        "<ip>192.0.2.99</ip>",
        "<id>999</id>",
    ] {
        let contributor = format!("<contributor deleted=\"deleted\">{identity}</contributor>");
        let xml = String::from_utf8(CURRENT_PAGES.to_vec()).unwrap().replacen(
            "<contributor deleted=\"deleted\" />",
            &contributor,
            1,
        );
        let compressed = multistream_fixture(xml.as_bytes());
        assert!(matches!(
            drain_error(compressed.as_slice(), DumpLimits::default()),
            DumpError::InvalidStructure(_)
        ));
    }
}

#[test]
fn corrupted_bzip2_member_fails_closed() {
    let mut compressed = multistream_fixture(CURRENT_PAGES);
    let offset = compressed.len() / 3;
    compressed[offset] ^= 0x5a;
    let error = drain_error(compressed.as_slice(), DumpLimits::default());
    assert!(matches!(error, DumpError::Xml(_)));
}

#[test]
fn filters_and_limits_reject_unsafe_zero_or_empty_configuration() {
    assert!(DumpFilter::new([], ["wikitext"]).is_err());
    assert!(DumpFilter::new([0], Vec::<String>::new()).is_err());
    assert!(DumpFilter::new([0], ["bad\nmodel"]).is_err());

    let compressed = multistream_fixture(CURRENT_PAGES);
    let invalid = DumpLimits {
        max_text_bytes: 0,
        ..DumpLimits::default()
    };
    assert!(matches!(
        DumpReader::new(compressed.as_slice(), invalid),
        Err(DumpError::InvalidLimit(_))
    ));
}

fn drain_error(input: &[u8], limits: DumpLimits) -> DumpError {
    match DumpReader::new(input, limits) {
        Err(error) => error,
        Ok(mut reader) => loop {
            match reader.next() {
                Some(Ok(_)) => {}
                Some(Err(error)) => break error,
                None => panic!("fixture unexpectedly completed without an error"),
            }
        },
    }
}

fn multistream_fixture(xml: &[u8]) -> Vec<u8> {
    let split = xml
        .windows(b"  <page>\n    <title>Talk:Filtered".len())
        .position(|window| window == b"  <page>\n    <title>Talk:Filtered")
        .expect("fixture split point");
    let mut compressed = compress_member(&xml[..split]);
    compressed.extend(compress_member(&xml[split..]));
    compressed
}

fn compress_member(bytes: &[u8]) -> Vec<u8> {
    let mut encoder = BzEncoder::new(Vec::new(), Compression::best());
    encoder.write_all(bytes).expect("compress fixture member");
    encoder.finish().expect("finish fixture member")
}
