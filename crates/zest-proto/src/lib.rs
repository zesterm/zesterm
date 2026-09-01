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
pub mod predict;

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
pub use predict::{Key, Policy, Prediction, Predictor};

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
        /// This connection is a program acting for a model, not a person.
        ///
        /// Set once at startup, before the client has read a byte of terminal
        /// text, and it only ever *removes* authority: a connection that says
        /// this is refused [`Self::PairingDecision`] and [`Self::Enroll`] even
        /// on loopback, and is never subscribed to the approval queue, so the
        /// six-digit matching code never enters a model's context.
        ///
        /// `#[serde(default)]` for `watch_sessions`' reason, with one
        /// difference worth stating plainly: absent means `false`, and `false`
        /// is the *permissive* answer. It has to be — every already-shipped
        /// client omits the field, and a default that revoked their authority
        /// would break the desktop approval modal against a new daemon. So it
        /// binds a **cooperating** client that declares itself, and is not a
        /// gate against one that stays quiet; `zest_daemon::auth::Auth` carries
        /// the rest of that argument.
        #[serde(default)]
        agent: bool,
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
    /// What directories does `path` hold, on this host's filesystem?
    ///
    /// The cwd chip's browser (#439): a picker that only worked for local
    /// sessions would silently no-op on remote tabs, so the listing is a
    /// question the daemon answers about *its* disk — the connection is
    /// per-host, which is the routing. Additive on the `Enroll` precedent:
    /// only a person opening the browser sends it, an old daemon answers
    /// with its generic could-not-understand `Error` and keeps serving, and
    /// the picker reads that as "daemon too old". No more power than the
    /// client already has — anything attached can type `ls` into a shell —
    /// and it changes nothing: a listing is a question.
    ListDir { path: String },
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
        /// Extra environment for the child, layered over the host's own.
        ///
        /// Ordered and last-wins, and **an empty value unsets** — the same
        /// convention `CommandSpec.env` and the `shell.env` setting already
        /// carry, kept identical here so a launch can be handed straight to
        /// the pty rather than translated on the way.
        ///
        /// No new privilege: `command` is already arbitrary execution on the
        /// host, which is the argument `ReadFile` below makes about itself.
        /// What this *is* is the seam a per-profile environment needs — without
        /// it `shell.env` is a setting that does nothing, because the only
        /// path that applied it was the in-process `--no-daemon` fallback
        /// (#488).
        ///
        /// Skipped when empty so an ordinary launch is byte-identical to what
        /// a daemon predating the field sent, and the conformance fixtures do
        /// not move.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        env: Vec<(String, String)>,
        /// The profile this launch came from, for resolving `env`'s
        /// placeholders. Empty when no profile is behind it.
        ///
        /// A *name*, not the resolved values, because `${profile_dir}` has to
        /// name a directory on the machine that runs the shell. Expanded
        /// client-side, a profile launched on another host would carry this
        /// machine's config path — a Mac handing a Linux box `/Users/...` and
        /// calling it configuration. Same rule ADR-014 already applies to
        /// `starting_directory`.
        ///
        /// It resolves placeholders and nothing else: the host does **not**
        /// look this name up in its own config. A launch says what environment
        /// it wants; only where `${profile_dir}` lands is the host's to decide.
        /// (A published profile the host owns is #487's phase 3, and a
        /// different question.)
        #[serde(default, skip_serializing_if = "String::is_empty")]
        profile: String,
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
    /// What is uncommitted in the repository containing `cwd` (#453).
    ///
    /// Host-scoped like [`Self::ReadFile`], and for the same reason: the
    /// checkout is on the session's machine, so the question is answered by
    /// the daemon that can see it. Answered by
    /// [`HostMessage::GitDiffResult`], and degrading on an old daemon through
    /// the same could-not-understand `Error`.
    GitDiff { cwd: String },
    /// What is this machine configured to do, and where did each value come
    /// from? (#498)
    ///
    /// Host-scoped like [`Self::ReadFile`]: the config is on the machine that
    /// answers, and the connection is the routing. The `Enroll` bargain
    /// applies — an old daemon cannot decode the variant, answers its generic
    /// could-not-understand `Error` and keeps serving.
    ///
    /// Answered by [`HostMessage::ConfigState`].
    ///
    /// **No more authority than the client already holds.** `ReadFile` reads
    /// this exact file today, with no path restriction of any kind, and
    /// pairing grants `CreateSession { command }`. What this adds over
    /// scraping the file is not access, it is the cascade: an effective value
    /// with the layer that wrote it, which a reader of the raw TOML cannot
    /// reconstruct.
    GetConfig {
        /// Restrict the answer to these dotted keys, or to prefixes of them —
        /// `window` means every `window.*`. Empty means every key.
        ///
        /// A filter on the *reply's size*, never on permission. The whole
        /// value set is a kilobyte or two and is meant to be asked for
        /// unfiltered; the field metadata is twenty times that, which is what
        /// this exists for.
        #[serde(default)]
        keys: Vec<String>,
        /// Also describe this profile: what it overrides, its launch
        /// metadata, and where each value came from. Empty means no profile
        /// detail — the *names* come back either way.
        #[serde(default)]
        profile: String,
        /// Also send the editable-field metadata — widget, range, variants,
        /// default, description. Large, so it is opt-in, and `keys` filters
        /// it too.
        #[serde(default)]
        want_fields: bool,
        /// Also send this machine's theme roster: its built-ins and whatever
        /// it found in its own themes directory.
        #[serde(default)]
        want_themes: bool,
    },
    /// Change one thing about this machine's configuration (#498).
    ///
    /// Answered by [`HostMessage::ConfigWritten`], and refused there rather
    /// than obeyed when the key is unknown, the value is illegal, or the edit
    /// would leave a file that no longer parses as settings.
    ///
    /// **There is no `base_hash`, unlike [`Self::WriteFile`], and that is a
    /// decision rather than an omission.** A per-key edit goes through
    /// `toml_edit` into whatever is on disk at the moment of the write, so two
    /// writers touching different keys both survive — which is the common
    /// case, a person in the settings tab while an agent sets something else.
    /// A whole-file precondition would refuse edits that do not conflict, and
    /// the failure it would prevent — last-writer-wins on *one* key — costs a
    /// keystroke rather than a file.
    SetConfig {
        op: ConfigOp,
        /// The dotted settings key, for `set` and `reset`. Ignored otherwise.
        key: String,
        /// The profile the edit is scoped to; empty is the root settings. For
        /// the profile operations, the profile being acted on.
        #[serde(default)]
        profile: String,
        /// The new value in its TOML spelling, for `set`. Ignored otherwise —
        /// `op` is the discriminator, so an empty string stays a legal value
        /// rather than a second way to spell "reset".
        #[serde(default)]
        value: String,
        /// The destination name, for `copy-profile` and `rename-profile`.
        #[serde(default)]
        to: String,
    },
}

