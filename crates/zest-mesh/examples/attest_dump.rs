//! Write `crates/zest-proto/fixtures/attest.json` — the device-attestation
//! golden the TypeScript implementations will be held to.
//!
//! An attestation is one signature over one byte layout, and both ends of it
//! will be separate implementations: an approver device (Rust, or a browser)
//! signs, and daemons and Workers verify. Without a vector one side produced,
//! a drift surfaces at bring-up as a voucher that is refused, with neither
//! implementation able to say which of them moved — the same failure mode
//! `handshake.json` exists for, one layer down.
//!
//! It lives here rather than beside `fixture_dump` because `zest-proto` has no
//! crypto dependency and must not gain one; it writes into `zest-proto`'s
//! fixture directory because that is the one directory the web tests already
//! read. `cargo xtask fixtures` runs it, `check-fixtures` diffs it, and
//! `attest::tests::the_golden_fixture_verifies_against_this_implementation`
//! loads it back so the file cannot drift from the code.

use std::path::PathBuf;

use serde::Serialize;
use zest_mesh::attest::{attestation_message, sign_attestation, Attestation};
use zest_mesh::identity::ClientIdentity;

/// Bumped when the *shape* of this file changes, so a reader that predates a
/// field fails loudly rather than silently checking three of five values.
const SCHEMA: u32 = 1;

/// Fixed seeds. Not random, and that is the entire point.
const APPROVER_SEED: [u8; 32] = [0x11; 32];
const DEVICE_SEED: [u8; 32] = [0x22; 32];
const PHONE_SEED: [u8; 32] = [0x33; 32];

/// A readable, fixed instant: 2025-08-12T12:00:00Z, epoch milliseconds.
const T0: u64 = 1_755_000_000_000;
const DAY: u64 = 86_400_000;

#[derive(Serialize)]
struct Golden {
    schema: u32,
    notes: Vec<&'static str>,
    cases: Vec<Case>,
}

#[derive(Serialize)]
struct Case {
    name: &'static str,
    /// The approver's Ed25519 seed, so a reader can derive the signing side too
    /// rather than only verifying. Secret in the sense that a real one would
    /// be; these are one byte repeated and exist only for comparison.
    approver_seed: String,
    attestation: Fields,
    /// The canonical message bytes, hex. A reader that builds them differently
    /// disagrees here, by name, rather than at the signature.
    message: String,
    /// Ed25519 over the signing preimage (see `notes`), hex.
    signature: String,
    /// The clock the verdict below was reached at.
    now_ms: u64,
    expect_verify: bool,
    note: &'static str,
}

#[derive(Serialize)]
struct Fields {
    v: u16,
    account: String,
    device: String,
    label: String,
    by: String,
    iat: u64,
    exp: u64,
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn case(
    name: &'static str,
    approver_seed: [u8; 32],
    a: &Attestation,
    now_ms: u64,
    expect_verify: bool,
    note: &'static str,
) -> Case {
    let approver = ClientIdentity::from_secret_bytes(&approver_seed);
    assert_eq!(
        approver.client_id(),
        a.by,
        "{name}: the fixture must be signed by the key it names as the approver"
    );
    let message = attestation_message(a).expect("fixture fields encode");
    let signature = sign_attestation(&approver, a).expect("fixture fields encode");
    Case {
        name,
        approver_seed: hex(&approver_seed),
        attestation: Fields {
            v: a.v,
            account: a.account.clone(),
            device: hex(&a.device.0),
            label: a.label.clone(),
            by: hex(&a.by.0),
            iat: a.iat,
            exp: a.exp,
        },
        message: hex(&message),
        signature: hex(&signature.to_bytes()),
        now_ms,
        expect_verify,
        note,
    }
}

fn main() {
    let approver = ClientIdentity::from_secret_bytes(&APPROVER_SEED);
    let device = ClientIdentity::from_secret_bytes(&DEVICE_SEED);
    let phone = ClientIdentity::from_secret_bytes(&PHONE_SEED);

    let plain = Attestation {
        v: 1,
        account: "acct_0123456789".into(),
        device: device.client_id(),
        label: "andy-mac".into(),
        by: approver.client_id(),
        iat: T0,
        exp: T0 + 30 * DAY,
    };

    // Astral, on purpose. The label is length-prefixed in *bytes*, and an emoji
    // is exactly where a JavaScript implementation's `.length` -- UTF-16 code
    // units -- disagrees with the byte count. CJK does not catch it, because it
    // is BMP where the two agree. The same blindness cost this project a real
    // bug in the grid corpus.
    let astral = Attestation {
        v: 1,
        account: "acct_0123456789".into(),
        device: phone.client_id(),
        label: "phone \u{1F600}".into(),
        by: approver.client_id(),
        iat: T0,
        exp: T0 + DAY,
    };

    // Same statement as `plain`, evaluated at its own expiry. The signature is
    // still perfectly valid Ed25519 -- what a reader must reproduce is the
    // *refusal*, which is where an implementation that never checks the window
    // (or checks it inclusively) parts ways with this one.
    let expired = plain.clone();

    let golden = Golden {
        schema: SCHEMA,
        notes: vec![
            "Signing preimage: \"zesterm-sig-v1\\0client\\0device-attestation\\0\" ++ message              (NUL bytes literal). Plain Ed25519 over that -- no Ed25519ctx, no prehash.",
            "Message: \"zesterm-attest-v1\" (17 bytes, no terminator) ++ u16be(v) ++              u16be(len(account)) ++ account ++ device (32 raw bytes) ++ u16be(len(label)) ++              label ++ by (32 raw bytes) ++ u64be(iat) ++ u64be(exp). Lengths count UTF-8              BYTES, never UTF-16 code units -- the astral case is what catches `.length`.",
            "Validity window is [iat, exp) in epoch milliseconds: iat inclusive, exp              exclusive. The expired case is evaluated at now_ms == exp exactly, so an              implementation that accepts `now <= exp` fails it.",
            "Verification is against `by` -- the approver -- and no other key. Whether `by`              is in the verifier's trust store is deliberately outside this layer:              non-transitivity (only locally-paired keys may vouch) is the daemon's check.",
        ],
        cases: vec![
            case(
                "plain",
                APPROVER_SEED,
                &plain,
                T0 + 1_000,
                true,
                "the ordinary voucher: ASCII everywhere, 30-day window, checked one second in",
            ),
            case(
                "astral-label",
                APPROVER_SEED,
                &astral,
                T0 + 1_000,
                true,
                "the label's length prefix is 10 (bytes), not 8 (UTF-16 units); a reader counting code units builds a different message and fails here by name",
            ),
            case(
                "expired",
                APPROVER_SEED,
                &expired,
                plain.exp,
                false,
                "MUST verify false: a valid signature evaluated at now_ms == exp, which is outside the half-open window",
            ),
        ],
    };

    let path = fixtures_dir().join("attest.json");
    let json = serde_json::to_string_pretty(&golden).expect("serialize");
    // A trailing newline, and LF: `.gitattributes` pins the directory to LF, so
    // writing CRLF here would make every Windows run a phantom diff.
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
