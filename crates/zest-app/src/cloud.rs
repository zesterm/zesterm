//! The app's own sign-in: enrolling this desktop as a *device*, by code or by
//! browser link.
//!
//! Deliberately parallel to `zest_daemon::enroll` rather than a call into it:
//! the daemon enrols a machine (its `HostIdentity`, role `host`, token under
//! `cloud-token`), while the app enrols the key a person carries between
//! machines (its `ClientIdentity`, role `client`, token under
//! `app-cloud-token`). Same wire, different principals — the split the
//! keystore's doc comment on [`APP_CLOUD_TOKEN_NAME`] exists to defend. The
//! transport traits are seams for the same reason as the daemon's: every test
//! below drives a real enrolment without a socket.
//!
//! # What used to be here
//!
//! Everything *after* sign-in — reading the account's fleet, minting a relay
//! ticket, dialling one — now lives in [`zest_daemon::account`] and is
//! re-exported below, so this module's own paths are unchanged. It moved
//! because `zest-app` is a `[[bin]]`-only crate: no second client could reach
//! `fetch_hosts` or the relay ladder, which left the account half of the fleet
//! invisible to `zest-mcp` (#274). What stayed is what has one consumer and
//! always will — a person signing *this window* in.

use zest_daemon::enroll::{ControlPlane, EnrollError, Enrolled, ENROLL_PATH};
use zest_mesh::enroll::enrollment_request_for;
use zest_mesh::identity::{ClientIdentity, Purpose};
use zest_mesh::keystore::{SecretStore, APP_CLOUD_TOKEN_NAME};
use zest_proto::auth::Sig64;
use zest_proto::ClientId;

/// The account client's own home is `zest-daemon` now; these are this crate's
/// local names for it, not a second copy. Re-exported rather than re-pointed
/// at every call site because they are the same items — the module boundary
/// moved, the API did not.
pub use zest_daemon::account::{
    approve_device, fetch_devices, fetch_hosts, fetch_me, forget_app_token, mint_host_code,
    relay_dialer, stored_app_token, CloudError, HttpsAccountApi, MachineRefusal, RelayDialError,
};

/// The exact JSON body the app posts to claim a device code.
///
/// [`zest_daemon::enroll::signed_body`]'s shape, with the two differences the
/// principal split dictates: the signature is the `ClientIdentity`'s (role
/// `client` lives in the signing prefix, and the Worker verifies under the
/// role the *code* was minted for), and `deviceKind` is sent — its values are
/// device kinds, so a device may name itself where a daemon may not.
pub fn signed_client_body(
    identity: &ClientIdentity,
    code: &str,
    label: &str,
) -> Result<String, EnrollError> {
    let client = identity.client_id();
    // The preimage refuses a field it cannot encode rather than truncating —
    // there is nothing honest to sign (see zest-mesh's enroll module).
    let message =
        enrollment_request_for(code, &client.0, label).map_err(EnrollError::Preimage)?;
    let sig = identity.sign(Purpose::Enrollment, &message);

    #[derive(serde::Serialize)]
    struct Body<'a> {
        code: &'a str,
        // `hostId` even though this carries a ClientId: the field names the
        // key being enrolled, whatever its kind — the Worker's comment says
        // why it is not renamed per kind (the machine would have to know
        // which kind the code was minted for before it could format the
        // request). Spelled here rather than with a rename_all, like the
        // daemon's body, so a second field cannot inherit the convention.
        #[serde(rename = "hostId")]
        host_id: ClientId,
        label: &'a str,
        sig: Sig64,
        // Advisory and unsigned, like `platform`: the devices screen renders
        // it, nothing decides on it.
        #[serde(rename = "deviceKind")]
        device_kind: &'a str,
        platform: &'a str,
    }

    Ok(serde_json::to_string(&Body {
        code,
        host_id: client,
        label,
        sig: Sig64(sig.to_bytes()),
        device_kind: "desktop",
        platform: std::env::consts::OS,
    })
    // Infallible in practice: strings and fixed-width byte arrays only.
    .unwrap_or_else(|e| unreachable!("an enrolment body cannot fail to serialize: {e}")))
}

