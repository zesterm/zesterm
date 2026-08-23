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
//! # The transport, and why it is still injected
//!
//! [`HttpsControlPlane`] is the real one: `zest_cloud::http` over
//! `zest_cloud::tls`, which is the workspace's single owner of rustls and of
//! anything resembling an HTTP client. It replaced a `NoHttpClient` stub that
//! failed with an error naming the crate that did not exist yet — the seam was
//! built first on purpose, because the parts either side of the wire (the
//! signature, the exact JSON, what counts as a refusal, where the token goes)
//! are the ones that are wrong in ways nobody notices, and none of them needs a
//! socket to be got right.
//!
//! The [`ControlPlane`] trait outlived the stub for a reason that is not
//! historical: every test below drives a real enrolment without opening one.
//! A test that reached the network would be a test that fails on an aeroplane
//! and passes against whatever the deployed Worker happens to answer today.

use zest_cloud::http::Endpoint;
use zest_cloud::tls::Roots;
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
/// A trait rather than a function taking a URL, so that the binary can be given
/// [`HttpsControlPlane`] and every test a fake, without a line of the enrolment
/// logic between them changing. That is what kept this whole module — the
/// signing, the JSON, the refusals, the store — testable months before the
/// workspace had a TLS stack, and it is what keeps it testable now without one
/// test opening a socket.
pub trait ControlPlane {
    /// POST `body` to `url` as `application/json` and return what came back.
    ///
    /// An `Err` here means the request did not complete — no DNS, no route, a
    /// TLS failure. A response that completed and said no is `Ok` with a status
    /// in it, because that is a refusal and not a transport failure.
    fn post_json(&self, url: &str, body: &str) -> Result<Response, EnrollError>;
}

/// The real one: one HTTPS POST, through the crate that owns TLS.
///
/// Holds only the choice of trust anchors, because that is the one thing about
/// this request a machine can need to be told — see [`Roots`]. Everything else
/// is fixed by the URL.
/// There is no `Default`: which trust store to check the control plane's
/// certificate against is a choice with two live answers — a managed laptop
/// behind a middlebox needs [`Roots::Platform`], a minimal container has no
/// platform store to find — and a caller that did not think about it is a
/// caller that will meet the other one as an unexplained certificate error.
/// What actually goes to the network, behind a seam so a test can watch it.
///
/// The seam exists for one line — the call below — and that line is the only
/// production code here the shipping CLI runs which nothing else covers.
/// Without it, four separate mutations of it survived the whole suite:
/// dropping the parsed port for 443, posting to `/` instead of the parsed
/// path, sending an empty body, and ignoring the configured roots. The first
/// is the exact failure `Endpoint`'s doc comment says this design exists to
/// prevent — wrong on the first day anyone runs a control plane on 8443, and
/// `--control-plane https://127.0.0.1:8787` is in the README.
type Post = fn(&str, u16, &str, &str, Roots) -> std::io::Result<zest_cloud::http::Response>;

#[derive(Clone, Copy)]
pub struct HttpsControlPlane {
    roots: Roots,
    post: Post,
}

impl HttpsControlPlane {
    /// Verifying the control plane against `roots`.
    pub fn new(roots: Roots) -> Self {
        Self { roots, post: zest_cloud::http::post_json }
    }

    /// The same, over a stand-in that records what it was asked for.
    #[cfg(test)]
    fn posting_with(roots: Roots, post: Post) -> Self {
        Self { roots, post }
    }
}

impl ControlPlane for HttpsControlPlane {
    fn post_json(&self, url: &str, body: &str) -> Result<Response, EnrollError> {
        let to = addressed(url)?;
        received(url, (self.post)(&to.host, to.port, &to.path, body, self.roots))
    }
}

