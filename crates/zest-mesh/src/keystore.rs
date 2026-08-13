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

/// Re-exported because it is in [`KeyStore`]'s and [`SecretStore`]'s
/// signatures: an implementor in another crate — `zest-daemon`'s tests are the
/// first — would otherwise have to depend on `zeroize` at exactly this version
/// to name the type it is handed.
pub use zeroize::Zeroizing;

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

/// The control plane's bearer token for this machine, if it has enrolled.
///
/// Same namespace as the two key names above — one credential store, one entry
/// per name — which is why the names have to stay distinct. See
/// [`SecretStore`] for why the token does not travel through [`KeyStore`].
pub const CLOUD_TOKEN_NAME: &str = "cloud-token";

/// The desktop app's own bearer token, when *it* has signed in.
///
/// Deliberately not [`CLOUD_TOKEN_NAME`], and the split is principal, not
/// tidiness (#190): `cloud-token` names this machine's **host row** — a
/// machine that serves shells and must never act as the person — while this
/// names a **device of the user**, which may read the fleet, mint relay
/// admission, and one day approve other devices. One store, two principals,
/// two entries; sharing the name would hand the daemon the user's authority
/// the first time either side wrote the other's slot.
pub const APP_CLOUD_TOKEN_NAME: &str = "app-cloud-token";

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

/// Where a secret that is *not* a key lives when the process is not running.
///
/// A sibling of [`KeyStore`] rather than a widening of it. `KeyStore`'s fixed
/// `[u8; SECRET_LEN]` is what makes "the store handed back 31 bytes" a refusal
/// at the door instead of a silently different identity, and relaxing it to a
/// `Vec` would demote that guarantee to a runtime check every caller has to
/// remember to write. A bearer token has no such length to check — it is
/// whatever the control plane minted — so the two traits sit side by side over
/// the same backend and the same [`KEY_SERVICE`].
///
/// The `*_secret` suffixes are not a style choice. Both traits are implemented
/// by the same types, so a plain `load` here would make `store.load(name)`
/// ambiguous (E0034) at every call site that has both traits in scope —
/// including ones that exist today.
pub trait SecretStore: Send + Sync {
    /// `Ok(None)` means there is nothing under `name`, which for
    /// [`CLOUD_TOKEN_NAME`] is the normal state of a machine that has never
    /// enrolled.
    ///
    /// `Option` inside `Result` for the same reason as [`KeyStore::load`], and
    /// the failure it prevents is the mirror image: collapse the two and a
    /// locked keychain is indistinguishable from "not enrolled", so the daemon
    /// tells a person to enrol a machine that already is — and, worse, a
    /// successful re-enrolment then writes a second token over the first.
    fn load_secret(&self, name: &str) -> Result<Option<Zeroizing<Vec<u8>>>, MeshError>;

    fn store_secret(&self, name: &str, secret: &[u8]) -> Result<(), MeshError>;

    /// Signing out, and tests that clean up after themselves. Deleting
    /// something that is not there is not an error — see [`KeyStore::delete`].
    fn delete_secret(&self, name: &str) -> Result<(), MeshError>;

    /// A human name for this store. Spelled apart from [`KeyStore::describe`]
    /// for the ambiguity reason above, not because they differ.
    fn describe_secret_store(&self) -> String;
}

/// Both halves of one machine's credential store.
///
/// Exists so a caller does not have to choose between two boxes to talk to one
/// keychain: the OS backend really is a single store, [`KEY_SERVICE`] with an
/// entry per name, holding the host key and the cloud token side by side. The
/// bug this shape prevents is specific — a daemon that builds a `KeyStore` for
/// its identity and reaches for the OS store separately for the token ends up
/// with `--ephemeral` meaning a throwaway key *and* a durable token, which is
/// the one combination that enrols a machine nobody can reach afterwards.
pub trait CredentialStore: KeyStore + SecretStore {}

impl<T: KeyStore + SecretStore> CredentialStore for T {}

