//! Who this host will serve.
//!
//! # Two authorizations, not one
//!
//! A connection is authorized either by the *operating system* or by a *key*,
//! and which one applies is a property of the transport rather than a setting:
//!
//! | | loopback socket / named pipe | LAN (TCP) |
//! |---|---|---|
//! | who can connect at all | OS: `0600` mode, or the pipe's DACL | anyone who can route a packet |
//! | version check | yes | yes |
//! | fresh nonces, mutual signature | yes | yes |
//! | trust-store lookup | **no** | **yes** |
//! | approval prompt | never | on an unknown client |
//! | accepts a pairing decision | **unless it said it is an agent** | **never** |
//!
//! Loopback still runs the *proof* — the wire is uniform, which matters
//! because ADR-007 makes the desktop app a client of its own daemon — but it
//! skips the trust store. A process that can reach the loopback socket runs as
//! the same user, and can already ptrace the daemon, read the key it just used
//! and replace the binary. A trust check there is theatre. What it buys is
//! real: the app can use an **ephemeral** identity, which keeps the OS keychain
//! and its prompt off the startup path entirely.
//!
//! # The gate is a type, not a flag
//!
//! [`Auth::Transport`] is constructed by the loopback listener and nowhere
//! else. The LAN listener takes an [`Authenticator`] as a non-optional
//! argument and has no code path that produces `Transport`, so the roadmap's
//! one live sequencing rule — *`listen_lan` must not be turned on until pairing
//! exists* — is enforced by the compiler rather than by discipline.

use std::sync::Arc;
use std::time::SystemTime;

use zest_mesh::identity::HostIdentity;
use zest_mesh::pairing::{Decision, PairingQueue, PairingRequest, Refusal};
use zest_mesh::trust::{TrustRecord, TrustStore};
use zest_proto::{AuthFailure, ClientId};

/// Everything needed to decide whether to serve a connection.
pub struct Authenticator {
    identity: Arc<HostIdentity>,
    trust: Arc<dyn TrustStore>,
    queue: Arc<PairingQueue>,
    label: String,
}

impl Authenticator {
    pub fn new(
        identity: Arc<HostIdentity>,
        trust: Arc<dyn TrustStore>,
        queue: Arc<PairingQueue>,
        label: impl Into<String>,
    ) -> Self {
        Self { identity, trust, queue, label: label.into() }
    }

    #[must_use]
    pub fn identity(&self) -> &Arc<HostIdentity> {
        &self.identity
    }

    #[must_use]
    pub fn trust(&self) -> &Arc<dyn TrustStore> {
        &self.trust
    }

    #[must_use]
    pub fn queue(&self) -> &Arc<PairingQueue> {
        &self.queue
    }

    #[must_use]
    pub fn label(&self) -> &str {
        &self.label
    }

    /// Trust a client without anyone being present.
    ///
    /// For `--trust`, and for a daemon under launchd or systemd where there is
    /// no stdin to prompt on.
    pub fn trust_now(&self, client: ClientId, label: &str) -> Result<(), zest_mesh::MeshError> {
        self.trust.insert(TrustRecord {
            client,
            label: label.to_string(),
            paired_at: SystemTime::now(),
            last_seen: None,
        })
    }

    /// Answer a waiting request.
    pub fn decide(&self, client: ClientId, decision: Decision) -> usize {
        self.queue.resolve(client, decision)
    }

    /// Queue a request and be told the answer later.
    pub fn ask(
        &self,
        request: PairingRequest,
        notify: Box<dyn FnOnce(Decision) + Send>,
    ) -> zest_mesh::pairing::PendingHandle {
        self.queue.submit(request, notify)
    }
}

/// How a connection proves it may be served.
///
/// Deliberately not a bool on `DaemonConfig`. A configuration value can be set
/// wrongly; a variant that only one listener can construct cannot.
#[derive(Clone)]
pub enum Auth {
    /// Authorized by the operating system. **Loopback only.**
    ///
    /// The handshake still runs — the wire is uniform — but the trust store is
    /// not consulted, and a client may use a throwaway identity.
    Transport(Arc<Authenticator>),
    /// Must prove a key *and* be in the trust store.
    Proof(Arc<Authenticator>),
}

