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
    /// Bumped whenever a listing would read differently: create, close,
    /// collection, attach, detach. What lets a watching connection answer
    /// "did anything change?" without diffing two listings.
    generation: std::sync::atomic::AtomicU64,
    /// Wakers for connections that asked to watch the session list
    /// (`Hello.watch_sessions`), keyed by a token so `Drop` can unregister.
    watchers: Mutex<HashMap<u64, Arc<dyn Fn() + Send + Sync>>>,
    next_watcher: Mutex<u64>,
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
        self.touch();
        Ok(session)
    }

    /// The session list changed; tell everyone who asked to hear it.
    ///
    /// Called *after* the change is visible in `sessions`, so a woken
    /// connection that lists immediately sees the new truth.
    pub fn touch(&self) {
        self.generation.fetch_add(1, std::sync::atomic::Ordering::Release);
        for waker in self.watchers.lock().expect("watchers lock").values() {
            waker();
        }
    }

    #[must_use]
    pub fn generation(&self) -> u64 {
        self.generation.load(std::sync::atomic::Ordering::Acquire)
    }

    /// Register a waker to run whenever the listing changes.
    pub fn watch(&self, waker: Arc<dyn Fn() + Send + Sync>) -> u64 {
        let token = {
            let mut next = self.next_watcher.lock().expect("watcher id lock");
            *next += 1;
            *next
        };
        self.watchers.lock().expect("watchers lock").insert(token, waker);
        token
    }

    pub fn unwatch(&self, token: u64) {
        self.watchers.lock().expect("watchers lock").remove(&token);
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
                    cwd: s.cwd(),
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
    ///
    /// The hangup is explicit and happens **outside** the registry lock. Two
    /// reasons, and the first is correctness rather than contention: dropping
    /// the `Arc` cannot be relied on to end anything, because a concurrent
    /// `get`, `list` or `poll` may be holding a clone, and on unix even the last
    /// drop would not hang up a pty whose reader is parked. The second is that
    /// `hangup` blocks for as long as the child takes to leave, and holding the
    /// registry lock across that would stall every other session's poll.
    pub fn close(&self, id: SessionId) {
        let session = self.sessions.lock().expect("registry lock").remove(&id.0);
        if let Some(s) = session {
            s.hangup();
            // Sweep funnels through here too, so a collected session bumps
            // the generation exactly once — and a close of a session already
            // gone bumps nothing.
            self.touch();
        }
    }

    /// Forget sessions whose child has exited and that nobody is watching.
    ///
    /// Without this a shell that exits on its own is reported as `Exited` and
    /// then kept forever, holding its terminal and the whole scrollback behind
    /// it, and appearing in every listing as though it were alive. `close` only
    /// covers the case where a client asks.
    ///
    /// **Three conditions, and the middle one is the subtle one.** A session is
    /// only collectable once somebody has actually attached to it, because
    /// creating a session and attaching to it are two round trips: a short
    /// command like `echo` exits in between, and sweeping there hands the client
    /// that just created the session a "no session" error for a shell that ran
    /// perfectly. It cost a CI failure that read as a test race.
    ///
    /// The residual case is a client that creates a session and never attaches
    /// -- it dies between the two round trips. That session is kept, which is
    /// the right way round: keeping something nobody asked to be rid of is a
    /// leak, and dropping something a client is about to ask for is data loss.
    ///
    /// **Sweeping on exit alone would drop the
    /// session before an attached client had been told, and a client that never
    /// learns its shell exited waits for output that will never come.** A
    /// subscriber is released when its client detaches or its connection drops,
    /// so this becomes true shortly afterwards either way.
    pub fn sweep(&self) {
        let dead: Vec<u64> = {
            let sessions = self.sessions.lock().expect("registry lock");
            sessions
                .iter()
                .filter(|(_, s)| s.has_exited() && s.ever_attached() && !s.attached())
                .map(|(&id, _)| id)
                .collect()
        };
        for id in dead {
            // Through `close`, so an exited-but-unreaped child is still reaped.
            // `has_exited` means the reader saw EOF, which is not the same as
            // the process having been waited on.
            self.close(SessionId(id));
        }
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
    /// Called once, when the handshake completes.
    ///
    /// The LAN listener uses it to disarm its watchdog and release its
    /// mid-handshake slot. Without a signal at exactly this moment, "still
    /// handshaking" and "connection still open" are the same question, and both
    /// the watchdog and the connection cap answered the wrong one.
    on_ready: Option<Box<dyn FnOnce() + Send>>,
    /// Called when this connection starts waiting for a person to approve it.
    ///
    /// Separate from `on_ready` because the two mean different things to the
    /// LAN listener: ready means "stop watching and give back the slot", while
    /// this means "still unauthenticated, still holding its slot, but waiting on
    /// a human rather than stalled". Conflating them cut every pairing attempt
    /// at the handshake timeout — ten seconds into a window advertised as 120.
    on_pending: Option<Box<dyn FnOnce() + Send>>,
    /// Handed to each session on attach, so output wakes the writer.
    waker: Option<Arc<dyn Fn() + Send + Sync>>,
    /// This client asked (`Hello.watch_sessions`) to hear listing changes.
    watch_sessions: bool,
    /// Registration in [`Registry::watch`], for `Drop` to release.
    watch_token: Option<u64>,
    /// The registry generation this connection last told its client about.
    seen_generation: u64,
}

impl Drop for Connection {
    /// Release every subscription this connection held.
    ///
    /// A connection ends far more often by the socket dropping — a closed lid, a
    /// lost Wi-Fi link — than by a polite `Detach`, and until this existed those
    /// endings left the subscriber registered forever. Three things followed
    /// from that: the ~48KB encoder shadow behind each one was never freed, the
    /// session reported itself as attached in every listing, and — once sessions
    /// began being swept — an exited shell could never be collected, because
    /// something was permanently "watching" it.
    ///
    /// The sweep runs here rather than only in `poll` so that a daemon whose
    /// last client just vanished still collects, instead of waiting for a client
    /// that may not come back for hours.
    fn drop(&mut self) {
        if let Some(token) = self.watch_token.take() {
            self.registry.unwatch(token);
        }
        let had_subscriptions = !self.attached.is_empty();
        for (&id, &handle) in &self.attached {
            if let Some(s) = self.registry.get(SessionId(id)) {
                s.detach(handle);
            }
        }
        self.registry.sweep();
        // The detaches above changed `attached` in every listing row this
        // connection held; sweep announces its own removals through `close`.
        if had_subscriptions {
            self.registry.touch();
        }
    }
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
            on_ready: None,
            on_pending: None,
            waker: None,
            watch_sessions: false,
            watch_token: None,
            seen_generation: 0,
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

    /// Be told when this connection finishes its handshake.
    pub fn set_on_ready(&mut self, f: Box<dyn FnOnce() + Send>) {
        self.on_ready = Some(f);
    }

    pub fn set_on_pending(&mut self, f: Box<dyn FnOnce() + Send>) {
        self.on_pending = Some(f);
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

        // The listing push, first and coalesced: however many changes piled
        // up since this connection last looked, one `Sessions` describes the
        // current truth. Only for clients that asked (`Hello.watch_sessions`).
        if self.watch_sessions && matches!(self.gate, Gate::Served) {
            let generation = self.registry.generation();
            if generation != self.seen_generation {
                self.seen_generation = generation;
                out.push(HostMessage::Sessions {
                    sessions: self.registry.list(self.config.host),
                    created: None,
                });
            }
        }
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
                    blocks: k.blocks,
                    title: k.title,
                }),
                None => {}
            }

            if session.has_exited() {
                out.push(HostMessage::Exited { session: addr, code: None });
            }
        }
        // After the loop: anything reported `Exited` above is still attached
        // here, so it survives this pass and is collected once this connection
        // detaches or drops. Sweeping from every connection's poll — rather than
        // from a timer — is enough, because a session only becomes sweepable
        // when a client goes away, and a client going away is itself a wakeup.
        self.registry.sweep();
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
            // Only the handshake's own two messages. `PairingDecision` used to
            // be exempt here, which let a loopback process approve a pending
            // device without completing a handshake at all -- not a remote
            // hole, since the socket is the authorization there, but a wider
            // surface than the design describes and not what this arm implies.
            Gate::Handshaking(_)
                if !matches!(
                    msg,
                    ClientMessage::Hello { .. } | ClientMessage::Auth { .. }
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
            ClientMessage::Hello { version, client, label, nonce, watch_sessions } => {
                self.watch_sessions = watch_sessions;
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
                vec![HostMessage::Sessions {
                    sessions: self.registry.list(self.config.host),
                    created: None,
                }]
            }

            ClientMessage::CreateSession { command, cwd, cols, rows } => {
                let mut spec = CommandSpec::default_shell();
                if !command.is_empty() {
                    spec.command_line = command;
                }
                if !cwd.is_empty() {
                    spec.cwd = Some(cwd.into());
                }
                // After `command_line` is settled, because which shell this is
                // decides what gets injected -- and a client may have asked for
                // something that is not a shell at all.
                if self.config.shell_integration {
                    spec.enable_shell_integration(&shell_integration_dir());
                }
                match self.registry.create(&spec, PtySize::new(cols, rows), 10_000) {
                    Ok(created) => {
                        vec![HostMessage::Sessions {
                            sessions: self.registry.list(self.config.host),
                            // Named explicitly: `sessions.last()` was the old
                            // heuristic, and it hands one of two concurrent
                            // creators the other one's shell.
                            created: Some(created.id),
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
                // Another watcher's listing shows this session as attached now.
                self.registry.touch();
                vec![HostMessage::Keyframe {
                    session,
                    seq: Seq(seq),
                    cols: keyframe.cols,
                    rows: keyframe.rows,
                    rows_data: keyframe.rows_data,
                    attrs: keyframe.attrs,
                    cursor: keyframe.cursor,
                    modes: keyframe.modes.bits(),
                    blocks: keyframe.blocks,
                    title: keyframe.title,
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
                    Some((seq, k)) => vec![HostMessage::Keyframe {
                        session,
                        // The *real* sequence. Sending 0 here set the client's
                        // baseline to 0 while the daemon set `sub.sent` to the
                        // true value, so the next update's `base` never matched
                        // and the client asked for another keyframe -- which
                        // came back as 0 again. A resize sends RequestKeyframe,
                        // so resizing a daemon-backed window froze the terminal
                        // in that loop for every byte the shell printed.
                        seq: Seq(seq),
                        cols: k.cols,
                        rows: k.rows,
                        rows_data: k.rows_data,
                        attrs: k.attrs,
                        cursor: k.cursor,
                        modes: k.modes.bits(),
                        blocks: k.blocks,
                        title: k.title,
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
                    // `attached` changed in the listing.
                    self.registry.touch();
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
        // Exactly here, and only once: the gate has opened. The LAN listener
        // disarms its watchdog and gives back its mid-handshake slot.
        if let Some(f) = self.on_ready.take() {
            f();
        }
        // Registered only once served — an unauthenticated connection has no
        // business being woken by listing changes it may never see.
        if self.watch_sessions && self.watch_token.is_none() {
            self.seen_generation = self.registry.generation();
            if let Some(waker) = self.waker.clone() {
                self.watch_token = Some(self.registry.watch(waker));
            }
        }
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

        // Exactly here: the request is queued and the client is about to be told
        // to wait. Anything watching this connection for a stalled handshake has
        // to be told that a person is now in the loop, or it cuts the socket
        // long before the person can answer.
        if let Some(f) = self.on_pending.take() {
            f();
        }

        tracing::info!(
            client = %request.client.short(),
            label = %request.label,
            remote = %request.remote,
            code = %request.code,
            "a device is asking to pair"
        );

        // `AuthPending` only. `PairingRequested` is the *local* notification --
        // it exists so the desktop app can raise a modal -- and it belongs on
        // whatever loopback connections are watching, not on the connection
        // that caused it. Sending it here told the device asking to pair the
        // address it was connecting from, which is at best noise and at worst
        // an echo of something it did not know.
        //
        // Pushing it to other connections needs a subscription the daemon does
        // not have yet, and neither does the modal that would consume it. The
        // message stays defined; nothing sends it until there is somewhere for
        // it to go.
        vec![HostMessage::AuthPending {
            code: code.to_string(),
            expires_in_secs: u32::try_from(
                zest_mesh::pairing::APPROVAL_TIMEOUT.as_secs()
            )
            .unwrap_or(u32::MAX),
        }]
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
    reader: R,
    writer: W,
    config: DaemonConfig,
    registry: Arc<Registry>,
    auth: crate::auth::Auth,
    remote: impl Into<String>,
) -> Result<(), DaemonError>
where
    R: Read + Send + 'static,
    W: Write + Send + 'static,
{
    serve_with(reader, writer, config, registry, auth, remote, Hooks::default())
}

fn serve_with<R, W>(
    mut reader: R,
    mut writer: W,
    config: DaemonConfig,
    registry: Arc<Registry>,
    auth: crate::auth::Auth,
    remote: impl Into<String>,
    hooks: Hooks,
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
    if let Some(f) = hooks.ready {
        conn.lock().expect("connection lock").set_on_ready(f);
    }
    if let Some(f) = hooks.pending {
        conn.lock().expect("connection lock").set_on_pending(f);
    }
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

/// Serve a LAN connection, telling the listener when the handshake completes.
///
/// A thin wrapper rather than more parameters on `serve`, so the loopback path
/// -- which has no watchdog and no connection cap -- stays exactly as it was.
#[allow(clippy::too_many_arguments, reason = "one call site, in lan.rs")]
pub fn serve_lan<R, W>(
    reader: R,
    writer: W,
    config: DaemonConfig,
    registry: Arc<Registry>,
    auth: crate::auth::Auth,
    remote: String,
    watchdog: crate::lan::WatchdogHandle,
    mut slot: crate::lan::Countdown,
) -> Result<(), DaemonError>
where
    R: Read + Send + 'static,
    W: Write + Send + 'static,
{
    let waiting = watchdog.clone();
    serve_with(
        reader,
        writer,
        config,
        registry,
        auth,
        remote,
        Hooks {
            ready: Some(Box::new(move || {
                watchdog.completed();
                slot.release();
            })),
            // Waiting on a person, not stalled. The deadline moves out past the
            // pairing window so the queue's own expiry denies the request and
            // the client is told why — rather than the socket simply vanishing,
            // which is indistinguishable from the host going away.
            pending: Some(Box::new(move || {
                waiting.awaiting_approval(
                    zest_mesh::pairing::APPROVAL_TIMEOUT + std::time::Duration::from_secs(10),
                );
            })),
        },
    )
}

/// What the transport wants to know about a connection's progress.
///
/// One struct rather than two parameters because they are answers to the same
/// question — how far has this connection got — and because the loopback
/// transport wants neither, which `Hooks::default()` says more clearly than a
/// pair of `None`s.
#[derive(Default)]
struct Hooks {
    /// The handshake completed and the connection is being served.
    ready: Option<Box<dyn FnOnce() + Send>>,
    /// The connection is waiting for a person to approve it. Still
    /// unauthenticated, still holding whatever the transport counted it against.
    pending: Option<Box<dyn FnOnce() + Send>>,
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

/// Where the shell-integration shim is written.
///
/// A `shell-integration/` subdirectory of the config directory, for the reason
/// `fleet/` is one: `zest-config` watches the config root non-recursively, so
/// writing here cannot produce an event the settings watcher has to filter out
/// — and this is written on every spawn.
///
/// Falls back to the current directory when there is no config directory at
/// all, which is a machine with no home. A shim written somewhere odd still
/// works; refusing to spawn a shell would not.
fn shell_integration_dir() -> std::path::PathBuf {
    directories::ProjectDirs::from("dev", "zesterm", "zesterm").map_or_else(
        || std::path::PathBuf::from("zesterm-shell-integration"),
        |dirs| dirs.config_dir().join("shell-integration"),
    )
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
        authenticate_with(c, false)
    }

    fn authenticate_with(c: &mut Connection, watch_sessions: bool) -> zest_proto::ClientId {
        let client = zest_mesh::identity::ClientIdentity::generate().expect("client key");
        let out = send(
            c,
            &ClientMessage::Hello {
                version: PROTOCOL_VERSION,
                client: client.client_id(),
                label: "test".into(),
                nonce: zest_proto::Nonce32::from_bytes([0x5c; 32]),
                watch_sessions,
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
            lan_bind: "127.0.0.1".into(),
            lan_port: 0,
            listen_ws: false,
            ws_bind: "127.0.0.1".into(),
            ws_port: 0,
            shell_integration: true,
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

    #[test]
    fn a_watcher_hears_about_sessions_it_did_not_touch() {
        // The picker's liveness: a listing that only answers this
        // connection's own requests goes stale the moment another client
        // acts. `Hello.watch_sessions` opts in; the push is the whole current
        // list, coalesced through the generation counter.
        let (mut watcher, registry) = conn();
        let woken = Arc::new(std::sync::atomic::AtomicBool::new(false));
        {
            let woken = Arc::clone(&woken);
            watcher.set_waker(Box::new(move || {
                woken.store(true, std::sync::atomic::Ordering::Release);
            }));
        }
        authenticate_with(&mut watcher, true);

        let mut creator = Connection::new(
            config(),
            Arc::clone(&registry),
            crate::auth::Auth::Transport(test_authenticator()),
            "test2",
        );
        authenticate(&mut creator);
        let out = send(
            &mut creator,
            &ClientMessage::CreateSession {
                command: sleep_cmd(),
                cwd: String::new(),
                cols: 20,
                rows: 5,
            },
        );

        // The creator's reply names its session outright — the `.last()`
        // heuristic hands one of two concurrent creators the other's shell.
        let [HostMessage::Sessions { sessions, created: Some(id) }] = &out[..] else {
            panic!("expected a Sessions reply naming the created session, got {out:?}");
        };
        assert_eq!(sessions.last().map(|s| s.addr.session), Some(*id));
        let id = *id;

        assert!(
            woken.load(std::sync::atomic::Ordering::Acquire),
            "creating a session must wake a watching connection"
        );
        let pushed = watcher.poll();
        assert!(
            matches!(&pushed[..], [HostMessage::Sessions { sessions, created: None }]
                if sessions.len() == 1),
            "the watcher's poll must carry the listing push, got {pushed:?}"
        );
        assert!(
            watcher.poll().iter().all(|m| !matches!(m, HostMessage::Sessions { .. })),
            "no change since the last poll means no push"
        );

        registry.close(id);
    }

    #[test]
    fn a_client_that_did_not_ask_gets_no_push() {
        // Push is opt-in: an old client would mistake an unsolicited
        // Sessions for the reply to a request it is about to make.
        let (mut c, registry) = conn();
        authenticate(&mut c);
        let out = send(
            &mut c,
            &ClientMessage::CreateSession {
                command: sleep_cmd(),
                cwd: String::new(),
                cols: 20,
                rows: 5,
            },
        );
        let [HostMessage::Sessions { created: Some(id), .. }] = &out[..] else {
            panic!("expected a Sessions reply, got {out:?}");
        };
        let id = *id;
        assert!(
            c.poll().iter().all(|m| !matches!(m, HostMessage::Sessions { .. })),
            "a non-watcher must never receive an unsolicited Sessions"
        );
        registry.close(id);
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
            watch_sessions: false,
        }
    }

    fn echo_cmd() -> String {
        if cfg!(windows) { "cmd.exe /c echo probe".into() } else { "/bin/echo probe".into() }
    }

    /// A child that outlives the test unless something ends it.
    ///
    /// The lifetime is the point: anything about *closing* a session is vacuous
    /// against a command that has already exited on its own.
    fn sleep_cmd() -> String {
        if cfg!(windows) {
            "powershell.exe -NoProfile -Command Start-Sleep 30".into()
        } else {
            "/bin/sleep 30".into()
        }
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
    fn a_requested_keyframe_carries_a_real_sequence() {
        // This is where the bug was: the session returned the right sequence
        // and the wire message threw it away for `Seq(0)`. The client's
        // baseline went to zero, every later update was refused as stale, and
        // each refusal asked for another keyframe that again said zero -- so a
        // window that had been resized once did a full repaint round trip for
        // every byte the shell printed. It still *updated*, which is exactly
        // why nothing noticed.
        let (mut c, _) = conn();
        authenticate(&mut c);
        send(&mut c, &ClientMessage::CreateSession {
            command: echo_cmd(),
            cwd: String::new(),
            cols: 80,
            rows: 24,
        });
        let addr = SessionAddr::new(config().host, SessionId(1));
        send(&mut c, &ClientMessage::Attach { session: addr, cols: 80, rows: 24 });

        let out = send(&mut c, &ClientMessage::RequestKeyframe { session: addr });
        let [HostMessage::Keyframe { seq, .. }] = &out[..] else {
            panic!("expected a keyframe, got {out:?}");
        };
        assert_ne!(
            seq.0, 0,
            "the keyframe named sequence 0, so the client's baseline and the \
             daemon's would disagree from that moment on"
        );
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
                watch_sessions: false,
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
        assert!(matches!(&out[..], [HostMessage::Sessions { sessions, .. }] if sessions.len() == 1));
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

    /// The half of `CloseSession` the registry cannot see.
    ///
    /// Removing the entry was the whole of `close` for a long time, and every
    /// test asked only whether the map had shrunk — which it had, while the
    /// shell went on running. The observable that distinguishes them is the
    /// reader thread reaching EOF, so the session is held across the close and
    /// asked whether its child actually left. A long-lived command is essential:
    /// with `echo` the child is already gone and this passes either way.
    #[test]
    fn closing_a_session_ends_its_child() {
        let (mut c, registry) = conn();
        authenticate(&mut c);
        send(
            &mut c,
            &ClientMessage::CreateSession {
                command: sleep_cmd(),
                cwd: String::new(),
                cols: 80,
                rows: 24,
            },
        );
        let addr = registry.list(config().host)[0].addr;
        let session = registry.get(addr.session).expect("the session was just created");
        assert!(!session.has_exited(), "the child should still be running");

        send(&mut c, &ClientMessage::CloseSession { session: addr });

        assert!(
            wait_for(|| session.has_exited()),
            "the child outlived CloseSession. Removing the registry entry does \
             not end anything: the pty's reader is parked holding the master \
             open, so nothing closes and the shell runs until the daemon does"
        );
    }

    /// A shell that exits on its own must not be kept forever.
    ///
    /// **Unix only, and that is a known gap rather than a platform quirk.**
    /// `has_exited` is driven by the reader thread ending, which on Windows
    /// cannot happen: ConPTY holds the output pipe's write end until the
    /// pseudoconsole is closed, so a blocked `ReadFile` stays blocked after the
    /// shell is gone (windows.rs gotcha 2b). No `Exited` is sent there and
    /// nothing is swept. → #18.
    ///
    /// An attempt to close that gap by waiting on the process handle and
    /// flagging exit from there deadlocked Windows CI outright — `cargo test`
    /// ran for over an hour against a normal nine minutes. Backed out rather
    /// than iterated on blind, because the API involved has a documented
    /// deadlock and this machine cannot run it.
    ///
    /// It was: `Exited` was reported to whoever was attached and the session
    /// stayed in the registry with its whole terminal and scrollback, listed as
    /// though it were alive. Both halves are asserted, because sweeping too
    /// eagerly is its own bug — a client that never learns its shell exited
    /// waits for output that is not coming.
    #[cfg(unix)]
    #[test]
    fn an_exited_session_is_kept_until_nobody_is_watching() {
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

        assert!(
            wait_for(|| c
                .poll()
                .iter()
                .any(|m| matches!(m, HostMessage::Exited { .. }))),
            "an attached client must be told its shell exited"
        );
        assert_eq!(
            registry.len(),
            1,
            "swept while still attached: the client would be told about a \
             session that no longer exists"
        );

        send(&mut c, &ClientMessage::Detach { session: addr });
        c.poll();
        assert_eq!(
            registry.len(),
            0,
            "an exited session nobody is watching is dead, and keeping it holds \
             its terminal and scrollback for the life of the daemon"
        );
    }

    /// A session must survive the gap between creating it and attaching to it.
    ///
    /// The two are separate round trips, and a short command exits inside the
    /// gap. Sweeping there hands the client that just created the session a "no
    /// session" error for a shell that ran perfectly — which is exactly what
    /// happened on CI, and read at first like a flaky test rather than a lost
    /// session.
    #[cfg(unix)]
    #[test]
    fn a_session_is_not_swept_before_anyone_has_attached() {
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
        let session = registry.get(addr.session).expect("just created");

        // The child is gone well before a real client's next round trip.
        assert!(wait_for(|| session.has_exited()), "echo should exit at once");
        c.poll();

        assert_eq!(
            registry.len(),
            1,
            "swept before the client that created it could attach; that client \
             now gets \"no session\" for a shell it asked for and that ran"
        );
        let out = send(&mut c, &ClientMessage::Attach { session: addr, cols: 80, rows: 24 });
        assert!(
            matches!(&out[..], [HostMessage::Keyframe { .. }]),
            "attach must still succeed, and carry what the command printed: {out:?}"
        );
    }

    /// A connection that vanishes is the common case, not the polite `Detach`.
    #[test]
    fn a_dropped_connection_releases_its_subscriptions() {
        let registry = Arc::new(Registry::new());
        let session = {
            let mut c = Connection::new(
                config(),
                Arc::clone(&registry),
                crate::auth::Auth::Transport(test_authenticator()),
                "test",
            );
            authenticate(&mut c);
            send(
                &mut c,
                &ClientMessage::CreateSession {
                    command: sleep_cmd(),
                    cwd: String::new(),
                    cols: 80,
                    rows: 24,
                },
            );
            let addr = registry.list(config().host)[0].addr;
            send(&mut c, &ClientMessage::Attach { session: addr, cols: 80, rows: 24 });
            let s = registry.get(addr.session).expect("just created");
            assert!(s.attached(), "the attach did not register a subscriber");
            s
        };

        assert!(
            !session.attached(),
            "a dropped connection left its subscriber behind. The encoder \
             shadow behind it is never freed, the session reports itself as \
             watched in every listing, and it can never be swept"
        );
        session.hangup();
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