/// Sign the code, post it, and keep the token — under the *app's* name.
///
/// The order is the daemon's, and it is load-bearing there for the same
/// reason here: the credential store is probed **before** the POST, because a
/// code is one-shot and a locked keychain discovered after the claim leaves
/// the account holding a device row and this app holding nothing.
pub fn enroll_desktop(
    identity: &ClientIdentity,
    code: &str,
    label: &str,
    base_url: &str,
    http: &dyn ControlPlane,
    secrets: &dyn SecretStore,
) -> Result<Enrolled, EnrollError> {
    // A probe, not a check for an existing token — re-enrolling after a
    // revoke is a thing people do, and refusing it here would be wrong.
    let _ = secrets.load_secret(APP_CLOUD_TOKEN_NAME)?;

    let url = format!("{}{ENROLL_PATH}", base_url.trim_end_matches('/'));
    let response = http.post_json(&url, &signed_client_body(identity, code, label)?)?;

    if !(200..300).contains(&response.status) {
        let (message, detail) = refusal_from(&response.body);
        return Err(EnrollError::Refused { status: response.status, message, detail });
    }

    #[derive(serde::Deserialize)]
    struct Answer {
        token: Option<String>,
        account: Option<String>,
        // A 200 carrying an error is a refusal, not a missing token — the
        // GitHub-OAuth shape the daemon's enroll documents.
        error: Option<String>,
        detail: Option<String>,
    }

    let answer: Answer = serde_json::from_str(&response.body)
        .map_err(|e| EnrollError::BadResponse(format!("{e}; body was {:?}", clip(&response.body))))?;

    if let Some(error) = answer.error {
        return Err(EnrollError::Refused {
            status: response.status,
            message: error,
            detail: answer.detail,
        });
    }

    let token = answer.token.filter(|t| !t.trim().is_empty()).ok_or_else(|| {
        EnrollError::BadResponse(format!("no token in {:?}", clip(&response.body)))
    })?;

    secrets.store_secret(APP_CLOUD_TOKEN_NAME, token.as_bytes())?;
    Ok(Enrolled { account: answer.account })
}

/// Where a browser hand-off starts and where it is claimed (#226).
///
/// Pinned as literals for `ENROLL_PATH`'s reason: the other end is
/// TypeScript (`cloud/packages/web/src/api/link.ts`, routed in `router.ts`)
/// and nothing compiles both.
const LINK_START_PATH: &str = "/api/link/start";
const LINK_CLAIM_PATH: &str = "/api/link/claim";

/// Where the browser approves — the page [`start_link`]'s grant is carried
/// to. The app opens `<control-plane>/link?grant=<id>`.
pub const LINK_PAGE_PATH: &str = "/link";

/// What `POST /api/link/start` granted: the id the browser page is opened
/// with, and when the whole hand-off dies server-side.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LinkGrant {
    pub grant: String,
    /// Epoch milliseconds; the poller stops here — a grant the server has
    /// reaped answers only the collapsed refusal, which reads worse.
    pub expires_at: u64,
}

/// One `claim_link` poll's answer, as the poller consumes it.
///
/// `Refused` is an `Ok` variant, not an error, because it is terminal where
/// a transport `Err` is transient: the poller keeps polling through a wifi
/// blip and stops on a refusal, and folding the two together would make it
/// do one of those wrongly.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LinkOutcome {
    /// Nobody has clicked Approve yet. Poll again.
    Pending,
    /// Claimed: the token is stored and this app is signed in.
    SignedIn { account: Option<String> },
    /// The grant is dead — denied, expired, superseded or spent — in the
    /// server's one collapsed answer, or the enrolment was refused.
    Refused(String),
}

