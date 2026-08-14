//! Write `crates/zest-proto/fixtures/link.json` — the browser hand-off golden
//! the TypeScript implementation will be held to.
//!
//! The link flow is two signatures over two byte layouts, signed in Rust by
//! the desktop app and verified in TypeScript by the Worker. Without a vector
//! one side produced, a drift surfaces at bring-up as a sign-in that hangs on
//! "pending" forever — the claim signature refused, with neither
//! implementation able to say which of them moved. Same failure mode
//! `attest.json` exists for, one flow over.
//!
//! It lives here rather than beside `fixture_dump` because `zest-proto` has no
//! crypto dependency and must not gain one; `cargo xtask fixtures` runs it,
//! `check-fixtures` diffs it, and
//! `link::tests::the_golden_fixture_verifies_against_this_implementation`
//! loads it back so the file cannot drift from the code.

use std::path::PathBuf;

use serde::Serialize;
use zest_mesh::identity::{ClientIdentity, Purpose};
use zest_mesh::link::{link_claim, link_request};

/// Bumped when the *shape* of this file changes, so a reader that predates a
/// field fails loudly rather than silently checking three of five values.
const SCHEMA: u32 = 1;

/// Fixed seed. Not random, and that is the entire point.
const DEVICE_SEED: [u8; 32] = [0x44; 32];

/// A grant id shaped like the real thing: `base64url(32 bytes)`, 43 chars.
/// The bytes underneath are 0x5a repeated — fixed, so the fixture is stable.
const GRANT: &str = "WlpaWlpaWlpaWlpaWlpaWlpaWlpaWlpaWlpaWlpaWlo";

#[derive(Serialize)]
struct Golden {
    schema: u32,
    notes: Vec<&'static str>,
    cases: Vec<Case>,
}

#[derive(Serialize)]
struct Case {
    name: &'static str,
    /// The device's Ed25519 seed, so a reader can derive the signing side too.
    seed: String,
    /// The device's public key — the `ClientId` — hex.
    key: String,
    /// Which verifier the case exercises: the message below is built by that
    /// verifier's layout, and the signature is checked against it.
    verify_as: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    label: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    grant: Option<&'static str>,
    /// The canonical message bytes, hex. A reader that builds them differently
    /// disagrees here, by name, rather than at the signature.
    message: String,
    /// Ed25519 over the signing preimage (see `notes`), hex.
    signature: String,
    expect_verify: bool,
    note: &'static str,
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn main() {
    let device = ClientIdentity::from_secret_bytes(&DEVICE_SEED);
    let key = device.client_id();
    let key_hex = hex(&key.0);

    let request = link_request(&key.0, "andy-desktop").expect("fits");
    let request_sig = device.sign(Purpose::Enrollment, &request);

    // Astral, on purpose: the label is length-prefixed in *bytes*, and an
    // emoji is exactly where a JavaScript `.length` — UTF-16 code units —
    // disagrees with the byte count. CJK does not catch it, because it is BMP
    // where the two agree.
    let astral = link_request(&key.0, "desktop \u{1F600}").expect("fits");
    let astral_sig = device.sign(Purpose::Enrollment, &astral);

    let claim = link_claim(GRANT, &key.0).expect("fits");
    let claim_sig = device.sign(Purpose::Enrollment, &claim);

    let golden = Golden {
        schema: SCHEMA,
        notes: vec![
            "Signing preimage: \"zesterm-sig-v1\\0client\\0enrollment\\0\" ++ message (NUL bytes              literal). Plain Ed25519 over that -- no Ed25519ctx, no prehash. No new Purpose:              the zesterm-link-v1 domain separates these from zesterm-enroll-v1 and              zesterm-register-v1 at byte 8, before any caller-supplied field.",
            "Request message: \"zesterm-link-v1\" (15 bytes, no terminator) ++ 0x01 ++              key (32 raw bytes) ++ u16be(len(label)) ++ label. Lengths count UTF-8 BYTES,              never UTF-16 code units -- the astral case is what catches `.length`.",
            "Claim message: \"zesterm-link-v1\" ++ 0x02 ++ u16be(len(grant)) ++ grant ++              key (32 raw bytes). The tag byte is load-bearing: both messages share the              domain and carry the same key plus one variable string in opposite orders,              so without it their disjointness would rest on key bytes never resembling a              length prefix. The cross-replay case pins the refusal.",
        ],
        cases: vec![
            Case {
                name: "request",
                seed: hex(&DEVICE_SEED),
                key: key_hex.clone(),
                verify_as: "request",
                label: Some("andy-desktop"),
                grant: None,
                message: hex(&request),
                signature: hex(&request_sig.to_bytes()),
                expect_verify: true,
                note: "the ordinary ask: proof of key possession under a label",
            },
            Case {
                name: "request-astral",
                seed: hex(&DEVICE_SEED),
                key: key_hex.clone(),
                verify_as: "request",
                label: Some("desktop \u{1F600}"),
                grant: None,
                message: hex(&astral),
                signature: hex(&astral_sig.to_bytes()),
                expect_verify: true,
                note: "the label's length prefix is 12 (bytes), not 10 (UTF-16 units); a reader counting code units builds a different message and fails here by name",
            },
            Case {
                name: "claim",
                seed: hex(&DEVICE_SEED),
                key: key_hex.clone(),
                verify_as: "claim",
                label: None,
                grant: Some(GRANT),
                message: hex(&claim),
                signature: hex(&claim_sig.to_bytes()),
                expect_verify: true,
                note: "the spend: grant ++ key, so a grant id captured from a browser URL claims nothing without the key",
            },
            Case {
                name: "request-replayed-as-claim",
                seed: hex(&DEVICE_SEED),
                key: key_hex,
                verify_as: "claim",
                label: None,
                // The request's label as the grant, the nastiest alignment
                // available: same key, same domain, one variable string each.
                grant: Some("andy-desktop"),
                message: hex(&link_claim("andy-desktop", &key.0).expect("fits")),
                signature: hex(&request_sig.to_bytes()),
                expect_verify: false,
                note: "MUST verify false: a valid approval-phase signature presented as a claim -- the tag byte is what refuses it",
            },
        ],
    };

    let path = fixtures_dir().join("link.json");
    let json = serde_json::to_string_pretty(&golden).expect("serialize");
    // A trailing newline, and LF: `.gitattributes` pins the directory to LF.
    std::fs::write(&path, format!("{json}\n")).expect("write");
    println!("wrote {}", path.display());
}

fn fixtures_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("crates/")
        .join("zest-proto")
        .join("fixtures")
}
