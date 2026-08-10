//! The channel that makes a relay a pipe rather than a party.
//!
//! Everything after the handshake used to be plaintext MessagePack — fine on a
//! unix socket, defensible on a LAN, and unacceptable the moment a Durable
//! Object sits between two machines and copies bytes. ADR-006 says Cloudflare
//! holds a directory and is "never in the data path"; this is what lets a relay
//! exist without contradicting that, because a pipe that cannot read what it
//! carries is not in the trust path even when it is in the byte path.
//!
//! # Why this rather than Noise
//!
//! Noise is the textbook answer and the wrong one here. Two independent
//! implementations of a *framework* is far more surface than two of one
//! specified handshake, `snow` has no browser twin the web client could use,
//! and the properties Noise would buy — identity hiding, 0-RTT — are not wanted.
//!
//! [`crate::pairing::auth_transcript`] already makes both sides sign identical
//! bytes. Adding two 32-byte fields to it makes the *existing* Ed25519
//! signatures certify the ephemeral DH keys: no certificate type, no static
//! X25519 to store or rotate, and forward secrecy for free.
//!
//! # Why the identity key is not converted
//!
//! Both dalek and `@noble` will map an Ed25519 key to Montgomery form, and it
//! is refused here. It reuses one key across two algorithms, defeats the
//! `Role`/`Purpose` domain separation the signing preimage invests in, and
//! gives up forward secrecy — a key compromised later would decrypt every
//! session recorded before it. The identity key signs; ephemeral keys agree.

use chacha20poly1305::aead::{Aead, KeyInit, Payload};
use chacha20poly1305::{ChaCha20Poly1305, Key, Nonce};
use hkdf::Hkdf;
use sha2::Sha256;
use x25519_dalek::{PublicKey, StaticSecret};
use zeroize::Zeroizing;

use crate::identity::Role;
use crate::MeshError;

/// An X25519 public key, ephemeral to one connection.
pub const DH_PUB_LEN: usize = 32;

/// Poly1305's tag. A sealed record is plaintext + this.
pub const TAG_LEN: usize = 16;

/// Records per key before the ratchet turns.
///
/// 2^24 is far below where ChaCha20-Poly1305's bounds bite and far above any
/// real session — a keystroke-per-record link would need weeks. It is low
/// enough that the rekey path is reachable in a test, which matters more:
/// a ratchet that never runs in anger is a ratchet nobody has checked.
pub const REKEY_AFTER: u64 = 1 << 24;

/// The most plaintext a single record may carry.
///
/// `frame::encode` checks its body against `MAX_FRAME` *before* prefixing, and
/// what it will be handed is the ciphertext — plaintext plus [`TAG_LEN`]. So
/// the limit has to be lower here by exactly the tag, or a maximal keyframe
/// encodes fine and then fails at the frame layer, which only happens on very
/// large grids and reads as a framing bug.
pub const MAX_PLAINTEXT: usize = zest_proto::frame::MAX_FRAME - TAG_LEN;

/// An X25519 public key on the wire.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DhPublic(pub [u8; DH_PUB_LEN]);

impl DhPublic {
    /// All zero — what a peer that sent no key looks like after `serde(default)`.
    ///
    /// Checked by name rather than by comparing to a literal, for the same
    /// reason [`crate::identity::Nonce`] has `is_absent`: a peer that omits the
    /// field must be refused loudly rather than silently agreeing on a shared
    /// secret with a known value.
    #[must_use]
    pub fn is_absent(&self) -> bool {
        self.0 == [0u8; DH_PUB_LEN]
    }
}

/// One connection's private half. Consumed by [`EphemeralDh::agree`].
pub struct EphemeralDh {
    secret: StaticSecret,
}

impl EphemeralDh {
    /// A fresh key, for this connection and no other.
    ///
    /// `StaticSecret` rather than `EphemeralSecret` despite the name: the
    /// ephemeral type cannot expose its public key and be consumed separately,
    /// and the handshake needs the public half on the wire before the peer's
    /// arrives. The lifetime is enforced by this type being consumed on
    /// `agree`, not by the dalek type's name.
    pub fn generate() -> Result<Self, MeshError> {
        let mut seed = Zeroizing::new([0u8; 32]);
        getrandom::fill(seed.as_mut())
            .map_err(|e| MeshError::Identity(format!("no randomness for a session key: {e}")))?;
        Ok(Self { secret: StaticSecret::from(*seed) })
    }

