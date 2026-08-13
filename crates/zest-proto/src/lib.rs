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
pub(crate) mod hex;
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
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
pub struct Seq(pub u64);

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
    Attach { session: SessionAddr, cols: u16, rows: u16 },
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
    RequestScrollback { session: SessionAddr, from_line: i64, count: u32 },
    /// End the session and its child process.
    CloseSession { session: SessionAddr },
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
    },
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
    },
    /// A change from `base` to `seq`.
    ///
    /// A client whose ack is not `base` must discard this and wait for a
    /// keyframe rather than applying it out of order.
    Update { session: SessionAddr, base: Seq, seq: Seq, delta: Delta },
    /// Requested history.
    Scrollback {
        session: SessionAddr,
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
    /// Something went wrong, phrased for a person.
    Error { session: Option<SessionAddr>, message: String },
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
        };
        let json = serde_json::to_string(&msg).expect("serialize");
        let back: ClientMessage = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(msg, back);
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
    fn an_unknown_field_does_not_break_an_older_peer() {
        // A fleet is never upgraded atomically: the phone updates through an app
        // store and the Mac daemon whenever someone remembers. A newer host
        // adding a field must not take an older client down.
        let json = r#"{"t":"list_sessions","something_new":42}"#;
        let parsed: ClientMessage = serde_json::from_str(json).expect("tolerated");
        assert_eq!(parsed, ClientMessage::ListSessions);
    }
}
