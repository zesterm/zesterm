//! Joining this machine to an account.
//!
//! One person, one code, carried by hand from a signed-in browser to the
//! machine they are sitting at — and a signature over that code proving the
//! far end really holds the key it claims. The bytes being signed, and why
//! both halves are needed, are in [`zest_mesh::enroll`]; this module is the
//! daemon's side of it: sign, post, keep the token.
//!
//! # Why this is a foreground flag and not something the daemon does
//!
//! The daemon is detached. It has no terminal to print a code to and nobody to
//! read one to it, and the two places it might get one from — a config file, an
//! environment variable — are both places a one-shot credential should never
//! be written down. So `zest-daemon --enroll <code>` runs in the foreground,
//! does exactly this, and exits, alongside `--trust`, `--forget` and
//! `--trusted`. The running daemon picks the token up from the credential store
//! the next time it starts.
//!
//! # The transport is deliberately missing
//!
//! **This module cannot make the HTTP request yet, and says so at runtime
//! rather than pretending.** The workspace has no HTTP client and no TLS stack,
//! and adding `reqwest` or `rustls` here would settle by accident a decision
//! that belongs to a future `zest-cloud` crate — which of the two TLS backends,
//! whether an async runtime enters the tree at all, and what else in the
//! workspace inherits them. That is a boundary decision, and it lands with the
//! relay that needs the same transport.
//!
//! What is worth having *now* is everything either side of the wire: the
//! signature, the exact JSON, what counts as a refusal, and where the token
//! goes. Those are the parts that are wrong in ways nobody notices, so they are
//! written and tested here against an injected [`ControlPlane`], and
//! [`NoHttpClient`] fills the hole with an error that names the missing
//! dependency. Swapping in a real client is one impl of one method.

use zest_mesh::enroll::enrollment_request;
use zest_mesh::identity::{HostIdentity, Purpose};
use zest_mesh::keystore::{SecretStore, CLOUD_TOKEN_NAME};
use zest_mesh::MeshError;
use zest_proto::auth::Sig64;
use zest_proto::HostId;

/// Where the accounts API lives.
///
/// Matches `APP_ORIGIN` in `cloud/packages/web/wrangler.jsonc`; the two are the
/// same deployment seen from either end, and a machine posting an enrolment to
/// a different origin than the browser minted the code on gets a 404 that reads
/// like an expired code.
pub const DEFAULT_CONTROL_PLANE: &str = "https://zesterm.sigx.workers.dev";

/// The one route this module knows.
/// Where a claim is posted.
///
/// `/api/enroll/claim`, not `/api/enroll` — the control plane also serves
/// `/api/enroll/code`, which is the *minting* half a browser calls while signed
/// in. Posting to the parent path 404s, and a 404 here reads as "that code was
/// not accepted", so the symptom is every valid code looking expired.
pub const ENROLL_PATH: &str = "/api/enroll/claim";

/// What came back from the control plane.
///
/// The status is carried separately rather than folded into an error by the
/// transport, because "was this refused" is a question about enrolment and not
/// about HTTP: a 409 on a spent code and a 403 on a wrong signature are two
/// different things to tell a person, and a transport that collapsed them into
/// `Err(String)` would make that distinction unrecoverable.
#[derive(Debug, Clone)]
pub struct Response {
    pub status: u16,
    pub body: String,
}

/// The single network call enrolment makes.
///
/// A trait rather than a function taking a URL, so that the daemon's binary can
/// be given a real client, a fake, or [`NoHttpClient`] without any of the code
/// around it changing. See the module docs for why the real one does not exist
/// yet.
pub trait ControlPlane {
    /// POST `body` to `url` as `application/json` and return what came back.
    ///
    /// An `Err` here means the request did not complete — no DNS, no route, a
    /// TLS failure. A response that completed and said no is `Ok` with a status
    /// in it, because that is a refusal and not a transport failure.
    fn post_json(&self, url: &str, body: &str) -> Result<Response, EnrollError>;
}

/// The placeholder that stands where the HTTP client will be.
///
/// **TODO: implement this against the workspace's HTTP client once one
/// exists.** It does not exist on purpose — see the module docs — and this
/// fails loudly rather than silently, because the failure mode of a stub that
/// returns `Ok` is a machine that reports itself enrolled and holds no token.
#[derive(Debug, Clone, Copy)]
pub struct NoHttpClient;

