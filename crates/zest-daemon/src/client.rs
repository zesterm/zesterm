//! The client half of a daemon conversation, as a reusable object.
//!
//! Extracted from `remote.rs`'s private `Handshake` because attaching is no
//! longer the only conversation a client has: the tab strip lists sessions,
//! creates them on a chosen host, and closes them — none of which should have
//! to construct a `RemoteSession` to speak. `RemoteSession` builds on this
//! for its first connection and for every redial.
//!
//! **It lives here, in the daemon's own crate, rather than in the app.** It was
//! in `zest-app` while the app was the only client, and the daemon's tests and
//! its `attach`/`pair` examples each hand-rolled their own Hello → Challenge →
//! Auth. That was tolerable while a wrong peer failed loudly at the signature.
//! Protocol 3 made it untenable: the same steps now derive the key everything
//! afterwards is encrypted under, so a hand-rolled peer that gets the seal
//! switch or the transcript wrong produces a connection that authenticates
//! perfectly and then cannot be read — and nine copies is nine chances at it.
//! One implementation, exercised by the app *and* by every diagnostic.
//!
//! The shape is synchronous and inline on purpose, exactly as the handshake
//! always was: a failure is an error the caller can fall back from, not a
//! window that opens and then reports it has nothing to show.

use std::io::{Read, Write};
use std::sync::Arc;

use zest_mesh::identity::{ClientIdentity, Signature};
use zest_mesh::pairing::{Challenge, ClientHandshake};
use zest_mesh::secure::{DhPublic, SecureChannel};
use zest_proto::{
    frame, ClientMessage, FrameReader, HostMessage, SessionAddr, SessionInfo, PROTOCOL_VERSION,
};

use crate::DaemonError;

/// A host message a request/reply loop had to step over to reach its answer.
///
/// Loud, because stepping over it *loses* it — this is the second half of issue
/// #54's shape, and the half that is not fixed. `into_halves` now carries every
/// frame the client has not yet decoded, but a frame these loops decode and do
/// not match on is gone, and the sealed channel's counter has already advanced
/// past it so no retry exists.
///
/// It is a log rather than a queue because today it cannot fire: nothing on
/// this path subscribes to anything until `Attach`, and `Attach`'s own reply is
/// the first frame the host writes back. Plumbing a deferred queue through the
/// streaming reader for a case that cannot occur would add shape to the exact
/// loop this issue was about. If this line ever appears, that trade is off.
fn discarded(waiting_for: &str, msg: &HostMessage) {
    let kind = match msg {
        HostMessage::Welcome { .. } => "Welcome",
        HostMessage::Challenge { .. } => "Challenge",
        HostMessage::AuthPending { .. } => "AuthPending",
        HostMessage::AuthFailed { .. } => "AuthFailed",
        HostMessage::PairingRequested { .. } => "PairingRequested",
        HostMessage::EnrollResult { .. } => "EnrollResult",
        HostMessage::Sessions { .. } => "Sessions",
        HostMessage::Keyframe { .. } => "Keyframe",
        HostMessage::Update { .. } => "Update",
        HostMessage::Scrollback { .. } => "Scrollback",
        HostMessage::Exited { .. } => "Exited",
        HostMessage::Attention { .. } => "Attention",
        HostMessage::Progress { .. } => "Progress",
        HostMessage::Error { .. } => "Error",
        HostMessage::FileContents { .. } => "FileContents",
        HostMessage::FileWritten { .. } => "FileWritten",
        HostMessage::DirListing { .. } => "DirListing",
        HostMessage::GitDiffResult { .. } => "GitDiffResult",
        HostMessage::ConfigState { .. } => "ConfigState",
        HostMessage::ConfigWritten { .. } => "ConfigWritten",
        HostMessage::BlockMatches { .. } => "BlockMatches",
    };
    tracing::warn!(
        message = kind,
        %waiting_for,
        "discarded a host message while waiting for a reply; it is unrecoverable"
    );
}

/// A connection taken apart, so the caller can stream it from its own threads.
///
/// A struct rather than a tuple because `frames` is the field people forget:
/// named, a caller that ignores it is doing so visibly.
pub struct Halves {
    pub read: Box<dyn Read + Send>,
    pub write: Box<dyn Write + Send>,
    pub channel: Option<SecureChannel>,
    /// Whole frames the client already pulled off the socket and has not
    /// consumed. Feed this to the streaming reader; see
    /// [`DaemonClient::into_halves`].
    pub frames: FrameReader,
}

