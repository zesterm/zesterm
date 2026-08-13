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
//! C -> H  Hello     { version, client, label, nonce_c, dh_c }        plaintext
//! H -> C  Challenge { version, host, label, nonce_h, dh_h, sig_h }   plaintext
//! C -> H  Auth      { sig_c }                                        SEALED
//! H -> C  Welcome | AuthPending | AuthFailed                         SEALED
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
//! # The channel comes out of the same transcript
//!
//! Each side puts an ephemeral X25519 public key into the transcript, so the
//! Ed25519 signatures that were already being exchanged certify those keys for
//! free — no certificate type, no static DH key to store or rotate, and
//! forward secrecy because the ephemerals die with the connection. The shared
//! secret is keyed by the *hash of the signed transcript*, so two sides that
//! disagree about any byte of it — a label, a version, a nonce — derive
//! different keys and simply cannot talk. See [`crate::secure`].
//!
//! This is what closes the old hole. Before it, an attacker who could relay the
//! handshake verbatim held two connections and read and rewrote everything
//! after it; the matching code was the only defence, and only if a person
//! compared it. Now such an attacker must forge a signature over its own DH
//! key. It is also what lets a relay carry the bytes without being trusted with
//! them — see ADR-008.
//!
//! **The seal switch is positional, not by message type.** The `Challenge` is
//! the switch: everything up to and including it is plaintext, and the client's
//! `Auth` is the first sealed frame in each direction. A refusal *before* a
//! Challenge is therefore plaintext, and that set is exactly "the host never
//! sent a Challenge" — a rule two independent implementations can agree on
//! without a table of message types to disagree about.
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

use crate::identity::{verify_client, verify_host, HostIdentity, Nonce, Purpose, Role, Signature};
use crate::secure::{DhPublic, EphemeralDh, SecureChannel};
use crate::MeshError;

/// Domain separator for the authentication transcript.
///
/// **The `v2` counts layouts, not protocol versions.** This is the second shape
/// the signed bytes have ever had; the protocol carried inside them is at 3.
/// They will keep diverging, because a protocol bump does not always move these
/// bytes and moving these bytes does not always need a protocol bump. Anyone
/// deriving one from the other -- which the TypeScript twin is one line away
/// from doing -- gets signatures that will not verify and no field named in the
/// error. A test pins the literal.
const AUTH_DOMAIN: &[u8] = b"zesterm-auth-v2";

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
    /// The host's ephemeral X25519 public key.
    ///
    /// Inside the transcript so the host's *existing* signature certifies it.
    /// That is the whole trick: no certificate type, no static key to store,
    /// and a relay that substituted a DH key would have to forge a signature
    /// to make the two sides agree on a channel.
    pub host_dh: [u8; 32],
    /// The client's, for the same reason.
    pub client_dh: [u8; 32],
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
/// as a deliberate diff. It changed once, at `v2`, to carry the ephemeral DH
/// keys — which is what made the data plane encryptable without a second
/// signature scheme.
///
/// Fallible since `v2`. It used to clamp an oversize label to `u16::MAX` and
/// truncate: two labels sharing their first 65535 bytes then produced identical
/// signed bytes, so a signature over one was a valid signature over the other.
/// A label is attacker-influenced and is the human's entire decision input on
/// the approval prompt, so "nobody would send one that long" was not an
/// argument available. Worse, the truncation was an implicit rule the
/// TypeScript verifier had to reproduce exactly, and a disagreement there
/// surfaces as a signature that will not verify with nothing naming the length.
/// (#43.)
pub fn auth_transcript(t: &Transcript) -> Result<Vec<u8>, MeshError> {
    let host_label = t.host_label.as_bytes();
    let client_label = t.client_label.as_bytes();
    let host_len = len_prefix(host_label, "host label")?;
    let client_len = len_prefix(client_label, "client label")?;

    let mut out = Vec::with_capacity(
        AUTH_DOMAIN.len() + 2 + 32 + 32 + 32 + 32 + 32 + 32 + 2 + host_label.len() + 2 + client_label.len(),
    );
    out.extend_from_slice(AUTH_DOMAIN);
    out.extend_from_slice(&t.version.to_be_bytes());
    out.extend_from_slice(&t.host.0);
    out.extend_from_slice(&t.client.0);
    out.extend_from_slice(t.host_nonce.as_bytes());
    out.extend_from_slice(t.client_nonce.as_bytes());
    // The DH keys sit after the nonces and before the labels: fixed-width
    // fields stay together, ahead of anything carrying its own length.
    out.extend_from_slice(&t.host_dh);
    out.extend_from_slice(&t.client_dh);
    out.extend_from_slice(&host_len.to_be_bytes());
    out.extend_from_slice(host_label);
    out.extend_from_slice(&client_len.to_be_bytes());
    out.extend_from_slice(client_label);
    Ok(out)
}

