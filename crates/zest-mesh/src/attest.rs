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

/// The layout version [`attestation_message`] writes and [`decode_attestation`]
/// accepts. Anything else is refused unread — it is signed, so an old statement
/// cannot be replayed as a newer shape.
pub const ATTESTATION_VERSION: u16 = 1;

/// The longest window an attestation may claim, milliseconds: 365 days.
///
/// Pinned to `ATTESTATION_TTL_MS` in `cloud/packages/shared/src/attestation.ts`
/// — the two are separate projects and nothing compiles both, so this is the
/// `ENROLL_PATH` discipline again: a Rust minting `exp - iat` wider than the
/// TypeScript accepts produces vouchers the control plane refuses with a
/// `bad_request` that names a field and not the drift.
pub const ATTESTATION_TTL_MS: u64 = 365 * 24 * 60 * 60 * 1000;

/// An attestation in its wire form:
/// `base64url(message) "." base64url(signature)`, both parts unpadded.
///
/// The message is re-derived from the fields, which is safe *here* and only
/// here: [`attestation_message`] is deterministic, so the bytes signed and the
/// bytes encoded cannot differ when both come from the same fields — unlike
/// the verify side, which must use the bytes that arrived. Round-tripped
/// against [`decode_attestation`] and the golden fixture by the tests below,
/// because the far end is TypeScript and the framing is the contract.
pub fn encode_attestation(a: &Attestation, sig: &Signature) -> Result<String, AttestError> {
    let message = attestation_message(a)?;
    Ok(format!("{}.{}", base64url_encode(&message), base64url_encode(&sig.to_bytes())))
}

/// How long a blob may be before it is refused unread.
///
/// Mirrors `MAX_ATTESTATION_CHARS` in `cloud/packages/shared/src/attestation.ts`,
/// and for its reason: the blob is caller-supplied and reaches the decoder
/// before any signature is checked, so the cheap bound comes first. Two
/// kilobytes is nearly three times any honest blob.
pub const MAX_ATTESTATION_CHARS: usize = 2048;

/// An attestation as it travels: `base64url(message) "." base64url(signature)`,
/// both parts unpadded.
///
/// **Verification is over the bytes that arrived, never a re-encoding.** The
/// blob is stored and re-served verbatim by the control plane, and the message
/// this holds is exactly what was parsed — re-deriving it from the fields
/// would quietly bless any parse bug by verifying a message nobody sent.
#[derive(Debug, Clone)]
pub struct DecodedAttestation {
    pub fields: Attestation,
    /// The message exactly as it arrived — the bytes the signature covers.
    message: Vec<u8>,
    pub signature: Signature,
}

impl DecodedAttestation {
    /// The arrived message bytes, for anyone re-serving or pinning them.
    #[must_use]
    pub fn message(&self) -> &[u8] {
        &self.message
    }

    /// [`verify_attestation`], over the arrived bytes.
    #[must_use]
    pub fn verify(&self, now_ms: u64) -> bool {
        // The same gate order as `verify_attestation`: the window first,
        // because this runs on untrusted input and the Ed25519 verify is the
        // expensive step.
        if now_ms < self.fields.iat || now_ms >= self.fields.exp {
            return false;
        }
        verify_client(self.fields.by, Purpose::DeviceAttestation, &self.message, &self.signature)
            .is_ok()
    }
}

/// Split, decode and narrow — and **verify nothing**, the split
/// `cloud/packages/shared/src/attestation.ts` makes for the same reason: what
/// to verify against and at which clock is the caller's decision. Every
/// malformed shape collapses to one `None`.
///
/// The walk refuses short buffers, **trailing bytes**, and any version or
/// domain that is not this one. Trailing bytes matter more than they look: a
/// decoder that ignored them would accept two different blobs as one
/// statement, and the extra bytes would still be inside what the signature is
/// checked over — a mismatch that surfaces as "bad signature" and names
/// nothing.
#[must_use]
pub fn decode_attestation(text: &str) -> Option<DecodedAttestation> {
    if text.is_empty() || text.len() > MAX_ATTESTATION_CHARS {
        return None;
    }
    let mut parts = text.split('.');
    let (message, signature) = (parts.next()?, parts.next()?);
    if parts.next().is_some() {
        return None;
    }
    let message = base64url_decode(message)?;
    let signature = Signature::from_slice(&base64url_decode(signature)?).ok()?;
    let fields = parse_message(&message)?;
    Some(DecodedAttestation { fields, message, signature })
}

