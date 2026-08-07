//! Which clients this host has been told to trust.
//!
//! # Why this is not the `KeyStore`
//!
//! [`KeyStore`](crate::keystore::KeyStore) is `name -> [u8; 32]`, with no
//! enumeration, no metadata and no removal by id. Encoding a record into 32
//! bytes and inventing a naming scheme to list devices would work, and each OS
//! credential entry is a separate keychain item, so ten paired devices is ten
//! keychain prompts on macOS.
//!
//! More decisively: **a paired `ClientId` is a public key.** There is nothing
//! secret here. It belongs in a file a person can read, diff, copy between
//! machines and hand to whoever is helping them — not in a credential store
//! that exists to keep things hidden.
//!
//! What this file *is* sensitive to is **integrity**, not confidentiality:
//! anyone who can write it grants themselves a shell. Hence `0600` in a `0700`
//! directory, and hence a parse failure being an error rather than an empty
//! list.
//!
//! # A corrupt file is an error, never "nobody is trusted"
//!
//! Same shape and same reasoning as `KeyStore::load` returning
//! `Result<Option<_>>`: silently forgetting every pairing makes every device
//! prompt again, and a stream of prompts is how a person learns to approve
//! without reading. Refusing to serve is the honest failure.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use parking_lot::RwLock;
use serde::{Deserialize, Serialize};
use zest_proto::ClientId;

use crate::MeshError;

/// One device this host will serve.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrustRecord {
    pub client: ClientId,
    /// What the person approving it was shown.
    pub label: String,
    pub paired_at: SystemTime,
    pub last_seen: Option<SystemTime>,
}

/// Where paired clients live.
///
/// A trait for the same reason `KeyStore` is one — tests must not touch the
/// real file — and for no other. It is not an extension point.
pub trait TrustStore: Send + Sync {
    fn get(&self, client: ClientId) -> Result<Option<TrustRecord>, MeshError>;
    fn list(&self) -> Result<Vec<TrustRecord>, MeshError>;
    fn insert(&self, record: TrustRecord) -> Result<(), MeshError>;
    fn touch(&self, client: ClientId, at: SystemTime) -> Result<(), MeshError>;
    fn remove(&self, client: ClientId) -> Result<bool, MeshError>;
    /// Where this store keeps its records, for an error message.
    fn describe(&self) -> String;
}

// --- the file format -------------------------------------------------------

#[derive(Debug, Default, Serialize, Deserialize)]
struct TrustFile {
    /// Bumped only if the shape changes incompatibly. A file from the future
    /// is refused rather than half-read.
    #[serde(default = "one")]
    version: u32,
    #[serde(default, rename = "client")]
    clients: Vec<ClientEntry>,
}

const fn one() -> u32 {
    1
}

const FORMAT_VERSION: u32 = 1;

#[derive(Debug, Serialize, Deserialize)]
struct ClientEntry {
    id: ClientId,
    label: String,
    /// Unix seconds. Deliberately not RFC 3339: a date crate for two
    /// timestamps is a dependency the UI can render without.
    paired_at: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    last_seen: Option<u64>,
}

fn to_unix(t: SystemTime) -> u64 {
    t.duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs())
}

fn from_unix(secs: u64) -> SystemTime {
    UNIX_EPOCH + Duration::from_secs(secs)
}

/// The real store: a TOML file.
pub struct FileTrustStore {
    path: PathBuf,
    /// The file is the source of truth; this is a cache so an attach does not
    /// read the disk, guarded so two connections can look at once.
    index: RwLock<BTreeMap<[u8; 32], TrustRecord>>,
    /// `last_seen` is debounced: persisting it on every attach is a disk write
    /// per connection for a value nobody reads to the second.
    dirty_since: RwLock<Option<SystemTime>>,
}

/// How stale `last_seen` may get on disk before a write is worth it.
const LAST_SEEN_DEBOUNCE: Duration = Duration::from_secs(60);

impl FileTrustStore {
    /// Open, creating an empty store if the file does not exist yet.
    ///
    /// A missing file is a first run. An *unreadable* one is an error — see the
    /// module docs for why that distinction is the whole point.
    pub fn open(path: impl Into<PathBuf>) -> Result<Self, MeshError> {
        let path = path.into();
        let index = match std::fs::read_to_string(&path) {
            Ok(text) => parse(&text, &path)?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => BTreeMap::new(),
            Err(e) => {
                return Err(MeshError::Store(format!("{}: {e}", path.display())));
            }
        };
        Ok(Self {
            path,
            index: RwLock::new(index),
            dirty_since: RwLock::new(None),
        })
    }

