//! A file-backed [`Storage`] implementation, so a simulated charger's persisted state (in-flight
//! transaction, boot reason, cached device model, ...) survives a process restart the same way it
//! would on real hardware with an EEPROM or flash-backed key/value store.
//!
//! Each key becomes exactly one file inside a directory supplied by the caller - typically one
//! directory per charger, so two simulated chargers never share storage even if the same key name
//! (e.g. `"boot_reason"`) is used by both. Locating *that* directory (under the platform config
//! dir, alongside [`super::super::connection_store::ConnectionStore`]'s `connections.yaml`) is the
//! caller's job, exactly as [`super::super::catalog::discover_configured_chargers`] and
//! [`super::super::connection_store::ConnectionStore::load`] take an explicit [`Path`] rather than
//! resolving one internally - this type stays decoupled from `dirs` and fully testable with
//! [`tempfile`].
//!
//! # Key encoding
//!
//! [`Storage`]'s keys are opaque strings chosen entirely by the upstream crate - this
//! implementation has no say in what they look like, and they land directly on a filesystem. A
//! key of `"../../etc/passwd"`, `"a/b"`, or one containing a NUL byte must never be interpreted as
//! a path (escaping [`FileStorage`]'s directory or colliding with an unrelated key), and a key
//! that happens to collide with a name a platform treats specially (`CON`, `NUL`, a bare `.` or
//! `..`, one ending in a space or dot) must not trip over that either.
//!
//! Every key is therefore hex-encoded byte-for-byte (`format_args!("{byte:02x}")` per byte of the
//! key's UTF-8 representation) before touching the filesystem, with a `k` prefix. Hex-encoding is
//! a lossless bijection - two different keys can never produce the same filename, and no key can
//! ever produce another key's filename - so this is exactly as collision-free as the keys
//! themselves, with no hashing (and therefore no hash-collision risk or non-determinism: unlike
//! [`std::collections::HashMap`]'s default hasher, this mapping is identical across every process
//! and platform, which matters for something that has to mean the same thing after a restart).
//! It also happens to rule out every hostile case above for free, as a consequence of the output
//! alphabet being just `0-9a-f` plus the `k` prefix:
//!
//! - No `/`, `\`, NUL, or any other byte a filesystem treats specially - the alphabet contains
//!   none of them.
//! - No `.` at all, so no `.`, `..`, or hidden/dotfile name, and no trailing-dot issue.
//! - No Windows reserved device name (`CON`, `PRN`, `AUX`, `NUL`, `COM1`-`COM9`, `LPT1`-`LPT9`):
//!   every one of those contains a letter outside `a`-`f` (the `o`/`u`/`x`/`p`/`r`/`m`/`l`/`t`),
//!   so none of them is even a well-formed hex string, let alone a possible encoding of one.
//! - The `k` prefix guarantees the filename is never empty (the empty key hex-encodes to an empty
//!   string) and keeps entry files textually distinct from the `.tmp-*` staging files a write
//!   creates transiently (see "Durability" below) - a real key's filename never contains `.`, so
//!   it can never collide with, or be mistaken for, a temp file's name.
//!
//! An unusually long key can still exceed a filesystem's filename length limit once hex-doubled;
//! that surfaces as an ordinary I/O error from the operation rather than being pre-validated,
//! since no length limit is specified by [`Storage`] itself.
//!
//! # Durability and crash-atomicity
//!
//! [`Storage::set`] must be durable by the time it returns `Ok` (a write the caller can no longer
//! see after a clean process exit would be a broken implementation), and per [`Storage`]'s docs, a
//! *torn* write on a crash mid-write is this crate's problem, not the implementor's - but this
//! implementation closes that gap anyway, because doing so is cheap on a real filesystem:
//!
//! `set` writes the new value to a temporary file in the same directory, flushes it to disk
//! ([`std::fs::File::sync_all`]), and only then renames it over the target filename
//! ([`std::fs::rename`]). A same-filesystem rename is a single atomic directory-entry update on
//! every mainstream filesystem this crate targets (ext4, APFS, NTFS) - a reader ([`get`](Storage::get))
//! can only ever see the complete old file or the complete new file, never a half-written one, and
//! a crash at any point before the rename leaves the previous value (or no value) untouched. This
//! does **not** additionally `fsync` the containing directory entry, so it stops short of surviving
//! a crash during the rename's own metadata write on a filesystem without write-ordering
//! guarantees; [`ocpp_charge_point::hardware::AtomicStorage`] exists for callers that need to wrap
//! a non-atomic store to close a gap like that, and reads its documentation as "implement the
//! simple thing, don't build a journal underneath one" - write-then-rename already *is* the simple
//! thing here, so this does not also layer `AtomicStorage`'s A/B-slot protocol on top.
//!
//! [`Storage::remove`] deletes the target file directly. Unlike `set`, this needs no temp-file
//! dance: unlinking a file is already a single atomic filesystem operation with no intermediate
//! state a concurrent [`get`](Storage::get) could observe.
//!
//! # Concurrency
//!
//! Every [`Storage`] method takes `&self`, and the upstream crate calls them from multiple
//! independently-registered blocks, so they must be safe to call concurrently. [`FileStorage`]
//! holds no in-process lock: `get`/`remove` are single filesystem calls, and `set` picks a
//! per-call unique temporary filename (a process id and an atomic counter), so concurrent writers
//! never contend on the same temp file even when writing the same key. Two concurrent `set` calls
//! to the same key each still complete atomically (per "Durability" above); whichever rename lands
//! second wins, with no guarantee about which that is - the same "single writer per key" caveat
//! [`ocpp_charge_point::hardware::AtomicStorage`] documents for itself.

