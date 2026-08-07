//! Who is talking, and about which session.
//!
//! Identities are **public keys, not random numbers**. A `HostId` is the
//! fingerprint of the host's own keypair, so "is this really my Mac" is
//! answerable by asking it to sign a nonce rather than by trusting whatever
//! routed the message. A UUID would be equally unique and prove nothing — and
//! the difference only shows up once there is a relay in the path, by which
//! point every stored id would have to be reissued.

use std::fmt;

use serde::{Deserialize, Serialize};

/// A machine in the mesh. The fingerprint of its long-lived public key.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
pub struct HostId(#[serde(with = "hex32")] pub [u8; 32]);

/// A device that attaches to hosts. Same construction, different role.
///
/// Separate from [`HostId`] on purpose even though the bytes look identical: a
/// machine can be both — the Windows box hosts its own shells *and* attaches to
/// the Mac — and conflating the two roles is how an authorization check ends up
/// asking the wrong question.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
pub struct ClientId(#[serde(with = "hex32")] pub [u8; 32]);

/// A session, unique within one host.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
pub struct SessionId(pub u64);

/// A session anywhere in the mesh.
///
/// The unit every session-scoped message is addressed by. See the crate docs for
/// why the host half is not optional.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
pub struct SessionAddr {
    pub host: HostId,
    pub session: SessionId,
}

impl HostId {
    #[must_use]
    pub const fn from_bytes(bytes: [u8; 32]) -> Self {
        Self(bytes)
    }

    /// Short form for logs and the UI.
    ///
    /// Eight hex characters. Long enough that two machines in one person's fleet
    /// will not collide, short enough to read out loud during pairing — which is
    /// what it is actually for.
    #[must_use]
    pub fn short(&self) -> String {
        hex32::encode(&self.0[..4])
    }
}

impl ClientId {
    #[must_use]
    pub const fn from_bytes(bytes: [u8; 32]) -> Self {
        Self(bytes)
    }

    #[must_use]
    pub fn short(&self) -> String {
        hex32::encode(&self.0[..4])
    }
}

impl SessionAddr {
    #[must_use]
    pub const fn new(host: HostId, session: SessionId) -> Self {
        Self { host, session }
    }
}

impl fmt::Display for SessionAddr {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}:{}", self.host.short(), self.session.0)
    }
}

/// Hex, so an id in a log or a JSON payload can be read and compared by eye.
///
/// A raw `[u8; 32]` serializes as a 32-element array, which is unreadable in a
/// log line and hostile in the TypeScript clients.
mod hex32 {
    use serde::{Deserialize, Deserializer, Serializer};

    pub fn encode(bytes: &[u8]) -> String {
        let mut s = String::with_capacity(bytes.len() * 2);
        for b in bytes {
            s.push(char::from_digit(u32::from(b >> 4), 16).unwrap_or('0'));
            s.push(char::from_digit(u32::from(b & 0x0f), 16).unwrap_or('0'));
        }
        s
    }

    pub fn serialize<S: Serializer>(bytes: &[u8; 32], s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&encode(bytes))
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<[u8; 32], D::Error> {
        use serde::de::Error as _;
        let text = String::deserialize(d)?;
        if text.len() != 64 {
            return Err(D::Error::custom("expected 64 hex characters"));
        }
        let mut out = [0u8; 32];
        let chars: Vec<char> = text.chars().collect();
        for (i, pair) in chars.chunks_exact(2).enumerate() {
            let hi = pair[0].to_digit(16).ok_or_else(|| D::Error::custom("not hex"))?;
            let lo = pair[1].to_digit(16).ok_or_else(|| D::Error::custom("not hex"))?;
            out[i] = ((hi << 4) | lo) as u8;
        }
        Ok(out)
    }
}



#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_serialize_as_readable_hex() {
        let id = HostId::from_bytes([0xab; 32]);
        let json = serde_json::to_string(&id).expect("serialize");
        assert_eq!(json, "\"abababababababababababababababababababababababababababababababab\"");
    }

    #[test]
    fn ids_round_trip() {
        let mut bytes = [0u8; 32];
        for (i, b) in bytes.iter_mut().enumerate() {
            *b = i as u8;
        }
        let id = HostId::from_bytes(bytes);
        let json = serde_json::to_string(&id).expect("serialize");
        let back: HostId = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(id, back);
    }

    #[test]
    fn a_truncated_id_is_rejected_rather_than_padded() {
        // Silently accepting a short id would make two different hosts compare
        // equal, which is a security bug wearing a parsing bug's clothes.
        let r: Result<HostId, _> = serde_json::from_str("\"abcd\"");
        assert!(r.is_err());
    }

    #[test]
    fn the_short_form_is_readable_aloud() {
        // It exists to be spoken during pairing, so length is the whole point.
        let id = HostId::from_bytes([0x1f; 32]);
        assert_eq!(id.short(), "1f1f1f1f");
    }

    #[test]
    fn an_address_prints_as_host_colon_session() {
        let addr = SessionAddr::new(HostId::from_bytes([0x2a; 32]), SessionId(12));
        assert_eq!(addr.to_string(), "2a2a2a2a:12");
    }
}
