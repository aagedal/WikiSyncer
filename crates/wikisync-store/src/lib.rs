//! SQLite metadata and immutable content-object storage.
//!
//! Logical [`ObjectId`] values contain no physical location information. New bytes
//! are compressed into a temporary file, made durable, and atomically installed
//! before the SQLite transaction records their location.

use std::error::Error;
use std::fmt;
use std::fs::{self, File};
use std::io::{self, Read, Write};
use std::path::{Component, Path, PathBuf};
use std::str::FromStr;
use std::time::{SystemTime, UNIX_EPOCH};

use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};

const MIGRATION_1: &str = include_str!("../migrations/0001_initial.sql");
const OBJECT_DOMAIN: &[u8] = b"wikisync-object-v1\0";
const DATABASE_NAME: &str = "library.sqlite3";
const COPY_BUFFER_SIZE: usize = 64 * 1024;

/// Default upper bound for one uncompressed canonical object (64 MiB).
pub const DEFAULT_MAX_OBJECT_BYTES: u64 = 64 * 1024 * 1024;

/// Configuration for a [`Library`].
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct StoreConfig {
    max_object_bytes: u64,
    compression_level: i32,
}

impl Default for StoreConfig {
    fn default() -> Self {
        Self {
            max_object_bytes: DEFAULT_MAX_OBJECT_BYTES,
            compression_level: 3,
        }
    }
}

impl StoreConfig {
    /// Sets the maximum accepted uncompressed object length.
    pub fn with_max_object_bytes(mut self, bytes: u64) -> Result<Self, StoreError> {
        if bytes == 0 {
            return Err(StoreError::InvalidConfig(
                "maximum object size must be greater than zero",
            ));
        }
        self.max_object_bytes = bytes;
        Ok(self)
    }

    /// Sets the Zstandard compression level used for new loose objects.
    pub fn with_compression_level(mut self, level: i32) -> Result<Self, StoreError> {
        if !(-7..=22).contains(&level) {
            return Err(StoreError::InvalidConfig(
                "Zstandard compression level must be between -7 and 22",
            ));
        }
        self.compression_level = level;
        Ok(self)
    }
}

/// Canonical object categories. The category participates in logical identity.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum ObjectKind {
    /// Exact MediaWiki revision source text.
    Wikitext,
    /// Captured binary media.
    Media,
}

impl ObjectKind {
    const fn identity_tag(self) -> u8 {
        match self {
            Self::Wikitext => 1,
            Self::Media => 2,
        }
    }

    const fn database_value(self) -> &'static str {
        match self {
            Self::Wikitext => "wikitext",
            Self::Media => "media",
        }
    }

    const fn default_media_type(self) -> &'static str {
        match self {
            Self::Wikitext => "text/x-wiki; charset=utf-8",
            Self::Media => "application/octet-stream",
        }
    }

    fn from_database(value: &str) -> Result<Self, StoreError> {
        match value {
            "wikitext" => Ok(Self::Wikitext),
            "media" => Ok(Self::Media),
            _ => Err(StoreError::CorruptMetadata("unknown object kind")),
        }
    }
}

/// A versioned, domain-separated BLAKE3 identity for canonical bytes.
#[derive(Clone, Copy, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct ObjectId([u8; 32]);

impl ObjectId {
    /// Computes an object identity without writing it.
    #[must_use]
    pub fn for_bytes(kind: ObjectKind, bytes: &[u8]) -> Self {
        let length = u64::try_from(bytes.len()).expect("slice length fits in u64");
        let mut hasher = object_hasher(kind, length);
        hasher.update(bytes);
        Self(*hasher.finalize().as_bytes())
    }

    /// Returns the raw 32-byte BLAKE3 digest.
    #[must_use]
    pub const fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }

    fn digest_hex(self) -> String {
        blake3::Hash::from_bytes(self.0).to_hex().to_string()
    }
}

impl fmt::Debug for ObjectId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(self, formatter)
    }
}

impl fmt::Display for ObjectId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "b3:{}", self.digest_hex())
    }
}

