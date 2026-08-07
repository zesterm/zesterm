//! Proving who is on the other end, and asking a person about it once.
//!
//! Pure. This module takes ids, nonces and signatures — never a
//! `ClientMessage` — so the state machine can be tested without building a
//! single wire message, and so a protocol change and a change to the signing
//! scheme can never be the same commit.
//!
//! # The handshake, and why it costs a round trip
//!
//! ```text
//! C -> H  Hello     { version, client, label, nonce_c }
//! H -> C  Challenge { version, host, label, nonce_h, sig_h }
//! C -> H  Auth      { sig_c }
//! H -> C  Welcome | AuthPending | AuthFailed
//! ```
//!
//! A signature over only what the *client* chose is a recording waiting to be
//! replayed. The host has to contribute freshness, and it therefore has to
//! speak before the client's proof exists.
//!
//! **The host signs first, deliberately.** A client dialling an address it
//! learned from an mDNS advertisement must be able to check that the machine
//! which answered is the `HostId` the advertisement claimed, and hang up before
//! revealing anything. The cost, stated rather than hidden: the host will sign
//! a transcript containing an attacker-chosen nonce for anyone who connects.
//! Ed25519 has no chosen-message weakness and the signing domain confines those
//! signatures to exactly what they already are, so this is a bounded CPU cost
//! and not a key-recovery risk.
//!
//! # What is signed
//!
//! One transcript, signed by *both* sides over identical bytes.
//! `identity::Role` is already inside the preimage, so a reflected signature —
//! bouncing the client's proof back as the host's — fails by construction.
//!
//! The labels are in there because the label is what a person reads on the
//! approval prompt. An unsigned label lets anyone on the path make the host ask
//! "pair with andy-phone?" for a connection that is not the phone, and the
//! prompt text is the entire decision input.
//!
//! The version is in there so a future protocol cannot be downgraded into a
//! transcript that omits whatever it added.
//!
//! # What this does not protect
//!
//! Entity authentication, not a secure channel. No key is derived, so
//! everything after the handshake is plaintext — an attacker who can relay the
//! handshake verbatim holds two connections and reads and rewrites all of it.
//! The matching code is the only defence against that, and only if a person
//! compares it. → M5's Noise IK closes it, reusing these same keys.
//!
//! # Locking
//!
//! **The queue lock is never taken while a connection lock is held.** The
//! approval callback does nothing but send on a channel, which is what keeps
//! that true. A blocking approver would hold a connection lock for two minutes
//! and make it impossible to send the "waiting for approval" message at all.

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use parking_lot::Mutex;
use sha2::{Digest, Sha256};
use zest_proto::{ClientId, HostId};

use crate::identity::{verify_client, verify_host, HostIdentity, Nonce, Purpose, Signature};
use crate::MeshError;

/// Domain separator for the authentication transcript.
const AUTH_DOMAIN: &[u8] = b"zesterm-auth-v1";

/// Domain separator for the matching code, so it can never collide with a
/// signature preimage.
const CODE_DOMAIN: &[u8] = b"zesterm-pairing-code-v1\0";

/// How many digits a person compares.
pub const PAIRING_CODE_DIGITS: u32 = 6;

/// Everything both ends agree they are talking about.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Transcript {
    pub version: u16,
    pub host: HostId,
    pub client: ClientId,
    pub host_nonce: Nonce,
    pub client_nonce: Nonce,
    pub host_label: String,
    pub client_label: String,
}

/// The exact bytes both sides sign.
///
/// Fixed-width fields first, then the two labels length-prefixed.
/// `identity::preimage` uses NUL separators and is sound only because the one
/// attacker-influenced field is last; labels are attacker-influenced *and*
/// variable, so they carry their own lengths rather than relying on that.
///
/// **Changing this layout unpairs every device in the field**, with the message
/// "did not prove its identity". A golden test pins it so that can only happen
/// as a deliberate diff.
#[must_use]
pub fn auth_transcript(t: &Transcript) -> Vec<u8> {
    let host_label = t.host_label.as_bytes();
    let client_label = t.client_label.as_bytes();

    let mut out = Vec::with_capacity(
        AUTH_DOMAIN.len() + 2 + 32 + 32 + 32 + 32 + 2 + host_label.len() + 2 + client_label.len(),
    );
    out.extend_from_slice(AUTH_DOMAIN);
    out.extend_from_slice(&t.version.to_be_bytes());
    out.extend_from_slice(&t.host.0);
    out.extend_from_slice(&t.client.0);
    out.extend_from_slice(t.host_nonce.as_bytes());
    out.extend_from_slice(t.client_nonce.as_bytes());
    // Truncated rather than refused: a label is cosmetic, and a host that
    // refuses to pair because someone named their laptop at length is worse
    // than one that shows the first 64KB of it.
    push_len_prefixed(&mut out, host_label);
    push_len_prefixed(&mut out, client_label);
    out
}

