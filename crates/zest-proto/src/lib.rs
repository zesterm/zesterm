//! The mesh wire protocol.
//!
//! **This crate is a frozen contract.** `zest-daemon`, the web client and the
//! phone client are all built against these shapes. Once any of them ships,
//! changing one is a coordinated release across three codebases gated on an
//! app-store cycle — so a change here is landed deliberately, with every
//! consumer, in one commit, and written down. See `docs/CONTRACTS.md`.
//!
//! # What is on the wire, and why it is not PTY bytes
//!
//! Grid *deltas*, not the raw VT stream. A VT stream is stateful and
//! non-restartable, so a mobile client that drops 400ms has no way back short of
//! replaying from session start; deltas describe a *state*, so a slow link can
//! simply skip intermediate ones. → ADR-004.
//!
//! # The shape that is expensive to change later
//!
//! Every session-scoped message carries a [`SessionAddr`] — **a host and a
//! session, never a session alone**. zesterm is a fleet: the Mac, the Windows
//! box and the Linux builder are all hosts, and a client holds sessions from
//! several at once. Retrofitting the host half means a protocol version bump and
//! a change to every client, which is why it is here on day one even though the
//! first daemon will only ever serve one host. → ADR-006.

// Plain `std`, deliberately. `zest-core` holds a `no_std` line because a Rust
// wasm terminal was once plausible; this crate does not, because the clients
// that cross a language boundary are TypeScript and consume the generated
// `ts-rs` bindings rather than compiled Rust. Every Rust consumer -- the daemon,
// and the desktop app acting as a client of another machine -- is `std`.

use serde::{Deserialize, Serialize};

pub mod apply;
pub mod auth;
pub mod decode;
pub mod delta;
pub mod encode;
pub mod frame;
/// Public since #446: a file's content hash is a fixed-width value that
/// travels on this wire as hex, so it is spelled by the same module the ids
/// and signatures use rather than by a second loop in the daemon.
pub mod hex;
pub mod ids;

pub use apply::{Applied, Applier};
pub use auth::{AuthFailure, Nonce32, Pub32, Sig64};
pub use delta::{
    AttrDef, AttrId, BlockPayload, BlockState, CellMarks, CursorState, Delta, DeltaOp, Run,
    RowPayload,
};
pub use decode::GridView;
pub use encode::{Encoder, Keyframe};
pub use frame::{FrameError, FrameReader};
pub use ids::{ClientId, HostId, SessionAddr, SessionId};

/// Wire format version.
///
/// Bumped only for changes a peer cannot ignore. Additive changes ride on
/// `serde`'s tolerance of unknown fields instead, because a fleet is never
/// upgraded atomically — the phone updates through an app store, the Mac daemon
/// when someone remembers.
///
/// # 1 → 2
///
/// Two changes that a peer *cannot* ignore, deliberately made together because
/// the coordinated moment is the expensive part and doing it twice is the thing
/// to avoid. At the time of the bump the entire consumer set was `zest-daemon`,
/// its tests and its `attach` example. After a web or phone client ships, the
/// same change is a release across three codebases gated on an app-store cycle.
///
/// **A challenge/response handshake.** A signature carried on `Hello` alone
/// proves nothing that survives being recorded, because the client picks every
/// byte it signs. See [`auth`].
///
/// **Terminal modes on the wire** ([`DeltaOp::Modes`]). A client encodes its own
/// keystrokes and cannot do it correctly without them, so an attached session
/// had no mouse reporting, no bracketed paste and broken arrow keys in every
/// full-screen program.
///
/// Neither could ride on `serde`'s tolerance: a field an old peer silently
/// ignores is exactly wrong for authentication, and a mode a client never
/// receives is not a degraded experience but a broken one.
pub const PROTOCOL_VERSION: u16 = 3;

