//! One trusted device vouching for a new one.
//!
//! Enrolment ([`crate::enroll`]) is a human carrying a code from a browser to a
//! machine. This is the other door into the fleet: a device that is *already*
//! paired signs a statement that a new device's key belongs to the same account,
//! and daemons accept the newcomer because they can verify the voucher against
//! a key they already trust.
//!
//! The signature binds the account, both keys and the label into one statement,
//! so none of them can be swapped afterwards: the approver vouched for *this*
//! key under *this* name on *this* account, within *this* window — not for
//! whichever key later claims the attestation.
//!
//! What this module deliberately does **not** decide: whether `by` is actually
//! in the verifier's trust store. Non-transitivity — only locally-paired keys
//! may vouch, so a chain of attestations is not a path into the fleet — is the
//! daemon's check against its own store, and encoding it here would mean a
//! missing check looks like a passing one.

use crate::identity::{ClientIdentity, Purpose, Signature, verify_client};
use zest_proto::ClientId;

/// Why an attestation message could not be built.
///
/// Fallible for the reason [`crate::enroll::EnrollError`] states: truncating an
/// oversize field would make two values sharing a 65535-byte prefix produce
/// identical signed bytes, and both strings here are caller-supplied.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum AttestError {
    #[error("account id is longer than 65535 bytes")]
    AccountTooLong,
    #[error("device label is longer than 65535 bytes")]
    LabelTooLong,
    #[error("the signing identity is not the approver the attestation names")]
    SignerIsNotApprover,
}

/// The domain this message lives in.
///
/// Separate from [`Purpose::DeviceAttestation`]'s place in the signing prefix
/// for the reason `enroll.rs` separates its own: the prefix says *what kind of
/// thing* is signed, this says *which message layout* follows. A future
/// attestation shape gets `v2` here and cannot be confused with this one.
const ATTEST_DOMAIN: &[u8] = b"zesterm-attest-v1";

/// What an approver signs when vouching for a new device.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Attestation {
    /// Layout version of the statement itself, signed so an old attestation
    /// cannot be replayed as a newer shape.
    pub v: u16,
    /// The control plane's user id. Signed so a voucher minted for one account
    /// cannot admit the same key to another.
    pub account: String,
    /// The key being vouched for.
    pub device: ClientId,
    /// Human name for the new device, so the entry on the device list cannot
    /// be renamed to impersonate another machine afterwards.
    pub label: String,
    /// The approver — the already-trusted key that signs. Verification is
    /// against this key and no other.
    pub by: ClientId,
    /// Issued at, epoch milliseconds.
    pub iat: u64,
    /// Expires at, epoch milliseconds. Exclusive: an attestation is dead the
    /// millisecond it names.
    pub exp: u64,
}

/// The exact bytes an approver signs.
///
/// **Length-prefixed, not NUL-separated.** [`crate::identity`]'s NUL form is
/// sound only because its variable field is last and the rest come from closed
/// enums; with two caller-supplied strings (`account` and `label`) that
/// reasoning does not survive — without prefixes, `("ab", "cd")` and
/// `("abc", "d")` would be the same bytes, so a signature over one would be a
/// valid signature over the other.
///
/// Layout, after the 17-byte domain: `u16be(v)`,
/// `u16be(len(account)) ++ account`, `device` (32 raw bytes),
/// `u16be(len(label)) ++ label`, `by` (32 raw bytes), `u64be(iat)`,
/// `u64be(exp)`. Lengths count *bytes*, never UTF-16 code units.
pub fn attestation_message(a: &Attestation) -> Result<Vec<u8>, AttestError> {
    let account_len = u16::try_from(a.account.len()).map_err(|_| AttestError::AccountTooLong)?;
    let label_len = u16::try_from(a.label.len()).map_err(|_| AttestError::LabelTooLong)?;

    let mut out = Vec::with_capacity(
        ATTEST_DOMAIN.len() + 2 + 2 + a.account.len() + 32 + 2 + a.label.len() + 32 + 8 + 8,
    );
    out.extend_from_slice(ATTEST_DOMAIN);
    out.extend_from_slice(&a.v.to_be_bytes());
    out.extend_from_slice(&account_len.to_be_bytes());
    out.extend_from_slice(a.account.as_bytes());
    out.extend_from_slice(&a.device.0);
    out.extend_from_slice(&label_len.to_be_bytes());
    out.extend_from_slice(a.label.as_bytes());
    out.extend_from_slice(&a.by.0);
    out.extend_from_slice(&a.iat.to_be_bytes());
    out.extend_from_slice(&a.exp.to_be_bytes());
    Ok(out)
}