/// A listener for the approval wait: the six-digit matching code, and for
/// how many seconds the host will still honour it. See
/// [`DaemonClient::connect_with`].
pub type OnPending<'a> = &'a dyn Fn(&str, u32);

/// How [`DaemonClient::enroll`] went, as the daemon said it.
#[derive(Debug, Clone)]
pub struct EnrollOutcome {
    pub ok: bool,
    /// Who the machine now belongs to, when the control plane said.
    pub account: Option<String>,
    /// On `!ok`: the failure as the CLI would print it, for showing verbatim.
    pub message: String,
}

/// What this connection's `Hello` subscribes to.
///
/// A struct rather than two more `bool` parameters, so the flag a caller
/// does not want never appears at its call site — and so the next
/// subscription is a field here instead of a signature change everywhere.
#[derive(Debug, Clone, Copy, Default)]
pub struct Watch {
    /// `Hello.watch_sessions`: push the session list when it changes.
    pub sessions: bool,
    /// `Hello.watch_pairings`: push devices waiting for approval. Honoured
    /// on loopback only — a daemon that will not take this connection's
    /// `PairingDecision` silently never subscribes it either.
    pub pairings: bool,
    /// `Hello.watch_hosts`: send what this machine offers — its facts and its
    /// own profiles — and push it again when the far config changes (#262).
    ///
    /// Unlike `pairings` this is honoured on every transport: a machine's
    /// launch targets are what a client is there to see, and the whole point
    /// is reading them from somewhere else.
    pub hosts: bool,
    /// `Hello.watch_signals`: send [`HostMessage::Attention`] when an attached
    /// session rings, notifies, or otherwise asks to be noticed.
    ///
    /// Off unless asked for, and the reason is sharper than for the flags
    /// above: a `HostMessage` tag a client cannot decode does not go unread,
    /// it ends the connection. So this is not a subscription in the "would you
    /// like these" sense — it is the client saying it is new enough to survive
    /// them.
    pub signals: bool,
}

/// What kind of client this connection is, as the daemon is told.
///
/// Not a field on [`Watch`]: that type's doc says what it holds — the things
/// this connection *subscribes to* — and a declaration of what the client
/// **is** is not a subscription. Not a bare `bool` either, because
/// `connect_with(.., watch, true, on_pending)` at a call site says nothing
/// about which `true` that is.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum ClientKind {
    /// A person is driving. The default, and what every client that predates
    /// the flag is taken to be.
    #[default]
    Interactive,
    /// A program acting for a model. Declared once at startup — before it has
    /// read a byte of terminal text — and the daemon refuses it
    /// `PairingDecision` and `Enroll` on the strength of it, even on loopback,
    /// and never sends it a pairing code.
    Agent,
}

/// An authenticated connection to one daemon.
pub struct DaemonClient {
    read: Box<dyn Read + Send>,
    write: Box<dyn Write + Send>,
    frames: FrameReader,
    host: zest_proto::HostId,
    host_label: String,
    /// This connection's encryption, from the `Challenge` onwards.
    ///
    /// Two flags in one `Option`, as the daemon has: `Some(_)` means incoming
    /// frames are sealed, `sealing_out` means outgoing ones are. They flip at
    /// different moments because the switch is positional — the `Challenge`
    /// arrives in plaintext and the `Auth` that answers it is the first sealed
    /// frame going the other way.
    channel: Option<SecureChannel>,
    sealing_out: bool,
}
/// What to start, when a call starts something.
///
/// A struct rather than four more parameters: these travel together, three of
/// them are strings, and `create(command, cwd, ...)` with a third `&str` on the
/// end is a transposition the compiler cannot catch. Clippy reached the same
/// conclusion at eight arguments; naming them at the call site is the point.
///
/// All four describe a session being **created**. Adoption ignores every one
/// of them, which [`DaemonClient::open_session`] says out loud.
#[derive(Debug, Clone, Copy, Default)]
pub struct Launch<'a> {
    /// Empty means the host's default shell.
    pub command: &'a str,
    /// Opaque to us — it is resolved on the host, and may name a path this
    /// machine has never heard of.
    pub cwd: &'a str,
    /// Layered over the host's own `shell.env`, last-wins, **empty value
    /// unsets** — the convention `CommandSpec` and the setting already share.
    /// Unexpanded: see `profile`.
    pub env: &'a [(String, String)],
    /// The profile this launch came from, for resolving `env`'s placeholders.
    ///
    /// Travels **beside** the unexpanded values rather than being resolved
    /// into them: `${profile_dir}` names a directory on the machine that runs
    /// the shell, so the host expands it. Empty when no profile is behind the
    /// launch.
    pub profile: &'a str,
}