fn push_len_prefixed(out: &mut Vec<u8>, bytes: &[u8]) {
    let len = u16::try_from(bytes.len()).unwrap_or(u16::MAX);
    out.extend_from_slice(&len.to_be_bytes());
    out.extend_from_slice(&bytes[..len as usize]);
}

/// The code a person compares between two screens.
///
/// Derived from the transcript the signatures cover, because its entire job is
/// to *differ* on the two screens when a relay is in the path: a relay runs two
/// handshakes with two nonce pairs, so the codes diverge. Deriving it from
/// anything stable — the ids, the labels — would make it constant, and a
/// constant code is one an attacker can display too.
///
/// [`HostId::short`] is **not** this. That identifies a machine and is the same
/// at every pairing; the prompt shows both, because they answer different
/// questions: *what claims to be connecting* and *is this the connection I am
/// looking at*.
///
/// Six digits: one online guess per connection at 1-in-10⁶, each needing a
/// fresh connection and a person at a prompt. Digits rather than hex because it
/// is read aloud and typed on a phone, where `0`/`O` and `b`/`6` are a real
/// problem.
#[must_use]
pub fn pairing_code(t: &Transcript) -> String {
    let mut hasher = Sha256::new();
    hasher.update(CODE_DOMAIN);
    hasher.update(auth_transcript(t));
    let digest = hasher.finalize();
    let n = u32::from_be_bytes([digest[0], digest[1], digest[2], digest[3]]);
    let modulus = 10u32.pow(PAIRING_CODE_DIGITS);
    format!("{:0width$}", n % modulus, width = PAIRING_CODE_DIGITS as usize)
}

/// What the host should do next.
#[derive(Debug, Clone, PartialEq, Eq)]
#[must_use]
pub enum HostStep {
    /// Answer with this nonce and proof.
    Challenge { nonce: Nonce, signature: Signature },
    /// Serve the connection.
    Welcome,
    /// The key is good but unknown. Show the code and wait for a person.
    NeedsApproval { code: String },
    /// Do not serve. The reason is the client's to branch on.
    Refused(Refusal),
}

/// Why a host refused, in terms `zest-proto`'s `AuthFailure` maps onto.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Refusal {
    Version,
    Signature,
    UnknownClient,
    Denied,
    TimedOut,
}

/// The host half of the handshake, for one connection.
///
/// One per connection, and that is what makes the nonce cache nobody has to
/// write: the host's nonce is per-connection state, so a captured `Auth` is
/// useless anywhere else. A cache only becomes necessary for M4's stateless
/// attach tickets, which is why `Purpose::AttachTicket` is already reserved.
pub struct HostHandshake {
    identity: std::sync::Arc<HostIdentity>,
    label: String,
    version: u16,
    state: State,
}

enum State {
    /// Nothing yet. Only `Hello` is accepted.
    Greeting,
    /// Challenged, waiting for the proof.
    Challenged(Box<Transcript>),
    /// Proved, waiting for a person.
    Pending(Box<Transcript>),
    /// Served.
    Ready,
    /// Finished, badly.
    Refused,
}

impl HostHandshake {
    pub fn new(identity: std::sync::Arc<HostIdentity>, label: impl Into<String>, version: u16) -> Self {
        Self { identity, label: label.into(), version, state: State::Greeting }
    }

    /// Whether this connection may be served.
    #[must_use]
    pub fn is_ready(&self) -> bool {
        matches!(self.state, State::Ready)
    }