/// Unpadded base64url — the one spelling [`base64url_decode`] accepts, so an
/// encoded blob is its own canonical form by construction.
fn base64url_encode(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] =
        b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let b = [chunk[0], *chunk.get(1).unwrap_or(&0), *chunk.get(2).unwrap_or(&0)];
        let n = (u32::from(b[0]) << 16) | (u32::from(b[1]) << 8) | u32::from(b[2]);
        let chars = [(n >> 18) & 63, (n >> 12) & 63, (n >> 6) & 63, n & 63];
        for (i, ch) in chars.iter().enumerate() {
            // One output char per 6 input bits actually present; the unused
            // low bits of a final partial sextet are zero by the shifts
            // above, which is exactly the canonical form the decoder demands.
            if i <= chunk.len() {
                out.push(ALPHABET[*ch as usize] as char);
            }
        }
    }
    out
}

/// Strict base64url: the URL-safe alphabet, unpadded, nothing else.
///
/// `=` is refused rather than skipped — the TypeScript side neither emits nor
/// accepts it, and two spellings of one blob would be two identities for the
/// same statement wherever a blob is compared or deduplicated.
fn base64url_decode(text: &str) -> Option<Vec<u8>> {
    // A length of 1 mod 4 encodes a partial byte no encoder produces.
    if text.len() % 4 == 1 {
        return None;
    }
    let mut out = Vec::with_capacity(text.len() * 3 / 4);
    let mut acc: u32 = 0;
    let mut bits = 0u32;
    for &b in text.as_bytes() {
        let v = match b {
            b'A'..=b'Z' => b - b'A',
            b'a'..=b'z' => b - b'a' + 26,
            b'0'..=b'9' => b - b'0' + 52,
            b'-' => 62,
            b'_' => 63,
            _ => return None,
        };
        acc = (acc << 6) | u32::from(v);
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            #[allow(clippy::cast_possible_truncation)]
            out.push((acc >> bits) as u8);
        }
    }
    // The unused low bits of a final partial sextet must be zero, or the
    // encoding is not canonical: "-a" and "-Q" would both decode to 0xf9,
    // giving one blob two spellings — exactly the two-identities problem the
    // padding refusal above exists to prevent.
    if bits > 0 && acc & ((1 << bits) - 1) != 0 {
        return None;
    }
    Some(out)
}

/// Field by field off the wire bytes. `None` on anything malformed.
fn parse_message(message: &[u8]) -> Option<Attestation> {
    let mut c = Cursor { data: message, at: 0 };
    if c.take(ATTEST_DOMAIN.len())? != ATTEST_DOMAIN {
        return None;
    }
    let v = c.u16()?;
    if v != ATTESTATION_VERSION {
        return None;
    }
    let account = c.string()?;
    let device = ClientId::from_bytes(c.key()?);
    let label = c.string()?;
    let by = ClientId::from_bytes(c.key()?);
    let iat = c.u64()?;
    let exp = c.u64()?;
    if c.at != message.len() {
        return None;
    }
    Some(Attestation { v, account, device, label, by, iat, exp })
}

struct Cursor<'a> {
    data: &'a [u8],
    at: usize,
}

