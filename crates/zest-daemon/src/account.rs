//! The account's own client: the fleet a person's token can see, and the leg
//! that reaches an enrolled machine through the relay.
//!
//! Deliberately parallel to [`crate::enroll`] rather than a call into it: that
//! module enrols a *machine* (its `HostIdentity`, role `host`, token under
//! `cloud-token`), while a person's device holds its own key and token under
//! `app-cloud-token`. Same wire, different principals — the split the
//! keystore's doc comment on [`APP_CLOUD_TOKEN_NAME`] exists to defend.
//!
//! # Why it is here and not in `zest-app`
//!
//! It was in `zest-app`, which is a `[[bin]]`-only crate — so nothing outside
//! the window could call [`fetch_hosts`] or [`relay_dialer`], and the account
//! half of the fleet was structurally invisible to every other client. That is
//! the same shape #398 found `best_route` in and for the same reason: the
//! decision had a second consumer that could not reach it. `zest-mcp` is that
//! consumer here (#274, ADR-015).
//!
//! `zest-daemon` rather than a crate of its own because it already carries
//! `zest-cloud`, `zest-mesh` and `zest-proto`, already owns the *machine's*
//! control-plane client next door, and is already the crate every client links
//! for [`crate::DaemonClient`]. The move adds no dependency edge anywhere.
//!
//! The transport traits are seams for the reason [`crate::enroll::ControlPlane`]
//! is one: every test below drives a real account read, or a real relay dial's
//! sequencing, without a socket.

use crate::enroll::{clip, refusal_from, EnrollError};
use zest_cloud::http::Endpoint;
use zest_cloud::tls::Roots;
use zest_mesh::keystore::{SecretStore, APP_CLOUD_TOKEN_NAME};
use zest_proto::{ClientId, HostId};

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
pub enum CloudError {
    #[error("the account no longer accepts this device's token")]
    SignedOut,
    /// A 401 that named its cause (#371). Its own variant beside `SignedOut`
    /// rather than a field on it, so every existing `SignedOut` match keeps
    /// its exact meaning: a bare 401 — and a detail this build does not know
    /// (deploy skew) — still reads as it always did.
    #[error("the account refused this device's credential: {0}")]
    Refused(MachineRefusal),
    #[error("could not reach the control plane: {0}")]
    Transport(String),
    #[error("the control plane's answer was not usable: {0}")]
    BadAnswer(String),
}

/// Why the control plane refused this machine's credential, when its 401
/// said (#371). Each demands a different act, which is the whole point of
/// carrying it: `Revoked` → restore on the fleet screen (or sign in again),
/// `Pending` → wait for approval, `Expired` → sign in again.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MachineRefusal {
    Revoked,
    Pending,
    Expired,
}

impl std::fmt::Display for MachineRefusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            MachineRefusal::Revoked => "revoked",
            MachineRefusal::Pending => "pending approval",
            MachineRefusal::Expired => "expired",
        })
    }
}

/// A 401's meaning, read from its body. Bare — or carrying a detail this
/// build does not know — is `SignedOut`, the deploy-skew pin: an older Worker
/// sends no detail, and a newer one may send words this binary predates.
fn refusal_401(body: &str) -> CloudError {
    #[derive(serde::Deserialize)]
    struct Refusal {
        detail: Option<String>,
    }
    match serde_json::from_str::<Refusal>(body)
        .ok()
        .and_then(|r| r.detail)
        .as_deref()
    {
        Some("revoked") => CloudError::Refused(MachineRefusal::Revoked),
        Some("pending") => CloudError::Refused(MachineRefusal::Pending),
        Some("expired") => CloudError::Refused(MachineRefusal::Expired),
        _ => CloudError::SignedOut,
    }
}

/// The two authenticated requests the app makes.
///
/// A trait for the reason `ControlPlane` is one: the parsing and the 401
/// classification below are the parts that go wrong silently, and none of
/// them needs a socket to be got right.
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
pub struct HttpsAccountApi {
    base: Endpoint,
    roots: Roots,
}

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
pub struct AccountHost {
    pub host: HostId,
    pub label: String,
    /// When the relay last saw it, epoch milliseconds; `None` when it has
    /// never dialled in.
    #[allow(dead_code, reason = "the fleet card's `last seen` row is the consumer, later in #190's arc")]
    pub last_seen_ms: Option<u64>,
    /// Whether the relay can reach this machine **right now** — its control
    /// link is parked and was proved alive within the control plane's own
    /// bound.
    ///
    /// A boolean off the wire rather than a timestamp we age ourselves: how
    /// stale a parked link may be before it stops counting is the relay's
    /// refresh cadence, and a client that re-derived it would eventually
    /// disagree with the server about what online means. Absent — an older
    /// control plane — reads as `false`, which degrades to exactly the
    /// behaviour that shipped before #237. (`online` in `PublicHost`.)
    pub relay_online: bool,
}