impl FromStr for ObjectId {
    type Err = InvalidObjectId;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        let digest = value.strip_prefix("b3:").ok_or(InvalidObjectId)?;
        let hash = blake3::Hash::from_hex(digest).map_err(|_| InvalidObjectId)?;
        Ok(Self(*hash.as_bytes()))
    }
}

/// An object ID did not use the supported `b3:` form.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct InvalidObjectId;

impl fmt::Display for InvalidObjectId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("object ID must be b3: followed by 64 hexadecimal characters")
    }
}

impl Error for InvalidObjectId {}

/// Metadata returned after an object is durably installed.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StoredObject {
    /// Stable logical identity.
    pub id: ObjectId,
    /// Canonical object category.
    pub kind: ObjectKind,
    /// Length of the canonical, uncompressed bytes.
    pub uncompressed_length: u64,
}

/// One WikiSyncer library and its writer connection.
#[derive(Debug)]
pub struct Library {
    root: PathBuf,
    connection: Connection,
    config: StoreConfig,
}

impl Library {
    /// Opens or creates a library using default object bounds.
    pub fn open(root: impl AsRef<Path>) -> Result<Self, StoreError> {
        Self::open_with_config(root, StoreConfig::default())
    }

    /// Opens or creates a library, configures SQLite, and applies migrations.
    pub fn open_with_config(
        root: impl AsRef<Path>,
        config: StoreConfig,
    ) -> Result<Self, StoreError> {
        let root = root.as_ref().to_path_buf();
        fs::create_dir_all(root.join("objects/loose/b3"))?;
        fs::create_dir_all(root.join("objects/packs"))?;
        fs::create_dir_all(root.join("tmp"))?;

        let connection = Connection::open(root.join(DATABASE_NAME))?;
        connection.pragma_update(None, "foreign_keys", true)?;
        connection.pragma_update(None, "journal_mode", "WAL")?;
        connection.pragma_update(None, "synchronous", "FULL")?;
        connection.busy_timeout(std::time::Duration::from_secs(5))?;
        migrate(&connection)?;

        Ok(Self {
            root,
            connection,
            config,
        })
    }

    /// Returns the library root directory.
    #[must_use]
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Stores an in-memory canonical object.
    pub fn put_bytes(
        &mut self,
        kind: ObjectKind,
        bytes: &[u8],
    ) -> Result<StoredObject, StoreError> {
        let length = u64::try_from(bytes.len()).expect("slice length fits in u64");
        self.put_reader(kind, length, bytes)
    }

    /// Streams exactly `expected_length` canonical bytes into a durable loose object.
    ///
    /// No SQLite row is created if the reader fails or returns a different length.
    /// The expected length and configured maximum prevent an unbounded decompression
    /// or source stream from filling the library.
    pub fn put_reader(
        &mut self,
        kind: ObjectKind,
        expected_length: u64,
        mut reader: impl Read,
    ) -> Result<StoredObject, StoreError> {
        if expected_length > self.config.max_object_bytes {
            return Err(StoreError::ObjectTooLarge {
                limit: self.config.max_object_bytes,
                actual: expected_length,
            });
        }

        let mut temporary = tempfile::Builder::new()
            .prefix("object-")
            .suffix(".tmp")
            .tempfile_in(self.root.join("tmp"))?;
        let mut hasher = object_hasher(kind, expected_length);
        let mut encoder = zstd::stream::write::Encoder::new(
            temporary.as_file_mut(),
            self.config.compression_level,
        )?;
        let mut buffer = [0_u8; COPY_BUFFER_SIZE];
        let mut actual_length = 0_u64;

        loop {
            let count = reader.read(&mut buffer)?;
            if count == 0 {
                break;
            }
            actual_length =
                actual_length
                    .checked_add(count as u64)
                    .ok_or(StoreError::ObjectTooLarge {
                        limit: self.config.max_object_bytes,
                        actual: u64::MAX,
                    })?;
            if actual_length > expected_length || actual_length > self.config.max_object_bytes {
                return Err(StoreError::LengthMismatch {
                    expected: expected_length,
                    actual: actual_length,
                });
            }
            hasher.update(&buffer[..count]);
            encoder.write_all(&buffer[..count])?;
        }
        encoder.finish()?;

        if actual_length != expected_length {
            return Err(StoreError::LengthMismatch {
                expected: expected_length,
                actual: actual_length,
            });
        }

        temporary.as_file().sync_all()?;
        let compressed_length = temporary.as_file().metadata()?.len();
        let id = ObjectId(*hasher.finalize().as_bytes());
        let relative_path = loose_relative_path(id);
        let absolute_path = self.root.join(&relative_path);
        let parent = absolute_path.parent().ok_or(StoreError::CorruptMetadata(
            "loose object path has no parent",
        ))?;
        fs::create_dir_all(parent)?;

        // Replacing an existing path is safe: the content-derived target name can
        // only be reached by the same canonical bytes. `persist` uses an atomic
        // rename on the supported macOS and Linux filesystems, so readers observe
        // either complete representation rather than a partially copied file.
        temporary
            .persist(&absolute_path)
            .map_err(|error| StoreError::Io(error.error))?;
        sync_directory(parent)?;

        let now = unix_time()?;
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        transaction.execute(
            "INSERT OR IGNORE INTO content_objects (
                object_id, object_kind, uncompressed_length, media_type,
                verification_state, created_at
             ) VALUES (?1, ?2, ?3, ?4, 'verified', ?5)",
            params![
                id.to_string(),
                kind.database_value(),
                to_sql_integer(expected_length)?,
                kind.default_media_type(),
                now,
            ],
        )?;

