//! Handshake values, as bytes on the wire.
//!
//! This module deliberately knows **nothing about cryptography**. It has no
//! opinion on what a signature covers, no verification, and no dependency on
//! `zest-mesh` — the dependency runs the other way, `zest-mesh` → `zest-proto`,
//! and it stays that way so a protocol change and a change to the signing
//! scheme can never be the same commit.
//!
//! What lives here is the shape of the two extra frames a connection exchanges
//! before it is served, and the vocabulary for refusing one.
//!
//! # Why the handshake costs a round trip
//!
//! A nonce carried on `Hello` alone is worthless: the client chooses it, so a
//! recording of one connection replays onto the next. The host has to
//! contribute freshness, which means it has to speak before the client's proof
//! exists:
//!
//! ```text
//! C -> H  Hello     { version, client, label, nonce }
//! H -> C  Challenge { version, host, label, nonce, signature }
//! C -> H  Auth      { signature }
//! H -> C  Welcome | AuthPending | AuthFailed
//! ```
//!
//! The host signs first on purpose. A client dialling an address it learned
//! from an mDNS advertisement must be able to check "the machine that answered
//! is the `HostId` the advertisement claimed" and hang up before revealing
//! anything, and only a host signature over a client-chosen nonce proves that.
//!
//! # What this does not protect, and must not be believed to
//!
//! The signatures authenticate the two endpoints at the moment of connection.
//! They do not establish a key, so **nothing after the handshake is protected**
//! — every byte is plaintext MessagePack, including whatever gets typed at a
//! `sudo` prompt. An attacker able to relay the handshake verbatim ends up
//! holding two connections and can read and rewrite all of it.
//!
//! That makes LAN listening *trusted-network* security: it stops the
//! neighbour's laptop from getting a shell, and it does not stop a hostile
//! network. Since protocol 3 the end-to-end encryption closes that: nothing
//! here had to be undone to get there, exactly as this note predicted — the
//! same keys sign, the same transcript is the handshake hash, and the only
//! addition is a [`Pub32`] in each direction. See `zest_mesh::secure` and
//! ADR-008.

use serde::{Deserialize, Serialize};

/// Fresh random bytes contributed to one handshake.
///
/// `Default` is all zeroes, which exists only so a peer speaking the previous
/// protocol still *decodes* — see [`ClientMessage::Hello`](crate::ClientMessage)
/// — and is rejected explicitly rather than treated as a nonce.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
pub struct Nonce32(
    #[serde(with = "crate::hex")]
    #[cfg_attr(feature = "ts", ts(type = "string"))]
    pub [u8; 32],
);

/// An X25519 public key, ephemeral to one connection.
///
/// Beside `Nonce32` rather than reusing it, though both are 32 bytes: a nonce
/// is freshness and this is key material, and a type that means either is a
/// type a caller can pass to the wrong parameter. The wire carries the bytes;
/// `zest-mesh` decides what they mean — this crate has no crypto dependency
/// and gains none here.
/// `Default` is all zeroes, and exists for exactly one reason: so a peer
/// speaking the previous protocol reaches the *version* check with a decodable
/// message instead of failing to parse. It is never a usable key —
/// `is_absent` recognises it and `zest-mesh` refuses to derive from it.
#[derive(Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
pub struct Pub32(
    #[serde(with = "crate::hex")]
    #[cfg_attr(feature = "ts", ts(type = "string"))]
    pub [u8; 32],
);

impl Pub32 {
    #[must_use]
    pub fn from_bytes(bytes: [u8; 32]) -> Self {
        Self(bytes)
    }

    /// All zero — what `#[serde(default)]` produces for a peer that sent none.
    ///
    /// Same discipline as [`Nonce32::is_absent`], and the same reason: the
    /// default has to be recognisable, because agreeing on a shared secret
    /// with a peer that contributed nothing would look like success on both
    /// sides. The refusal lives in `zest-mesh`, where the key is used.
    #[must_use]
    pub fn is_absent(&self) -> bool {
        self.0 == [0u8; 32]
    }
}