impl DaemonClient {
    /// Hello → Challenge → Auth → Welcome over an already-open transport.
    ///
    /// `expect_host` pins the far end: an address learned from an mDNS
    /// advertisement is a claim, and the host signing first is what lets the
    /// client hang up before revealing anything if the claim is false.
    /// `None` on loopback, where the socket's permissions are the answer.
    pub fn connect(
        read: Box<dyn Read + Send>,
        write: Box<dyn Write + Send>,
        identity: &Arc<ClientIdentity>,
        label: &str,
        expect_host: Option<zest_proto::HostId>,
        watch_sessions: bool,
    ) -> Result<Self, DaemonError> {
        Self::connect_impl(
            read,
            write,
            identity,
            label,
            expect_host,
            Watch { sessions: watch_sessions, pairings: false, hosts: false, signals: false },
            ClientKind::Interactive,
            None,
        )
    }

    /// [`Self::connect`], subscribing per `watch`.
    ///
    /// The fleet watcher's constructor: one loopback connection that hears
    /// both the session list and the approval queue. Separate from `connect`
    /// so its two flags do not spread a growing parameter list through every
    /// caller that wants neither.
    pub fn connect_watching(
        read: Box<dyn Read + Send>,
        write: Box<dyn Write + Send>,
        identity: &Arc<ClientIdentity>,
        label: &str,
        expect_host: Option<zest_proto::HostId>,
        watch: Watch,
    ) -> Result<Self, DaemonError> {
        Self::connect_impl(read, write, identity, label, expect_host, watch, ClientKind::Interactive, None)
    }

    /// [`Self::connect_watching`], with a listener for the approval wait.
    ///
    /// `on_pending` is called — on this same thread, mid-connect — each time
    /// the host answers `AuthPending`: a person over there is being asked to
    /// approve this device, and the six-digit code is what they compare.
    /// Without it the code exists only in a log line while the caller shows a
    /// spinner (#190). The connect keeps blocking afterwards; the host's
    /// eventual `Welcome` or `AuthFailed` resolves it.
    #[allow(clippy::too_many_arguments, reason = "a frozen seam whose two consumers spell every argument out; a params struct would rename them, not reduce them")]
    pub fn connect_with(
        read: Box<dyn Read + Send>,
        write: Box<dyn Write + Send>,
        identity: &Arc<ClientIdentity>,
        label: &str,
        expect_host: Option<zest_proto::HostId>,
        watch: Watch,
        kind: ClientKind,
        on_pending: Option<OnPending<'_>>,
    ) -> Result<Self, DaemonError> {
        Self::connect_impl(read, write, identity, label, expect_host, watch, kind, on_pending)
    }

