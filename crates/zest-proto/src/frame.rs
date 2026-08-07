//! Putting messages on a byte stream.
//!
//! Length-prefixed MessagePack. A `u32` length, then that many bytes of
//! `rmp-serde`.
//!
//! # Why length-prefixed at all
//!
//! WebSocket frames their own messages, so on that transport this is redundant.
//! The loopback socket the desktop app uses does not, and neither does a raw TCP
//! LAN connection — and a self-delimiting format on a stream means a malformed
//! message desynchronizes everything after it, with no way back short of
//! dropping the connection. The length prefix makes a bad message one bad
//! message.
//!
//! # Why MessagePack and not JSON
//!
//! Grid deltas are mostly small integers and short strings, which MessagePack
//! encodes in roughly a third of the bytes. The types carry `serde` derives, so
//! JSON stays available for a debug dump — and the conformance corpus uses
//! exactly that, because a diff of two JSON payloads is readable and a diff of
//! two MessagePack blobs is not.

use serde::{de::DeserializeOwned, Serialize};

/// Largest message accepted.
///
/// A keyframe of a very large grid is the biggest legitimate message, and at
/// 500×200 with every cell styled that is comfortably under a megabyte. The
/// bound exists because the length prefix arrives from the network: without it,
/// a corrupt or hostile four bytes asks for a 4GB allocation, and the process
/// dies before anything has a chance to reject the message.
pub const MAX_FRAME: usize = 8 * 1024 * 1024;

#[derive(Debug, thiserror::Error)]
pub enum FrameError {
    #[error("frame of {0} bytes exceeds the {MAX_FRAME} byte limit")]
    TooLarge(usize),
    #[error("encoding failed: {0}")]
    Encode(String),
    #[error("decoding failed: {0}")]
    Decode(String),
    #[error(transparent)]
    Io(#[from] std::io::Error),
}

/// Encode one message, length prefix included.
pub fn encode<T: Serialize>(msg: &T) -> Result<Vec<u8>, FrameError> {
    // `named` so field names travel with the data. The unnamed form is smaller
    // and turns every field addition into a breaking change, which a fleet whose
    // parts upgrade independently cannot afford.
    let body = rmp_serde::to_vec_named(msg).map_err(|e| FrameError::Encode(e.to_string()))?;
    if body.len() > MAX_FRAME {
        return Err(FrameError::TooLarge(body.len()));
    }

    let mut out = Vec::with_capacity(body.len() + 4);
    out.extend_from_slice(&u32::try_from(body.len()).unwrap_or(u32::MAX).to_le_bytes());
    out.extend_from_slice(&body);
    Ok(out)
}

/// Decode one message from a complete frame body (no length prefix).
pub fn decode<T: DeserializeOwned>(body: &[u8]) -> Result<T, FrameError> {
    rmp_serde::from_slice(body).map_err(|e| FrameError::Decode(e.to_string()))
}

/// Pulls whole messages out of a byte stream.
///
/// Feeding bytes and taking messages are separate steps because a socket read
/// has no relationship to a message boundary: one read can carry three messages
/// and half of a fourth, and the half has to wait without blocking the three.
#[derive(Debug, Default)]
pub struct FrameReader {
    buf: Vec<u8>,
}

impl FrameReader {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Add bytes as they arrive.
    pub fn feed(&mut self, bytes: &[u8]) {
        self.buf.extend_from_slice(bytes);
    }

    /// Take the next complete frame body, if one has arrived.
    ///
    /// `Ok(None)` means *not yet*, which is the common case and not an error.
    pub fn next_frame(&mut self) -> Result<Option<Vec<u8>>, FrameError> {
        if self.buf.len() < 4 {
            return Ok(None);
        }
        let len = u32::from_le_bytes([self.buf[0], self.buf[1], self.buf[2], self.buf[3]]) as usize;

        // Checked before reserving anything. A length is the first thing an
        // attacker controls and the last thing to trust.
        if len > MAX_FRAME {
            return Err(FrameError::TooLarge(len));
        }
        if self.buf.len() < 4 + len {
            return Ok(None);
        }

        let body = self.buf[4..4 + len].to_vec();
        self.buf.drain(..4 + len);
        Ok(Some(body))
    }