/// A monotonically increasing state number for one session.
///
/// The client acknowledges the highest it has applied; the host uses that to
/// decide whether the next update can be a delta or has to be a keyframe. This
/// is the entire resync mechanism, and it is why the sequence counter already
/// exists in `zest-core`.
///
/// `ts(type = "number")`, not the `bigint` a `u64` would derive: `rmp-serde`
/// writes the narrowest integer that fits, so what actually reaches a
/// JavaScript decoder is a plain `number` for every value a session can
/// reach — sequences count up from zero, and 2^53 of them at sixty per
/// second is several million years. A binding that says `bigint` is wrong at
/// runtime for every real frame and right only for absurd ones (#14).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
pub struct Seq(#[cfg_attr(feature = "ts", ts(type = "number"))] pub u64);

/// What a client sends.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "t", rename_all = "snake_case")]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
pub enum ClientMessage {
    /// First message on any connection. Identifies the client and its protocol.
    Hello {
        version: u16,
        client: ClientId,
        /// Human-readable, for the host's audit log and approval prompt.
        ///
        /// Covered by both signatures. An unsigned label would let anyone on
        /// the path make the host's approval prompt read "pair with
        /// andy-phone?" for a connection that is not the phone — and the prompt
        /// text is the human's entire decision input.
        label: String,
        /// The client's half of the handshake freshness.
        ///
        /// `#[serde(default)]` is deliberate and is not laxity. Without it a
        /// version-1 `Hello` fails to *decode*, and the peer is told "message
        /// was not understood" instead of the truthful "protocol 1 is not
        /// compatible with 2". The default is all zeroes, which
        /// [`Nonce32::is_absent`] recognises and the host refuses explicitly —
        /// so the version check runs first and says the useful thing.
        #[serde(default)]
        nonce: Nonce32,
        /// The client's ephemeral X25519 public key.
        ///
        /// Signed into the transcript alongside the nonces, so the *existing*
        /// Ed25519 signatures certify it — that is what removes the need for a
        /// certificate type or a stored static key. `#[serde(default)]` for the
        /// same reason `nonce` has it: a version-2 `Hello` must fail the
        /// version check with a truthful message rather than fail to decode
        /// and be told "message was not understood".
        #[serde(default)]
        dh: Pub32,
        /// Ask to be told when this host's session list changes.
        ///
        /// A live tab picker needs to hear about sessions other clients
        /// create, close, or attach to; without asking, `Sessions` only ever
        /// answers this connection's own requests and a listing goes stale
        /// the moment someone else acts. A field rather than a new message:
        /// both enums are tagged, an unknown tag fails the *whole* message on
        /// an older peer, and a field an old daemon ignores degrades to
        /// exactly today's behavior — the client notices no pushes arrive and
        /// falls back to polling.
        #[serde(default)]
        watch_sessions: bool,
        /// Ask to be told when a device is waiting to be approved.
        ///
        /// The desktop approval modal's subscription: [`HostMessage::PairingRequested`]
        /// pushes follow, request and tombstone alike. **Honoured on loopback
        /// only** — the same authority rule as `PairingDecision`, decided by
        /// the transport, so a LAN connection that sets it is silently never
        /// subscribed rather than refused. A field rather than a new message
        /// for `watch_sessions`' reason exactly: `#[serde(default)]` degrades
        /// to today's behavior on an older daemon, which simply never pushes.
        #[serde(default)]
        watch_pairings: bool,
        /// Ask what this machine can offer, and to be told when that changes.
        ///
        /// The launcher's subscription (#262). A client that sets it gets a
        /// [`HostOffer`] on the first `Sessions` reply and another whenever the
        /// far config reloads — which is how the `+` menu can show a machine's
        /// own profiles at all, and how a fleet card fills its `os` row with a
        /// fact rather than a dash.
        ///
        /// A field rather than a new message, for `watch_sessions`' reason
        /// exactly, and here the reason is sharper than it is there: a new
        /// `HostMessage` tag does not merely go unread on an older peer, it
        /// **kills the connection**, because `DaemonClient::recv` maps a frame
        /// it cannot decode to `DaemonError::Transport`. An old daemon ignores
        /// this flag and sends no offer; a new daemon sends none to a client
        /// that did not ask. Both degrade to exactly today's behaviour.
        #[serde(default)]
        watch_hosts: bool,
        /// Ask to be told when an attached session asks to be noticed.
        ///
        /// The gate on [`HostMessage::Attention`], and it is load-bearing
        /// rather than tidy: a `HostMessage` tag an older client cannot decode
        /// does not go unread, it **kills the connection** — `DaemonClient::recv`
        /// maps a frame it cannot decode to `DaemonError::Transport`. So the
        /// daemon must never send one to a client that did not ask, and this
        /// is how it knows. `#[serde(default)]` for `watch_sessions`' reason:
        /// an old daemon ignores the flag and sends nothing, which is exactly
        /// today's behaviour at both ends.
        #[serde(default)]
        watch_signals: bool,
    },
    /// The client's proof, answering [`HostMessage::Challenge`].
    ///
    /// Carries no id: the connection already knows which client is speaking,
    /// and accepting an id here would invite a check against the wrong one.
    Auth { signature: Sig64 },
    /// Approve or refuse a pending pairing request.
    ///
    /// **Honoured on a loopback connection only.** Approving a device is the
    /// authority of whoever is logged in at the machine, which is exactly what
    /// reaching the loopback socket demonstrates. Accepting it over the LAN
    /// would let one paired device enrol others.
    PairingDecision { client: ClientId, approve: bool },
    /// Join this machine to an account, with a code the signed-in app minted.
    ///
    /// **Honoured on a loopback connection only**, `PairingDecision`'s rule
    /// for `PairingDecision`'s reason: enrolling the machine is the authority
    /// of whoever is logged in at it. The daemon signs the code with its own
    /// host key, posts the claim to the control plane and keeps the token —
    /// the exact work of `zest-daemon --enroll <code>`, reachable without a
    /// terminal (issue #227). Answered by [`HostMessage::EnrollResult`], off
    /// a worker: the claim is a keychain probe and an HTTPS round trip, and
    /// the serve loop must not hold its connection lock across either.
    ///
    /// **A new tag, and the compatibility that costs.** Unlike an added
    /// field, a variant an old daemon has never heard of does not decode —
    /// but the daemon answers an unparseable message with `Error { "could
    /// not understand that message" }` and keeps serving (see `on_bytes`),
    /// and this message is only ever sent over loopback to the sender's own
    /// daemon, on a person's click. The app treats that `Error` reply as
    /// "daemon too old" and names the fallback (`--enroll`). See
    /// docs/CONTRACTS.md.
    Enroll { code: String },
    /// Resend the whole state; this client cannot apply what it is being sent.
    ///
    /// Detach-and-reattach has the same effect and is what a client had to do
    /// before, at the cost of tearing down a subscriber to fix a dropped frame.
    RequestKeyframe { session: SessionAddr },
    /// What sessions does this host have?
    ListSessions,
    /// Start a new one.
    CreateSession {
        /// Empty means the host's default shell.
        command: String,
        cwd: String,
        cols: u16,
        rows: u16,
    },
    /// Begin receiving updates for a session.
    ///
    /// The client states the size *it* will render at. A session attached from
    /// two devices at once is a real case — desk and phone — and the host
    /// reconciles rather than the last attach silently winning.
    ///
    /// `observe` withdraws from that reconciliation: the subscriber receives
    /// every keyframe and delta and casts **no vote**, which is what a client
    /// with no pane needs. The daemon has always had the state for it
    /// (`Subscriber.size` is an `Option`, and `None` never constrains); until
    /// now nothing could ask for it, because these two fields are not
    /// `Option`. A headless reader had to invent a size and thereby shrink
    /// somebody's window — permanently, because `reconcile_size` reports no
    /// change when the minimum does not move, so the human growing their
    /// window pushes nothing and the observer never learns to let go.
    ///
    /// **`cols`/`rows` are still sent, and still mean what they always did.**
    /// A daemon that predates this field ignores it and counts an ordinary
    /// vote, so an observer degrades to pinning the session at the size it
    /// found — recoverable, and unlike a `0, 0` sentinel, which `clamp_size`
    /// would turn into a 2x1 terminal on exactly those older daemons.
    ///
    /// A vote is withdrawn by re-attaching with `observe`, not by a second
    /// message: attaching twice on one connection is already how a client
    /// resyncs, and the handler already replaces the stale subscriber. So
    /// `Resize` needs no flag of its own, and does not get one.
    Attach {
        session: SessionAddr,
        cols: u16,
        rows: u16,
        #[serde(default)]
        observe: bool,
    },
    /// Stop receiving updates. The session keeps running.
    Detach { session: SessionAddr },
    /// Keystrokes, already encoded to terminal bytes by the client.
    ///
    /// Encoding client-side keeps modifier and keymap handling next to the
    /// keyboard that produced it, where the platform conventions are known.
    Input { session: SessionAddr, bytes: Vec<u8> },
    /// The client's viewport changed.
    Resize { session: SessionAddr, cols: u16, rows: u16 },
    /// Everything up to and including this sequence has been applied.
    Ack { session: SessionAddr, seq: Seq },
    /// Fetch history the client does not hold.
    ///
    /// `from_line` is a line id — `ts(type = "number")` for
    /// [`RowPayload::line`](crate::delta::RowPayload)'s reason.
    RequestScrollback {
        session: SessionAddr,
        #[cfg_attr(feature = "ts", ts(type = "number"))]
        from_line: i64,
        count: u32,
    },
    /// End the session and its child process.
    CloseSession { session: SessionAddr },
    /// Read a file on this host, for the built-in editor (#446).
    ///
    /// Host-scoped, not session-scoped: the connection is per-host, which is
    /// the routing. A session's cwd is what a *relative* `path` resolves
    /// against, and the client sends that as `cwd` rather than an address,
    /// because the question is about a filesystem and not about a terminal.
    ///
    /// Answered by [`HostMessage::FileContents`]. The `Enroll` bargain
    /// applies: an old daemon cannot decode the variant, answers its generic
    /// could-not-understand `Error` and keeps serving, and the app reads that
    /// as "this host's daemon is too old for the editor" and says so.
    ///
    /// No more authority than the client already holds — pairing grants
    /// `CreateSession { command }`, so `cat` through a shell is strictly more
    /// powerful than this.
    ReadFile {
        /// The file, as the host reads paths. Absolute, or resolved against
        /// `cwd`; opaque to the client, like `CreateSession.cwd`.
        path: String,
        /// The base a relative `path` resolves against. Empty means `path`
        /// must already be absolute.
        #[serde(default)]
        cwd: String,
    },
    /// Save a file this client previously read (#446).
    ///
    /// Answered by [`HostMessage::FileWritten`], and refused there rather
    /// than obeyed when `base_hash` no longer describes what is on disk.
    WriteFile {
        path: String,
        #[serde(default)]
        cwd: String,
        /// The whole new content. Bounded by `MAX_FRAME` at the encoder.
        data: Vec<u8>,
        /// [`HostMessage::FileContents::hash`] from the read this edit was
        /// based on, so a file that moved underneath is refused instead of
        /// clobbered. Empty means "create it, and refuse if it exists".
        ///
        /// Every way the disk can disagree — it changed, it is gone, it
        /// appeared where the client expected nothing — comes back as one
        /// `conflict`, so the client has one branch to write rather than four
        /// that each have to be told apart from an I/O failure.
        #[serde(default)]
        base_hash: String,
    },
}

/// What a host sends.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "t", rename_all = "snake_case")]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
pub enum HostMessage {
    /// Answer to a completed handshake. The connection is now served.
    Welcome { version: u16, host: HostId, label: String },
    /// Answer to `Hello`: the host's freshness, and its own proof.
    ///
    /// The host signs *first*, before it has any reason to trust the client.
    /// That is what lets a client dialling an address learned from an mDNS
    /// advertisement check that the machine which answered is the `HostId` the
    /// advertisement claimed, and hang up before revealing anything.
    ///
    /// The cost, stated rather than hidden: the host is a free signing oracle
    /// over an attacker-chosen nonce. Ed25519 has no chosen-message weakness
    /// and the signing domain confines these to what they already are, so this
    /// is a bounded CPU cost, not a key-recovery risk.
    Challenge {
        version: u16,
        host: HostId,
        label: String,
        nonce: Nonce32,
        /// The host's ephemeral X25519 public key. See `Hello::dh`.
        ///
        /// **This message is the switch.** Everything after a Challenge is
        /// sealed, in both directions; everything up to and including it is
        /// plaintext. Positional rather than per-message-type, because a table
        /// of which variants are encrypted is a table two independent
        /// implementations can disagree about — and the set of refusals that
        /// happen *before* a Challenge is exactly "the host never sent one".
        #[serde(default)]
        dh: Pub32,
        signature: Sig64,
    },
    /// The client proved its key, but nobody has approved it yet.
    ///
    /// `code` is the matching code to display. A person compares it with the
    /// one on the host's screen — a relay necessarily runs two handshakes with
    /// two nonce pairs, so the codes differ, and that is the only defence
    /// against a hostile network available before the data plane is encrypted.
    AuthPending { code: String, expires_in_secs: u32 },
    /// The connection will not be served. See [`AuthFailure`].
    AuthFailed { reason: AuthFailure, message: String },
    /// A device is asking to be paired; show it and call for a decision.
    ///
    /// Pushed to loopback connections that asked (`Hello.watch_pairings`) —
    /// the desktop app is a client of its own daemon, so the approval modal is
    /// a front end over this rather than a second mechanism. `remote` is for
    /// the prompt: "from 192.168.1.42".
    ///
    /// One message plays both halves, because a modal must also *close*:
    /// `resolved: false` means "show this" and `resolved: true` is the
    /// tombstone — the request left the queue (approved, denied, expired, or
    /// the device hung up), so stop showing it. A marker field rather than a
    /// second variant on purpose: both fields are `#[serde(default)]`, which
    /// the frozen-contract rule blesses as additive, while a new tag in a
    /// tagged enum fails the *whole* message on an older peer (`DeltaOp`'s
    /// lesson, docs/CONTRACTS.md). A tombstone carries only `client`; the
    /// other fields are empty rather than repeated, so nobody is tempted to
    /// read a code out of a message that means "there is nothing to compare".
    PairingRequested {
        client: ClientId,
        label: String,
        code: String,
        remote: String,
        /// How long the code is still worth comparing, mirroring
        /// [`HostMessage::AuthPending`]. `0` from a daemon that predates the
        /// field — treat as unknown, not as already-expired.
        #[serde(default)]
        expires_in_secs: u32,
        #[serde(default)]
        resolved: bool,
    },
    Sessions {
        sessions: Vec<SessionInfo>,
        /// The session this reply's `CreateSession` produced, when it did.
        ///
        /// Before this existed the client picked `sessions.last()`, which is
        /// wrong the moment two clients create on one host concurrently —
        /// each may adopt the other's shell. Absent on listings and pushes.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        created: Option<SessionId>,
        /// What this machine can offer: its facts, and its own profiles.
        ///
        /// Sent only to connections that asked (`Hello.watch_hosts`), and only
        /// when there is something new to say — `Some` on the first reply and
        /// again whenever the host's config reloads, `None` on every ordinary
        /// session push. `None` is therefore "nothing new", which is also
        /// exactly what a daemon predating this field sends, so one reading
        /// serves both.
        ///
        /// **On `Sessions` rather than a message of its own**, and that is the
        /// honest cost of the no-new-tag rule rather than a natural fit — see
        /// docs/CONTRACTS.md. It is less arbitrary than it first looks:
        /// `Sessions` is already "what this host has to offer you", already
        /// both the `ListSessions` reply and the watch push, and already what a
        /// client re-reads on every reconnect.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        offer: Option<HostOffer>,
    },
    /// How [`ClientMessage::Enroll`] went.
    ///
    /// Sent only to the connection that asked, once its worker settles. On
    /// `ok`, `account` is who the machine now belongs to, as the control
    /// plane names them (optional for `enroll()`'s reason: the display name
    /// is the control plane's to withhold). On `!ok`, `message` is the
    /// failure as the CLI would have printed it — the app shows it verbatim,
    /// because "the control plane refused this enrolment (409): code already
    /// used" is the person's next move and a summary of it is not.
    EnrollResult { ok: bool, account: Option<String>, message: String },
    /// A complete grid state.
    ///
    /// Sent on attach, and whenever the client's ack has fallen so far behind
    /// that a delta chain would be larger than the state it describes.
    Keyframe {
        session: SessionAddr,
        seq: Seq,
        cols: u16,
        rows: u16,
        rows_data: Vec<RowPayload>,
        attrs: Vec<AttrDef>,
        cursor: CursorState,
        /// `zest_core::Modes::bits()`. See [`DeltaOp::Modes`].
        ///
        /// A keyframe is a complete state, and a client encodes its own
        /// keystrokes — without this, a freshly attached session has broken
        /// arrow keys until the host next happens to change a mode.
        #[serde(default)]
        modes: u32,
        /// Every command block the host holds.
        ///
        /// Additive, so a peer that predates it still decodes — it simply has
        /// no semantic view of the session, which is what every peer had
        /// before this existed.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        blocks: Vec<BlockPayload>,
        /// The id from which `blocks` is authoritative; see
        /// [`Keyframe::blocks_from`](crate::Keyframe::blocks_from).
        ///
        /// Additive, so an older peer still decodes. Its default of 0 makes a
        /// keyframe from a host that predates this replace the client's list
        /// wholesale, which is what the browser's `GridView` already did.
        #[serde(default)]
        blocks_from: u32,
        /// The session's title at this instant.
        ///
        /// A keyframe is a complete state, and the title was the one piece of
        /// it that only travelled as a *change* (`DeltaOp::Title`) — so a tab
        /// attaching to a session already titled `vim` showed blank until the
        /// host next happened to retitle. Empty means untitled and travels as
        /// absent.
        #[serde(default, skip_serializing_if = "String::is_empty")]
        title: String,
        /// How many times ED 3 has destroyed the host's scrollback; see
        /// [`Keyframe::history_clears`](crate::Keyframe::history_clears).
        ///
        /// Additive (`blocks_from`'s precedent), so an older peer still
        /// decodes; its default of 0 never advances a replica's shadow, so a
        /// host that predates this simply never announces a clear — which is
        /// what every host did before it existed.
        #[serde(default)]
        history_clears: u32,
    },
    /// A change from `base` to `seq`.
    ///
    /// A client whose ack is not `base` must discard this and wait for a
    /// keyframe rather than applying it out of order.
    Update { session: SessionAddr, base: Seq, seq: Seq, delta: Delta },
    /// Requested history.
    Scrollback {
        session: SessionAddr,
        /// A line id — `ts(type = "number")` for
        /// [`RowPayload::line`](crate::delta::RowPayload)'s reason.
        #[cfg_attr(feature = "ts", ts(type = "number"))]
        from_line: i64,
        rows_data: Vec<RowPayload>,
        /// Every attribute these rows name.
        ///
        /// Additive, so a peer that predates it still decodes — but a peer that
        /// ignores it renders history in whatever style it last held, because
        /// scrollback is prepended rather than diffed and no later delta will
        /// define these ids.
        #[serde(default)]
        attrs: Vec<AttrDef>,
    },
    /// The child process ended.
    Exited { session: SessionAddr, code: Option<i32> },
    /// The session asked to be noticed: `BEL`, `OSC 9`, or `OSC 777;notify`.
    ///
    /// A `HostMessage` rather than a `DeltaOp` because it describes the
    /// *session*, not the grid — the same reason [`Self::Exited`] is one. It
    /// also needs no ordering against the rows: `AltScreen` has to come before
    /// them because it decides which grid they land in, and nothing about a
    /// bell decides anything.
    ///
    /// **Sent only to a client that set `Hello.watch_signals`.** A new
    /// `HostMessage` tag is not the harmless kind of addition — an older peer
    /// fails to decode the whole frame and `DaemonClient::recv` maps that to
    /// `DaemonError::Transport`, which *ends the connection*. The flag is what
    /// makes this additive in practice as well as in shape, and it is the same
    /// device `watch_sessions`, `watch_pairings` and `watch_hosts` each use.
    ///
    /// # There is no "unread" bit on the host, deliberately
    ///
    /// A latched flag would have to be cleared by someone, and with two
    /// devices watching one shell there is no answer to who. So the host
    /// reports the moment and each viewer keeps its own idea of what it has
    /// seen — which removes the question instead of answering it. A client
    /// that was not attached when the bell rang is simply never told, which is
    /// the right answer for a signal that means "look at this now".
    /// It carries no notification *text*, though `OSC 9` and `OSC 777` both
    /// supply some: nothing renders a body yet, and a wire field nothing reads
    /// is indistinguishable from one nothing can fill. `#[serde(default)]`
    /// makes adding it free the day something shows it.
    Attention { session: SessionAddr, cause: AttentionCause },
    /// A long job in this session reported how it is going (`OSC 9;4`).
    ///
    /// Behind `Hello.watch_signals`, like [`Self::Attention`], and a
    /// `HostMessage` for the same reason: it describes the *session*, not the
    /// grid. Nothing about a progress bar decides where a row lands, which is
    /// what a `DeltaOp` would have to be ordered against.
    ///
    /// **Unlike `Attention` this is state, and the difference is the whole
    /// reason they are two messages** even though they arrive through the same
    /// escape sequence. A client attaching halfway through a build must learn
    /// the bar is at 60%; a client attaching after a bell has rung must hear
    /// nothing about it. So the host keeps a per-subscriber shadow of what it
    /// last sent here — a fresh subscriber's is `None`, so it is told at once
    /// if the session is already busy — and keeps no memory at all of a bell.
    Progress { session: SessionAddr, progress: delta::Progress },
    /// Something went wrong, phrased for a person.
    Error { session: Option<SessionAddr>, message: String },
    /// The answer to [`ClientMessage::ReadFile`] (#446).
    ///
    /// A reply, not a push: only the asker receives it, so — unlike
    /// `Attention` and `Progress` — no `Hello` flag guards it. A peer that
    /// cannot decode this tag structurally never asks for it. `path` echoes
    /// the question, which is the correlation: an editor that moved on drops
    /// a stale answer by comparing paths.
    ///
    /// A refusal is *this* message with `error` set, never `Error` — a
    /// sessionless `Error` is what an **old** daemon says, and the app reads
    /// that as "too old". The two must not be confusable.
    FileContents {
        /// The path as the host resolved it: absolute, symlinks followed.
        ///
        /// What the editor titles itself with, and deliberately not what was
        /// asked: a relative path resolves against a cwd the shell reported,
        /// and anything that can print can forge one. The resolved path is
        /// disk truth, which is the only kind worth showing a person.
        path: String,
        /// At most the read cap; `truncated` says the disk holds more.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        data: Vec<u8>,
        /// More existed than `data` carries. Said rather than silently cut.
        #[serde(default)]
        truncated: bool,
        /// A NUL in the first 8 KiB. Display guidance, not a gate — the bytes
        /// are sent either way, and what to do with them is the client's call.
        #[serde(default)]
        binary: bool,
        /// SHA-256, lowercase hex, of the content — the base a later
        /// [`ClientMessage::WriteFile`] is checked against.
        ///
        /// **Empty when `truncated`, and that is the mechanism rather than an
        /// omission.** An empty `base_hash` means "create, and refuse if it
        /// exists", and the file plainly exists — so a buffer holding only the
        /// first few megabytes of a larger file *cannot* save over the rest of
        /// it. The alternative, hashing a file of any size to hand back a base
        /// the client must then be trusted not to use, is unbounded work
        /// guarded by good intentions.
        #[serde(default)]
        hash: String,
        /// Bytes on disk. `ts(type = "number")` for
        /// [`RowPayload::line`](crate::delta::RowPayload)'s reason (#14): a
        /// file past 2^53 bytes is not an editor's problem.
        #[serde(default)]
        #[cfg_attr(feature = "ts", ts(type = "number"))]
        size: u64,
        /// The file cannot be written by the daemon's user.
        #[serde(default)]
        readonly: bool,
        /// Why there is no content, when there is none for a *reason*.
        /// Empty when the read simply succeeded — an empty file and a refused
        /// one must not render the same.
        #[serde(default, skip_serializing_if = "String::is_empty")]
        error: String,
    },
    /// The answer to [`ClientMessage::WriteFile`] (#446).
    FileWritten {
        /// The resolved path, as [`Self::FileContents::path`].
        path: String,
        /// On success, the hash of what was written — the editor's next base.
        /// On a conflict, the hash of what *stands* on disk, which is what
        /// lets the app offer "reload theirs" without a second round trip and
        /// lets it tell "somebody saved exactly what I have" apart from a real
        /// divergence.
        #[serde(default)]
        hash: String,
        /// The disk no longer matched `base_hash`, so nothing was written.
        ///
        /// A bool rather than one of several error strings because it is the
        /// one answer the client must *branch* on rather than display.
        #[serde(default)]
        conflict: bool,
        /// Why nothing was written, phrased for a person. Empty on success.
        #[serde(default, skip_serializing_if = "String::is_empty")]
        error: String,
    },
}

/// Why a session is asking to be noticed.
///
/// Mirrors `zest_core::AttentionCause` for [`delta::BlockState`]'s reason:
/// this side carries a `ts_rs` derive, and `zest-core` has to keep building
/// for `wasm32` without one.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
pub enum AttentionCause {
    /// `BEL` — the oldest "hey" in the terminal.
    Bell,
    /// `OSC 9` or `OSC 777;notify`: the program asked for a desktop
    /// notification.
    Notify,
}

impl From<zest_core::AttentionCause> for AttentionCause {
    fn from(c: zest_core::AttentionCause) -> Self {
        match c {
            zest_core::AttentionCause::Bell => Self::Bell,
            zest_core::AttentionCause::Notify => Self::Notify,
        }
    }
}

/// A session as it appears in a listing.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
pub struct SessionInfo {
    pub addr: SessionAddr,
    pub title: String,
    pub cwd: String,
    pub cols: u16,
    pub rows: u16,
    /// True while a program has the alternate screen.
    ///
    /// The phone client switches from blocks view to grid view on exactly this
    /// signal — `vim` and `htop` need a grid, everything else is better as a
    /// list. Tracked host-side, so the decision is made from truth rather than
    /// guessed from output.
    pub alt_screen: bool,
    /// Whether a client is currently attached.
    pub attached: bool,
    /// What the session is standing in — branch, venv, kube context — computed
    /// by the daemon that owns it, so every client (and every agent) renders
    /// the same chips without running anything in the user's shell.
    ///
    /// `#[serde(default)]` for the `HostOffer.os` reason: a required field an
    /// older daemon omits fails the whole `Sessions` decode, which
    /// `DaemonClient::recv` maps to a transport error, which costs the client
    /// its connection. `None` is "the daemon did not say", never "no context".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts", ts(optional))]
    pub context: Option<SessionContext>,
    /// A command is running in this session right now.
    ///
    /// The tail block's word, so the ⌘K picker and the fleet cards can say it
    /// for a session this window is not attached to — which they structurally
    /// could not before: the listing carried nothing, and nothing pushed it on
    /// a block transition anyway. False from a shell emitting no markers, like
    /// every other blocks-derived fact.
    #[serde(default)]
    pub busy: bool,
}

/// Where the session is standing, as its own daemon sees it.
///
/// Everything here is *display*. A chip, a picker row, an agent's situational
/// awareness — never a gate: half of it comes from the filesystem as of a
/// debounce window ago, and the other half will come from shell-emitted
/// escapes anything that can print can forge (ADR-015's line, applied to
/// context instead of exit codes). Each fact says which half it is.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
pub struct SessionContext {
    /// The repository the cwd is inside, when it is inside one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts", ts(optional))]
    pub git: Option<GitContext>,
    /// Everything else, as labeled strings: `venv`, `kube`, `node`, …
    ///
    /// The `HostOffer` philosophy — facts, deliberately, not a capability
    /// matrix. A client meeting a key it does not know shows one chip fewer;
    /// adding a fact is never a protocol change.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub facts: Vec<ContextFact>,
    /// Bumped by the producing daemon whenever this context changes, so a
    /// client can skip rebuilding chrome for a listing push that moved for
    /// some other reason. Monotonic per session, not wall time.
    ///
    /// `ts(type = "number")` for the #14 reason: `rmp-serde` writes the
    /// narrowest integer that fits, so this reaches JavaScript as a plain
    /// `number` for every count a daemon will ever reach.
    #[serde(default)]
    #[cfg_attr(feature = "ts", ts(type = "number"))]
    pub revision: u64,
}

/// Git, structured, because three clients render its parts differently.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
pub struct GitContext {
    /// The branch name, or the short hash when detached.
    pub branch: String,
    pub detached: bool,
    /// Whether the tree has uncommitted changes.
    ///
    /// `None` until something actually asks git — answering honestly means a
    /// subprocess, and the background probe's first answer lands a beat
    /// after the branch (#432). An `Option` because "clean" and "unknown"
    /// rendering the same chip is exactly the dash-pretending-to-be-a-fact
    /// the `HostOffer` fields warn about.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts", ts(optional))]
    pub dirty: Option<bool>,
    /// How many files say so (`git status --porcelain -uno` lines), when
    /// the tree is dirty. `None` when clean or unknown — a `0` beside
    /// `dirty: true` would be two fields disagreeing in public.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts", ts(type = "number", optional))]
    pub changed: Option<u32>,
}

/// One labeled thing true about the session's surroundings.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
pub struct ContextFact {
    /// `venv`, `conda`, `kube`, `node`, `python`, `rust`, `ssh_host`, …
    pub key: String,
    pub value: String,
    /// Who said so — and therefore how far it can be trusted.
    pub source: ContextSource,
}

/// Who produced a [`ContextFact`], carried per fact rather than documented,
/// because the distinction has to survive into every payload an agent reads —
/// a tool description is not where a trust boundary lives (ADR-015).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
pub enum ContextSource {
    /// The daemon read it off the filesystem itself.
    DaemonProbe,
    /// The shell (or anything that can print an escape) reported it.
    ShellReport,
}

/// What a machine can offer a client: what it *is*, and what it can launch.
///
/// Answers two questions the protocol could not ask before (#262): the fleet
/// card's `os` row, and the `+` launcher's "what can I run on that machine".
/// Both were structurally unanswerable — `Welcome { host, label }` was the
/// entire description of a machine a client ever received.
///
/// Facts, deliberately, and not a capability matrix. Nothing here is matched
/// on to decide whether a feature is available; it is rendered, and a client
/// that meets an empty string shows one row fewer.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
pub struct HostOffer {
    /// `std::env::consts::OS` — `windows`, `macos`, `linux`.
    ///
    /// `#[serde(default)]` like every other field here, and the reason is the
    /// same one that made this a field rather than a new message: a required
    /// field a future peer omits does not fail *softly*. Decoding the whole
    /// `Sessions` message fails, and `DaemonClient::recv` maps that to a
    /// transport error, which ends the connection — so one absent string would
    /// cost a client its session list too.
    #[serde(default)]
    pub os: String,
    /// `std::env::consts::ARCH` — `x86_64`, `aarch64`.
    #[serde(default)]
    pub arch: String,
    /// The OS as the machine names itself — `Darwin 24.5.0`,
    /// `Linux 6.8.0-31-generic` — best effort.
    ///
    /// Carries the kernel's *name* as well as its release, because this is the
    /// only place that name reaches a client: [`Self::os`] is
    /// `std::env::consts::OS`, which says `macos` where design §7's card says
    /// `Darwin`.
    ///
    /// Empty when unknown, never a placeholder: those cards show only what is
    /// actually known, and a dash pretending to be a fact is the thing that
    /// rule exists to prevent.
    #[serde(default)]
    pub os_version: String,
    /// What a session with an empty `CreateSession.command` will run.
    ///
    /// So a launcher row for a remote profile with no command of its own can
    /// say what it will start, instead of showing this machine's shell for a
    /// session that will run the far one's.
    #[serde(default)]
    pub default_shell: String,
    /// This machine's own launch targets, resolved through its own Defaults.
    #[serde(default)]
    pub profiles: Vec<HostProfile>,
    /// Whether this machine's daemon holds an account token in its own store.
    ///
    /// The daemon's own word, and deliberately not the account table's: a host
    /// key can be a live row in the account's `hosts` table while the daemon
    /// on that machine holds no token at all — a re-installed machine, a
    /// cleared credential store, `--logout` — and the two facts sharing the
    /// word "enrolled" is how the enrol affordance hid exactly when it was
    /// needed (#245). Still a fact about the machine, not a capability bit:
    /// what a client renders from it is the machine's *state*.
    ///
    /// Three states on purpose. `None` is "the daemon did not say" — a daemon
    /// predating this field, or a credential store that could not be read
    /// (a locked keychain is not a machine that never enrolled) — and a
    /// client falls back to whatever else it knows. `Some(false)` is a
    /// positive fact: the store was readable and holds nothing.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub has_account_token: Option<bool>,
}

/// One launch target a machine publishes.
///
/// The launcher-facing half of `zest_config::profiles::ProfileMeta`, and
/// notably **not** its `host` or `ask_host` fields: a profile published by a
/// machine is pinned to that machine by construction, and re-sending a `host`
/// key invites a client to resolve a label against its own fleet and send the
/// launch somewhere else entirely.
///
/// Values arrive already folded through that host's `profiles.defaults`, so a
/// client renders and launches what it was told rather than re-running a
/// cascade over a config it does not have.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
pub struct HostProfile {
    pub name: String,
    /// Empty means this host's default shell — the same convention
    /// `CreateSession.command` uses, so a launch can pass it straight through.
    #[serde(default)]
    pub command: String,
    #[serde(default)]
    pub starting_directory: String,
    #[serde(default)]
    pub icon: String,
    #[serde(default)]
    pub color_scheme: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tab_color: Option<u8>,
}

#[derive(Debug, thiserror::Error)]
pub enum ProtoError {
    #[error("protocol version {theirs} is not compatible with {ours}")]
    Version { ours: u16, theirs: u16 },
    #[error("message was not understood: {0}")]
    Malformed(String),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_session_scoped_message_carries_a_host() {
        // The single most expensive thing to retrofit. If a variant is ever
        // added that names a session without its host, the fleet stops being
        // addressable and every client needs a protocol bump.
        let addr = SessionAddr::new(HostId::from_bytes([7; 32]), SessionId(3));
        let msg = ClientMessage::Input { session: addr, bytes: vec![b'a'] };
        let json = serde_json::to_string(&msg).expect("serialize");
        assert!(json.contains("host"), "no host in {json}");
        assert!(json.contains("session"), "no session id in {json}");
    }

    #[test]
    fn messages_round_trip() {
        let msg = ClientMessage::Hello {
            version: PROTOCOL_VERSION,
            client: ClientId::from_bytes([1; 32]),
            label: "phone".to_string(),
            nonce: Nonce32::from_bytes([4; 32]),
            dh: Pub32::from_bytes([6; 32]),
            watch_sessions: false,
            watch_pairings: false,
            watch_hosts: false,
            watch_signals: false,
        };
        let json = serde_json::to_string(&msg).expect("serialize");
        let back: ClientMessage = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(msg, back);
    }

    #[test]
    fn a_peer_that_predates_the_host_offer_decodes_on_both_sides() {
        // #262 added one field to each enum, and the whole justification for
        // that shape over a new variant rests on this test. A new tag would
        // not merely go unread on an older peer: `DaemonClient::recv` maps an
        // undecodable `HostMessage` to a transport error, which tears the
        // connection down. So both directions must parse.

        // A client built before `watch_hosts`: the daemon must read it as an
        // ordinary unsubscribed connection, not refuse the message.
        let json = r#"{"t":"hello","version":3,
            "client":"0101010101010101010101010101010101010101010101010101010101010101",
            "label":"old","nonce":"0404040404040404040404040404040404040404040404040404040404040404",
            "dh":"0606060606060606060606060606060606060606060606060606060606060606",
            "watch_sessions":true,"watch_pairings":false}"#;
        let parsed: ClientMessage = serde_json::from_str(json).expect("a pre-#262 Hello decodes");
        assert!(
            matches!(parsed, ClientMessage::Hello { watch_hosts: false,
            watch_signals: false, watch_sessions: true, .. }),
            "the absent field is 'did not subscribe', and the rest still reads"
        );

        // A daemon built before `offer`: the client must read its listing.
        let json = r#"{"t":"sessions","sessions":[]}"#;
        let parsed: HostMessage = serde_json::from_str(json).expect("a pre-#262 Sessions decodes");
        assert!(
            matches!(parsed, HostMessage::Sessions { offer: None, created: None, .. }),
            "no offer means 'nothing new to say' — the same branch a current daemon takes \
             on an ordinary session push, so the client needs one reading and not two"
        );
    }

    #[test]
    fn an_absent_offer_is_absent_on_the_wire_rather_than_a_null() {
        // `skip_serializing_if` is what keeps the common case free: every
        // ordinary session push carries no offer, and a machine with tabs open
        // on it sends a great many of those.
        let msg = HostMessage::Sessions { sessions: Vec::new(), created: None, offer: None };
        let json = serde_json::to_string(&msg).expect("serialize");
        assert!(!json.contains("offer"), "an absent offer costs no bytes: {json}");

        let msg = HostMessage::Sessions {
            sessions: Vec::new(),
            created: None,
            offer: Some(HostOffer { os: "linux".into(), ..Default::default() }),
        };
        let json = serde_json::to_string(&msg).expect("serialize");
        let back: HostMessage = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(msg, back, "and a present one round-trips");
    }

    #[test]
    fn a_published_profile_names_no_host_of_its_own() {
        // Structural, not a convention: a profile published by a machine is
        // pinned to that machine by construction. A `host` field here would
        // invite a client to resolve a label against its *own* fleet and send
        // the launch somewhere else entirely — the one way this feature could
        // run a command on the wrong computer.
        let json = serde_json::to_string(&HostProfile {
            name: "ubuntu".into(),
            command: "wsl.exe -d Ubuntu".into(),
            ..Default::default()
        })
        .expect("serialize");
        assert!(!json.contains("\"host\""), "no host key travels with a profile: {json}");
        assert!(!json.contains("ask_host"), "nor an ask_host: {json}");
    }

    #[test]
    fn a_version_one_hello_still_decodes() {
        // The entire reason `Hello.nonce` is `#[serde(default)]`.
        //
        // Without it this fails to *decode*, and the peer is told "could not
        // understand that message" -- which sends whoever is debugging it
        // looking for a framing bug. What they need to hear is that their
        // protocol version is too old, and that answer is only reachable if
        // the message parses far enough for the version to be read.
        let json = r#"{"t":"hello","version":1,"client":"0101010101010101010101010101010101010101010101010101010101010101","label":"old"}"#;
        let parsed: ClientMessage = serde_json::from_str(json).expect("a v1 Hello must decode");
        let ClientMessage::Hello { version, nonce, .. } = parsed else {
            panic!("expected Hello");
        };
        assert_eq!(version, 1, "so the version check can refuse it by name");
        assert!(nonce.is_absent(), "a missing nonce must be recognisable, not silently zero");
    }

    #[test]
    fn a_hello_without_watch_pairings_still_decodes() {
        // The entire reason the field is `#[serde(default)]`: every client
        // shipped before the approval modal sends a Hello without it, and a
        // Hello that fails to decode reads as a framing bug rather than as
        // an old peer. Through the real msgpack framing, not serde_json --
        // `to_vec_named` is what the daemon decodes.
        #[derive(serde::Serialize)]
        struct OldHello<'a> {
            t: &'a str,
            version: u16,
            client: ClientId,
            label: &'a str,
            nonce: Nonce32,
            dh: Pub32,
            watch_sessions: bool,
        }
        let old = rmp_serde::to_vec_named(&OldHello {
            t: "hello",
            version: PROTOCOL_VERSION,
            client: ClientId::from_bytes([9; 32]),
            label: "old-client",
            nonce: Nonce32::from_bytes([1; 32]),
            dh: Pub32::from_bytes([2; 32]),
            watch_sessions: true,
        })
        .expect("encode");
        let parsed: ClientMessage = crate::frame::decode(&old).expect("an old Hello must decode");
        let ClientMessage::Hello { watch_sessions, watch_pairings, .. } = parsed else {
            panic!("expected Hello");
        };
        assert!(watch_sessions, "the fields the old client did send survive");
        assert!(
            !watch_pairings,
            "absent must mean 'not subscribed', exactly today's behavior"
        );
    }

    #[test]
    fn a_pairing_requested_without_the_new_fields_still_decodes() {
        // An app talking to an older daemon: `expires_in_secs` and `resolved`
        // are `#[serde(default)]` so its pushes still parse -- 0 reads as
        // "expiry unknown" (never as already-expired) and absent `resolved`
        // is an ordinary request, both of which are what that daemon meant.
        #[derive(serde::Serialize)]
        struct OldPush<'a> {
            t: &'a str,
            client: ClientId,
            label: &'a str,
            code: &'a str,
            remote: &'a str,
        }
        let old = rmp_serde::to_vec_named(&OldPush {
            t: "pairing_requested",
            client: ClientId::from_bytes([7; 32]),
            label: "andy-phone",
            code: "481502",
            remote: "192.168.1.42:7717",
        })
        .expect("encode");
        let parsed: HostMessage = crate::frame::decode(&old).expect("an old push must decode");
        let HostMessage::PairingRequested { code, expires_in_secs, resolved, .. } = parsed else {
            panic!("expected PairingRequested");
        };
        assert_eq!(code, "481502");
        assert_eq!(expires_in_secs, 0, "absent expiry is unknown, not zero minutes");
        assert!(!resolved, "an old daemon only ever pushes live requests");
    }

    #[test]
    fn an_absent_nonce_is_not_a_valid_one() {
        // The other half: having decoded, the all-zero default must not be
        // mistaken for freshness a client actually supplied.
        let msg = ClientMessage::Hello {
            version: PROTOCOL_VERSION,
            client: ClientId::from_bytes([2; 32]),
            label: "no-nonce".into(),
            nonce: Nonce32::default(),
            dh: Pub32::default(),
            watch_sessions: false,
            watch_pairings: false,
            watch_hosts: false,
            watch_signals: false,
        };
        let ClientMessage::Hello { nonce, dh, .. } = msg else { panic!("expected Hello") };
        assert!(nonce.is_absent());
        // The DH key gets the same treatment for a sharper reason: a nonce
        // nobody supplied only costs freshness, while a key nobody supplied
        // produces a shared secret an attacker already knows -- and it would
        // look like a working channel on both sides.
        assert!(dh.is_absent());
    }

    #[test]
    fn a_create_sessions_cwd_crosses_the_wire_intact() {
        // The profile launch path (issue #175) rides this field for
        // `starting_directory` — it was already on the frame, so no growth
        // was needed, but nothing pinned it. Through the real msgpack
        // framing, not serde_json: `to_vec_named` is what the daemon
        // decodes, and the path is deliberately opaque — `\\wsl$\...` is a
        // path only the *host* can interpret, so any normalization here
        // would be corruption.
        let msg = ClientMessage::CreateSession {
            command: "wsl.exe -d Ubuntu-24.04".to_string(),
            cwd: r"\\wsl$\Ubuntu-24.04\home\andy".to_string(),
            cols: 120,
            rows: 34,
        };
        let body = crate::frame::encode_body(&msg).expect("encode");
        let back: ClientMessage = crate::frame::decode(&body).expect("decode");
        assert_eq!(msg, back, "command, cwd and size all survive the frame");
    }

    #[test]
    fn the_editors_messages_cross_the_wire_intact() {
        // Through the real msgpack framing, not serde_json, for
        // `a_create_sessions_cwd_crosses_the_wire_intact`'s reason:
        // `to_vec_named` is what the peer decodes.
        for msg in [
            ClientMessage::ReadFile { path: "src/main.rs".into(), cwd: "/home/a/p".into() },
            ClientMessage::WriteFile {
                path: "/abs/f.txt".into(),
                cwd: String::new(),
                // A high byte and a NUL: `Vec<u8>` goes through serde's seq
                // path under `to_vec_named`, so a byte that is not ASCII is
                // the one worth pinning.
                data: vec![0xff, 0x00, b'x'],
                base_hash: "abc123".into(),
            },
        ] {
            let body = crate::frame::encode_body(&msg).expect("encode");
            let back: ClientMessage = crate::frame::decode(&body).expect("decode");
            assert_eq!(msg, back);
        }

        for msg in [
            HostMessage::FileContents {
                path: "/home/a/p/src/main.rs".into(),
                data: vec![b'f', b'n', 0xc3, 0xa9],
                truncated: true,
                binary: false,
                hash: String::new(),
                // Past a u32, to pin the `ts(type = "number")` field against a
                // narrowing that msgpack's own encoding would happily hide.
                size: 5_000_000_000,
                readonly: true,
                error: String::new(),
            },
            HostMessage::FileWritten {
                path: "/home/a/p/src/main.rs".into(),
                hash: "deadbeef".into(),
                conflict: true,
                error: "the file changed on disk since it was opened".into(),
            },
        ] {
            let body = crate::frame::encode_body(&msg).expect("encode");
            let back: HostMessage = crate::frame::decode(&body).expect("decode");
            assert_eq!(msg, back);
        }
    }

    #[test]
    fn a_minimal_file_reply_decodes_through_its_defaults() {
        // Every field but the tag and `path` is `#[serde(default)]` and
        // skipped when empty, so what actually goes out for a plain empty file
        // is close to this map. A peer that could not decode it would fail on
        // the *successful* case, which is the one nobody tests by hand.
        let mut buf = Vec::new();
        let mut s = rmp_serde::Serializer::new(&mut buf).with_struct_map();
        use serde::Serialize as _;
        #[derive(Serialize)]
        struct Minimal<'a> {
            t: &'a str,
            path: &'a str,
        }
        Minimal { t: "file_contents", path: "/tmp/empty" }.serialize(&mut s).expect("encode");

        let back: HostMessage = crate::frame::decode(&buf).expect("decode");
        assert_eq!(
            back,
            HostMessage::FileContents {
                path: "/tmp/empty".into(),
                data: Vec::new(),
                truncated: false,
                binary: false,
                hash: String::new(),
                size: 0,
                readonly: false,
                error: String::new(),
            },
            "an empty file's reply is mostly absence, and absence has to decode"
        );
    }

    #[test]
    fn an_unknown_field_does_not_break_an_older_peer() {
        // A fleet is never upgraded atomically: the phone updates through an app
        // store and the Mac daemon whenever someone remembers. A newer host
        // adding a field must not take an older client down.
        let json = r#"{"t":"list_sessions","something_new":42}"#;
        let parsed: ClientMessage = serde_json::from_str(json).expect("tolerated");
        assert_eq!(parsed, ClientMessage::ListSessions);
    }
}