/// The refusal a store gives when an entry is not a key.
///
/// Shared by every [`KeyStore`] rather than written twice, because this is the
/// sentence a person reads when their identity will not load, and the two must
/// not drift into disagreeing about what happened.
fn wrong_key_length(name: &str, store: &str, len: usize) -> MeshError {
    MeshError::Identity(format!(
        "{name} in {store} is {len} bytes, not {SECRET_LEN} — refusing to \
         pad or truncate it into a different identity"
    ))
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

        let bytes = copied.map_err(|_| wrong_key_length(name, STORE_NAME, len))?;
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

#[cfg(feature = "os-keystore")]
impl SecretStore for OsKeyStore {
    fn load_secret(&self, name: &str) -> Result<Option<Zeroizing<Vec<u8>>>, MeshError> {
        match Self::entry(name)?.get_secret() {
            // Wrapped as it arrives rather than copied out: the `Vec` keyring
            // returns is ordinary heap memory, and `Zeroizing` taking ownership
            // of *this* allocation is what scrubs the page the token was
            // actually decrypted into.
            Ok(secret) => Ok(Some(Zeroizing::new(secret))),
            // The only error that is not an error; see `KeyStore::load`.
            Err(keyring::Error::NoEntry) => Ok(None),
            Err(e) => Err(MeshError::Identity(format!(
                "{STORE_NAME} would not return {name}: {e}"
            ))),
        }
    }

    fn store_secret(&self, name: &str, secret: &[u8]) -> Result<(), MeshError> {
        Self::entry(name)?
            .set_secret(secret)
            .map_err(|e| MeshError::Identity(format!("{STORE_NAME} would not store {name}: {e}")))
    }

    fn delete_secret(&self, name: &str) -> Result<(), MeshError> {
        self.delete(name)
    }

    fn describe_secret_store(&self) -> String {
        STORE_NAME.to_string()
    }
}

/// Keys that vanish with the process.
///
/// For tests, and for anything that legitimately wants an identity that lasts
/// exactly one run.
#[derive(Debug, Default)]
pub struct MemoryKeyStore {
    /// One namespace for keys and secrets alike, deliberately.
    ///
    /// The OS store files every entry under [`KEY_SERVICE`] by name, so a
    /// double with a map per kind would let a test hold a key and a secret
    /// under one name — a state the machine cannot reach, proving a property
    /// the real store does not have.
    secrets: parking_lot::Mutex<HashMap<String, Zeroizing<Vec<u8>>>>,
}

impl MemoryKeyStore {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }
}

impl KeyStore for MemoryKeyStore {
    fn load(&self, name: &str) -> Result<Option<Zeroizing<[u8; SECRET_LEN]>>, MeshError> {
        let Some(stored) = self.secrets.lock().get(name).cloned() else {
            return Ok(None);
        };
        // Length-checked here as well as in `OsKeyStore`, because the namespace
        // this shares with `SecretStore` can hold anything: a double that
        // returned the first 32 bytes of a token would be the one place the
        // "never pad or truncate" rule does not hold.
        let bytes = <[u8; SECRET_LEN]>::try_from(stored.as_slice())
            .map_err(|_| wrong_key_length(name, &self.describe(), stored.len()))?;
        Ok(Some(Zeroizing::new(bytes)))
    }

    fn store(&self, name: &str, secret: &[u8; SECRET_LEN]) -> Result<(), MeshError> {
        self.secrets
            .lock()
            .insert(name.to_string(), Zeroizing::new(secret.to_vec()));
        Ok(())
    }

    fn delete(&self, name: &str) -> Result<(), MeshError> {
        self.secrets.lock().remove(name);
        Ok(())
    }

    fn describe(&self) -> String {
        "an in-memory store that forgets on exit".to_string()
    }
}

impl SecretStore for MemoryKeyStore {
    fn load_secret(&self, name: &str) -> Result<Option<Zeroizing<Vec<u8>>>, MeshError> {
        Ok(self.secrets.lock().get(name).cloned())
    }

    fn store_secret(&self, name: &str, secret: &[u8]) -> Result<(), MeshError> {
        self.secrets
            .lock()
            .insert(name.to_string(), Zeroizing::new(secret.to_vec()));
        Ok(())
    }

    fn delete_secret(&self, name: &str) -> Result<(), MeshError> {
        self.delete(name)
    }