/// Sign an attestation as the approver.
///
/// `identity` must be the identity behind [`Attestation::by`] — the approver's,
/// never the new device's — and a mismatch is an error *here*, not merely a
/// voucher that fails later.
pub fn sign_attestation(identity: &ClientIdentity, a: &Attestation) -> Result<Signature, AttestError> {
    // Refused at mint time rather than left for verification: a voucher signed
    // by a key other than `by` is guaranteed to fail [`verify_attestation`],
    // and that late `verify == false` names neither the mistake nor the side
    // that made it.
    if identity.client_id() != a.by {
        return Err(AttestError::SignerIsNotApprover);
    }
    Ok(identity.sign(Purpose::DeviceAttestation, &attestation_message(a)?))
}

/// Did the holder of `a.by` vouch for exactly this statement, and is it live?
///
/// Answers only "`a.by` signed *these* bytes and `now_ms` falls in
/// `[iat, exp)`" — whether `by` is in the verifier's trust store, and whether
/// it is allowed to vouch at all, is the caller's business, and deliberately
/// not mixed in here where a missing check would look like a passing one.
#[must_use]
pub fn verify_attestation(a: &Attestation, sig: &Signature, now_ms: u64) -> bool {
    // The window first: this runs on untrusted input, and the Ed25519 verify
    // is the expensive step, so a voucher that is dead on arrival is refused
    // before any signature work is spent on it.
    if now_ms < a.iat || now_ms >= a.exp {
        return false;
    }
    // A field too long to encode is refused rather than truncated-then-checked:
    // there is no signature that can be correct over bytes we decline to build.
    let Ok(message) = attestation_message(a) else { return false };
    verify_client(a.by, Purpose::DeviceAttestation, &message, sig).is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::keystore::SECRET_LEN;

    fn approver() -> ClientIdentity {
        ClientIdentity::from_secret_bytes(&[0x11; SECRET_LEN])
    }

    fn attestation(by: ClientId) -> Attestation {
        Attestation {
            v: 1,
            account: "acct_0123456789".into(),
            device: ClientIdentity::from_secret_bytes(&[0x22; SECRET_LEN]).client_id(),
            label: "andy-phone".into(),
            by,
            iat: 1_755_000_000_000,
            exp: 1_755_086_400_000,
        }
    }

    #[test]
    fn an_approver_can_vouch_and_the_voucher_verifies() {
        let me = approver();
        let a = attestation(me.client_id());
        let sig = sign_attestation(&me, &a).expect("fits");
        assert!(
            verify_attestation(&a, &sig, a.iat + 1_000),
            "the whole point: a trusted device's voucher must be checkable \
             against the key the fleet already knows"
        );
    }

    #[test]
    fn the_window_includes_iat_and_excludes_exp() {
        let me = approver();
        let a = attestation(me.client_id());
        let sig = sign_attestation(&me, &a).expect("fits");

        assert!(
            verify_attestation(&a, &sig, a.iat),
            "iat is inclusive: an attestation minted this millisecond is live \
             this millisecond, or the common sign-then-verify-immediately \
             sequence fails on a fast clock"
        );
        assert!(
            verify_attestation(&a, &sig, a.exp - 1),
            "the last millisecond before exp is still inside the window"
        );
        assert!(
            !verify_attestation(&a, &sig, a.exp),
            "exp is exclusive: an attestation is dead the millisecond it names, \
             and both implementations must agree on the boundary or one accepts \
             what the other refuses"
        );
        assert!(
            !verify_attestation(&a, &sig, a.iat - 1),
            "a voucher is not live before it was issued: accepting one dated in \
             the future would let a stolen approver key mint vouchers that \
             outlive its revocation"
        );
        assert!(
            !verify_attestation(&a, &Signature::from_bytes([0; 64]), a.exp),
            "outside the window the verdict is the window's alone -- the check \
             short-circuits before any signature work, so a garbage signature \
             gets the same refusal as a valid one"
        );
    }

    #[test]
    fn a_signature_by_a_key_other_than_by_is_rejected() {
        // The approver named in the statement and the key that signed it must
        // be the same key. Otherwise anyone could mint a voucher naming a
        // trusted device as its approver and sign it themselves. Signed by
        // hand rather than through `sign_attestation`, which refuses to mint
        // this mismatch -- an attacker is not obliged to use our signer.
        let impostor = ClientIdentity::from_secret_bytes(&[0x99; SECRET_LEN]);
        let a = attestation(approver().client_id());
        let sig = impostor.sign(
            Purpose::DeviceAttestation,
            &attestation_message(&a).expect("fits"),
        );
        assert!(
            !verify_attestation(&a, &sig, a.iat + 1_000),
            "verification must use `a.by`, never whichever key happened to sign"
        );
    }

    #[test]
    fn signing_as_a_key_other_than_by_is_refused_at_mint_time() {
        // A voucher signed by a key other than `by` is guaranteed to fail
        // verification later, and that late `verify == false` names neither
        // the mistake nor the side that made it -- so the mismatch is an error
        // here, where the caller holding the wrong identity can still be told
        // which one.
        let impostor = ClientIdentity::from_secret_bytes(&[0x99; SECRET_LEN]);
        let a = attestation(approver().client_id());
        assert_eq!(
            sign_attestation(&impostor, &a),
            Err(AttestError::SignerIsNotApprover),
            "minting a voucher the verifier is certain to refuse helps nobody"
        );
    }

    #[test]
    fn changing_any_signed_field_invalidates_it() {
        let me = approver();
        let a = attestation(me.client_id());
        let sig = sign_attestation(&me, &a).expect("fits");
        let now = a.iat + 1_000;

        let mut other = a.clone();
        other.account = "acct_someone_else".into();
        assert!(!verify_attestation(&other, &sig, now), "a different account");

        let mut other = a.clone();
        other.device = ClientIdentity::from_secret_bytes(&[0x33; SECRET_LEN]).client_id();
        assert!(
            !verify_attestation(&other, &sig, now),
            "a different device key -- the voucher must not transfer to a key \
             the approver never saw"
        );

        let mut other = a.clone();
        other.label = "not-my-name".into();
        assert!(!verify_attestation(&other, &sig, now), "a different label");

        let mut other = a.clone();
        other.exp += 1;
        assert!(
            !verify_attestation(&other, &sig, now),
            "a stretched window -- exp is signed, so an attacker cannot extend \
             a voucher they intercepted"
        );
    }

    #[test]
    fn the_boundary_between_account_and_device_cannot_be_moved() {
        // The reason this message is length-prefixed. The device id follows the
        // account directly, so without a prefix the account/device boundary is
        // movable: the two statements below concatenate to identical bytes --
        // ("ab", 'c'+[0x41;31], "cd") and ("abc", [0x41;31]+'c', "d") -- while
        // naming *different device keys*. A signature over one would then vouch
        // for a key the approver never saw, and the account is caller-supplied,
        // so "nobody would do that" is not available.
        let a = attestation(approver().client_id());

        let mut left_device = [0x41u8; 32];
        left_device[0] = b'c';
        let mut left = a.clone();
        left.account = "ab".into();
        left.device = ClientId::from_bytes(left_device);
        left.label = "cd".into();

        let mut right_device = [0x41u8; 32];
        right_device[31] = b'c';
        let mut right = a;
        right.account = "abc".into();
        right.device = ClientId::from_bytes(right_device);
        right.label = "d".into();

        assert_ne!(
            attestation_message(&left).expect("fits"),
            attestation_message(&right).expect("fits"),
            "field boundaries must be unambiguous, or two different devices \
             share one valid signature"
        );
    }

    #[test]
    fn an_unencodable_field_is_refused_rather_than_truncated() {
        // Truncating would mean two accounts (or labels) sharing their first
        // 65535 bytes produce identical signed bytes, so a signature over one
        // verifies the other -- and both strings are caller-supplied.
        let huge = "x".repeat(usize::from(u16::MAX) + 1);

        let mut a = attestation(approver().client_id());
        a.account = huge.clone();
        assert_eq!(attestation_message(&a), Err(AttestError::AccountTooLong));

        let mut a = attestation(approver().client_id());
        a.label = huge;
        assert_eq!(attestation_message(&a), Err(AttestError::LabelTooLong));

        // Exactly at the boundary still encodes, so the limit is the
        // encoding's and not an arbitrary one.
        let mut a = attestation(approver().client_id());
        a.label = "x".repeat(u16::MAX.into());
        assert!(attestation_message(&a).is_ok());
    }

    #[test]
    fn an_attestation_signature_is_not_an_auth_signature() {
        // `Purpose` is inside the signing prefix, so a signature harvested from
        // one flow cannot be replayed into the other. Without this, getting a
        // device to answer an auth challenge would also get it to vouch.
        let me = approver();
        let a = attestation(me.client_id());
        let message = attestation_message(&a).expect("fits");
        let auth_sig = me.sign(Purpose::Auth, &message);
        assert!(
            !verify_attestation(&a, &auth_sig, a.iat + 1_000),
            "a signature made for Auth must not verify as DeviceAttestation"
        );
    }

    #[test]
    fn the_golden_fixture_verifies_against_this_implementation() {
        // `fixtures/attest.json` is what the TypeScript implementations will be
        // written against, and this test is what keeps the file from drifting
        // from the code between `cargo xtask fixtures` runs: every signature in
        // it must verify (or, for the expired case, fail) *here* too, so the
        // fixture and the Rust cannot disagree without a red test naming which.
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../zest-proto/fixtures/attest.json");
        let json: serde_json::Value = serde_json::from_str(
            &std::fs::read_to_string(&path).expect("attest.json exists -- run `cargo xtask fixtures`"),
        )
        .expect("attest.json parses");

        let cases = json["cases"].as_array().expect("cases");
        assert!(cases.len() >= 3, "the fixture pins at least three cases");

        let mut saw_a_failure = false;
        for case in cases {
            let name = case["name"].as_str().expect("name");
            let a = Attestation {
                v: u16::try_from(case["attestation"]["v"].as_u64().expect("v")).expect("u16"),
                account: case["attestation"]["account"].as_str().expect("account").into(),
                device: ClientId::from_bytes(hex32(case["attestation"]["device"].as_str().expect("device"))),
                label: case["attestation"]["label"].as_str().expect("label").into(),
                by: ClientId::from_bytes(hex32(case["attestation"]["by"].as_str().expect("by"))),
                iat: case["attestation"]["iat"].as_u64().expect("iat"),
                exp: case["attestation"]["exp"].as_u64().expect("exp"),
            };

            let message = attestation_message(&a).expect("fixture fields encode");
            assert_eq!(
                hex(&message),
                case["message"].as_str().expect("message"),
                "{name}: the canonical message bytes moved -- a TS implementation \
                 built against the file would now disagree with the Rust"
            );

            let sig = Signature::from_slice(&hex_bytes(case["signature"].as_str().expect("signature")))
                .expect("64 bytes");
            let now = case["now_ms"].as_u64().expect("now_ms");
            let expect = case["expect_verify"].as_bool().expect("expect_verify");
            saw_a_failure |= !expect;
            assert_eq!(
                verify_attestation(&a, &sig, now),
                expect,
                "{name}: the fixture says this must {}",
                if expect { "verify" } else { "be refused" }
            );
        }
        assert!(
            saw_a_failure,
            "the fixture must carry a case that verifies false, or a TS \
             implementation that accepts everything passes it"
        );
    }

    fn hex(bytes: &[u8]) -> String {
        bytes.iter().map(|b| format!("{b:02x}")).collect()
    }

    fn hex_bytes(s: &str) -> Vec<u8> {
        assert_eq!(s.len() % 2, 0, "even-length hex");
        (0..s.len() / 2)
            .map(|i| u8::from_str_radix(&s[i * 2..i * 2 + 2], 16).expect("hex"))
            .collect()
    }

    fn hex32(s: &str) -> [u8; 32] {
        <[u8; 32]>::try_from(hex_bytes(s).as_slice()).expect("32 bytes")
    }
}