impl ControlPlane for NoHttpClient {
    fn post_json(&self, url: &str, _body: &str) -> Result<Response, EnrollError> {
        Err(EnrollError::Transport(format!(
            "this build cannot reach {url}: zesterm has no HTTP client or TLS stack yet. \
             The signing and the token store are in place; the transport lands with the \
             relay, in a zest-cloud crate that owns the choice of TLS backend"
        )))
    }
}

#[derive(Debug, thiserror::Error)]
pub enum EnrollError {
    /// The request never completed.
    #[error("could not reach the control plane: {0}")]
    Transport(String),
    /// It completed, and the answer was no.
    #[error("the control plane refused this enrolment ({status}): {message}")]
    Refused { status: u16, message: String },
    /// It completed, said yes, and said something this cannot act on.
    #[error("the control plane's answer was not an enrolment: {0}")]
    BadResponse(String),
    /// The token could not be kept, or the store could not be consulted.
    #[error("{0}")]
    Store(#[from] MeshError),
    /// A code or label too long to encode unambiguously in the preimage.
    ///
    /// Refused rather than truncated: two labels sharing their first 65535
    /// bytes would otherwise sign identical bytes.
    #[error("{0}")]
    Preimage(#[from] zest_mesh::enroll::EnrollError),
}

/// What this machine got out of enrolling.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Enrolled {
    /// Who it now belongs to, as the control plane names them — a login or an
    /// email. Optional because the account's display name is the control
    /// plane's to change, and enrolment must not fail over a missing one.
    pub account: Option<String>,
}

/// The exact JSON body a machine posts to claim a code.
///
/// Separate from [`enroll`] so the encoding can be tested against
/// [`zest_mesh::enroll::verify_enrollment`] — the check the Worker runs — with
/// no transport and no store in the way. `HostId` and `Sig64` serialize as hex
/// through `zest-proto`, which is what keeps this field-compatible with every
/// other place an id or a signature crosses a wire.
pub fn signed_body(
    identity: &HostIdentity,
    code: &str,
    label: &str,
) -> Result<String, EnrollError> {
    let host = identity.host_id();
    // Fallible since the preimage refuses a field it cannot encode rather than
    // truncating it -- see zest-mesh's enroll module. Refusing here is the only
    // honest option: there is nothing to sign.
    let message = enrollment_request(code, host, label).map_err(EnrollError::Preimage)?;
    let sig = identity.sign(Purpose::Enrollment, &message);

    #[derive(serde::Serialize)]
    struct Body<'a> {
        code: &'a str,
        // camelCase to match the TypeScript on the other end, which is the only
        // consumer. Spelled here rather than with a rename_all so that a second
        // field cannot inherit a convention nobody chose for it.
        #[serde(rename = "hostId")]
        host_id: HostId,
        label: &'a str,
        sig: Sig64,
    }

    Ok(serde_json::to_string(&Body {
        code,
        host_id: host,
        label,
        sig: Sig64(sig.to_bytes()),
    })
    // Infallible in practice: every field is a string or a fixed-width byte
    // array, and none of them can fail to serialize.
    .unwrap_or_else(|e| unreachable!("an enrolment body cannot fail to serialize: {e}")))
}