/// Where the configured URL says to post.
///
/// A URL that cannot be requested — the wrong scheme, no host, a port that is
/// not a number — is a `Transport` failure and not a new variant, because it is
/// the same thing to everyone downstream: nothing was asked, so nothing was
/// refused, and the token is exactly where it was. The message quotes the URL,
/// which is the part a person can act on.
fn addressed(url: &str) -> Result<Endpoint, EnrollError> {
    Endpoint::parse(url).map_err(|e| EnrollError::Transport(e.to_string()))
}

/// The one place `zest_cloud::http`'s answer becomes enrolment's.
///
/// Everything that crate reports as an `Err` is a request that produced no
/// usable response — no route, a certificate that did not verify, a reply that
/// is not HTTP/1.x, a body in an encoding it refuses to guess at — so all of it
/// is [`EnrollError::Transport`]. That is what keeps the two things a person
/// must be told apart: [`EnrollError::Refused`] and
/// [`EnrollError::BadResponse`] are reachable only through a response that
/// actually arrived, so "the control plane said no" can never be printed
/// because the wifi dropped.
///
/// Separate from the impl above so the tests can put real HTTP bytes through
/// the real client and the real mapping with no socket in the way; a fake that
/// returned a `Response` directly would leave this function untested and it is
/// where the classification is decided.
fn received(
    url: &str,
    answer: std::io::Result<zest_cloud::http::Response>,
) -> Result<Response, EnrollError> {
    let got = answer.map_err(|e| EnrollError::Transport(format!("POST {url}: {e}")))?;
    Ok(Response { status: got.status, body: got.body })
}

#[derive(Debug, thiserror::Error)]
pub enum EnrollError {
    /// The request never completed.
    #[error("could not reach the control plane: {0}")]
    Transport(String),
    /// It completed, and the answer was no.
    ///
    /// `detail` is the Worker's optional second word (#367): for
    /// `already_enrolled` it names which of the two collapsed causes held —
    /// `revoked` or `other_account` — because they need opposite next moves.
    /// `None` from a Worker that predates it; absent from `Display` on
    /// purpose, since [`refusal_text`] is what turns the pair into a sentence.
    #[error("the control plane refused this enrolment ({status}): {message}")]
    Refused { status: u16, message: String, detail: Option<String> },
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
    /// The link-flow twin of `Preimage` (#226): a grant id or label too long
    /// to encode unambiguously. Its own variant because the two preimage
    /// families are separate types on purpose, and stringifying one into the
    /// other would lose which flow refused.
    #[error("{0}")]
    LinkPreimage(#[from] zest_mesh::link::LinkError),
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
        // Advisory, and deliberately *outside* the signature: the preimage
        // stays `(code, key, label)` so no fixture moves. The Worker screens
        // it for control characters and renders it, never decides on it. No
        // `deviceKind` here — that field's values are device kinds
        // (browser/phone/desktop) and the Worker 400s anything else, so a
        // daemon naming itself would be refused, not described.
        platform: &'a str,
    }