    /// A key from a fixed seed, for the golden fixtures and nothing else.
    ///
    /// `#[doc(hidden)]` rather than `#[cfg(test)]`: `handshake_dump` is an
    /// example, not a test, and the whole point of that file is that the
    /// TypeScript implementation can be held to the same key schedule -- which
    /// is impossible if every run produces different keys.
    #[doc(hidden)]
    #[must_use]
    pub fn from_seed(seed: [u8; 32]) -> Self {
        Self { secret: StaticSecret::from(seed) }
    }

    #[must_use]
    pub fn public(&self) -> DhPublic {
        DhPublic(PublicKey::from(&self.secret).to_bytes())
    }

    /// Agree on a channel, consuming the secret.
    ///
    /// `transcript_hash` is SHA-256 over the bytes both sides signed, so the
    /// derived keys are bound to *this* handshake: the ids, the nonces, the
    /// labels, the version and both DH publics. A relay that swapped a key
    /// would produce a different transcript, and the signatures over it would
    /// no longer verify.
    ///
    /// `role` decides which derived key sends and which receives. Directional
    /// keys mean a record cannot be reflected back down the other direction,
    /// and the two counters never share a nonce space.
    pub fn agree(
        self,
        theirs: DhPublic,
        transcript_hash: &[u8],
        role: Role,
    ) -> Result<SecureChannel, MeshError> {
        if theirs.is_absent() {
            return Err(MeshError::BadSignature(
                "peer sent no ephemeral key; refusing to derive a channel".into(),
            ));
        }

        let shared = self.secret.diffie_hellman(&PublicKey::from(theirs.0));
        // A low-order peer key drives the output to zero, which both sides
        // would then "agree" on without either contributing. dalek exposes the
        // check; taking it is the difference between a shared secret and a
        // constant an attacker chose.
        if !shared.was_contributory() {
            return Err(MeshError::BadSignature(
                "peer's ephemeral key is non-contributory".into(),
            ));
        }

        let hk = Hkdf::<Sha256>::new(Some(transcript_hash), shared.as_bytes());
        let h2c = expand(&hk, b"zesterm-e2e-v1 host->client")?;
        let c2h = expand(&hk, b"zesterm-e2e-v1 client->host")?;

        let (send, recv) = match role {
            Role::Host => (h2c, c2h),
            Role::Client => (c2h, h2c),
        };
        Ok(SecureChannel { send: Direction::new(send)?, recv: Direction::new(recv)? })
    }
}

fn expand(hk: &Hkdf<Sha256>, info: &[u8]) -> Result<Zeroizing<[u8; 32]>, MeshError> {
    let mut out = Zeroizing::new([0u8; 32]);
    hk.expand(info, out.as_mut())
        .map_err(|e| MeshError::Identity(format!("key schedule failed: {e}")))?;
    Ok(out)
}

/// One direction's key, counter and epoch.
struct Direction {
    cipher: ChaCha20Poly1305,
    key: Zeroizing<[u8; 32]>,
    epoch: u32,
    counter: u64,
}

impl Direction {
    fn new(key: Zeroizing<[u8; 32]>) -> Result<Self, MeshError> {
        let cipher = ChaCha20Poly1305::new(Key::from_slice(key.as_ref()));
        Ok(Self { cipher, key, epoch: 0, counter: 0 })
    }

    /// `epoch: u32 BE || counter: u64 BE`, twelve bytes.
    fn nonce(&self) -> Nonce {
        let mut n = [0u8; 12];
        n[..4].copy_from_slice(&self.epoch.to_be_bytes());
        n[4..].copy_from_slice(&self.counter.to_be_bytes());
        *Nonce::from_slice(&n)
    }