/// What a [`ClientMessage::SetConfig`] does.
///
/// A typed enum rather than a string because these are the operations that
/// exist and a client naming one that does not is a bug in the client, not a
/// value to display. Contrast [`HostMessage::ConfigWritten::invalidation`],
/// which is a string precisely because a new class must not be a coordinated
/// wire change.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
pub enum ConfigOp {
    /// Write `key` = `value`.
    Set,
    /// Remove `key`, so it falls back to whatever the weaker layers say.
    /// Idempotent: a key that was never set is a success, not an error.
    Reset,
    /// Create an empty profile. Idempotent, like the config crate's own.
    CreateProfile,
    /// Duplicate `profile` as `to`.
    CopyProfile,
    /// Rename `profile` to `to`.
    RenameProfile,
    /// Delete `profile`.
    RemoveProfile,
}

/// One settings value, as this machine resolves it.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
pub struct ConfigValue {
    /// Dotted, e.g. `typography.size_pt`.
    pub key: String,
    /// The value in its **TOML spelling** — `20.0`, `true`, `"nord"`,
    /// `["Berkeley Mono", "monospace"]`.
    ///
    /// Not JSON, and not a typed sum, for three reasons that all point the
    /// same way. The file is TOML, so an agent reading `size_pt = 20.0` in
    /// `config.toml` and reading this see the same text, and the two pictures
    /// compose. The write direction takes the same spelling straight to
    /// `toml_edit`, so `14` stays `14` rather than becoming `14.0` in
    /// somebody's hand-written file — the exact problem `UiField.integer`
    /// exists for. And it costs `zest-proto` no dependency, in a crate whose
    /// manifest says it is "data and the rules for encoding it, nothing else".
    ///
    /// The same trade [`HostMessage::GitDiffResult::diff`] makes: parsing is a
    /// small pure function in each client, where a wire-level value vocabulary
    /// would be a large surface frozen on the day it ships.
    pub value: String,
    /// Which layer wrote it: `default`, `user`, `profile:<name>`,
    /// `workspace`, `command-line`.
    ///
    /// The machine-readable spelling, deliberately **not** `Source`'s
    /// `Display` — that one is prose ("set by profile `k8s-prod`"), and a
    /// client parsing prose is the thing this field exists to avoid.
    pub source: String,
}