impl Auth {
    #[must_use]
    pub fn authenticator(&self) -> &Arc<Authenticator> {
        match self {
            Self::Transport(a) | Self::Proof(a) => a,
        }
    }

    /// Whether the trust store decides, or the socket already did.
    #[must_use]
    pub const fn checks_trust(&self) -> bool {
        matches!(self, Self::Proof(_))
    }

    /// Whether this connection may approve *other* devices.
    ///
    /// Loopback only, always. Reaching the loopback socket is a demonstration
    /// that you are logged in at this machine, which is exactly the authority
    /// enrolling a device requires. Accepting it over the LAN would let one
    /// paired device enrol others.
    ///
    /// **This answers the transport half only, and is no longer the whole
    /// question.** A client may also decline the authority itself
    /// (`Hello.agent`), which is a claim the *peer* makes rather than a fact
    /// this type establishes — putting it here would sit a self-report inside
    /// the enum whose entire value is that it cannot be self-reported. The two
    /// are combined in `Connection::device_authority`, which is what every
    /// call site asks.
    #[must_use]
    pub const fn may_approve_devices(&self) -> bool {
        matches!(self, Self::Transport(_))
    }
}

/// Map a refusal onto the wire's vocabulary.
///
/// A client branches on this: wait quietly, stop retrying, or back off. That
/// is why it is an enum and not a message string.
#[must_use]
pub const fn failure_for(refusal: Refusal) -> AuthFailure {
    match refusal {
        Refusal::Version => AuthFailure::Version,
        Refusal::Signature => AuthFailure::Signature,
        Refusal::UnknownClient => AuthFailure::UnknownClient,
        Refusal::Denied => AuthFailure::Denied,
        Refusal::TimedOut => AuthFailure::TimedOut,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use zest_mesh::trust::MemoryTrustStore;

    fn authenticator() -> Arc<Authenticator> {
        Arc::new(Authenticator::new(
            Arc::new(HostIdentity::generate().expect("host key")),
            Arc::new(MemoryTrustStore::new()),
            PairingQueue::new(),
            "test-host",
        ))
    }

    #[test]
    fn only_a_loopback_connection_checks_no_trust_store() {
        let a = authenticator();
        assert!(!Auth::Transport(Arc::clone(&a)).checks_trust());
        assert!(Auth::Proof(a).checks_trust());
    }

    #[test]
    fn only_a_loopback_connection_may_enrol_devices() {
        // Otherwise a paired device could enrol others, and approving a device
        // is the authority of whoever is logged in at the machine.
        let a = authenticator();
        assert!(Auth::Transport(Arc::clone(&a)).may_approve_devices());
        assert!(!Auth::Proof(a).may_approve_devices());
    }

    #[test]
    fn every_refusal_has_a_wire_name_a_client_can_branch_on() {
        // Reusing the human-facing Error message would mean clients matching on
        // strings, which is how one breaks when someone improves the wording.
        assert_eq!(failure_for(Refusal::Denied), AuthFailure::Denied);
        assert_eq!(failure_for(Refusal::Version), AuthFailure::Version);
        assert_eq!(failure_for(Refusal::Signature), AuthFailure::Signature);
        assert_eq!(failure_for(Refusal::UnknownClient), AuthFailure::UnknownClient);
        assert_eq!(failure_for(Refusal::TimedOut), AuthFailure::TimedOut);

        // And the ones a client must not retry stay non-retryable.
        assert!(!failure_for(Refusal::Denied).is_retryable());
        assert!(failure_for(Refusal::TimedOut).is_retryable());
    }

    #[test]
    fn trusting_a_client_directly_records_it() {
        // What --trust does, and what a daemon with no stdin depends on.
        let a = authenticator();
        let client = ClientId::from_bytes([0x5a; 32]);
        a.trust_now(client, "headless").expect("trust");
        assert!(a.trust().get(client).expect("get").is_some());
    }
}