/// What `GET /api/hosts` answers: the fleet as the account knows it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AccountHosts {
    /// The account's display name, when the answer carries one.
    #[allow(dead_code, reason = "the fleet header shows it once the watcher threads it through")]
    pub account: Option<String>,
    /// Where the relay lives; `None` on a deployment without one, in which
    /// case the hosts are listed but unreachable through the account.
    pub relay_origin: Option<String>,
    pub hosts: Vec<AccountHost>,
}

/// The account's host list, or why it could not be read.
pub fn fetch_hosts(api: &dyn AccountApi, token: &str) -> Result<AccountHosts, CloudError> {
    let got = api
        .get("/api/hosts", token)
        .map_err(|e| CloudError::Transport(e.to_string()))?;
    if got.status == 401 {
        return Err(refusal_401(&got.body));
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
        #[serde(default)]
        online: bool,
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
            .map(|r| AccountHost {
                host: r.id,
                label: r.label,
                last_seen_ms: r.last_seen_at,
                relay_online: r.online,
            })
            .collect(),
    })
}

/// An attach ticket for `host`, or why the relay would not admit us.
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
        return Err(refusal_401(&got.body));
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

/// Mint a host enrol code with the app's own token (issue #227).
///
/// The policy this leans on is the Worker's: an **approved device** bearer
/// may mint `host` codes — adding machines is the enroll button's whole
/// point — and nothing else; minting *device* codes stays a signed-in
/// person's act, so a leaked app token cannot manufacture credentials for
/// further devices.
pub fn mint_host_code(api: &dyn AccountApi, token: &str) -> Result<String, CloudError> {
    let got = api
        .post("/api/enroll/code", token, r#"{"kind":"host"}"#)
        .map_err(|e| CloudError::Transport(e.to_string()))?;
    if got.status == 401 {
        return Err(refusal_401(&got.body));
    }
    if !(200..300).contains(&got.status) {
        return Err(CloudError::BadAnswer(format!("{}: {}", got.status, clip(&got.body))));
    }

    #[derive(serde::Deserialize)]
    struct Answer {
        code: Option<String>,
    }
    let answer: Answer = serde_json::from_str(&got.body)
        .map_err(|e| CloudError::BadAnswer(format!("{e}; body was {:?}", clip(&got.body))))?;
    answer
        .code
        .filter(|c| !c.is_empty())
        .ok_or_else(|| CloudError::BadAnswer(format!("no code in {:?}", clip(&got.body))))
}

/// One device the account lists — the fleet screen's devices section.
///
/// `kind` and `status` stay strings on purpose: their variants are the
/// control plane's to grow (`browser|phone|desktop`, `pending|approved`
/// today), and an enum here would turn a new kind into a parse failure that
/// takes the whole listing with it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AccountDevice {
    pub id: ClientId,
    pub label: String,
    pub kind: String,
    pub status: String,
    /// The key can be read by script on its origin — a browser on the
    /// fallback path. Rendered, never decided on.
    #[allow(dead_code, reason = "the devices section's seed-backed marker is later polish")]
    pub extractable: bool,
}

impl AccountDevice {
    /// The one status the control plane treats as trusted.
    #[must_use]
    pub fn approved(&self) -> bool {
        self.status == "approved"
    }
}