    #[allow(clippy::too_many_arguments, reason = "a frozen seam whose two consumers spell every argument out; a params struct would rename them, not reduce them")]
    fn connect_impl(
        read: Box<dyn Read + Send>,
        write: Box<dyn Write + Send>,
        identity: &Arc<ClientIdentity>,
        label: &str,
        expect_host: Option<zest_proto::HostId>,
        watch: Watch,
        kind: ClientKind,
        on_pending: Option<OnPending<'_>>,
    ) -> Result<Self, DaemonError> {
        let mut client = Self {
            read,
            write,
            frames: FrameReader::new(),
            host: zest_proto::HostId::from_bytes([0; 32]),
            host_label: String::new(),
            channel: None,
            sealing_out: false,
        };

        let mut hs = ClientHandshake::new(Arc::clone(identity), label)
            .map_err(|e| DaemonError::Transport(e.to_string()))?;
        client.send(&ClientMessage::Hello {
            version: PROTOCOL_VERSION,
            client: identity.client_id(),
            label: label.to_string(),
            nonce: zest_proto::Nonce32::from_bytes(*hs.nonce().as_bytes()),
            dh: zest_proto::Pub32::from_bytes(hs.dh().0),
            watch_sessions: watch.sessions,
            watch_pairings: watch.pairings,
            watch_hosts: watch.hosts,
            watch_signals: watch.signals,
            agent: matches!(kind, ClientKind::Agent),
        })?;

        // Challenge -> Auth -> Welcome. Two round trips on connect, which on a
        // loopback socket is tens of microseconds and on a LAN is under a
        // millisecond -- paid once, against a session that lasts hours.
        loop {
            match client.recv()? {
                HostMessage::Challenge { version, host, label: host_label, nonce, dh, signature } => {
                    if version != PROTOCOL_VERSION {
                        return Err(DaemonError::Version {
                            ours: PROTOCOL_VERSION,
                            theirs: version,
                        });
                    }
                    let host_sig = Signature::from_slice(&signature.0)
                        .map_err(|e| DaemonError::Refused(e.to_string()))?;

                    // Verifies the host, signs, and derives the key, in that
                    // order and in one call. The ordering is the property —
                    // nothing is revealed and no key exists until the machine
                    // that answered has proved it is the one that was dialled —
                    // so it lives in `zest-mesh` where a test holds it, not
                    // here where it would be three steps a call site could
                    // reorder.
                    let (sig, _transcript, channel) = hs
                        .on_challenge(
                            expect_host,
                            &Challenge {
                                version,
                                host,
                                label: host_label,
                                nonce: zest_mesh::identity::Nonce::from_bytes(nonce.0),
                                dh: DhPublic(dh.0),
                                signature: host_sig,
                            },
                        )
                        .map_err(|e| DaemonError::Refused(e.to_string()))?;

                    // Incoming is sealed from the *next* frame; `send` flips
                    // outgoing after this `Auth`, which is itself sealed.
                    client.channel = Some(channel);
                    client.sealing_out = true;
                    client.send(&ClientMessage::Auth {
                        signature: zest_proto::Sig64::from_bytes(sig.to_bytes()),
                    })?;
                }
                HostMessage::Welcome { version, host, label } => {
                    if version != PROTOCOL_VERSION {
                        return Err(DaemonError::Version {
                            ours: PROTOCOL_VERSION,
                            theirs: version,
                        });
                    }
                    // The Welcome's host id is what the fleet model keys on:
                    // it is how the app learns its own local daemon's identity
                    // without a wire change.
                    client.host = host;
                    client.host_label = label;
                    return Ok(client);
                }
                // Not an error: the key is good and nobody has said yes yet.
                // The caller decides how long to wait.
                HostMessage::AuthPending { code, expires_in_secs } => {
                    tracing::info!(
                        %code,
                        expires_in_secs,
                        "waiting for this device to be approved on the host"
                    );
                    if let Some(notify) = on_pending {
                        notify(&code, expires_in_secs);
                    }
                }
                HostMessage::AuthFailed { reason, message } => {
                    return Err(DaemonError::Refused(format!("{reason:?}: {message}")));
                }
                HostMessage::Error { message, .. } => return Err(DaemonError::Refused(message)),
                _ => {}
            }
        }
    }

    /// The machine on the far end, from its signed Welcome.
    #[must_use]
    #[allow(dead_code, reason = "the fleet model is the second consumer, later in the #23 sequence")]
    pub fn host(&self) -> zest_proto::HostId {
        self.host
    }

    #[must_use]
    pub fn host_label(&self) -> &str {
        &self.host_label
    }

    /// What sessions this host has.
    pub fn list(&mut self) -> Result<Vec<SessionInfo>, DaemonError> {
        Ok(self.list_with_offer()?.0)
    }

