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

pub mod auth;
pub mod decode;
pub mod delta;
pub mod encode;
pub mod frame;
pub(crate) mod hex;
pub mod ids;

pub use auth::{AuthFailure, Nonce32, Sig64};
pub use delta::{AttrDef, AttrId, CursorState, Delta, DeltaOp, Run, RowPayload};
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
pub const PROTOCOL_VERSION: u16 = 2;

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
    Challenge { version: u16, host: HostId, label: String, nonce: Nonce32, signature: Sig64 },
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
    /// Pushed to loopback connections only — the desktop app is a client of its
    /// own daemon, so the approval modal is a front end over this rather than a
    /// second mechanism. `remote` is for the prompt: "from 192.168.1.42".
    PairingRequested { client: ClientId, label: String, code: String, remote: String },
    Sessions { sessions: Vec<SessionInfo> },
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
    },
    /// A change from `base` to `seq`.
    ///
    /// A client whose ack is not `base` must discard this and wait for a
    /// keyframe rather than applying it out of order.
    Update { session: SessionAddr, base: Seq, seq: Seq, delta: Delta },
    /// Requested history.
    Scrollback { session: SessionAddr, from_line: i64, rows_data: Vec<RowPayload> },
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
    fn an_absent_nonce_is_not_a_valid_one() {
        // The other half: having decoded, the all-zero default must not be
        // mistaken for freshness a client actually supplied.
        let msg = ClientMessage::Hello {
            version: PROTOCOL_VERSION,
            client: ClientId::from_bytes([2; 32]),
            label: "no-nonce".into(),
            nonce: Nonce32::default(),
        };
        let ClientMessage::Hello { nonce, .. } = msg else { panic!("expected Hello") };
        assert!(nonce.is_absent());
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
