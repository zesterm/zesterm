//! Answering a client.
//!
//! Deliberately written against `Read + Write` rather than against a socket, so
//! the whole protocol loop can be driven from a byte buffer in a test. A message
//! handler that can only be exercised through a real connection is one whose
//! error paths are never exercised at all.

use std::collections::HashMap;
use std::io::{Read, Write};
use std::sync::{Arc, Mutex};

use zest_proto::{
    frame, ClientId, ClientMessage, FrameReader, HostMessage, HostId, SessionAddr, SessionId,
    SessionInfo, Seq, PROTOCOL_VERSION,
};
use zest_pty::{CommandSpec, PtySize};

use crate::session::{Session, Update};
use crate::{DaemonConfig, DaemonError};

/// Whether a connection may be served.
///
/// Three states, not an `Option<Handshake>`. With an option, "no handshake in
/// progress" is ambiguous between *served* and *refused*, and clearing it on a
/// refusal is a one-character way to serve a client that just failed to prove
/// itself. That is not hypothetical: it is what the first version of this did,
/// and a test caught it answering `ListSessions` after a bad signature.
enum Gate {
    /// Mid-handshake. Only `Hello` and `Auth` are accepted.
    Handshaking(Box<zest_mesh::pairing::HostHandshake>),
    /// Proved. Everything is accepted.
    Served,
    /// Failed. **Nothing** is accepted, ever again on this connection.
    Refused,
}

/// A trust store that trusts everyone, for the loopback path.
///
/// Not a bypass in the handshake -- the proof still runs, and the wire is
/// identical -- but on loopback the *socket* is the authorization, so the
/// question the trust store answers has already been answered. Making it a
/// store rather than an `if` inside the handshake keeps the state machine with
/// one shape and no security-relevant branch.
struct AlwaysTrusted;

static ALWAYS_TRUSTED: AlwaysTrusted = AlwaysTrusted;

impl zest_mesh::trust::TrustStore for AlwaysTrusted {
    fn get(
        &self,
        client: zest_proto::ClientId,
    ) -> Result<Option<zest_mesh::trust::TrustRecord>, zest_mesh::MeshError> {
        Ok(Some(zest_mesh::trust::TrustRecord {
            client,
            label: "local".into(),
            paired_at: std::time::SystemTime::UNIX_EPOCH,
            last_seen: None,
        }))
    }
    fn list(&self) -> Result<Vec<zest_mesh::trust::TrustRecord>, zest_mesh::MeshError> {
        Ok(Vec::new())
    }
    fn insert(&self, _: zest_mesh::trust::TrustRecord) -> Result<(), zest_mesh::MeshError> {
        Ok(())
    }
    fn touch(
        &self,
        _: zest_proto::ClientId,
        _: std::time::SystemTime,
    ) -> Result<(), zest_mesh::MeshError> {
        Ok(())
    }
    fn remove(&self, _: zest_proto::ClientId) -> Result<bool, zest_mesh::MeshError> {
        Ok(false)
    }
    fn describe(&self) -> String {
        "the loopback socket (permissions are the authorization)".into()
    }
}

/// Human-facing text for a refusal.
///
/// Deliberately separate from the `AuthFailure` a client branches on: this can
/// be reworded freely, and nothing depends on it.
const fn message_for(reason: zest_proto::AuthFailure) -> &'static str {
    match reason {
        zest_proto::AuthFailure::Signature => "the connection did not prove its identity",
        zest_proto::AuthFailure::UnknownClient => "this device is not paired with this host",
        zest_proto::AuthFailure::Denied => "the request was declined",
        zest_proto::AuthFailure::TimedOut => "nobody answered the pairing request",
        zest_proto::AuthFailure::Revoked => "this device was removed",
        zest_proto::AuthFailure::RateLimited => "too many attempts; try again shortly",
        zest_proto::AuthFailure::Version => "protocol versions are not compatible",
        _ => "refused",
    }
}

/// Most rows one `RequestScrollback` may fetch.
///
/// A client scrolling up wants a screenful at a time, not the whole history in
/// one frame. Without a bound, a request for ten thousand rows produces a
/// payload the 8 MiB frame limit then refuses — which is a much worse way to
/// discover the limit than being handed the first page.
const SCROLLBACK_PAGE: usize = 500;

/// Every session this machine owns.
///
/// Shared between connections, because the point of the daemon is that a session
/// is not owned by whoever happens to be looking at it.
#[derive(Default)]
pub struct Registry {
    sessions: Mutex<HashMap<u64, Arc<Session>>>,
    next_id: Mutex<u64>,
}

impl Registry {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Start a session and keep it.
    pub fn create(
        &self,
        cmd: &CommandSpec,
        size: PtySize,
        scrollback: usize,
    ) -> Result<Arc<Session>, DaemonError> {
        let id = {
            let mut next = self.next_id.lock().expect("id lock");
            *next += 1;
            SessionId(*next)
        };
        let session = Arc::new(Session::spawn(id, cmd, size, scrollback, |_| {})?);
        self.sessions.lock().expect("registry lock").insert(id.0, Arc::clone(&session));
        Ok(session)
    }