    /// The transcript under negotiation, for a prompt.
    #[must_use]
    pub fn transcript(&self) -> Option<&Transcript> {
        match &self.state {
            State::Challenged(t) | State::Pending(t) => Some(t),
            _ => None,
        }
    }

    /// Answer a `Hello`.
    pub fn on_hello(
        &mut self,
        version: u16,
        client: ClientId,
        client_label: &str,
        client_nonce: Nonce,
    ) -> HostStep {
        if !matches!(self.state, State::Greeting) {
            self.state = State::Refused;
            return HostStep::Refused(Refusal::Signature);
        }
        if version != self.version {
            self.state = State::Refused;
            return HostStep::Refused(Refusal::Version);
        }
        // An all-zero nonce is the `#[serde(default)]` on the wire, which means
        // a peer that did not send one at all.
        if client_nonce.as_bytes() == &[0u8; 32] {
            self.state = State::Refused;
            return HostStep::Refused(Refusal::Signature);
        }

        let Ok(host_nonce) = Nonce::random() else {
            // No randomness means no freshness, and a fixed nonce is worse than
            // refusing: it would look like a working handshake.
            self.state = State::Refused;
            return HostStep::Refused(Refusal::Signature);
        };

        let transcript = Transcript {
            version: self.version,
            host: self.identity.host_id(),
            client,
            host_nonce,
            client_nonce,
            host_label: self.label.clone(),
            client_label: client_label.to_string(),
        };
        let signature = self.identity.sign(Purpose::Auth, &auth_transcript(&transcript));
        self.state = State::Challenged(Box::new(transcript));
        HostStep::Challenge { nonce: host_nonce, signature }
    }

    /// Check the client's proof, then whether it is trusted.
    ///
    /// **In that order, always.** Consulting the trust store first would let
    /// anyone who knows a `ClientId` make this machine pop an approval prompt
    /// without holding the key — prompt-spam, and a social-engineering vector.
    /// A test enforces it with a store that panics if it is read.
    pub fn on_auth(
        &mut self,
        signature: &Signature,
        trust: &dyn crate::trust::TrustStore,
    ) -> HostStep {
        let State::Challenged(transcript) = &self.state else {
            self.state = State::Refused;
            return HostStep::Refused(Refusal::Signature);
        };
        let transcript = transcript.clone();

        if verify_client(
            transcript.client,
            Purpose::Auth,
            &auth_transcript(&transcript),
            signature,
        )
        .is_err()
        {
            self.state = State::Refused;
            return HostStep::Refused(Refusal::Signature);
        }

        match trust.get(transcript.client) {
            Ok(Some(_)) => {
                self.state = State::Ready;
                HostStep::Welcome
            }
            Ok(None) => {
                let code = pairing_code(&transcript);
                self.state = State::Pending(transcript);
                HostStep::NeedsApproval { code }
            }
            Err(e) => {
                // A trust store that cannot be read is an operator problem, and
                // serving anyway would be serving without knowing who is
                // trusted.
                tracing::error!(error = %e, "trust store unreadable; refusing");
                self.state = State::Refused;
                HostStep::Refused(Refusal::UnknownClient)
            }
        }
    }

    /// A person said yes.
    ///
    /// **Writes the record before returning `Welcome`.** A crash in between
    /// would otherwise leave someone who watched themselves approve a device
    /// with a device that is not paired.
    pub fn approved(&mut self, trust: &dyn crate::trust::TrustStore) -> HostStep {
        let State::Pending(transcript) = &self.state else {
            return HostStep::Refused(Refusal::Denied);
        };
        let record = crate::trust::TrustRecord {
            client: transcript.client,
            label: transcript.client_label.clone(),
            paired_at: std::time::SystemTime::now(),
            last_seen: None,
        };
        if let Err(e) = trust.insert(record) {
            tracing::error!(error = %e, "could not record the pairing; refusing");
            self.state = State::Refused;
            return HostStep::Refused(Refusal::UnknownClient);
        }
        self.state = State::Ready;
        HostStep::Welcome
    }

    /// A person said no, or nobody answered.
    pub fn denied(&mut self, reason: Refusal) -> HostStep {
        self.state = State::Refused;
        HostStep::Refused(reason)
    }
}

// --- the approval queue ----------------------------------------------------