use std::fmt::Write as _;
use std::io::{self, Write as _};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use ocpp_charge_point::hardware::Storage;

/// A [`Storage`] implementation backed by one file per key inside a directory on disk - see the
/// module docs for the key encoding and durability guarantees.
#[derive(Debug, Clone)]
pub struct FileStorage {
    dir: PathBuf,
    /// Disambiguates the temp file used by concurrent `set` calls (including two calls to the
    /// same key) so they never write through the same path - see the module docs' "Concurrency"
    /// section.
    write_counter: std::sync::Arc<AtomicU64>,
}

impl FileStorage {
    /// Creates a store rooted at `dir`. `dir` does not need to exist yet - it is created lazily
    /// the first time [`Storage::set`] actually needs it, matching [`Storage::get`]/
    /// [`Storage::remove`] on a key that simply isn't present yet (also not an error).
    pub fn new(dir: impl Into<PathBuf>) -> Self {
        Self {
            dir: dir.into(),
            write_counter: std::sync::Arc::new(AtomicU64::new(0)),
        }
    }

    /// The directory this store reads and writes.
    pub fn dir(&self) -> &std::path::Path {
        &self.dir
    }
}

/// The error returned by a failed [`FileStorage`] operation: the underlying [`io::Error`] plus
/// which operation and path it happened on, so a failure is diagnosable without the caller having
/// to reconstruct that context itself.
#[derive(Debug)]
pub struct FileStorageError {
    operation: &'static str,
    path: PathBuf,
    source: io::Error,
}

impl std::fmt::Display for FileStorageError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "storage {} failed for {}: {}",
            self.operation,
            self.path.display(),
            self.source
        )
    }
}

impl std::error::Error for FileStorageError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(&self.source)
    }
}

/// Hex-encodes `key`'s UTF-8 bytes into a filename that can never traverse out of the storage
/// directory or collide with another key's filename - see the module docs' "Key encoding"
/// section for why.
fn encode_key(key: &str) -> String {
    let mut out = String::with_capacity(key.len() * 2 + 1);
    out.push('k');
    for byte in key.as_bytes() {
        let _ = write!(out, "{byte:02x}");
    }
    out
}

/// A temp filename for a single `set` call, unique per call even when several calls target the
/// same key concurrently - see the module docs' "Concurrency" section. Never collides with a real
/// entry's filename (which is never `.`-prefixed and never contains a `.`, since [`encode_key`]'s
/// output alphabet is `k0-9a-f`).
fn temp_file_name(entry_name: &str, unique: u64) -> String {
    format!("{entry_name}.tmp-{}-{unique}", std::process::id())
}

fn blocking_get(path: PathBuf) -> Result<Option<Vec<u8>>, FileStorageError> {
    match std::fs::read(&path) {
        Ok(bytes) => Ok(Some(bytes)),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(FileStorageError {
            operation: "get",
            path,
            source: error,
        }),
    }
}

fn blocking_set(
    dir: PathBuf,
    path: PathBuf,
    temp_path: PathBuf,
    value: Vec<u8>,
) -> Result<(), FileStorageError> {
    std::fs::create_dir_all(&dir).map_err(|error| FileStorageError {
        operation: "set (create directory)",
        path: dir,
        source: error,
    })?;

    let write_result = (|| -> io::Result<()> {
        let mut file = std::fs::File::create(&temp_path)?;
        file.write_all(&value)?;
        file.sync_all()
    })();

    if let Err(error) = write_result {
        let _ = std::fs::remove_file(&temp_path);
        return Err(FileStorageError {
            operation: "set (write)",
            path: temp_path,
            source: error,
        });
    }

    std::fs::rename(&temp_path, &path).map_err(|error| {
        let _ = std::fs::remove_file(&temp_path);
        FileStorageError {
            operation: "set (rename)",
            path,
            source: error,
        }
    })
}

