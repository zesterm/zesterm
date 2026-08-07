//! Who this machine is, and how it proves it.
//!
//! # The id is the key
//!
//! A [`HostId`] holds the machine's Ed25519 public key verbatim — not a random
//! number that happens to be unique, and not a hash of the key. A UUID would be
//! just as unique and would prove nothing: once a relay is in the path, "this
//! message says it is from my Mac" and "this message is from my Mac" stop being
//! the same statement, and only a signature closes the gap. Discovering that
//! later means reissuing every stored id, across a config file, a directory row
//! and whatever a phone remembered. → ADR-006.
//!
//! Hashing the key into a shorter fingerprint was the other candidate and buys
//! less than it looks like. Ed25519 public keys are already 32 bytes, exactly
//! what a `HostId` holds, so there is nothing to compress and no secrecy to
//! protect — and a hash would mean the key has to travel *beside* the id
//! everywhere, with a `hash(key) == id` check at each site. Every one of those
//! checks is an authorization check that can be written to ask the wrong
//! question, which is the hazard `zest-proto`'s split of `HostId` from
//! `ClientId` exists to avoid.
//!
//! The cost, stated plainly: `HostId` is now Ed25519-specific, and a different
//! signature algorithm would mean reissuing ids. It would also mean re-pairing
//! every machine, because the trust anchor itself would have changed — so the
//! id was never the expensive part of that migration.
//!
//! One consequence is easy to miss. `HostId` is a frozen contract with a public
//! `[u8; 32]` field and a `from_bytes` that accepts anything, so **not every
//! `HostId` is a valid identity**: the bytes have to decompress to a point on
//! Curve25519, and roughly half of all 32-byte strings do not. Validity is
//! therefore checked where a key is *used*, in [`verify_host`] and
//! [`verify_client`], and those are the only doors. (The
//! `HostId::from_bytes([1; 32])` fixtures elsewhere in this crate happen to be
//! points, but nobody holds the matching secret, so they can route and cannot
//! sign — which is exactly what routing tests need.)
//!
//! # What gets signed
//!
//! Never the caller's bytes alone. A key that will sign any 32 bytes handed to
//! it is a signing oracle for every other part of the system that signs 32
//! bytes: ask a host to "authenticate" against a nonce that is really an
//! enrollment approval and it hands one over. So the preimage carries a
//! versioned domain, the [`Role`] and the [`Purpose`] ahead of the message, and
//! signatures minted in one context are not valid in any other.

use core::fmt;

use ed25519_dalek::{Signer, SigningKey, VerifyingKey};
use zeroize::Zeroizing;
use zest_proto::{ClientId, HostId};

use crate::keystore::{KeyStore, CLIENT_KEY_NAME, HOST_KEY_NAME, SECRET_LEN};
use crate::MeshError;

/// The signature scheme these ids name. Recorded so a log line, a directory row
/// or a pairing screen can say which algorithm an id came from — an id that
/// never says what it is cannot be migrated gracefully.
pub const HOST_KEY_ALGORITHM: &str = "ed25519";

/// A signature is 64 bytes, always.
pub const SIGNATURE_LEN: usize = 64;

/// A challenge is 32 bytes, always.
pub const NONCE_LEN: usize = 32;

/// The prefix every signed preimage starts with.
///
/// This, and not the id, is where algorithm agility lives: `v2` invalidates
/// every old signature without touching the id scheme.
const SIGNING_DOMAIN: &[u8] = b"zesterm-sig-v1";

/// Which key produced a signature.
///
/// Part of the signing domain rather than merely a Rust type, so that a machine
/// which is both a host and a client — the ordinary case, per ADR-006 — cannot
/// have a host-role signature replayed as a client-role proof even if the two
/// roles ever shared key material.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    Host,
    Client,
}

impl Role {
    const fn as_domain(self) -> &'static [u8] {
        match self {
            Self::Host => b"host",
            Self::Client => b"client",
        }
    }
}

/// What a signature is *for*.
///
/// An enum rather than a `&str`, because a caller who can pass any string can
/// pass the wrong one, and the failure mode is a signature that is valid in a
/// context nobody intended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum Purpose {
    /// Proving possession of the key behind an id. Pairing, and `Hello`.
    Auth,
    /// Approving a device into the fleet.
    Enrollment,
    /// A short-lived attach ticket. M4; the domain is reserved now so that
    /// signatures minted then cannot collide with signatures minted today.
    AttachTicket,
}