    /// [`Self::list`], keeping the offer that rides the same reply.
    ///
    /// Two methods rather than one, and the reason is a bug this shape
    /// prevents rather than taste. A `watch_hosts` connection's *first* offer
    /// arrives on the first `Sessions` the host sends — which, for anything
    /// that lists before it starts reading pushes, is this reply. The daemon
    /// marks it sent on the way out, so a `list()` that drops it means the
    /// push loop waits for a generation bump that will not come until somebody
    /// edits a config on the far machine. Callers that subscribed must use
    /// this one; `list()` stays for the many that did not.
    ///
    /// `None` means nothing new to say — this connection did not subscribe,
    /// the far daemon publishes nothing, or it predates the field entirely.
    pub fn list_with_offer(
        &mut self,
    ) -> Result<(Vec<SessionInfo>, Option<zest_proto::HostOffer>), DaemonError> {
        self.send(&ClientMessage::ListSessions)?;
        loop {
            match self.recv()? {
                HostMessage::Sessions { sessions, offer, .. } => return Ok((sessions, offer)),
                HostMessage::Error { message, .. } => return Err(DaemonError::Refused(message)),
                other => discarded("Sessions", &other),
            }
        }
    }

    /// Start a fresh session and return its address.
    ///
    pub fn create(
        &mut self,
        launch: &Launch<'_>,
        cols: u16,
        rows: u16,
    ) -> Result<SessionAddr, DaemonError> {
        self.send(&ClientMessage::CreateSession {
            command: launch.command.to_string(),
            cwd: launch.cwd.to_string(),
            cols,
            rows,
            env: launch.env.to_vec(),
            profile: launch.profile.to_string(),
        })?;
        loop {
            match self.recv()? {
                HostMessage::Sessions { sessions, created, .. } => {
                    // The daemon names the created session outright. The
                    // `.last()` fallback survives only for an older daemon
                    // that predates the field — where it is racy against a
                    // concurrent creator, exactly as it always was.
                    if let Some(id) = created {
                        return Ok(SessionAddr::new(self.host, id));
                    }
                    let Some(newest) = sessions.last() else { continue };
                    return Ok(newest.addr);
                }
                HostMessage::Error { message, .. } => return Err(DaemonError::Refused(message)),
                other => discarded("Sessions", &other),
            }
        }
    }

    /// Find a session to use, or make one.
    ///
    /// The adopt policy lives client-side on purpose: `ListSessions` then the
    /// newest unattached, else create. See `AttachOptions::adopt` for why
    /// adopting is the GUI default today.
    ///
    /// The launch's `env` and `cwd` reach only a session this call *creates*.
    /// An adopted one is already running and its environment was fixed when it
    /// spawned — a process cannot be handed a new one, and pretending
    /// otherwise is how a profile would appear to apply while the shell it
    /// adopted belongs to a different identity entirely.
    pub fn open_session(
        &mut self,
        launch: &Launch<'_>,
        cols: u16,
        rows: u16,
        adopt: bool,
    ) -> Result<SessionAddr, DaemonError> {
        let existing = self.list()?;
        let adopted =
            adopt.then(|| existing.iter().rev().find(|s| !s.attached).map(|s| s.addr)).flatten();
        match adopted {
            Some(addr) => Ok(addr),
            None => self.create(launch, cols, rows),
        }
    }

    /// Attach to a session that already exists, and take its keyframe.
    ///
    /// Every attach starts with a keyframe, including a reattach: the host has
    /// no idea what this client still holds, and after a link drop neither
    /// does the client with any confidence.
    ///
    /// `cols`/`rows` are a vote in the size arbitration. A caller with no pane
    /// to protect wants [`Self::attach_observing`] instead.
    pub fn attach(
        &mut self,
        addr: SessionAddr,
        cols: u16,
        rows: u16,
    ) -> Result<(u64, zest_proto::Keyframe), DaemonError> {
        self.attach_with(addr, cols, rows, false)
    }

    /// Attach without voting on the session's size.
    ///
    /// For a client that renders nothing -- an agent, a probe, anything
    /// headless. `attach` would make it the smallest attached client and hold
    /// the session there for as long as it watched, which the human at the
    /// window cannot undo and is never told about (#274).
    ///
    /// The size is still stated, and is still what an older daemon will count,
    /// so pass what the session is already running at
    /// ([`zest_proto::SessionInfo`]) rather than something invented: against a
    /// daemon that predates the flag that turns a silent shrink into a
    /// no-op.
    pub fn attach_observing(
        &mut self,
        addr: SessionAddr,
        cols: u16,
        rows: u16,
    ) -> Result<(u64, zest_proto::Keyframe), DaemonError> {
        self.attach_with(addr, cols, rows, true)
    }