        let existing: (String, i64) = transaction.query_row(
            "SELECT object_kind, uncompressed_length
             FROM content_objects WHERE object_id = ?1",
            [id.to_string()],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )?;
        if existing.0 != kind.database_value() || existing.1 != to_sql_integer(expected_length)? {
            return Err(StoreError::CorruptMetadata(
                "object ID is associated with conflicting metadata",
            ));
        }

        transaction.execute(
            "INSERT OR IGNORE INTO object_locations (
                object_id, storage_kind, encoding, relative_path, compressed_length,
                verification_state, created_at
             ) VALUES (?1, 'loose', 'zstd', ?2, ?3, 'verified', ?4)",
            params![
                id.to_string(),
                path_to_database(&relative_path)?,
                to_sql_integer(compressed_length)?,
                now,
            ],
        )?;
        transaction.commit()?;

        Ok(StoredObject {
            id,
            kind,
            uncompressed_length: expected_length,
        })
    }

    /// Returns whether verified metadata and a physical location are recorded.
    pub fn contains(&self, id: ObjectId) -> Result<bool, StoreError> {
        let exists = self.connection.query_row(
            "SELECT EXISTS (
                SELECT 1 FROM content_objects AS objects
                JOIN object_locations AS locations USING (object_id)
                WHERE objects.object_id = ?1
                  AND objects.verification_state = 'verified'
                  AND locations.verification_state = 'verified'
             )",
            [id.to_string()],
            |row| row.get(0),
        )?;
        Ok(exists)
    }

    /// Reads, bounds, decompresses, and verifies a canonical object.
    pub fn read_object(&self, id: ObjectId) -> Result<Vec<u8>, StoreError> {
        let metadata = self
            .connection
            .query_row(
                "SELECT objects.object_kind, objects.uncompressed_length,
                        locations.relative_path
                 FROM content_objects AS objects
                 JOIN object_locations AS locations USING (object_id)
                 WHERE objects.object_id = ?1
                   AND objects.verification_state = 'verified'
                   AND locations.storage_kind = 'loose'
                   AND locations.encoding = 'zstd'
                   AND locations.verification_state = 'verified'
                 ORDER BY locations.location_id DESC LIMIT 1",
                [id.to_string()],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, i64>(1)?,
                        row.get::<_, String>(2)?,
                    ))
                },
            )
            .optional()?
            .ok_or(StoreError::ObjectNotFound(id))?;

        let kind = ObjectKind::from_database(&metadata.0)?;
        let expected_length = u64::try_from(metadata.1)
            .map_err(|_| StoreError::CorruptMetadata("negative object length"))?;
        if expected_length > self.config.max_object_bytes {
            return Err(StoreError::ObjectTooLarge {
                limit: self.config.max_object_bytes,
                actual: expected_length,
            });
        }
        let relative_path = database_path(&metadata.2)?;
        let file = File::open(self.root.join(relative_path))?;
        let decoder = zstd::stream::read::Decoder::new(file)?;
        let read_limit = expected_length
            .checked_add(1)
            .ok_or(StoreError::CorruptMetadata("object length overflow"))?;
        let mut bytes = Vec::with_capacity(
            usize::try_from(expected_length)
                .map_err(|_| StoreError::CorruptMetadata("object is too large for this host"))?,
        );
        decoder.take(read_limit).read_to_end(&mut bytes)?;
        let actual_length = bytes.len() as u64;
        if actual_length != expected_length {
            return Err(StoreError::LengthMismatch {
                expected: expected_length,
                actual: actual_length,
            });
        }
        if ObjectId::for_bytes(kind, &bytes) != id {
            return Err(StoreError::HashMismatch(id));
        }
        Ok(bytes)
    }

    /// Returns the current SQLite schema version.
    pub fn schema_version(&self) -> Result<u32, StoreError> {
        self.connection
            .query_row("PRAGMA user_version", [], |row| row.get(0))
            .map_err(Into::into)
    }

    #[cfg(test)]
    fn connection(&self) -> &Connection {
        &self.connection
    }
}