/// The account's device list, or why it could not be read.
pub fn fetch_devices(
    api: &dyn AccountApi,
    token: &str,
) -> Result<Vec<AccountDevice>, CloudError> {
    let got = api
        .get("/api/devices", token)
        .map_err(|e| CloudError::Transport(e.to_string()))?;
    if got.status == 401 {
        return Err(refusal_401(&got.body));
    }
    if !(200..300).contains(&got.status) {
        return Err(CloudError::BadAnswer(format!("{}: {}", got.status, clip(&got.body))));
    }

    // Permissive like the hosts parse: unknown fields are the control
    // plane's future, and only what the section renders is required.
    #[derive(serde::Deserialize)]
    struct Row {
        id: ClientId,
        label: String,
        kind: String,
        status: String,
        #[serde(default)]
        extractable: bool,
    }
    #[derive(serde::Deserialize)]
    struct Answer {
        #[serde(default)]
        devices: Vec<Row>,
    }
    let answer: Answer = serde_json::from_str(&got.body)
        .map_err(|e| CloudError::BadAnswer(format!("{e}; body was {:?}", clip(&got.body))))?;
    Ok(answer
        .devices
        .into_iter()
        .map(|d| AccountDevice {
            id: d.id,
            label: d.label,
            kind: d.kind,
            status: d.status,
            extractable: d.extractable,
        })
        .collect())
}

/// Whose account this token speaks for — the `userId` an attestation must
/// name inside its signed bytes.
///
/// Asked per approval rather than persisted: the app stores only the token
/// (PR #210 deliberately kept nothing else), and `/api/me` answers a bearer
/// with `principal.userId` for exactly this caller.
pub fn fetch_me(api: &dyn AccountApi, token: &str) -> Result<String, CloudError> {
    let got =
        api.get("/api/me", token).map_err(|e| CloudError::Transport(e.to_string()))?;
    if got.status == 401 {
        return Err(refusal_401(&got.body));
    }
    if !(200..300).contains(&got.status) {
        return Err(CloudError::BadAnswer(format!("{}: {}", got.status, clip(&got.body))));
    }

    #[derive(serde::Deserialize)]
    struct Principal {
        #[serde(rename = "userId")]
        user_id: Option<String>,
    }
    #[derive(serde::Deserialize)]
    struct Answer {
        principal: Option<Principal>,
    }
    let answer: Answer = serde_json::from_str(&got.body)
        .map_err(|e| CloudError::BadAnswer(format!("{e}; body was {:?}", clip(&got.body))))?;
    answer
        .principal
        .and_then(|p| p.user_id)
        .filter(|u| !u.is_empty())
        // `/api/me` answers `{user: null}` with a 200 for an unknown
        // credential; that is "signed out" in a 200's clothing, not a
        // malformed answer.
        .ok_or(CloudError::SignedOut)
}

/// Submit a signed attestation for `device` — Approve on a pending row,
/// Vouch on an approved one; the route is the same statement either way.
pub fn approve_device(
    api: &dyn AccountApi,
    token: &str,
    device: ClientId,
    blob: &str,
) -> Result<(), CloudError> {
    let body = serde_json::json!({ "attestation": blob }).to_string();
    let path = format!("/api/devices/{}/approve", hex(&device.0));
    let got = api
        .post(&path, token, &body)
        .map_err(|e| CloudError::Transport(e.to_string()))?;
    if got.status == 401 {
        return Err(refusal_401(&got.body));
    }
    if !(200..300).contains(&got.status) {
        // The refusal's own word (`bad_signature`, `forbidden`, a named
        // field) is the only actionable part; the status alone reads as an
        // outage.
        return Err(CloudError::BadAnswer(message_from(&got.body)));
    }
    Ok(())
}

/// The relay's attach subprotocol, and the path an attach dials.
///
/// Pinned as literals because the other ends are TypeScript
/// (`cloud/packages/relay/src/ticket.ts` and `routes.ts`) and nothing
/// compiles both — the same argument as `ENROLL_PATH`'s.
pub const RELAY_SUBPROTOCOL: &str = "zesterm.relay.v1";
const RELAY_ATTACH_PATH: &str = "/v1/attach";

/// Why a relay dial produced no leg.
///
/// Two variants because the caller's redial loop must tell them apart, and
/// that classification is the load-bearing half of [`relay_dial`]: a refused
/// or absent token can never succeed on a retry, so a supervisor has to
/// **stop** rather than back off against guaranteed 401s, while anything else
/// is worth trying again. It was `zest-app`'s `RemoteError` when this lived
/// there; a shared home cannot name one consumer's error type, and collapsing
/// the two into a string is exactly the flattening `RemoteError`'s own doc
/// warns turns "not paired" into an infinite reconnect.
#[derive(Debug, Clone, thiserror::Error)]
pub enum RelayDialError {
    /// The credential cannot open a leg, and no retry will change that until a
    /// person acts.
    ///
    /// **Which** act differs, which is why the reason is carried rather than
    /// collapsed: `MachineRefusal`'s own doc makes the point — revoked wants a
    /// restore, pending wants waiting, expired and absent want a sign-in — and
    /// a single "signed out, sign in again" would point three of the four at
    /// the wrong remedy.
    #[error("{0}")]
    Credential(CredentialRefusal),
    #[error("{0}")]
    Io(String),
}

