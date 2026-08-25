#![no_main]

use libfuzzer_sys::fuzz_target;
use wikisync_integrity::{MAX_TRUSTED_HEAD_BYTES, TrustedManifestHead};

fuzz_target!(|data: &[u8]| {
    if data.len() > MAX_TRUSTED_HEAD_BYTES + 1 {
        return;
    }

    let Ok(anchor) = TrustedManifestHead::from_canonical_json(data) else {
        return;
    };
    let encoded = anchor
        .to_canonical_json()
        .expect("a parsed anchor re-encodes");
    assert_eq!(encoded, data);

    let reparsed =
        TrustedManifestHead::from_canonical_json(&encoded).expect("canonical output parses again");
    assert_eq!(reparsed, anchor);
});