/// A device asking to be let in.
#[derive(Debug, Clone)]
pub struct PairingRequest {
    pub client: ClientId,
    pub label: String,
    pub code: String,
    /// For the prompt: "from 192.168.1.42".
    pub remote: String,
    pub requested_at: Instant,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decision {
    Approve,
    Deny,
}

/// How long a request waits before it gives up.
pub const APPROVAL_TIMEOUT: Duration = Duration::from_secs(120);

struct Pending {
    request: PairingRequest,
    notify: Box<dyn FnOnce(Decision) + Send>,
}

/// Requests waiting for a person, and whoever is watching for them.
///
/// The front end is deliberately not part of this. A terminal prompt on the
/// daemon's stdin, `--trust` on the command line, and the desktop app's modal
/// are three views of the same queue, which is what lets the modal land later
/// as a front-end swap rather than a second mechanism.
#[derive(Default)]
pub struct PairingQueue {
    pending: Mutex<Vec<(u64, Pending)>>,
    next_id: AtomicU64,
    on_request: Mutex<Option<Box<dyn Fn() + Send + Sync>>>,
}

/// Cancels its request when dropped.
///
/// A prompt for a device that has already hung up is exactly what teaches
/// someone to dismiss prompts without reading them.
pub struct PendingHandle {
    id: u64,
    queue: std::sync::Weak<PairingQueue>,
}

impl Drop for PendingHandle {
    fn drop(&mut self) {
        if let Some(q) = self.queue.upgrade() {
            q.cancel(self.id);
        }
    }
}

impl PairingQueue {
    #[must_use]
    pub fn new() -> std::sync::Arc<Self> {
        std::sync::Arc::new(Self::default())
    }

    /// Be told when a request arrives, so a UI can raise a prompt without
    /// polling — which is what would cost the 0%-idle guarantee.
    pub fn on_request(&self, f: impl Fn() + Send + Sync + 'static) {
        *self.on_request.lock() = Some(Box::new(f));
    }

    /// Queue a request. `notify` is called exactly once, from whichever thread
    /// resolves it.
    pub fn submit(
        self: &std::sync::Arc<Self>,
        request: PairingRequest,
        notify: Box<dyn FnOnce(Decision) + Send>,
    ) -> PendingHandle {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        self.pending.lock().push((id, Pending { request, notify }));
        // Outside the lock: a UI callback is arbitrary code, and running it
        // while holding the queue lock is how a deadlock is built.
        if let Some(f) = self.on_request.lock().as_ref() {
            f();
        }
        PendingHandle { id, queue: std::sync::Arc::downgrade(self) }
    }

    /// Everything waiting, for a prompt or a listing.
    #[must_use]
    pub fn pending(&self) -> Vec<PairingRequest> {
        self.pending.lock().iter().map(|(_, p)| p.request.clone()).collect()
    }

    /// Answer every request from this client.
    ///
    /// By client rather than by id: a person approving "andy-phone" is
    /// approving the device, and a device that retried while they were reading
    /// should not be left waiting behind its own duplicate.
    pub fn resolve(&self, client: ClientId, decision: Decision) -> usize {
        // Taken out under the lock, notified outside it: `notify` is arbitrary
        // caller code, and running it while holding the queue lock is how a
        // deadlock gets built.
        let taken: Vec<Pending> = {
            let mut pending = self.pending.lock();
            let mut out = Vec::new();
            let mut i = 0;
            while i < pending.len() {
                if pending[i].1.request.client == client {
                    out.push(pending.remove(i).1);
                } else {
                    i += 1;
                }
            }
            out
        };
        let n = taken.len();
        for p in taken {
            (p.notify)(decision);
        }
        n
    }

    /// Give up on anything older than [`APPROVAL_TIMEOUT`].
    pub fn expire(&self, now: Instant) -> usize {
        let expired: Vec<Pending> = {
            let mut pending = self.pending.lock();
            let mut out = Vec::new();
            let mut i = 0;
            while i < pending.len() {
                let age = now.saturating_duration_since(pending[i].1.request.requested_at);
                if age >= APPROVAL_TIMEOUT {
                    out.push(pending.remove(i).1);
                } else {
                    i += 1;
                }
            }
            out
        };
        let n = expired.len();
        for p in expired {
            (p.notify)(Decision::Deny);
        }
        n
    }

