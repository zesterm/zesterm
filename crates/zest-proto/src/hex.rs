//! Fixed-width byte arrays as hex strings.
//!
//! Every fixed-width secret-adjacent value on this wire — ids, nonces,
//! signatures — serializes as hex rather than as a byte array. A raw `[u8; 32]`
//! becomes a 32-element list, which is unreadable in a log line and hostile in
//! the TypeScript clients, and a 64-byte signature is twice as bad.
//!
//! Length is checked on the way in and **never padded**. Silently accepting a
//! short id would make two different hosts compare equal, and silently accepting
//! a short signature would hand `ed25519-dalek` something it must then reject
//! for a reason that reads like a cryptography failure rather than a parsing
//! one.

use serde::{Deserialize, Deserializer, Serializer};

/// Lowercase hex, two characters per byte.
pub fn encode(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push(char::from_digit(u32::from(b >> 4), 16).unwrap_or('0'));
        s.push(char::from_digit(u32::from(b & 0x0f), 16).unwrap_or('0'));
    }
    s
}

pub fn serialize<const N: usize, S: Serializer>(
    bytes: &[u8; N],
    s: S,
) -> Result<S::Ok, S::Error> {
    s.serialize_str(&encode(bytes))
}

pub fn deserialize<'de, const N: usize, D: Deserializer<'de>>(
    d: D,
) -> Result<[u8; N], D::Error> {
    use serde::de::Error as _;
    let text = String::deserialize(d)?;
    if text.len() != N * 2 {
        return Err(D::Error::custom(format!("expected {} hex characters", N * 2)));
    }
    let mut out = [0u8; N];
    let chars: Vec<char> = text.chars().collect();
    for (i, pair) in chars.chunks_exact(2).enumerate() {
        let hi = pair[0].to_digit(16).ok_or_else(|| D::Error::custom("not hex"))?;
        let lo = pair[1].to_digit(16).ok_or_else(|| D::Error::custom("not hex"))?;
        out[i] = ((hi << 4) | lo) as u8;
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use serde::{Deserialize, Serialize};

    #[derive(Debug, PartialEq, Serialize, Deserialize)]
    struct Big(#[serde(with = "super")] [u8; 64]);

    #[derive(Debug, PartialEq, Serialize, Deserialize)]
    struct Small(#[serde(with = "super")] [u8; 32]);

    #[test]
    fn a_sixty_four_byte_value_round_trips() {
        let v = Big([0x5a; 64]);
        let json = serde_json::to_string(&v).expect("serialize");
        assert_eq!(json.len(), 64 * 2 + 2, "hex string plus quotes");
        assert_eq!(serde_json::from_str::<Big>(&json).expect("deserialize"), v);
    }

    #[test]
    fn a_value_of_the_wrong_length_is_rejected_rather_than_padded() {
        // A 32-byte id offered where a 64-byte signature belongs must not
        // become a signature with 32 zero bytes on the end.
        let thirty_two = format!("\"{}\"", "ab".repeat(32));
        assert!(serde_json::from_str::<Big>(&thirty_two).is_err());

        let sixty_four = format!("\"{}\"", "ab".repeat(64));
        assert!(serde_json::from_str::<Small>(&sixty_four).is_err());
    }
}