impl Purpose {
    const fn as_domain(self) -> &'static [u8] {
        match self {
            Self::Auth => b"auth",
            Self::Enrollment => b"enrollment",
            Self::AttachTicket => b"attach-ticket",
        }
    }
}

/// The exact bytes an Ed25519 key is asked to sign.
///
/// `SIGNING_DOMAIN \0 role \0 purpose \0 message`.
///
/// NUL separators rather than length prefixes are sound here for one specific
/// reason: `role` and `purpose` come from closed enums with fixed NUL-free
/// ASCII names, so no shorter field can be extended to look like a longer one,
/// and the only attacker-influenced field is last. Add a caller-supplied string
/// to the domain and this must become length-prefixed.
///
/// Deliberately a prefix rather than `Ed25519ctx` (dalek's `with_context`),
/// which is the textbook answer and the wrong one here: the web and phone
/// clients will verify these signatures with `@noble/ed25519` or `tweetnacl`,
/// and neither implements Ed25519ctx. Prefix-then-plain-Ed25519 is verifiable
/// by every library in every language.
fn preimage(role: Role, purpose: Purpose, message: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(SIGNING_DOMAIN.len() + 24 + message.len());
    out.extend_from_slice(SIGNING_DOMAIN);
    out.push(0);
    out.extend_from_slice(role.as_domain());
    out.push(0);
    out.extend_from_slice(purpose.as_domain());
    out.push(0);
    out.extend_from_slice(message);
    out
}

/// An Ed25519 signature.
///
/// Deliberately not `ed25519_dalek::Signature`. The wire format is 64 bytes and
/// nothing else, and re-exporting a dependency's type would make a `dalek` major
/// bump a breaking change to this crate and to everything behind it.
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct Signature([u8; SIGNATURE_LEN]);

impl Signature {
    #[must_use]
    pub const fn from_bytes(bytes: [u8; SIGNATURE_LEN]) -> Self {
        Self(bytes)
    }

    #[must_use]
    pub const fn to_bytes(self) -> [u8; SIGNATURE_LEN] {
        self.0
    }

    /// A short slice is an error, never left-padded — the same rule
    /// `zest-proto` applies to a truncated id, and for the same reason.
    pub fn from_slice(bytes: &[u8]) -> Result<Self, MeshError> {
        <[u8; SIGNATURE_LEN]>::try_from(bytes)
            .map(Self)
            .map_err(|_| {
                MeshError::BadSignature(format!(
                    "a signature is {SIGNATURE_LEN} bytes, this one is {}",
                    bytes.len()
                ))
            })
    }
}

impl fmt::Debug for Signature {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Signature({})", short_hex(&self.0))
    }
}

/// A one-shot challenge, generated by whichever side is doing the asking.
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct Nonce([u8; NONCE_LEN]);

impl Nonce {
    /// 32 bytes from the OS.
    ///
    /// Fails only when the OS RNG is unavailable, which on every platform this
    /// runs on means the process is already in trouble.
    pub fn random() -> Result<Self, MeshError> {
        let mut bytes = [0u8; NONCE_LEN];
        getrandom::fill(&mut bytes)
            .map_err(|e| MeshError::Identity(format!("the OS random source failed: {e}")))?;
        Ok(Self(bytes))
    }

    #[must_use]
    pub const fn from_bytes(bytes: [u8; NONCE_LEN]) -> Self {
        Self(bytes)
    }

    #[must_use]
    pub const fn as_bytes(&self) -> &[u8; NONCE_LEN] {
        &self.0
    }
}

impl fmt::Debug for Nonce {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Nonce({})", short_hex(&self.0))
    }
}

/// This machine's long-lived identity as a host.
///
/// Not `Clone`: one process, one identity. Share it behind an `Arc`.
pub struct HostIdentity {
    key: SigningKey,
    id: HostId,
}

impl HostIdentity {
    /// A brand-new keypair. Callers almost always want
    /// [`HostIdentity::load_or_create`] instead.
    pub fn generate() -> Result<Self, MeshError> {
        Ok(Self::from_secret_bytes(&*random_secret()?))
    }

    #[must_use]
    pub fn from_secret_bytes(secret: &[u8; SECRET_LEN]) -> Self {
        let key = SigningKey::from_bytes(secret);
        let id = HostId::from_bytes(key.verifying_key().to_bytes());
        Self { key, id }
    }