fn blocking_remove(path: PathBuf) -> Result<(), FileStorageError> {
    match std::fs::remove_file(&path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(FileStorageError {
            operation: "remove",
            path,
            source: error,
        }),
    }
}

/// Runs a blocking filesystem closure on a blocking-friendly thread, converting an unexpected
/// [`tokio::task::JoinError`] (the closure itself never panics - every fallible step returns a
/// `Result`) into the same [`FileStorageError`] type callers already handle.
async fn run_blocking<T, F>(f: F) -> Result<T, FileStorageError>
where
    F: FnOnce() -> Result<T, FileStorageError> + Send + 'static,
    T: Send + 'static,
{
    match tokio::task::spawn_blocking(f).await {
        Ok(result) => result,
        Err(join_error) => Err(FileStorageError {
            operation: "blocking task",
            path: PathBuf::new(),
            source: io::Error::other(join_error),
        }),
    }
}

#[async_trait::async_trait]
impl Storage for FileStorage {
    type Error = FileStorageError;

    async fn get(&self, key: &str) -> Result<Option<Vec<u8>>, Self::Error> {
        let path = self.dir.join(encode_key(key));
        run_blocking(move || blocking_get(path)).await
    }

    async fn set(&self, key: &str, value: &[u8]) -> Result<(), Self::Error> {
        let entry_name = encode_key(key);
        let path = self.dir.join(&entry_name);
        let unique = self.write_counter.fetch_add(1, Ordering::Relaxed);
        let temp_path = self.dir.join(temp_file_name(&entry_name, unique));
        let dir = self.dir.clone();
        let value = value.to_vec();
        run_blocking(move || blocking_set(dir, path, temp_path, value)).await
    }

    async fn remove(&self, key: &str) -> Result<(), Self::Error> {
        let path = self.dir.join(encode_key(key));
        run_blocking(move || blocking_remove(path)).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    fn storage() -> (tempfile::TempDir, FileStorage) {
        let dir = tempfile::tempdir().unwrap();
        let storage = FileStorage::new(dir.path());
        (dir, storage)
    }

    #[tokio::test]
    async fn round_trips_a_value() {
        let (_dir, storage) = storage();

        assert_eq!(storage.get("key").await.unwrap(), None);

        storage.set("key", b"value").await.unwrap();
        assert_eq!(
            storage.get("key").await.unwrap(),
            Some(Vec::from(&b"value"[..]))
        );
    }

    #[tokio::test]
    async fn get_of_an_absent_key_is_ok_none() {
        let (_dir, storage) = storage();
        assert_eq!(storage.get("never-set").await.unwrap(), None);
    }

    #[tokio::test]
    async fn set_overwrites_a_previous_value() {
        let (_dir, storage) = storage();

        storage.set("key", b"first").await.unwrap();
        storage.set("key", b"second").await.unwrap();

        assert_eq!(
            storage.get("key").await.unwrap(),
            Some(Vec::from(&b"second"[..]))
        );
    }

    #[tokio::test]
    async fn remove_then_get_returns_none() {
        let (_dir, storage) = storage();

        storage.set("key", b"value").await.unwrap();
        storage.remove("key").await.unwrap();

        assert_eq!(storage.get("key").await.unwrap(), None);
    }

    #[tokio::test]
    async fn remove_of_an_absent_key_is_not_an_error() {
        let (_dir, storage) = storage();
        storage.remove("never-set").await.unwrap();
    }

    #[tokio::test]
    async fn empty_value_round_trips() {
        let (_dir, storage) = storage();

        storage.set("key", b"").await.unwrap();
        assert_eq!(storage.get("key").await.unwrap(), Some(Vec::new()));
    }

    #[tokio::test]
    async fn a_large_value_round_trips() {
        let (_dir, storage) = storage();
        let large = vec![0xABu8; 4 * 1024 * 1024];

        storage.set("key", &large).await.unwrap();
        assert_eq!(storage.get("key").await.unwrap(), Some(large));
    }

    #[tokio::test]
    async fn missing_directory_on_first_use_is_not_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let nested = dir.path().join("does/not/exist/yet");
        let storage = FileStorage::new(&nested);

        // Reading before anything has ever been written must not error, and must not create the
        // directory itself.
        assert_eq!(storage.get("key").await.unwrap(), None);
        assert!(!nested.exists());

        // The first `set` creates it lazily.
        storage.set("key", b"value").await.unwrap();
        assert!(nested.is_dir());
        assert_eq!(
            storage.get("key").await.unwrap(),
            Some(Vec::from(&b"value"[..]))
        );
    }