impl<'a> Cursor<'a> {
    fn take(&mut self, n: usize) -> Option<&'a [u8]> {
        let end = self.at.checked_add(n)?;
        let out = self.data.get(self.at..end)?;
        self.at = end;
        Some(out)
    }

    fn u16(&mut self) -> Option<u16> {
        Some(u16::from_be_bytes(self.take(2)?.try_into().ok()?))
    }

    fn u64(&mut self) -> Option<u64> {
        Some(u64::from_be_bytes(self.take(8)?.try_into().ok()?))
    }

    fn key(&mut self) -> Option<[u8; 32]> {
        self.take(32)?.try_into().ok()
    }

    fn string(&mut self) -> Option<String> {
        let len = usize::from(self.u16()?);
        // Invalid UTF-8 is a refusal, not U+FFFD: a string that re-encodes to
        // different bytes than were signed is a field the encoder side could
        // never have produced.
        String::from_utf8(self.take(len)?.to_vec()).ok()
    }
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

    #[test]
    fn a_blob_decodes_to_the_bytes_that_were_encoded() {
        // Round-trip through the wire form for every golden case, so the Rust
        // decoder and the TypeScript encoder are pinned to one blob framing:
        // base64url(message) "." base64url(signature), unpadded. The verdicts
        // must survive the trip too, expired case included.
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../zest-proto/fixtures/attest.json");
        let json: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).expect("attest.json"))
                .expect("parses");

        for case in json["cases"].as_array().expect("cases") {
            let name = case["name"].as_str().expect("name");
            let message = hex_bytes(case["message"].as_str().expect("message"));
            let signature = hex_bytes(case["signature"].as_str().expect("signature"));
            let blob = format!("{}.{}", base64url(&message), base64url(&signature));

            let decoded = decode_attestation(&blob)
                .unwrap_or_else(|| panic!("{name}: the golden blob must decode"));
            assert_eq!(decoded.message(), &message[..], "{name}: the arrived bytes are kept");
            assert_eq!(
                hex(&decoded.fields.device.0),
                case["attestation"]["device"].as_str().expect("device"),
                "{name}: the parsed device key"
            );
            assert_eq!(
                decoded.fields.label,
                case["attestation"]["label"].as_str().expect("label"),
                "{name}: the parsed label, UTF-8 bytes and not UTF-16 units"
            );
            assert_eq!(
                decoded.verify(case["now_ms"].as_u64().expect("now_ms")),
                case["expect_verify"].as_bool().expect("expect_verify"),
                "{name}: the verdict must survive the wire form"
            );
        }
    }

    #[test]
    fn an_encoded_attestation_round_trips_and_verifies() {
        // The app's whole mint path in one loop: fields → sign → encode →
        // decode → verify, with the decoded fields byte-equal to what was
        // signed. This is what makes `encode_attestation` safe to re-derive
        // the message from its fields — determinism, proven rather than
        // assumed.
        let me = approver();
        let a = attestation(me.client_id());
        let sig = sign_attestation(&me, &a).expect("fits");
        let blob = encode_attestation(&a, &sig).expect("fits");

        let decoded = decode_attestation(&blob).expect("our own encoding must decode");
        assert_eq!(decoded.fields, a, "every field survives the wire form");
        assert_eq!(
            decoded.message(),
            attestation_message(&a).expect("fits"),
            "the encoded message is the signed message — re-derivation is sound only \
             while this holds"
        );
        assert!(
            decoded.verify(a.iat + 1_000),
            "a blob the app minted must verify at the far end's decoder"
        );
        assert!(
            !blob.contains('='),
            "unpadded, or the strict decoder (and the TypeScript one) refuses our own blob"
        );
    }

    #[test]
    fn the_ttl_matches_the_control_planes() {
        // 365 days, as `cloud/packages/shared/src/attestation.ts` writes it.
        // Pinned as arithmetic on both sides; a drift here mints vouchers the
        // Worker refuses with `bad_request: exp`.
        assert_eq!(ATTESTATION_TTL_MS, 31_536_000_000, "365 * 24 * 60 * 60 * 1000");
    }

    #[test]
    fn a_malformed_blob_is_refused_not_guessed_at() {
        let me = approver();
        let a = attestation(me.client_id());
        let message = attestation_message(&a).expect("fits");
        let sig = sign_attestation(&me, &a).expect("fits");
        let good = format!("{}.{}", base64url(&message), base64url(&sig.to_bytes()));
        assert!(decode_attestation(&good).is_some(), "the well-formed blob decodes");

        assert!(decode_attestation("").is_none(), "empty");
        assert!(decode_attestation(&"a".repeat(MAX_ATTESTATION_CHARS + 1)).is_none(), "over the length bound");
        assert!(decode_attestation(&good.replace('.', "")).is_none(), "no separator");
        assert!(decode_attestation(&format!("{good}.")).is_none(), "a second separator");
        assert!(
            decode_attestation(&format!("{good}=")).is_none(),
            "padding is refused: two spellings of one blob would be two identities \
             for the same statement"
        );
        assert!(
            decode_attestation(&format!("{}.{}", base64url(&message), base64url(&sig.to_bytes()[..63]))).is_none(),
            "a 63-byte signature is an error, never a pad"
        );

        // Trailing bytes inside the message: still inside what the signature
        // covers, so ignoring them would surface later as a nameless mismatch.
        let mut trailing = message.clone();
        trailing.push(0);
        assert!(
            decode_attestation(&format!("{}.{}", base64url(&trailing), base64url(&sig.to_bytes()))).is_none(),
            "trailing bytes are refused at the parse, by name"
        );

        let mut truncated = message.clone();
        truncated.truncate(message.len() - 1);
        assert!(
            decode_attestation(&format!("{}.{}", base64url(&truncated), base64url(&sig.to_bytes()))).is_none(),
            "a short message is refused"
        );

        // A version this build does not speak, and a foreign domain.
        let mut v2 = message.clone();
        v2[ATTEST_DOMAIN.len() + 1] = 2;
        assert!(
            decode_attestation(&format!("{}.{}", base64url(&v2), base64url(&sig.to_bytes()))).is_none(),
            "an unknown layout version is refused unread"
        );
        let mut wrong_domain = message.clone();
        wrong_domain[0] ^= 1;
        assert!(
            decode_attestation(&format!("{}.{}", base64url(&wrong_domain), base64url(&sig.to_bytes()))).is_none(),
            "a foreign domain is not this message"
        );

        // Invalid UTF-8 where the label's bytes go. The label in `a` is ASCII
        // "andy-phone", so flipping its first byte to 0xff makes the string
        // fail to decode rather than merely change.
        let label_at = ATTEST_DOMAIN.len() + 2 + 2 + a.account.len() + 32 + 2;
        let mut bad_utf8 = message.clone();
        bad_utf8[label_at] = 0xff;
        assert!(
            decode_attestation(&format!("{}.{}", base64url(&bad_utf8), base64url(&sig.to_bytes()))).is_none(),
            "invalid UTF-8 is a refusal, not U+FFFD: it re-encodes to different \
             bytes than were signed"
        );
    }

    #[test]
    fn non_canonical_base64url_is_refused() {
        // A final partial sextet carries unused low bits, and they must be
        // zero: "-a" (trailing 1010) and "-Q" (trailing 0000) would otherwise
        // both decode to 0xf9, giving one blob two spellings -- two identities
        // wherever blobs are compared or deduplicated, which is the same
        // problem the padding refusal exists to prevent.
        assert_eq!(base64url_decode("-Q"), Some(vec![0xf9]), "the canonical spelling decodes");
        assert_eq!(base64url_decode("-a"), None, "non-zero trailing bits are refused");
        assert_eq!(
            base64url_decode("abc"),
            Some(vec![0x69, 0xb7]),
            "a canonical three-char tail decodes to two bytes"
        );
        assert_eq!(
            base64url_decode("abd"),
            None,
            "the same tail with its two spare bits set is another spelling of \
             the same bytes, and is refused"
        );
    }

    /// The production encoder — no longer TypeScript-only, since the app
    /// mints blobs too (`encode_attestation`).
    use super::base64url_encode as base64url;

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