    /// Read this machine's key from `store`, creating one on the first run.
    ///
    /// Idempotent, and it never overwrites a key that is already there: a host
    /// that silently reissues its identity has removed itself from every fleet
    /// listing that had learned the old one.
    pub fn load_or_create(store: &dyn KeyStore) -> Result<Self, MeshError> {
        Ok(Self::from_secret_bytes(&*load_or_create_secret(
            store,
            HOST_KEY_NAME,
        )?))
    }

    #[must_use]
    pub const fn host_id(&self) -> HostId {
        self.id
    }

    #[must_use]
    pub fn sign(&self, purpose: Purpose, message: &[u8]) -> Signature {
        Signature(
            self.key
                .sign(&preimage(Role::Host, purpose, message))
                .to_bytes(),
        )
    }

    /// The raw secret, for handing to a [`KeyStore`]. Scrubbed on drop.
    #[must_use]
    pub fn secret_bytes(&self) -> Zeroizing<[u8; SECRET_LEN]> {
        Zeroizing::new(self.key.to_bytes())
    }
}

// Hand-written, and load-bearing: `tracing` formats its fields with `Debug`, so
// a derived one would put this machine's private key in the log the first time
// anyone writes `tracing::debug!(?identity)`.
impl fmt::Debug for HostIdentity {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "HostIdentity({})", self.id.short())
    }
}

impl fmt::Display for HostIdentity {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.id.short())
    }
}

/// A device that attaches to hosts. The same construction, a different role.
///
/// Separate from [`HostIdentity`] rather than one type with a role parameter,
/// for the reason `zest-proto` keeps `HostId` and `ClientId` apart: a machine
/// can be both, and conflating the two is how an authorization check ends up
/// asking the wrong question. A phantom type parameter would read as "these are
/// the same thing".
pub struct ClientIdentity {
    key: SigningKey,
    id: ClientId,
}

impl ClientIdentity {
    pub fn generate() -> Result<Self, MeshError> {
        Ok(Self::from_secret_bytes(&*random_secret()?))
    }

    #[must_use]
    pub fn from_secret_bytes(secret: &[u8; SECRET_LEN]) -> Self {
        let key = SigningKey::from_bytes(secret);
        let id = ClientId::from_bytes(key.verifying_key().to_bytes());
        Self { key, id }
    }

    /// See [`HostIdentity::load_or_create`].
    pub fn load_or_create(store: &dyn KeyStore) -> Result<Self, MeshError> {
        Ok(Self::from_secret_bytes(&*load_or_create_secret(
            store,
            CLIENT_KEY_NAME,
        )?))
    }

    #[must_use]
    pub const fn client_id(&self) -> ClientId {
        self.id
    }

    #[must_use]
    pub fn sign(&self, purpose: Purpose, message: &[u8]) -> Signature {
        Signature(
            self.key
                .sign(&preimage(Role::Client, purpose, message))
                .to_bytes(),
        )
    }

    #[must_use]
    pub fn secret_bytes(&self) -> Zeroizing<[u8; SECRET_LEN]> {
        Zeroizing::new(self.key.to_bytes())
    }
}

impl fmt::Debug for ClientIdentity {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "ClientIdentity({})", self.id.short())
    }
}

impl fmt::Display for ClientIdentity {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.id.short())
    }
}

/// Did `host` sign `message` for `purpose`?
pub fn verify_host(
    host: HostId,
    purpose: Purpose,
    message: &[u8],
    signature: &Signature,
) -> Result<(), MeshError> {
    let key = verifying_key(&host.0, &host.short())?;
    verify_with(&key, Role::Host, purpose, message, signature, &host.short())
}

/// Did `client` sign `message` for `purpose`?
///
/// Two functions rather than one taking "some id", because the two id types are
/// separate precisely so that an authorization check cannot accept a client's
/// proof where a host's was required. A single generic entry point would hand
/// that mistake straight back.
pub fn verify_client(
    client: ClientId,
    purpose: Purpose,
    message: &[u8],
    signature: &Signature,
) -> Result<(), MeshError> {
    let key = verifying_key(&client.0, &client.short())?;
    verify_with(
        &key,
        Role::Client,
        purpose,
        message,
        signature,
        &client.short(),
    )
}