fn object_hasher(kind: ObjectKind, length: u64) -> blake3::Hasher {
    let mut hasher = blake3::Hasher::new();
    hasher.update(OBJECT_DOMAIN);
    hasher.update(&[kind.identity_tag()]);
    hasher.update(&length.to_be_bytes());
    hasher
}

fn loose_relative_path(id: ObjectId) -> PathBuf {
    let digest = id.digest_hex();
    PathBuf::from("objects")
        .join("loose")
        .join("b3")
        .join(&digest[0..2])
        .join(&digest[2..4])
        .join(digest)
}

fn path_to_database(path: &Path) -> Result<String, StoreError> {
    if !is_safe_relative_path(path) {
        return Err(StoreError::CorruptMetadata("unsafe object location"));
    }
    path.to_str()
        .map(str::to_owned)
        .ok_or(StoreError::CorruptMetadata("object path is not UTF-8"))
}

fn database_path(value: &str) -> Result<PathBuf, StoreError> {
    let path = PathBuf::from(value);
    if !is_safe_relative_path(&path) || !path.starts_with("objects/loose/b3") {
        return Err(StoreError::CorruptMetadata("unsafe object location"));
    }
    Ok(path)
}

fn is_safe_relative_path(path: &Path) -> bool {
    !path.as_os_str().is_empty()
        && path
            .components()
            .all(|component| matches!(component, Component::Normal(_)))
}

fn migrate(connection: &Connection) -> Result<(), StoreError> {
    connection.execute_batch(
        "CREATE TABLE IF NOT EXISTS schema_migrations (
            version INTEGER PRIMARY KEY NOT NULL CHECK (version > 0),
            name TEXT NOT NULL,
            applied_at INTEGER NOT NULL
         ) STRICT;",
    )?;
    let version: u32 = connection.query_row("PRAGMA user_version", [], |row| row.get(0))?;
    if version > 1 {
        return Err(StoreError::UnsupportedSchemaVersion(version));
    }
    if version == 0 {
        let transaction = connection.unchecked_transaction()?;
        transaction.execute_batch(MIGRATION_1)?;
        transaction.execute(
            "INSERT INTO schema_migrations (version, name, applied_at)
             VALUES (1, 'initial', ?1)",
            [unix_time()?],
        )?;
        transaction.pragma_update(None, "user_version", 1)?;
        transaction.commit()?;
    }
    Ok(())
}

fn unix_time() -> Result<i64, StoreError> {
    let seconds = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|_| StoreError::ClockBeforeUnixEpoch)?
        .as_secs();
    to_sql_integer(seconds)
}

