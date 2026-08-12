//! Answering a client.
//!
//! Deliberately written against `Read + Write` rather than against a socket, so
//! the whole protocol loop can be driven from a byte buffer in a test. A message
//! handler that can only be exercised through a real connection is one whose
//! error paths are never exercised at all.

use std::collections::HashMap;
use std::io::{Read, Write};
use std::sync::{Arc, Mutex};
use std::time::Instant;

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

/// This connection's channel, and where the switch to it stands.
///
/// The seal switch is *positional* — the `Challenge` is the last plaintext
/// frame in each direction — and the two directions therefore flip at two
/// different moments. Incoming flips when the `Challenge` is **produced**, so a
/// client that pipelines its `Auth` behind the `Hello` is still read correctly.
/// Outgoing flips when the `Challenge` is **written**, because that frame
/// carries the host's DH key and sealing it under a key derived from itself
/// would be unopenable.
///
/// One flag rather than two channels because a `SecureChannel` holds both
/// directions: `Some(_)` means incoming is sealed, and `out` means outgoing is.
struct Seal {
    channel: zest_mesh::secure::SecureChannel,
    out: bool,
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
    /// This connection's encryption, from the `Challenge` onwards. See [`Seal`].
    seal: Option<Seal>,
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
            seal: None,
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

            // Since protocol 3 the bytes behind the prefix are ciphertext from
            // the `Auth` onwards, so this is where they stop being opaque. A
            // frame that will not open is fatal in a way an unparseable one is
            // not: the counter has already advanced, so there is no position to
            // resume from, and it means either tampering or a key disagreement
            // -- neither of which gets better by reading the next frame.
            let body = match &mut self.seal {
                Some(s) => match s.channel.open(&body) {
                    Ok(plain) => plain,
                    Err(e) => {
                        return Err(DaemonError::Transport(format!(
                            "a frame did not open: {e}"
                        )))
                    }
                },
                None => body,
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
        self.poll_with(true)
    }