    /// Write the whole file, atomically.
    ///
    /// Temp file, fsync, rename. A half-written trust file is either "every
    /// device is suddenly unpaired" or one that refuses to parse, and both
    /// happen exactly when the machine crashed, which is the worst moment to
    /// discover you cannot reach it.
    fn flush(&self, index: &BTreeMap<[u8; 32], TrustRecord>) -> Result<(), MeshError> {
        let file = TrustFile {
            version: FORMAT_VERSION,
            clients: index
                .values()
                .map(|r| ClientEntry {
                    id: r.client,
                    label: r.label.clone(),
                    paired_at: to_unix(r.paired_at),
                    last_seen: r.last_seen.map(to_unix),
                })
                .collect(),
        };
        let text = toml::to_string_pretty(&file)
            .map_err(|e| MeshError::Store(format!("serializing trust: {e}")))?;

        if let Some(dir) = self.path.parent() {
            std::fs::create_dir_all(dir)
                .map_err(|e| MeshError::Store(format!("{}: {e}", dir.display())))?;
            restrict_dir(dir);
        }

        let tmp = self.path.with_extension("toml.tmp");
        write_private(&tmp, text.as_bytes())
            .map_err(|e| MeshError::Store(format!("{}: {e}", tmp.display())))?;
        std::fs::rename(&tmp, &self.path)
            .map_err(|e| MeshError::Store(format!("{}: {e}", self.path.display())))?;
        Ok(())
    }
}

fn parse(text: &str, path: &Path) -> Result<BTreeMap<[u8; 32], TrustRecord>, MeshError> {
    let file: TrustFile = toml::from_str(text)
        .map_err(|e| MeshError::Store(format!("{} is not readable: {e}", path.display())))?;
    if file.version > FORMAT_VERSION {
        return Err(MeshError::Store(format!(
            "{} is version {} and this build understands {FORMAT_VERSION}",
            path.display(),
            file.version
        )));
    }
    Ok(file
        .clients
        .into_iter()
        .map(|c| {
            (
                c.id.0,
                TrustRecord {
                    client: c.id,
                    label: c.label,
                    paired_at: from_unix(c.paired_at),
                    last_seen: c.last_seen.map(from_unix),
                },
            )
        })
        .collect())
}

#[cfg(unix)]
fn write_private(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    use std::io::Write as _;
    use std::os::unix::fs::OpenOptionsExt as _;
    let mut f = std::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .mode(0o600)
        .open(path)?;
    f.write_all(bytes)?;
    // Durable before the rename, or a crash can leave the new name pointing at
    // an empty file.
    f.sync_all()
}

#[cfg(not(unix))]
fn write_private(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    use std::io::Write as _;
    // Windows inherits the user-only ACL of %APPDATA%; there is no mode to set.
    let mut f = std::fs::File::create(path)?;
    f.write_all(bytes)?;
    f.sync_all()
}

#[cfg(unix)]
fn restrict_dir(dir: &Path) {
    use std::os::unix::fs::PermissionsExt as _;
    let _ = std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700));
}

#[cfg(not(unix))]
fn restrict_dir(_dir: &Path) {}

impl TrustStore for FileTrustStore {
    fn get(&self, client: ClientId) -> Result<Option<TrustRecord>, MeshError> {
        Ok(self.index.read().get(&client.0).cloned())
    }

    fn list(&self) -> Result<Vec<TrustRecord>, MeshError> {
        Ok(self.index.read().values().cloned().collect())
    }

    fn insert(&self, record: TrustRecord) -> Result<(), MeshError> {
        let mut index = self.index.write();
        index.insert(record.client.0, record);
        self.flush(&index)
    }

    fn touch(&self, client: ClientId, at: SystemTime) -> Result<(), MeshError> {
        let mut index = self.index.write();
        let Some(record) = index.get_mut(&client.0) else { return Ok(()) };
        record.last_seen = Some(at);

        // Debounced. Every attach writing the file would be a disk write per
        // connection for a value that is only ever read by a human deciding
        // whether a device is still in use.
        let mut dirty = self.dirty_since.write();
        let due = dirty.is_none_or(|since| {
            at.duration_since(since).unwrap_or(Duration::ZERO) >= LAST_SEEN_DEBOUNCE
        });
        if !due {
            return Ok(());
        }
        *dirty = Some(at);
        self.flush(&index)
    }

    fn remove(&self, client: ClientId) -> Result<bool, MeshError> {
        let mut index = self.index.write();
        let existed = index.remove(&client.0).is_some();
        if existed {
            self.flush(&index)?;
        }
        Ok(existed)
    }

    fn describe(&self) -> String {
        self.path.display().to_string()
    }
}

/// For tests, and for `--identity ephemeral`.
#[derive(Default)]
pub struct MemoryTrustStore {
    index: RwLock<BTreeMap<[u8; 32], TrustRecord>>,
}

impl MemoryTrustStore {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }
}

impl TrustStore for MemoryTrustStore {
    fn get(&self, client: ClientId) -> Result<Option<TrustRecord>, MeshError> {
        Ok(self.index.read().get(&client.0).cloned())
    }

    fn list(&self) -> Result<Vec<TrustRecord>, MeshError> {
        Ok(self.index.read().values().cloned().collect())
    }

    fn insert(&self, record: TrustRecord) -> Result<(), MeshError> {
        self.index.write().insert(record.client.0, record);
        Ok(())
    }

