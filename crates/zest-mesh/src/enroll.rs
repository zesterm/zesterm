//! Joining a machine to an account.
//!
//! The account does not get to *assert* which keys are its own — that would be
//! a UUID with extra steps, and ADR-006 is explicit that an id proves nothing.
//! What happens instead is that the holder of the key signs the one-shot code a
//! person just approved in their browser.
//!
//! That single signature binds three separate things:
//!
//! - **this human approved this code** — they were signed in, and they carried
//!   it to the machine themselves;
//! - **this code was answered by the holder of this key** — not by whoever saw
//!   it in a terminal's scrollback;
//! - **this key claims this label** — so the name on the device list cannot be
//!   swapped for another machine's afterwards.
//!
//! Neither half is sufficient alone. A code with no signature enrols whoever
//! intercepted it; a signature over no code enrols a key nobody approved.

use crate::identity::{Purpose, Role, Signature, verify_host};
use zest_proto::HostId;

/// The domain this preimage lives in.
///
/// Separate from [`Purpose::Enrollment`]'s place in the signing prefix rather
/// than relying on it alone: the prefix says *what kind of thing* is being
/// signed, and this says *which message layout* follows. A future enrolment
/// shape gets `v2` here and cannot be confused with this one.
const ENROLL_DOMAIN: &[u8] = b"zesterm-enroll-v1";

/// The exact bytes a machine signs to claim an enrolment code.
///
/// **Length-prefixed, not NUL-separated.** [`crate::identity::preimage`]'s NUL
/// form is sound only because its variable field is last and the others come
/// from closed enums; that reasoning does not survive two caller-supplied
/// strings. Without prefixes, `code="ab", label="cd"` and `code="abc",
/// label="d"` would produce identical bytes — so a signature over one would be
/// a valid signature over the other, and a label is attacker-chosen.
#[must_use]
pub fn enrollment_request(code: &str, host: HostId, label: &str) -> Vec<u8> {
    let mut out = Vec::with_capacity(ENROLL_DOMAIN.len() + 4 + 32 + code.len() + label.len() + 4);
    out.extend_from_slice(ENROLL_DOMAIN);
    push_len_prefixed(&mut out, code.as_bytes());
    out.extend_from_slice(&host.0);
    push_len_prefixed(&mut out, label.as_bytes());
    out
}

fn push_len_prefixed(out: &mut Vec<u8>, bytes: &[u8]) {
    let len = u16::try_from(bytes.len()).unwrap_or(u16::MAX);
    out.extend_from_slice(&len.to_be_bytes());
    out.extend_from_slice(&bytes[..len as usize]);
}

/// Whether this machine really holds the key it is enrolling.
///
/// The verification the control plane runs. It answers only "the holder of
/// `host` signed *these* bytes" — whether the code is fresh, unspent and
/// belongs to the account asking is the caller's business, and deliberately
/// not mixed in here where a missing check would look like a passing one.
#[must_use]
pub fn verify_enrollment(code: &str, host: HostId, label: &str, sig: &Signature) -> bool {
    verify_host(host, Purpose::Enrollment, &enrollment_request(code, host, label), sig).is_ok()
}

/// The role this preimage is signed under, for callers building the signature.
///
/// A host enrols itself, so it signs as [`Role::Host`]. A browser enrolling a
/// client key signs the same layout as [`Role::Client`], and the two cannot be
/// swapped because the role is inside the signing prefix.
pub const ENROLL_ROLE: Role = Role::Host;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::identity::HostIdentity;
    use crate::keystore::MemoryKeyStore;

    fn identity() -> HostIdentity {
        HostIdentity::load_or_create(&MemoryKeyStore::new()).expect("memory store cannot fail")
    }

    #[test]
    fn a_machine_that_holds_the_key_is_accepted() {
        let host = identity();
        let sig = host.sign(Purpose::Enrollment, &enrollment_request("ABCD1234", host.host_id(), "andy-mac"));
        assert!(
            verify_enrollment("ABCD1234", host.host_id(), "andy-mac", &sig),
            "the holder of the key must be able to enrol it"
        );
    }

    #[test]
    fn changing_any_signed_field_invalidates_it() {
        let host = identity();
        let sig = host.sign(Purpose::Enrollment, &enrollment_request("ABCD1234", host.host_id(), "andy-mac"));

        assert!(!verify_enrollment("WRONG123", host.host_id(), "andy-mac", &sig), "a different code");
        assert!(!verify_enrollment("ABCD1234", host.host_id(), "someone-else", &sig), "a different label");
        assert!(
            !verify_enrollment("ABCD1234", identity().host_id(), "andy-mac", &sig),
            "a different key -- the signature must not transfer to another machine"
        );
    }

    #[test]
    fn the_boundary_between_code_and_label_cannot_be_moved() {
        // The reason this preimage is length-prefixed rather than NUL-separated.
        // Concatenated, `("ab", "cd")` and `("abc", "d")` are the same bytes, so
        // a signature over one would verify the other -- and the label is
        // attacker-chosen, so that is a machine enrolling under a code it was
        // never given.
        let host = identity();
        assert_ne!(
            enrollment_request("ab", host.host_id(), "cd"),
            enrollment_request("abc", host.host_id(), "d"),
            "field boundaries must be unambiguous"
        );
    }

    #[test]
    fn an_enrolment_signature_is_not_an_auth_signature() {
        // `Purpose` is inside the signing prefix, so a signature harvested from
        // one flow cannot be replayed into the other. Without this a stolen
        // enrolment signature would be a login.
        let host = identity();
        let message = enrollment_request("ABCD1234", host.host_id(), "andy-mac");
        let auth_sig = host.sign(Purpose::Auth, &message);
        assert!(
            !verify_enrollment("ABCD1234", host.host_id(), "andy-mac", &auth_sig),
            "a signature made for Auth must not verify as Enrollment"
        );
    }

    #[test]
    fn the_preimage_is_stable() {
        // A golden, because both ends of this are separate implementations: the
        // daemon signs it in Rust and the Worker verifies it in TypeScript. A
        // change here that is not deliberate breaks enrolment for every machine
        // already enrolled, and the failure is a signature mismatch that names
        // nothing.
        let host = HostId::from_bytes([0x11; 32]);
        let bytes = enrollment_request("ABCD1234", host, "andy-mac");
        let hex: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
        assert_eq!(
            hex,
            // Verified by decoding rather than by hand: 17-byte domain,
            // u16 length + "ABCD1234", 32 host bytes, u16 length + "andy-mac",
            // nothing trailing. The first version of this literal was
            // miscounted, which is exactly the failure mode a golden written
            // from arithmetic has.
            "7a65737465726d2d656e726f6c6c2d763100084142434431323334\
             1111111111111111111111111111111111111111111111111111111111111111\
             0008616e64792d6d6163",
            "the enrolment preimage changed"
        );
    }
}
