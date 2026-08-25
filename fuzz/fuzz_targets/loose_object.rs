#![no_main]

use std::fs;
use std::path::{Path, PathBuf};

use libfuzzer_sys::fuzz_target;
use tempfile::tempdir;
use wikisync_store::{Library, ObjectId, ObjectKind, StoreConfig};

const MAX_INPUT_BYTES: usize = 64 * 1024;
const CANONICAL_BYTES: &[u8] = b"bounded loose-object decompression fixture\n";

fuzz_target!(|data: &[u8]| {
    if data.len() > MAX_INPUT_BYTES {
        return;
    }

    let root = tempdir().expect("create temporary fuzz library");
    let config = StoreConfig::default()
        .with_max_object_bytes(4 * 1024)
        .expect("valid object bound");
    let mut library = Library::open_with_config(root.path(), config).expect("open fuzz library");
    let object = library
        .put_bytes(ObjectKind::Wikitext, CANONICAL_BYTES)
        .expect("install seed object");

    fs::write(loose_path(root.path(), object.id), data).expect("replace loose representation");
    if let Ok(decoded) = library.read_object(object.id) {
        assert_eq!(decoded, CANONICAL_BYTES);
    }
});

fn loose_path(root: &Path, id: ObjectId) -> PathBuf {
    let encoded = id.to_string();
    let digest = encoded.strip_prefix("b3:").expect("object ID prefix");
    root.join("objects")
        .join("loose")
        .join("b3")
        .join(&digest[0..2])
        .join(&digest[2..4])
        .join(digest)
}