    fn cancel(&self, id: u64) {
        let taken = {
            let mut pending = self.pending.lock();
            pending
                .iter()
                .position(|(pid, _)| *pid == id)
                .map(|i| pending.remove(i).1)
        };
        if let Some(p) = taken {
            (p.notify)(Decision::Deny);
        }
    }
}

// --- the client half -------------------------------------------------------

/// Check that the machine which answered is the one that was dialled.
///
/// This is what the host signing first buys: a client that learned an address
/// from an mDNS advertisement can verify the `HostId` before revealing
/// anything. Without it, "connect to my Mac" means "connect to whatever
/// answered on that port".
pub fn verify_challenge(
    expected: Option<HostId>,
    transcript: &Transcript,
    signature: &Signature,
) -> Result<(), MeshError> {
    if let Some(expected) = expected {
        if expected != transcript.host {
            return Err(MeshError::BadSignature(format!(
                "dialled {} and reached {}",
                expected.short(),
                transcript.host.short()
            )));
        }
    }
    verify_host(transcript.host, Purpose::Auth, &auth_transcript(transcript), signature)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::identity::ClientIdentity;
    use crate::trust::{MemoryTrustStore, TrustRecord, TrustStore};
    use std::sync::Arc;

    fn transcript() -> Transcript {
        Transcript {
            version: 2,
            host: HostId::from_bytes([0x11; 32]),
            client: ClientId::from_bytes([0x22; 32]),
            host_nonce: Nonce::from_bytes([0x33; 32]),
            client_nonce: Nonce::from_bytes([0x44; 32]),
            host_label: "andy-mac".into(),
            client_label: "andy-phone".into(),
        }
    }

    #[test]
    fn the_transcript_layout_is_pinned() {
        // A silent change here unpairs every device in the field, and reports
        // it as "did not prove its identity" -- which sends whoever is
        // debugging it looking at the wrong thing entirely. It must only ever
        // change as a deliberate diff to this hex.
        let bytes = auth_transcript(&transcript());
        let digest = Sha256::digest(&bytes);
        let hex: String = digest.iter().map(|b| format!("{b:02x}")).collect();
        assert_eq!(
            hex, "348a45c7c12b2de3fe57c700204c934833583a1e71cea0037b21d1afc0c88cce",
            "the transcript layout changed"
        );
    }

    #[test]
    fn every_field_is_bound_into_the_signature() {
        // "Bound to the connection and to both ids" is four separate claims,
        // so each gets its own assertion rather than one that could pass for
        // the wrong reason.
        let base = auth_transcript(&transcript());

        let mut t = transcript();
        t.host_nonce = Nonce::from_bytes([0xff; 32]);
        assert_ne!(auth_transcript(&t), base, "the host nonce is not covered");

        let mut t = transcript();
        t.client_nonce = Nonce::from_bytes([0xff; 32]);
        assert_ne!(auth_transcript(&t), base, "the client nonce is not covered");

        let mut t = transcript();
        t.client = ClientId::from_bytes([0xff; 32]);
        assert_ne!(auth_transcript(&t), base, "the client id is not covered");

        let mut t = transcript();
        t.host = HostId::from_bytes([0xff; 32]);
        assert_ne!(auth_transcript(&t), base, "the host id is not covered");

        let mut t = transcript();
        t.client_label = "not-the-phone".into();
        assert_ne!(auth_transcript(&t), base, "the label a person reads is not covered");

        let mut t = transcript();
        t.version = 3;
        assert_ne!(auth_transcript(&t), base, "the version is not covered");
    }

    #[test]
    fn labels_cannot_be_slid_between_fields() {
        // Length-prefixed, not separated. Without the prefixes these two
        // transcripts would produce the same bytes, and a tamperer could move
        // text from one label into the other.
        let mut a = transcript();
        a.host_label = "ab".into();
        a.client_label = "c".into();
        let mut b = transcript();
        b.host_label = "a".into();
        b.client_label = "bc".into();
        assert_ne!(auth_transcript(&a), auth_transcript(&b));
    }

    #[test]
    fn the_code_changes_when_either_nonce_does() {
        // Its whole job: a relay runs two handshakes with two nonce pairs, so
        // the codes on the two screens differ. A code derived from anything
        // stable would be identical on both and prove nothing.
        let base = pairing_code(&transcript());

        let mut t = transcript();
        t.host_nonce = Nonce::from_bytes([0x99; 32]);
        assert_ne!(pairing_code(&t), base);

        let mut t = transcript();
        t.client_nonce = Nonce::from_bytes([0x99; 32]);
        assert_ne!(pairing_code(&t), base);
    }

    #[test]
    fn the_code_is_six_digits_including_leading_zeros() {
        // Formatting, tested directly: finding a transcript that hashes small
        // is not worth the effort, and "428913" vs "28913" is the difference
        // between two codes matching and a person deciding they do not.
        assert_eq!(format!("{:0width$}", 913u32, width = 6), "000913");
        let code = pairing_code(&transcript());
        assert_eq!(code.len(), 6, "code was {code}");
        assert!(code.chars().all(|c| c.is_ascii_digit()), "code was {code}");
    }

    #[test]
    fn both_ends_of_one_handshake_compute_the_same_code() {
        let t = transcript();
        assert_eq!(pairing_code(&t), pairing_code(&t.clone()));
    }

    // --- the state machine ---

    fn host(identity: &Arc<HostIdentity>) -> HostHandshake {
        HostHandshake::new(Arc::clone(identity), "andy-mac", 2)
    }

    fn run_to_auth(
        h: &mut HostHandshake,
        client: &ClientIdentity,
        label: &str,
    ) -> Signature {
        let nonce = Nonce::from_bytes([0x55; 32]);
        let step = h.on_hello(2, client.client_id(), label, nonce);
        let HostStep::Challenge { .. } = step else { panic!("expected a challenge, got {step:?}") };
        let t = h.transcript().expect("challenged").clone();
        client.sign(Purpose::Auth, &auth_transcript(&t))
    }

    #[test]
    fn a_paired_client_is_welcomed() {
        let identity = Arc::new(HostIdentity::generate().expect("host key"));
        let client = ClientIdentity::generate().expect("client key");
        let trust = MemoryTrustStore::new();
        trust
            .insert(TrustRecord {
                client: client.client_id(),
                label: "phone".into(),
                paired_at: std::time::SystemTime::now(),
                last_seen: None,
            })
            .expect("insert");

        let mut h = host(&identity);
        let sig = run_to_auth(&mut h, &client, "phone");
        assert_eq!(h.on_auth(&sig, &trust), HostStep::Welcome);
        assert!(h.is_ready());
    }

    #[test]
    fn an_unknown_client_needs_approval_and_is_recorded_on_yes() {
        let identity = Arc::new(HostIdentity::generate().expect("host key"));
        let client = ClientIdentity::generate().expect("client key");
        let trust = MemoryTrustStore::new();

        let mut h = host(&identity);
        let sig = run_to_auth(&mut h, &client, "new-laptop");
        let step = h.on_auth(&sig, &trust);
        assert!(matches!(step, HostStep::NeedsApproval { .. }), "{step:?}");
        assert!(!h.is_ready(), "an unapproved client must not be served");

        assert_eq!(h.approved(&trust), HostStep::Welcome);
        assert!(
            trust.get(client.client_id()).expect("get").is_some(),
            "the pairing was not written before Welcome"
        );
    }

    /// A store that fails the test if it is consulted at all.
    struct NeverRead;
    impl TrustStore for NeverRead {
        fn get(&self, _: ClientId) -> Result<Option<TrustRecord>, MeshError> {
            panic!("the trust store was consulted before the signature was checked");
        }
        fn list(&self) -> Result<Vec<TrustRecord>, MeshError> {
            Ok(Vec::new())
        }
        fn insert(&self, _: TrustRecord) -> Result<(), MeshError> {
            panic!("a record was written for an unverified client");
        }
        fn touch(&self, _: ClientId, _: std::time::SystemTime) -> Result<(), MeshError> {
            Ok(())
        }
        fn remove(&self, _: ClientId) -> Result<bool, MeshError> {
            Ok(false)
        }
        fn describe(&self) -> String {
            "never-read".into()
        }
    }

    #[test]
    fn a_bad_signature_is_refused_before_the_trust_store_is_touched() {
        // Otherwise anyone who knows a ClientId can make this machine pop an
        // approval prompt without holding the key: prompt-spam, and a
        // social-engineering vector. Mechanical rather than aspirational.
        let identity = Arc::new(HostIdentity::generate().expect("host key"));
        let client = ClientIdentity::generate().expect("client key");
        let impostor = ClientIdentity::generate().expect("other key");

        let mut h = host(&identity);
        let nonce = Nonce::from_bytes([0x55; 32]);
        assert!(matches!(
            h.on_hello(2, client.client_id(), "phone", nonce),
            HostStep::Challenge { .. }
        ));
        let t = h.transcript().expect("challenged").clone();
        // Signed by someone else entirely.
        let sig = impostor.sign(Purpose::Auth, &auth_transcript(&t));

        assert_eq!(h.on_auth(&sig, &NeverRead), HostStep::Refused(Refusal::Signature));
    }

    #[test]
    fn a_host_signature_is_not_accepted_as_the_clients() {
        // `Role` is inside the preimage, so bouncing the host's own proof back
        // fails by construction. This asserts the property rather than the
        // mechanism.
        let identity = Arc::new(HostIdentity::generate().expect("host key"));
        let client = ClientIdentity::generate().expect("client key");

        let mut h = host(&identity);
        let nonce = Nonce::from_bytes([0x55; 32]);
        let step = h.on_hello(2, client.client_id(), "phone", nonce);
        let HostStep::Challenge { signature, .. } = step else { panic!("expected a challenge") };

        assert_eq!(h.on_auth(&signature, &NeverRead), HostStep::Refused(Refusal::Signature));
    }

    #[test]
    fn a_replayed_proof_does_not_work_on_a_second_connection() {
        // The reason there is no nonce cache anywhere: the host's nonce is
        // per-connection state, so the connection *is* the replay window.
        let identity = Arc::new(HostIdentity::generate().expect("host key"));
        let client = ClientIdentity::generate().expect("client key");
        let trust = MemoryTrustStore::new();
        trust
            .insert(TrustRecord {
                client: client.client_id(),
                label: "phone".into(),
                paired_at: std::time::SystemTime::now(),
                last_seen: None,
            })
            .expect("insert");

        let mut first = host(&identity);
        let sig = run_to_auth(&mut first, &client, "phone");
        assert_eq!(first.on_auth(&sig, &trust), HostStep::Welcome);

        // The same proof, on a fresh connection with a fresh host nonce.
        let mut second = host(&identity);
        assert!(matches!(
            second.on_hello(2, client.client_id(), "phone", Nonce::from_bytes([0x55; 32])),
            HostStep::Challenge { .. }
        ));
        assert_eq!(
            second.on_auth(&sig, &trust),
            HostStep::Refused(Refusal::Signature),
            "a captured proof replayed onto another connection"
        );
    }

    #[test]
    fn a_version_mismatch_is_refused_before_anything_else() {
        let identity = Arc::new(HostIdentity::generate().expect("host key"));
        let mut h = host(&identity);
        let step = h.on_hello(99, ClientId::from_bytes([1; 32]), "future", Nonce::from_bytes([1; 32]));
        assert_eq!(step, HostStep::Refused(Refusal::Version));
    }

    #[test]
    fn a_missing_nonce_is_refused_by_name() {
        // The `#[serde(default)]` on the wire is all zeroes. Accepting it would
        // be accepting a handshake with no freshness at all.
        let identity = Arc::new(HostIdentity::generate().expect("host key"));
        let mut h = host(&identity);
        let step = h.on_hello(2, ClientId::from_bytes([1; 32]), "old", Nonce::from_bytes([0; 32]));
        assert_eq!(step, HostStep::Refused(Refusal::Signature));
    }

    #[test]
    fn auth_before_hello_is_refused() {
        let identity = Arc::new(HostIdentity::generate().expect("host key"));
        let mut h = host(&identity);
        let sig = Signature::from_bytes([0; 64]);
        assert_eq!(h.on_auth(&sig, &NeverRead), HostStep::Refused(Refusal::Signature));
    }

    #[test]
    fn a_second_hello_on_one_connection_is_refused() {
        let identity = Arc::new(HostIdentity::generate().expect("host key"));
        let client = ClientIdentity::generate().expect("client key");
        let mut h = host(&identity);
        assert!(matches!(
            h.on_hello(2, client.client_id(), "phone", Nonce::from_bytes([5; 32])),
            HostStep::Challenge { .. }
        ));
        let step = h.on_hello(2, client.client_id(), "phone", Nonce::from_bytes([6; 32]));
        assert_eq!(step, HostStep::Refused(Refusal::Signature));
    }

    #[test]
    fn a_client_can_tell_it_reached_the_wrong_machine() {
        // What the host signing first buys: a client dialling an address from
        // an advertisement can hang up before revealing anything.
        let identity = Arc::new(HostIdentity::generate().expect("host key"));
        let client = ClientIdentity::generate().expect("client key");
        let mut h = host(&identity);
        let step = h.on_hello(2, client.client_id(), "phone", Nonce::from_bytes([5; 32]));
        let HostStep::Challenge { signature, .. } = step else { panic!("expected a challenge") };
        let t = h.transcript().expect("challenged").clone();

        assert!(verify_challenge(Some(identity.host_id()), &t, &signature).is_ok());
        assert!(
            verify_challenge(Some(HostId::from_bytes([0xee; 32])), &t, &signature).is_err(),
            "a client accepted a host it did not dial"
        );
    }

    // --- the queue ---

    fn request(client: u8) -> PairingRequest {
        PairingRequest {
            client: ClientId::from_bytes([client; 32]),
            label: "phone".into(),
            code: "123456".into(),
            remote: "192.168.1.42:51314".into(),
            requested_at: Instant::now(),
        }
    }

    #[test]
    fn a_decision_reaches_the_waiting_connection() {
        let q = PairingQueue::new();
        let (tx, rx) = std::sync::mpsc::channel();
        let _handle = q.submit(request(1), Box::new(move |d| tx.send(d).expect("send")));

        assert_eq!(q.pending().len(), 1);
        assert_eq!(q.resolve(ClientId::from_bytes([1; 32]), Decision::Approve), 1);
        assert_eq!(rx.recv().expect("decision"), Decision::Approve);
        assert!(q.pending().is_empty());
    }

    #[test]
    fn a_client_that_hangs_up_takes_its_prompt_with_it() {
        // A prompt for a device that already left is exactly what teaches
        // someone to dismiss prompts without reading them.
        let q = PairingQueue::new();
        let (tx, rx) = std::sync::mpsc::channel();
        let handle = q.submit(request(2), Box::new(move |d| tx.send(d).expect("send")));

        assert_eq!(q.pending().len(), 1);
        drop(handle);
        assert!(q.pending().is_empty(), "the request outlived its connection");
        assert_eq!(rx.recv().expect("decision"), Decision::Deny);
    }

    #[test]
    fn waiting_too_long_is_a_denial() {
        let q = PairingQueue::new();
        let (tx, rx) = std::sync::mpsc::channel();
        let mut req = request(3);
        req.requested_at = Instant::now() - (APPROVAL_TIMEOUT + Duration::from_secs(1));
        let _handle = q.submit(req, Box::new(move |d| tx.send(d).expect("send")));

        assert_eq!(q.expire(Instant::now()), 1);
        assert_eq!(rx.recv().expect("decision"), Decision::Deny);
    }

    #[test]
    fn approving_a_device_answers_every_request_from_it() {
        // A device that retried while someone was reading the prompt must not
        // be left waiting behind its own duplicate.
        let q = PairingQueue::new();
        let (tx1, rx1) = std::sync::mpsc::channel();
        let (tx2, rx2) = std::sync::mpsc::channel();
        let _a = q.submit(request(4), Box::new(move |d| tx1.send(d).expect("send")));
        let _b = q.submit(request(4), Box::new(move |d| tx2.send(d).expect("send")));

        assert_eq!(q.resolve(ClientId::from_bytes([4; 32]), Decision::Approve), 2);
        assert_eq!(rx1.recv().expect("first"), Decision::Approve);
        assert_eq!(rx2.recv().expect("second"), Decision::Approve);
    }
}