    #[must_use]
    pub fn get(&self, id: SessionId) -> Option<Arc<Session>> {
        self.sessions.lock().expect("registry lock").get(&id.0).cloned()
    }

    /// Everything running, for a listing.
    #[must_use]
    pub fn list(&self, host: HostId) -> Vec<SessionInfo> {
        let sessions = self.sessions.lock().expect("registry lock");
        let mut out: Vec<SessionInfo> = sessions
            .values()
            .map(|s| {
                let (cols, rows) = s.size();
                SessionInfo {
                    addr: SessionAddr::new(host, s.id),
                    title: s.title(),
                    cwd: String::new(),
                    cols,
                    rows,
                    alt_screen: s.alt_screen(),
                    attached: s.attached(),
                }
            })
            .collect();
        // Sorted so a fleet listing does not reshuffle between polls, which
        // makes a list on a phone unusable.
        out.sort_by_key(|s| s.addr.session.0);
        out
    }

    /// Drop a session and end its child.
    pub fn close(&self, id: SessionId) {
        self.sessions.lock().expect("registry lock").remove(&id.0);
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.sessions.lock().expect("registry lock").len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// One client connection.
pub struct Connection {
    config: DaemonConfig,
    registry: Arc<Registry>,
    reader: FrameReader,
    /// Session id to this connection's subscriber handle.
    ///
    /// Per connection, not per session: two devices watching one shell each need
    /// their own position in it.
    attached: HashMap<u64, u64>,
    /// Where this connection stands. See [`Gate`].
    gate: Gate,
    /// Kept only so that dropping the connection cancels its approval prompt.
    /// A prompt for a device that already hung up is what teaches someone to
    /// dismiss prompts without reading them.
    pending: Option<zest_mesh::pairing::PendingHandle>,
    /// How this connection is authorized. See `auth::Auth`.
    auth: crate::auth::Auth,
    /// Where the peer is, for the approval prompt.
    remote: String,
    /// A decision that arrived while this connection was waiting.
    ///
    /// Written by the approval callback on whichever thread resolved it, read
    /// by the writer loop after it is woken.
    decided: Arc<Mutex<Option<zest_mesh::pairing::Decision>>>,
    /// Handed to each session on attach, so output wakes the writer.
    waker: Option<Arc<dyn Fn() + Send + Sync>>,
}

impl Connection {
    #[must_use]
    pub fn new(
        config: DaemonConfig,
        registry: Arc<Registry>,
        auth: crate::auth::Auth,
        remote: impl Into<String>,
    ) -> Self {
        let a = auth.authenticator();
        let handshake = zest_mesh::pairing::HostHandshake::new(
            Arc::clone(a.identity()),
            a.label().to_string(),
            PROTOCOL_VERSION,
        );
        Self {
            config,
            registry,
            reader: FrameReader::new(),
            attached: HashMap::new(),
            gate: Gate::Handshaking(Box::new(handshake)),
            pending: None,
            auth,
            remote: remote.into(),
            decided: Arc::new(Mutex::new(None)),
            waker: None,
        }
    }

    /// Whether the handshake has completed. Used by the LAN watchdog.
    #[must_use]
    pub fn is_ready(&self) -> bool {
        matches!(self.gate, Gate::Served)
    }

    fn handshake_mut(&mut self) -> Option<&mut zest_mesh::pairing::HostHandshake> {
        match &mut self.gate {
            Gate::Handshaking(h) => Some(h),
            _ => None,
        }
    }

    fn handshake(&self) -> Option<&zest_mesh::pairing::HostHandshake> {
        match &self.gate {
            Gate::Handshaking(h) => Some(h),
            _ => None,
        }
    }

    /// Set what an attached session calls when it has output.
    pub fn set_waker(&mut self, waker: Box<dyn Fn() + Send + Sync>) {
        self.waker = Some(Arc::from(waker));
    }

    /// Feed bytes from the client and collect what to send back.
    pub fn on_bytes(&mut self, bytes: &[u8]) -> Result<Vec<HostMessage>, DaemonError> {
        self.reader.feed(bytes);
        let mut out = Vec::new();
        loop {
            let body = match self.reader.next_frame() {
                Ok(Some(b)) => b,
                Ok(None) => break,
                // A framing error means the stream position is no longer
                // trustworthy, so the caller must drop the connection rather
                // than try to continue reading past it.
                Err(e) => return Err(DaemonError::Transport(e.to_string())),
            };

            match frame::decode::<ClientMessage>(&body) {
                Ok(msg) => out.extend(self.handle(msg)),
                // A message we cannot parse is *not* fatal: a newer client may
                // send something this build has never heard of, and dropping the
                // connection over it would make every upgrade a hard cutover.
                Err(e) => {
                    tracing::warn!(error = %e, "unparseable message; ignoring");
                    out.push(HostMessage::Error {
                        session: None,
                        message: format!("could not understand that message: {e}"),
                    });
                }
            }
        }
        Ok(out)
    }

    /// Anything the attached sessions have produced since the last call.
    pub fn poll(&mut self) -> Vec<HostMessage> {
        let mut out = Vec::new();
        for (&id, &handle) in &self.attached {
            let Some(session) = self.registry.get(SessionId(id)) else { continue };
            let addr = SessionAddr::new(self.config.host, SessionId(id));

            match session.poll(handle) {
                Some((base, seq, Update::Delta(delta))) => out.push(HostMessage::Update {
                    session: addr,
                    base: Seq(base),
                    seq: Seq(seq),
                    delta,
                }),
                Some((_, seq, Update::Keyframe(k))) => out.push(HostMessage::Keyframe {
                    session: addr,
                    seq: Seq(seq),
                    cols: k.cols,
                    rows: k.rows,
                    rows_data: k.rows_data,
                    attrs: k.attrs,
                    cursor: k.cursor,
                    modes: k.modes.bits(),
                }),
                None => {}
            }

            if session.has_exited() {
                out.push(HostMessage::Exited { session: addr, code: None });
            }
        }
        out
    }

    fn handle(&mut self, msg: ClientMessage) -> Vec<HostMessage> {
        // Nothing is served before the handshake completes. Not "has said
        // hello" -- has *proved* it may be here.
        match &self.gate {
            // A refused connection stays refused. Anything else and a client
            // that failed to prove itself gets served its next message.
            Gate::Refused => {
                return vec![HostMessage::Error {
                    session: None,
                    message: "this connection was refused".into(),
                }]
            }
            Gate::Handshaking(_)
                if !matches!(
                    msg,
                    ClientMessage::Hello { .. }
                        | ClientMessage::Auth { .. }
                        | ClientMessage::PairingDecision { .. }
                ) =>
            {
                return vec![HostMessage::Error {
                    session: None,
                    message: "expected Hello first".into(),
                }]
            }
            _ => {}
        }

        match msg {
            ClientMessage::Hello { version, client, label, nonce } => {
                let Some(h) = self.handshake_mut() else {
                    return vec![HostMessage::Error {
                        session: None,
                        message: "already connected".into(),
                    }];
                };
                let step = h.on_hello(
                    version,
                    client,
                    &label,
                    zest_mesh::identity::Nonce::from_bytes(nonce.0),
                );
                match step {
                    zest_mesh::pairing::HostStep::Challenge { nonce, signature } => {
                        tracing::debug!(client = %client.short(), %label, "challenging");
                        // The host id and label come from the *transcript*, not
                        // from `config`. They are inside the signature, so a
                        // second source for either is a signature no client can
                        // verify -- which presents as "did not prove its
                        // identity" and sends whoever is debugging it looking
                        // at the crypto rather than at the two fields.
                        let t = h.transcript().expect("challenged");
                        vec![HostMessage::Challenge {
                            version: t.version,
                            host: t.host,
                            label: t.host_label.clone(),
                            nonce: zest_proto::Nonce32::from_bytes(*nonce.as_bytes()),
                            signature: zest_proto::Sig64::from_bytes(signature.to_bytes()),
                        }]
                    }
                    zest_mesh::pairing::HostStep::Refused(r) => self.refuse(r, client),
                    // `on_hello` answers with a challenge or a refusal and
                    // nothing else; the other arms are unreachable rather than
                    // merely unexpected.
                    other => {
                        tracing::error!(?other, "unexpected handshake step for Hello");
                        vec![HostMessage::AuthFailed {
                            reason: zest_proto::AuthFailure::Signature,
                            message: "handshake failed".into(),
                        }]
                    }
                }
            }

            ClientMessage::Auth { signature } => {
                if self.handshake().is_none() {
                    return vec![HostMessage::Error {
                        session: None,
                        message: "already connected".into(),
                    }];
                }
                let Ok(sig) = zest_mesh::identity::Signature::from_slice(&signature.0) else {
                    self.gate = Gate::Refused;
                    return vec![HostMessage::AuthFailed {
                        reason: zest_proto::AuthFailure::Signature,
                        message: "malformed signature".into(),
                    }];
                };

                let auth = self.auth.clone();
                let authenticator = auth.authenticator();

                // On loopback the *socket* already authorized this connection:
                // a process that can reach it runs as this user and could
                // ptrace the daemon anyway. The proof still runs -- the wire is
                // uniform -- but the trust store is not consulted, which is
                // what lets the desktop app use a throwaway identity and keep
                // the OS keychain off its startup path.
                let store: &dyn zest_mesh::trust::TrustStore = if auth.checks_trust() {
                    authenticator.trust().as_ref()
                } else {
                    &ALWAYS_TRUSTED
                };

                let h = self.handshake_mut().expect("checked above");
                let step = h.on_auth(&sig, store);
                match step {
                    zest_mesh::pairing::HostStep::Welcome => self.welcome(),
                    zest_mesh::pairing::HostStep::NeedsApproval { code } => {
                        self.ask_for_approval(&code)
                    }
                    zest_mesh::pairing::HostStep::Refused(r) => {
                        let client = self
                            .handshake()
                            .and_then(zest_mesh::pairing::HostHandshake::transcript)
                            .map_or_else(|| ClientId::from_bytes([0; 32]), |t| t.client);
                        self.refuse(r, client)
                    }
                    other => {
                        tracing::error!(?other, "unexpected handshake step for Auth");
                        vec![HostMessage::AuthFailed {
                            reason: zest_proto::AuthFailure::Signature,
                            message: "handshake failed".into(),
                        }]
                    }
                }
            }

            ClientMessage::PairingDecision { client, approve } => {
                // Loopback only, always. Reaching the loopback socket is a
                // demonstration that you are logged in at this machine, which
                // is exactly the authority enrolling a device requires --
                // accepting it over the LAN would let one paired device enrol
                // others.
                if !self.auth.may_approve_devices() {
                    tracing::warn!(
                        remote = %self.remote,
                        "a remote connection tried to approve a device"
                    );
                    return vec![HostMessage::Error {
                        session: None,
                        message: "only a local client may approve devices".into(),
                    }];
                }
                let decision = if approve {
                    zest_mesh::pairing::Decision::Approve
                } else {
                    zest_mesh::pairing::Decision::Deny
                };
                let n = self.auth.authenticator().decide(client, decision);
                tracing::info!(client = %client.short(), approve, answered = n, "pairing decision");
                Vec::new()
            }

            ClientMessage::ListSessions => {
                vec![HostMessage::Sessions { sessions: self.registry.list(self.config.host) }]
            }

            ClientMessage::CreateSession { command, cwd, cols, rows } => {
                let mut spec = CommandSpec::default_shell();
                if !command.is_empty() {
                    spec.command_line = command;
                }
                if !cwd.is_empty() {
                    spec.cwd = Some(cwd.into());
                }
                match self.registry.create(&spec, PtySize::new(cols, rows), 10_000) {
                    Ok(_) => {
                        vec![HostMessage::Sessions {
                            sessions: self.registry.list(self.config.host),
                        }]
                    }
                    Err(e) => vec![HostMessage::Error {
                        session: None,
                        message: format!("could not start a session: {e}"),
                    }],
                }
            }

            ClientMessage::Attach { session, cols, rows } => {
                let Some(s) = self.registry.get(session.session) else {
                    return vec![Self::no_such(session)];
                };
                // Attaching twice on one connection is how a client resyncs by
                // reattaching. Without this the old subscriber is dropped from
                // the map here but stays in the *session*, holding a ~48KB
                // encoder shadow and a waker for a poll that never comes again
                // -- one leak per resync, for the life of the session.
                if let Some(stale) = self.attached.remove(&session.session.0) {
                    s.detach(stale);
                }
                s.resize(cols, rows);
                let waker = self.waker.clone();
                let (handle, seq, keyframe) = s.attach_with(Box::new(move || {
                    if let Some(w) = &waker {
                        w();
                    }
                }));
                self.attached.insert(session.session.0, handle);
                vec![HostMessage::Keyframe {
                    session,
                    seq: Seq(seq),
                    cols: keyframe.cols,
                    rows: keyframe.rows,
                    rows_data: keyframe.rows_data,
                    attrs: keyframe.attrs,
                    cursor: keyframe.cursor,
                    modes: keyframe.modes.bits(),
                }]
            }

            ClientMessage::RequestKeyframe { session } => {
                let Some(handle) = self.attached.get(&session.session.0).copied() else {
                    return vec![HostMessage::Error {
                        session: Some(session),
                        message: "not attached to that session".into(),
                    }];
                };
                let Some(s) = self.registry.get(session.session) else {
                    return vec![HostMessage::Error {
                        session: Some(session),
                        message: "no such session".into(),
                    }];
                };
                match s.keyframe_for(handle) {
                    Some((_seq, k)) => vec![HostMessage::Keyframe {
                        session,
                        seq: Seq(0),
                        cols: k.cols,
                        rows: k.rows,
                        rows_data: k.rows_data,
                        attrs: k.attrs,
                        cursor: k.cursor,
                        modes: k.modes.bits(),
                    }],
                    None => vec![HostMessage::Error {
                        session: Some(session),
                        message: "not attached to that session".into(),
                    }],
                }
            }

            ClientMessage::Detach { session } => {
                if let (Some(s), Some(handle)) =
                    (self.registry.get(session.session), self.attached.remove(&session.session.0))
                {
                    // Removes the subscriber and nothing else. The shell keeps
                    // running -- that is the whole design. → ADR-007.
                    s.detach(handle);
                }
                Vec::new()
            }

            ClientMessage::Input { session, bytes } => {
                match self.registry.get(session.session) {
                    Some(s) => {
                        s.write(&bytes);
                        Vec::new()
                    }
                    None => vec![Self::no_such(session)],
                }
            }

            ClientMessage::Resize { session, cols, rows } => {
                if let Some(s) = self.registry.get(session.session) {
                    s.resize(cols, rows);
                }
                Vec::new()
            }

            ClientMessage::Ack { session, seq } => {
                if let (Some(s), Some(&handle)) =
                    (self.registry.get(session.session), self.attached.get(&session.session.0))
                {
                    s.ack(handle, seq.0);
                }
                Vec::new()
            }

            ClientMessage::RequestScrollback { session, from_line, count } => {
                let Some(s) = self.registry.get(session.session) else {
                    return vec![Self::no_such(session)];
                };
                // A negative line is not history: absolute ids start at 0, and
                // the encoder uses `i64::MIN` as its own "never seen" marker.
                let from = u64::try_from(from_line).unwrap_or(0);
                // Bounded so one request cannot ask a host to encode its entire
                // scrollback into a single frame. `MAX_FRAME` would refuse it
                // afterwards, which is a worse way to find out.
                let count = (count as usize).min(SCROLLBACK_PAGE);
                let (rows_data, attrs) = s.scrollback(from, count);
                vec![HostMessage::Scrollback { session, from_line, rows_data, attrs }]
            }

            ClientMessage::CloseSession { session } => {
                self.attached.remove(&session.session.0);
                self.registry.close(session.session);
                Vec::new()
            }
        }
    }

    /// Complete the handshake and start serving.
    fn welcome(&mut self) -> Vec<HostMessage> {
        if let Some(h) = self.handshake() {
            if let Some(t) = h.transcript() {
                tracing::info!(
                    client = %t.client.short(),
                    label = %t.client_label,
                    remote = %self.remote,
                    "client authenticated"
                );
                // Best-effort: a client is served whether or not the timestamp
                // reaches the disk, and refusing over it would turn a full disk
                // into an outage.
                let _ = self
                    .auth
                    .authenticator()
                    .trust()
                    .touch(t.client, std::time::SystemTime::now());
            }
        }
        self.gate = Gate::Served;
        self.pending = None;
        vec![HostMessage::Welcome {
            version: PROTOCOL_VERSION,
            host: self.config.host,
            label: self.config.label.clone(),
        }]
    }

    /// Say no, in terms the client can branch on.
    fn refuse(&mut self, refusal: zest_mesh::pairing::Refusal, client: ClientId) -> Vec<HostMessage> {
        // Refused, not "no longer handshaking". Nothing on this connection is
        // served afterwards.
        self.gate = Gate::Refused;
        self.pending = None;
        let reason = crate::auth::failure_for(refusal);
        tracing::warn!(
            client = %client.short(),
            remote = %self.remote,
            ?reason,
            "refused a connection"
        );
        vec![HostMessage::AuthFailed { reason, message: message_for(reason).into() }]
    }

    /// Queue an approval request and tell the client to wait.
    ///
    /// **Returns immediately.** Blocking here would hold the connection lock --
    /// the reader thread holds it across `on_bytes` -- for as long as someone
    /// took to answer, which would also make it impossible to send the very
    /// message telling them to answer.
    fn ask_for_approval(&mut self, code: &str) -> Vec<HostMessage> {
        let Some(h) = self.handshake() else { return Vec::new() };
        let Some(t) = h.transcript() else { return Vec::new() };

        let request = zest_mesh::pairing::PairingRequest {
            client: t.client,
            label: t.client_label.clone(),
            code: code.to_string(),
            remote: self.remote.clone(),
            requested_at: std::time::Instant::now(),
        };

        // The decision arrives on the channel the writer is already blocked on,
        // so nothing new has to be woken and no lock is held while waiting.
        let waker = self.waker.clone();
        let decided = self.decided.clone();
        let handle = self.auth.authenticator().ask(
            request.clone(),
            Box::new(move |d| {
                *decided.lock().expect("decision lock") = Some(d);
                if let Some(w) = &waker {
                    w();
                }
            }),
        );
        self.pending = Some(handle);

        tracing::info!(
            client = %request.client.short(),
            label = %request.label,
            remote = %request.remote,
            code = %request.code,
            "a device is asking to pair"
        );

        let mut out = vec![HostMessage::AuthPending {
            code: code.to_string(),
            expires_in_secs: u32::try_from(
                zest_mesh::pairing::APPROVAL_TIMEOUT.as_secs()
            )
            .unwrap_or(u32::MAX),
        }];
        // And tell whoever is watching locally, so the desktop app can raise a
        // modal over the same queue a terminal prompt reads.
        out.push(HostMessage::PairingRequested {
            client: request.client,
            label: request.label,
            code: request.code,
            remote: request.remote,
        });
        out
    }

    /// Apply a decision that arrived while this connection was waiting.
    ///
    /// Called from the writer loop, which is where the wake lands.
    pub fn take_decision(&mut self) -> Vec<HostMessage> {
        let decision = self.decided.lock().expect("decision lock").take();
        let Some(decision) = decision else { return Vec::new() };
        if self.handshake().is_none() {
            return Vec::new();
        }
        let client = self
            .handshake()
            .and_then(zest_mesh::pairing::HostHandshake::transcript)
            .map_or_else(|| ClientId::from_bytes([0; 32]), |t| t.client);

        match decision {
            zest_mesh::pairing::Decision::Approve => {
                let store = Arc::clone(self.auth.authenticator().trust());
                let h = self.handshake_mut().expect("checked above");
                match h.approved(store.as_ref()) {
                    zest_mesh::pairing::HostStep::Welcome => self.welcome(),
                    zest_mesh::pairing::HostStep::Refused(r) => self.refuse(r, client),
                    _ => Vec::new(),
                }
            }
            zest_mesh::pairing::Decision::Deny => {
                self.refuse(zest_mesh::pairing::Refusal::Denied, client)
            }
        }
    }

    fn no_such(session: SessionAddr) -> HostMessage {
        HostMessage::Error {
            session: Some(session),
            message: format!("no session {session}"),
        }
    }
}

/// Serve one connection until it closes.
///
/// Takes the read and write halves **separately**, and that is the whole design
/// rather than an inconvenience. A single stream behind a mutex deadlocks: the
/// reader holds the lock across a blocking `read`, which is exactly what a
/// server should be doing while a client is quiet, and the writer can then never
/// acquire it to push what a session produced.
///
/// Polling on a timer would avoid the split and cost the 0%-idle guarantee — a
/// daemon that wakes ten times a second to find nothing is a laptop that does
/// not sleep.
pub fn serve<R, W>(
    mut reader: R,
    mut writer: W,
    config: DaemonConfig,
    registry: Arc<Registry>,
    auth: crate::auth::Auth,
    remote: impl Into<String>,
) -> Result<(), DaemonError>
where
    R: Read + Send + 'static,
    W: Write + Send + 'static,
{
    let conn = Arc::new(Mutex::new(Connection::new(
        config,
        Arc::clone(&registry),
        auth,
        remote,
    )));
    let (tx, rx) = std::sync::mpsc::channel::<Wake>();

    {
        let conn = Arc::clone(&conn);
        let tx = tx.clone();
        std::thread::Builder::new()
            .name("zest-daemon-conn-read".into())
            .spawn(move || {
                let mut buf = vec![0u8; 64 * 1024];
                loop {
                    let n = match reader.read(&mut buf) {
                        Ok(0) | Err(_) => break,
                        Ok(n) => n,
                    };
                    let outgoing = {
                        let mut c = conn.lock().expect("connection lock");
                        match c.on_bytes(&buf[..n]) {
                            Ok(msgs) => msgs,
                            // Framing is broken, so the stream position can no
                            // longer be trusted and reading on would produce
                            // garbage. Ending the connection is the only honest
                            // response.
                            Err(e) => {
                                tracing::warn!(error = %e, "framing lost; closing");
                                break;
                            }
                        }
                    };
                    if tx.send(Wake::Send(outgoing)).is_err() {
                        break;
                    }
                }
                let _ = tx.send(Wake::Closed);
            })
            .map_err(|e| DaemonError::Transport(e.to_string()))?;
    }

    // Registered on every attach, so a session wakes the writer directly rather
    // than the writer discovering output by looking for it.
    {
        let tx = tx.clone();
        conn.lock().expect("connection lock").set_waker(Box::new(move || {
            let _ = tx.send(Wake::Poll);
        }));
    }

    while let Ok(wake) = rx.recv() {
        let mut outgoing = match wake {
            Wake::Closed => return Ok(()),
            Wake::Send(msgs) => msgs,
            Wake::Poll => Vec::new(),
        };
        {
            let mut c = conn.lock().expect("connection lock");
            // A pairing decision may have arrived while this connection was
            // waiting. It is delivered through the same wake the writer is
            // already blocked on, so nothing polls and no lock is held across
            // however long someone took to answer.
            outgoing.extend(c.take_decision());
            outgoing.extend(c.poll());
        }
        if outgoing.is_empty() {
            continue;
        }

        for msg in outgoing {
            let bytes = frame::encode(&msg).map_err(|e| DaemonError::Transport(e.to_string()))?;
            // Logged, not swallowed. A write failure treated as a clean
            // disconnect is indistinguishable from a client that left, and the
            // difference is the whole diagnosis when nothing arrives.
            if let Err(e) = writer.write_all(&bytes) {
                tracing::debug!(error = %e, "write failed; client is gone");
                return Ok(());
            }
        }
        if let Err(e) = writer.flush() {
            tracing::debug!(error = %e, "flush failed; client is gone");
            return Ok(());
        }
    }
    Ok(())
}

/// Why the writer woke.
enum Wake {
    /// The reader produced replies.
    Send(Vec<HostMessage>),
    /// A session has output waiting.
    Poll,
    /// The client went away.
    Closed,
}


#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};
    use zest_proto::ClientId;

    /// A host identity and an empty trust store, for the connection tests.
    fn test_authenticator() -> Arc<crate::auth::Authenticator> {
        Arc::new(crate::auth::Authenticator::new(
            Arc::new(zest_mesh::identity::HostIdentity::generate().expect("host key")),
            Arc::new(zest_mesh::trust::MemoryTrustStore::new()),
            zest_mesh::pairing::PairingQueue::new(),
            "test-host",
        ))
    }

    /// Drive a connection through the handshake, as a real client would.
    ///
    /// The tests below are about what happens *after* authentication, so each
    /// needs a connection that has been through it -- which is itself the
    /// clearest statement that nothing is served before then.
    fn authenticate(c: &mut Connection) -> zest_proto::ClientId {
        let client = zest_mesh::identity::ClientIdentity::generate().expect("client key");
        let out = send(
            c,
            &ClientMessage::Hello {
                version: PROTOCOL_VERSION,
                client: client.client_id(),
                label: "test".into(),
                nonce: zest_proto::Nonce32::from_bytes([0x5c; 32]),
            },
        );
        let [HostMessage::Challenge { nonce, host, label, version, .. }] = &out[..] else {
            panic!("expected a challenge, got {out:?}");
        };
        let transcript = zest_mesh::pairing::Transcript {
            version: *version,
            host: *host,
            client: client.client_id(),
            host_nonce: zest_mesh::identity::Nonce::from_bytes(nonce.0),
            client_nonce: zest_mesh::identity::Nonce::from_bytes([0x5c; 32]),
            host_label: label.clone(),
            client_label: "test".into(),
        };
        let sig = client.sign(
            zest_mesh::identity::Purpose::Auth,
            &zest_mesh::pairing::auth_transcript(&transcript),
        );
        let out = send(
            c,
            &ClientMessage::Auth { signature: zest_proto::Sig64::from_bytes(sig.to_bytes()) },
        );
        assert!(
            matches!(&out[..], [HostMessage::Welcome { .. }]),
            "loopback must welcome any proved client, got {out:?}"
        );
        client.client_id()
    }

    fn config() -> DaemonConfig {
        DaemonConfig {
            host: HostId::from_bytes([5; 32]),
            label: "test-host".into(),
            local_socket: String::new(),
            listen_lan: false,
        }
    }

    fn conn() -> (Connection, Arc<Registry>) {
        let registry = Arc::new(Registry::new());
        (
            Connection::new(
                config(),
                Arc::clone(&registry),
                // Loopback, which is what these tests exercise: the handshake
                // still runs, the trust store is not consulted.
                crate::auth::Auth::Transport(test_authenticator()),
                "test",
            ),
            registry,
        )
    }

    fn send(c: &mut Connection, msg: &ClientMessage) -> Vec<HostMessage> {
        let bytes = frame::encode(msg).expect("encode");
        c.on_bytes(&bytes).expect("handled")
    }

    fn hello() -> ClientMessage {
        ClientMessage::Hello {
            version: PROTOCOL_VERSION,
            client: ClientId::from_bytes([1; 32]),
            label: "test".into(),
            nonce: zest_proto::Nonce32::from_bytes([6; 32]),
        }
    }

    fn echo_cmd() -> String {
        if cfg!(windows) { "cmd.exe /c echo probe".into() } else { "/bin/echo probe".into() }
    }

    fn wait_for(mut f: impl FnMut() -> bool) -> bool {
        let deadline = Instant::now() + Duration::from_secs(10);
        while Instant::now() < deadline {
            if f() {
                return true;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        false
    }

    #[test]
    fn hello_is_answered_with_a_challenge_not_a_welcome() {
        // The whole of this stage in one assertion. Saying hello is not proof
        // of anything, and a host that welcomed on it authorized nobody.
        let (mut c, _) = conn();
        let out = send(&mut c, &hello());
        assert!(
            matches!(&out[..], [HostMessage::Challenge { .. }]),
            "a bare Hello was served: {out:?}"
        );
    }

    #[test]
    fn a_proved_client_is_welcomed_by_this_host() {
        let (mut c, _) = conn();
        authenticate(&mut c);
        let out = send(&mut c, &ClientMessage::ListSessions);
        assert!(matches!(&out[..], [HostMessage::Sessions { .. }]), "{out:?}");
    }

    #[test]
    fn an_unsigned_client_is_never_served() {
        // Answering the challenge with noise must not get anywhere, and must
        // say why in a form a client can branch on.
        let (mut c, _) = conn();
        let out = send(&mut c, &hello());
        assert!(matches!(&out[..], [HostMessage::Challenge { .. }]), "{out:?}");

        let out = send(
            &mut c,
            &ClientMessage::Auth { signature: zest_proto::Sig64::from_bytes([0; 64]) },
        );
        assert!(
            matches!(
                &out[..],
                [HostMessage::AuthFailed { reason: zest_proto::AuthFailure::Signature, .. }]
            ),
            "{out:?}"
        );

        // And it stays refused rather than being served the next message.
        let out = send(&mut c, &ClientMessage::ListSessions);
        assert!(matches!(&out[..], [HostMessage::Error { .. }]), "{out:?}");
    }

    #[test]
    fn a_remote_connection_may_not_approve_devices() {
        // Approving a device is the authority of whoever is logged in at the
        // machine. Accepting it over the LAN would let one paired device enrol
        // others.
        let registry = Arc::new(Registry::new());
        let mut c = Connection::new(
            config(),
            registry,
            crate::auth::Auth::Proof(test_authenticator()),
            "192.168.1.42:51314",
        );
        let out = send(
            &mut c,
            &ClientMessage::PairingDecision {
                client: ClientId::from_bytes([9; 32]),
                approve: true,
            },
        );
        assert!(matches!(&out[..], [HostMessage::Error { .. }]), "{out:?}");
    }

    #[test]
    fn a_version_mismatch_is_refused_rather_than_served() {
        // Proceeding anyway produces a corrupt grid on the client, which looks
        // like a rendering bug and gets chased in the wrong codebase.
        let (mut c, _) = conn();
        let out = send(
            &mut c,
            &ClientMessage::Hello {
                version: PROTOCOL_VERSION + 99,
                client: ClientId::from_bytes([1; 32]),
                label: "future".into(),
                nonce: zest_proto::Nonce32::from_bytes([7; 32]),
            },
        );
        assert!(
            matches!(
                &out[..],
                [HostMessage::AuthFailed { reason: zest_proto::AuthFailure::Version, .. }]
            ),
            "a version mismatch must be refused by name, not as a generic error: {out:?}"
        );
    }

    #[test]
    fn nothing_is_served_before_the_handshake() {
        let (mut c, _) = conn();
        let out = send(&mut c, &ClientMessage::ListSessions);
        assert!(matches!(&out[..], [HostMessage::Error { .. }]), "{out:?}");
    }

    #[test]
    fn an_unparseable_message_does_not_drop_the_connection() {
        // A newer client may send something this build has never heard of.
        // Dropping the connection would make every upgrade a hard cutover.
        let (mut c, _) = conn();
        authenticate(&mut c);

        let mut junk = Vec::new();
        junk.extend_from_slice(&(4u32).to_le_bytes());
        junk.extend_from_slice(b"junk");
        let out = c.on_bytes(&junk).expect("not fatal");
        assert!(matches!(&out[..], [HostMessage::Error { .. }]));

        // ...and the connection still works.
        let after = send(&mut c, &ClientMessage::ListSessions);
        assert!(matches!(&after[..], [HostMessage::Sessions { .. }]), "{after:?}");
    }

    #[test]
    fn creating_a_session_lists_it() {
        let (mut c, registry) = conn();
        authenticate(&mut c);

        let out = send(
            &mut c,
            &ClientMessage::CreateSession {
                command: echo_cmd(),
                cwd: String::new(),
                cols: 80,
                rows: 24,
            },
        );
        assert!(matches!(&out[..], [HostMessage::Sessions { sessions }] if sessions.len() == 1));
        assert_eq!(registry.len(), 1);
    }

    #[test]
    fn attaching_returns_a_keyframe_and_then_output() {
        let (mut c, registry) = conn();
        authenticate(&mut c);
        send(
            &mut c,
            &ClientMessage::CreateSession {
                command: echo_cmd(),
                cwd: String::new(),
                cols: 80,
                rows: 24,
            },
        );
        let addr = registry.list(config().host)[0].addr;

        let out = send(&mut c, &ClientMessage::Attach { session: addr, cols: 80, rows: 24 });
        assert!(matches!(&out[..], [HostMessage::Keyframe { .. }]), "{out:?}");

        assert!(
            wait_for(|| !c.poll().is_empty()),
            "the child produced output but nothing reached the client"
        );
    }

    #[test]
    fn detaching_leaves_the_session_in_the_registry() {
        // The property ADR-007 exists for: a client leaving must not end a shell.
        let (mut c, registry) = conn();
        authenticate(&mut c);
        send(
            &mut c,
            &ClientMessage::CreateSession {
                command: echo_cmd(),
                cwd: String::new(),
                cols: 80,
                rows: 24,
            },
        );
        let addr = registry.list(config().host)[0].addr;
        send(&mut c, &ClientMessage::Attach { session: addr, cols: 80, rows: 24 });

        send(&mut c, &ClientMessage::Detach { session: addr });
        assert_eq!(registry.len(), 1, "detaching removed the session");
        assert!(registry.get(addr.session).is_some());
    }

    #[test]
    fn closing_a_session_removes_it() {
        let (mut c, registry) = conn();
        authenticate(&mut c);
        send(
            &mut c,
            &ClientMessage::CreateSession {
                command: echo_cmd(),
                cwd: String::new(),
                cols: 80,
                rows: 24,
            },
        );
        let addr = registry.list(config().host)[0].addr;

        send(&mut c, &ClientMessage::CloseSession { session: addr });
        assert_eq!(registry.len(), 0);
    }

    #[test]
    fn input_for_an_unknown_session_is_an_error_not_a_panic() {
        let (mut c, _) = conn();
        authenticate(&mut c);
        let addr = SessionAddr::new(config().host, SessionId(999));
        let out = send(&mut c, &ClientMessage::Input { session: addr, bytes: vec![b'x'] });
        assert!(matches!(&out[..], [HostMessage::Error { .. }]), "{out:?}");
    }

    #[test]
    fn a_listing_keeps_a_stable_order() {
        // A list that reshuffles between polls is unusable on a phone.
        let (mut c, registry) = conn();
        authenticate(&mut c);
        for _ in 0..4 {
            send(
                &mut c,
                &ClientMessage::CreateSession {
                    command: echo_cmd(),
                    cwd: String::new(),
                    cols: 80,
                    rows: 24,
                },
            );
        }
        let first: Vec<u64> =
            registry.list(config().host).iter().map(|s| s.addr.session.0).collect();
        let second: Vec<u64> =
            registry.list(config().host).iter().map(|s| s.addr.session.0).collect();
        assert_eq!(first, second);
        assert!(first.windows(2).all(|w| w[0] < w[1]), "not sorted: {first:?}");
    }

    #[test]
    fn every_session_is_addressed_with_this_host() {
        // The fleet property. A session named without its host is unreachable
        // from a client holding sessions from several machines.
        let (mut c, registry) = conn();
        authenticate(&mut c);
        send(
            &mut c,
            &ClientMessage::CreateSession {
                command: echo_cmd(),
                cwd: String::new(),
                cols: 80,
                rows: 24,
            },
        );
        for info in registry.list(config().host) {
            assert_eq!(info.addr.host, config().host);
        }
    }
}