/// Why this device's credential could not open a relay leg.
///
/// [`MachineRefusal`] plus the case that never reached the control plane at
/// all: there was no token to send. Same four acts, one enum, so a caller
/// matching on it cannot meet a state with no sentence.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum CredentialRefusal {
    #[error("this machine is signed out - sign in again")]
    SignedOut,
    #[error("this machine's access was revoked - restore it, or sign in again")]
    Revoked,
    #[error("this machine is still waiting to be approved")]
    Pending,
    #[error("this machine's credential has expired - sign in again")]
    Expired,
}

impl From<MachineRefusal> for CredentialRefusal {
    fn from(r: MachineRefusal) -> Self {
        match r {
            MachineRefusal::Revoked => Self::Revoked,
            MachineRefusal::Pending => Self::Pending,
            MachineRefusal::Expired => Self::Expired,
        }
    }
}

/// One relay dial: a fresh ticket, a leg to the relay, the WS upgrade —
/// halves the ordinary encrypted daemon handshake then runs through
/// unchanged (the relay's pipe leg hands the same WS halves straight to the
/// daemon's `serve_lan`, so the host still challenges and authorizes; the
/// relay never sees plaintext).
///
/// `mint` and `connect` are injected for the reason `ControlPlane` is: the
/// sequencing here — SignedOut stops *before* a socket is opened, a
/// transient mint failure stays retryable, every dial mints anew because
/// tickets are 30-second single-use — is what goes wrong silently, and none
/// of it needs a network to be got right. The two-offer wire bytes are
/// `ws::client`'s own tests' business.
pub fn relay_dial(
    host: HostId,
    host_header: &str,
    mint: &dyn Fn() -> Result<String, CloudError>,
    connect: &dyn Fn() -> std::io::Result<RelayLeg>,
) -> Result<DialHalves, RelayDialError> {
    // Mint before dialling: a refused mint must cost no socket, and a
    // signed-out app must stop the supervisor rather than back off against
    // guaranteed 401s.
    let ticket = mint().map_err(|e| match e {
        // A named refusal is terminal exactly as a bare one: a revoked or
        // pending machine's dial must stop, not back off against guaranteed
        // 401s. The header's honest wording is the account watcher's job.
        CloudError::SignedOut => RelayDialError::Credential(CredentialRefusal::SignedOut),
        CloudError::Refused(r) => RelayDialError::Credential(r.into()),
        // Transient by classification: `Io` is the shape the redial loop
        // backs off on, which is right for an unreachable control plane.
        other => RelayDialError::Io(other.to_string()),
    })?;
    let leg = connect().map_err(|e| RelayDialError::Io(e.to_string()))?;

    let path = format!("{RELAY_ATTACH_PATH}?host={}", hex(&host.0));
    let offer = format!("ticket.{ticket}");
    let (reader, writer) = crate::ws::client::connect_to_offering(
        leg.reader,
        leg.writer,
        host_header,
        &path,
        &[],
        // Protocol first, ticket second — the relay splits the one header
        // on commas and takes whichever entry wears the ticket prefix.
        &[RELAY_SUBPROTOCOL, &offer],
        RELAY_SUBPROTOCOL,
    )
    .map_err(|e| RelayDialError::Io(format!("relay upgrade: {e}")))?;
    Ok((Box::new(reader), Box::new(writer)))
}

/// What a dial hands the supervisor — `remote::Dialer`'s own success shape.
pub type DialHalves = (Box<dyn std::io::Read + Send>, Box<dyn std::io::Write + Send>);

/// The raw byte legs a relay dial upgrades — what `connect` produces.
pub struct RelayLeg {
    pub reader: Box<dyn std::io::Read + Send>,
    pub writer: Box<dyn std::io::Write + Send>,
}