fn len_prefix(bytes: &[u8], what: &str) -> Result<u16, MeshError> {
    u16::try_from(bytes.len()).map_err(|_| {
        MeshError::BadSignature(format!("{what} is too long to sign unambiguously"))
    })
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
/// Fallible for the same reason `auth_transcript` is: a transcript whose labels
/// cannot be encoded unambiguously has no code either, and inventing one would
/// put a number on two screens that no signature stands behind.
pub fn pairing_code(t: &Transcript) -> Result<String, MeshError> {
    let mut hasher = Sha256::new();
    hasher.update(CODE_DOMAIN);
    hasher.update(auth_transcript(t)?);
    let digest = hasher.finalize();
    let n = u32::from_be_bytes([digest[0], digest[1], digest[2], digest[3]]);
    let modulus = 10u32.pow(PAIRING_CODE_DIGITS);
    Ok(format!("{:0width$}", n % modulus, width = PAIRING_CODE_DIGITS as usize))
}

/// What the host should do next.
#[derive(Debug, Clone, PartialEq, Eq)]
#[must_use]
pub enum HostStep {
    /// Answer with this nonce and proof.
    Challenge { nonce: Nonce, dh: DhPublic, signature: Signature },
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
    /// This connection's ephemeral secret, held from `on_hello` until the
    /// channel is taken.
    ///
    /// Beside the state rather than inside it: `State`'s transcript accessor is
    /// shared by three variants, and pairing a secret into one of them would
    /// make every reader of the other two carry it too. Taken by `channel()`,
    /// which is what enforces one connection, one key.
    dh: Option<EphemeralDh>,
}

enum State {
    /// Nothing yet. Only `Hello` is accepted.
    Greeting,
    /// Challenged, waiting for the proof.
    Challenged(Box<Transcript>),
    /// Proved, waiting for a person.
    Pending(Box<Transcript>),
    /// Served. Keeps the transcript so the caller can say who this is.
    Ready(Box<Transcript>),
    /// Finished, badly.
    Refused,
}

impl HostHandshake {
    pub fn new(identity: std::sync::Arc<HostIdentity>, label: impl Into<String>, version: u16) -> Self {
        Self { identity, label: label.into(), version, state: State::Greeting, dh: None }
    }

    /// Whether this connection may be served.
    #[must_use]
    pub fn is_ready(&self) -> bool {
        matches!(self.state, State::Ready(_))
    }

    /// The transcript under negotiation, for a prompt.
    ///
    /// **Also available once the connection is `Ready`**, and that matters: the
    /// caller logs which device authenticated and stamps `last_seen` *after*
    /// `on_auth` returns `Welcome`, by which point the state has already
    /// advanced. When `Ready` was excluded here, that whole branch silently
    /// never ran -- no audit line, and `last_seen` stayed `None` on every
    /// paired device forever.
    #[must_use]
    pub fn transcript(&self) -> Option<&Transcript> {
        match &self.state {
            State::Challenged(t) | State::Pending(t) | State::Ready(t) => Some(t),
            State::Greeting | State::Refused => None,
        }
    }

    /// Take this connection's secure channel, once.
    ///
    /// Called immediately after the `Challenge` goes out, because the
    /// *positional* seal switch says the client's `Auth` is already sealed —
    /// the host cannot check a signature it cannot decrypt. That is not the
    /// same as serving an unauthenticated peer: opening the `Auth` proves only
    /// that whoever completed the DH also sent it, and `on_auth` still decides
    /// whether that key may have a shell.
    ///
    /// Returns `None` if there is no handshake to derive from, or if the
    /// channel has already been taken — one connection, one key, and the
    /// second caller getting a fresh channel with reset counters would silently
    /// reuse nonces.
    pub fn channel(&mut self) -> Option<Result<SecureChannel, MeshError>> {
        let dh = self.dh.take()?;
        let transcript = self.transcript()?;
        let message = match auth_transcript(transcript) {
            Ok(m) => m,
            Err(e) => return Some(Err(e)),
        };
        let theirs = DhPublic(transcript.client_dh);
        Some(dh.agree(theirs, &Sha256::digest(&message), Role::Host))
    }

    /// Answer a `Hello`.
    pub fn on_hello(
        &mut self,
        version: u16,
        client: ClientId,
        client_label: &str,
        client_nonce: Nonce,
        client_dh: DhPublic,
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

        // Generated here so it is bound into the very transcript the host is
        // about to sign: a relay that substituted a key would have to forge
        // that signature to make the two sides agree on a channel.
        let Ok(host_dh) = EphemeralDh::generate() else {
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
            host_dh: host_dh.public().0,
            client_dh: client_dh.0,
        };
        let Ok(message) = auth_transcript(&transcript) else {
            self.state = State::Refused;
            return HostStep::Refused(Refusal::Signature);
        };
        let signature = self.identity.sign(Purpose::Auth, &message);
        let host_dh_public = host_dh.public();
        self.dh = Some(host_dh);
        self.state = State::Challenged(Box::new(transcript));
        HostStep::Challenge { nonce: host_nonce, dh: host_dh_public, signature }
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

        let Ok(message) = auth_transcript(&transcript) else {
            self.state = State::Refused;
            return HostStep::Refused(Refusal::Signature);
        };
        if verify_client(transcript.client, Purpose::Auth, &message, signature).is_err() {
            self.state = State::Refused;
            return HostStep::Refused(Refusal::Signature);
        }

        match trust.get(transcript.client) {
            Ok(Some(_)) => {
                self.state = State::Ready(transcript);
                HostStep::Welcome
            }
            Ok(None) => {
                let code = pairing_code(&transcript);
                self.state = State::Pending(transcript);
                // A transcript that cannot be encoded has no code to compare,
                // and showing a number no signature stands behind is worse
                // than refusing.
                let Ok(code) = code else {
                    self.state = State::Refused;
                    return HostStep::Refused(Refusal::Signature);
                };
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
        let transcript = transcript.clone();
        if let Err(e) = trust.insert(record) {
            tracing::error!(error = %e, "could not record the pairing; refusing");
            self.state = State::Refused;
            return HostStep::Refused(Refusal::UnknownClient);
        }
        self.state = State::Ready(transcript);
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
    /// Bumped whenever `pending` would read differently: submit, resolve,
    /// expire, cancel. What lets a watching connection answer "did anything
    /// change?" without diffing two listings — the same shape as
    /// `Registry.generation`, because the daemon's push loop is the same.
    generation: AtomicU64,
    /// Wakers for connections that asked to watch the queue
    /// (`Hello.watch_pairings`), keyed by a token so `Drop` can unregister.
    watchers: Mutex<std::collections::HashMap<u64, std::sync::Arc<dyn Fn() + Send + Sync>>>,
    next_watcher: AtomicU64,
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

    /// The queue changed; tell everyone who asked to hear it.
    ///
    /// Called *after* the change is visible in `pending`, so a woken watcher
    /// that lists immediately sees the new truth. Wakers are cloned out and
    /// run outside the lock for `submit`'s reason: a waker is arbitrary code.
    fn touch(&self) {
        self.generation.fetch_add(1, Ordering::Release);
        let wakers: Vec<_> = self.watchers.lock().values().cloned().collect();
        for waker in wakers {
            waker();
        }
    }

    /// Where the queue's contents currently stand; see `touch`.
    #[must_use]
    pub fn generation(&self) -> u64 {
        self.generation.load(Ordering::Acquire)
    }

    /// Register a waker to run whenever the queue changes.
    ///
    /// Beside `on_request` rather than replacing it: the stdin prompter wants
    /// "a request arrived", a pushing connection wants "the listing you
    /// announced may be stale" — which includes requests *leaving*, so a
    /// modal can close when someone answers at the terminal.
    pub fn watch(&self, waker: std::sync::Arc<dyn Fn() + Send + Sync>) -> u64 {
        let token = self.next_watcher.fetch_add(1, Ordering::Relaxed);
        self.watchers.lock().insert(token, waker);
        token
    }

    pub fn unwatch(&self, token: u64) {
        self.watchers.lock().remove(&token);
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
        self.touch();
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
        if n > 0 {
            self.touch();
        }
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
        if n > 0 {
            self.touch();
        }
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
            self.touch();
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
    verify_host(transcript.host, Purpose::Auth, &auth_transcript(transcript)?, signature)
}

/// A `Challenge` as the client received it, ready to be checked.
///
/// A struct rather than six parameters, and not only because clippy counts:
/// every field here comes out of a wire message with the same name, so
/// positional arguments are two ids, two 32-byte arrays and a `u16` in a row —
/// a swap the compiler cannot see and a signature failure is the only symptom.
pub struct Challenge {
    pub version: u16,
    pub host: HostId,
    pub label: String,
    pub nonce: Nonce,
    pub dh: DhPublic,
    pub signature: Signature,
}

/// The client half of the handshake, for one connection.
///
/// It exists because the client half used to be hand-rolled at every call site
/// — the app, `attach`, `pair`, the loopback path and three tests each built
/// their own `Transcript` and signed it. That was survivable while the only
/// thing at stake was a signature that either verified or did not; it stopped
/// being survivable when the same steps also derive the key everything
/// afterwards is encrypted under, because a call site that skipped one would
/// produce a connection that authenticates perfectly and then cannot be read.
///
/// The ephemeral secret is generated in `new` and consumed by `on_challenge`,
/// so the type enforces one connection, one key without anybody remembering to.
pub struct ClientHandshake {
    identity: std::sync::Arc<crate::identity::ClientIdentity>,
    label: String,
    nonce: Nonce,
    dh: Option<EphemeralDh>,
}

impl ClientHandshake {
    /// Generate this connection's nonce and ephemeral key.
    pub fn new(
        identity: std::sync::Arc<crate::identity::ClientIdentity>,
        label: impl Into<String>,
    ) -> Result<Self, MeshError> {
        Ok(Self {
            identity,
            label: label.into(),
            nonce: Nonce::random()?,
            dh: Some(EphemeralDh::generate()?),
        })
    }

    /// What goes in the `Hello`.
    #[must_use]
    pub fn nonce(&self) -> Nonce {
        self.nonce
    }

    /// What goes in the `Hello`, and is thereby bound into what the host signs.
    #[must_use]
    pub fn dh(&self) -> DhPublic {
        self.dh.as_ref().map_or(DhPublic([0u8; 32]), EphemeralDh::public)
    }

    /// Verify the host, sign the transcript, and derive the channel.
    ///
    /// All three, together, because the ordering is the security property:
    /// `verify_challenge` runs **before** the client signs anything or derives
    /// any key, so a client that dialled an address from an mDNS advertisement
    /// hangs up on the wrong machine having revealed nothing. Splitting these
    /// into three public steps would make that ordering a convention a call
    /// site could get wrong.
    ///
    /// Returns the proof to send and the channel to seal it with — the `Auth`
    /// is the first sealed frame, so both are needed at the same moment.
    pub fn on_challenge(
        &mut self,
        expect_host: Option<HostId>,
        c: &Challenge,
    ) -> Result<(Signature, Transcript, SecureChannel), MeshError> {
        let dh = self
            .dh
            .take()
            .ok_or_else(|| MeshError::BadSignature("a second challenge on one connection".into()))?;

        let transcript = Transcript {
            version: c.version,
            host: c.host,
            client: self.identity.client_id(),
            host_nonce: c.nonce,
            client_nonce: self.nonce,
            host_label: c.label.clone(),
            client_label: self.label.clone(),
            host_dh: c.dh.0,
            client_dh: dh.public().0,
        };

        verify_challenge(expect_host, &transcript, &c.signature)?;

        let message = auth_transcript(&transcript)?;
        let proof = self.identity.sign(Purpose::Auth, &message);
        let channel = dh.agree(c.dh, &Sha256::digest(&message), Role::Client)?;
        Ok((proof, transcript, channel))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::identity::ClientIdentity;
    use crate::trust::{MemoryTrustStore, TrustRecord, TrustStore};
    use std::sync::Arc;

    fn transcript() -> Transcript {
        Transcript {
            version: 3,
            host: HostId::from_bytes([0x11; 32]),
            client: ClientId::from_bytes([0x22; 32]),
            host_nonce: Nonce::from_bytes([0x33; 32]),
            client_nonce: Nonce::from_bytes([0x44; 32]),
            host_label: "andy-mac".into(),
            client_label: "andy-phone".into(),
            host_dh: [0x55; 32],
            client_dh: [0x66; 32],
        }
    }

    fn bytes_of(t: &Transcript) -> Vec<u8> {
        auth_transcript(t).expect("these labels encode")
    }

    fn code_of(t: &Transcript) -> String {
        pairing_code(t).expect("these labels encode")
    }

    #[test]
    fn the_transcript_layout_is_pinned() {
        // A silent change here unpairs every device in the field, and reports
        // it as "did not prove its identity" -- which sends whoever is
        // debugging it looking at the wrong thing entirely. It must only ever
        // change as a deliberate diff to this hex.
        let bytes = bytes_of(&transcript());
        let digest = Sha256::digest(&bytes);
        let hex: String = digest.iter().map(|b| format!("{b:02x}")).collect();
        assert_eq!(
            hex, "1e813e605865aaa8b8cb1438981c11cabeb24f3c2fa9f7a6f01977feea75441d",
            "the transcript layout changed"
        );
    }

    #[test]
    fn every_field_is_bound_into_the_signature() {
        // "Bound to the connection and to both ids" is four separate claims,
        // so each gets its own assertion rather than one that could pass for
        // the wrong reason.
        let base = bytes_of(&transcript());

        let mut t = transcript();
        t.host_nonce = Nonce::from_bytes([0xff; 32]);
        assert_ne!(bytes_of(&t), base, "the host nonce is not covered");

        let mut t = transcript();
        t.client_nonce = Nonce::from_bytes([0xff; 32]);
        assert_ne!(bytes_of(&t), base, "the client nonce is not covered");

        let mut t = transcript();
        t.client = ClientId::from_bytes([0xff; 32]);
        assert_ne!(bytes_of(&t), base, "the client id is not covered");

        let mut t = transcript();
        t.host = HostId::from_bytes([0xff; 32]);
        assert_ne!(bytes_of(&t), base, "the host id is not covered");

        let mut t = transcript();
        t.client_label = "not-the-phone".into();
        assert_ne!(bytes_of(&t), base, "the label a person reads is not covered");

        let mut t = transcript();
        t.version = 4;
        assert_ne!(bytes_of(&t), base, "the version is not covered");

        // The two that make the channel: if a DH key were outside the signed
        // bytes, a relay could substitute its own and hold two channels while
        // both signatures still verified -- the handshake would look perfect
        // and the encryption would be worth nothing.
        let mut t = transcript();
        t.host_dh = [0xff; 32];
        assert_ne!(bytes_of(&t), base, "the host's DH key is not covered");

        let mut t = transcript();
        t.client_dh = [0xff; 32];
        assert_ne!(bytes_of(&t), base, "the client's DH key is not covered");
    }

    #[test]
    fn the_signing_domain_does_not_track_the_protocol_version() {
        // Two version numbers, deliberately unequal, and the temptation to
        // derive one from the other is exactly why this is a test and not a
        // comment. A TypeScript peer that computed the domain from
        // PROTOCOL_VERSION would build "zesterm-auth-v3" and produce
        // signatures that fail to verify with nothing naming the cause.
        assert_eq!(AUTH_DOMAIN, b"zesterm-auth-v2", "the signing domain moved");
        assert_ne!(
            zest_proto::PROTOCOL_VERSION,
            2,
            "if these ever coincide this test stops proving they are independent"
        );
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
        assert_ne!(bytes_of(&a), bytes_of(&b));
    }

    #[test]
    fn the_code_changes_when_either_nonce_does() {
        // Its whole job: a relay runs two handshakes with two nonce pairs, so
        // the codes on the two screens differ. A code derived from anything
        // stable would be identical on both and prove nothing.
        let base = code_of(&transcript());

        let mut t = transcript();
        t.host_nonce = Nonce::from_bytes([0x99; 32]);
        assert_ne!(code_of(&t), base);

        let mut t = transcript();
        t.client_nonce = Nonce::from_bytes([0x99; 32]);
        assert_ne!(code_of(&t), base);
    }

    #[test]
    fn the_code_is_six_digits_including_leading_zeros() {
        // Formatting, tested directly: finding a transcript that hashes small
        // is not worth the effort, and "428913" vs "28913" is the difference
        // between two codes matching and a person deciding they do not.
        assert_eq!(format!("{:0width$}", 913u32, width = 6), "000913");
        let code = code_of(&transcript());
        assert_eq!(code.len(), 6, "code was {code}");
        assert!(code.chars().all(|c| c.is_ascii_digit()), "code was {code}");
    }

    #[test]
    fn both_ends_of_one_handshake_compute_the_same_code() {
        let t = transcript();
        assert_eq!(code_of(&t), code_of(&t.clone()));
    }

    // --- the state machine ---

    const V: u16 = 3;

    /// A well-formed DH key for tests that are not about the DH at all.
    ///
    /// A real one, generated, rather than a constant: `agree` rejects
    /// non-contributory keys, and a hand-picked array is exactly how a test
    /// starts failing for a reason unrelated to what it is checking.
    fn test_dh() -> DhPublic {
        EphemeralDh::generate().expect("randomness").public()
    }

    fn host(identity: &Arc<HostIdentity>) -> HostHandshake {
        HostHandshake::new(Arc::clone(identity), "andy-mac", V)
    }

    /// Drive both halves against each other, as a real connection does.
    ///
    /// Returns the client's proof and its channel, so a test can assert the two
    /// sides agree on a key as well as on a signature.
    fn run_to_auth(
        h: &mut HostHandshake,
        client: &Arc<ClientIdentity>,
        label: &str,
    ) -> (Signature, SecureChannel) {
        let mut c = ClientHandshake::new(Arc::clone(client), label).expect("client handshake");
        let step = h.on_hello(V, client.client_id(), label, c.nonce(), c.dh());
        let HostStep::Challenge { nonce, dh, signature } = step else {
            panic!("expected a challenge, got {step:?}")
        };
        let t = h.transcript().expect("challenged");
        let (proof, _, channel) = c
            .on_challenge(
                None,
                &Challenge {
                    version: V,
                    host: t.host,
                    label: t.host_label.clone(),
                    nonce,
                    dh,
                    signature,
                },
            )
            .expect("the host proved itself");
        (proof, channel)
    }

    #[test]
    fn a_paired_client_is_welcomed() {
        let identity = Arc::new(HostIdentity::generate().expect("host key"));
        let client = Arc::new(ClientIdentity::generate().expect("client key"));
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
        let (sig, _channel) = run_to_auth(&mut h, &client, "phone");
        assert_eq!(h.on_auth(&sig, &trust), HostStep::Welcome);
        assert!(h.is_ready());
    }

    #[test]
    fn one_handshake_gives_both_ends_a_channel_that_works() {
        // The claim the whole phase rests on, and the one nothing else checks:
        // two signatures verifying does not mean two key schedules agreed. If
        // the transcript hash, the role, or the field order differed by a byte
        // between the halves, every test above would still pass and the first
        // real frame would fail to open.
        let identity = Arc::new(HostIdentity::generate().expect("host key"));
        let client = Arc::new(ClientIdentity::generate().expect("client key"));
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
        let (sig, mut client_channel) = run_to_auth(&mut h, &client, "phone");
        let mut host_channel = h.channel().expect("challenged").expect("agreed");
        assert_eq!(h.on_auth(&sig, &trust), HostStep::Welcome);

        let sealed = client_channel.seal(b"resize 120x40").expect("seal");
        assert_ne!(&sealed[..], b"resize 120x40", "that is not encryption");
        assert_eq!(
            host_channel.open(&sealed).expect("the host must open what the client sealed"),
            b"resize 120x40"
        );

        let back = host_channel.seal(b"welcome").expect("seal");
        assert_eq!(client_channel.open(&back).expect("open"), b"welcome");
    }

    #[test]
    fn the_channel_is_taken_once_and_only_once() {
        // A second caller getting a fresh channel would reset the counters and
        // silently reuse nonces under the same key, which is the one mistake
        // ChaCha20-Poly1305 does not survive. Returning None is what makes that
        // a compile-time-shaped mistake rather than a silent one.
        let identity = Arc::new(HostIdentity::generate().expect("host key"));
        let client = Arc::new(ClientIdentity::generate().expect("client key"));
        let mut h = host(&identity);
        let _ = run_to_auth(&mut h, &client, "phone");
        assert!(h.channel().is_some(), "the first take must succeed");
        assert!(h.channel().is_none(), "a connection has exactly one channel");
    }

    #[test]
    fn a_relay_that_swaps_a_dh_key_cannot_make_the_signature_fit() {
        // The property that lets Cloudflare carry these bytes without being
        // trusted with them. A relay sits in the middle, forwards the Hello
        // with its own DH key, and now needs a host signature over a
        // transcript it just changed.
        let identity = Arc::new(HostIdentity::generate().expect("host key"));
        let client = Arc::new(ClientIdentity::generate().expect("client key"));
        let mut h = host(&identity);
        let mut c = ClientHandshake::new(Arc::clone(&client), "phone").expect("handshake");

        let step = h.on_hello(V, client.client_id(), "phone", c.nonce(), c.dh());
        let HostStep::Challenge { nonce, dh: _, signature } = step else { panic!("{step:?}") };
        let t = h.transcript().expect("challenged");

        // The relay substitutes its own key and forwards everything else
        // verbatim -- including the host's real signature.
        let relay_dh = test_dh();
        // `.err()`, not `expect_err` -- `SecureChannel` has no `Debug`, on
        // purpose, so a key never reaches a log line by way of a panic message.
        let err = c
            .on_challenge(
                None,
                &Challenge {
                    version: V,
                    host: t.host,
                    label: t.host_label.clone(),
                    nonce,
                    dh: relay_dh,
                    signature,
                },
            )
            .err()
            .expect("a substituted DH key must not verify");
        assert!(
            matches!(err, MeshError::BadSignature(_)),
            "the client must refuse before deriving any key, got {err:?}"
        );
    }

    #[test]
    fn a_served_connection_still_says_who_it_is() {
        // The caller logs the authenticated device and stamps `last_seen`
        // *after* on_auth returns Welcome, by which point the state has already
        // advanced. When `Ready` did not keep the transcript, that whole branch
        // silently never ran: no audit line, and `last_seen` stayed None on
        // every paired device forever -- so `--trusted` showed devices that
        // had apparently never connected.
        let identity = Arc::new(HostIdentity::generate().expect("host key"));
        let client = Arc::new(ClientIdentity::generate().expect("client key"));
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
        let (sig, _channel) = run_to_auth(&mut h, &client, "phone");
        assert_eq!(h.on_auth(&sig, &trust), HostStep::Welcome);

        let t = h.transcript().expect("a served connection must still name its client");
        assert_eq!(t.client, client.client_id());
        assert_eq!(t.client_label, "phone");
    }

    #[test]
    fn an_unknown_client_needs_approval_and_is_recorded_on_yes() {
        let identity = Arc::new(HostIdentity::generate().expect("host key"));
        let client = Arc::new(ClientIdentity::generate().expect("client key"));
        let trust = MemoryTrustStore::new();

        let mut h = host(&identity);
        let (sig, _channel) = run_to_auth(&mut h, &client, "new-laptop");
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
        let client = Arc::new(ClientIdentity::generate().expect("client key"));
        let impostor = ClientIdentity::generate().expect("other key");

        let mut h = host(&identity);
        let nonce = Nonce::from_bytes([0x55; 32]);
        assert!(matches!(
            h.on_hello(V, client.client_id(), "phone", nonce, test_dh()),
            HostStep::Challenge { .. }
        ));
        let t = h.transcript().expect("challenged").clone();
        // Signed by someone else entirely.
        let sig = impostor.sign(Purpose::Auth, &bytes_of(&t));

        assert_eq!(h.on_auth(&sig, &NeverRead), HostStep::Refused(Refusal::Signature));
    }

    #[test]
    fn a_host_signature_is_not_accepted_as_the_clients() {
        // `Role` is inside the preimage, so bouncing the host's own proof back
        // fails by construction. This asserts the property rather than the
        // mechanism.
        let identity = Arc::new(HostIdentity::generate().expect("host key"));
        let client = Arc::new(ClientIdentity::generate().expect("client key"));

        let mut h = host(&identity);
        let nonce = Nonce::from_bytes([0x55; 32]);
        let step = h.on_hello(V, client.client_id(), "phone", nonce, test_dh());
        let HostStep::Challenge { signature, .. } = step else { panic!("expected a challenge") };

        assert_eq!(h.on_auth(&signature, &NeverRead), HostStep::Refused(Refusal::Signature));
    }

    #[test]
    fn a_replayed_proof_does_not_work_on_a_second_connection() {
        // The reason there is no nonce cache anywhere: the host's nonce is
        // per-connection state, so the connection *is* the replay window.
        let identity = Arc::new(HostIdentity::generate().expect("host key"));
        let client = Arc::new(ClientIdentity::generate().expect("client key"));
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
        let (sig, _channel) = run_to_auth(&mut first, &client, "phone");
        assert_eq!(first.on_auth(&sig, &trust), HostStep::Welcome);

        // The same proof, on a fresh connection with a fresh host nonce.
        let mut second = host(&identity);
        assert!(matches!(
            second.on_hello(V, client.client_id(), "phone", Nonce::from_bytes([0x55; 32]), test_dh()),
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
        let step = h.on_hello(99, ClientId::from_bytes([1; 32]), "future", Nonce::from_bytes([1; 32]), test_dh());
        assert_eq!(step, HostStep::Refused(Refusal::Version));
    }

    #[test]
    fn a_missing_nonce_is_refused_by_name() {
        // The `#[serde(default)]` on the wire is all zeroes. Accepting it would
        // be accepting a handshake with no freshness at all.
        let identity = Arc::new(HostIdentity::generate().expect("host key"));
        let mut h = host(&identity);
        let step = h.on_hello(V, ClientId::from_bytes([1; 32]), "old", Nonce::from_bytes([0; 32]), test_dh());
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
        let client = Arc::new(ClientIdentity::generate().expect("client key"));
        let mut h = host(&identity);
        assert!(matches!(
            h.on_hello(V, client.client_id(), "phone", Nonce::from_bytes([5; 32]), test_dh()),
            HostStep::Challenge { .. }
        ));
        let step = h.on_hello(V, client.client_id(), "phone", Nonce::from_bytes([6; 32]), test_dh());
        assert_eq!(step, HostStep::Refused(Refusal::Signature));
    }

    #[test]
    fn a_client_can_tell_it_reached_the_wrong_machine() {
        // What the host signing first buys: a client dialling an address from
        // an advertisement can hang up before revealing anything.
        let identity = Arc::new(HostIdentity::generate().expect("host key"));
        let client = Arc::new(ClientIdentity::generate().expect("client key"));
        let mut h = host(&identity);
        let step = h.on_hello(V, client.client_id(), "phone", Nonce::from_bytes([5; 32]), test_dh());
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