fn to_sql_integer(value: u64) -> Result<i64, StoreError> {
    i64::try_from(value).map_err(|_| StoreError::IntegerOutOfRange(value))
}

fn sync_directory(path: &Path) -> Result<(), StoreError> {
    File::open(path)?.sync_all()?;
    Ok(())
}

/// Storage, validation, or integrity failure.
#[derive(Debug)]
pub enum StoreError {
    /// Filesystem or stream failure.
    Io(io::Error),
    /// SQLite failure.
    Sqlite(rusqlite::Error),
    /// A configuration value was invalid.
    InvalidConfig(&'static str),
    /// An object exceeded the configured bound.
    ObjectTooLarge {
        /// Configured maximum.
        limit: u64,
        /// Observed or declared length.
        actual: u64,
    },
    /// A bounded stream or decoded object had an unexpected size.
    LengthMismatch {
        /// Declared metadata length.
        expected: u64,
        /// Number of bytes observed before stopping.
        actual: u64,
    },
    /// No verified physical representation was recorded for the ID.
    ObjectNotFound(ObjectId),
    /// Canonical bytes did not reproduce their logical identity.
    HashMismatch(ObjectId),
    /// Stored metadata violated a library invariant.
    CorruptMetadata(&'static str),
    /// The database was created by a newer application schema.
    UnsupportedSchemaVersion(u32),
    /// A `u64` value cannot be represented by SQLite's signed integer.
    IntegerOutOfRange(u64),
    /// The system clock cannot provide a conventional capture timestamp.
    ClockBeforeUnixEpoch,
}

impl fmt::Display for StoreError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(error) => write!(formatter, "I/O error: {error}"),
            Self::Sqlite(error) => write!(formatter, "SQLite error: {error}"),
            Self::InvalidConfig(message) => formatter.write_str(message),
            Self::ObjectTooLarge { limit, actual } => {
                write!(
                    formatter,
                    "object is {actual} bytes; limit is {limit} bytes"
                )
            }
            Self::LengthMismatch { expected, actual } => {
                write!(
                    formatter,
                    "expected {expected} object bytes, observed {actual}"
                )
            }
            Self::ObjectNotFound(id) => write!(formatter, "object {id} was not found"),
            Self::HashMismatch(id) => write!(formatter, "object {id} failed hash verification"),
            Self::CorruptMetadata(message) => write!(formatter, "corrupt metadata: {message}"),
            Self::UnsupportedSchemaVersion(version) => {
                write!(
                    formatter,
                    "database schema version {version} is newer than supported"
                )
            }
            Self::IntegerOutOfRange(value) => {
                write!(formatter, "value {value} exceeds SQLite integer range")
            }
            Self::ClockBeforeUnixEpoch => formatter.write_str("system clock is before Unix epoch"),
        }
    }
}

impl Error for StoreError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Io(error) => Some(error),
            Self::Sqlite(error) => Some(error),
            _ => None,
        }
    }
}

impl From<io::Error> for StoreError {
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}