/// The whole ladder, from a host id to two byte legs: token from the
/// credential store, ticket from the control plane, TLS to the relay.
///
/// **One copy, deliberately.** Every step here is a place to be subtly wrong
/// — a token captured once instead of read per dial, a ticket reused past its
/// 30 seconds, the plaintext dialler reached for a non-loopback origin — and
/// the second consumer arriving (#274) is exactly when a near-duplicate on a
/// security-sensitive path would have been written. Both callers pass through
/// this function or neither is right.
///
/// Runs the whole thing **fresh per dial, including per redial**: tickets are
/// single-use, and reading the token each time is what makes a signed-out
/// caller stop at the next redial with [`RelayDialError::SignedOut`] rather
/// than ride a credential captured when a person clicked something. It blocks
/// on a keychain read and two network round trips, so it belongs on a worker
/// thread and never on an event loop.
pub fn relay_dialer(
    host: HostId,
    relay_origin: &str,
    control_plane: &str,
    roots: Roots,
    secrets: &dyn SecretStore,
) -> Result<DialHalves, RelayDialError> {
    let mint = || {
        let token = stored_app_token(secrets)
            .map_err(|e| match e {
                // The store answered, and what it holds is not a token -- a
                // key filed under this name, or a corrupt entry. Terminal:
                // every retry re-reads the same bytes, so classifying it as
                // transport would spin the caller's redial loop for ever
                // against something only a fresh sign-in can fix.
                EnrollError::BadResponse(_) => CloudError::SignedOut,
                // Everything else is the store itself failing, and a store
                // that could not be read is not a store with no key: a locked
                // keychain is the ordinary case here, and it unlocks.
                other => CloudError::Transport(other.to_string()),
            })?
            .ok_or(CloudError::SignedOut)?;
        let api = HttpsAccountApi::new(control_plane, roots)
            .map_err(|e| CloudError::Transport(e.to_string()))?;
        mint_ticket(&api, &token, host)
    };
    // The daemon's own relay diallers, reused whole: the TLS one's read poll
    // arrangement is the trap `zest_cloud::tls::READ_POLL` documents, and the
    // plaintext one is loopback-only by `RelayOrigin::parse`'s rule — a
    // `wrangler dev` relay for the edit-run loop. No `cut` is armed: this
    // returns a link whose lifecycle its caller owns through read errors, and
    // there is no handshake watchdog here to arm one from.
    let parsed = crate::relay::RelayOrigin::parse(relay_origin)
        .map_err(|e| RelayDialError::Io(e.to_string()))?;
    let connect = || {
        let dial = if parsed.tls {
            crate::relay::tls_dialler(roots)
        } else {
            crate::relay::plaintext_dialler()
        };
        let wire = dial(&parsed.host, parsed.port)?;
        Ok(RelayLeg { reader: wire.reader, writer: wire.writer })
    };
    relay_dial(host, &parsed.host_header(), &mint, &connect)
}