    fn attach_with(
        &mut self,
        addr: SessionAddr,
        cols: u16,
        rows: u16,
        observe: bool,
    ) -> Result<(u64, zest_proto::Keyframe), DaemonError> {
        self.send(&ClientMessage::Attach { session: addr, cols, rows, observe })?;
        loop {
            match self.recv()? {
                HostMessage::Keyframe {
                    seq,
                    cols,
                    rows,
                    rows_data,
                    attrs,
                    cursor,
                    modes,
                    blocks,
                    blocks_from,
                    title,
                    history_clears,
                    ..
                } => {
                    return Ok((
                        seq.0,
                        zest_proto::Keyframe {
                            cols,
                            rows,
                            rows_data,
                            attrs,
                            cursor,
                            modes: zest_core::Modes::from_bits_truncate(modes),
                            blocks,
                            blocks_from,
                            title,
                            history_clears,
                        },
                    ));
                }
                HostMessage::Error { message, .. } => return Err(DaemonError::Refused(message)),
                other => discarded("Keyframe", &other),
            }
        }
    }

    /// Ask this daemon to join an account with `code`, and wait for how it
    /// went.
    ///
    /// Blocking, deliberately: the daemon does the claim on a worker and
    /// answers when it settles, and this call's only caller is itself a
    /// worker thread (the app's enroll button). Loopback only — elsewhere
    /// the daemon refuses with an `Error`, which lands here as `Refused`.
    ///
    /// **A daemon that predates `Enroll` also answers `Error`** — "could not
    /// understand that message" — rather than closing (see `on_bytes`), so
    /// an old daemon under a new app surfaces as a `Refused` naming exactly
    /// that, and the caller can tell the person about `--enroll`.
    pub fn enroll(&mut self, code: &str) -> Result<EnrollOutcome, DaemonError> {
        self.send(&ClientMessage::Enroll { code: code.to_string() })?;
        loop {
            match self.recv()? {
                HostMessage::EnrollResult { ok, account, message } => {
                    return Ok(EnrollOutcome { ok, account, message });
                }
                HostMessage::Error { message, .. } => return Err(DaemonError::Refused(message)),
                other => discarded("EnrollResult", &other),
            }
        }
    }

    /// Answer a pending pairing request: allow (or refuse) `client` to attach.
    ///
    /// Fire-and-forget on the wire, like `close`: the daemon sends no reply
    /// on success. It is honoured on a loopback connection only — elsewhere
    /// the daemon answers with an `Error` and decides nothing, which the
    /// watcher's read loop will surface as a log line rather than a shell.
    pub fn decide_pairing(
        &mut self,
        client: zest_proto::ClientId,
        approve: bool,
    ) -> Result<(), DaemonError> {
        self.send(&ClientMessage::PairingDecision { client, approve })
    }

    /// End a session: the daemon hangs its child up and removes it.
    ///
    /// Fire-and-forget on the wire — the daemon sends no reply — so the only
    /// errors are transport ones.
    #[allow(dead_code, reason = "the fleet model is the second consumer, later in the #23 sequence")]
    pub fn close(&mut self, addr: SessionAddr) -> Result<(), DaemonError> {
        self.send(&ClientMessage::CloseSession { session: addr })
    }

    /// The next message from the host, blocking.
    ///
    /// For watchers: after `Hello.watch_sessions`, `Sessions` pushes arrive
    /// whenever the listing changes, and this is how they are read.
    pub fn next_message(&mut self) -> Result<HostMessage, DaemonError> {
        self.recv()
    }

    /// Bytes already taken off the socket and not yet turned into messages.
    ///
    /// Non-zero whenever the host's last write carried more than the reply this
    /// client was waiting for, which a batching daemon and a stream socket make
    /// ordinary rather than exceptional. Exposed so a caller can assert the
    /// handoff in [`into_halves`](Self::into_halves) did not lose them.
    #[must_use]
    pub fn pending(&self) -> usize {
        self.frames.pending()
    }

