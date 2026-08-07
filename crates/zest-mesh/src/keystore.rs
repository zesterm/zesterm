//! Where the private key sleeps.
//!
//! Keychain on macOS, Credential Manager on Windows, Secret Service elsewhere.
//! Never a file: a file needs permission bits we would get subtly wrong on one
//! of three platforms, it survives into backups and sync folders, and it is
//! readable by anything running as the user — which on a developer machine is
//! every build script of every dependency of every project.
//!
//! # Why there is a trait here at all
//!
//! One reason, and it is worth stating so nobody generalizes it: the tests must
//! not touch the machine's real credential store. CI has no unlocked Secret
//! Service on Linux and no reliable keychain access on macOS, so a test that
//! needed one would fail for reasons that have nothing to do with the code —
//! and a test that fails for unrelated reasons is a test people learn to re-run
//! rather than read.
//!
//! # The machine with no store at all
//!
//! A headless build box has no session bus and therefore no Secret Service.
//! This module refuses to start rather than quietly writing a file, and says
//! which store it looked for. That leaves a Linux daemon without an identity
//! for now, which is the honest state of things: giving a host somewhere else
//! to keep its key is a decision about how the fleet is administered, and it
//! belongs with pairing and enrollment rather than being smuggled in here.
//!
//! # The one platform difference
//!
//! There is no `windows.rs`/`macos.rs` split like `zest-pty`'s, because there
//! is nothing to split: `keyring` already selects the platform store, and
//! writing our own three-way branch would produce three copies of six lines and
//! three places for [`KEY_SERVICE`] to drift. The only thing that varies is the
//! store's *name*, and it varies only inside an error message.

use std::collections::HashMap;

use zeroize::Zeroizing;

use crate::MeshError;

/// An Ed25519 secret key is 32 bytes, and nothing else belongs in here.
pub const SECRET_LEN: usize = 32;

/// Stable names for the two keys a machine can hold.
///
/// Part of the on-disk contract: change one and every existing installation
/// forgets who it is.
pub const HOST_KEY_NAME: &str = "host-key";
/// See [`HOST_KEY_NAME`].
pub const CLIENT_KEY_NAME: &str = "client-key";

/// The service every zesterm credential is filed under.
///
/// Matches `directories::ProjectDirs::from("dev", "zesterm", "zesterm")`, so a
/// user looking for what this program stored finds it under the same name the
/// config lives under.
pub const KEY_SERVICE: &str = "dev.zesterm.zesterm";

/// Where a 32-byte secret lives when the process is not running.
pub trait KeyStore: Send + Sync {
    /// `Ok(None)` means there is no key yet, which is the normal first run.
    ///
    /// `Err` means the store could not be consulted. That distinction is the
    /// whole reason this returns `Option` inside `Result` rather than either
    /// alone: collapse the two and a locked keychain looks like a first run, so
    /// the daemon mints a *second* identity and drops out of every fleet
    /// listing that had learned the first — with no error logged anywhere.
    fn load(&self, name: &str) -> Result<Option<Zeroizing<[u8; SECRET_LEN]>>, MeshError>;

    fn store(&self, name: &str, secret: &[u8; SECRET_LEN]) -> Result<(), MeshError>;

    /// Used by key rotation, and by tests that clean up after themselves.
    fn delete(&self, name: &str) -> Result<(), MeshError>;

    /// A human name for this store, for the startup log and error messages.
    fn describe(&self) -> String;
}

/// The platform store's name, for messages only.
///
/// A `cfg` rather than a runtime question because `keyring` does not report
/// which store it chose when it succeeds, and a user reading "no credential
/// store" needs to know which one was missing in order to fix it.
#[cfg(all(feature = "os-keystore", target_os = "macos"))]
const STORE_NAME: &str = "the macOS Keychain";
#[cfg(all(feature = "os-keystore", windows))]
const STORE_NAME: &str = "the Windows Credential Manager";
#[cfg(all(feature = "os-keystore", unix, not(target_os = "macos")))]
const STORE_NAME: &str = "the Secret Service";

/// The machine's own credential store.
#[cfg(feature = "os-keystore")]
#[derive(Debug, Clone, Copy)]
pub struct OsKeyStore;