/// Lowercase hex, the spelling every id on this wire uses.
fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// The sentence alone, for the flows that keep no detail.
fn message_from(body: &str) -> String {
    refusal_from(body).0
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use zest_mesh::keystore::{MemoryKeyStore, CLOUD_TOKEN_NAME};

    struct FakeAccountApi {
        status: u16,
        body: String,
        calls: RefCell<Vec<(String, String)>>,
        /// What each POST carried — the approve tests assert the blob
        /// reaches the wire verbatim.
        posted: RefCell<Vec<String>>,
    }

    impl FakeAccountApi {
        fn answering(status: u16, body: &str) -> Self {
            Self {
                status,
                body: body.to_string(),
                calls: RefCell::new(Vec::new()),
                posted: RefCell::new(Vec::new()),
            }
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
            body: &str,
        ) -> std::io::Result<zest_cloud::http::Response> {
            self.calls.borrow_mut().push((path.to_string(), bearer.to_string()));
            self.posted.borrow_mut().push(body.to_string());
            Ok(zest_cloud::http::Response { status: self.status, body: self.body.clone() })
        }
    }

    /// A store that is present and unreadable — a locked keychain.

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
    fn a_401_that_names_its_cause_reaches_the_header_as_that_cause() {
        // #371's client half. The three details each demand a different act
        // (restore / wait / sign in again), so they must survive the parse —
        // and everything else must degrade to the old meaning.
        for (detail, want) in [
            ("revoked", MachineRefusal::Revoked),
            ("pending", MachineRefusal::Pending),
            ("expired", MachineRefusal::Expired),
        ] {
            let api = FakeAccountApi::answering(
                401,
                &format!(r#"{{"error":"unauthorized","detail":"{detail}"}}"#),
            );
            assert!(
                matches!(fetch_hosts(&api, "zt1_x"), Err(CloudError::Refused(w)) if w == want),
                "{detail} must arrive as {want:?}"
            );
        }

        // The deploy-skew pins, both directions: a Worker predating #371
        // sends no detail, and a future one may send a word this binary does
        // not know. Both are the bare 401's old meaning, never an error.
        for body in [
            r#"{"error":"unauthorized"}"#,
            r#"{"error":"unauthorized","detail":"quarantined"}"#,
            "not json at all",
        ] {
            let api = FakeAccountApi::answering(401, body);
            assert!(
                matches!(fetch_hosts(&api, "zt1_x"), Err(CloudError::SignedOut)),
                "{body:?} must keep the old meaning"
            );
        }
    }

    #[test]
    fn the_relay_online_flag_is_read_and_an_older_control_plane_reads_false() {
        // #237's wire half. `online` is a *verdict* the control plane reaches
        // against its own bound, so the app reads it rather than re-deriving
        // it from a timestamp — two implementations of "how stale is too
        // stale" is two answers that eventually differ.
        let api = FakeAccountApi::answering(
            200,
            &format!(
                r#"{{"hosts":[{{"id":"{}","label":"win","online":true}},{{"id":"{}","label":"attic","online":false}}]}}"#,
                hex(&[0x22; 32]),
                hex(&[0x23; 32])
            ),
        );
        let got = fetch_hosts(&api, "zt1_x").expect("a listing parses");
        assert!(
            got.hosts[0].relay_online,
            "the machine whose control link is parked is the one the fleet screen must \
             stop calling asleep"
        );
        assert!(!got.hosts[1].relay_online, "and an enrolled machine that is off is not");

        // A Worker deployed before 0007 sends no `online` at all. Reading that
        // as false degrades to exactly the behaviour that shipped before this
        // change, rather than to a parse error that empties the fleet screen.
        let older = FakeAccountApi::answering(
            200,
            &format!(r#"{{"hosts":[{{"id":"{}","label":"win"}}]}}"#, hex(&[0x22; 32])),
        );
        let got = fetch_hosts(&older, "zt1_x").expect("an older control plane still parses");
        assert!(!got.hosts[0].relay_online, "absent is not online");
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
    fn relay_dial_signed_out_stops_before_any_socket() {
        use std::cell::Cell;
        // The order is the point: a signed-out app must stop the supervisor
        // (RelayDialError::SignedOut is its Refused), and it must do so having
        // opened nothing — a socket per doomed redial would knock on the
        // relay forever about a token that cannot mint.
        let connected = Cell::new(false);
        // A `let..else` rather than `expect_err`: the Ok halves are boxed
        // readers with no Debug to print.
        let Err(err) = relay_dial(
            HostId::from_bytes([9; 32]),
            "relay.example",
            &|| Err(CloudError::SignedOut),
            &|| {
                connected.set(true);
                Err(std::io::Error::other("must not be reached"))
            },
        ) else {
            panic!("no ticket, no dial")
        };
        assert!(
            matches!(err, RelayDialError::Credential(CredentialRefusal::SignedOut)),
            "`Credential` is the variant the supervisor stops on; got {err:?}"
        );
        assert!(!connected.get(), "a refused mint must cost no socket");
    }

    #[test]
    fn a_named_refusal_keeps_its_own_name_and_its_own_remedy() {
        // Terminal like a bare sign-out, and *not* the same sentence: revoked
        // wants a restore, pending wants waiting, expired wants a sign-in. One
        // collapsed "sign in again" would point two of the three at the wrong
        // act, which is the whole reason `MachineRefusal` exists.
        for (refusal, expect, word) in [
            (MachineRefusal::Revoked, CredentialRefusal::Revoked, "revoked"),
            (MachineRefusal::Pending, CredentialRefusal::Pending, "approved"),
            (MachineRefusal::Expired, CredentialRefusal::Expired, "expired"),
        ] {
            let Err(err) = relay_dial(
                HostId::from_bytes([9; 32]),
                "relay.example",
                &|| Err(CloudError::Refused(refusal)),
                &|| panic!("a refused mint must cost no socket"),
            ) else {
                panic!("no ticket, no dial")
            };
            assert!(matches!(err, RelayDialError::Credential(r) if r == expect), "{err:?}");
            assert!(
                err.to_string().contains(word),
                "the sentence names this refusal's own remedy: {err}"
            );
        }
    }

    #[test]
    fn a_token_that_is_not_text_is_terminal_rather_than_retried_for_ever() {
        // A key filed under the token's name, or a corrupt entry. Every retry
        // re-reads the same bytes, so classifying it as transport would spin a
        // redial loop against something only a fresh sign-in can fix -- while
        // a store that merely could not be *read* (a locked keychain) must stay
        // retryable, because that one unlocks.
        let store = MemoryKeyStore::new();
        store.store_secret(APP_CLOUD_TOKEN_NAME, &[0xff, 0xfe]).expect("store");
        let Err(err) = relay_dialer(
            HostId::from_bytes([9; 32]),
            "http://127.0.0.1:1",
            "https://control.example",
            zest_cloud::tls::Roots::Bundled,
            &store,
        ) else {
            panic!("bytes that are not a token cannot mint a ticket")
        };
        assert!(
            matches!(err, RelayDialError::Credential(CredentialRefusal::SignedOut)),
            "a corrupt credential is terminal, not a network blip; got {err:?}"
        );
    }

    #[test]
    fn relay_dial_transient_mint_failure_stays_retryable() {
        let Err(err) = relay_dial(
            HostId::from_bytes([9; 32]),
            "relay.example",
            &|| Err(CloudError::Transport("no route to host".into())),
            &|| Err(std::io::Error::other("must not be reached")),
        ) else {
            panic!("nothing to upgrade")
        };
        assert!(
            matches!(err, RelayDialError::Io(_)),
            "Io is the shape the redial loop backs off and retries on — mapping a wifi \
             blip to SignedOut would permanently kill a session over a hiccup; got {err:?}"
        );
    }

    /// A writer that shares its capture, and a reader that ends at once: the
    /// upgrade's request reaches the capture and then fails on EOF — which
    /// is what these tests want, since the wire bytes are the assertion and
    /// no fake relay needs to answer for them.
    struct Capture(std::sync::Arc<std::sync::Mutex<Vec<u8>>>);
    impl std::io::Write for Capture {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.0.lock().expect("lock").extend_from_slice(bytes);
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    struct Eof;
    impl std::io::Read for Eof {
        fn read(&mut self, _out: &mut [u8]) -> std::io::Result<usize> {
            Ok(0)
        }
    }

    #[test]
    fn every_relay_dial_mints_a_fresh_ticket_and_the_request_carries_it() {
        use std::cell::Cell;
        // Tickets are 30-second single-use: a dialler that cached one would
        // pass its first dial and fail every redial with a refusal that
        // reads as an outage. Two dials, two mints, each ticket on its own
        // request — counted, not timed.
        let mints = Cell::new(0u32);
        let sent = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let mint = || {
            mints.set(mints.get() + 1);
            Ok(format!("T{}", mints.get()))
        };
        let sent_for_leg = std::sync::Arc::clone(&sent);
        let connect = move || {
            Ok(RelayLeg {
                reader: Box::new(Eof) as Box<dyn std::io::Read + Send>,
                writer: Box::new(Capture(std::sync::Arc::clone(&sent_for_leg))),
            })
        };

        let host = HostId::from_bytes([0xab; 32]);
        for expected in ["T1", "T2"] {
            sent.lock().expect("lock").clear();
            // The dial itself fails on the EOF leg; the request it wrote is
            // the assertion.
            let _ = relay_dial(host, "relay.example", &mint, &connect);
            let request =
                String::from_utf8(sent.lock().expect("lock").clone()).expect("utf-8");
            assert!(
                request.starts_with(&format!("GET /v1/attach?host={} HTTP/1.1\r\n", hex(&host.0))),
                "the attach path names the host in hex — the relay routes rooms on it; \
                 got {request:?}"
            );
            assert!(
                request.contains(&format!(
                    "\r\nSec-WebSocket-Protocol: zesterm.relay.v1, ticket.{expected}\r\n"
                )),
                "dial after dial, the ticket on the wire must be the one just minted; \
                 got {request:?}"
            );
        }
        assert_eq!(mints.get(), 2, "two dials are two mints, never a cached ticket");
    }

    #[test]
    fn a_device_listing_parses_tolerantly_and_names_its_statuses() {
        // Unknown fields and a missing `extractable` are the control plane's
        // future; a listing that breaks on them freezes the schema from the
        // wrong end — the hosts parse's rule, applied to devices.
        let api = FakeAccountApi::answering(
            200,
            &format!(
                r#"{{"devices":[
                    {{"id":"{}","label":"work-browser","kind":"browser","status":"pending","extractable":true,"enrolledAt":1,"future":1}},
                    {{"id":"{}","label":"studio-app","kind":"desktop","status":"approved"}}
                ],"unknownTop":true}}"#,
                hex(&[0x31; 32]),
                hex(&[0x32; 32]),
            ),
        );
        let got = fetch_devices(&api, "zt1_x").expect("a listing with extras still parses");
        assert_eq!(got.len(), 2);
        assert_eq!(got[0].label, "work-browser");
        assert!(!got[0].approved(), "pending is the state the Approve button exists for");
        assert!(got[0].extractable, "seed-backed keys are named as such");
        assert!(got[1].approved());
        assert!(!got[1].extractable, "absent means no claim, defaulted false");
        assert_eq!(
            api.calls.borrow()[0],
            ("/api/devices".to_string(), "zt1_x".to_string()),
            "the route and the bearer are the request"
        );
    }

    #[test]
    fn every_account_read_maps_a_401_to_signed_out() {
        // One refusal, one meaning, on all three reads the approver flow
        // makes: a revoked token asks for sign-in, never for a retry.
        let api = FakeAccountApi::answering(401, r#"{"error":"unauthorized"}"#);
        assert!(matches!(fetch_devices(&api, "zt1_x"), Err(CloudError::SignedOut)));
        assert!(matches!(fetch_me(&api, "zt1_x"), Err(CloudError::SignedOut)));
        assert!(matches!(
            approve_device(&api, "zt1_x", ClientId::from_bytes([7; 32]), "m.s"),
            Err(CloudError::SignedOut)
        ));
    }

    #[test]
    fn me_answers_the_user_id_and_a_null_user_is_signed_out() {
        // The bearer shape: `user` present-but-null, the principal named.
        let api = FakeAccountApi::answering(
            200,
            r#"{"user":null,"principal":{"kind":"device","id":"abc","userId":"user_1234"}}"#,
        );
        assert_eq!(fetch_me(&api, "zt1_x").expect("a principal"), "user_1234");

        // A 200 with `user: null` and no principal is how /api/me spells
        // "this credential is nobody" — signed out in a 200's clothing.
        let api = FakeAccountApi::answering(200, r#"{"user":null}"#);
        assert!(matches!(fetch_me(&api, "zt1_x"), Err(CloudError::SignedOut)));
    }

    #[test]
    fn an_approval_posts_the_blob_to_the_devices_own_path() {
        let device = ClientId::from_bytes([0x44; 32]);
        let api = FakeAccountApi::answering(200, r#"{"device":{"status":"approved"}}"#);
        approve_device(&api, "zt1_x", device, "bWVzc2FnZQ.c2ln").expect("a 200 is an approval");
        assert_eq!(
            api.calls.borrow()[0].0,
            format!("/api/devices/{}/approve", hex(&device.0)),
            "the id in the path is the statement's subject — the handler cross-checks it \
             against the signed bytes"
        );
        assert_eq!(
            api.posted.borrow()[0],
            r#"{"attestation":"bWVzc2FnZQ.c2ln"}"#,
            "the blob travels verbatim; the control plane stores and re-serves these bytes"
        );
    }

    #[test]
    fn a_refused_approval_surfaces_the_control_planes_word() {
        let api = FakeAccountApi::answering(400, r#"{"error":"bad_signature"}"#);
        let err = approve_device(&api, "zt1_x", ClientId::from_bytes([7; 32]), "m.s")
            .expect_err("a 400 is not an approval");
        assert!(
            err.to_string().contains("bad_signature"),
            "the refusal's own word is the only actionable part; got {err}"
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