    Ok(serde_json::to_string(&Body {
        code,
        host_id: host,
        label,
        sig: Sig64(sig.to_bytes()),
        platform: std::env::consts::OS,
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
        let (message, detail) = refusal_from(&response.body);
        return Err(EnrollError::Refused { status: response.status, message, detail });
    }

    #[derive(serde::Deserialize)]
    struct Answer {
        token: Option<String>,
        account: Option<String>,
        // A 200 carrying an error is not hypothetical — it is how GitHub's
        // OAuth endpoint reports a bad code, and `cloud/`'s own tests exercise
        // it. Read here so that shape is a refusal rather than a missing token.
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

/// The most useful sentence in a refusal body, and the optional cause beside it.
///
/// `cloud/`'s Worker answers `{"error":"forbidden"}` — since #367 sometimes
/// with a `detail` naming which collapsed cause held — but a 502 from in front
/// of it is an HTML page, so this falls back to the raw body rather than
/// reporting nothing when the JSON does not parse — "the control plane refused
/// this" with no reason is the least actionable error there is.
pub(crate) fn refusal_from(body: &str) -> (String, Option<String>) {
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

/// A refusal as the sentence a person at a *host* enrolment should read —
/// `--enroll` on a terminal, or the app's "Enroll this machine" card via the
/// `EnrollSeam` (the app's own device sign-in has its own wording, because the
/// next move points at the other kind's button).
///
/// One function for both call sites, because two copies of "what does
/// already_enrolled mean" would drift — that is how the app spent a release
/// telling revoked machines to mint fresh codes (#368).
pub fn refusal_text(e: &EnrollError) -> String {
    let EnrollError::Refused { message, detail, .. } = e else {
        return e.to_string();
    };
    match (message.as_str(), detail.as_deref()) {
        // A device code fed to a host enrolment (#228): the generic advice
        // would mint another code of the same wrong kind.
        ("wrong_kind", _) => {
            "that code is for the app's sign-in — in the browser use Add a machine instead".into()
        }
        ("already_enrolled", Some("revoked")) => "this machine was revoked — restore it in the \
             browser (fleet screen, Revoked section), then try again"
            .into(),
        ("already_enrolled", Some("other_account")) => "this machine's key is enrolled with a \
             different account — manage it from that account's fleet screen"
            .into(),
        // A Worker predating #367 names no cause, but a fresh code is still
        // never the fix for this refusal.
        ("already_enrolled", _) => "this machine's key is already enrolled — if it was revoked, \
             restore it in the browser (fleet screen, Revoked section)"
            .into(),
        _ => e.to_string(),
    }
}

/// Enough of a response to diagnose it, and not a whole error page.
pub(crate) fn clip(body: &str) -> String {
    const LIMIT: usize = 200;
    let trimmed = body.trim();
    match trimmed.char_indices().nth(LIMIT) {
        Some((end, _)) => format!("{}…", &trimmed[..end]),
        None => trimmed.to_string(),
    }
}

#[cfg(test)]
mod tests {

    /// What `HttpsControlPlane` hands the transport, which nothing else checks.
    ///
    /// Four mutations of that one line survived the entire suite before this
    /// existed: the parsed port replaced by 443, the parsed path by `/`, the
    /// signed body by `""`, and the configured roots by `Bundled`. Every one
    /// is silent — the daemon dials *something* and reports whatever comes
    /// back — and the port one is the failure `Endpoint`'s doc comment says
    /// the parsing exists to prevent.
    ///
    /// A `fn` pointer rather than a closure so `HttpsControlPlane` stays
    /// `Copy`; the recording goes through a thread-local because a `fn` cannot
    /// capture.
    #[test]
    fn what_reaches_the_transport_is_what_the_url_said() {
        use std::cell::RefCell;
        /// What the transport was handed, minus the roots — those have their
        /// own test below, because asserting them here would mean recording a
        /// value this test never varies.
        struct Asked {
            host: String,
            port: u16,
            path: String,
            body: String,
        }
        thread_local! {
            static SEEN: RefCell<Option<Asked>> = const { RefCell::new(None) };
        }
        fn record(
            host: &str,
            port: u16,
            path: &str,
            body: &str,
            roots: Roots,
        ) -> std::io::Result<zest_cloud::http::Response> {
            let _ = roots;
            SEEN.with(|s| {
                *s.borrow_mut() = Some(Asked {
                    host: host.to_string(),
                    port,
                    path: path.to_string(),
                    body: body.to_string(),
                });
            });
            Ok(zest_cloud::http::Response { status: 200, body: "{}".into() })
        }

        let plane = HttpsControlPlane::posting_with(Roots::Bundled, record);
        let _ = plane.post_json("https://127.0.0.1:8787/api/enroll/claim", "{\"code\":\"abc\"}");

        let seen = SEEN.with(|s| s.borrow_mut().take()).expect("the transport was never called");
        assert_eq!(seen.host, "127.0.0.1", "the host in the URL is the host dialled");
        assert_eq!(
            seen.port, 8787,
            "the port was parsed and then thrown away, so every control plane not on 443 is \
             unreachable -- the one day this is wrong is the first day anyone runs one"
        );
        assert_eq!(seen.path, "/api/enroll/claim", "the path was parsed and then not used");
        assert_eq!(seen.body, "{\"code\":\"abc\"}", "the signed claim never reached the wire");
    }

    /// The roots a caller chose are the roots that verify the certificate.
    #[test]
    fn the_configured_roots_reach_the_transport() {
        use std::cell::RefCell;
        thread_local! {
            static ROOTS: RefCell<Option<Roots>> = const { RefCell::new(None) };
        }
        fn record(
            _h: &str,
            _p: u16,
            _path: &str,
            _b: &str,
            roots: Roots,
        ) -> std::io::Result<zest_cloud::http::Response> {
            ROOTS.with(|s| *s.borrow_mut() = Some(roots));
            Ok(zest_cloud::http::Response { status: 200, body: "{}".into() })
        }

        let plane = HttpsControlPlane::posting_with(Roots::Platform, record);
        let _ = plane.post_json("https://example.test/api/enroll/claim", "{}");
        assert!(
            matches!(ROOTS.with(|s| s.borrow_mut().take()), Some(Roots::Platform)),
            "a caller that asked for the OS trust store got something else, which is a \
             certificate error nobody can explain on the machine it happens to"
        );
    }
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

    /// The real client with the socket taken out: real request bytes into a
    /// `Vec`, a canned response out of one.
    ///
    /// This is [`HttpsControlPlane`] with `TlsDuplex::connect` replaced and
    /// nothing else — the same [`addressed`], the same request writer and
    /// response parser in `zest_cloud::http`, the same [`received`]. What it
    /// deliberately does not exercise is TLS and the `host:` header's port
    /// suffix, both of which are `zest-cloud`'s to test; what it buys is that
    /// "a 403 is a refusal" is proven against bytes a Worker could really
    /// send, rather than against a `Response` a fake handed over ready-made.
    struct CannedHttp {
        response: Vec<u8>,
        sent: RefCell<Vec<u8>>,
    }

    impl CannedHttp {
        /// `head` carries the status line and any headers, each already
        /// CRLF-terminated; the length and the blank line are added here so a
        /// test only has to say the interesting part.
        fn answering(head: &str, body: &str) -> Self {
            Self {
                response: format!("{head}content-length: {}\r\n\r\n{body}", body.len()).into_bytes(),
                sent: RefCell::new(Vec::new()),
            }
        }

        /// A peer that accepted the connection and then said nothing.
        fn silent() -> Self {
            Self { response: Vec::new(), sent: RefCell::new(Vec::new()) }
        }

        fn request(&self) -> String {
            String::from_utf8(self.sent.borrow().clone()).expect("the request is UTF-8")
        }
    }

    impl ControlPlane for CannedHttp {
        fn post_json(&self, url: &str, body: &str) -> Result<Response, EnrollError> {
            let to = addressed(url)?;
            let mut sent = self.sent.borrow_mut();
            received(
                url,
                zest_cloud::http::exchange(&self.response[..], &mut *sent, &to.host, &to.path, body),
            )
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
        assert_eq!(
            parsed["platform"].as_str(),
            Some(std::env::consts::OS),
            "platform is advisory and unsigned — it decorates the devices screen — but it \
             still has to be present, or every enrolled machine renders as platform unknown"
        );
        assert_eq!(
            parsed.get("deviceKind"),
            None,
            "deviceKind's values are browser/phone/desktop and the Worker 400s anything \
             else, so a daemon that names itself here is refusing its own enrolment"
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
    fn a_refusal_carries_the_workers_detail_when_one_is_sent() {
        // #367's server half names the cause beside the error; this is the
        // client half picking it up. `detail` is what turns "already_enrolled"
        // from a dead end into a next move (#368).
        let store = MemoryKeyStore::new();
        let http = FakeControlPlane::answering(
            409,
            r#"{"error":"already_enrolled","detail":"revoked"}"#,
        );
        let err = enroll(&identity(), "C", "l", "https://example", &http, &store)
            .expect_err("a 409 is not an enrolment");
        let EnrollError::Refused { status, message, detail } = err else {
            panic!("expected a refusal, got {err}");
        };
        assert_eq!(status, 409);
        assert_eq!(message, "already_enrolled");
        assert_eq!(
            detail.as_deref(),
            Some("revoked"),
            "the detail is the difference between 'restore it' and 'wrong account'"
        );
    }

    #[test]
    fn a_refusal_without_a_detail_is_the_old_shape_unchanged() {
        // The deploy-skew pin: a Worker predating #367 sends no `detail`, and
        // this daemon may be newer than the deployment it talks to. Absent
        // must be `None` and the rendered error must keep carrying the
        // control plane's own word.
        let store = MemoryKeyStore::new();
        let http = FakeControlPlane::answering(409, r#"{"error":"already_enrolled"}"#);
        let err = enroll(&identity(), "C", "l", "https://example", &http, &store)
            .expect_err("still a refusal");
        assert!(
            matches!(err, EnrollError::Refused { ref detail, .. } if detail.is_none()),
            "got {err}"
        );
        assert!(err.to_string().contains("already_enrolled"), "got {err}");
    }

    #[test]
    fn refusal_text_is_the_persons_next_move() {
        let refused = |message: &str, detail: Option<&str>| EnrollError::Refused {
            status: 409,
            message: message.into(),
            detail: detail.map(String::from),
        };

        assert!(
            refusal_text(&refused("wrong_kind", None)).contains("Add a machine"),
            "a device code fed to --enroll: the fresh-code advice mints the same wrong kind"
        );
        let revoked = refusal_text(&refused("already_enrolled", Some("revoked")));
        assert!(
            revoked.contains("revoked") && revoked.contains("restore"),
            "a revoked machine's only way back is the owner restoring it; got {revoked:?}"
        );
        assert!(
            refusal_text(&refused("already_enrolled", Some("other_account")))
                .contains("different account"),
            "restoring cannot help a key that lives on another account"
        );
        let bare = refusal_text(&refused("already_enrolled", None));
        assert!(
            bare.contains("restore"),
            "an old Worker names no cause, but 'mint a fresh code' is still never the fix \
             for already_enrolled; got {bare:?}"
        );
        assert!(
            refusal_text(&refused("invalid_code", None)).contains("refused this enrolment"),
            "every other refusal keeps the Display phrasing, which carries the status and word"
        );
        assert_eq!(
            refusal_text(&EnrollError::Transport("no route".into())),
            EnrollError::Transport("no route".into()).to_string(),
            "a transport failure is not a refusal and keeps its own words"
        );
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
    fn a_real_response_read_off_the_wire_enrols_this_machine() {
        // End to end through the code that will run in production: the URL is
        // split by `Endpoint`, the request is written and the response parsed
        // by `zest_cloud::http`, and only the socket is missing.
        let store = MemoryKeyStore::new();
        let http = CannedHttp::answering(
            "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\n",
            r#"{"token":"zt_abc","account":"andy"}"#,
        );

        let enrolled = enroll(
            &identity(),
            "ABCD1234",
            "andy-mac",
            DEFAULT_CONTROL_PLANE,
            &http,
            &store,
        )
        .expect("a 200 carrying a token is an enrolment");

        assert_eq!(enrolled.account.as_deref(), Some("andy"));
        assert_eq!(token_in(&store).as_deref(), Some("zt_abc"));
        let request = http.request();
        assert!(
            request.starts_with(&format!("POST {ENROLL_PATH} HTTP/1.1\r\n")),
            "the configured base URL has to reduce to this request target, or every claim \
             404s and a 404 here reads as an expired code; got {request:?}"
        );
        assert!(
            request.contains("\r\ncontent-type: application/json\r\n")
                && request.contains(r#""hostId":"#),
            "the Worker routes on the content type and verifies the signed body it finds; \
             got {request:?}"
        );
    }

    #[test]
    fn a_refusal_that_arrives_as_http_is_refused_and_not_a_transport_failure() {
        // The classification `received` makes, against real bytes rather than
        // against a `Response` a fake handed over. A completed request that
        // said no must never be reported as a request that did not complete:
        // one tells a person to try again, the other tells them why.
        let store = MemoryKeyStore::new();
        let http = CannedHttp::answering(
            "HTTP/1.1 403 Forbidden\r\n",
            r#"{"error":"that code has been used"}"#,
        );

        let err = enroll(&identity(), "ABCD1234", "andy-mac", DEFAULT_CONTROL_PLANE, &http, &store)
            .expect_err("a 403 is not an enrolment");

        assert!(matches!(err, EnrollError::Refused { status: 403, .. }), "got {err}");
        assert!(err.to_string().contains("that code has been used"), "got {err}");
        assert!(token_in(&store).is_none());
    }

    #[test]
    fn a_response_that_is_not_json_reports_what_arrived() {
        // The shape a proxy or an error page produces: a status that says yes
        // and a body that is HTML. Naming it is the whole value — "the control
        // plane refused this" with no reason is the least actionable error
        // there is, and this is not a refusal at all.
        let store = MemoryKeyStore::new();
        let http = CannedHttp::answering(
            "HTTP/1.1 200 OK\r\ncontent-type: text/html\r\n",
            "<html><body>Cloudflare</body></html>",
        );

        let err = enroll(&identity(), "C", "l", DEFAULT_CONTROL_PLANE, &http, &store)
            .expect_err("a page is not an enrolment");

        assert!(matches!(err, EnrollError::BadResponse(_)), "got {err}");
        assert!(
            err.to_string().contains("<html>"),
            "the body has to reach the person, or a proxy in the way is indistinguishable \
             from a control plane that answered nonsense; got {err}"
        );
        assert!(token_in(&store).is_none());
    }

    #[test]
    fn a_peer_that_answers_nothing_is_a_transport_failure_naming_the_url() {
        let store = MemoryKeyStore::new();
        let http = CannedHttp::silent();
        let err = enroll(&identity(), "C", "l", DEFAULT_CONTROL_PLANE, &http, &store)
            .expect_err("a connection that said nothing did not enrol anything");
        assert!(
            matches!(err, EnrollError::Transport(_)),
            "nothing was refused, because nothing answered; got {err}"
        );
        assert!(
            err.to_string().contains(DEFAULT_CONTROL_PLANE),
            "the URL is what a person checks first when nothing answers; got {err}"
        );
        assert!(token_in(&store).is_none());
    }

    #[test]
    fn a_url_the_client_cannot_request_fails_before_anything_is_dialled() {
        // The real `HttpsControlPlane`, and the one thing it can be asked that
        // needs no network: a `--control-plane` that is not an https URL is
        // refused by `Endpoint::parse` before `TlsDuplex::connect` is reached.
        // Posting a bearer token over plaintext is the failure that would
        // otherwise work, which is why it is refused rather than upgraded.
        let store = MemoryKeyStore::new();
        let err = enroll(
            &identity(),
            "C",
            "l",
            "http://zesterm.example",
            &HttpsControlPlane::new(Roots::Platform),
            &store,
        )
        .expect_err("plaintext must not carry an enrolment");

        assert!(matches!(err, EnrollError::Transport(_)), "got {err}");
        assert!(
            err.to_string().contains("https"),
            "the error has to name the scheme, or it reads as the control plane being down; \
             got {err}"
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