/// Ask for a link grant, proving this app holds its key.
///
/// The store is probed **before** the request — `enroll_desktop`'s
/// locked-keychain argument, one step earlier still: the grant costs a
/// person a browser approval, and a keychain discovered locked after they
/// clicked Approve wastes exactly that click.
pub fn start_link(
    identity: &ClientIdentity,
    label: &str,
    base_url: &str,
    http: &dyn ControlPlane,
    secrets: &dyn SecretStore,
) -> Result<LinkGrant, EnrollError> {
    let _ = secrets.load_secret(APP_CLOUD_TOKEN_NAME)?;

    let key = identity.client_id();
    let message = zest_mesh::link::link_request(&key.0, label)?;
    let sig = identity.sign(Purpose::Enrollment, &message);

    #[derive(serde::Serialize)]
    struct Body<'a> {
        // camelCase spelled per field, the `signed_client_body` discipline.
        #[serde(rename = "deviceId")]
        device_id: ClientId,
        label: &'a str,
        // Advisory pair, exactly as the code claim sends them: the grant
        // card renders them, nothing decides on them.
        kind: &'a str,
        platform: &'a str,
        sig: Sig64,
    }
    let body = serde_json::to_string(&Body {
        device_id: key,
        label,
        kind: "desktop",
        platform: std::env::consts::OS,
        sig: Sig64(sig.to_bytes()),
    })
    .unwrap_or_else(|e| unreachable!("a link request cannot fail to serialize: {e}"));

    let url = format!("{}{LINK_START_PATH}", base_url.trim_end_matches('/'));
    let response = http.post_json(&url, &body)?;
    if !(200..300).contains(&response.status) {
        let (message, detail) = refusal_from(&response.body);
        return Err(EnrollError::Refused { status: response.status, message, detail });
    }

    #[derive(serde::Deserialize)]
    struct Answer {
        grant: Option<String>,
        #[serde(rename = "expiresAt")]
        expires_at: Option<u64>,
    }
    let answer: Answer = serde_json::from_str(&response.body)
        .map_err(|e| EnrollError::BadResponse(format!("{e}; body was {:?}", clip(&response.body))))?;
    match (answer.grant.filter(|g| !g.is_empty()), answer.expires_at) {
        (Some(grant), Some(expires_at)) => Ok(LinkGrant { grant, expires_at }),
        _ => Err(EnrollError::BadResponse(format!("no grant in {:?}", clip(&response.body)))),
    }
}

/// One poll of the grant: still pending, signed in, or dead.
///
/// On success the token lands under [`APP_CLOUD_TOKEN_NAME`] — the same
/// slot, the same principal, whichever door the sign-in came through.
pub fn claim_link(
    identity: &ClientIdentity,
    grant: &str,
    base_url: &str,
    http: &dyn ControlPlane,
    secrets: &dyn SecretStore,
) -> Result<LinkOutcome, EnrollError> {
    let key = identity.client_id();
    let message = zest_mesh::link::link_claim(grant, &key.0)?;
    let sig = identity.sign(Purpose::Enrollment, &message);

    #[derive(serde::Serialize)]
    struct Body<'a> {
        grant: &'a str,
        #[serde(rename = "deviceId")]
        device_id: ClientId,
        sig: Sig64,
    }
    let body = serde_json::to_string(&Body { grant, device_id: key, sig: Sig64(sig.to_bytes()) })
        .unwrap_or_else(|e| unreachable!("a link claim cannot fail to serialize: {e}"));

    let url = format!("{}{LINK_CLAIM_PATH}", base_url.trim_end_matches('/'));
    let response = http.post_json(&url, &body)?;
    if !(200..300).contains(&response.status) {
        // Terminal, not an error: the collapsed refusal (and the 409) are
        // the grant's end, and the poller must stop rather than retry a
        // request the server will refuse identically forever.
        return Ok(LinkOutcome::Refused(message_from(&response.body)));
    }

    #[derive(serde::Deserialize)]
    struct Answer {
        status: Option<String>,
        token: Option<String>,
        account: Option<String>,
        error: Option<String>,
    }
    let answer: Answer = serde_json::from_str(&response.body)
        .map_err(|e| EnrollError::BadResponse(format!("{e}; body was {:?}", clip(&response.body))))?;

    if let Some(error) = answer.error {
        return Ok(LinkOutcome::Refused(error));
    }
    if answer.status.as_deref() == Some("pending") {
        return Ok(LinkOutcome::Pending);
    }
    let token = answer.token.filter(|t| !t.trim().is_empty()).ok_or_else(|| {
        EnrollError::BadResponse(format!("no token in {:?}", clip(&response.body)))
    })?;
    secrets.store_secret(APP_CLOUD_TOKEN_NAME, token.as_bytes())?;
    Ok(LinkOutcome::SignedIn { account: answer.account })
}