    /// Surrender the transport, for a caller that now wants to stream.
    ///
    /// The channel goes with it, and must: the counters are per connection, so
    /// a streaming caller that started a fresh channel would seal under nonces
    /// the daemon has already seen. It is `Option` only because the field is —
    /// after a successful `connect` it is always present.
    ///
    /// **The `FrameReader` goes with it too, and that is the load-bearing
    /// part.** `recv` reads up to 64 KiB and returns the *first* whole frame in
    /// it; anything else the host had already written is left in the buffer.
    /// Leaving it behind here is issue #54: the frames are already in
    /// userspace, so no timeout and no retry can bring them back, and since the
    /// seal's nonce is an implicit per-direction counter, the very next frame
    /// is then opened under the wrong nonce and the connection is over. It cost
    /// an empty window that never filled in.
    #[must_use]
    pub fn into_halves(self) -> Halves {
        Halves { read: self.read, write: self.write, channel: self.channel, frames: self.frames }
    }

    fn send(&mut self, msg: &ClientMessage) -> Result<(), DaemonError> {
        let body = frame::encode_body(msg).map_err(|e| DaemonError::Transport(e.to_string()))?;
        let body = match (&mut self.channel, self.sealing_out) {
            (Some(ch), true) => ch.seal(&body).map_err(|e| DaemonError::Transport(e.to_string()))?,
            _ => body,
        };
        let bytes = frame::frame_bytes(&body).map_err(|e| DaemonError::Transport(e.to_string()))?;
        self.write.write_all(&bytes).map_err(|e| DaemonError::Transport(e.to_string()))?;
        self.write.flush().map_err(|e| DaemonError::Transport(e.to_string()))
    }