/// What one settings key accepts — the wire half of `zest_config::ui::UiField`.
///
/// Restated here rather than imported, exactly as [`HostProfile`] restates
/// `ProfileMeta` and for the same reason (ADR-014): `zest-proto` importing
/// `zest-config` would put a settings cascade inside the frozen wire crate.
/// The projection lives in the daemon.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
pub struct ConfigField {
    pub key: String,
    /// The display group the settings UIs put it in.
    pub group: String,
    /// `toggle`, `number`, `slider`, `select`, `theme-picker`, … — the
    /// kebab-case spelling of `zest_config::ui::Widget`.
    pub widget: String,
    /// The field's doc comment, which is what a person or a model reads to
    /// decide what to set it to.
    pub description: String,
    /// Schema minimum and maximum, when the schema gives both.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts", ts(optional))]
    pub range: Option<(f64, f64)>,
    /// The schema types this as an integer, so a write must spell it `14`
    /// rather than `14.0`.
    #[serde(default)]
    pub integer: bool,
    /// The legal values, when there is a closed set of them. Empty for a
    /// picker whose roster is live state rather than schema — a theme list is
    /// [`ConfigState::themes`], not this.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub variants: Vec<ConfigVariant>,
    /// The schema default, in the same TOML spelling as
    /// [`ConfigValue::value`], so "is this still the default" is a string
    /// comparison rather than a second encoding to agree about.
    #[serde(default)]
    pub default: String,
    /// The schema's advisory restart flag. `invalidate::class_of` is the
    /// authoritative answer and is what [`HostMessage::ConfigWritten`]
    /// reports; this covers fewer keys and exists for a client holding only
    /// the schema.
    #[serde(default)]
    pub restart_hint: bool,
}

/// One option of a [`ConfigField`] with a closed set of values.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
pub struct ConfigVariant {
    /// The wire value, exactly as it must be spelled in the file.
    pub value: String,
    /// The variant's doc comment; empty when it has none.
    #[serde(default)]
    pub description: String,
}

/// One profile as its file describes it.
///
/// **Carries `host` and `ask_host`, which [`HostProfile`] deliberately does
/// not**, and the difference is the whole reason these are two types rather
/// than one. `HostProfile` is a *launch target*: a client resolves it and then
/// dials, so a `host` key there invites resolution against the **viewer's**
/// fleet, which is the one way that feature could start a command on the wrong
/// computer (ADR-014). This is a *view of a file*, for a client about to edit
/// that file, and nothing launches from it.
///
/// Nothing structural enforces that. A launcher wired to this type later would
/// reintroduce exactly the bug ADR-014 removed, so the rule lives here, in the
/// tool descriptions that expose it, and nowhere else it can be missed.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
pub struct ConfigProfile {
    pub name: String,
    #[serde(default)]
    pub command: String,
    /// The machine this profile is pinned to, as the *file* spells it. Empty
    /// means the machine that answered.
    #[serde(default)]
    pub host: String,
    #[serde(default)]
    pub ask_host: bool,
    #[serde(default)]
    pub starting_directory: String,
    /// `from-shell`, `profile-name`, or the literal custom title.
    #[serde(default)]
    pub tab_title: String,
    #[serde(default)]
    pub color_scheme: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts", ts(optional))]
    pub tab_color: Option<u8>,
    #[serde(default)]
    pub icon: String,
    #[serde(default)]
    pub color_from: String,
    /// The settings keys this profile overrides, each with `source` reading
    /// `profile:<name>` or `profile:defaults` — so "this profile sets it" and
    /// "it fell through to Defaults" are distinguishable, which is the
    /// question anyone editing a profile is actually asking.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub overrides: Vec<ConfigValue>,
    /// Keys in the profile's table that are neither profile-only nor schema
    /// keys — the typo surface.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub unknown_keys: Vec<String>,
}