/// The only place an id becomes a key.
///
/// Private on purpose: a caller who needs the key object is a caller who is
/// about to verify something, and that should happen here where the domain
/// separation and the strictness are not optional.
fn verifying_key(bytes: &[u8; 32], who: &str) -> Result<VerifyingKey, MeshError> {
    let key = VerifyingKey::from_bytes(bytes)
        .map_err(|_| MeshError::Identity(format!("{who} is not a valid ed25519 public key")))?;
    // `[0u8; 32]` is a perfectly valid encoding of the identity element, and a
    // small-order key verifies almost anything under permissive rules.
    // Rejecting here means such a key cannot enter the fleet at all, rather
    // than merely failing later at some call site that remembered to check.
    if key.is_weak() {
        return Err(MeshError::Identity(format!(
            "{who} is a small-order ed25519 key and cannot identify a machine"
        )));
    }
    Ok(key)
}

fn verify_with(
    key: &VerifyingKey,
    role: Role,
    purpose: Purpose,
    message: &[u8],
    signature: &Signature,
    who: &str,
) -> Result<(), MeshError> {
    let sig = ed25519_dalek::Signature::from_bytes(&signature.0);
    // `verify_strict`, not `Verifier::verify`: it rejects non-canonical
    // encodings and small-order components, and "this message verifies under
    // exactly one identity" is the property the whole design rests on once the
    // id *is* the key.
    key.verify_strict(&preimage(role, purpose, message), &sig)
        .map_err(|_| MeshError::BadSignature(who.to_string()))
}

fn random_secret() -> Result<Zeroizing<[u8; SECRET_LEN]>, MeshError> {
    let mut bytes = Zeroizing::new([0u8; SECRET_LEN]);
    getrandom::fill(bytes.as_mut_slice())
        .map_err(|e| MeshError::Identity(format!("the OS random source failed: {e}")))?;
    Ok(bytes)
}

fn load_or_create_secret(
    store: &dyn KeyStore,
    name: &str,
) -> Result<Zeroizing<[u8; SECRET_LEN]>, MeshError> {
    // Note the shape: a `load` error propagates rather than falling through to
    // generation. A store that could not be read is not a store with no key.
    if let Some(existing) = store.load(name)? {
        return Ok(existing);
    }
    let fresh = random_secret()?;
    store.store(name, &fresh)?;
    tracing::info!(store = %store.describe(), key = name, "minted a new identity");
    Ok(fresh)
}

