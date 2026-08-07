//! The mesh wire protocol.
//!
//! **This crate is a frozen contract.** `zest-daemon`, the web client and the
//! phone client are all built against these shapes, by different people at
//! different times. Changing one is a coordinated release across three
//! codebases, so changes go through an issue rather than a commit — see
//! `docs/CONTRACTS.md`.
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

pub mod decode;
pub mod delta;
pub mod encode;
pub mod frame;
pub mod ids;

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
pub const PROTOCOL_VERSION: u16 = 1;

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
        label: String,
    },
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
    /// Answer to `Hello`.
    Welcome { version: u16, host: HostId, label: String },
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
        };
        let json = serde_json::to_string(&msg).expect("serialize");
        let back: ClientMessage = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(msg, back);
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