    /// Advance, ratcheting the key when the counter wraps.
    ///
    /// **Deterministic, and never announced on the wire.** Both sides count the
    /// same records in the same order, so both turn the ratchet at the same
    /// point — and a rekey *message* would be one more thing two independent
    /// implementations could disagree about, at the exact moment a
    /// disagreement is undetectable until the next record fails to open.
    fn advance(&mut self) -> Result<(), MeshError> {
        self.counter += 1;
        if self.counter < REKEY_AFTER {
            return Ok(());
        }
        let hk = Hkdf::<Sha256>::from_prk(self.key.as_ref())
            .map_err(|e| MeshError::Identity(format!("rekey failed: {e}")))?;
        let next = expand(&hk, b"zesterm-rekey-v1")?;
        self.cipher = ChaCha20Poly1305::new(Key::from_slice(next.as_ref()));
        self.key = next;
        self.counter = 0;
        self.epoch = self.epoch.checked_add(1).ok_or_else(|| {
            MeshError::Identity("this channel has run out of epochs; redial".into())
        })?;
        Ok(())
    }
}

/// A sealed, ordered, one-connection channel.
///
/// Ordering is assumed and is given: TCP and WebSocket both deliver in order
/// and without gaps, and a link that loses either is closed rather than
/// resynchronised. A reconnect is a **fresh handshake, never a resumption** —
/// `remote.rs` already works that way — so there is no state here for a relay
/// to preserve across a drop, and nothing to replay into.
pub struct SecureChannel {
    send: Direction,
    recv: Direction,
}

impl SecureChannel {
    /// Seal one frame's body.
    pub fn seal(&mut self, plaintext: &[u8]) -> Result<Vec<u8>, MeshError> {
        Self::seal_with(&mut self.send, plaintext)
    }

    /// Open one frame's body.
    ///
    /// A failure here is not recoverable and must close the link. The counter
    /// is *not* advanced on failure: a record that did not open was not a
    /// record, and skipping past it would desynchronise the two sides in a way
    /// that looks like corruption several frames later.
    pub fn open(&mut self, ciphertext: &[u8]) -> Result<Vec<u8>, MeshError> {
        Self::open_with(&mut self.recv, ciphertext)
    }

    /// The one implementation, so a split channel and a joined one cannot
    /// drift. A second copy of this is a second nonce schedule.
    fn seal_with(d: &mut Direction, plaintext: &[u8]) -> Result<Vec<u8>, MeshError> {
        if plaintext.len() > MAX_PLAINTEXT {
            return Err(MeshError::Identity(format!(
                "{} bytes is more than a frame can carry once sealed",
                plaintext.len()
            )));
        }
        let out = d
            .cipher
            .encrypt(&d.nonce(), Payload { msg: plaintext, aad: &[] })
            .map_err(|_| MeshError::Identity("sealing failed".into()))?;
        d.advance()?;
        Ok(out)
    }

    fn open_with(d: &mut Direction, ciphertext: &[u8]) -> Result<Vec<u8>, MeshError> {
        let out = d
            .cipher
            .decrypt(&d.nonce(), Payload { msg: ciphertext, aad: &[] })
            .map_err(|_| MeshError::BadSignature("a sealed frame did not open".into()))?;
        d.advance()?;
        Ok(out)
    }

    /// Split into the two halves, for a reader thread and a writer thread.
    ///
    /// The two directions have separate keys and separate counters and share no
    /// state, so this needs no lock — which is the point. The alternative, one
    /// `Mutex<SecureChannel>` held by both threads, is the deadlock `ws.rs`
    /// documents in a new hat: the reader sits in a blocking read while holding
    /// it, which is exactly what a client does while it is quiet, and the
    /// writer never gets in.
    #[must_use]
    pub fn split(self) -> (Sealer, Opener) {
        (Sealer(self.send), Opener(self.recv))
    }

    /// The two directional keys, for the golden fixture only.
    ///
    /// Exposed so `handshake.json` can pin the *key schedule* and not merely
    /// its output: a TypeScript peer whose HKDF info strings differ produces
    /// different keys, and comparing only sealed records would say "it did not
    /// open" without naming which step diverged.
    #[doc(hidden)]
    #[must_use]
    pub fn keys(&self) -> ([u8; 32], [u8; 32]) {
        (*self.send.key, *self.recv.key)
    }