    #[tokio::test]
    async fn hostile_keys_stay_inside_the_directory_and_round_trip() {
        // A private, otherwise-empty sandbox: `storage`'s directory is a subdirectory of it, so
        // an escape attempt (e.g. a key of `"../evil"`) would have to surface as a new entry
        // directly inside `sandbox` - unlike the shared system temp dir, nothing else on the
        // machine ever touches this directory, so any new entry is unambiguously ours.
        let sandbox = tempfile::tempdir().unwrap();
        let dir = sandbox.path().join("storage");
        let storage = FileStorage::new(&dir);

        let hostile_keys = [
            "../../etc/passwd",
            "..",
            ".",
            "a/b",
            "a\\b",
            "/etc/passwd",
            "CON",
            "NUL",
            "con",
            "key\0with\0nul",
            "",
            "   ",
            "trailing.",
            "trailing ",
            "🔥emoji-key🔥",
        ];

        for (index, key) in hostile_keys.iter().enumerate() {
            let value = format!("value-{index}").into_bytes();
            storage.set(key, &value).await.unwrap();
            assert_eq!(
                storage.get(key).await.unwrap(),
                Some(value),
                "key {key:?} did not round-trip"
            );
        }

        // Nothing escaped the storage directory: the sandbox contains exactly the one `storage`
        // subdirectory `FileStorage` was given, nothing else.
        let sandbox_entries: Vec<_> = std::fs::read_dir(sandbox.path())
            .unwrap()
            .filter_map(|entry| entry.ok())
            .map(|entry| entry.file_name())
            .collect();
        assert_eq!(
            sandbox_entries,
            vec![dir.file_name().unwrap().to_owned()],
            "a hostile key must never create anything outside the storage directory"
        );

        let entries: Vec<_> = std::fs::read_dir(&dir)
            .unwrap()
            .filter_map(|entry| entry.ok())
            .collect();
        assert_eq!(
            entries.len(),
            hostile_keys.len(),
            "every hostile key must occupy its own distinct file, with no collisions"
        );
    }

    #[tokio::test]
    async fn hostile_keys_do_not_collide_with_each_other() {
        let (_dir, storage) = storage();

        // Two keys that a naive path-joining implementation might normalize to the same thing.
        storage.set("a/b", b"first").await.unwrap();
        storage.set("a\\b", b"second").await.unwrap();
        storage.set("a", b"third").await.unwrap();

        assert_eq!(
            storage.get("a/b").await.unwrap(),
            Some(Vec::from(&b"first"[..]))
        );
        assert_eq!(
            storage.get("a\\b").await.unwrap(),
            Some(Vec::from(&b"second"[..]))
        );
        assert_eq!(
            storage.get("a").await.unwrap(),
            Some(Vec::from(&b"third"[..]))
        );
    }

    #[test]
    fn encode_key_is_injective_over_a_sample_of_keys() {
        let keys = [
            "", "a", "b", "ab", "a/b", "a\\b", "..", ".", "CON", "con", "key\0", " key", "key ",
        ];
        let encoded: HashSet<String> = keys.iter().map(|key| encode_key(key)).collect();
        assert_eq!(encoded.len(), keys.len(), "encode_key produced a collision");
    }

    #[test]
    fn encode_key_never_produces_an_empty_or_dotted_name() {
        for key in ["", "a", "..", "."] {
            let encoded = encode_key(key);
            assert!(!encoded.is_empty());
            assert!(!encoded.contains('.'));
            assert!(!encoded.contains('/'));
            assert!(!encoded.contains('\\'));
        }
    }

    #[tokio::test]
    async fn concurrent_sets_and_gets_across_many_keys_do_not_error() {
        let (_dir, storage) = storage();

        let mut tasks = Vec::new();
        for i in 0..32 {
            let storage = storage.clone();
            tasks.push(tokio::spawn(async move {
                let key = format!("key-{}", i % 8);
                let value = format!("value-{i}").into_bytes();
                storage.set(&key, &value).await.unwrap();
                storage.get(&key).await.unwrap();
                if i % 5 == 0 {
                    storage.remove(&key).await.unwrap();
                }
            }));
        }

        for task in tasks {
            task.await.unwrap();
        }
    }

    #[tokio::test]
    async fn concurrent_writers_to_the_same_key_never_leave_a_torn_value() {
        let (_dir, storage) = storage();
        let values: Vec<Vec<u8>> = (0..16).map(|i| vec![i as u8; 4096]).collect();

        let mut tasks = Vec::new();
        for value in &values {
            let storage = storage.clone();
            let value = value.clone();
            tasks.push(tokio::spawn(
                async move { storage.set("shared", &value).await },
            ));
        }
        for task in tasks {
            task.await.unwrap().unwrap();
        }

        // Whichever write landed last, the surviving value must be exactly one of the ones
        // written - never a mix of two (which would prove a torn write was observable).
        let result = storage.get("shared").await.unwrap().unwrap();
        assert!(
            values.iter().any(|value| value == &result),
            "final value must be exactly one complete write, not a mix"
        );
    }
}
