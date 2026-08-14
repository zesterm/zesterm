//! The browser hand-off sign-in: the two messages a device signs to link
//! itself to an account without a typed code (#226).
//!
//! [`crate::enroll`] is a human carrying a code from a browser to a machine.
//! This is the flow going the other way: the app asks for a **grant**, opens
//! the account's browser at `/link?grant=…`, the signed-in person clicks
//! Approve, and the app claims the grant. Two signatures, one per phase:
//!
//! - [`link_request`] — "the holder of this key asks to link, under this
//!   label". Proof of key possession at the start, so the control plane never
//!   parks a grant for a key nobody holds.
//! - [`link_claim`] — "the holder of this key spends this grant". The grant id
//!   alone is deliberately not enough to claim: it travels through a browser
//!   URL, and a leaked id must be worth nothing without the key.
//!
//! Both are signed as [`Role::Client`] + [`Purpose::Enrollment`] — no new
//! purpose, the register-preimage argument restated: the outer domains do the
//! separating, because `zesterm-link-v1` and `zesterm-enroll-v1` diverge at
//! byte 8 (`l` vs `e`), before any caller-supplied field, so no message under
//! one domain can be bytes under the other.
//!
//! **The two link messages carry a tag byte, and it is load-bearing.** They
//! share one domain, and each is `key32` plus one length-prefixed
//! caller-influenced string — in opposite orders. Order alone makes them only
//! *probabilistically* disjoint (a key whose first bytes happen to read as a
//! plausible length prefix overlaps the other layout), and a security
//! argument that leans on key bytes being unlucky is one nobody can check by
//! reading. One tag byte after the domain makes the disjointness structural:
//! an approval-phase signature can never be replayed as a claim, and the
//! fixture carries that exact replay as a MUST-fail case.

use crate::identity::{Purpose, Signature, verify_client};
use zest_proto::ClientId;

/// Why a message could not be built.
///
/// Fallible for [`crate::enroll::EnrollError`]'s reason: truncating an
/// oversize field would make two values sharing a 65535-byte prefix produce
/// identical signed bytes, and both strings here reach this code from a
/// caller.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum LinkError {
    #[error("device label is longer than 65535 bytes")]
    LabelTooLong,
    #[error("grant id is longer than 65535 bytes")]
    GrantTooLong,
}

/// The domain both link messages live in. The tag byte distinguishes them.
const LINK_DOMAIN: &[u8] = b"zesterm-link-v1";

/// The tag byte for [`link_request`].
const REQUEST_TAG: u8 = 1;

/// The tag byte for [`link_claim`].
const CLAIM_TAG: u8 = 2;

/// The exact bytes the app signs to ask for a grant.
///
/// `domain ++ 0x01 ++ key ++ u16be(len(label)) ++ label`. Length-prefixed for
/// the reason every multi-field preimage here is: the label is caller-chosen,
/// and unprefixed variable fields make field boundaries movable.
pub fn link_request(key: &[u8; 32], label: &str) -> Result<Vec<u8>, LinkError> {
    let label_len = u16::try_from(label.len()).map_err(|_| LinkError::LabelTooLong)?;

    let mut out = Vec::with_capacity(LINK_DOMAIN.len() + 1 + 32 + 2 + label.len());
    out.extend_from_slice(LINK_DOMAIN);
    out.push(REQUEST_TAG);
    out.extend_from_slice(key);
    out.extend_from_slice(&label_len.to_be_bytes());
    out.extend_from_slice(label.as_bytes());
    Ok(out)
}

/// The exact bytes the app signs to spend a grant.
///
/// `domain ++ 0x02 ++ u16be(len(grant)) ++ grant ++ key`. Binding the key in
/// is the point: the grant id rode through a browser URL, and this signature
/// is what makes a captured one worthless — the control plane only enrols the
/// key that both asked and claimed.
pub fn link_claim(grant: &str, key: &[u8; 32]) -> Result<Vec<u8>, LinkError> {
    let grant_len = u16::try_from(grant.len()).map_err(|_| LinkError::GrantTooLong)?;

    let mut out = Vec::with_capacity(LINK_DOMAIN.len() + 1 + 2 + grant.len() + 32);
    out.extend_from_slice(LINK_DOMAIN);
    out.push(CLAIM_TAG);
    out.extend_from_slice(&grant_len.to_be_bytes());
    out.extend_from_slice(grant.as_bytes());
    out.extend_from_slice(key);
    Ok(out)
}

/// Did the holder of `client` sign this exact link request?
///
/// Answers that and nothing else — whether a grant should be minted for it is
/// the control plane's business, kept out of here where a missing check would
/// look like a passing one. Same line [`crate::enroll::verify_enrollment`]
/// draws.
#[must_use]
pub fn verify_link_request(client: ClientId, label: &str, sig: &Signature) -> bool {
    let Ok(message) = link_request(&client.0, label) else { return false };
    verify_client(client, Purpose::Enrollment, &message, sig).is_ok()
}

