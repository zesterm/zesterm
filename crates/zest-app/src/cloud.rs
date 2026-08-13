//! The app's own account client: enrolling this desktop as a *device*, and
//! the bearer-token reads that follow from it.
//!
//! Deliberately parallel to `zest_daemon::enroll` rather than a call into it:
//! the daemon enrols a machine (its `HostIdentity`, role `host`, token under
//! `cloud-token`), while the app enrols the key a person carries between
//! machines (its `ClientIdentity`, role `client`, token under
//! `app-cloud-token`). Same wire, different principals — the split the
//! keystore's doc comment on [`APP_CLOUD_TOKEN_NAME`] exists to defend. The
//! transport traits are seams for the same reason as the daemon's: every test
//! below drives a real enrolment or a real account read without a socket.

use zest_cloud::http::Endpoint;
use zest_cloud::tls::Roots;
use zest_daemon::enroll::{ControlPlane, EnrollError, Enrolled, ENROLL_PATH};
use zest_mesh::enroll::enrollment_request_for;
use zest_mesh::identity::{ClientIdentity, Purpose};
use zest_mesh::keystore::{SecretStore, APP_CLOUD_TOKEN_NAME};
use zest_proto::auth::Sig64;
use zest_proto::{ClientId, HostId};

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
        return Err(EnrollError::Refused {
            status: response.status,
            message: message_from(&response.body),
        });
    }

    #[derive(serde::Deserialize)]
    struct Answer {
        token: Option<String>,
        account: Option<String>,
        // A 200 carrying an error is a refusal, not a missing token — the
        // GitHub-OAuth shape the daemon's enroll documents.
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

    secrets.store_secret(APP_CLOUD_TOKEN_NAME, token.as_bytes())?;
    Ok(Enrolled { account: answer.account })
}

/// The app's bearer token, if it has one. `Ok(None)` is "never enrolled".
pub fn stored_app_token(secrets: &dyn SecretStore) -> Result<Option<String>, EnrollError> {
    let Some(bytes) = secrets.load_secret(APP_CLOUD_TOKEN_NAME)? else {
        return Ok(None);
    };
    // Not text means not a token — a key filed under the wrong name — and
    // guessing would put a corrupt Authorization header on every request.
    String::from_utf8(bytes.to_vec())
        .map(Some)
        .map_err(|_| EnrollError::BadResponse(format!("{APP_CLOUD_TOKEN_NAME} is not text")))
}

/// Forget the app's token. Local only — the account still lists the device
/// until it is revoked from the devices screen.
pub fn forget_app_token(secrets: &dyn SecretStore) -> Result<bool, EnrollError> {
    let had = secrets.load_secret(APP_CLOUD_TOKEN_NAME)?.is_some();
    secrets.delete_secret(APP_CLOUD_TOKEN_NAME)?;
    Ok(had)
}

/// Why an account read failed.
///
/// `SignedOut` is its own variant rather than a status inside `BadAnswer`
/// because it demands a different act from the person: a 401 means the token
/// was revoked (or never was one), and the app should offer sign-in again —
/// not retry, which is what a transport failure suggests.
#[derive(Debug, Clone, thiserror::Error)]
#[allow(dead_code, reason = "the account-backed picker consumes these; tests hold them until then")]
pub enum CloudError {
    #[error("the account no longer accepts this device's token")]
    SignedOut,
    #[error("could not reach the control plane: {0}")]
    Transport(String),
    #[error("the control plane's answer was not usable: {0}")]
    BadAnswer(String),
}

/// The two authenticated requests the app makes.
///
/// A trait for the reason `ControlPlane` is one: the parsing and the 401
/// classification below are the parts that go wrong silently, and none of
/// them needs a socket to be got right.
#[allow(dead_code, reason = "the account-backed picker consumes these; tests hold them until then")]
pub trait AccountApi {
    fn get(&self, path: &str, bearer: &str) -> std::io::Result<zest_cloud::http::Response>;
    fn post(
        &self,
        path: &str,
        bearer: &str,
        body: &str,
    ) -> std::io::Result<zest_cloud::http::Response>;
}

/// The real one: bearer-headed HTTPS through the crate that owns TLS.
#[allow(dead_code, reason = "the account-backed picker consumes these; tests hold them until then")]
pub struct HttpsAccountApi {
    base: Endpoint,
    roots: Roots,
}

#[allow(dead_code, reason = "the account-backed picker consumes these; tests hold them until then")]
impl HttpsAccountApi {
    /// Verifying the control plane against `roots`; refuses a base URL that
    /// cannot be requested (`Endpoint`'s rules — https only).
    pub fn new(base_url: &str, roots: Roots) -> std::io::Result<Self> {
        Ok(Self { base: Endpoint::parse(base_url)?, roots })
    }