/// Sign the code, post it, and keep whatever token comes back.
///
/// The order inside is load-bearing and is not the obvious one: the credential
/// store is consulted **before** the request goes out. An enrolment code is
/// one-shot, so a locked keychain discovered after the POST means the code is
/// spent, the account has a host row, and this machine has no token — a state
/// only a person with access to the devices screen can undo. Reading the store
/// first turns that into a refusal that costs nothing.
pub fn enroll(
    identity: &HostIdentity,
    code: &str,
    label: &str,
    base_url: &str,
    http: &dyn ControlPlane,
    secrets: &dyn SecretStore,
) -> Result<Enrolled, EnrollError> {
    // The value is deliberately dropped: this is a probe of the store, not a
    // check for an existing token. Re-enrolling a machine that is already
    // enrolled is a thing people do — after revoking a device, or moving it to
    // another account — and refusing it here would send them to the keychain
    // with a screwdriver.
    let _ = secrets.load_secret(CLOUD_TOKEN_NAME)?;

    let url = format!("{}{ENROLL_PATH}", base_url.trim_end_matches('/'));
    let response = http.post_json(&url, &signed_body(identity, code, label)?)?;

    if !(200..300).contains(&response.status) {
        return Err(EnrollError::Refused {
            status: response.status,
            message: message_from(&response.body),
        });
    }

    #[derive(serde::Deserialize)]
    struct Answer {
        token: Option<String>,
        account: Option<String>,
        // A 200 carrying an error is not hypothetical — it is how GitHub's
        // OAuth endpoint reports a bad code, and `cloud/`'s own tests exercise
        // it. Read here so that shape is a refusal rather than a missing token.
        error: Option<String>,
    }

    let answer: Answer = serde_json::from_str(&response.body)
        .map_err(|e| EnrollError::BadResponse(format!("{e}; body was {:?}", clip(&response.body))))?;

    if let Some(error) = answer.error {
        return Err(EnrollError::Refused { status: response.status, message: error });
    }

    let token = answer.token.filter(|t| !t.trim().is_empty()).ok_or_else(|| {
        EnrollError::BadResponse(format!("no token in {:?}", clip(&response.body)))
    })?;

    secrets.store_secret(CLOUD_TOKEN_NAME, token.as_bytes())?;
    Ok(Enrolled { account: answer.account })
}

/// This machine's bearer token, if it has one.
///
/// `Ok(None)` is "never enrolled"; an `Err` is a store that could not be read.
/// Keeping those apart is [`SecretStore`]'s whole reason for returning an
/// `Option` inside a `Result`.
pub fn stored_token(secrets: &dyn SecretStore) -> Result<Option<String>, EnrollError> {
    let Some(bytes) = secrets.load_secret(CLOUD_TOKEN_NAME)? else {
        return Ok(None);
    };
    // A token is an opaque ASCII string on this wire, so bytes that are not
    // UTF-8 are something else entirely — a key filed under the wrong name, a
    // half-written entry — and guessing at them would put a corrupt
    // `Authorization` header on every request.
    String::from_utf8(bytes.to_vec())
        .map(Some)
        .map_err(|_| EnrollError::BadResponse(format!("{CLOUD_TOKEN_NAME} is not text")))
}

/// Forget the token. Returns whether there was one to forget.
///
/// Local only, and honestly so: this drops the machine's copy, and the account
/// still lists the host until it is revoked from the devices screen. Saying
/// otherwise would be the more comfortable lie.
pub fn forget_token(secrets: &dyn SecretStore) -> Result<bool, EnrollError> {
    let had = secrets.load_secret(CLOUD_TOKEN_NAME)?.is_some();
    secrets.delete_secret(CLOUD_TOKEN_NAME)?;
    Ok(had)
}

/// The most useful sentence in a refusal body.
///
/// `cloud/`'s Worker answers `{"error":"forbidden"}`, but a 502 from in front
/// of it is an HTML page, so this falls back to the raw body rather than
/// reporting nothing when the JSON does not parse — "the control plane refused
/// this" with no reason is the least actionable error there is.
fn message_from(body: &str) -> String {
    #[derive(serde::Deserialize)]
    struct Refusal {
        error: Option<String>,
    }
    serde_json::from_str::<Refusal>(body)
        .ok()
        .and_then(|r| r.error)
        .unwrap_or_else(|| clip(body))
}

