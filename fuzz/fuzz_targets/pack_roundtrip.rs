#![no_main]

use libfuzzer_sys::fuzz_target;
use tempfile::tempdir;
use wikisync_store::{Library, ObjectKind, StoreConfig};

const MAX_INPUT_BYTES: usize = 32 * 1024;
const VARIANT_COUNT: usize = 4;

fuzz_target!(|data: &[u8]| {
    if data.len() > MAX_INPUT_BYTES {
        return;
    }

    let expected = related_variants(data);
    let root = tempdir().expect("create temporary fuzz library");
    let config = StoreConfig::default()
        .with_max_object_bytes(64 * 1024)
        .and_then(|config| config.with_max_pack_objects(VARIANT_COUNT as u32))
        .and_then(|config| config.with_max_pack_input_bytes(256 * 1024))
        .expect("valid storage bounds");
    let mut library = Library::open_with_config(root.path(), config).expect("open fuzz library");

    let objects = expected
        .iter()
        .map(|bytes| {
            library
                .put_bytes(ObjectKind::Wikitext, bytes)
                .expect("store fuzz object")
        })
        .collect::<Vec<_>>();

    for (object, bytes) in objects.iter().zip(&expected) {
        assert_eq!(
            library.read_object(object.id).expect("read loose object"),
            *bytes
        );
    }

    let first = library
        .pack_loose_objects()
        .expect("build verified pack")
        .expect("objects produce a pack");
    assert_eq!(first.object_count as usize, objects.len());
    assert!(first.delta_entries > 0);
    library
        .prune_packed_loose_objects(&first.pack_id)
        .expect("prune loose copies");

    for (object, bytes) in objects.iter().zip(&expected) {
        assert_eq!(
            library
                .read_object(object.id)
                .expect("decode packed object"),
            *bytes
        );
    }

    let replacement = library
        .repack_pack(&first.pack_id)
        .expect("repack verified pack");
    assert_eq!(replacement.object_count, first.object_count);
    assert!(replacement.delta_entries > 0);
    library
        .retire_pack(&first.pack_id)
        .expect("retire superseded pack");

    for (object, bytes) in objects.iter().zip(&expected) {
        assert_eq!(
            library
                .read_object(object.id)
                .expect("reconstruct replacement pack object"),
            *bytes
        );
    }
});

fn related_variants(data: &[u8]) -> Vec<Vec<u8>> {
    // A deterministic, poorly compressible common base ensures the complete entry
    // is materially larger than the tiny one-byte deltas, including for empty input.
    let mut state = 0x9e37_79b9_u32;
    let mut base = (0..4096)
        .map(|_| {
            state ^= state << 13;
            state ^= state >> 17;
            state ^= state << 5;
            state as u8
        })
        .collect::<Vec<_>>();
    base.extend_from_slice(data);
    (0..VARIANT_COUNT)
        .map(|variant| {
            let mut bytes = base.clone();
            bytes[variant] = b'0' + variant as u8;
            bytes
        })
        .collect()
}
