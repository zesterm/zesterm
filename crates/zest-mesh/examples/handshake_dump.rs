//! Write `crates/zest-proto/fixtures/handshake.json` — the golden the browser
//! is held to.
//!
//! Everything else in `fixtures/` pins *encoding*: given these bytes, decode
//! this message. This one pins the *key schedule*, which no type check reaches
//! and no round trip catches. Two implementations can agree on every field of
//! the transcript, sign compatible signatures, and still derive different keys
//! — because one appended the DH publics in the other order, or hashed the
//! transcript with the domain included, or wrote `host->client` where the other
//! wrote `host→client`. Each of those is a one-character difference that
//! presents identically: the handshake completes and the first real frame does
//! not open.
//!
//! So the file carries every intermediate value and not merely the answer:
//! the transcript bytes, their hash, both directional keys, and sealed records
//! with their plaintexts and counters. A TypeScript peer that diverges at step
//! three fails at step three, by name.
//!
//! It lives here rather than beside `fixture_dump` because `zest-proto` has no
//! crypto dependency and must not gain one — it carries bytes, `zest-mesh`
//! decides what they mean. It writes into `zest-proto`'s fixture directory
//! because that is the one directory the web tests already read, and a second
//! location is a second thing to remember. `cargo xtask fixtures` runs both.

use std::path::PathBuf;

use serde::Serialize;
use zest_mesh::identity::{ClientIdentity, HostIdentity, Nonce, Purpose, Role, Signature};
use zest_mesh::pairing::{auth_transcript, pairing_code, Transcript};
use zest_mesh::secure::{DhPublic, EphemeralDh};

/// Bumped when the *shape* of this file changes, so a reader that predates a
/// field fails loudly rather than silently checking three of five values.
const SCHEMA: u32 = 1;

/// Fixed seeds. Not random, and that is the entire point.
const HOST_SEED: [u8; 32] = [0x11; 32];
const CLIENT_SEED: [u8; 32] = [0x22; 32];
const HOST_DH_SEED: [u8; 32] = [0x33; 32];
const CLIENT_DH_SEED: [u8; 32] = [0x44; 32];
const HOST_NONCE: [u8; 32] = [0x55; 32];
const CLIENT_NONCE: [u8; 32] = [0x66; 32];

#[derive(Serialize)]
struct Golden {
    schema: u32,
    /// Read this before implementing against the file.
    ///
    /// It is in the data rather than only in a comment here because the reader
    /// is a TypeScript file in another tree, and the one trap below cost a real
    /// hour on the *first* attempt at a second implementation.
    notes: Vec<&'static str>,
    protocol: u16,
    /// What went into the transcript, so a reader can rebuild it rather than
    /// trusting the bytes below.
    inputs: Inputs,
    /// The exact bytes both sides sign, hex. The single most load-bearing
    /// value here: everything downstream is a function of it.
    transcript: String,
    /// SHA-256 of `transcript`, which is the HKDF salt.
    transcript_hash: String,
    /// Both signatures over `transcript`, so a verifier can be checked without
    /// a signer.
    host_signature: String,
    client_signature: String,
    /// The six digits a person compares, derived from the same transcript
    /// under a different domain.
    pairing_code: String,
    /// The two directional keys. Named by direction, never by role: "the
    /// client's key" is ambiguous about which way it points, and that ambiguity
    /// is exactly how a peer ends up sealing with the key it should open with.
    key_host_to_client: String,
    key_client_to_host: String,
    records: Vec<Record>,
}

#[derive(Serialize)]
struct Inputs {
    /// The Ed25519 seeds, so a reader can derive the identities too rather
    /// than taking the public keys on trust. Secret in the sense that a real
    /// one would be; these are `0x11` and `0x22` repeated, and exist only so
    /// two implementations can be compared.
    host_seed: String,
    client_seed: String,
    host: String,
    client: String,
    host_label: String,
    client_label: String,
    host_nonce: String,
    client_nonce: String,
    host_dh: String,
    client_dh: String,
}