impl std::fmt::Debug for Pub32 {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Short form, for the same reason `Sig64` has one: this appears on
        // every connection and the full 64 characters push the useful fields
        // off the line.
        write!(f, "Pub32({})", crate::hex::encode(&self.0[..4]))
    }
}

/// A detached Ed25519 signature.
///
/// Hand-written `Debug`: a signature is 128 hex characters and appears in
/// tracing output at every connection, where the full value is noise that
/// pushes the useful fields off the line.
#[derive(Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
pub struct Sig64(
    #[serde(with = "crate::hex")]
    #[cfg_attr(feature = "ts", ts(type = "string"))]
    pub [u8; 64],
);

impl Nonce32 {
    #[must_use]
    pub const fn from_bytes(bytes: [u8; 32]) -> Self {
        Self(bytes)
    }

    /// True for the all-zero value, which is never a real nonce.
    #[must_use]
    pub fn is_absent(&self) -> bool {
        self.0 == [0u8; 32]
    }
}

impl Sig64 {
    #[must_use]
    pub const fn from_bytes(bytes: [u8; 64]) -> Self {
        Self(bytes)
    }
}

impl std::fmt::Debug for Sig64 {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Sig64({}..)", crate::hex::encode(&self.0[..4]))
    }
}

/// Why a connection was refused.
///
/// A separate message rather than reusing [`HostMessage::Error`][err], because
/// a client must *branch* on this: wait quietly for someone to approve it, stop
/// retrying forever, or back off. Reusing the human-facing error would mean
/// matching on a message string, which is how a client breaks when someone
/// improves the wording.
///
/// [err]: crate::HostMessage::Error
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
#[non_exhaustive]
pub enum AuthFailure {
    /// The signature did not verify, or no nonce was offered. Not retryable:
    /// the client does not hold the key it claimed.
    Signature,
    /// The key is valid but this host has never been told to trust it, and
    /// nobody is present to approve it now.
    UnknownClient,
    /// A person saw the request and said no. **Do not retry** — a client that
    /// reconnects after a refusal turns one prompt into a stream of them, which
    /// is how people learn to click Approve without reading.
    Denied,
    /// Nobody answered the approval prompt in time. Retryable, slowly.
    TimedOut,
    /// Trusted once, since removed.
    Revoked,
    /// Too many failures from this address lately.
    RateLimited,
    /// Protocol versions are not compatible.
    Version,
}

impl AuthFailure {
    /// Whether trying again could plausibly succeed.
    ///
    /// Exists so the retry decision is made once, here, rather than by each
    /// client's own reading of the variant names.
    #[must_use]
    pub const fn is_retryable(self) -> bool {
        matches!(self, Self::TimedOut | Self::RateLimited)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_absent_nonce_is_recognisable() {
        assert!(Nonce32::default().is_absent(), "the serde default must be detectable");
        assert!(!Nonce32::from_bytes([1; 32]).is_absent());
    }

    #[test]
    fn a_signature_does_not_dump_itself_into_the_logs() {
        let s = format!("{:?}", Sig64::from_bytes([0xab; 64]));
        assert_eq!(s, "Sig64(abababab..)");
    }

    #[test]
    fn refusals_that_a_client_must_not_retry_say_so() {
        // Denied is the one that matters: a client that retries after a person
        // said no converts a single prompt into a stream of them.
        assert!(!AuthFailure::Denied.is_retryable());
        assert!(!AuthFailure::Signature.is_retryable());
        assert!(!AuthFailure::UnknownClient.is_retryable());
        assert!(!AuthFailure::Revoked.is_retryable());
        assert!(AuthFailure::TimedOut.is_retryable());
        assert!(AuthFailure::RateLimited.is_retryable());
    }

    #[test]
    fn failures_travel_as_names_rather_than_numbers() {
        // A numeric discriminant would silently shift meaning the moment a
        // variant is inserted, and this enum is `non_exhaustive`.
        let json = serde_json::to_string(&AuthFailure::UnknownClient).expect("serialize");
        assert_eq!(json, "\"unknown_client\"");
    }
}
