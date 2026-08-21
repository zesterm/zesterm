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
    /// An `EnrollResult` a worker settled while this connection waited —
    /// `decided`'s shape for `decided`'s reason: the enrolment is a keychain
    /// probe and an HTTPS round trip, and the reader thread holds this
    /// connection's lock across `on_bytes`, so the work happens off-thread
    /// and the answer rides the wake the writer is already blocked on.
    enroll_result: Arc<Mutex<Option<HostMessage>>>,
    /// The mailbox above holds exactly one answer, so this connection runs
    /// exactly one enrolment at a time: set when a worker is spawned,
    /// cleared only when its answer has been *drained* for delivery — a
    /// second `Enroll` in between would otherwise overwrite a result nobody
    /// has read yet, and one caller would wait for ever on an answer the
    /// other received. A second ask is refused with an honest
    /// `EnrollResult`; enrolment is a one-shot human act, and the app's
    /// button disables itself in flight anyway.
    enroll_running: bool,
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
    /// This client asked (`Hello.watch_hosts`) what this machine offers, and
    /// to be told when that changes (#262).
    watch_hosts: bool,
    /// This client asked (`Hello.watch_signals`) to be told when a session it
    /// is attached to rings or notifies.
    ///
    /// The gate on `HostMessage::Attention`, and it is not politeness: a tag
    /// an older client cannot decode ends its connection. The session fills
    /// every subscriber's slot regardless — it has no idea who asked — and
    /// this is where the answer is dropped for a client that did not.
    watch_signals: bool,
    /// The offer generation this connection last told its client about. `0`
    /// means "has never sent one", and `OfferSource` starts at 1, so a
    /// subscriber's very first `Sessions` carries the offer without needing a
    /// separate "send it once" flag.
    seen_offer_generation: u64,
    /// Registration in [`crate::offer::OfferSource::watch`], for `Drop` to
    /// release. Without it a config edit moves the generation and then waits
    /// for an unrelated event to carry it — the serve loop blocks until
    /// something wakes it.
    offer_watch_token: Option<u64>,
    /// Registration in [`Registry::watch`], for `Drop` to release.
    watch_token: Option<u64>,
    /// The registry generation this connection last told its client about.
    seen_generation: u64,
    /// This client asked (`Hello.watch_pairings`) to hear about devices
    /// waiting for approval. Honoured only where `may_approve_devices` says
    /// yes — the loopback socket — so a LAN connection that asks is silently
    /// never subscribed, exactly like its `PairingDecision` would be refused.
    watch_pairings: bool,
    /// Registration in `PairingQueue::watch`, for `Drop` to release.
    pairing_watch_token: Option<u64>,
    /// The queue generation this connection last pushed from. Left at 0 on
    /// subscribe *on purpose*: unlike sessions, the client cannot list the
    /// queue on demand, so anything already pending must be replayed by the
    /// first poll rather than assumed seen.
    seen_pairing_generation: u64,
    /// Which clients this connection has announced and not yet tombstoned —
    /// the diff state that turns "the queue changed" into "show this" /
    /// "stop showing that". Keyed by client because the queue resolves by
    /// client: a device that retried is one prompt, not two.
    announced_pairings: std::collections::HashSet<ClientId>,
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
        if let Some(token) = self.pairing_watch_token.take() {
            self.auth.authenticator().queue().unwatch(token);
        }
        if let Some(token) = self.offer_watch_token.take() {
            if let Some(source) = self.config.offer.as_ref() {
                source.unwatch(token);
            }
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
            enroll_result: Arc::new(Mutex::new(None)),
            enroll_running: false,
            on_ready: None,
            on_pending: None,
            waker: None,
            watch_sessions: false,
            watch_hosts: false,
            watch_signals: false,
            seen_offer_generation: 0,
            offer_watch_token: None,
            watch_token: None,
            seen_generation: 0,
            watch_pairings: false,
            pairing_watch_token: None,
            seen_pairing_generation: 0,
            announced_pairings: std::collections::HashSet::new(),
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

    /// This machine's offer, if this connection asked for it and has not
    /// already been told this version of it (#262).
    ///
    /// Three ways to get `None`, and they are deliberately one branch on the
    /// client: this connection did not set `Hello.watch_hosts`, this daemon
    /// publishes no offer at all (`DaemonConfig::offer` is `None`), or nothing
    /// has changed since the last time. A client that predates the field, and
    /// a daemon that does, both land here too — so "no offer on this message"
    /// has exactly one meaning everywhere: *nothing new to say*.
    ///
    /// Marks as sent on the way out. That is a side effect in a getter, which
    /// buys the honesty of the alternative: every caller pushes the message it
    /// returns into `out`, and a separate "now mark it" call is a line one of
    /// the three would eventually forget — resending the whole profile list on
    /// every session poll.
    fn offer_if_new(&mut self) -> Option<zest_proto::HostOffer> {
        if !self.watch_hosts {
            return None;
        }
        let source = self.config.offer.as_ref()?;
        let generation = source.generation();
        if generation == self.seen_offer_generation {
            return None;
        }
        self.seen_offer_generation = generation;
        Some(source.snapshot())
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
                    offer: self.offer_if_new(),
                });
            }
        }
        // An offer that changed with no session change of its own still has to
        // reach a subscriber — a config edit moves the profile list and moves
        // nothing in the registry, and without this the far launcher would show
        // the old rows until somebody happened to open a shell.
        if self.watch_hosts && matches!(self.gate, Gate::Served) {
            if let Some(offer) = self.offer_if_new() {
                out.push(HostMessage::Sessions {
                    sessions: self.registry.list(self.config.host),
                    created: None,
                    offer: Some(offer),
                });
            }
        }
        // The approval pushes, same coalescing shape: however many queue
        // changes piled up, one diff against what this connection already
        // announced says "show this" (a request, with its remaining
        // validity) and "stop showing that" (a tombstone, `resolved: true`,
        // carrying only the client — there is nothing left to compare).
        if self.watch_pairings
            && self.auth.may_approve_devices()
            && matches!(self.gate, Gate::Served)
        {
            let queue = self.auth.authenticator().queue();
            let generation = queue.generation();
            if generation != self.seen_pairing_generation {
                self.seen_pairing_generation = generation;
                let pending = queue.pending();
                let now = std::time::Instant::now();
                for r in &pending {
                    if self.announced_pairings.insert(r.client) {
                        let left = zest_mesh::pairing::APPROVAL_TIMEOUT
                            .saturating_sub(now.saturating_duration_since(r.requested_at));
                        out.push(HostMessage::PairingRequested {
                            client: r.client,
                            label: r.label.clone(),
                            code: r.code.clone(),
                            remote: r.remote.clone(),
                            expires_in_secs: pairing_expiry_secs(left),
                            resolved: false,
                        });
                    }
                }
                self.announced_pairings.retain(|c| {
                    if pending.iter().any(|r| r.client == *c) {
                        return true;
                    }
                    out.push(HostMessage::PairingRequested {
                        client: *c,
                        label: String::new(),
                        code: String::new(),
                        remote: String::new(),
                        expires_in_secs: 0,
                        resolved: true,
                    });
                    false
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
                    history_clears: k.history_clears,
                }),
                None => {}
            }

            // Outside the `updates || ended` throttle above, and not folded
            // into `poll`: a session that is idle produces no delta at all, so
            // a bell folded into the update path would arrive behind output
            // that may never come. The coalescing floor exists to spare the
            // *bandwidth* of a busy grid; one signal is two words.
            // Drained whether or not this client asked, so a slot cannot sit
            // filled for the life of a connection that will never read it.
            if let Some(cause) = session.take_attention(handle) {
                if self.watch_signals {
                    out.push(HostMessage::Attention { session: addr, cause });
                }
            }
            // And the shadow advances either way, for the same reason: a
            // client that did not ask is not owed the message, but it is also
            // not owed every tick it missed the moment anything changes.
            if let Some(progress) = session.progress_for(handle) {
                if self.watch_signals {
                    out.push(HostMessage::Progress { session: addr, progress });
                }
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
                // `code` was hard-coded `None` from protocol 2 until #299,
                // which made every client's decoder read a field nothing ever
                // filled — indistinguishable from a host that genuinely could
                // not determine a status, and it silently cost `zest-mcp` the
                // one unforgeable exit code it exists to report.
                //
                // Asked here rather than snapshotted with `ended`: the reader
                // sets that flag on EOF, and seeing EOF is not the same as
                // having waited on the process. Asking a moment later is what
                // gives the status time to exist; `Session::exit_code` memoizes
                // so re-asking on each pass is free.
                out.push(HostMessage::Exited { session: addr, code: session.exit_code() });
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
            ClientMessage::Hello {
                version,
                client,
                label,
                nonce,
                dh,
                watch_sessions,
                watch_pairings,
                watch_hosts,
                watch_signals,
            } => {
                self.watch_sessions = watch_sessions;
                self.watch_pairings = watch_pairings;
                self.watch_hosts = watch_hosts;
                self.watch_signals = watch_signals;
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

            ClientMessage::Enroll { code } => {
                // Loopback only, `PairingDecision`'s gate verbatim: joining
                // the machine to an account is the authority of whoever is
                // logged in at it.
                if !self.auth.may_approve_devices() {
                    tracing::warn!(
                        remote = %self.remote,
                        "a remote connection tried to enroll this machine"
                    );
                    return vec![HostMessage::Error {
                        session: None,
                        message: "only a local client may enroll this machine".into(),
                    }];
                }
                let Some(seam) = self.config.enroll.clone() else {
                    // An --ephemeral daemon, honestly: its key dies with the
                    // process, so an account row for it would name a host
                    // nobody can ever reach (the `--enroll` flag refuses for
                    // the same reason).
                    return vec![HostMessage::EnrollResult {
                        ok: false,
                        account: None,
                        message: "this daemon cannot enroll: its key is ephemeral \
                                  (start it without --ephemeral, or run \
                                  zest-daemon --enroll <code>)"
                            .into(),
                    }];
                };
                if self.enroll_running {
                    return vec![HostMessage::EnrollResult {
                        ok: false,
                        account: None,
                        message: "an enrolment is already running; wait for it to finish"
                            .into(),
                    }];
                }
                self.enroll_running = true;
                let identity = Arc::clone(self.auth.authenticator().identity());
                let label = self.auth.authenticator().label().to_string();
                let cell = Arc::clone(&self.enroll_result);
                let waker = self.waker.clone();
                let offer = self.config.offer.clone();
                let spawned = std::thread::Builder::new().name("zest-enroll".into()).spawn(
                    move || {
                        let outcome = crate::enroll::enroll(
                            &identity,
                            &code,
                            &label,
                            &seam.base_url,
                            seam.http.as_ref(),
                            seam.secrets.as_ref(),
                        );
                        let msg = match outcome {
                            Ok(enrolled) => {
                                // The token just landed in the store, so the
                                // published fact flips with it (#245): the
                                // generation moves, the watchers wake, and
                                // every fleet card learns this machine no
                                // longer needs enrolling — without waiting
                                // for the account listing to catch up.
                                if let Some(source) = &offer {
                                    source.set_account_token(Some(true));
                                }
                                HostMessage::EnrollResult {
                                    ok: true,
                                    account: enrolled.account,
                                    message: String::new(),
                                }
                            }
                            // Rendered as the CLI renders it — `refusal_text`
                            // is what `--enroll` prints — because the message
                            // is the person's next move and the app shows it
                            // verbatim (#368).
                            Err(e) => HostMessage::EnrollResult {
                                ok: false,
                                account: None,
                                message: crate::enroll::refusal_text(&e),
                            },
                        };
                        *cell.lock().expect("enroll lock") = Some(msg);
                        if let Some(w) = &waker {
                            w();
                        }
                    },
                );
                if let Err(e) = spawned {
                    // No worker ever ran, so nothing will be drained; free
                    // the slot here or this connection could never try again.
                    self.enroll_running = false;
                    return vec![HostMessage::EnrollResult {
                        ok: false,
                        account: None,
                        message: format!("no thread for the enrolment: {e}"),
                    }];
                }
                Vec::new()
            }

            ClientMessage::ListSessions => {
                vec![HostMessage::Sessions {
                    sessions: self.registry.list(self.config.host),
                    created: None,
                    offer: self.offer_if_new(),
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
                            offer: self.offer_if_new(),
                        }]
                    }
                    Err(e) => vec![HostMessage::Error {
                        session: None,
                        message: format!("could not start a session: {e}"),
                    }],
                }
            }

            ClientMessage::Attach { session, cols, rows, observe } => {
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
                // The ask is a vote, not a command: the session sizes itself to
                // the smallest attached client (#215), and the reply keyframe
                // carries whatever was granted.
                //
                // An observer abstains. `attach_with` has always taken an
                // `Option` and `reconcile_size` has always skipped a `None`;
                // what did not exist was a way to say so from the wire, so a
                // client with no pane had to invent a size and pin the session
                // at it for ever (#274). The keyframe is unaffected either way
                // -- abstaining is about the arbitration, not the subscription.
                let waker = self.waker.clone();
                let (handle, seq, keyframe) = s.attach_with(
                    Box::new(move || {
                        if let Some(w) = &waker {
                            w();
                        }
                    }),
                    (!observe).then_some((cols, rows)),
                );
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
                    history_clears: keyframe.history_clears,
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
                        history_clears: k.history_clears,
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
                // Names this connection's *attachment*, not the session: only
                // an attached client has a pane worth arbitrating over, so a
                // Resize from a connection that never attached is ignored.
                // Both shipped clients attach before they ever resize.
                if let (Some(s), Some(&handle)) =
                    (self.registry.get(session.session), self.attached.get(&session.session.0))
                {
                    if s.set_client_size(handle, cols, rows) {
                        // `SessionInfo` carries cols/rows, so the listing rows
                        // a watcher holds just went stale.
                        self.registry.touch();
                    }
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
        // The offer subscription. Unlike sessions, `seen_offer_generation` is
        // deliberately *not* snapped to current: a subscriber's first message
        // must carry the offer, or a launcher sees this machine's profiles
        // only after somebody edits them.
        if self.watch_hosts && self.offer_watch_token.is_none() {
            if let (Some(source), Some(waker)) = (self.config.offer.clone(), self.waker.clone()) {
                self.offer_watch_token = Some(source.watch(waker));
            }
        }
        // The approval-modal subscription, gated by the transport: only a
        // connection that could answer (`may_approve_devices`) is told what
        // is waiting, so the codes never leave the machine. Unlike sessions,
        // `seen_pairing_generation` is not snapped to current here — a
        // request already waiting when the app connects must be replayed by
        // the first poll, or a modal only ever shows for requests that
        // arrive while the app happens to be running.
        if self.watch_pairings
            && self.auth.may_approve_devices()
            && self.pairing_watch_token.is_none()
        {
            if let Some(waker) = self.waker.clone() {
                self.pairing_watch_token =
                    Some(self.auth.authenticator().queue().watch(waker));
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

    /// The enrolment outcome, if a worker settled one. Writer-loop drained,
    /// like [`Self::take_decision`]; draining is also what frees the
    /// one-answer mailbox for the next `Enroll`.
    pub fn take_enroll_result(&mut self) -> Vec<HostMessage> {
        let taken = self.enroll_result.lock().expect("enroll lock").take();
        if taken.is_some() {
            self.enroll_running = false;
        }
        taken.into_iter().collect()
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
                    // `Err(_) => break` here treated a signal as the end of the
                    // stream, which closes a healthy peer or ends a live shell.
                    let n = match crate::read_retrying(&mut reader, &mut buf) {
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
            outgoing.extend(c.take_enroll_result());
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

/// A live request's remaining validity, for the wire.
///
/// Ceiled, and never 0: the wire reserves `0` for "expiry unknown" (a daemon
/// predating the field) and for tombstones, and the client answers unknown by
/// assuming the full pairing window — so a request with 400ms left that
/// truncated to 0 would keep a dead code on screen for minutes. A request
/// still in the queue is still answerable, which is what `max(1)` claims.
fn pairing_expiry_secs(left: std::time::Duration) -> u32 {
    let ceiled = left.as_secs() + u64::from(left.subsec_nanos() > 0);
    u32::try_from(ceiled.max(1)).unwrap_or(u32::MAX)
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
        authenticate_identity(c, &client, watch_sessions, false, false)
    }

    /// The full handshake for a caller that needs to pick the identity (to
    /// pre-trust it) or to watch the approval queue or the host's offer.
    fn authenticate_identity(
        c: &mut Connection,
        client: &std::sync::Arc<zest_mesh::identity::ClientIdentity>,
        watch_sessions: bool,
        watch_pairings: bool,
        watch_hosts: bool,
    ) -> Peer {
        let client = std::sync::Arc::clone(client);
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
                watch_pairings,
                watch_hosts,
                // The tests that need signals set it on the connection
                // directly; every other one must look like a client that
                // never asked, which is the case the gate exists for.
                watch_signals: false,
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
            relay: None,
            shell_integration: true,
            min_delta_interval: Duration::ZERO,
            enroll: None,
            offer: None,
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

    /// A pairing request submitted to this queue, with the decision captured.
    fn pending_request(
        auth: &Arc<crate::auth::Authenticator>,
        device: ClientId,
    ) -> (
        zest_mesh::pairing::PendingHandle,
        Arc<std::sync::Mutex<Option<zest_mesh::pairing::Decision>>>,
    ) {
        let decided: Arc<std::sync::Mutex<Option<zest_mesh::pairing::Decision>>> =
            Arc::default();
        let sink = Arc::clone(&decided);
        let handle = auth.ask(
            zest_mesh::pairing::PairingRequest {
                client: device,
                label: "andy-phone".into(),
                code: "481502".into(),
                remote: "192.168.1.42:60123".into(),
                requested_at: Instant::now(),
            },
            Box::new(move |d| {
                *sink.lock().expect("decision lock") = Some(d);
            }),
        );
        (handle, decided)
    }

    #[test]
    fn a_loopback_watcher_hears_the_approval_queue_and_its_decision_answers() {
        // ROADMAP M4's modal, at the daemon seam: the app subscribes with
        // `Hello.watch_pairings` over loopback, hears what is waiting —
        // including a request that arrived *before* it connected, or the
        // modal only ever covers lucky timing — answers it with the
        // `PairingDecision` loopback already honours, and hears the
        // tombstone that closes the prompt.
        let auth = test_authenticator();
        let registry = Arc::new(Registry::new());
        let device = ClientId::from_bytes([0xd0; 32]);

        // Waiting before the watcher exists: the replay case.
        let (_handle, decided) = pending_request(&auth, device);

        let mut watcher = Connection::new(
            config(),
            Arc::clone(&registry),
            crate::auth::Auth::Transport(Arc::clone(&auth)),
            "test",
        );
        let woken = Arc::new(std::sync::atomic::AtomicBool::new(false));
        {
            let woken = Arc::clone(&woken);
            watcher.set_waker(Box::new(move || {
                woken.store(true, std::sync::atomic::Ordering::Release);
            }));
        }
        let identity = std::sync::Arc::new(
            zest_mesh::identity::ClientIdentity::generate().expect("client key"),
        );
        let mut peer = authenticate_identity(&mut watcher, &identity, false, true, false);

        let pushed = watcher.poll();
        let [HostMessage::PairingRequested {
            client,
            label,
            code,
            remote,
            expires_in_secs,
            resolved,
        }] = &pushed[..]
        else {
            panic!("a request already waiting must be replayed on subscribe, got {pushed:?}");
        };
        assert_eq!(*client, device);
        assert_eq!(code, "481502", "the code is the person's comparison input");
        assert_eq!(label, "andy-phone");
        assert_eq!(remote, "192.168.1.42:60123");
        assert!(!resolved, "a live request is not a tombstone");
        assert!(
            *expires_in_secs > 0,
            "a zero expiry would tell the modal the code is already dead"
        );

        // The decision goes back over the same loopback connection.
        let out = peer.send(
            &mut watcher,
            &ClientMessage::PairingDecision { client: device, approve: true },
        );
        assert!(out.is_empty(), "a loopback decision is honoured silently, got {out:?}");
        assert_eq!(
            *decided.lock().expect("decision lock"),
            Some(zest_mesh::pairing::Decision::Approve),
            "the modal's Approve must reach the device's waiting handshake"
        );

        // Resolving woke the watcher, and the coalesced diff says "gone" —
        // which is what closes a modal someone answered elsewhere too.
        assert!(
            woken.load(std::sync::atomic::Ordering::Acquire),
            "answering must wake the watching connection"
        );
        let pushed = watcher.poll();
        let [HostMessage::PairingRequested { client, resolved: true, code, .. }] = &pushed[..]
        else {
            panic!("the request leaving the queue must push a tombstone, got {pushed:?}");
        };
        assert_eq!(*client, device);
        assert!(code.is_empty(), "a tombstone carries no code — there is nothing to compare");
    }

    #[test]
    fn a_live_requests_expiry_is_never_the_unknown_marker() {
        // `0` means "expiry unknown" on the wire (an old daemon, or a
        // tombstone), and the client answers unknown by assuming the full
        // pairing window. Truncation made a request with under a second
        // left claim exactly that, so its dead code stayed on screen for
        // minutes. Live requests therefore ceil and floor at 1.
        assert_eq!(pairing_expiry_secs(Duration::ZERO), 1, "expired-but-queued is still answerable");
        assert_eq!(pairing_expiry_secs(Duration::from_millis(400)), 1, "sub-second must not truncate to unknown");
        assert_eq!(pairing_expiry_secs(Duration::from_secs(1)), 1);
        assert_eq!(
            pairing_expiry_secs(Duration::from_millis(1200)),
            2,
            "partial seconds round up — the code outlives the number, never the reverse"
        );
        assert_eq!(
            pairing_expiry_secs(zest_mesh::pairing::APPROVAL_TIMEOUT),
            u32::try_from(zest_mesh::pairing::APPROVAL_TIMEOUT.as_secs()).expect("fits"),
        );
    }

    /// A control plane that says yes, and remembers being asked.
    struct FakeControlPlane {
        asked: Arc<std::sync::Mutex<Vec<String>>>,
    }

    impl crate::enroll::ControlPlane for FakeControlPlane {
        fn post_json(
            &self,
            url: &str,
            _body: &str,
        ) -> Result<crate::enroll::Response, crate::enroll::EnrollError> {
            self.asked.lock().expect("asked lock").push(url.to_string());
            Ok(crate::enroll::Response {
                status: 200,
                body: r#"{"token":"tok-1","account":"andy"}"#.into(),
            })
        }
    }

    #[test]
    fn a_loopback_enroll_runs_the_claim_and_answers_with_the_account() {
        // Issue #227's daemon half: the app sends the code it minted, and the
        // daemon does exactly what `--enroll` does — sign, post, keep the
        // token — answering off a worker so the serve loop never holds its
        // connection lock across a keychain probe or an HTTPS round trip.
        let asked: Arc<std::sync::Mutex<Vec<String>>> = Arc::default();
        let secrets = Arc::new(zest_mesh::keystore::MemoryKeyStore::new());
        let mut cfg = config();
        cfg.enroll = Some(crate::EnrollSeam {
            base_url: "https://control.test".into(),
            http: Arc::new(FakeControlPlane { asked: Arc::clone(&asked) }),
            secrets: Arc::clone(&secrets) as Arc<dyn zest_mesh::keystore::SecretStore>,
        });
        let mut c = Connection::new(
            cfg,
            Arc::new(Registry::new()),
            crate::auth::Auth::Transport(test_authenticator()),
            "test",
        );
        let woken = Arc::new(std::sync::atomic::AtomicBool::new(false));
        {
            let woken = Arc::clone(&woken);
            c.set_waker(Box::new(move || {
                woken.store(true, std::sync::atomic::Ordering::Release);
            }));
        }
        let mut peer = authenticate(&mut c);

        let out = peer.send(&mut c, &ClientMessage::Enroll { code: "GOLDCODE".into() });
        assert!(out.is_empty(), "the reply comes off the worker, not the serve loop: {out:?}");

        // The worker settles and wakes the writer, which drains the result.
        let deadline = Instant::now() + Duration::from_secs(10);
        let result = loop {
            let msgs = c.take_enroll_result();
            if !msgs.is_empty() {
                break msgs;
            }
            assert!(Instant::now() < deadline, "the enrolment never settled");
            std::thread::sleep(Duration::from_millis(10));
        };
        let [HostMessage::EnrollResult { ok, account, .. }] = &result[..] else {
            panic!("expected an EnrollResult, got {result:?}");
        };
        assert!(ok, "a control plane that said yes must reach the app as yes");
        assert_eq!(account.as_deref(), Some("andy"), "…naming the account");
        assert!(
            woken.load(std::sync::atomic::Ordering::Acquire),
            "the worker must wake the writer, or the answer sits until traffic"
        );
        assert_eq!(
            asked.lock().expect("asked lock").as_slice(),
            ["https://control.test/api/enroll/claim"],
            "the claim goes to the seam's URL and the claim path"
        );
        use zest_mesh::keystore::SecretStore as _;
        assert!(
            secrets
                .load_secret(zest_mesh::keystore::CLOUD_TOKEN_NAME)
                .expect("store readable")
                .is_some(),
            "the token must land in the daemon's own store, exactly like --enroll"
        );
    }

    #[test]
    fn a_successful_enrolment_flips_the_published_token_fact() {
        // #245: the enrol affordance gates on the daemon's own word
        // (`HostOffer::has_account_token`), so the moment the token lands in
        // the store the word must change — and through the offer's
        // generation, so the watchers wake and every fleet card learns this
        // machine no longer needs enrolling without waiting for the account
        // listing to catch up.
        let source = crate::offer::OfferSource::new(crate::offer::facts("zsh".into()));
        source.set_account_token(Some(false));
        let generation_before = source.generation();

        let mut cfg = config();
        cfg.offer = Some(source.clone());
        cfg.enroll = Some(crate::EnrollSeam {
            base_url: "https://control.test".into(),
            http: Arc::new(FakeControlPlane { asked: Arc::default() }),
            secrets: Arc::new(zest_mesh::keystore::MemoryKeyStore::new()),
        });
        let mut c = Connection::new(
            cfg,
            Arc::new(Registry::new()),
            crate::auth::Auth::Transport(test_authenticator()),
            "test",
        );
        let mut peer = authenticate(&mut c);

        let out = peer.send(&mut c, &ClientMessage::Enroll { code: "GOLDCODE".into() });
        assert!(out.is_empty(), "the reply comes off the worker: {out:?}");
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            if !c.take_enroll_result().is_empty() {
                break;
            }
            assert!(Instant::now() < deadline, "the enrolment never settled");
            std::thread::sleep(Duration::from_millis(10));
        }

        assert_eq!(
            source.snapshot().has_account_token,
            Some(true),
            "the published fact must follow the token into the store"
        );
        assert!(
            source.generation() > generation_before,
            "and move the generation, or no watcher is ever woken to send it"
        );
    }

    #[test]
    fn a_refused_claim_reaches_the_app_as_the_persons_next_move() {
        // The seam's error is shown verbatim by the app's card (#227), so what
        // crosses it must already be the sentence to act on. Before #368 this
        // shipped `Display` — "the control plane refused this enrolment (409):
        // already_enrolled" — which names no move at all.
        struct RefusingControlPlane;
        impl crate::enroll::ControlPlane for RefusingControlPlane {
            fn post_json(
                &self,
                _url: &str,
                _body: &str,
            ) -> Result<crate::enroll::Response, crate::enroll::EnrollError> {
                Ok(crate::enroll::Response {
                    status: 409,
                    body: r#"{"error":"already_enrolled","detail":"revoked"}"#.into(),
                })
            }
        }
        let mut cfg = config();
        cfg.enroll = Some(crate::EnrollSeam {
            base_url: "https://control.test".into(),
            http: Arc::new(RefusingControlPlane),
            secrets: Arc::new(zest_mesh::keystore::MemoryKeyStore::new()),
        });
        let mut c = Connection::new(
            cfg,
            Arc::new(Registry::new()),
            crate::auth::Auth::Transport(test_authenticator()),
            "test",
        );
        let mut peer = authenticate(&mut c);

        let out = peer.send(&mut c, &ClientMessage::Enroll { code: "GOLDCODE".into() });
        assert!(out.is_empty(), "the reply comes off the worker: {out:?}");
        let deadline = Instant::now() + Duration::from_secs(10);
        let result = loop {
            let msgs = c.take_enroll_result();
            if !msgs.is_empty() {
                break msgs;
            }
            assert!(Instant::now() < deadline, "the enrolment never settled");
            std::thread::sleep(Duration::from_millis(10));
        };
        let [HostMessage::EnrollResult { ok, message, .. }] = &result[..] else {
            panic!("expected an EnrollResult, got {result:?}");
        };
        assert!(!ok);
        assert!(
            message.contains("restore"),
            "a revoked machine's refusal must say the way back, not restate the 409; \
             got {message:?}"
        );
    }

    /// A control plane that answers only when the test says so — how a
    /// worker is held parked mid-claim.
    struct ParkedControlPlane {
        release: std::sync::Mutex<std::sync::mpsc::Receiver<()>>,
    }

    impl crate::enroll::ControlPlane for ParkedControlPlane {
        fn post_json(
            &self,
            _url: &str,
            _body: &str,
        ) -> Result<crate::enroll::Response, crate::enroll::EnrollError> {
            self.release
                .lock()
                .expect("release lock")
                .recv()
                .expect("the test releases the claim");
            Ok(crate::enroll::Response {
                status: 200,
                body: r#"{"token":"tok-1","account":"andy"}"#.into(),
            })
        }
    }

    #[test]
    fn a_second_enroll_is_refused_while_the_first_is_still_running() {
        // The mailbox holds one answer, so the connection must run one
        // enrolment at a time: without the refusal, a second Enroll's worker
        // overwrites a result nobody has drained yet, and one caller waits
        // for ever on an answer the other received (review finding on #232).
        let (release, gate) = std::sync::mpsc::channel::<()>();
        let secrets = Arc::new(zest_mesh::keystore::MemoryKeyStore::new());
        let mut cfg = config();
        cfg.enroll = Some(crate::EnrollSeam {
            base_url: "https://control.test".into(),
            http: Arc::new(ParkedControlPlane { release: std::sync::Mutex::new(gate) }),
            secrets: Arc::clone(&secrets) as Arc<dyn zest_mesh::keystore::SecretStore>,
        });
        let mut c = Connection::new(
            cfg,
            Arc::new(Registry::new()),
            crate::auth::Auth::Transport(test_authenticator()),
            "test",
        );
        let mut peer = authenticate(&mut c);

        let out = peer.send(&mut c, &ClientMessage::Enroll { code: "FIRSTCODE".into() });
        assert!(out.is_empty(), "the first Enroll goes to its worker: {out:?}");

        // The first worker is parked inside the claim; a second ask must be
        // refused now, honestly, without touching the worker or its mailbox.
        let out = peer.send(&mut c, &ClientMessage::Enroll { code: "SECONDCODE".into() });
        let [HostMessage::EnrollResult { ok: false, message, .. }] = &out[..] else {
            panic!("a concurrent Enroll must be refused with an EnrollResult, got {out:?}");
        };
        assert!(
            message.contains("already running"),
            "the refusal says what is happening, not a mechanism: {message}"
        );

        // Release the first claim: its own result — not the second's refusal,
        // not an overwrite — arrives through the drain.
        release.send(()).expect("worker is waiting");
        let deadline = Instant::now() + Duration::from_secs(10);
        let result = loop {
            let msgs = c.take_enroll_result();
            if !msgs.is_empty() {
                break msgs;
            }
            assert!(Instant::now() < deadline, "the first enrolment never settled");
            std::thread::sleep(Duration::from_millis(10));
        };
        let [HostMessage::EnrollResult { ok: true, account, .. }] = &result[..] else {
            panic!("the first Enroll must still get its own result, got {result:?}");
        };
        assert_eq!(account.as_deref(), Some("andy"));

        // Drained means free: the connection can enrol again (re-enrolling a
        // machine is a thing people do — the store probe's own comment).
        let out = peer.send(&mut c, &ClientMessage::Enroll { code: "THIRDCODE".into() });
        assert!(
            out.is_empty(),
            "after the drain a fresh Enroll must reach a worker again: {out:?}"
        );
        release.send(()).expect("third worker is waiting");
    }

    #[test]
    fn a_remote_connection_may_not_enroll_this_machine() {
        // `PairingDecision`'s gate, `PairingDecision`'s reason: a trusted LAN
        // device is allowed in, not allowed to bind this machine to an
        // account. The refusal is immediate — no worker, no control plane.
        let auth = test_authenticator();
        let identity = std::sync::Arc::new(
            zest_mesh::identity::ClientIdentity::generate().expect("client key"),
        );
        auth.trust_now(identity.client_id(), "trusted-lan").expect("trust");
        let mut lan = Connection::new(
            config(),
            Arc::new(Registry::new()),
            crate::auth::Auth::Proof(auth),
            "192.168.1.9:50000",
        );
        let mut peer = authenticate_identity(&mut lan, &identity, false, false, false);
        let out = peer.send(&mut lan, &ClientMessage::Enroll { code: "GOLDCODE".into() });
        let [HostMessage::Error { message, .. }] = &out[..] else {
            panic!("a remote Enroll must be refused with an Error, got {out:?}");
        };
        assert!(
            message.contains("local"),
            "the refusal names the rule, not a mechanism: {message}"
        );
    }

    /// A connection on a daemon that publishes `offer`.
    fn conn_offering(offer: zest_proto::HostOffer) -> (Connection, crate::offer::OfferSource) {
        let source = crate::offer::OfferSource::new(offer);
        let mut cfg = config();
        cfg.offer = Some(source.clone());
        (
            Connection::new(
                cfg,
                Arc::new(Registry::new()),
                crate::auth::Auth::Transport(test_authenticator()),
                "test",
            ),
            source,
        )
    }

    fn offer_with(profiles: &[&str]) -> zest_proto::HostOffer {
        zest_proto::HostOffer {
            os: "macos".into(),
            arch: "aarch64".into(),
            os_version: "24.5.0".into(),
            default_shell: "zsh -l".into(),
            profiles: profiles
                .iter()
                .map(|n| zest_proto::HostProfile {
                    name: (*n).to_string(),
                    command: format!("run-{n}"),
                    ..Default::default()
                })
                .collect(),
            ..Default::default()
        }
    }

    #[test]
    fn a_subscriber_is_told_what_this_machine_offers_and_a_non_subscriber_is_not() {
        // The whole point of #262: a launcher on another machine can only
        // list this one's profiles if this one says what they are.
        let (mut c, _source) = conn_offering(offer_with(&["ubuntu", "pwsh"]));
        let mut peer = authenticate_identity(
            &mut c,
            &Arc::new(zest_mesh::identity::ClientIdentity::generate().expect("key")),
            false,
            false,
            true,
        );
        let out = peer.send(&mut c, &ClientMessage::ListSessions);
        let [HostMessage::Sessions { offer: Some(offer), .. }] = &out[..] else {
            panic!("a watch_hosts listing must carry the offer, got {out:?}");
        };
        assert_eq!(offer.os, "macos");
        assert_eq!(offer.default_shell, "zsh -l", "so a remote row can say what it will run");
        let names: Vec<&str> = offer.profiles.iter().map(|p| p.name.as_str()).collect();
        assert_eq!(names, ["ubuntu", "pwsh"]);

        // And a client that did not ask gets nothing — the profile list is not
        // free, and a connection attaching to one session should not carry it.
        let (mut quiet, _source) = conn_offering(offer_with(&["ubuntu"]));
        let mut peer = authenticate(&mut quiet);
        let out = peer.send(&mut quiet, &ClientMessage::ListSessions);
        assert!(
            matches!(&out[..], [HostMessage::Sessions { offer: None, .. }]),
            "no subscription, no offer: {out:?}"
        );
    }

    #[test]
    fn the_offer_is_sent_once_and_again_only_when_it_changes() {
        // The generation diff, and the reason it is not an optimisation: a
        // session listing is polled constantly, and re-sending every profile
        // on this machine with each one would put the whole config on the wire
        // per keystroke.
        let (mut c, source) = conn_offering(offer_with(&["ubuntu"]));
        let mut peer = authenticate_identity(
            &mut c,
            &Arc::new(zest_mesh::identity::ClientIdentity::generate().expect("key")),
            false,
            false,
            true,
        );
        let first = peer.send(&mut c, &ClientMessage::ListSessions);
        assert!(matches!(&first[..], [HostMessage::Sessions { offer: Some(_), .. }]));

        let second = peer.send(&mut c, &ClientMessage::ListSessions);
        assert!(
            matches!(&second[..], [HostMessage::Sessions { offer: None, .. }]),
            "nothing changed, so nothing is repeated: {second:?}"
        );

        // A config edit on this machine moves the generation, and the very
        // next message carries the new list — without this, a profile written
        // on the far machine is invisible until something restarts.
        assert!(source.set(offer_with(&["ubuntu", "nightly"])), "a different offer is a change");
        let third = peer.send(&mut c, &ClientMessage::ListSessions);
        let [HostMessage::Sessions { offer: Some(offer), .. }] = &third[..] else {
            panic!("a changed offer must be re-sent, got {third:?}");
        };
        assert_eq!(offer.profiles.len(), 2);
    }

    #[test]
    fn a_config_edit_reaches_a_watcher_that_asked_nothing_else() {
        // The push that has no session change behind it. A config edit moves
        // the profile list and moves nothing in the registry, so without its
        // own branch in `poll_with` the far launcher would show the old rows
        // until somebody happened to open a shell.
        let (mut c, source) = conn_offering(offer_with(&["ubuntu"]));
        let mut peer = authenticate_identity(
            &mut c,
            &Arc::new(zest_mesh::identity::ClientIdentity::generate().expect("key")),
            false,
            false,
            true,
        );
        // Drain the first offer, so what follows is only what the edit caused.
        let _ = peer.send(&mut c, &ClientMessage::ListSessions);
        assert!(
            c.poll().iter().all(|m| !matches!(m, HostMessage::Sessions { .. })),
            "a quiet daemon pushes nothing"
        );

        source.set(offer_with(&["ubuntu", "nightly"]));
        let pushed = c.poll();
        let [HostMessage::Sessions { offer: Some(offer), .. }] = &pushed[..] else {
            panic!("the edit must reach the watcher unprompted, got {pushed:?}");
        };
        assert_eq!(offer.profiles.len(), 2);
    }

    #[test]
    fn a_config_edit_wakes_the_serve_loop_and_not_only_the_generation() {
        // The half `a_config_edit_reaches_a_watcher_that_asked_nothing_else`
        // cannot see, and the one that was missing: that test calls `poll`
        // itself, so it passes whether or not anything would have *caused* a
        // poll. The real serve loop blocks until something wakes it, so an
        // offer change that bumps the generation and wakes nobody waits for an
        // unrelated event to carry it — a machine with an idle session
        // publishes its new profile list at the next keystroke, and one with
        // no sessions at all may never publish it at all.
        let (mut c, source) = conn_offering(offer_with(&["ubuntu"]));
        let woken = Arc::new(std::sync::atomic::AtomicBool::new(false));
        {
            let woken = Arc::clone(&woken);
            c.set_waker(Box::new(move || {
                woken.store(true, std::sync::atomic::Ordering::Release);
            }));
        }
        let _peer = authenticate_identity(
            &mut c,
            &Arc::new(zest_mesh::identity::ClientIdentity::generate().expect("key")),
            false,
            false,
            true,
        );
        woken.store(false, std::sync::atomic::Ordering::Release);

        assert!(
            !source.set(offer_with(&["ubuntu"])),
            "precondition: an identical offer is not a change"
        );
        assert!(
            !woken.load(std::sync::atomic::Ordering::Acquire),
            "and must not wake anyone — a file watcher fires several times per save"
        );

        assert!(source.set(offer_with(&["ubuntu", "nightly"])), "a different offer is a change");
        assert!(
            woken.load(std::sync::atomic::Ordering::Acquire),
            "a real edit must wake the connection, or the push waits for unrelated traffic"
        );
    }

    #[test]
    fn a_connection_that_did_not_subscribe_is_never_woken_by_an_offer() {
        // The subscription is what registers the waker, so an attach-only
        // connection must not be woken by somebody editing a config it will
        // never be told about.
        let (mut c, source) = conn_offering(offer_with(&["ubuntu"]));
        let woken = Arc::new(std::sync::atomic::AtomicBool::new(false));
        {
            let woken = Arc::clone(&woken);
            c.set_waker(Box::new(move || {
                woken.store(true, std::sync::atomic::Ordering::Release);
            }));
        }
        let _peer = authenticate(&mut c);
        woken.store(false, std::sync::atomic::Ordering::Release);

        assert!(source.set(offer_with(&["ubuntu", "nightly"])));
        assert!(
            !woken.load(std::sync::atomic::Ordering::Acquire),
            "no subscription, no wakeup"
        );
    }

    #[test]
    fn a_daemon_that_publishes_nothing_reads_like_one_that_predates_the_field() {
        // Three ways to get `offer: None` — did not subscribe, daemon
        // publishes none, nothing changed — and the client must not need to
        // tell them apart. `config()` carries no offer, which is both the test
        // harness and an older daemon.
        let (mut c, _registry) = conn();
        let mut peer = authenticate_identity(
            &mut c,
            &Arc::new(zest_mesh::identity::ClientIdentity::generate().expect("key")),
            false,
            false,
            true,
        );
        let out = peer.send(&mut c, &ClientMessage::ListSessions);
        assert!(
            matches!(&out[..], [HostMessage::Sessions { offer: None, .. }]),
            "subscribed to a daemon with nothing to say: {out:?}"
        );
    }

    #[test]
    fn an_ephemeral_daemon_says_it_cannot_enroll() {
        // config() carries no seam, which is exactly the --ephemeral daemon:
        // its key dies with the process, so an account row for it would name
        // a host nobody can ever reach. The answer is an honest EnrollResult
        // rather than silence.
        let (mut c, _registry) = conn();
        let mut peer = authenticate(&mut c);
        let out = peer.send(&mut c, &ClientMessage::Enroll { code: "GOLDCODE".into() });
        let [HostMessage::EnrollResult { ok: false, message, .. }] = &out[..] else {
            panic!("expected an immediate refusal, got {out:?}");
        };
        assert!(
            message.contains("--enroll"),
            "the refusal names the person's fallback: {message}"
        );
    }

    #[test]
    fn a_lan_connection_never_hears_the_approval_queue() {
        // The gate: `watch_pairings` is honoured where `may_approve_devices`
        // is — loopback — and silently ignored elsewhere. A LAN peer that
        // asked would otherwise be shown other people's matching codes,
        // which is exactly the information a hostile network wants.
        let auth = test_authenticator();
        let registry = Arc::new(Registry::new());

        // Trusted first, because an untrusted Proof connection pends instead
        // of welcoming — and a *trusted* device asking to watch is precisely
        // the sharp case: it is allowed in, just not allowed to see this.
        let identity = std::sync::Arc::new(
            zest_mesh::identity::ClientIdentity::generate().expect("client key"),
        );
        auth.trust_now(identity.client_id(), "trusted-lan").expect("trust");
        let mut lan = Connection::new(
            config(),
            Arc::clone(&registry),
            crate::auth::Auth::Proof(Arc::clone(&auth)),
            "192.168.1.9:50000",
        );
        let _peer = authenticate_identity(&mut lan, &identity, false, true, false);

        let (_handle, _decided) = pending_request(&auth, ClientId::from_bytes([0xd1; 32]));
        let pushed = lan.poll();
        assert!(
            !pushed
                .iter()
                .any(|m| matches!(m, HostMessage::PairingRequested { .. })),
            "a LAN connection asked to watch and must still hear nothing: {pushed:?}"
        );
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
        let [HostMessage::Sessions { sessions, created: Some(id), .. }] = &out[..] else {
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
            matches!(&pushed[..], [HostMessage::Sessions { sessions, created: None, .. }]
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
                watch_pairings: false,
                watch_hosts: false,
                watch_signals: false,
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
            watch_pairings: false,
            watch_hosts: false,
            watch_signals: false,
        }
    }

    fn echo_cmd() -> String {
        if cfg!(windows) { "cmd.exe /c echo probe".into() } else { "/bin/echo probe".into() }
    }

    /// A child that fails, with a status nothing could have guessed.
    ///
    /// `3` rather than `1`: a `1` is what a dozen accidents produce, so a test
    /// asserting it passes for a reader that reports "failed" without ever
    /// having read a number.
    fn exit_3_cmd() -> String {
        if cfg!(windows) {
            "cmd.exe /c exit 3".into()
        } else {
            "/bin/sh -c 'exit 3'".into()
        }
    }

    /// A child that outlives the test unless something ends it.
    ///
    /// The lifetime is the point: anything about *closing* a session is vacuous
    /// against a command that has already exited on its own.
    ///
    /// `ping` against loopback rather than `Start-Sleep`: nothing here is
    /// about a shell, and pwsh's boot on a contended runner is what #285
    /// removed from every assertion budget in this module. `-n 31` is one
    /// ping a second, ~30 seconds -- the same lifetime `Start-Sleep 30`
    /// gave. The cmd wrapper exists to spell `>nul`: `Start-Sleep` was
    /// *silent*, and a dozen tests picked this child for exactly that, so a
    /// ping line arriving every second would quietly change what they hold
    /// still. Redirection is a shell's trick -- bare `ping.exe` would take
    /// `>nul` as one more argument and exit on it.
    fn sleep_cmd() -> String {
        if cfg!(windows) {
            "cmd.exe /c ping.exe -n 31 127.0.0.1 >nul".into()
        } else {
            "/bin/sleep 30".into()
        }
    }

    /// A child that prints *after* the client has had time to attach.
    ///
    /// The delay is the whole point: with a plain `echo` the output is already
    /// in the terminal when `Attach` builds its keyframe, so a test about what
    /// a later poll produces would be asserting on nothing.
    ///
    /// On Windows the delay is `ping`'s (`-n 3` = three pings a second
    /// apart, ~2s), not a shell's sleep: the attach it has to land after is
    /// a synchronous call microseconds after the create, and the
    /// `Start-Sleep` that stood here cost a pwsh boot that, on a loaded
    /// runner, pushed the *exit* past `wait_for`'s deadline -- reported as
    /// an exit-ordering failure for a child that had not finished starting
    /// (#285).
    fn delayed_echo_cmd() -> String {
        if cfg!(windows) {
            "cmd.exe /c ping.exe -n 3 127.0.0.1 >nul & echo probe".into()
        } else {
            "/bin/sh -c 'sleep 0.3; echo probe'".into()
        }
    }

    /// The same deadline `session.rs`'s copy carries, for the same measured
    /// reason: on a contended Windows runner a `wait_for(|| has_exited())` can
    /// burn ten seconds while PowerShell is still starting, and the assertion
    /// that then fires names whatever came after it rather than the child that
    /// never ran (#80). #92 raised it there; this copy was missed, and
    /// `an_ended_session_sends_its_last_output_in_front_of_the_exit` failed on
    /// Windows CI in exactly that shape.
    ///
    /// Generous costs nothing: only a run that was going to fail pays for it.
    ///
    /// And 30 seconds was outgrown the same way 10 was: three tests waiting
    /// on PowerShell children hit the deadline together, twice, on PRs that
    /// touched nothing in this crate (#285). The answer is not a bigger
    /// number — that buys time again and a real hang then takes longer to
    /// report — it is that no test here boots PowerShell any more. `cmd.exe`
    /// (a shell too, but one without a runtime to lift) and `ping.exe` start
    /// in milliseconds on the same loaded runner that took several
    /// concurrent pwsh boots past half a minute.
    fn wait_for(mut f: impl FnMut() -> bool) -> bool {
        let deadline = Instant::now() + Duration::from_secs(30);
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
        peer.send(&mut c, &ClientMessage::Attach { session: addr, cols: 80, rows: 24, observe: false });
        // Wait for the echo's output so the session's sequence is really
        // nonzero. Attaching used to guarantee that by resizing the terminal
        // even to its existing size; arbitration made the equal-size attach a
        // true no-op (#215), so a fresh session is honestly at sequence 0 --
        // and a keyframe saying 0 then *matches* the daemon's baseline, which
        // is the agreement this test exists to protect.
        assert!(wait_for(|| !c.poll().is_empty()), "the echo never produced output");

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
                watch_pairings: false,
                watch_hosts: false,
                watch_signals: false,
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

        let out = peer.send(&mut c, &ClientMessage::Attach { session: addr, cols: 80, rows: 24, observe: false });
        assert!(matches!(&out[..], [HostMessage::Keyframe { .. }]), "{out:?}");

        assert!(
            wait_for(|| !c.poll().is_empty()),
            "the child produced output but nothing reached the client"
        );
    }

    /// The original #215 bug: merely attaching counted as a resize.
    ///
    /// One session on two devices at once is the product, and a phone peeking
    /// at a desktop session must not reshape the desktop's pty. The session's
    /// size is the smallest attached client, so a *larger* second attach
    /// changes nothing — and its keyframe reports the granted size, not the
    /// ask.
    #[test]
    fn a_second_attach_does_not_resize_the_first_clients_pty() {
        let (mut a, registry) = conn();
        let mut peer_a = authenticate(&mut a);
        peer_a.send(
            &mut a,
            &ClientMessage::CreateSession {
                command: sleep_cmd(),
                cwd: String::new(),
                cols: 80,
                rows: 24,
            },
        );
        let addr = registry.list(config().host)[0].addr;
        peer_a.send(&mut a, &ClientMessage::Attach { session: addr, cols: 80, rows: 24, observe: false });

        let mut b = Connection::new(
            config(),
            Arc::clone(&registry),
            crate::auth::Auth::Transport(test_authenticator()),
            "test2",
        );
        let mut peer_b = authenticate(&mut b);
        let out = peer_b.send(&mut b, &ClientMessage::Attach { session: addr, cols: 100, rows: 40, observe: false });

        let s = registry.get(addr.session).expect("the session is attached");
        assert_eq!(
            s.size(),
            (80, 24),
            "a larger second attach resized the pty out from under the first client"
        );
        assert!(
            matches!(&out[..], [HostMessage::Keyframe { cols: 80, rows: 24, .. }]),
            "the attach keyframe must carry the granted size, not the ask: {out:?}"
        );
        registry.close(addr.session);
    }

    /// The other half of the min: a smaller client does shrink the session --
    /// every viewer must see a complete screen -- and its detach gives the
    /// space back without anyone else doing anything.
    #[test]
    fn a_smaller_attach_wins_and_its_detach_restores_the_size() {
        let (mut a, registry) = conn();
        let mut peer_a = authenticate(&mut a);
        peer_a.send(
            &mut a,
            &ClientMessage::CreateSession {
                command: sleep_cmd(),
                cwd: String::new(),
                cols: 80,
                rows: 24,
            },
        );
        let addr = registry.list(config().host)[0].addr;
        peer_a.send(&mut a, &ClientMessage::Attach { session: addr, cols: 80, rows: 24, observe: false });

        let mut b = Connection::new(
            config(),
            Arc::clone(&registry),
            crate::auth::Auth::Transport(test_authenticator()),
            "test2",
        );
        let mut peer_b = authenticate(&mut b);
        peer_b.send(&mut b, &ClientMessage::Attach { session: addr, cols: 60, rows: 20, observe: false });

        let s = registry.get(addr.session).expect("the session is attached");
        assert_eq!(s.size(), (60, 20), "the smallest attached client sets the session size");

        peer_b.send(&mut b, &ClientMessage::Detach { session: addr });
        assert_eq!(s.size(), (80, 24), "detaching the constraining client must give the space back");
        registry.close(addr.session);
    }

    /// The whole point of `observe`: a client with no pane must be able to
    /// watch without shrinking the one somebody is looking at.
    ///
    /// Asserted on the *session*, not on what the observer was sent, because
    /// the observer sees a correct keyframe either way -- the damage is done
    /// to the other client, which is exactly why this went unnoticed until an
    /// agent needed it (#274).
    #[test]
    fn an_observer_attach_does_not_shrink_the_session() {
        let (mut a, registry) = conn();
        let mut peer_a = authenticate(&mut a);
        peer_a.send(
            &mut a,
            &ClientMessage::CreateSession {
                command: sleep_cmd(),
                cwd: String::new(),
                cols: 80,
                rows: 24,
            },
        );
        let addr = registry.list(config().host)[0].addr;
        peer_a.send(&mut a, &ClientMessage::Attach { session: addr, cols: 80, rows: 24, observe: false });

        let mut b = Connection::new(
            config(),
            Arc::clone(&registry),
            crate::auth::Auth::Transport(test_authenticator()),
            "observer",
        );
        let mut peer_b = authenticate(&mut b);
        // A size that *would* win the min, so a regression cannot pass by the
        // observer happening to ask for something larger.
        let out =
            peer_b.send(&mut b, &ClientMessage::Attach { session: addr, cols: 60, rows: 20, observe: true });

        let s = registry.get(addr.session).expect("the session is attached");
        assert_eq!(
            s.size(),
            (80, 24),
            "an observer voted in the size arbitration and shrank the human's window"
        );
        assert!(
            matches!(&out[..], [HostMessage::Keyframe { cols: 80, rows: 24, .. }]),
            "an observer must still be sent a keyframe, at the granted size: {out:?}"
        );
        registry.close(addr.session);
    }

    /// Abstaining is not the same as asking for the session's own size.
    ///
    /// A client that voted what it found would still pin the session there:
    /// `reconcile_size` reports no change when the minimum does not move, so
    /// the human growing their window pushes nothing and the observer is never
    /// told it should raise its vote. This is the case with no client-side
    /// workaround, and the reason `observe` had to exist on the wire.
    #[test]
    fn an_observer_does_not_pin_a_session_that_later_grows() {
        let (mut a, registry) = conn();
        let mut peer_a = authenticate(&mut a);
        peer_a.send(
            &mut a,
            &ClientMessage::CreateSession {
                command: sleep_cmd(),
                cwd: String::new(),
                cols: 80,
                rows: 24,
            },
        );
        let addr = registry.list(config().host)[0].addr;
        peer_a.send(&mut a, &ClientMessage::Attach { session: addr, cols: 80, rows: 24, observe: false });

        let mut b = Connection::new(
            config(),
            Arc::clone(&registry),
            crate::auth::Auth::Transport(test_authenticator()),
            "observer",
        );
        let mut peer_b = authenticate(&mut b);
        peer_b.send(&mut b, &ClientMessage::Attach { session: addr, cols: 80, rows: 24, observe: true });

        // The human drags their window bigger.
        peer_a.send(&mut a, &ClientMessage::Resize { session: addr, cols: 120, rows: 40 });

        let s = registry.get(addr.session).expect("the session is attached");
        assert_eq!(
            s.size(),
            (120, 40),
            "the observer held the session at the size it attached to, and nothing \
             could have told it to let go"
        );
        registry.close(addr.session);
    }

    /// An observer alone must not resize a session nobody is rendering.
    ///
    /// The session keeps the size it was created at. Without this a headless
    /// reader attaching to a background session would silently reshape it,
    /// and on Windows a resize is a full ConPTY repaint (#200) -- not cosmetic.
    #[test]
    fn an_observer_alone_leaves_the_session_at_its_creation_size() {
        let (mut a, registry) = conn();
        let mut peer_a = authenticate(&mut a);
        peer_a.send(
            &mut a,
            &ClientMessage::CreateSession {
                command: sleep_cmd(),
                cwd: String::new(),
                cols: 100,
                rows: 30,
            },
        );
        let addr = registry.list(config().host)[0].addr;
        peer_a.send(&mut a, &ClientMessage::Attach { session: addr, cols: 40, rows: 10, observe: true });

        let s = registry.get(addr.session).expect("the session is attached");
        assert_eq!(
            s.size(),
            (100, 30),
            "the only attached client abstained, so nothing declared a size and the \
             session must keep its own"
        );
        registry.close(addr.session);
    }

    /// Re-attaching is how a vote is withdrawn, which is why `Resize` needed no
    /// flag of its own. The handler already replaces a stale subscriber, so the
    /// old vote must go with it rather than being counted for ever.
    #[test]
    fn re_attaching_as_an_observer_withdraws_the_vote() {
        let (mut a, registry) = conn();
        let mut peer_a = authenticate(&mut a);
        peer_a.send(
            &mut a,
            &ClientMessage::CreateSession {
                command: sleep_cmd(),
                cwd: String::new(),
                cols: 80,
                rows: 24,
            },
        );
        let addr = registry.list(config().host)[0].addr;
        peer_a.send(&mut a, &ClientMessage::Attach { session: addr, cols: 80, rows: 24, observe: false });

        let mut b = Connection::new(
            config(),
            Arc::clone(&registry),
            crate::auth::Auth::Transport(test_authenticator()),
            "observer",
        );
        let mut peer_b = authenticate(&mut b);
        peer_b.send(&mut b, &ClientMessage::Attach { session: addr, cols: 60, rows: 20, observe: false });
        let s = registry.get(addr.session).expect("the session is attached");
        assert_eq!(s.size(), (60, 20), "precondition: the second client is the binding minimum");

        peer_b.send(&mut b, &ClientMessage::Attach { session: addr, cols: 60, rows: 20, observe: true });
        assert_eq!(
            s.size(),
            (80, 24),
            "re-attaching as an observer must drop the earlier vote, not leave it \
             behind on a replaced subscriber"
        );
        registry.close(addr.session);
    }

    /// A client whose pane did not change has no reason to re-render: its
    /// ResizeObserver never fires. The daemon must push it an authoritative
    /// keyframe, because a *shrink* described only by deltas lands inside the
    /// stale larger grid without ever tripping NeedsKeyframe (apply.rs).
    #[test]
    fn a_foreign_size_change_reaches_the_other_client_as_a_keyframe() {
        let (mut a, registry) = conn();
        let mut peer_a = authenticate(&mut a);
        peer_a.send(
            &mut a,
            &ClientMessage::CreateSession {
                command: sleep_cmd(),
                cwd: String::new(),
                cols: 80,
                rows: 24,
            },
        );
        let addr = registry.list(config().host)[0].addr;
        peer_a.send(&mut a, &ClientMessage::Attach { session: addr, cols: 80, rows: 24, observe: false });
        // Drain whatever the attach owed.
        while !a.poll().is_empty() {}

        let mut b = Connection::new(
            config(),
            Arc::clone(&registry),
            crate::auth::Auth::Transport(test_authenticator()),
            "test2",
        );
        let mut peer_b = authenticate(&mut b);
        peer_b.send(&mut b, &ClientMessage::Attach { session: addr, cols: 60, rows: 20, observe: false });

        let mut granted = None;
        wait_for(|| {
            for m in a.poll() {
                if let HostMessage::Keyframe { cols, rows, .. } = m {
                    granted = Some((cols, rows));
                }
            }
            granted.is_some()
        });
        assert_eq!(
            granted,
            Some((60, 20)),
            "the unchanged client was never told the session is a different shape"
        );
        registry.close(addr.session);
    }

    /// `Resize` now names an attachment, not the session: only an attached
    /// client has a size worth arbitrating over. Both shipped clients attach
    /// before they ever resize.
    #[test]
    fn a_resize_from_an_unattached_connection_is_ignored() {
        let (mut a, registry) = conn();
        let mut peer_a = authenticate(&mut a);
        peer_a.send(
            &mut a,
            &ClientMessage::CreateSession {
                command: sleep_cmd(),
                cwd: String::new(),
                cols: 80,
                rows: 24,
            },
        );
        let addr = registry.list(config().host)[0].addr;
        peer_a.send(&mut a, &ClientMessage::Attach { session: addr, cols: 80, rows: 24, observe: false });

        let mut b = Connection::new(
            config(),
            Arc::clone(&registry),
            crate::auth::Auth::Transport(test_authenticator()),
            "test2",
        );
        let mut peer_b = authenticate(&mut b);
        peer_b.send(&mut b, &ClientMessage::Resize { session: addr, cols: 10, rows: 5 });

        let s = registry.get(addr.session).expect("the session is attached");
        assert_eq!(s.size(), (80, 24), "a connection that never attached resized the session");
        registry.close(addr.session);
    }

    /// `SessionInfo` carries cols/rows, so a granted resize changes the
    /// listing -- a watcher's fleet screen shows sizes that are now wrong
    /// unless the resize bumps the generation like attach and detach do.
    #[test]
    fn a_granted_resize_pushes_a_listing_update_to_watchers() {
        let (mut watcher, registry) = conn();
        let _watcher_peer = authenticate_with(&mut watcher, true);

        let mut c = Connection::new(
            config(),
            Arc::clone(&registry),
            crate::auth::Auth::Transport(test_authenticator()),
            "test2",
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
        peer.send(&mut c, &ClientMessage::Attach { session: addr, cols: 80, rows: 24, observe: false });
        // Drain the pushes the create and attach owed.
        while !watcher.poll().is_empty() {}

        peer.send(&mut c, &ClientMessage::Resize { session: addr, cols: 60, rows: 20 });

        let mut listed = None;
        wait_for(|| {
            for m in watcher.poll() {
                if let HostMessage::Sessions { sessions, .. } = m {
                    listed = sessions.first().map(|i| (i.cols, i.rows));
                }
            }
            listed.is_some()
        });
        assert_eq!(
            listed,
            Some((60, 20)),
            "a granted resize must reach watchers as a fresh listing"
        );
        registry.close(addr.session);
    }

    /// A child that rings the bell and exits, on either platform.
    fn bell_cmd() -> String {
        if cfg!(windows) {
            "cmd.exe /c echo \u{7}".into()
        } else {
            "/bin/echo \u{7}".into()
        }
    }

    /// Poll until a message satisfies `stop`, keeping **everything** seen.
    ///
    /// Collecting is not tidiness. A negative assertion made against whatever
    /// happens to be left after a search is an assertion about the search: a
    /// `find` per batch drops the rest of that batch, so a message arriving
    /// beside — or before — the one that ends the wait is thrown away, and
    /// "it never arrived" and "it was discarded" become the same result.
    ///
    /// Returns what was seen and whether `stop` ever fired; a caller that
    /// waited for something and did not get it must say so rather than
    /// asserting over an empty transcript.
    fn poll_collecting(
        c: &mut Connection,
        mut stop: impl FnMut(&HostMessage) -> bool,
    ) -> (Vec<HostMessage>, bool) {
        let mut seen: Vec<HostMessage> = Vec::new();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
        while std::time::Instant::now() < deadline {
            let batch = c.poll();
            let done = batch.iter().any(&mut stop);
            seen.extend(batch);
            if done {
                return (seen, true);
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        (seen, false)
    }

    #[test]
    fn a_bell_reaches_a_client_that_asked_for_signals() {
        let (mut c, registry) = conn();
        let mut peer = authenticate(&mut c);
        // The flag the `Hello` would have set. Set here rather than through
        // the handshake helper so the two halves of this property can be
        // asserted against one another below.
        c.watch_signals = true;
        peer.send(
            &mut c,
            &ClientMessage::CreateSession {
                command: bell_cmd(),
                cwd: String::new(),
                cols: 80,
                rows: 24,
            },
        );
        let addr = registry.list(config().host)[0].addr;
        peer.send(&mut c, &ClientMessage::Attach { session: addr, cols: 80, rows: 24, observe: false });

        let (seen, arrived) =
            poll_collecting(&mut c, |m| matches!(m, HostMessage::Attention { .. }));
        assert!(arrived, "a client that asked for signals was never sent the bell: {seen:?}");
        assert!(
            seen.iter().any(|m| matches!(
                m,
                HostMessage::Attention { cause, .. } if *cause == zest_proto::AttentionCause::Bell
            )),
            "and it is the bell it is told about: {seen:?}"
        );
    }

    #[test]
    fn a_bell_never_reaches_a_client_that_did_not_ask() {
        // **The load-bearing half.** A `HostMessage` tag an older client
        // cannot decode is not skipped: `DaemonClient::recv` maps an
        // undecodable frame to a transport error, which ends the connection.
        // So sending this to a client that did not ask does not merely
        // annoy it — it disconnects it, and the symptom is a window that goes
        // blank the first time a shell rings.
        let (mut c, registry) = conn();
        let mut peer = authenticate(&mut c);
        assert!(!c.watch_signals, "the default is not to send them");
        peer.send(
            &mut c,
            &ClientMessage::CreateSession {
                command: bell_cmd(),
                cwd: String::new(),
                cols: 80,
                rows: 24,
            },
        );
        let addr = registry.list(config().host)[0].addr;
        peer.send(&mut c, &ClientMessage::Attach { session: addr, cols: 80, rows: 24, observe: false });

        // Wait for the child to have *exited*, so the bell has certainly been
        // parsed by now — waiting a fixed time would pass on a machine slow
        // enough that nothing had happened yet, which is the failure mode a
        // negative assertion is most prone to.
        //
        // And every message is kept, not just the one that ended the wait: an
        // `Attention` arriving in the same batch as the `Exited` is exactly
        // what this is looking for, and a search that discarded its batch
        // would report the hole it was written to catch as a pass.
        let (mut seen, ended) =
            poll_collecting(&mut c, |m| matches!(m, HostMessage::Exited { .. }));
        assert!(ended, "the child never exited, so this proves nothing: {seen:?}");
        seen.extend(c.poll());
        assert!(
            !seen.iter().any(|m| matches!(m, HostMessage::Attention { .. })),
            "an unasked client was sent a signal: {seen:?}"
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
        peer.send(&mut c, &ClientMessage::Attach { session: addr, cols: 80, rows: 24, observe: false });

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

    /// The exit code the daemon knows has to reach the client.
    ///
    /// Asserted from the **client side**, over the wire, because that is where
    /// the bug was: `HostMessage::Exited` carried `code: None` unconditionally
    /// from protocol 2 until #299, and `Session::has_exited` was perfectly
    /// correct the whole time. Every host-side assertion agreed the daemon knew
    /// the child had gone; nothing checked that it said what it went with. A
    /// field that exists on the wire, is decoded by every client, and is never
    /// filled is indistinguishable from a host that cannot determine a status —
    /// which is exactly how it survived unnoticed.
    ///
    /// This is the only unforgeable exit status in the system. A block's comes
    /// from OSC 133;D, which any program can print.
    #[test]
    fn an_exited_child_reports_the_status_it_exited_with() {
        let (mut c, registry) = conn();
        let mut peer = authenticate(&mut c);
        peer.send(
            &mut c,
            &ClientMessage::CreateSession {
                command: exit_3_cmd(),
                cwd: String::new(),
                cols: 80,
                rows: 24,
            },
        );
        let addr = registry.list(config().host)[0].addr;
        peer.send(
            &mut c,
            &ClientMessage::Attach { session: addr, cols: 80, rows: 24, observe: false },
        );

        let mut seen = None;
        assert!(
            wait_for(|| {
                for m in c.poll() {
                    if let HostMessage::Exited { code, .. } = m {
                        seen = Some(code);
                        return true;
                    }
                }
                false
            }),
            "an attached client must be told its child exited"
        );

        assert_eq!(
            seen,
            Some(Some(3)),
            "the child exited with 3 and the client was told {seen:?}. `Some(None)` is the \
             regression this test exists for: the daemon reporting the exit while dropping \
             the status, which reads to a client as a host that could not determine one"
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
        peer.send(&mut c, &ClientMessage::Attach { session: addr, cols: 80, rows: 24, observe: false });

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
        let out = peer.send(&mut c, &ClientMessage::Attach { session: addr, cols: 80, rows: 24, observe: false });
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
            peer.send(&mut c, &ClientMessage::Attach { session: addr, cols: 80, rows: 24, observe: false });
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
        peer.send(&mut c, &ClientMessage::Attach { session: addr, cols: 80, rows: 24, observe: false });

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