#[cfg(feature = "os-keystore")]
impl OsKeyStore {
    /// Whether this machine has a usable credential store, and if not, why.
    ///
    /// Worth calling at startup rather than discovering the answer on the first
    /// write: a daemon that has been running for an hour and only then finds it
    /// cannot persist its identity has already told peers who it is.
    pub fn availability() -> Result<(), MeshError> {
        match keyring::Entry::store_status() {
            Ok(()) => Ok(()),
            Err(e) => Err(MeshError::Identity(format!(
                "no credential store on this machine ({STORE_NAME}: {e}). \
                 A daemon will not write its private key to a file — run it under \
                 a session with an unlocked keyring"
            ))),
        }
    }

    /// Fails now rather than on first use if there is no store. See
    /// [`OsKeyStore::availability`].
    pub fn new() -> Result<Self, MeshError> {
        Self::availability()?;
        Ok(Self)
    }

    fn entry(name: &str) -> Result<keyring::Entry, MeshError> {
        keyring::Entry::new(KEY_SERVICE, name)
            .map_err(|e| MeshError::Identity(format!("{STORE_NAME} refused an entry: {e}")))
    }
}

#[cfg(feature = "os-keystore")]
impl KeyStore for OsKeyStore {
    fn load(&self, name: &str) -> Result<Option<Zeroizing<[u8; SECRET_LEN]>>, MeshError> {
        let mut secret = match Self::entry(name)?.get_secret() {
            Ok(secret) => secret,
            // The only error that is not an error. Everything below this line
            // is a store we could not read, which must never be reported as an
            // empty store.
            Err(keyring::Error::NoEntry) => return Ok(None),
            Err(e) => {
                return Err(MeshError::Identity(format!(
                    "{STORE_NAME} would not return {name}: {e}"
                )))
            }
        };

        let len = secret.len();
        let copied = <[u8; SECRET_LEN]>::try_from(secret.as_slice());
        // The `Vec` keyring hands back is ordinary heap memory, so scrub it
        // before it is dropped rather than leaving a copy of the key in
        // whatever allocates that page next.
        zeroize::Zeroize::zeroize(&mut secret);

        let bytes = copied.map_err(|_| {
            MeshError::Identity(format!(
                "{name} in {STORE_NAME} is {len} bytes, not {SECRET_LEN} — refusing to \
                 pad or truncate it into a different identity"
            ))
        })?;
        Ok(Some(Zeroizing::new(bytes)))
    }

    fn store(&self, name: &str, secret: &[u8; SECRET_LEN]) -> Result<(), MeshError> {
        Self::entry(name)?
            .set_secret(secret)
            .map_err(|e| MeshError::Identity(format!("{STORE_NAME} would not store {name}: {e}")))
    }

    fn delete(&self, name: &str) -> Result<(), MeshError> {
        match Self::entry(name)?.delete_credential() {
            Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
            Err(e) => Err(MeshError::Identity(format!(
                "{STORE_NAME} would not delete {name}: {e}"
            ))),
        }
    }

    fn describe(&self) -> String {
        STORE_NAME.to_string()
    }
}

/// Keys that vanish with the process.
///
/// For tests, and for anything that legitimately wants an identity that lasts
/// exactly one run.
#[derive(Debug, Default)]
pub struct MemoryKeyStore {
    keys: parking_lot::Mutex<HashMap<String, Zeroizing<[u8; SECRET_LEN]>>>,
}

impl MemoryKeyStore {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }
}

impl KeyStore for MemoryKeyStore {
    fn load(&self, name: &str) -> Result<Option<Zeroizing<[u8; SECRET_LEN]>>, MeshError> {
        Ok(self.keys.lock().get(name).cloned())
    }

    fn store(&self, name: &str, secret: &[u8; SECRET_LEN]) -> Result<(), MeshError> {
        self.keys
            .lock()
            .insert(name.to_string(), Zeroizing::new(*secret));
        Ok(())
    }

    fn delete(&self, name: &str) -> Result<(), MeshError> {
        self.keys.lock().remove(name);
        Ok(())
    }