    /// For tests and for the fixture dump; never used on a live link.
    #[doc(hidden)]
    #[must_use]
    pub fn counters(&self) -> ((u32, u64), (u32, u64)) {
        ((self.send.epoch, self.send.counter), (self.recv.epoch, self.recv.counter))
    }
}

/// The outgoing half of a split channel.
pub struct Sealer(Direction);

impl Sealer {
    /// Seal one frame's body. See [`SecureChannel::seal`].
    pub fn seal(&mut self, plaintext: &[u8]) -> Result<Vec<u8>, MeshError> {
        SecureChannel::seal_with(&mut self.0, plaintext)
    }
}

/// The incoming half of a split channel.
pub struct Opener(Direction);

impl Opener {
    /// Open one frame's body. See [`SecureChannel::open`].
    pub fn open(&mut self, ciphertext: &[u8]) -> Result<Vec<u8>, MeshError> {
        SecureChannel::open_with(&mut self.0, ciphertext)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A host and a client that agreed on the same transcript.
    fn pair() -> (SecureChannel, SecureChannel) {
        let h = EphemeralDh::generate().expect("randomness");
        let c = EphemeralDh::generate().expect("randomness");
        let (hp, cp) = (h.public(), c.public());
        let th = [7u8; 32];
        (
            h.agree(cp, &th, Role::Host).expect("host agrees"),
            c.agree(hp, &th, Role::Client).expect("client agrees"),
        )
    }

    #[test]
    fn what_one_seals_the_other_opens() {
        let (mut host, mut client) = pair();
        let sealed = host.seal(b"hello from the host").expect("seals");
        assert_ne!(sealed, b"hello from the host", "the wire must not carry plaintext");
        assert_eq!(client.open(&sealed).expect("opens"), b"hello from the host");

        let back = client.seal(b"and back").expect("seals");
        assert_eq!(host.open(&back).expect("opens"), b"and back");
    }

    #[test]
    fn a_record_cannot_be_reflected_down_the_other_direction() {
        // The reason the keys are directional. Without it, a relay could echo a
        // host's frame back at the host and have it open — replaying whatever
        // the host last said as though the client had said it.
        let (mut host, _client) = pair();
        let sealed = host.seal(b"a message from the host").expect("seals");
        assert!(host.open(&sealed).is_err(), "a host must not open its own record");
    }

    #[test]
    fn a_peer_that_sent_no_key_is_refused() {
        // `Pub32` is `serde(default)`, so a peer that omits the field arrives
        // as all zeroes. Deriving from it would agree on a value the peer never
        // contributed to — and both sides would agree, so nothing would look
        // wrong until someone read the wire.
        let dh = EphemeralDh::generate().expect("randomness");
        let err = dh.agree(DhPublic([0u8; 32]), &[7u8; 32], Role::Host);
        assert!(err.is_err(), "an absent ephemeral key must not derive a channel");
    }

    #[test]
    fn a_low_order_key_is_refused() {
        // A non-contributory peer key drives the shared secret to zero, so an
        // attacker in the middle picks the key both sides use. dalek can tell;
        // the check is the difference between a secret and a constant.
        let dh = EphemeralDh::generate().expect("randomness");
        // The order-8 point that is the classic small-subgroup input.
        let mut low = [0u8; 32];
        low[0] = 1;
        assert!(
            dh.agree(DhPublic(low), &[7u8; 32], Role::Host).is_err(),
            "a low-order peer key must be refused"
        );
    }

    #[test]
    fn a_different_transcript_is_a_different_channel() {
        // The binding that makes a swapped DH key useless: the keys come from
        // the bytes both sides *signed*, so a relay that substituted one would
        // have to forge a signature to make the two agree.
        let h = EphemeralDh::generate().expect("randomness");
        let c = EphemeralDh::generate().expect("randomness");
        let (hp, cp) = (h.public(), c.public());
        let mut host = h.agree(cp, &[1u8; 32], Role::Host).expect("agrees");
        let mut client = c.agree(hp, &[2u8; 32], Role::Client).expect("agrees");

        let sealed = host.seal(b"secret").expect("seals");
        assert!(client.open(&sealed).is_err(), "different transcripts must not agree");
    }

    #[test]
    fn a_tampered_record_does_not_open() {
        let (mut host, mut client) = pair();
        let mut sealed = host.seal(b"the original message").expect("seals");
        let last = sealed.len() - 1;
        sealed[last] ^= 0x01;
        assert!(client.open(&sealed).is_err(), "one flipped bit must fail the tag");
    }

    #[test]
    fn records_out_of_order_do_not_open() {
        // The counter is implicit, so order is part of the contract. A relay
        // that reordered two frames would break the link rather than deliver
        // them wrongly, which is the failure worth having.
        let (mut host, mut client) = pair();
        let first = host.seal(b"first").expect("seals");
        let second = host.seal(b"second").expect("seals");
        assert!(client.open(&second).is_err(), "the second record is not the first");
        // And the receiver did not advance past the failure.
        assert_eq!(client.open(&first).expect("opens"), b"first");
    }

    #[test]
    fn a_failed_open_does_not_advance_the_counter() {
        // Advancing past a record that did not open would desynchronise the two
        // sides, and the symptom would appear several frames later as
        // corruption rather than here as a rejected frame.
        let (mut host, mut client) = pair();
        let good = host.seal(b"good").expect("seals");
        assert!(client.open(b"not a record at all").is_err());
        assert_eq!(client.open(&good).expect("opens"), b"good", "the channel is still in step");
    }

    #[test]
    fn the_ratchet_turns_at_the_same_point_on_both_sides() {
        // Deterministic and unannounced: both sides count the same records, so
        // both rekey at the same moment. If they did not, the first record
        // after the boundary would simply fail to open with nothing saying why.
        let (mut host, mut client) = pair();
        // Wind both to one record before the boundary without doing the work of
        // sealing 16 million frames.
        host.send.counter = REKEY_AFTER - 1;
        client.recv.counter = REKEY_AFTER - 1;

        let sealed = host.seal(b"the last record of the epoch").expect("seals");
        assert_eq!(client.open(&sealed).expect("opens"), b"the last record of the epoch");

        assert_eq!(host.counters().0, (1, 0), "the sender turned the ratchet");
        assert_eq!(client.counters().1, (1, 0), "and so did the receiver");

        let across = host.seal(b"the first of the next").expect("seals");
        assert_eq!(
            client.open(&across).expect("opens"),
            b"the first of the next",
            "the channel survives the boundary"
        );
    }

    #[test]
    fn plaintext_too_large_to_frame_is_refused_here() {
        // `frame::encode` checks the *ciphertext* against MAX_FRAME, so a
        // plaintext within a tag's width of the limit seals fine and then fails
        // at the frame layer — on very large grids only, reading as a framing
        // bug rather than as a limit.
        let (mut host, _client) = pair();
        assert!(host.seal(&vec![0u8; MAX_PLAINTEXT]).is_ok(), "exactly at the limit is fine");
        assert!(host.seal(&vec![0u8; MAX_PLAINTEXT + 1]).is_err(), "one byte over is refused");
    }

    #[test]
    fn a_split_channel_is_the_same_channel() {
        // The two halves go to two threads, so nothing forces them through the
        // same code path any more. If `split` gave either half a fresh counter
        // -- or if a second copy of the nonce schedule ever drifted from this
        // one -- a redial would look identical and every frame after the first
        // would fail to open.
        let (mut host, client) = pair();
        let a = host.seal(b"one").expect("seal");
        let b = host.seal(b"two").expect("seal");

        let (_, mut opener) = client.split();
        assert_eq!(opener.open(&a).expect("first"), b"one");
        assert_eq!(opener.open(&b).expect("second"), b"two", "the counter did not carry over");
    }

    #[test]
    fn an_empty_frame_still_seals() {
        // Not hypothetical: an ack carries almost nothing, and a channel that
        // could not seal an empty body would fail on the quietest link.
        let (mut host, mut client) = pair();
        let sealed = host.seal(b"").expect("seals");
        assert_eq!(sealed.len(), TAG_LEN, "an empty record is exactly a tag");
        assert_eq!(client.open(&sealed).expect("opens"), b"");
    }
}