    /// `path` under the configured base — a control plane behind a path
    /// prefix keeps its prefix, and a bare origin's `/` does not double.
    fn target(&self, path: &str) -> String {
        if self.base.path == "/" {
            path.to_string()
        } else {
            format!("{}{path}", self.base.path.trim_end_matches('/'))
        }
    }
}

impl AccountApi for HttpsAccountApi {
    fn get(&self, path: &str, bearer: &str) -> std::io::Result<zest_cloud::http::Response> {
        zest_cloud::http::get(
            &self.base.host,
            self.base.port,
            &self.target(path),
            &[("authorization", &format!("Bearer {bearer}"))],
            self.roots,
        )
    }

    fn post(
        &self,
        path: &str,
        bearer: &str,
        body: &str,
    ) -> std::io::Result<zest_cloud::http::Response> {
        zest_cloud::http::post_json_with(
            &self.base.host,
            self.base.port,
            &self.target(path),
            body,
            &[("authorization", &format!("Bearer {bearer}"))],
            self.roots,
        )
    }
}

/// One machine the account lists.
#[derive(Debug, Clone, PartialEq, Eq)]
#[allow(dead_code, reason = "the account-backed picker consumes these; tests hold them until then")]
pub struct AccountHost {
    pub host: HostId,
    pub label: String,
    /// When the relay last saw it, epoch milliseconds; `None` when it has
    /// never dialled in.
    pub last_seen_ms: Option<u64>,
}

/// What `GET /api/hosts` answers: the fleet as the account knows it.
#[derive(Debug, Clone, PartialEq, Eq)]
#[allow(dead_code, reason = "the account-backed picker consumes these; tests hold them until then")]
pub struct AccountHosts {
    /// The account's display name, when the answer carries one.
    pub account: Option<String>,
    /// Where the relay lives; `None` on a deployment without one, in which
    /// case the hosts are listed but unreachable through the account.
    pub relay_origin: Option<String>,
    pub hosts: Vec<AccountHost>,
}

/// The account's host list, or why it could not be read.
#[allow(dead_code, reason = "the account-backed picker consumes these; tests hold them until then")]
pub fn fetch_hosts(api: &dyn AccountApi, token: &str) -> Result<AccountHosts, CloudError> {
    let got = api
        .get("/api/hosts", token)
        .map_err(|e| CloudError::Transport(e.to_string()))?;
    if got.status == 401 {
        return Err(CloudError::SignedOut);
    }
    if !(200..300).contains(&got.status) {
        return Err(CloudError::BadAnswer(format!("{}: {}", got.status, clip(&got.body))));
    }

    // Permissive on purpose: unknown fields are the control plane's future,
    // and every field the app does not strictly need is optional — a missing
    // relayOrigin is a listing without a route, not a parse failure.
    #[derive(serde::Deserialize)]
    struct Row {
        id: HostId,
        label: String,
        #[serde(rename = "lastSeenAt", default)]
        last_seen_at: Option<u64>,
    }
    #[derive(serde::Deserialize)]
    struct Answer {
        #[serde(default)]
        hosts: Vec<Row>,
        #[serde(rename = "relayOrigin", default)]
        relay_origin: Option<String>,
        #[serde(default)]
        account: Option<String>,
    }

    let answer: Answer = serde_json::from_str(&got.body)
        .map_err(|e| CloudError::BadAnswer(format!("{e}; body was {:?}", clip(&got.body))))?;
    Ok(AccountHosts {
        account: answer.account,
        relay_origin: answer.relay_origin,
        hosts: answer
            .hosts
            .into_iter()
            .map(|r| AccountHost { host: r.id, label: r.label, last_seen_ms: r.last_seen_at })
            .collect(),
    })
}

/// An attach ticket for `host`, or why the relay would not admit us.
#[allow(dead_code, reason = "the account-backed picker consumes these; tests hold them until then")]
pub fn mint_ticket(
    api: &dyn AccountApi,
    token: &str,
    host: HostId,
) -> Result<String, CloudError> {
    let body = serde_json::json!({ "hostId": host }).to_string();
    let got = api
        .post("/api/relay/ticket", token, &body)
        .map_err(|e| CloudError::Transport(e.to_string()))?;
    if got.status == 401 {
        return Err(CloudError::SignedOut);
    }
    if !(200..300).contains(&got.status) {
        return Err(CloudError::BadAnswer(format!("{}: {}", got.status, clip(&got.body))));
    }

    #[derive(serde::Deserialize)]
    struct Answer {
        ticket: Option<String>,
    }
    let answer: Answer = serde_json::from_str(&got.body)
        .map_err(|e| CloudError::BadAnswer(format!("{e}; body was {:?}", clip(&got.body))))?;
    answer
        .ticket
        .filter(|t| !t.is_empty())
        .ok_or_else(|| CloudError::BadAnswer(format!("no ticket in {:?}", clip(&got.body))))
}