    fn recv(&mut self) -> Result<HostMessage, DaemonError> {
        let mut buf = vec![0u8; 64 * 1024];
        loop {
            if let Ok(Some(body)) = self.frames.next_frame() {
                // A frame that will not open ends the connection rather than
                // being skipped: the counter has already advanced, so there is
                // no position to resume from.
                let body = match &mut self.channel {
                    Some(ch) => ch.open(&body).map_err(|e| DaemonError::Transport(e.to_string()))?,
                    None => body,
                };
                return frame::decode::<HostMessage>(&body)
                    .map_err(|e| DaemonError::Transport(e.to_string()));
            }
            // A signal is not a dropped connection: see `read_retrying`. Here
            // it reported `Transport("Interrupted system call")` from the
            // handshake, which reads as the daemon refusing a key it accepted.
            let n = crate::read_retrying(&mut self.read, &mut buf)
                .map_err(|e| DaemonError::Transport(e.to_string()))?;
            if n == 0 {
                return Err(DaemonError::Closed);
            }
            self.frames.feed(&buf[..n]);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::TcpStream;
    use std::sync::Mutex;
    use std::time::{Duration, Instant};

    /// An attach that is waiting for a person must hand the caller the code.
    ///
    /// The daemon answers `AuthPending` while an unknown device waits for
    /// approval, and until #190 the only client-side trace was a log line —
    /// the app showed a spinner while the six-digit matching code existed
    /// nowhere a person could see it. This drives the real LAN listener (the
    /// one transport that consults the trust store) with an *empty* store, so
    /// the handshake genuinely pends; a trusted client would skip approval and
    /// prove nothing, which is exactly how the watchdog bug survived its own
    /// test (see "Traps already paid for").
    #[test]
    fn a_pending_approval_reports_its_code_and_still_welcomes() {
        let registry = Arc::new(crate::Registry::new());
        let auth = Arc::new(crate::auth::Authenticator::new(
            Arc::new(zest_mesh::identity::HostIdentity::generate().expect("host key")),
            Arc::new(zest_mesh::trust::MemoryTrustStore::new()),
            zest_mesh::pairing::PairingQueue::new(),
            "lan-harness",
        ));
        let listener = crate::lan::LanListener::bind("127.0.0.1", 0).expect("bind the LAN listener");
        let addr = listener.local_addr();
        let config = crate::DaemonConfig {
            host: zest_proto::HostId::from_bytes([7; 32]),
            label: "lan-harness".into(),
            local_socket: String::new(),
            listen_lan: true,
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
            settings: None,
        };
        {
            let registry = Arc::clone(&registry);
            let auth = Arc::clone(&auth);
            std::thread::spawn(move || {
                let _ = listener.serve_forever(
                    config,
                    registry,
                    auth,
                    Arc::new(crate::lan::Gate::new()),
                );
            });
        }

        let identity = Arc::new(ClientIdentity::generate().expect("client key"));

        // The approver: a person on the host, played by a thread. It must not
        // answer before the request is queued — `decide` on an empty queue
        // resolves nothing and returns — so it waits for the client to have
        // *heard* `AuthPending`, which the daemon only sends after queueing.
        let seen: Arc<Mutex<Option<(String, u32)>>> = Arc::default();
        {
            let seen = Arc::clone(&seen);
            let auth = Arc::clone(&auth);
            let client_id = identity.client_id();
            std::thread::spawn(move || {
                let deadline = Instant::now() + Duration::from_secs(10);
                while Instant::now() < deadline {
                    if seen.lock().expect("seen lock").is_some() {
                        auth.decide(client_id, zest_mesh::pairing::Decision::Approve);
                        return;
                    }
                    std::thread::sleep(Duration::from_millis(10));
                }
            });
        }

        let stream = TcpStream::connect(addr).expect("dial the listener");
        let write = stream.try_clone().expect("clone the stream");
        let sink = Arc::clone(&seen);
        let on_pending = move |code: &str, secs: u32| {
            *sink.lock().expect("seen lock") = Some((code.to_string(), secs));
        };
        let client = DaemonClient::connect_with(
            Box::new(stream),
            Box::new(write),
            &identity,
            "test",
            None,
            Watch::default(),
            ClientKind::Interactive,
            Some(&on_pending),
        )
        .expect("an approved device must end up welcomed");

        let pending = seen.lock().expect("seen lock").clone();
        let (code, secs) = pending.expect(
            "on_pending never ran: the approval wait happened (the connect blocked \
             until the approver answered) and the caller was never told",
        );
        assert_eq!(
            code.len(),
            zest_mesh::pairing::PAIRING_CODE_DIGITS as usize,
            "the callback must carry the matching code itself, not a placeholder"
        );
        assert!(
            code.chars().all(|c| c.is_ascii_digit()),
            "the code is what a person compares digit by digit: {code:?}"
        );
        assert!(secs > 0, "an expiry of zero would tell the user the code is already dead");
        assert!(
            !client.host_label().is_empty(),
            "the callback informs the wait, it must not replace the Welcome"
        );
    }

    #[test]
    fn an_agent_connection_says_so_in_its_hello() {
        // The declaration is worth nothing unless it reaches the wire: a
        // `ClientKind` dropped in `connect_impl` would leave every daemon-side
        // refusal test passing against a client that never declares itself.
        // The `Hello` is plaintext -- the seal starts at the `Challenge` -- so
        // reading the first frame is enough, and the connect failing
        // afterwards is beside the point.
        for (kind, expected) in
            [(ClientKind::Agent, true), (ClientKind::Interactive, false)]
        {
            let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
            let addr = listener.local_addr().expect("addr");
            let seen: Arc<Mutex<Option<bool>>> = Arc::default();
            let sink = Arc::clone(&seen);
            let joiner = std::thread::spawn(move || {
                let (mut sock, _) = listener.accept().expect("accept");
                let mut frames = zest_proto::FrameReader::new();
                let mut buf = [0u8; 4096];
                loop {
                    let n = std::io::Read::read(&mut sock, &mut buf).expect("read");
                    if n == 0 {
                        return;
                    }
                    frames.feed(&buf[..n]);
                    if let Some(body) = frames.next_frame().expect("frame") {
                        let msg: zest_proto::ClientMessage =
                            zest_proto::frame::decode(&body).expect("a plaintext Hello");
                        if let zest_proto::ClientMessage::Hello { agent, .. } = msg {
                            *sink.lock().expect("seen lock") = Some(agent);
                        }
                        return;
                    }
                }
            });

            let stream = TcpStream::connect(addr).expect("dial");
            let write = stream.try_clone().expect("clone");
            let identity = Arc::new(ClientIdentity::generate().expect("client key"));
            let _ = DaemonClient::connect_with(
                Box::new(stream),
                Box::new(write),
                &identity,
                "test",
                None,
                Watch::default(),
                kind,
                None,
            );
            joiner.join().expect("reader thread");
            assert_eq!(
                *seen.lock().expect("seen lock"),
                Some(expected),
                "{kind:?} must reach the daemon as agent={expected}"
            );
        }
    }
}