/// One theme a machine has.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
pub struct ConfigTheme {
    pub id: String,
    pub name: String,
    /// `dark` or `light`.
    #[serde(default)]
    pub mode: String,
    /// Shipped with zesterm rather than imported by this machine's user. What
    /// separates "everyone has this" from "this one is on that laptop".
    #[serde(default)]
    pub builtin: bool,
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
    /// The answer to [`ClientMessage::ListDir`] (#439).
    ///
    /// A plain reply, not a push: only the asker receives it, so — unlike
    /// `Attention` and `Progress` — no `Hello` flag guards it; a peer that
    /// never asks is never handed a tag it cannot decode. `path` echoes the
    /// question, which is the correlation: a picker that navigated on while
    /// a slow answer was in flight drops the stale one by path.
    DirListing {
        /// The listed directory, as asked.
        path: String,
        /// Its parent, for the `..` row. `None` at a filesystem root.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        #[cfg_attr(feature = "ts", ts(optional))]
        parent: Option<String>,
        /// Child directory *names* (not paths), hidden ones skipped, sorted
        /// case-insensitively.
        dirs: Vec<String>,
        /// The cap bit: more existed than `dirs` carries. Said rather than
        /// silently cut, because a truncated listing that looks complete
        /// reads as "covered everything" when it didn't.
        #[serde(default)]
        truncated: bool,
        /// Why the listing is empty, when it is empty for a *reason* —
        /// permission denied, not a directory, gone. Empty string when the
        /// listing simply is what it is; an empty directory and a refused
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
    /// The answer to [`ClientMessage::GitDiff`] (#453).
    ///
    /// A reply, never a push, so no `Hello` flag — see
    /// [`Self::FileContents`]. `cwd` echoes the question, which is the
    /// correlation: a panel that has since been pointed at another session
    /// drops a stale answer by comparing it.
    GitDiffResult {
        /// The directory that was asked about, echoed.
        cwd: String,
        /// The repository root the diff describes, host-absolute — the panel's
        /// title, and what the repo-relative paths inside `diff` are relative
        /// to.
        ///
        /// **Spelled as git spells it, which on Windows is not how
        /// [`Self::FileContents::path`] is spelled**: `rev-parse` answers
        /// `C:/Users/…` where a canonicalized path is `\\?\C:\Users\…`. Same
        /// directory, different dialect — so a client joining this with a
        /// diff's path and handing the result to `ReadFile` is fine (the host
        /// resolves it), but one *comparing* the two strings is not.
        #[serde(default)]
        repo_root: String,
        /// Raw unified diff: staged *and* unstaged against HEAD, so the panel
        /// has one truth rather than two lists a person has to add up.
        ///
        /// **Raw text rather than a parsed structure** on purpose. Splitting
        /// on `diff --git ` is a small pure function in each client, while a
        /// wire-level hunk/rename/mode vocabulary would freeze a large surface
        /// on the day it shipped — and every client renders it differently
        /// anyway.
        #[serde(default)]
        diff: String,
        /// More files changed than `diff` carries. Whole files are dropped,
        /// never half of one: a header promising six lines followed by two is
        /// something a parser is entitled to call corrupt.
        #[serde(default)]
        truncated: bool,
        /// Untracked files, repo-relative, by **name only**.
        ///
        /// Their content is absent because `git diff` structurally cannot show
        /// it — an untracked file has no index entry to diff against — and a
        /// client that wants it already has [`ClientMessage::ReadFile`].
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        untracked: Vec<String>,
        #[serde(default)]
        untracked_truncated: bool,
        /// Why there is no diff, when there is none for a reason — not a
        /// repository, or git did not answer. Empty when the tree is simply
        /// clean, which must not render as a failure.
        #[serde(default, skip_serializing_if = "String::is_empty")]
        error: String,
    },
    /// The answer to [`ClientMessage::GetConfig`] (#498).
    ///
    /// A plain reply, not a push, so — like `DirListing`, `FileContents` and
    /// `GitDiffResult` — **no `Hello` flag guards it**: a peer that cannot
    /// decode this tag structurally never asks for it. `keys` and `profile`
    /// echo the question, which is the correlation; a client that moved on
    /// while a slow answer was in flight drops the stale one by comparing them.
    ///
    /// A refusal is *this* message with `error` set, never
    /// [`Self::Error`] — a sessionless `Error` is what an **old** daemon
    /// sends, and a client reads that as "this daemon is too old". The two
    /// must not be confusable.
    ConfigState {
        /// The request's `keys`, echoed.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        keys: Vec<String>,
        /// The request's `profile`, echoed.
        #[serde(default, skip_serializing_if = "String::is_empty")]
        profile: String,
        /// The file this machine reads and writes, host-absolute. Empty when
        /// the machine has no config directory at all.
        #[serde(default)]
        path: String,
        /// Whether that file exists yet.
        ///
        /// A machine running on pure defaults is not a broken one, and a
        /// client about to write needs to know which it is looking at — an
        /// empty `values` list would otherwise read as a failed read.
        #[serde(default)]
        exists: bool,
        /// One row per settings key in scope, effective after the cascade.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        values: Vec<ConfigValue>,
        /// Every profile's name.
        ///
        /// Always sent, unfiltered by `keys`: it is a handful of short strings
        /// and it is the answer to "what is there to edit", which a client
        /// otherwise has to ask a second time to find out.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        profiles: Vec<String>,
        /// The named profile in full, when one was asked for.
        ///
        /// Boxed, and only for the enum's shape: `ConfigProfile` is a dozen
        /// `String`s, which made `ConfigState` more than twice the size of
        /// every other `HostMessage` variant — and every variant pays that,
        /// including the `Update` sent thousands of times a second. `Box` is
        /// transparent to serde and to `ts-rs`, so the wire and the binding
        /// are byte-for-byte what they were.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        #[cfg_attr(feature = "ts", ts(optional))]
        profile_detail: Option<Box<ConfigProfile>>,
        /// What each key accepts, when `want_fields` asked. Filtered by `keys`
        /// like `values`, because this is the large half of the reply.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        fields: Vec<ConfigField>,
        /// This machine's theme roster, when `want_themes` asked.
        ///
        /// There is deliberately **no `active_theme` field**: the active theme
        /// is `appearance.theme` in `values`, beside `appearance.light_theme`
        /// and `appearance.follow_system_theme`, which together decide it. Two
        /// fields that must agree are two fields that can disagree.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        themes: Vec<ConfigTheme>,
        /// Keys in the file the schema does not know — a typo, or a config
        /// written for a newer zesterm. Reported rather than dropped, because
        /// those two look identical and only the person can tell them apart.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        unknown_keys: Vec<String>,
        /// Layers that could not be read, phrased for a person.
        ///
        /// Not a refusal: the values above are still the best this machine
        /// could resolve, which is exactly what it is running on.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        problems: Vec<String>,
        /// Why there is nothing, when there is nothing for a reason. Empty
        /// when the read simply succeeded — a machine on pure defaults and a
        /// refused read must not render the same.
        #[serde(default, skip_serializing_if = "String::is_empty")]
        error: String,
    },
    /// The answer to [`ClientMessage::SetConfig`] (#498).
    ///
    /// Reply-only, like [`Self::ConfigState`], and refused the same way: this
    /// message with `error` set, never [`Self::Error`].
    ConfigWritten {
        /// The request's `op`, echoed — with `key`, `profile` and `to`, the
        /// whole of the correlation this pair has.
        op: ConfigOp,
        #[serde(default, skip_serializing_if = "String::is_empty")]
        key: String,
        #[serde(default, skip_serializing_if = "String::is_empty")]
        profile: String,
        #[serde(default, skip_serializing_if = "String::is_empty")]
        to: String,
        /// The file that changed, host-absolute. Empty when nothing was
        /// written, which is what separates a refusal from a no-op.
        #[serde(default)]
        path: String,
        /// What the change costs, as this machine computed it: `none`,
        /// `free`, `atlas-bump`, `geometry`, `surface-rebuild`, `restart`.
        ///
        /// A **string**, where [`ConfigOp`] is a typed enum, and the asymmetry
        /// is deliberate. An op a client names is either one that exists or a
        /// bug in the client; an invalidation class is something the *host*
        /// computed and the client displays. Typing it here would make a
        /// seventh class a coordinated wire change across every consumer, to
        /// buy a match arm nobody branches on.
        #[serde(default)]
        invalidation: String,
        /// The running app will not pick this up on its own.
        ///
        /// The one thing a client must *branch* on rather than show, which is
        /// the bar `FileWritten.conflict` sets for a bool on this wire.
        #[serde(default)]
        needs_restart: bool,
        /// What the key now *resolves* to, when the op had a key.
        ///
        /// "I wrote it" and "it is in force" are different facts: a profile
        /// layer or a command-line flag can shadow a write, and a client that
        /// reported success while the value did not move would be telling the
        /// truth about the wrong thing.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        #[cfg_attr(feature = "ts", ts(optional))]
        effective: Option<ConfigValue>,
        /// The edit was refused because a name is taken or a source is gone —
        /// a rename onto a live profile, a copy from one that is not there.
        ///
        /// A bool rather than one of several error strings for
        /// `FileWritten.conflict`'s reason: the client's next act is to pick
        /// another name, which is a branch and not a message.
        #[serde(default)]
        conflict: bool,
        /// Why nothing was written, phrased for a person. Empty on success —
        /// an idempotent reset and a refused one must not read the same.
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
            agent: false,
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
    fn config_messages_round_trip() {
        // Every TOML shape the settings schema actually holds, because
        // `ConfigValue.value` is a string carrying a *spelling* — a bool, an
        // integer, a float, a string and an array all have to survive as the
        // text a client would put in the file.
        for value in ["true", "14", "20.0", "\"nord\"", "[\"Berkeley Mono\", \"monospace\"]"] {
            let msg = HostMessage::ConfigState {
                keys: vec!["typography".into()],
                profile: String::new(),
                path: "/home/a/.config/zesterm/config.toml".into(),
                exists: true,
                values: vec![ConfigValue {
                    key: "typography.size_pt".into(),
                    value: value.into(),
                    source: "user".into(),
                }],
                profiles: vec!["work".into()],
                profile_detail: None,
                fields: Vec::new(),
                themes: Vec::new(),
                unknown_keys: Vec::new(),
                problems: Vec::new(),
                error: String::new(),
            };
            let bytes = rmp_serde::to_vec_named(&msg).expect("encode");
            let back: HostMessage = rmp_serde::from_slice(&bytes).expect("decode");
            assert_eq!(msg, back, "a `{value}` did not survive the wire");
        }

        for op in [
            ConfigOp::Set,
            ConfigOp::Reset,
            ConfigOp::CreateProfile,
            ConfigOp::CopyProfile,
            ConfigOp::RenameProfile,
            ConfigOp::RemoveProfile,
        ] {
            let msg = ClientMessage::SetConfig {
                op,
                key: "window.opacity".into(),
                profile: "work".into(),
                value: "0.95".into(),
                to: "work-2".into(),
            };
            let bytes = rmp_serde::to_vec_named(&msg).expect("encode");
            let back: ClientMessage = rmp_serde::from_slice(&bytes).expect("decode");
            assert_eq!(msg, back, "{op:?} did not survive the wire");
        }
    }

    #[test]
    fn a_config_reply_with_no_optional_slices_still_decodes() {
        // Every field but `op` is `#[serde(default)]`, so a daemon that omits
        // the lot must still produce something a client can read — the shape
        // an older host, or a refusal, actually sends.
        let json = r#"{"t":"config_written","op":"reset"}"#;
        let back: HostMessage = serde_json::from_str(json).expect("decode");
        match back {
            HostMessage::ConfigWritten { op, key, needs_restart, effective, conflict, .. } => {
                assert_eq!(op, ConfigOp::Reset);
                assert!(key.is_empty() && !needs_restart && !conflict);
                assert!(effective.is_none());
            }
            other => panic!("decoded as the wrong variant: {other:?}"),
        }
    }

    #[test]
    fn the_expensive_half_of_a_config_reply_costs_nothing_when_unasked() {
        // The whole justification for `want_fields`/`want_themes` being opt-in
        // rather than always sent: the common read is values only, and the
        // field metadata is roughly twenty times their size. Without
        // `skip_serializing_if` a client pays for two empty lists on every
        // read, which is the shape `an_absent_offer_is_absent_on_the_wire`
        // already argues about for `Sessions`.
        let msg = HostMessage::ConfigState {
            keys: Vec::new(),
            profile: String::new(),
            path: "/c.toml".into(),
            exists: true,
            values: vec![ConfigValue {
                key: "appearance.theme".into(),
                value: "\"nord\"".into(),
                source: "user".into(),
            }],
            profiles: Vec::new(),
            profile_detail: None,
            fields: Vec::new(),
            themes: Vec::new(),
            unknown_keys: Vec::new(),
            problems: Vec::new(),
            error: String::new(),
        };
        let json = serde_json::to_string(&msg).expect("serialize");
        for absent in ["fields", "themes", "profile_detail", "unknown_keys", "problems", "error"] {
            assert!(!json.contains(absent), "an unasked `{absent}` costs bytes: {json}");
        }
    }

    #[test]
    fn a_config_refusal_is_a_config_reply_and_never_a_sessionless_error() {
        // The two must not be confusable, and the reason is directional: a
        // sessionless `Error` is what an *old* daemon sends when it cannot
        // decode `GetConfig` at all, and a client reads that as "this host is
        // too old for the config surface". A daemon that answered a bad key
        // with `Error` would make a fixable mistake look like an unfixable
        // one, and the client would stop asking.
        let refusal = HostMessage::ConfigWritten {
            op: ConfigOp::Set,
            key: "typography.sizept".into(),
            profile: String::new(),
            to: String::new(),
            path: String::new(),
            invalidation: String::new(),
            needs_restart: false,
            effective: None,
            conflict: false,
            error: "no setting named `typography.sizept`".into(),
        };
        assert!(
            matches!(refusal, HostMessage::ConfigWritten { .. }),
            "a refusal must wear the reply's own shape"
        );
        let HostMessage::ConfigWritten { path, error, .. } = &refusal else { unreachable!() };
        assert!(!error.is_empty(), "a refusal has to say why");
        assert!(path.is_empty(), "an empty path is what says nothing was written");

        let read_refusal = HostMessage::ConfigState {
            keys: Vec::new(),
            profile: String::new(),
            path: String::new(),
            exists: false,
            values: Vec::new(),
            profiles: Vec::new(),
            profile_detail: None,
            fields: Vec::new(),
            themes: Vec::new(),
            unknown_keys: Vec::new(),
            problems: Vec::new(),
            error: "this machine has no config directory".into(),
        };
        assert!(matches!(read_refusal, HostMessage::ConfigState { .. }));
    }

    #[test]
    fn a_config_profile_keeps_the_host_key_that_a_published_one_drops() {
        // The pair `a_published_profile_names_no_host_of_its_own` guards from
        // the other side. `HostProfile` is a launch target and must not carry
        // `host`, because a client resolves it against its *own* fleet and
        // then dials — ADR-014. `ConfigProfile` is a view of a file for a
        // client about to edit that file, so dropping `host` would hide a key
        // the person plainly wrote. Two types, opposite rules, and this test
        // exists so a later "why are these not one struct" tidy-up has to
        // read the reason first.
        let fields = serde_json::to_value(ConfigProfile {
            name: "k8s".into(),
            host: "big-linux".into(),
            ask_host: true,
            ..Default::default()
        })
        .expect("serialize");
        assert!(fields.get("host").is_some(), "a file view must show the host key");
        assert!(fields.get("ask_host").is_some());

        let published = serde_json::to_value(HostProfile::default()).expect("serialize");
        assert!(
            published.get("host").is_none(),
            "a launch target must not carry a host for the viewer to re-resolve"
        );
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
    fn a_hello_without_the_agent_flag_still_decodes_and_keeps_its_authority() {
        // Every client shipped before this flag omits it, so absent has to
        // mean `false` -- and `false` is the *permissive* answer here, unlike
        // the `watch_*` flags above where absent simply means "no pushes".
        // A default that revoked authority would refuse the desktop approval
        // modal the moment it met a newer daemon.
        #[derive(serde::Serialize)]
        struct OldHello<'a> {
            t: &'a str,
            version: u16,
            client: ClientId,
            label: &'a str,
            nonce: Nonce32,
            dh: Pub32,
            watch_sessions: bool,
            watch_pairings: bool,
        }
        let old = rmp_serde::to_vec_named(&OldHello {
            t: "hello",
            version: PROTOCOL_VERSION,
            client: ClientId::from_bytes([9; 32]),
            label: "old-client",
            nonce: Nonce32::from_bytes([1; 32]),
            dh: Pub32::from_bytes([2; 32]),
            watch_sessions: true,
            watch_pairings: true,
        })
        .expect("encode");
        let parsed: ClientMessage = crate::frame::decode(&old).expect("an old Hello must decode");
        let ClientMessage::Hello { watch_pairings, agent, .. } = parsed else {
            panic!("expected Hello");
        };
        assert!(watch_pairings, "the fields the old client did send survive");
        assert!(
            !agent,
            "a client that says nothing is taken to be a person's -- the flag removes \
             authority from clients that ask for that, and binds nobody else"
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
            agent: false,
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
            env: Vec::new(),
            profile: String::new(),
        };
        let body = crate::frame::encode_body(&msg).expect("encode");
        let back: ClientMessage = crate::frame::decode(&body).expect("decode");
        assert_eq!(msg, back, "command, cwd and size all survive the frame");
    }

    #[test]
    fn a_create_sessions_env_crosses_the_wire_and_costs_nothing_when_empty() {
        // Two claims, because the second is what lets this field be added to a
        // frozen contract at all (#488).
        //
        // One: order and the empty-value-unsets convention survive the frame.
        // `Vec<(String, String)>` rather than a map precisely so both do --
        // last-wins needs an order, and a map would silently keep one of two
        // entries naming the same variable.
        let msg = ClientMessage::CreateSession {
            command: String::new(),
            cwd: String::new(),
            cols: 80,
            rows: 24,
            env: vec![
                ("CLAUDE_CONFIG_DIR".into(), "/home/a/.config/zesterm/work".into()),
                ("TERM".into(), "xterm-256color".into()),
                // The unset spelling. A map would round-trip this identically;
                // the assertion that matters is that it is still *here* and
                // still last, because the daemon applies it in order.
                ("WT_SESSION".into(), String::new()),
            ],
            profile: String::new(),
        };
        let body = crate::frame::encode_body(&msg).expect("encode");
        let back: ClientMessage = crate::frame::decode(&body).expect("decode");
        assert_eq!(msg, back, "the launch environment survives the frame in order");

        // Two: an ordinary launch is byte-identical to what a peer predating
        // the field sent. `skip_serializing_if` is doing that work, and
        // without it every `CreateSession` on every machine in the fleet would
        // grow a field, and the conformance fixtures would move for a feature
        // nobody in them uses.
        let bare = ClientMessage::CreateSession {
            command: String::new(),
            cwd: String::new(),
            cols: 80,
            rows: 24,
            env: Vec::new(),
            profile: String::new(),
        };
        // Decoded as a map and checked by *key*, not by searching the bytes
        // for the text "env": a MessagePack body is arbitrary bytes, so a
        // substring search can match a value that merely contains those three
        // characters -- `cwd: "/srv/env"` would have done it -- and would
        // equally miss a key spelled across a boundary it does not expect.
        // `IgnoredAny` reads the keys and discards every value, so this stays
        // a statement about the field set and nothing else.
        let keys = |msg: &ClientMessage| -> std::collections::BTreeSet<String> {
            let body = crate::frame::encode_body(msg).expect("encode");
            let map: std::collections::BTreeMap<String, serde::de::IgnoredAny> =
                rmp_serde::from_slice(&body).expect("a named map, which is what to_vec_named writes");
            map.into_keys().collect()
        };
        assert!(
            !keys(&bare).contains("env"),
            "an empty launch env must not reach the wire at all"
        );
        // The negative above is only worth anything if the positive holds:
        // a `keys` that never reported `env` would pass it for free.
        assert!(
            keys(&msg).contains("env"),
            "a non-empty launch env must reach the wire, or the assertion above proves nothing"
        );
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
    fn a_diff_crosses_the_wire_intact() {
        let ask = ClientMessage::GitDiff { cwd: "/home/a/p".into() };
        let body = crate::frame::encode_body(&ask).expect("encode");
        assert_eq!(ask, crate::frame::decode::<ClientMessage>(&body).expect("decode"));

        let msg = HostMessage::GitDiffResult {
            cwd: "/home/a/p".into(),
            repo_root: "/home/a/p".into(),
            // Newlines and a non-ASCII byte, since the diff is the one field
            // here that carries arbitrary file content.
            diff: "diff --git a/é.txt b/é.txt\n@@ -1 +1 @@\n-a\n+b\n".into(),
            truncated: true,
            // A space in a name is the case `-z` exists for; it has to survive
            // the wire as well as the parse.
            untracked: vec!["two words.txt".into()],
            untracked_truncated: true,
            error: String::new(),
        };
        let body = crate::frame::encode_body(&msg).expect("encode");
        assert_eq!(msg, crate::frame::decode::<HostMessage>(&body).expect("decode"));
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
