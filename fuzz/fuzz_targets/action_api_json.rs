#![no_main]

use libfuzzer_sys::fuzz_target;
use wikisync_mediawiki::{ActionApiResponseKind, validate_action_api_response};

const MAX_INPUT_BYTES: usize = 256 * 1024;

fuzz_target!(|data: &[u8]| {
    if data.len() > MAX_INPUT_BYTES {
        return;
    }

    let (selector, json) = data
        .split_first()
        .map_or((0, &[][..]), |(&selector, json)| (selector, json));
    let kind = response_kind(selector);
    let _ = validate_action_api_response(json, kind);
});

fn response_kind(selector: u8) -> ActionApiResponseKind {
    match selector {
        b'T' | b't' => ActionApiResponseKind::TitleResolution,
        b'H' | b'h' => ActionApiResponseKind::PageHead,
        b'B' | b'b' => ActionApiResponseKind::RevisionBatch,
        b'C' | b'c' => ActionApiResponseKind::RevisionContent,
        b'M' | b'm' => ActionApiResponseKind::CategoryMembers,
        b'I' | b'i' => ActionApiResponseKind::RevisionImages,
        b'N' | b'n' => ActionApiResponseKind::ThumbnailMetadata,
        other => match other % 7 {
            0 => ActionApiResponseKind::TitleResolution,
            1 => ActionApiResponseKind::PageHead,
            2 => ActionApiResponseKind::RevisionBatch,
            3 => ActionApiResponseKind::RevisionContent,
            4 => ActionApiResponseKind::CategoryMembers,
            5 => ActionApiResponseKind::RevisionImages,
            _ => ActionApiResponseKind::ThumbnailMetadata,
        },
    }
}