#[derive(Serialize)]
struct Record {
    /// Which direction sealed it, so a reader knows which key to use.
    from: &'static str,
    /// The nonce's two halves, spelled out. A reader that computes the nonce
    /// differently disagrees here rather than at the AEAD tag.
    epoch: u32,
    counter: u64,
    /// Hex, and short enough to read.
    plaintext: String,
    /// Framed exactly as it goes on the wire: `u32` LE length, then ciphertext
    /// and tag. The length describes the *ciphertext*, which is 16 bytes longer
    /// than the plaintext — the off-by-16 that only shows up on a maximal
    /// keyframe if it is got wrong.
    frame: String,
    note: &'static str,
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn main() {
    let host = HostIdentity::from_secret_bytes(&HOST_SEED);
    let client = ClientIdentity::from_secret_bytes(&CLIENT_SEED);
    let host_dh = EphemeralDh::from_seed(HOST_DH_SEED);
    let client_dh = EphemeralDh::from_seed(CLIENT_DH_SEED);

    let transcript = Transcript {
        version: zest_proto::PROTOCOL_VERSION,
        host: host.host_id(),
        client: client.client_id(),
        host_nonce: Nonce::from_bytes(HOST_NONCE),
        client_nonce: Nonce::from_bytes(CLIENT_NONCE),
        host_label: "andy-mac".into(),
        // Astral, on purpose. The label is length-prefixed in *bytes*, and an
        // emoji is exactly where a JavaScript implementation's `.length` --
        // UTF-16 code units -- disagrees with the byte count. CJK does not
        // catch it, because it is BMP where the two agree. The same blindness
        // cost this project a real bug in the grid corpus.
        client_label: "web \u{1F600}".into(),
        host_dh: host_dh.public().0,
        client_dh: client_dh.public().0,
    };

    let bytes = auth_transcript(&transcript).expect("these labels encode");
    let th = <sha2::Sha256 as sha2::Digest>::digest(&bytes);

    let host_sig: Signature = host.sign(Purpose::Auth, &bytes);
    let client_sig: Signature = client.sign(Purpose::Auth, &bytes);

    let mut h = host_dh.agree(DhPublic(transcript.client_dh), &th, Role::Host).expect("agree");
    let mut c = client_dh.agree(DhPublic(transcript.host_dh), &th, Role::Client).expect("agree");
    let (h2c, c2h) = h.keys();

    let mut records = Vec::new();
    let mut push = |from: &'static str,
                    ch: &mut zest_mesh::secure::SecureChannel,
                    plaintext: &[u8],
                    note: &'static str| {
        let ((epoch, counter), _) = ch.counters();
        let sealed = ch.seal(plaintext).expect("seal");
        let frame = zest_proto::frame::frame_bytes(&sealed).expect("frame");
        records.push(Record {
            from,
            epoch,
            counter,
            plaintext: hex(plaintext),
            frame: hex(&frame),
            note,
        });
    };

    push("client", &mut c, b"", "an empty body still seals -- ciphertext is the 16-byte tag alone");
    push("client", &mut c, &[0x01, 0x02, 0x03], "the second record, so the counter is seen to move");
    push("host", &mut h, b"welcome", "the other direction, under the other key");

    // A record whose counter is one below the ratchet point, and the one after
    // it -- which is the only place the epoch and the key change, and it
    // happens with nothing on the wire to announce it. An implementation that
    // never ratchets passes every test until 16 million frames into a long
    // session, which is a bug reported as "it dies after a few hours".
    let mut far = EphemeralDh::from_seed(CLIENT_DH_SEED)
        .agree(DhPublic(host_dh_public()), &th, Role::Client)
        .expect("agree");
    for _ in 0..(zest_mesh::secure::REKEY_AFTER - 1) {
        far.seal(b"").expect("seal");
    }
    push("client", &mut far, b"last-of-epoch-0", "counter = REKEY_AFTER - 1");
    push("client", &mut far, b"first-of-epoch-1", "the ratchet: epoch 1, counter 0, a new key, no message on the wire");

    let golden = Golden {
        schema: SCHEMA,
        notes: vec![
            "The session keys use HKDF (Extract with salt = transcript_hash, then Expand).              The ratchet uses HKDF-Expand ALONE -- the current key already is the PRK. Full              HKDF with an empty salt is a different function: `@noble/hashes/hkdf` exports              both `hkdf` and `expand`, and taking the wrong one is undetectable until 2^24              records into a session. Record 4 below is the only thing that catches it.",
            "The `v2` in the transcript's domain separator counts transcript layouts, not              protocol versions. The protocol here is 3. Do not derive one from the other.",
            "The frame's u32 LE prefix describes the CIPHERTEXT, which is the plaintext plus              a 16-byte tag. A sealer that bounds the plaintext against MAX_FRAME passes every              small test and fails only on a maximal keyframe.",
            "Nonce is epoch:u32 BE || counter:u64 BE, twelve bytes. Both counters start at 0              per direction, and the two directions have different keys.",
        ],
        protocol: zest_proto::PROTOCOL_VERSION,
        inputs: Inputs {
            host_seed: hex(&HOST_SEED),
            client_seed: hex(&CLIENT_SEED),
            host: hex(&transcript.host.0),
            client: hex(&transcript.client.0),
            host_label: transcript.host_label.clone(),
            client_label: transcript.client_label.clone(),
            host_nonce: hex(&HOST_NONCE),
            client_nonce: hex(&CLIENT_NONCE),
            host_dh: hex(&transcript.host_dh),
            client_dh: hex(&transcript.client_dh),
        },
        transcript: hex(&bytes),
        transcript_hash: hex(&th),
        host_signature: hex(&host_sig.to_bytes()),
        client_signature: hex(&client_sig.to_bytes()),
        pairing_code: pairing_code(&transcript).expect("a code"),
        key_host_to_client: hex(&h2c),
        key_client_to_host: hex(&c2h),
        records,
    };

    let path = fixtures_dir().join("handshake.json");
    let json = serde_json::to_string_pretty(&golden).expect("serialize");
    // A trailing newline, and LF: `.gitattributes` pins the directory to LF, so
    // writing CRLF here would make every Windows run a phantom diff.
    std::fs::write(&path, format!("{json}\n")).expect("write");
    println!("wrote {}", path.display());
}

fn host_dh_public() -> [u8; 32] {
    EphemeralDh::from_seed(HOST_DH_SEED).public().0
}

fn fixtures_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("crates/")
        .join("zest-proto")
        .join("fixtures")
}
