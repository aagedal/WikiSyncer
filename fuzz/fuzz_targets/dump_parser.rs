#![no_main]

use std::io::Cursor;

use libfuzzer_sys::fuzz_target;
use wikisync_mediawiki::{DumpLimits, DumpReader};

const MAX_INPUT_BYTES: usize = 256 * 1024;

fuzz_target!(|data: &[u8]| {
    if data.len() > MAX_INPUT_BYTES {
        return;
    }

    let limits = DumpLimits {
        max_compressed_bytes: (data.len() as u64).saturating_add(1),
        max_decompressed_bytes: 256 * 1024,
        max_pages: 16,
        max_page_xml_bytes: 64 * 1024,
        max_text_bytes: 32 * 1024,
        max_metadata_field_bytes: 4 * 1024,
        max_siteinfo_bytes: 64 * 1024,
        max_namespaces: 64,
    };

    let Ok(mut dump) = DumpReader::new(Cursor::new(data), limits) else {
        return;
    };
    for item in dump.by_ref() {
        if item.is_err() {
            break;
        }
    }
    assert!(dump.pages_examined() <= limits.max_pages);
    assert!(dump.pages_yielded() <= dump.pages_examined());
    assert!(dump.decompressed_bytes_read() <= limits.max_decompressed_bytes);
});