/// The first 8 hex of this app's key, grouped `ab12 cd34`.
///
/// MUST render exactly as the approval page does — `FINGERPRINT_CHARS` in
/// `cloud/packages/web/src/api/link.ts` (first 8 lowercase hex) through
/// `fingerprintGroups` in `clients/web/packages/app/src/link.ts` (a space
/// every 4) — because the whole anti-phishing half is a person comparing
/// the two strings. A different grouping here would make honest pairs look
/// different, which trains people to stop comparing.
#[must_use]
pub fn key_fingerprint(id: ClientId) -> String {
    let full = hex(&id.0[..4]);
    format!("{} {}", &full[..4], &full[4..])
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// The most useful sentence in a refusal body, with the Worker's optional
/// `detail` beside it (#367) — the daemon's `refusal_from`, private there and
/// a few lines here.
fn refusal_from(body: &str) -> (String, Option<String>) {
    #[derive(serde::Deserialize)]
    struct Refusal {
        error: Option<String>,
        detail: Option<String>,
    }
    match serde_json::from_str::<Refusal>(body) {
        Ok(Refusal { error: Some(error), detail }) => (error, detail),
        _ => (clip(body), None),
    }
}

/// The sentence alone, for the flows that keep no detail.
fn message_from(body: &str) -> String {
    refusal_from(body).0
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
    use zest_daemon::enroll::Response;
    use zest_mesh::identity::{verify_client, Signature};
    use zest_mesh::keystore::{MemoryKeyStore, Zeroizing, CLOUD_TOKEN_NAME};
    use zest_mesh::MeshError;

    fn identity() -> ClientIdentity {
        ClientIdentity::load_or_create(&MemoryKeyStore::new()).expect("memory store cannot fail")
    }

    /// Records what it was asked, and answers what it was told to — the
    /// daemon's fake-ControlPlane pattern (its fake is `#[cfg(test)]` there).
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
    }

    impl ControlPlane for FakeControlPlane {
        fn post_json(&self, url: &str, body: &str) -> Result<Response, EnrollError> {
            self.calls.borrow_mut().push((url.to_string(), body.to_string()));
            self.answer.clone().map_err(EnrollError::Transport)
        }
    }

    /// A control plane whose answers are a script, one per call, in order —
    /// what the link poller needs and one canned answer cannot say: pending
    /// on the first poll, signed-in on the second.
    struct ScriptedControlPlane {
        answers: RefCell<std::collections::VecDeque<Response>>,
        calls: RefCell<Vec<(String, String)>>,
    }

    impl ScriptedControlPlane {
        fn answering(script: &[(u16, &str)]) -> Self {
            Self {
                answers: RefCell::new(
                    script
                        .iter()
                        .map(|(status, body)| Response { status: *status, body: (*body).to_string() })
                        .collect(),
                ),
                calls: RefCell::new(Vec::new()),
            }
        }
    }

    impl ControlPlane for ScriptedControlPlane {
        fn post_json(&self, url: &str, body: &str) -> Result<Response, EnrollError> {
            self.calls.borrow_mut().push((url.to_string(), body.to_string()));
            self.answers
                .borrow_mut()
                .pop_front()
                .ok_or_else(|| EnrollError::Transport("the script ran out of answers".into()))
        }
    }

    /// One canned answer for every authenticated request, recording the path
    /// and the bearer it was handed.
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

    fn hex(bytes: &[u8]) -> String {
        bytes.iter().map(|b| format!("{b:02x}")).collect()
    }

    #[test]
    fn the_body_a_device_posts_verifies_as_a_client() {
        // The whole seam in one test: the Worker verifies a device claim as
        // role `client` over the (code, key, label) preimage, and both ends
        // are separate implementations — agreement here is what makes the
        // app's signature worth anything.
        let id = identity();
        let body = signed_client_body(&id, "ABCD1234", "andy-mac").expect("fits");
        let parsed: serde_json::Value = serde_json::from_str(&body).expect("valid JSON");

        let sig_hex = parsed["sig"].as_str().expect("a sig field");
        let sig_bytes: Vec<u8> = (0..sig_hex.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&sig_hex[i..i + 2], 16).expect("hex"))
            .collect();
        let sig = Signature::from_slice(&sig_bytes).expect("64 bytes");

        let message = enrollment_request_for("ABCD1234", &id.client_id().0, "andy-mac")
            .expect("fits");
        assert!(
            verify_client(id.client_id(), Purpose::Enrollment, &message, &sig).is_ok(),
            "the signature must verify as role `client` — the role the Worker takes from \
             a device code's kind; a host-role signature would refuse every device enrolment"
        );
        assert_eq!(
            parsed["hostId"].as_str(),
            Some(hex(&id.client_id().0).as_str()),
            "the wire field is named hostId whatever key it carries — the Worker's comment \
             says why it is not renamed per kind"
        );
        assert_eq!(
            parsed["deviceKind"].as_str(),
            Some("desktop"),
            "unlike the daemon, the app IS a device kind and must say which — the devices \
             screen renders it"
        );
        assert_eq!(parsed["platform"].as_str(), Some(std::env::consts::OS));
        assert_eq!(parsed["code"], "ABCD1234");
        assert_eq!(parsed["label"], "andy-mac");
    }

    #[test]
    fn a_link_start_posts_a_verifying_request_and_parses_the_grant() {
        let store = MemoryKeyStore::new();
        let id = identity();
        let http = FakeControlPlane::answering(
            200,
            r#"{"grant":"g_0123456789012345678901234567890123456789012","expiresAt":1755000600000}"#,
        );

        let granted = start_link(&id, "andy-desktop", "https://example", &http, &store)
            .expect("a 200 with a grant is a grant");
        assert_eq!(granted.grant, "g_0123456789012345678901234567890123456789012");
        assert_eq!(granted.expires_at, 1_755_000_600_000, "the poller's deadline");

        let calls = http.calls.borrow();
        assert_eq!(
            calls[0].0,
            format!("https://example{LINK_START_PATH}"),
            "the route is the contract with the Worker"
        );
        let parsed: serde_json::Value = serde_json::from_str(&calls[0].1).expect("valid JSON");
        assert_eq!(parsed["kind"], "desktop", "this flow exists for the app");
        let sig_hex = parsed["sig"].as_str().expect("a sig field");
        let sig = zest_mesh::identity::Signature::from_slice(
            &(0..sig_hex.len())
                .step_by(2)
                .map(|i| u8::from_str_radix(&sig_hex[i..i + 2], 16).expect("hex"))
                .collect::<Vec<_>>(),
        )
        .expect("64 bytes");
        assert!(
            zest_mesh::link::verify_link_request(id.client_id(), "andy-desktop", &sig),
            "the posted signature must pass the exact check the Worker runs, or every \
             hand-off dies at start with bad_signature"
        );
    }

    #[test]
    fn a_locked_store_stops_the_link_before_the_browser_is_ever_opened() {
        // Earlier than the enrol probe, and for a dearer reason: the cost of
        // a late locked keychain here is a person's browser click, not just
        // a code.
        let http = FakeControlPlane::answering(200, r#"{"grant":"g","expiresAt":1}"#);
        let err = start_link(&identity(), "andy-desktop", "https://example", &http, &LockedStore)
            .expect_err("an unusable store must stop this");
        assert!(matches!(err, EnrollError::Store(_)), "got {err}");
        assert!(
            http.calls.borrow().is_empty(),
            "no grant may be minted for a token that cannot be kept"
        );
    }

    #[test]
    fn a_link_claim_pends_then_lands_the_token_under_the_apps_name() {
        // The poller's happy path in miniature: first poll pending, second
        // signed in — and the claim's signature verifies as the Worker will.
        let store = MemoryKeyStore::new();
        let id = identity();
        let http = ScriptedControlPlane::answering(&[
            (200, r#"{"status":"pending"}"#),
            (200, r#"{"device":{"id":"x"},"token":"zt1_link","account":"andy"}"#),
        ]);

        let first = claim_link(&id, "grant-id", "https://example", &http, &store)
            .expect("pending is an answer, not an error");
        assert_eq!(first, LinkOutcome::Pending);
        assert!(
            stored_app_token(&store).expect("readable").is_none(),
            "nothing was granted yet, so nothing may be kept"
        );

        let second = claim_link(&id, "grant-id", "https://example", &http, &store)
            .expect("an approved claim");
        assert_eq!(second, LinkOutcome::SignedIn { account: Some("andy".into()) });
        assert_eq!(
            stored_app_token(&store).expect("readable").as_deref(),
            Some("zt1_link"),
            "the token is the entire point — and under the app's name"
        );
        assert!(
            store.load_secret(CLOUD_TOKEN_NAME).expect("readable").is_none(),
            "the daemon's slot stays empty — same store, different principal"
        );

        let calls = http.calls.borrow();
        assert_eq!(calls[0].0, format!("https://example{LINK_CLAIM_PATH}"));
        let parsed: serde_json::Value = serde_json::from_str(&calls[0].1).expect("valid JSON");
        let sig_hex = parsed["sig"].as_str().expect("a sig field");
        let sig = zest_mesh::identity::Signature::from_slice(
            &(0..sig_hex.len())
                .step_by(2)
                .map(|i| u8::from_str_radix(&sig_hex[i..i + 2], 16).expect("hex"))
                .collect::<Vec<_>>(),
        )
        .expect("64 bytes");
        assert!(
            zest_mesh::link::verify_link_claim("grant-id", id.client_id(), &sig),
            "the claim signature must pass the Worker's check — a leaked grant id alone \
             must be worth nothing, and this signature is what makes that true"
        );
    }

    #[test]
    fn a_dead_grant_is_a_terminal_refusal_and_not_an_error() {
        // The distinction the poller lives on: `Refused` stops the loop,
        // a transport `Err` keeps it polling through a blip. The server's
        // collapsed answer must land in the former.
        let store = MemoryKeyStore::new();
        let http = FakeControlPlane::answering(400, r#"{"error":"invalid_grant"}"#);
        let got = claim_link(&identity(), "grant-id", "https://example", &http, &store)
            .expect("a refusal is an outcome, not a transport failure");
        assert!(
            matches!(&got, LinkOutcome::Refused(m) if m.contains("invalid_grant")),
            "the server's one collapsed word survives; got {got:?}"
        );
        assert!(stored_app_token(&store).expect("readable").is_none());
    }

    #[test]
    fn the_fingerprint_matches_the_approval_pages_rendering() {
        // Pinned to FINGERPRINT_CHARS in cloud/packages/web/src/api/link.ts
        // (first 8 lowercase hex) through fingerprintGroups in
        // clients/web/packages/app/src/link.ts (a space every 4): the person
        // COMPARES the app's string against the page's, and an honest pair
        // that renders differently trains them to stop comparing.
        let mut bytes = [0u8; 32];
        bytes[0] = 0xab;
        bytes[1] = 0x12;
        bytes[2] = 0xcd;
        bytes[3] = 0x34;
        assert_eq!(key_fingerprint(ClientId::from_bytes(bytes)), "ab12 cd34");
    }

    #[test]
    fn the_token_lands_under_the_apps_name_and_not_the_daemons() {
        // The principal split in one assertion: a token filed under
        // `cloud-token` would be presented by the daemon as the *machine's*,
        // and the two principals' revocations would cross.
        let store = MemoryKeyStore::new();
        let http = FakeControlPlane::answering(200, r#"{"token":"zt1_abc","account":"andy"}"#);

        let enrolled =
            enroll_desktop(&identity(), "ABCD1234", "andy-mac", "https://example", &http, &store)
                .expect("a 200 with a token is an enrolment");

        assert_eq!(enrolled.account.as_deref(), Some("andy"));
        assert_eq!(
            stored_app_token(&store).expect("readable").as_deref(),
            Some("zt1_abc"),
            "the token is the entire point of enrolling"
        );
        assert!(
            store.load_secret(CLOUD_TOKEN_NAME).expect("readable").is_none(),
            "the daemon's slot must stay empty — same store, different principal"
        );
        assert_eq!(
            http.calls.borrow()[0].0,
            format!("https://example{ENROLL_PATH}"),
            "the route is part of the contract with the Worker"
        );
    }

    #[test]
    fn a_locked_store_stops_the_claim_before_the_code_is_spent() {
        // The load-bearing order: a code is one-shot, so a keychain
        // discovered locked after the POST leaves the account with a device
        // row and this app with nothing.
        let http = FakeControlPlane::answering(200, r#"{"token":"zt1_abc"}"#);
        let err = enroll_desktop(&identity(), "C", "l", "https://example", &http, &LockedStore)
            .expect_err("an unusable store must stop this");
        assert!(matches!(err, EnrollError::Store(_)), "got {err}");
        assert!(
            http.calls.borrow().is_empty(),
            "the code must not be spent on an enrolment whose token cannot be kept"
        );
    }

    #[test]
    fn a_refusal_stores_nothing() {
        let store = MemoryKeyStore::new();
        let http = FakeControlPlane::answering(400, r#"{"error":"invalid_code"}"#);
        let err = enroll_desktop(&identity(), "C", "l", "https://example", &http, &store)
            .expect_err("a 400 is not an enrolment");
        assert!(matches!(err, EnrollError::Refused { status: 400, .. }), "got {err}");
        assert!(
            stored_app_token(&store).expect("readable").is_none(),
            "nothing was granted, so nothing may be kept"
        );
    }

}