impl From<rusqlite::Error> for StoreError {
    fn from(error: rusqlite::Error) -> Self {
        Self::Sqlite(error)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_library() -> (tempfile::TempDir, Library) {
        let directory = tempfile::tempdir().expect("temporary library");
        let library = Library::open(directory.path()).expect("open library");
        (directory, library)
    }

    #[test]
    fn migration_is_applied_once_and_keeps_locations_separate() {
        let (directory, library) = test_library();
        assert_eq!(library.schema_version().expect("schema version"), 1);
        assert_eq!(migration_count(&library), 1);

        drop(library);
        let reopened = Library::open(directory.path()).expect("reopen library");
        assert_eq!(reopened.schema_version().expect("schema version"), 1);
        assert_eq!(migration_count(&reopened), 1);

        let logical_columns: Vec<String> = reopened
            .connection()
            .prepare("PRAGMA table_info(content_objects)")
            .expect("prepare table info")
            .query_map([], |row| row.get(1))
            .expect("query columns")
            .collect::<Result<_, _>>()
            .expect("collect columns");
        assert!(
            !logical_columns
                .iter()
                .any(|column| column == "relative_path")
        );
    }

    #[test]
    fn object_identity_is_versioned_and_kind_separated() {
        let source = b"== Rust ==\nA language.";
        let wikitext = ObjectId::for_bytes(ObjectKind::Wikitext, source);
        let media = ObjectId::for_bytes(ObjectKind::Media, source);

        assert_ne!(wikitext, media);
        assert_eq!(
            wikitext.to_string().parse::<ObjectId>().expect("parse ID"),
            wikitext
        );
    }

    #[test]
    fn loose_objects_round_trip_and_deduplicate() {
        let (_directory, mut library) = test_library();
        let bytes = "WikiSyncer canonical source".repeat(1_000).into_bytes();

        let first = library
            .put_bytes(ObjectKind::Wikitext, &bytes)
            .expect("store object");
        let second = library
            .put_reader(ObjectKind::Wikitext, bytes.len() as u64, bytes.as_slice())
            .expect("store duplicate");

        assert_eq!(first, second);
        assert!(library.contains(first.id).expect("contains object"));
        assert_eq!(library.read_object(first.id).expect("read object"), bytes);
        assert_eq!(table_count(&library, "content_objects"), 1);
        assert_eq!(table_count(&library, "object_locations"), 1);
    }

    #[test]
    fn failed_bounded_stream_never_creates_metadata() {
        let directory = tempfile::tempdir().expect("temporary library");
        let config = StoreConfig::default()
            .with_max_object_bytes(8)
            .expect("valid limit");
        let mut library =
            Library::open_with_config(directory.path(), config).expect("open library");

        assert!(matches!(
            library.put_reader(ObjectKind::Wikitext, 4, b"too long".as_slice()),
            Err(StoreError::LengthMismatch {
                expected: 4,
                actual: 8
            })
        ));
        assert!(matches!(
            library.put_reader(ObjectKind::Wikitext, 9, b"ignored".as_slice()),
            Err(StoreError::ObjectTooLarge {
                limit: 8,
                actual: 9
            })
        ));
        assert_eq!(table_count(&library, "content_objects"), 0);
    }

    #[test]
    fn corrupt_loose_object_is_rejected() {
        let (_directory, mut library) = test_library();
        let stored = library
            .put_bytes(ObjectKind::Wikitext, b"canonical")
            .expect("store object");
        let relative: String = library
            .connection()
            .query_row(
                "SELECT relative_path FROM object_locations WHERE object_id = ?1",
                [stored.id.to_string()],
                |row| row.get(0),
            )
            .expect("object path");
        fs::write(library.root().join(relative), b"not zstd").expect("tamper object");

        assert!(matches!(
            library.read_object(stored.id),
            Err(StoreError::Io(_))
        ));
    }

    #[test]
    fn existing_durable_file_can_be_adopted_after_metadata_loss() {
        let (directory, mut library) = test_library();
        let bytes = b"durable before metadata";
        let stored = library
            .put_bytes(ObjectKind::Wikitext, bytes)
            .expect("store object");
        library
            .connection()
            .execute("DELETE FROM content_objects", [])
            .expect("simulate lost transaction");
        drop(library);

        let mut reopened = Library::open(directory.path()).expect("reopen library");
        let adopted = reopened
            .put_bytes(ObjectKind::Wikitext, bytes)
            .expect("adopt object");
        assert_eq!(adopted.id, stored.id);
        assert_eq!(
            reopened.read_object(adopted.id).expect("read adopted"),
            bytes
        );
    }

    fn migration_count(library: &Library) -> u32 {
        library
            .connection()
            .query_row("SELECT COUNT(*) FROM schema_migrations", [], |row| {
                row.get(0)
            })
            .expect("migration count")
    }

    fn table_count(library: &Library, table: &str) -> u32 {
        let statement = format!("SELECT COUNT(*) FROM {table}");
        library
            .connection()
            .query_row(&statement, [], |row| row.get(0))
            .expect("table count")
    }
}