    /// Bytes held awaiting a complete frame.
    #[must_use]
    pub fn pending(&self) -> usize {
        self.buf.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{ClientId, ClientMessage, HostId, SessionAddr, SessionId, PROTOCOL_VERSION};

    fn hello() -> ClientMessage {
        ClientMessage::Hello {
            version: PROTOCOL_VERSION,
            client: ClientId::from_bytes([3; 32]),
            label: "phone".into(),
            nonce: crate::Nonce32::from_bytes([9; 32]),
        }
    }

    #[test]
    fn a_message_round_trips() {
        let bytes = encode(&hello()).expect("encode");
        let mut r = FrameReader::new();
        r.feed(&bytes);
        let body = r.next_frame().expect("no error").expect("a whole frame");
        let back: ClientMessage = decode(&body).expect("decode");
        assert_eq!(back, hello());
    }

    #[test]
    fn a_read_split_mid_message_waits_rather_than_failing() {
        // The normal case on a socket. A reader that treated a partial message
        // as an error would drop the connection on ordinary TCP segmentation.
        let bytes = encode(&hello()).expect("encode");
        let mut r = FrameReader::new();

        for chunk in bytes.chunks(3) {
            r.feed(chunk);
        }
        assert!(r.next_frame().expect("no error").is_some());
    }

    #[test]
    fn a_partial_frame_yields_nothing_and_keeps_its_bytes() {
        let bytes = encode(&hello()).expect("encode");
        let mut r = FrameReader::new();
        r.feed(&bytes[..bytes.len() - 1]);

        assert!(r.next_frame().expect("no error").is_none(), "returned an incomplete message");
        assert!(r.pending() > 0, "the partial frame was discarded");

        r.feed(&bytes[bytes.len() - 1..]);
        assert!(r.next_frame().expect("no error").is_some());
    }

    #[test]
    fn several_messages_in_one_read_all_come_out() {
        // One socket read routinely carries several messages, and a reader that
        // handled only the first would stall until the next arrived.
        let mut stream = Vec::new();
        for _ in 0..3 {
            stream.extend_from_slice(&encode(&hello()).expect("encode"));
        }
        let mut r = FrameReader::new();
        r.feed(&stream);

        let mut count = 0;
        while r.next_frame().expect("no error").is_some() {
            count += 1;
        }
        assert_eq!(count, 3);
        assert_eq!(r.pending(), 0, "bytes left over after the last message");
    }

    #[test]
    fn an_absurd_length_is_rejected_before_anything_is_allocated() {
        // The length prefix is the first thing an attacker controls. Without
        // this check a hostile four bytes asks for a 4GB allocation and the
        // process dies before it can reject the message.
        let mut r = FrameReader::new();
        r.feed(&u32::MAX.to_le_bytes());
        r.feed(b"nowhere near that much");

        assert!(matches!(r.next_frame(), Err(FrameError::TooLarge(_))));
    }

    #[test]
    fn a_corrupt_body_fails_that_message_and_not_the_stream() {
        // The reason for framing at all: a self-delimiting format would
        // desynchronize everything after a bad message.
        let good = encode(&hello()).expect("encode");
        let mut stream = Vec::new();
        stream.extend_from_slice(&(4u32).to_le_bytes());
        stream.extend_from_slice(b"junk");
        stream.extend_from_slice(&good);

        let mut r = FrameReader::new();
        r.feed(&stream);

        let bad = r.next_frame().expect("framing is intact").expect("a whole frame");
        assert!(decode::<ClientMessage>(&bad).is_err(), "junk decoded as a message");

        let next = r.next_frame().expect("framing is intact").expect("the next frame");
        assert_eq!(decode::<ClientMessage>(&next).expect("decode"), hello());
    }

    #[test]
    fn field_names_travel_so_a_new_field_is_not_a_breaking_change() {
        // A fleet upgrades in pieces -- the phone through an app store, the Mac
        // daemon whenever someone remembers. Positional encoding would make a
        // field addition break every peer that had not updated.
        let msg = ClientMessage::Input {
            session: SessionAddr::new(HostId::from_bytes([1; 32]), SessionId(9)),
            bytes: vec![b'x'],
        };
        let bytes = encode(&msg).expect("encode");
        // Decoded through a self-describing value rather than back into the
        // typed form: round-tripping would pass even with positional encoding,
        // which is exactly the thing under test.
        let value: serde_json::Value =
            rmp_serde::from_slice(&bytes[4..]).expect("msgpack is self-describing");
        assert!(
            value.get("session").is_some(),
            "field names were not encoded, so adding a field would break older peers: {value}"
        );
    }

    #[test]
    fn an_empty_reader_is_not_an_error() {
        let mut r = FrameReader::new();
        assert!(r.next_frame().expect("no error").is_none());
    }
}