    /// The same, with the option of leaving session output where it is.
    ///
    /// `updates == false` is the coalescing floor (`min_delta_interval`): the
    /// grid is *not* asked for its difference, so nothing is consumed and
    /// nothing is buffered here — the next pass sends one delta describing
    /// wherever the terminal has got to. Everything else in this function still
    /// runs, which is the point: a listing push, an `Exited` and a session the
    /// sweep can collect are not deltas, and a floor that delayed them would be
    /// delaying the end of a session to save a message.
    fn poll_with(&mut self, updates: bool) -> Vec<HostMessage> {
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

            // Not polled-and-discarded: `Session::poll` advances the
            // subscriber's baseline and the encoder shadow with it, so a
            // discarded return value is output the client can never be sent.
            // Not asking is what makes the floor lossless.
            // A session that has ended is never throttled, and this is the one
            // place the floor must look at a session rather than a clock.
            //
            // `Exited` is deliberately not delayed (see the doc above), so
            // skipping this session's delta while sending its `Exited` behind
            // it does not postpone the last screenful — it deletes it.
            // `zest-app`'s reader *returns out of its thread* on `Exited`
            // (`remote.rs`), so nothing after that message is ever decoded, and
            // the window closes on `Wakeup::Exited` having never shown what the
            // command printed last. A floor may delay output; it must never
            // reorder it past the end of the stream.
            let ended = session.has_exited();
            let update = if updates || ended { session.poll(handle) } else { None };
            match update {
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
                    blocks_from: k.blocks_from,
                    blocks: k.blocks,
                    title: k.title,
                }),
                None => {}
            }

            // The snapshot from above, not a fresh read, and the difference is
            // load-bearing. A session that exits *between* the two would be
            // reported here having had its delta skipped a few lines earlier —
            // which is exactly the ordering bug the snapshot was added to
            // prevent, reintroduced by asking twice.
            //
            // Nothing is lost by being one pass late: the exit that set this
            // flag is itself a wakeup, so the next pass polls with `ended`
            // true and sends the final delta and the `Exited` together.
            if ended {
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

    /// Serialize one outgoing message, sealed if this connection is past the
    /// `Challenge`.
    ///
    /// On `Connection` rather than free-standing because the nonce counter
    /// lives here and advances once per call, which makes call order part of
    /// the wire format: two frames sealed in one order and written in the other
    /// are two frames the peer cannot open. One writer thread is what keeps
    /// that true, and it is the reason this returns bytes instead of taking the
    /// writer -- holding the connection lock across a blocking `write_all` is
    /// the deadlock `ws.rs` documents.
    pub fn encode(&mut self, msg: &HostMessage) -> Result<Vec<u8>, DaemonError> {
        let body = frame::encode_body(msg).map_err(|e| DaemonError::Transport(e.to_string()))?;
        let body = match &mut self.seal {
            Some(s) if s.out => {
                s.channel.seal(&body).map_err(|e| DaemonError::Transport(e.to_string()))?
            }
            _ => body,
        };
        let out = frame::frame_bytes(&body).map_err(|e| DaemonError::Transport(e.to_string()))?;
        // The positional switch, on the way out: this frame carries the host's
        // DH key, so it is the last plaintext one. Flipped after sealing rather
        // than before, or the `Challenge` would be encrypted under a key the
        // client cannot have yet.
        if matches!(msg, HostMessage::Challenge { .. }) {
            if let Some(s) = &mut self.seal {
                s.out = true;
            }
        }
        Ok(out)
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
            ClientMessage::Hello { version, client, label, nonce, dh, watch_sessions } => {
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
                    zest_mesh::secure::DhPublic(dh.0),
                );
                match step {
                    zest_mesh::pairing::HostStep::Challenge { nonce, dh, signature } => {
                        tracing::debug!(client = %client.short(), %label, "challenging");
                        // The host id and label come from the *transcript*, not
                        // from `config`. They are inside the signature, so a
                        // second source for either is a signature no client can
                        // verify -- which presents as "did not prove its
                        // identity" and sends whoever is debugging it looking
                        // at the crypto rather than at the two fields.
                        let t = h.transcript().expect("challenged");
                        let challenge = HostMessage::Challenge {
                            version: t.version,
                            host: t.host,
                            label: t.host_label.clone(),
                            nonce: zest_proto::Nonce32::from_bytes(*nonce.as_bytes()),
                            dh: zest_proto::Pub32::from_bytes(dh.0),
                            signature: zest_proto::Sig64::from_bytes(signature.to_bytes()),
                        };

                        // Derived here, not on `Welcome`: the client's `Auth` is
                        // already sealed, so the host needs the key before it
                        // can read the very signature that decides whether to
                        // serve. That is not trust granted early -- opening the
                        // `Auth` proves only that whoever completed the DH sent
                        // it, and `on_auth` still decides who gets a shell.
                        let channel = h.channel();
                        match channel {
                            Some(Ok(channel)) => self.seal = Some(Seal { channel, out: false }),
                            Some(Err(e)) => {
                                // The transcript was signed and the key still
                                // would not agree, which is not a peer problem.
                                // Serving on would mean serving in plaintext
                                // while both sides believe otherwise.
                                tracing::error!(error = %e, "could not derive this connection's key");
                                return self.refuse(
                                    zest_mesh::pairing::Refusal::Signature,
                                    client,
                                );
                            }
                            None => {
                                tracing::error!("a challenged handshake with no channel");
                                return self.refuse(
                                    zest_mesh::pairing::Refusal::Signature,
                                    client,
                                );
                            }
                        }
                        vec![challenge]
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
                    blocks_from: keyframe.blocks_from,
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
                        blocks_from: k.blocks_from,
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
    let floor = config.min_delta_interval;
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

    // The coalescing floor, kept here rather than in `Connection` because it is
    // a property of this loop's *timing* and nothing the protocol can see.
    // With `min_delta_interval` at zero — every transport that pays nothing per
    // message, which is loopback and the LAN — no poll is ever skipped, so
    // `owed_at` stays `None`, the `rx.recv()` arm below is the only one taken,
    // and the loop behaves exactly as it did before this existed.
    let mut last_update: Option<Instant> = None;
    let mut owed_at: Option<Instant> = None;
    loop {
        let wake = match owed_at {
            // A poll was skipped, so the wake that releases it has to come from
            // a timer: the session's waker has already fired for the output
            // being held back and will not fire again until there is *more* —
            // which a command that has finished printing never produces. Without
            // this the last frame of every burst waits for the next keystroke.
            Some(at) => match rx.recv_timeout(at.saturating_duration_since(Instant::now())) {
                Ok(w) => w,
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => Wake::Poll,
                Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => return Ok(()),
            },
            None => match rx.recv() {
                Ok(w) => w,
                Err(_) => return Ok(()),
            },
        };
        let mut outgoing = match wake {
            Wake::Closed => return Ok(()),
            Wake::Send(msgs) => msgs,
            Wake::Poll => Vec::new(),
        };
        let now = Instant::now();
        let updates = floor.is_zero()
            || last_update.is_none_or(|sent| now.duration_since(sent) >= floor);
        let replies = outgoing.len();
        {
            let mut c = conn.lock().expect("connection lock");
            // A pairing decision may have arrived while this connection was
            // waiting. It is delivered through the same wake the writer is
            // already blocked on, so nothing polls and no lock is held across
            // however long someone took to answer.
            outgoing.extend(c.take_decision());
            outgoing.extend(c.poll_with(updates));
        }
        if updates {
            owed_at = None;
            // Only what the *poll* produced restarts the interval. A keyframe
            // answering `Attach` or `RequestKeyframe` is a reply, and letting a
            // reply push the floor out would delay the resync it is part of.
            if outgoing[replies..].iter().any(|m| {
                matches!(m, HostMessage::Update { .. } | HostMessage::Keyframe { .. })
            }) {
                last_update = Some(now);
            }
        } else {
            owed_at = last_update.map(|sent| sent + floor);
        }
        if outgoing.is_empty() {
            continue;
        }

        for msg in outgoing {
            // The lock is taken per message and released before the write:
            // sealing must be ordered with respect to the wire, and holding it
            // across a blocking `write_all` is the deadlock `ws.rs` documents.
            // One writer thread is what makes per-message locking sufficient.
            let bytes = { conn.lock().expect("connection lock").encode(&msg)? };
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
    fn authenticate(c: &mut Connection) -> Peer {
        authenticate_with(c, false)
    }

    fn authenticate_with(c: &mut Connection, watch_sessions: bool) -> Peer {
        let client =
            std::sync::Arc::new(zest_mesh::identity::ClientIdentity::generate().expect("client key"));
        // The shared client handshake, not a hand-rolled one: a test peer that
        // derived its key differently would fail every frame *after* the
        // handshake, which reads as a broken daemon rather than a broken test.
        let mut hs = zest_mesh::pairing::ClientHandshake::new(
            std::sync::Arc::clone(&client),
            "test",
        )
        .expect("client handshake");

        let out = send(
            c,
            &ClientMessage::Hello {
                version: PROTOCOL_VERSION,
                client: client.client_id(),
                label: "test".into(),
                nonce: zest_proto::Nonce32::from_bytes(*hs.nonce().as_bytes()),
                dh: zest_proto::Pub32::from_bytes(hs.dh().0),
                watch_sessions,
            },
        );
        let [HostMessage::Challenge { nonce, host, label, version, dh, signature }] = &out[..]
        else {
            panic!("expected a challenge, got {out:?}");
        };
        let host_sig =
            zest_mesh::identity::Signature::from_slice(&signature.0).expect("a 64-byte signature");
        let (sig, _, channel) = hs
            .on_challenge(
                None,
                &zest_mesh::pairing::Challenge {
                    version: *version,
                    host: *host,
                    label: label.clone(),
                    nonce: zest_mesh::identity::Nonce::from_bytes(nonce.0),
                    dh: zest_mesh::secure::DhPublic(dh.0),
                    signature: host_sig,
                },
            )
            .expect("the host must prove itself");

        let mut peer = Peer { channel };
        let out = peer.send(
            c,
            &ClientMessage::Auth { signature: zest_proto::Sig64::from_bytes(sig.to_bytes()) },
        );
        assert!(
            matches!(&out[..], [HostMessage::Welcome { .. }]),
            "loopback must welcome any proved client, got {out:?}"
        );
        peer
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
            min_delta_interval: Duration::ZERO,
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
        let _watcher_peer = authenticate_with(&mut watcher, true);

        let mut creator = Connection::new(
            config(),
            Arc::clone(&registry),
            crate::auth::Auth::Transport(test_authenticator()),
            "test2",
        );
        let mut creator_peer = authenticate(&mut creator);
        let out = creator_peer.send(
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
        let mut peer = authenticate(&mut c);
        let out = peer.send(
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

    /// Feed one message in as plaintext.
    ///
    /// Correct only before the `Challenge`. After it the daemon expects sealed
    /// frames, which is what [`Peer`] is for -- and the tests that still use
    /// this are exactly the ones about refusing a connection before it ever
    /// gets a channel.
    fn send(c: &mut Connection, msg: &ClientMessage) -> Vec<HostMessage> {
        let bytes = frame::encode(msg).expect("encode");
        c.on_bytes(&bytes).expect("handled")
    }

    /// An authenticated client, holding the channel its frames are sealed with.
    ///
    /// These tests drive `Connection` in memory, so they exercise the *inbound*
    /// seal only -- `Connection::encode` is the outbound half and is covered
    /// over real sockets in `tests/lan.rs` and `tests/ws.rs`. Worth knowing
    /// before trusting a green run here to mean the wire is right.
    struct Peer {
        channel: zest_mesh::secure::SecureChannel,
    }

    impl Peer {
        fn send(&mut self, c: &mut Connection, msg: &ClientMessage) -> Vec<HostMessage> {
            let body = frame::encode_body(msg).expect("encode");
            let sealed = self.channel.seal(&body).expect("seal");
            let bytes = frame::frame_bytes(&sealed).expect("frame");
            c.on_bytes(&bytes).expect("handled")
        }

        /// Seal bytes that are not a `ClientMessage`.
        ///
        /// For the one case that needs it: a message this build cannot parse
        /// but whose sender does hold the key.
        fn send_body(
            &mut self,
            c: &mut Connection,
            body: &[u8],
        ) -> Result<Vec<HostMessage>, DaemonError> {
            let sealed = self.channel.seal(body).expect("seal");
            c.on_bytes(&frame::frame_bytes(&sealed).expect("frame"))
        }
    }

    /// Get as far as a channel without proving anything.
    ///
    /// For tests about a *failed* `Auth`: the channel exists from the
    /// `Challenge` onwards, so a peer that signs badly still has to seal.
    fn challenge_only(c: &mut Connection) -> Peer {
        let client =
            std::sync::Arc::new(zest_mesh::identity::ClientIdentity::generate().expect("client key"));
        let mut hs =
            zest_mesh::pairing::ClientHandshake::new(std::sync::Arc::clone(&client), "test")
                .expect("client handshake");
        let out = send(
            c,
            &ClientMessage::Hello {
                version: PROTOCOL_VERSION,
                client: client.client_id(),
                label: "test".into(),
                nonce: zest_proto::Nonce32::from_bytes(*hs.nonce().as_bytes()),
                dh: zest_proto::Pub32::from_bytes(hs.dh().0),
                watch_sessions: false,
            },
        );
        let [HostMessage::Challenge { nonce, host, label, version, dh, signature }] = &out[..]
        else {
            panic!("expected a challenge, got {out:?}");
        };
        let host_sig =
            zest_mesh::identity::Signature::from_slice(&signature.0).expect("a 64-byte signature");
        let (_, _, channel) = hs
            .on_challenge(
                None,
                &zest_mesh::pairing::Challenge {
                    version: *version,
                    host: *host,
                    label: label.clone(),
                    nonce: zest_mesh::identity::Nonce::from_bytes(nonce.0),
                    dh: zest_mesh::secure::DhPublic(dh.0),
                    signature: host_sig,
                },
            )
            .expect("the host must prove itself");
        Peer { channel }
    }

    fn hello() -> ClientMessage {
        ClientMessage::Hello {
            version: PROTOCOL_VERSION,
            client: ClientId::from_bytes([1; 32]),
            label: "test".into(),
            nonce: zest_proto::Nonce32::from_bytes([6; 32]),
            dh: zest_proto::Pub32::from_bytes([8; 32]),
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

    /// A child that prints *after* the client has had time to attach.
    ///
    /// The delay is the whole point: with a plain `echo` the output is already
    /// in the terminal when `Attach` builds its keyframe, so a test about what
    /// a later poll produces would be asserting on nothing.
    fn delayed_echo_cmd() -> String {
        if cfg!(windows) {
            "powershell.exe -NoProfile -Command Start-Sleep -Milliseconds 300; \
             Write-Output probe"
                .into()
        } else {
            "/bin/sh -c 'sleep 0.3; echo probe'".into()
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
        let mut peer = authenticate(&mut c);
        let out = peer.send(&mut c, &ClientMessage::ListSessions);
        assert!(matches!(&out[..], [HostMessage::Sessions { .. }]), "{out:?}");
    }

    #[test]
    fn an_unsigned_client_is_never_served() {
        // Answering the challenge with noise must not get anywhere, and must
        // say why in a form a client can branch on.
        // A *sealed* bad signature, which is the case that still has to be
        // answered by name. Since protocol 3 the `Auth` is encrypted, so a peer
        // that completed the DH and then signed wrongly is a different failure
        // from one that could not seal at all -- and only the first can be told
        // anything, because the second's bytes never became a message.
        let (mut c, _) = conn();
        let mut peer = challenge_only(&mut c);

        let out = peer.send(
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
        let out = peer.send(&mut c, &ClientMessage::ListSessions);
        assert!(matches!(&out[..], [HostMessage::Error { .. }]), "{out:?}");
    }

    #[test]
    fn an_auth_that_was_not_sealed_ends_the_connection() {
        // The other half, and the reason the one above had to change. The seal
        // switch is positional: the `Auth` is the first sealed frame, so a
        // plaintext one is not a client with a bad signature -- it is a peer
        // that does not hold the key, or bytes something on the path rewrote.
        // There is nothing to reply to it, and replying anyway would be a
        // decryption oracle answering questions about frames it could not read.
        let (mut c, _) = conn();
        let out = send(&mut c, &hello());
        assert!(matches!(&out[..], [HostMessage::Challenge { .. }]), "{out:?}");

        let bytes = frame::encode(&ClientMessage::Auth {
            signature: zest_proto::Sig64::from_bytes([0; 64]),
        })
        .expect("encode");
        assert!(
            c.on_bytes(&bytes).is_err(),
            "a plaintext frame after the Challenge must end the connection, not be answered"
        );
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
        let mut peer = authenticate(&mut c);
        peer.send(&mut c, &ClientMessage::CreateSession {
            command: echo_cmd(),
            cwd: String::new(),
            cols: 80,
            rows: 24,
        });
        let addr = SessionAddr::new(config().host, SessionId(1));
        peer.send(&mut c, &ClientMessage::Attach { session: addr, cols: 80, rows: 24 });

        let out = peer.send(&mut c, &ClientMessage::RequestKeyframe { session: addr });
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
                dh: zest_proto::Pub32::from_bytes([8; 32]),
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
        let mut peer = authenticate(&mut c);

        // Sealed junk, not raw junk -- and the distinction is the point. A
        // newer client's unknown message arrives *encrypted correctly*, because
        // it holds the key; only its contents are unfamiliar. Raw junk means
        // something that does not hold the key is writing to this socket, which
        // is the case below and is fatal.
        let out = peer.send_body(&mut c, b"junk").expect("an unknown message is not fatal");
        assert!(matches!(&out[..], [HostMessage::Error { .. }]), "{out:?}");

        // ...and the connection still works.
        let after = peer.send(&mut c, &ClientMessage::ListSessions);
        assert!(matches!(&after[..], [HostMessage::Sessions { .. }]), "{after:?}");
    }

    #[test]
    fn a_frame_that_does_not_open_is_fatal() {
        // The counterpart to the test above, and the line between them: an
        // unparseable *plaintext* is a client this build is older than, while a
        // body that will not open is tampering or a key disagreement. The
        // counter has already advanced by the time it is known, so there is no
        // position to resume from -- reading on would decrypt every later frame
        // under the wrong nonce and report the damage several frames away from
        // its cause.
        let (mut c, _) = conn();
        let _peer = authenticate(&mut c);

        let mut junk = Vec::new();
        junk.extend_from_slice(&(4u32).to_le_bytes());
        junk.extend_from_slice(b"junk");
        assert!(c.on_bytes(&junk).is_err(), "a frame that does not open must end the connection");
    }

    #[test]
    fn creating_a_session_lists_it() {
        let (mut c, registry) = conn();
        let mut peer = authenticate(&mut c);

        let out = peer.send(
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
        let mut peer = authenticate(&mut c);
        peer.send(
            &mut c,
            &ClientMessage::CreateSession {
                command: echo_cmd(),
                cwd: String::new(),
                cols: 80,
                rows: 24,
            },
        );
        let addr = registry.list(config().host)[0].addr;

        let out = peer.send(&mut c, &ClientMessage::Attach { session: addr, cols: 80, rows: 24 });
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
        let mut peer = authenticate(&mut c);
        peer.send(
            &mut c,
            &ClientMessage::CreateSession {
                command: echo_cmd(),
                cwd: String::new(),
                cols: 80,
                rows: 24,
            },
        );
        let addr = registry.list(config().host)[0].addr;
        peer.send(&mut c, &ClientMessage::Attach { session: addr, cols: 80, rows: 24 });

        peer.send(&mut c, &ClientMessage::Detach { session: addr });
        assert_eq!(registry.len(), 1, "detaching removed the session");
        assert!(registry.get(addr.session).is_some());
    }

    #[test]
    fn closing_a_session_removes_it() {
        let (mut c, registry) = conn();
        let mut peer = authenticate(&mut c);
        peer.send(
            &mut c,
            &ClientMessage::CreateSession {
                command: echo_cmd(),
                cwd: String::new(),
                cols: 80,
                rows: 24,
            },
        );
        let addr = registry.list(config().host)[0].addr;

        peer.send(&mut c, &ClientMessage::CloseSession { session: addr });
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
        let mut peer = authenticate(&mut c);
        peer.send(
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

        peer.send(&mut c, &ClientMessage::CloseSession { session: addr });

        assert!(
            wait_for(|| session.has_exited()),
            "the child outlived CloseSession. Removing the registry entry does \
             not end anything: the pty's reader is parked holding the master \
             open, so nothing closes and the shell runs until the daemon does"
        );
    }

    /// A shell that exits on its own must not be kept forever.
    ///
    /// **This ran on unix only until `ConPty::watch_exit` landed**, and the
    /// gap it was covering for was real: `has_exited` is driven by the reader
    /// thread ending, which on Windows cannot happen, because ConPTY holds the
    /// output pipe's write end until the pseudoconsole closes and a blocked
    /// `ReadFile` stays blocked after the shell is gone (windows.rs gotcha
    /// 2b). No `Exited` was sent there and nothing was ever swept. → #18.
    ///
    /// It is now a cross-platform test, and on Windows it is the acceptance
    /// criterion for the watcher: it fails if the exit is never reported, and
    /// it fails if the session is swept too eagerly. Sweeping early is its own
    /// bug — a client that never learns its shell exited waits for output that
    /// is not coming.
    #[test]
    fn an_exited_session_is_kept_until_nobody_is_watching() {
        let (mut c, registry) = conn();
        let mut peer = authenticate(&mut c);
        peer.send(
            &mut c,
            &ClientMessage::CreateSession {
                command: echo_cmd(),
                cwd: String::new(),
                cols: 80,
                rows: 24,
            },
        );
        let addr = registry.list(config().host)[0].addr;
        peer.send(&mut c, &ClientMessage::Attach { session: addr, cols: 80, rows: 24 });

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

        peer.send(&mut c, &ClientMessage::Detach { session: addr });
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
    ///
    /// Cross-platform since `ConPty::watch_exit`: it was unix-only for the
    /// same reason as the test above, not because the race is unix's.
    #[test]
    fn a_session_is_not_swept_before_anyone_has_attached() {
        let (mut c, registry) = conn();
        let mut peer = authenticate(&mut c);
        peer.send(
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
        let out = peer.send(&mut c, &ClientMessage::Attach { session: addr, cols: 80, rows: 24 });
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
            let mut peer = authenticate(&mut c);
            peer.send(
                &mut c,
                &ClientMessage::CreateSession {
                    command: sleep_cmd(),
                    cwd: String::new(),
                    cols: 80,
                    rows: 24,
                },
            );
            let addr = registry.list(config().host)[0].addr;
            peer.send(&mut c, &ClientMessage::Attach { session: addr, cols: 80, rows: 24 });
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
        let mut peer = authenticate(&mut c);
        let addr = SessionAddr::new(config().host, SessionId(999));
        let out = peer.send(&mut c, &ClientMessage::Input { session: addr, bytes: vec![b'x'] });
        assert!(matches!(&out[..], [HostMessage::Error { .. }]), "{out:?}");
    }

    #[test]
    fn a_listing_keeps_a_stable_order() {
        // A list that reshuffles between polls is unusable on a phone.
        let (mut c, registry) = conn();
        let mut peer = authenticate(&mut c);
        for _ in 0..4 {
            peer.send(
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
        let mut peer = authenticate(&mut c);
        peer.send(
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

    /// The two rules a message-rate test cannot see, at the layer that decides
    /// them. `tests/coalescing.rs` measures what the floor is *for*; this is
    /// what it is not allowed to cost.
    ///
    /// A floor that delays the news of a shell exiting is a client waiting for
    /// output that is never coming, and a floor implemented by polling the
    /// session and dropping the answer is output destroyed rather than
    /// coalesced — the encoder shadow advances with every poll, so a discarded
    /// return value can never be re-fetched.
    #[test]
    fn an_ended_session_sends_its_last_output_in_front_of_the_exit() {
        let (mut c, registry) = conn();
        let mut peer = authenticate(&mut c);
        peer.send(
            &mut c,
            &ClientMessage::CreateSession {
                command: delayed_echo_cmd(),
                cwd: String::new(),
                cols: 80,
                rows: 24,
            },
        );
        let addr = registry.list(config().host)[0].addr;
        let session = registry.get(addr.session).expect("just created");
        peer.send(&mut c, &ClientMessage::Attach { session: addr, cols: 80, rows: 24 });

        // Exit implies the reader drained the pty, so everything the child
        // printed is in the terminal — and it printed all of it after the
        // attach above, so none of it is in that keyframe.
        assert!(wait_for(|| session.has_exited()), "the child never exited");

        let held = c.poll_with(false);
        assert!(
            held.iter().any(|m| matches!(m, HostMessage::Exited { .. })),
            "the floor held back the end of a session. A client that is never told \
             its shell exited waits for output that will never come, and no saving \
             is worth that: {held:?}"
        );
        // And its last output rides in front of that `Exited`, throttled or not.
        //
        // This assertion replaces its own opposite. The first version of this
        // test required a skipped poll to send `Exited` with *no* update, which
        // reads like a floor doing its job and is a lost screenful: `zest-app`'s
        // reader returns out of its thread on `Exited` (`remote.rs`), so a delta
        // held back behind one is never applied by anybody, and the window
        // closes having never shown what the command last printed. There is no
        // later pass to release it to — the exit is the end of the stream.
        let update_at = held
            .iter()
            .position(|m| matches!(m, HostMessage::Update { .. } | HostMessage::Keyframe { .. }));
        let exited_at = held.iter().position(|m| matches!(m, HostMessage::Exited { .. }));
        assert!(
            update_at.is_some(),
            "an ended session's final output was throttled away. The floor may delay \
             output; past an `Exited` there is nothing left to delay it until: {held:?}"
        );
        assert!(
            update_at < exited_at,
            "the last delta came out behind the `Exited` that ends the stream, so no \
             client will ever apply it: {held:?}"
        );
    }
}