/// Did the holder of `client` sign this exact claim of this exact grant?
#[must_use]
pub fn verify_link_claim(grant: &str, client: ClientId, sig: &Signature) -> bool {
    let Ok(message) = link_claim(grant, &client.0) else { return false };
    verify_client(client, Purpose::Enrollment, &message, sig).is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::identity::ClientIdentity;
    use crate::keystore::SECRET_LEN;

    fn device() -> ClientIdentity {
        ClientIdentity::from_secret_bytes(&[0x44; SECRET_LEN])
    }

    #[test]
    fn the_holder_of_the_key_is_believed_in_both_phases() {
        let me = device();
        let key = me.client_id();

        let request = link_request(&key.0, "andy-desktop").expect("fits");
        let sig = me.sign(Purpose::Enrollment, &request);
        assert!(
            verify_link_request(key, "andy-desktop", &sig),
            "the holder of the key must be able to ask for a grant"
        );

        let claim = link_claim("grant-id", &key.0).expect("fits");
        let sig = me.sign(Purpose::Enrollment, &claim);
        assert!(
            verify_link_claim("grant-id", key, &sig),
            "and to spend one it was granted"
        );
    }

    #[test]
    fn a_request_signature_is_not_a_claim_signature() {
        // The tag byte's whole job. Both messages share a domain and carry the
        // same key plus one variable string, so without the tag their
        // disjointness would rest on key bytes never resembling a length
        // prefix -- an argument nobody can check by reading. The fixture
        // carries this exact replay so the TypeScript side proves it too.
        let me = device();
        let key = me.client_id();
        let request = link_request(&key.0, "andy-desktop").expect("fits");
        let sig = me.sign(Purpose::Enrollment, &request);
        assert!(
            !verify_link_claim("andy-desktop", key, &sig),
            "an approval-phase signature must not spend any grant"
        );
    }

    #[test]
    fn changing_any_signed_field_invalidates_it() {
        let me = device();
        let key = me.client_id();
        let sig = me.sign(Purpose::Enrollment, &link_claim("grant-id", &key.0).expect("fits"));

        assert!(!verify_link_claim("other-id", key, &sig), "a different grant");
        assert!(
            !verify_link_claim("grant-id", ClientIdentity::from_secret_bytes(&[0x55; SECRET_LEN]).client_id(), &sig),
            "a different key -- the signature must not transfer"
        );
    }

    #[test]
    fn a_link_signature_is_not_an_auth_signature() {
        // Purpose is inside the signing prefix; a signature harvested from a
        // login handshake must not mint or spend grants.
        let me = device();
        let key = me.client_id();
        let message = link_request(&key.0, "andy-desktop").expect("fits");
        let auth_sig = me.sign(Purpose::Auth, &message);
        assert!(!verify_link_request(key, "andy-desktop", &auth_sig));
    }

    #[test]
    fn an_unencodable_field_is_refused_rather_than_truncated() {
        let key = [0x44u8; 32];
        let huge = "x".repeat(usize::from(u16::MAX) + 1);

        assert_eq!(link_request(&key, &huge), Err(LinkError::LabelTooLong));
        assert_eq!(link_claim(&huge, &key), Err(LinkError::GrantTooLong));

        // Exactly at the boundary still encodes, so the limit is the
        // encoding's and not an arbitrary one.
        let max = "x".repeat(u16::MAX.into());
        assert!(link_request(&key, &max).is_ok());
    }

    #[test]
    fn the_golden_fixture_verifies_against_this_implementation() {
        // `fixtures/link.json` is what the TypeScript port is written against,
        // and this loading it back is what keeps the file from drifting from
        // the code between `cargo xtask fixtures` runs.
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../zest-proto/fixtures/link.json");
        let json: serde_json::Value = serde_json::from_str(
            &std::fs::read_to_string(&path).expect("link.json exists -- run `cargo xtask fixtures`"),
        )
        .expect("link.json parses");

        let cases = json["cases"].as_array().expect("cases");
        assert!(cases.len() >= 4, "the fixture pins at least four cases");

        let mut saw_a_failure = false;
        for case in cases {
            let name = case["name"].as_str().expect("name");
            let key = ClientId::from_bytes(hex32(case["key"].as_str().expect("key")));
            let kind = case["verify_as"].as_str().expect("verify_as");

            let message = match kind {
                "request" => link_request(&key.0, case["label"].as_str().expect("label")),
                "claim" => link_claim(case["grant"].as_str().expect("grant"), &key.0),
                other => panic!("{name}: unknown verify_as {other}"),
            }
            .expect("fixture fields encode");
            assert_eq!(
                hex(&message),
                case["message"].as_str().expect("message"),
                "{name}: the canonical message bytes moved -- a TS implementation \
                 built against the file would now disagree with the Rust"
            );

            let sig = Signature::from_slice(&hex_bytes(case["signature"].as_str().expect("signature")))
                .expect("64 bytes");
            let verified = match kind {
                "request" => verify_link_request(key, case["label"].as_str().expect("label"), &sig),
                _ => verify_link_claim(case["grant"].as_str().expect("grant"), key, &sig),
            };
            let expect = case["expect_verify"].as_bool().expect("expect_verify");
            saw_a_failure |= !expect;
            assert_eq!(
                verified,
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