    fn describe(&self) -> String {
        "an in-memory store that forgets on exit".to_string()
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// A store that is present but unreadable — a locked keychain, a session
    /// with no bus. The bug this exists to catch cannot be reproduced by hand
    /// on a developer machine, because a developer's keychain is always
    /// unlocked.
    pub(crate) struct FailingKeyStore;

    impl KeyStore for FailingKeyStore {
        fn load(&self, _name: &str) -> Result<Option<Zeroizing<[u8; SECRET_LEN]>>, MeshError> {
            Err(MeshError::Identity("the keychain is locked".into()))
        }
        fn store(&self, _name: &str, _secret: &[u8; SECRET_LEN]) -> Result<(), MeshError> {
            Err(MeshError::Identity("the keychain is locked".into()))
        }
        fn delete(&self, _name: &str) -> Result<(), MeshError> {
            Err(MeshError::Identity("the keychain is locked".into()))
        }
        fn describe(&self) -> String {
            "a store that cannot be read".into()
        }
    }

    #[test]
    fn a_key_survives_a_round_trip_through_the_store() {
        let store = MemoryKeyStore::new();
        let secret = [7u8; SECRET_LEN];
        store.store(HOST_KEY_NAME, &secret).expect("store");
        assert_eq!(
            store.load(HOST_KEY_NAME).expect("load").map(|s| *s),
            Some(secret),
            "a store that cannot return what it was given is not a store"
        );
    }

    #[test]
    fn an_empty_store_reports_no_key_rather_than_failing() {
        let store = MemoryKeyStore::new();
        assert!(
            store.load(HOST_KEY_NAME).expect("load").is_none(),
            "the first run of a brand-new machine is the normal case, not an error"
        );
    }

    #[test]
    fn a_key_name_is_not_shared_between_the_host_and_client_roles() {
        let store = MemoryKeyStore::new();
        store.store(HOST_KEY_NAME, &[1u8; SECRET_LEN]).expect("store");
        assert!(
            store.load(CLIENT_KEY_NAME).expect("load").is_none(),
            "a machine that is both a host and a client holds two keys; sharing \
             one slot would make the roles indistinguishable at the store"
        );
    }

    #[test]
    fn a_deleted_key_is_gone_and_deleting_it_again_is_not_an_error() {
        let store = MemoryKeyStore::new();
        store.store(HOST_KEY_NAME, &[3u8; SECRET_LEN]).expect("store");
        store.delete(HOST_KEY_NAME).expect("delete");
        assert!(
            store.load(HOST_KEY_NAME).expect("load").is_none(),
            "a rotated-out key that is still readable has not been rotated out"
        );
        store
            .delete(HOST_KEY_NAME)
            .expect("deleting an absent key is a no-op, not a failure");
    }

    #[test]
    fn a_store_that_cannot_be_read_reports_an_error_and_not_an_empty_store() {
        let store = FailingKeyStore;
        assert!(
            store.load(HOST_KEY_NAME).is_err(),
            "a locked keychain reported as `Ok(None)` makes the caller generate a \
             fresh identity, which silently removes this machine from every fleet \
             listing that had learned the old one"
        );
    }

    #[cfg(feature = "os-keystore")]
    #[test]
    #[ignore = "writes to this machine's real credential store, and on macOS can \
                block on a keychain prompt; run with `cargo test -p zest-mesh -- --ignored`"]
    fn the_os_key_store_round_trips_a_secret() {
        // Stays ignored permanently rather than "until CI is fixed": on a macOS
        // runner this does not fail, it hangs on an authorization prompt, and a
        // hanging job is worse than an absent test. `cargo clippy --all-targets`
        // still type-checks it, which is most of what it is for.
        let store = OsKeyStore::new().expect("this machine has a credential store");
        let name = "test-host-key";
        let secret = [42u8; SECRET_LEN];

        store.store(name, &secret).expect("store");
        let loaded = store.load(name).expect("load");
        store.delete(name).expect("clean up after ourselves");

        assert_eq!(
            loaded.map(|s| *s),
            Some(secret),
            "the platform store must return exactly the bytes it was given"
        );
    }
}