/// Enough of a response to diagnose it, and not a whole error page.
fn clip(body: &str) -> String {
    const LIMIT: usize = 200;
    let trimmed = body.trim();
    match trimmed.char_indices().nth(LIMIT) {
        Some((end, _)) => format!("{}…", &trimmed[..end]),
        None => trimmed.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use zest_mesh::enroll::verify_enrollment;
    use zest_mesh::identity::Signature;
    use zest_mesh::keystore::{KeyStore, MemoryKeyStore, Zeroizing, SECRET_LEN};

    fn identity() -> HostIdentity {
        HostIdentity::load_or_create(&MemoryKeyStore::new()).expect("memory store cannot fail")
    }

    /// Records what it was asked, and answers what it was told to.
    struct FakeControlPlane {
        answer: Result<Response, String>,
        calls: RefCell<Vec<(String, String)>>,
    }

    impl FakeControlPlane {
        fn answering(status: u16, body: &str) -> Self {
            Self {
                answer: Ok(Response { status, body: body.to_string() }),
                calls: RefCell::new(Vec::new()),
            }
        }

        fn unreachable() -> Self {
            Self { answer: Err("no route to host".into()), calls: RefCell::new(Vec::new()) }
        }
    }

    impl ControlPlane for FakeControlPlane {
        fn post_json(&self, url: &str, body: &str) -> Result<Response, EnrollError> {
            self.calls.borrow_mut().push((url.to_string(), body.to_string()));
            self.answer.clone().map_err(EnrollError::Transport)
        }
    }

    fn token_in(store: &MemoryKeyStore) -> Option<String> {
        stored_token(store).expect("a memory store is always readable")
    }

    /// A store that is present and unreadable — a locked keychain, or a Linux
    /// session with no bus. `zest-mesh` has one of these for its own tests; it
    /// is `pub(crate)` there, and the behaviour under test is this crate's.
    struct LockedStore;

    impl SecretStore for LockedStore {
        fn load_secret(&self, _name: &str) -> Result<Option<Zeroizing<Vec<u8>>>, MeshError> {
            Err(MeshError::Identity("the keychain is locked".into()))
        }
        fn store_secret(&self, _name: &str, _secret: &[u8]) -> Result<(), MeshError> {
            Err(MeshError::Identity("the keychain is locked".into()))
        }
        fn delete_secret(&self, _name: &str) -> Result<(), MeshError> {
            Err(MeshError::Identity("the keychain is locked".into()))
        }
        fn describe_secret_store(&self) -> String {
            "a store that cannot be read".into()
        }
    }

    #[test]
    fn the_body_a_machine_posts_verifies_at_the_far_end() {
        // The whole seam in one test: whatever hex, field names and byte order
        // this produces, the check the Worker runs must accept it. Both ends
        // are separate implementations, so agreement here is the only thing
        // that makes the daemon's signature worth anything.
        let host = identity();
        let body = signed_body(&host, "ABCD1234", "andy-mac").expect("fits");
        let parsed: serde_json::Value = serde_json::from_str(&body).expect("valid JSON");

        let sig_hex = parsed["sig"].as_str().expect("a sig field");
        let sig_bytes: Vec<u8> = (0..sig_hex.len() / 2)
            .map(|i| u8::from_str_radix(&sig_hex[i * 2..i * 2 + 2], 16).expect("hex"))
            .collect();
        let sig = Signature::from_slice(&sig_bytes).expect("64 bytes");

        assert!(
            verify_enrollment("ABCD1234", host.host_id(), "andy-mac", &sig),
            "the signature this posts must verify against the code, id and label it posts"
        );
        assert_eq!(parsed["code"], "ABCD1234");
        assert_eq!(parsed["label"], "andy-mac");
        assert_eq!(
            parsed["hostId"].as_str(),
            Some(hex(&host.host_id().0).as_str()),
            "the id is 64 lowercase hex, the spelling `hosts.id` is declared with and \
             every other message on this wire uses"
        );
    }

    fn hex(bytes: &[u8]) -> String {
        bytes.iter().map(|b| format!("{b:02x}")).collect()
    }

    #[test]
    fn the_path_is_the_one_the_control_plane_serves() {
        // Pinned as a literal because the two ends are separate projects: this
        // is Rust, the route is TypeScript in `cloud/`, and nothing compiles
        // both. The first version of this constant was `/api/enroll`, which is
        // the parent of the two real routes -- so every claim 404'd, and a 404
        // here reads as "that code was not accepted", making valid codes look
        // expired.
        assert_eq!(
            ENROLL_PATH, "/api/enroll/claim",
            "must match the Worker's route in cloud/packages/web/src/router.ts"
        );
    }

    #[test]
    fn the_signature_covers_the_code_and_label_in_the_right_order() {
        // The previous version of this test read `parsed["code"]` and
        // `parsed["label"]` back out of the JSON -- fields written straight
        // from the same parameters -- and claimed to guard the argument order
        // of `enrollment_request`. It could not: swapping the arguments inside
        // `signed_body` leaves the JSON identical and only corrupts the bytes
        // that were signed.
        //
        // Verifying the signature against the *correctly ordered* preimage is
        // what actually catches it, because that is the thing a wrong order
        // changes.
        let host = identity();
        let body = signed_body(&host, "code-here", "label-here").expect("fits");
        let parsed: serde_json::Value = serde_json::from_str(&body).expect("valid JSON");

        let sig_hex = parsed["sig"].as_str().expect("sig is a hex string");
        let sig_bytes: Vec<u8> = (0..sig_hex.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&sig_hex[i..i + 2], 16).expect("hex"))
            .collect();
        let sig = Signature::from_bytes(
            <[u8; 64]>::try_from(sig_bytes.as_slice()).expect("64 bytes"),
        );

        assert!(
            verify_enrollment("code-here", host.host_id(), "label-here", &sig),
            "the signature must cover the code and the label in the order the \
             control plane will verify them"
        );
        assert!(
            !verify_enrollment("label-here", host.host_id(), "code-here", &sig),
            "and swapping them must not also verify, or the guard proves nothing"
        );
    }

    #[test]
    fn a_token_that_comes_back_is_what_the_store_holds() {
        let store = MemoryKeyStore::new();
        let http = FakeControlPlane::answering(200, r#"{"token":"zt_abc","account":"andy"}"#);

        let enrolled = enroll(&identity(), "ABCD1234", "andy-mac", "https://example", &http, &store)
            .expect("a 200 with a token is an enrolment");

        assert_eq!(enrolled.account.as_deref(), Some("andy"));
        assert_eq!(
            token_in(&store).as_deref(),
            Some("zt_abc"),
            "the token is the entire point of enrolling; a machine that does not keep it \
             has to enrol again with a code that is already spent"
        );
        assert_eq!(
            http.calls.borrow()[0].0,
            format!("https://example{ENROLL_PATH}"),
            "the route is part of the contract with the Worker"
        );
    }

    #[test]
    fn one_trailing_slash_does_not_become_two() {
        let store = MemoryKeyStore::new();
        let http = FakeControlPlane::answering(200, r#"{"token":"zt_abc"}"#);
        enroll(&identity(), "C", "l", "https://example/", &http, &store).expect("enrols");
        assert_eq!(
            http.calls.borrow()[0].0,
            format!("https://example{ENROLL_PATH}"),
            "a `//` in the path is a 404 that reads exactly like an expired code"
        );
    }

    #[test]
    fn a_refusal_stores_nothing() {
        // The guard: a machine that keeps a token from a refused enrolment
        // sends an `Authorization` header that is never accepted, and every
        // later failure looks like an outage rather than a machine that was
        // never enrolled.
        let store = MemoryKeyStore::new();
        let http = FakeControlPlane::answering(403, r#"{"error":"that code has been used"}"#);

        let err = enroll(&identity(), "ABCD1234", "andy-mac", "https://example", &http, &store)
            .expect_err("a 403 is not an enrolment");

        assert!(
            matches!(err, EnrollError::Refused { status: 403, .. }),
            "the status has to survive: a spent code and a bad signature are different \
             things to tell a person; got {err}"
        );
        assert!(
            err.to_string().contains("that code has been used"),
            "the control plane's own words are the only actionable part; got {err}"
        );
        assert!(token_in(&store).is_none(), "nothing was granted, so nothing may be kept");
    }

    #[test]
    fn a_two_hundred_carrying_an_error_is_a_refusal_and_not_a_token() {
        // Not hypothetical: this is how GitHub's OAuth endpoint reports a bad
        // code, and `cloud/` already tests against that shape.
        let store = MemoryKeyStore::new();
        let http = FakeControlPlane::answering(200, r#"{"error":"bad_verification_code"}"#);

        let err = enroll(&identity(), "ABCD1234", "andy-mac", "https://example", &http, &store)
            .expect_err("a body that says error is not an enrolment");

        assert!(
            matches!(err, EnrollError::Refused { .. }),
            "an error in the body is a refusal, not a malformed answer — the difference \
             is whether a person is told to try again or told why; got {err}"
        );
        assert!(err.to_string().contains("bad_verification_code"), "got {err}");
        assert!(token_in(&store).is_none(), "a 200 that granted nothing may leave nothing behind");
    }

    #[test]
    fn a_success_with_no_token_is_reported_rather_than_stored_empty() {
        let store = MemoryKeyStore::new();
        let http = FakeControlPlane::answering(200, r#"{"account":"andy"}"#);
        let err = enroll(&identity(), "C", "l", "https://example", &http, &store)
            .expect_err("there is nothing to keep");
        assert!(matches!(err, EnrollError::BadResponse(_)), "got {err}");
        assert!(
            token_in(&store).is_none(),
            "an empty token stored is a machine that reports itself enrolled and is not"
        );
    }

    #[test]
    fn an_unreachable_control_plane_is_a_transport_failure_and_not_a_refusal() {
        // They demand opposite responses: retry the first, and stop and read
        // the second. Collapsing them is how "your wifi dropped" comes to read
        // as "your code was rejected".
        let store = MemoryKeyStore::new();
        let http = FakeControlPlane::unreachable();
        let err = enroll(&identity(), "C", "l", "https://example", &http, &store)
            .expect_err("no route is not an enrolment");
        assert!(matches!(err, EnrollError::Transport(_)), "got {err}");
        assert!(token_in(&store).is_none());
    }

    #[test]
    fn the_placeholder_transport_fails_and_names_what_is_missing() {
        // The stub must be impossible to mistake for a working client. An
        // `Ok(())` here would produce a machine that says it enrolled, holds no
        // token, and blames the relay months later.
        let store = MemoryKeyStore::new();
        let err = enroll(&identity(), "C", "l", DEFAULT_CONTROL_PLANE, &NoHttpClient, &store)
            .expect_err("there is no HTTP client in this workspace");
        assert!(
            err.to_string().contains("HTTP client"),
            "the error has to say what is missing, or it reads as an outage; got {err}"
        );
        assert!(token_in(&store).is_none());
    }

    #[test]
    fn a_store_that_cannot_be_read_is_refused_before_the_code_is_spent() {
        // The reason the store is probed first. An enrolment code is one-shot:
        // discovering a locked keychain *after* the POST leaves the account
        // holding a host row and this machine holding nothing, and no amount of
        // retrying fixes it because the code is gone.
        let store = LockedStore;
        let http = FakeControlPlane::answering(200, r#"{"token":"zt_abc"}"#);

        let err = enroll(&identity(), "ABCD1234", "andy-mac", "https://example", &http, &store)
            .expect_err("an unusable store must stop this");

        assert!(matches!(err, EnrollError::Store(_)), "got {err}");
        assert!(
            http.calls.borrow().is_empty(),
            "the code must not be spent on an enrolment whose token cannot be kept"
        );
    }

    #[test]
    fn forgetting_a_token_says_whether_there_was_one() {
        let store = MemoryKeyStore::new();
        assert!(
            !forget_token(&store).expect("logging out of nothing is not an error"),
            "a machine that was never enrolled has nothing to forget, and saying \
             'signed out' would be a lie about what just happened"
        );

        store.store_secret(CLOUD_TOKEN_NAME, b"zt_abc").expect("store");
        assert!(forget_token(&store).expect("forget"), "there was a token");
        assert!(token_in(&store).is_none(), "and now there is not");
    }

    #[test]
    fn a_token_that_is_not_text_is_an_error_rather_than_a_guess() {
        // Reachable by a name collision, since keys and tokens share one
        // namespace: 32 bytes of key read back as a token is not text. Guessing
        // would put a mangled `Authorization` header on every request and
        // produce a 401 that names nothing.
        let store = MemoryKeyStore::new();
        store.store(CLOUD_TOKEN_NAME, &[0xffu8; SECRET_LEN]).expect("store");
        assert!(
            stored_token(&store).is_err(),
            "bytes that are not a token must not be lossily converted into one"
        );
    }
}