fn short_hex(bytes: &[u8]) -> String {
    bytes[..4].iter().fold(String::new(), |mut acc, b| {
        use fmt::Write as _;
        let _ = write!(acc, "{b:02x}");
        acc
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::keystore::tests::FailingKeyStore;
    use crate::keystore::MemoryKeyStore;

    fn host(seed: u8) -> HostIdentity {
        HostIdentity::from_secret_bytes(&[seed; SECRET_LEN])
    }

    #[test]
    fn the_same_key_always_yields_the_same_host_id() {
        assert_eq!(
            host(1).host_id(),
            host(1).host_id(),
            "an id derived from a key must be a function of that key alone, or a \
             machine's name changes when nothing about the machine did"
        );
    }

    #[test]
    fn two_different_keys_never_yield_the_same_host_id() {
        assert_ne!(
            host(1).host_id(),
            host(2).host_id(),
            "two machines sharing an id would make every authorization check on \
             either of them answer for both"
        );
    }

    #[test]
    fn a_host_signature_verifies_against_its_own_id() {
        let me = host(3);
        let nonce = Nonce::from_bytes([9; NONCE_LEN]);
        let sig = me.sign(Purpose::Auth, nonce.as_bytes());
        verify_host(me.host_id(), Purpose::Auth, nonce.as_bytes(), &sig)
            .expect("the whole point: a host can prove it holds the key its id names");
    }

    #[test]
    fn a_signature_over_a_different_message_is_rejected() {
        let me = host(3);
        let sig = me.sign(Purpose::Auth, b"nonce-a");
        assert!(
            verify_host(me.host_id(), Purpose::Auth, b"nonce-b", &sig).is_err(),
            "a signature that verifies against any message proves possession of \
             nothing, because it can be replayed against a fresh challenge"
        );
    }

    #[test]
    fn a_signature_from_a_different_host_is_rejected() {
        let impostor = host(4);
        let sig = impostor.sign(Purpose::Auth, b"nonce");
        assert!(
            verify_host(host(5).host_id(), Purpose::Auth, b"nonce", &sig).is_err(),
            "an id that verifies a signature it did not produce would make \
             impersonation free, which is the entire thing the key exists to prevent"
        );
    }

    #[test]
    fn a_host_signature_is_not_valid_as_a_client_signature() {
        // Same key material, both roles — the Windows box, which hosts its own
        // shells and attaches to the Mac.
        let secret = [6u8; SECRET_LEN];
        let as_host = HostIdentity::from_secret_bytes(&secret);
        let as_client = ClientIdentity::from_secret_bytes(&secret);
        let sig = as_host.sign(Purpose::Auth, b"nonce");

        assert!(
            verify_client(as_client.client_id(), Purpose::Auth, b"nonce", &sig).is_err(),
            "without the role in the signing domain, a machine's host proof would \
             also be its client proof, and the two authorizations are not the same"
        );
    }

    #[test]
    fn a_signature_for_one_purpose_does_not_verify_for_another() {
        let me = host(7);
        let sig = me.sign(Purpose::Auth, b"payload");
        assert!(
            verify_host(me.host_id(), Purpose::Enrollment, b"payload", &sig).is_err(),
            "otherwise a peer that can get a host to answer an auth challenge has \
             also got it to sign an enrollment approval"
        );
    }

    #[test]
    fn a_host_id_that_is_not_a_curve_point_is_rejected_rather_than_ignored() {
        // `HostId` is frozen with a public field, so nothing stops arbitrary
        // bytes being called an id. The check has to live at the point of use.
        // `[2; 32]` does not decompress to a curve point; note that `[1; 32]`
        // and `[0xff; 32]` do, so the fixture matters.
        let nonsense = HostId::from_bytes([2; 32]);
        let err = verify_host(
            nonsense,
            Purpose::Auth,
            b"nonce",
            &Signature::from_bytes([0; SIGNATURE_LEN]),
        )
        .expect_err("bytes that are not a public key cannot identify a machine");
        assert!(
            matches!(err, MeshError::Identity(_)),
            "an unusable id is an identity problem, not a peer that failed to \
             prove itself: {err}"
        );
    }

    #[test]
    fn a_small_order_key_cannot_be_a_host_id() {
        // A valid point encoding, and one that verifies almost anything under
        // permissive rules — so it must be refused at the door.
        let weak = HostId::from_bytes([0u8; 32]);
        assert!(
            verify_host(
                weak,
                Purpose::Auth,
                b"nonce",
                &Signature::from_bytes([0; SIGNATURE_LEN])
            )
            .is_err(),
            "a small-order key would let one id answer for signatures it never made"
        );
    }

    #[test]
    fn a_truncated_signature_is_an_error_not_a_pad() {
        assert!(
            Signature::from_slice(&[0u8; SIGNATURE_LEN - 1]).is_err(),
            "padding a short signature out to 64 bytes invents bytes nobody signed"
        );
    }

    #[test]
    fn two_nonces_are_never_the_same() {
        let a = Nonce::random().expect("the OS random source");
        let b = Nonce::random().expect("the OS random source");
        assert_ne!(
            a.as_bytes(),
            b.as_bytes(),
            "a challenge that repeats is not a challenge: an answer to the last \
             one is an answer to this one"
        );
    }

    #[test]
    fn the_secret_never_appears_in_a_debug_print() {
        let me = HostIdentity::from_secret_bytes(&[0xab; SECRET_LEN]);
        let printed = format!("{me:?}");
        assert!(
            !printed.contains("abab"),
            "`tracing` formats its fields with `Debug`, so a derived one would put \
             this machine's private key in the log the first time anyone wrote \
             `tracing::debug!(?identity)`: {printed}"
        );
    }

    #[test]
    fn an_identity_is_created_once_and_then_loaded() {
        let store = MemoryKeyStore::new();
        let first = HostIdentity::load_or_create(&store).expect("first run mints a key");
        let second = HostIdentity::load_or_create(&store).expect("second run loads it");
        assert_eq!(
            first.host_id(),
            second.host_id(),
            "a daemon that reissued its identity on every restart would drop out \
             of every fleet listing that had learned the previous one"
        );
    }

    #[test]
    fn a_host_and_a_client_on_one_machine_do_not_share_a_key() {
        let store = MemoryKeyStore::new();
        let as_host = HostIdentity::load_or_create(&store).expect("host key");
        let as_client = ClientIdentity::load_or_create(&store).expect("client key");
        assert_ne!(
            as_host.host_id().0,
            as_client.client_id().0,
            "the roles are separate so that an authorization check cannot ask the \
             wrong question; one shared key would make them the same question"
        );
    }

    #[test]
    fn a_store_that_cannot_be_read_does_not_produce_a_fresh_identity() {
        assert!(
            HostIdentity::load_or_create(&FailingKeyStore).is_err(),
            "treating a locked keychain as an empty one mints a second identity \
             for a machine that already had one, and nothing logs an error"
        );
    }
}