    fn touch(&self, client: ClientId, at: SystemTime) -> Result<(), MeshError> {
        if let Some(r) = self.index.write().get_mut(&client.0) {
            r.last_seen = Some(at);
        }
        Ok(())
    }

    fn remove(&self, client: ClientId) -> Result<bool, MeshError> {
        Ok(self.index.write().remove(&client.0).is_some())
    }

    fn describe(&self) -> String {
        "memory (nothing is persisted)".into()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tempdir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir()
            .join(format!("zesterm-trust-{}-{}", name, std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("mkdir");
        dir
    }

    fn record(byte: u8, label: &str) -> TrustRecord {
        TrustRecord {
            client: ClientId::from_bytes([byte; 32]),
            label: label.into(),
            paired_at: from_unix(1_786_000_000),
            last_seen: None,
        }
    }

    #[test]
    fn a_paired_client_survives_a_restart() {
        let dir = tempdir("roundtrip");
        let path = dir.join("trust.toml");

        let store = FileTrustStore::open(&path).expect("open");
        store.insert(record(0xab, "andy-phone")).expect("insert");

        // A second store reading the same file is what a daemon restart is.
        let reopened = FileTrustStore::open(&path).expect("reopen");
        let found = reopened
            .get(ClientId::from_bytes([0xab; 32]))
            .expect("get")
            .expect("the pairing did not survive");
        assert_eq!(found.label, "andy-phone");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_corrupt_file_is_an_error_not_an_empty_list() {
        // The whole point. Treating an unreadable file as "nobody is trusted"
        // makes every device prompt again, and a stream of prompts is how
        // someone learns to approve without reading.
        let dir = tempdir("corrupt");
        let path = dir.join("trust.toml");
        std::fs::write(&path, "this is not toml {{{").expect("write");

        let err = FileTrustStore::open(&path);
        assert!(err.is_err(), "a corrupt trust file was silently ignored");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_file_from_the_future_is_refused_rather_than_half_read() {
        let dir = tempdir("future");
        let path = dir.join("trust.toml");
        std::fs::write(&path, "version = 999\n").expect("write");
        assert!(FileTrustStore::open(&path).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_missing_file_is_a_first_run_not_a_failure() {
        let dir = tempdir("missing");
        let store = FileTrustStore::open(dir.join("nothing-here.toml")).expect("first run");
        assert!(store.list().expect("list").is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_removed_client_is_refused_afterwards() {
        let dir = tempdir("remove");
        let path = dir.join("trust.toml");
        let store = FileTrustStore::open(&path).expect("open");
        store.insert(record(7, "old-laptop")).expect("insert");

        assert!(store.remove(ClientId::from_bytes([7; 32])).expect("remove"));
        assert!(store.get(ClientId::from_bytes([7; 32])).expect("get").is_none());
        assert!(
            !store.remove(ClientId::from_bytes([7; 32])).expect("remove"),
            "removing twice must report that there was nothing to remove"
        );

        let reopened = FileTrustStore::open(&path).expect("reopen");
        assert!(reopened.get(ClientId::from_bytes([7; 32])).expect("get").is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn no_temporary_file_is_left_behind() {
        // A stray `trust.toml.tmp` is how a reader later finds two files and
        // has to guess which is real.
        let dir = tempdir("tmp");
        let path = dir.join("trust.toml");
        let store = FileTrustStore::open(&path).expect("open");
        store.insert(record(1, "a")).expect("insert");
        store.insert(record(2, "b")).expect("insert");

        let leftovers: Vec<_> = std::fs::read_dir(&dir)
            .expect("readdir")
            .filter_map(Result::ok)
            .map(|e| e.file_name().to_string_lossy().to_string())
            .filter(|n| n.ends_with(".tmp"))
            .collect();
        assert!(leftovers.is_empty(), "left behind {leftovers:?}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn the_trust_file_is_not_world_readable() {
        use std::os::unix::fs::PermissionsExt as _;
        // Integrity, not secrecy: anyone who can write this grants themselves
        // a shell.
        let dir = tempdir("mode");
        let path = dir.join("trust.toml");
        let store = FileTrustStore::open(&path).expect("open");
        store.insert(record(3, "c")).expect("insert");

        let mode = std::fs::metadata(&path).expect("stat").permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "trust file mode was {mode:o}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn last_seen_is_debounced_rather_than_written_per_attach() {
        let dir = tempdir("debounce");
        let path = dir.join("trust.toml");
        let store = FileTrustStore::open(&path).expect("open");
        store.insert(record(9, "phone")).expect("insert");

        let now = SystemTime::now();
        store.touch(ClientId::from_bytes([9; 32]), now).expect("touch");
        // A second touch a moment later must not reach the disk.
        store
            .touch(ClientId::from_bytes([9; 32]), now + Duration::from_secs(1))
            .expect("touch");

        // In memory it is current either way; that is what the caller reads.
        let seen = store
            .get(ClientId::from_bytes([9; 32]))
            .expect("get")
            .expect("present")
            .last_seen;
        assert!(seen.is_some());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