/// The most useful sentence in a refusal body — the daemon's `message_from`,
/// private there and three lines here.
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

    /// One canned answer for every authenticated request, recording the path
    /// and the bearer it was handed.
    struct FakeAccountApi {
        status: u16,
        body: String,
        calls: RefCell<Vec<(String, String)>>,
    }

    impl FakeAccountApi {
        fn answering(status: u16, body: &str) -> Self {
            Self { status, body: body.to_string(), calls: RefCell::new(Vec::new()) }
        }
    }

    impl AccountApi for FakeAccountApi {
        fn get(&self, path: &str, bearer: &str) -> std::io::Result<zest_cloud::http::Response> {
            self.calls.borrow_mut().push((path.to_string(), bearer.to_string()));
            Ok(zest_cloud::http::Response { status: self.status, body: self.body.clone() })
        }
        fn post(
            &self,
            path: &str,
            bearer: &str,
            _body: &str,
        ) -> std::io::Result<zest_cloud::http::Response> {
            self.calls.borrow_mut().push((path.to_string(), bearer.to_string()));
            Ok(zest_cloud::http::Response { status: self.status, body: self.body.clone() })
        }
    }

    /// A store that is present and unreadable — a locked keychain.
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

    #[test]
    fn forgetting_the_app_token_leaves_the_daemons_alone() {
        let store = MemoryKeyStore::new();
        store.store_secret(APP_CLOUD_TOKEN_NAME, b"zt1_app").expect("store");
        store.store_secret(CLOUD_TOKEN_NAME, b"zt1_daemon").expect("store");

        assert!(forget_app_token(&store).expect("forget"), "there was a token");
        assert!(stored_app_token(&store).expect("readable").is_none());
        assert_eq!(
            store.load_secret(CLOUD_TOKEN_NAME).expect("readable").as_deref().map(Vec::as_slice),
            Some(b"zt1_daemon".as_slice()),
            "signing the app out must not sign the machine out — different principals"
        );
    }

    #[test]
    fn a_401_reads_as_signed_out_and_not_as_an_outage() {
        // The variant split's whole purpose: a revoked token asks the person
        // to sign in again, an outage asks them to wait — collapsing them
        // sends someone minting fresh codes at a network blip.
        let api = FakeAccountApi::answering(401, r#"{"error":"unauthorized"}"#);
        assert!(
            matches!(fetch_hosts(&api, "zt1_x"), Err(CloudError::SignedOut)),
            "hosts under a dead token is a sign-out"
        );
        assert!(
            matches!(
                mint_ticket(&api, "zt1_x", HostId::from_bytes([7; 32])),
                Err(CloudError::SignedOut)
            ),
            "a ticket under a dead token is the same fact"
        );
    }

    #[test]
    fn a_host_listing_tolerates_the_control_planes_future() {
        // Unknown fields and a missing relayOrigin are the shapes a deployed
        // Worker will legitimately grow; a client that breaks on them holds
        // the control plane's schema frozen from the wrong end.
        let api = FakeAccountApi::answering(
            200,
            &format!(
                r#"{{"hosts":[{{"id":"{}","label":"studio","lastSeenAt":null,"platform":"macos","extra":1}}],"unknownTop":true}}"#,
                hex(&[0x22; 32])
            ),
        );
        let got = fetch_hosts(&api, "zt1_x").expect("a listing with extras still parses");
        assert_eq!(got.hosts.len(), 1);
        assert_eq!(got.hosts[0].host, HostId::from_bytes([0x22; 32]));
        assert_eq!(got.hosts[0].label, "studio");
        assert_eq!(got.hosts[0].last_seen_ms, None, "null lastSeenAt is 'never', not an error");
        assert_eq!(
            got.relay_origin, None,
            "a deployment without a relay still answers a listing"
        );
        assert_eq!(
            api.calls.borrow()[0],
            ("/api/hosts".to_string(), "zt1_x".to_string()),
            "the route and the bearer are the request"
        );
    }

    #[test]
    fn a_ticket_comes_back_as_the_ticket_string() {
        let api = FakeAccountApi::answering(200, r#"{"ticket":"v1.abc.def","expiresAt":123}"#);
        assert_eq!(
            mint_ticket(&api, "zt1_x", HostId::from_bytes([7; 32])).expect("minted"),
            "v1.abc.def"
        );
    }

    #[test]
    fn paths_join_the_configured_base_without_doubling() {
        // A `//` in the request target is a 404 that reads as an expired
        // token; a lost prefix routes past a control plane behind one.
        let bare = HttpsAccountApi::new("https://example.test", Roots::Bundled).expect("parses");
        assert_eq!(bare.target("/api/hosts"), "/api/hosts");
        let prefixed =
            HttpsAccountApi::new("https://example.test/base/", Roots::Bundled).expect("parses");
        assert_eq!(prefixed.target("/api/hosts"), "/base/api/hosts");
    }
}