    fn describe_secret_store(&self) -> String {
        self.describe()
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

    impl SecretStore for FailingKeyStore {
        fn load_secret(&self, _name: &str) -> Result<Option<Zeroizing<Vec<u8>>>, MeshError> {
            Err(MeshError::Identity("the keychain is locked".into()))
        }
        fn store_secret(&self, _name: &str, _secret: &[u8]) -> Result<(), MeshError> {
            Err(MeshError::Identity("the keychain is locked".into()))
        }
        fn delete_secret(&self, _name: &str) -> Result<(), MeshError> {
            Err(MeshError::Identity("the keychain is locked".into()))
        }
        fn describe_secret_store(&self) -> String {
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

    #[test]
    fn a_token_survives_a_round_trip_and_keeps_its_length() {
        let store = MemoryKeyStore::new();
        // Deliberately not 32 bytes: the length `KeyStore` is fixed at is the
        // one length a token store must not quietly round to.
        let token = b"zt_a-token-of-no-particular-length";
        store.store_secret(CLOUD_TOKEN_NAME, token).expect("store");
        assert_eq!(
            store.load_secret(CLOUD_TOKEN_NAME).expect("load").as_deref().map(Vec::as_slice),
            Some(token.as_slice()),
            "a bearer token that comes back a different length is not the token"
        );
    }

    #[test]
    fn a_machine_that_never_enrolled_reports_no_token_rather_than_failing() {
        let store = MemoryKeyStore::new();
        assert!(
            store.load_secret(CLOUD_TOKEN_NAME).expect("load").is_none(),
            "not being enrolled is the state every machine starts in, not an error"
        );
    }

    // A test asserting that `FailingKeyStore::load_secret` returns `Err` used to
    // live here. It could not fail: the double's body is a hardcoded `Err`
    // twenty lines above, and no production code ran at all.
    //
    // The property it was reaching for is real -- a locked keychain reported as
    // `Ok(None)` tells a person to enrol a machine that already is, and the
    // second enrolment overwrites the token the first one earned. But nothing
    // in this crate consumes a `SecretStore`; the consumer is `zest-daemon`,
    // and the assertion belongs where the code is:
    // `enroll::tests::a_store_that_cannot_be_read_is_refused_before_the_code_is_spent`
    // drives the real `enroll()` against this same double and asserts the code
    // is never spent.

    #[test]
    fn a_forgotten_token_is_gone_and_forgetting_it_again_is_not_an_error() {
        let store = MemoryKeyStore::new();
        store.store_secret(CLOUD_TOKEN_NAME, b"zt_token").expect("store");
        store.delete_secret(CLOUD_TOKEN_NAME).expect("delete");
        assert!(
            store.load_secret(CLOUD_TOKEN_NAME).expect("load").is_none(),
            "--logout that leaves a readable token has not logged anything out"
        );
        store
            .delete_secret(CLOUD_TOKEN_NAME)
            .expect("logging out twice is a no-op, not a failure");
    }

    #[test]
    fn a_token_read_back_as_a_key_is_refused_rather_than_truncated() {
        // Keys and tokens share one namespace because the OS store does, so
        // this is reachable by a name collision rather than only by a corrupt
        // store. Truncating would hand back the first 32 bytes of a token as an
        // identity: every signature valid, and made by a machine nobody has
        // ever heard of.
        let store = MemoryKeyStore::new();
        store
            .store_secret(HOST_KEY_NAME, b"a token that is definitely not a key")
            .expect("store");
        let err = store.load(HOST_KEY_NAME).expect_err("must refuse");
        assert!(
            err.to_string().contains("not 32"),
            "the refusal must say what was wrong with it; got {err}"
        );
    }

    #[test]
    fn one_name_holds_one_thing_in_the_double_as_in_the_real_store() {
        // `keyring` files entries under (KEY_SERVICE, name) and has no notion
        // of a kind, so the second write wins there. A double with a map per
        // kind would let both survive, and a test written against it would pass
        // on a property no machine has.
        let store = MemoryKeyStore::new();
        store.store(HOST_KEY_NAME, &[9u8; SECRET_LEN]).expect("store");
        store.store_secret(HOST_KEY_NAME, b"overwritten").expect("store");
        assert!(
            store.load(HOST_KEY_NAME).is_err(),
            "the key must be gone, not shadowed by a secret stored beside it"
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

    #[cfg(feature = "os-keystore")]
    #[test]
    #[ignore = "writes to this machine's real credential store, and on macOS can \
                block on a keychain prompt; run with `cargo test -p zest-mesh -- --ignored`"]
    fn the_os_key_store_round_trips_a_token_of_arbitrary_length() {
        // Ignored for the same reason as the test above, and worth having for
        // one thing it does not cover: `set_secret`/`get_secret` are the same
        // two calls, but the Windows Credential Manager caps a blob at 2560
        // bytes and the Secret Service does not — so "how long may a token be"
        // is a platform question this is the only way to ask.
        let store = OsKeyStore::new().expect("this machine has a credential store");
        let name = "test-cloud-token";
        let token = b"zt_0123456789abcdef0123456789abcdef";

        store.store_secret(name, token).expect("store");
        let loaded = store.load_secret(name).expect("load");
        store.delete_secret(name).expect("clean up after ourselves");

        assert_eq!(
            loaded.as_deref().map(Vec::as_slice),
            Some(token.as_slice()),
            "the platform store must return exactly the bytes it was given"
        );
    }
}
