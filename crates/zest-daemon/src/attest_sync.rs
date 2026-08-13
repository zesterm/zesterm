//! Trusting devices the account vouches for, without a person at this machine.
//!
//! An enrolled daemon polls `GET /api/attestations` on the control plane and
//! holds the account's verified vouchers beside its own trust file. When an
//! unknown client knocks, [`AttestedTrustStore`] consults the set: a verified,
//! in-window, non-revoked attestation whose approver is already in the *file*
//! store admits the device, and the admission is recorded in the file — a
//! one-time introduction, not a lease. What un-introduces a device is the
//! revocation list served beside the blobs (Model B: a revoked id is removed
//! from local trust outright, locally-paired or not), and `--forget` remains
//! the always-works local answer.
//!
//! # Non-transitivity, and where it lives
//!
//! An approver must be in this daemon's own trust file, **and** must not be
//! there by attestation itself. Both halves matter: a chain of attestations is
//! not a path into the fleet, because each hop would need the previous device
//! to count as an approver, and an attestation-granted record is real trust
//! for serving shells but never authority to vouch — chaining would let one
//! compromised approver key grow the trusted set without bound, one hop per
//! refresh. The control plane's own route comment defers exactly this check
//! here, because only this daemon knows which keys a person actually paired at
//! the machine.
//!
//! The marker is the record's label suffix (see [`ATTESTED_MARKER`]) rather
//! than a new field on `TrustRecord`: the label is what `--trusted` prints, so
//! the provenance a security decision reads is the same one the person sees,
//! and the trust file format does not change. Forging the marker is only ever
//! privilege-*reducing* — a record wrongly carrying it loses the power to
//! vouch, never gains one.

use std::collections::BTreeSet;
use std::sync::mpsc;
use std::sync::{Arc, RwLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use zest_cloud::http::{Endpoint, Response};
use zest_cloud::tls::Roots;
use zest_mesh::attest::{decode_attestation, Attestation};
use zest_mesh::identity::{Nonce, Signature};
use zest_mesh::keystore::SecretStore;
use zest_mesh::pairing::{Decision, PairingQueue};
use zest_mesh::trust::{TrustRecord, TrustStore};
use zest_mesh::MeshError;
use zest_proto::ClientId;

/// The one route this module knows.
pub const ATTESTATIONS_PATH: &str = "/api/attestations";

/// How a record admitted by attestation is marked, in the one place a person
/// already looks: the label. `may_vouch` reads it back, so the marker is
/// load-bearing and not decoration.
pub const ATTESTED_MARKER: &str = " (attested)";

/// The ordinary poll. Five minutes: an approval made in a browser should reach
/// every daemon well inside the pairing prompt's patience, and an account's
/// set is a handful of blobs, so the poll costs less than one keystroke's
/// round trip.
const REFRESH_INTERVAL: Duration = Duration::from_secs(5 * 60);

/// After a failed fetch. Fifteen minutes, because a control plane that is down
/// is down for everyone, and every daemon on the account hammering it back up
/// is the relay's thundering-herd lesson over again.
const RETRY_INTERVAL: Duration = Duration::from_secs(15 * 60);

/// The least time between fetches, however many pokes arrive.
///
/// A poke is sent by [`AttestedTrustStore::get`] on *every* unknown client, and
/// an unknown client is anyone who can route a packet — so without a floor, a
/// stranger knocking in a loop turns this daemon into a control-plane hammer.
const POKE_FLOOR: Duration = Duration::from_secs(30);

/// `d` spread to `[0.75d, 1.25d]`.
///
/// Every daemon on an account polls the same route, so a bare interval has
/// them arrive in step and arrive again in step. Deliberately not `relay.rs`'s
/// private helper: that one is add-only reconnect backoff, and borrowing it
/// would couple two schedules that have no reason to move together.
fn jittered(d: Duration) -> Duration {
    let Ok(nonce) = Nonce::random() else { return d };
    let fraction = f64::from(nonce.as_bytes()[0]) / 255.0;
    d.mul_f64(0.75 + 0.5 * fraction)
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
}

/// The account's verified vouchers, shared between the sync thread and every
/// connection's trust lookup.
#[derive(Default)]
pub struct AttestationSet {
    /// Signature-checked at ingestion; the window is re-checked at grant time,
    /// because a fetch is minutes old by then.
    verified: RwLock<Vec<(Attestation, Signature)>>,
    /// Device ids the account has revoked. Consulted on every grant, and acted
    /// on at fetch time (Model B removes the records outright).
    revoked: RwLock<BTreeSet<[u8; 32]>>,
    /// Ids this daemon admitted *because of an attestation*, so a later
    /// `insert` for the same client keeps the marker even when the record
    /// comes from the pairing queue's approval path, which builds its label
    /// from the client's own transcript.
    granted: RwLock<BTreeSet<[u8; 32]>>,
}

impl AttestationSet {
    /// A verified, in-window, non-revoked attestation for `client` whose
    /// approver passes `trusted_by` — or nothing.
    ///
    /// `trusted_by` is a closure rather than a store reference so a test can
    /// hand in the exact authority rule under test; production passes
    /// [`may_vouch`] over the file store.
    #[must_use]
    pub fn granting(
        &self,
        client: ClientId,
        trusted_by: &dyn Fn(ClientId) -> bool,
        now_ms: u64,
    ) -> Option<Attestation> {
        let revoked = self.revoked.read().expect("revoked lock");
        if revoked.contains(&client.0) {
            return None;
        }
        self.verified
            .read()
            .expect("verified lock")
            .iter()
            .find(|(a, _)| {
                a.device == client
                    && a.iat <= now_ms
                    && now_ms < a.exp
                    // A revoked approver's vouch dies with it, even before the
                    // fetch that removes its record has run.
                    && !revoked.contains(&a.by.0)
                    && trusted_by(a.by)
            })
            .map(|(a, _)| a.clone())
    }

    /// Replace both halves with a fresh fetch's result.
    fn replace(&self, verified: Vec<(Attestation, Signature)>, revoked: BTreeSet<[u8; 32]>) {
        *self.verified.write().expect("verified lock") = verified;
        *self.revoked.write().expect("revoked lock") = revoked;
    }

    /// Remember that `client` was admitted by attestation, so its record keeps
    /// the marker through any later insert.
    fn note_granted(&self, client: ClientId) {
        self.granted.write().expect("granted lock").insert(client.0);
    }

    fn was_granted(&self, client: ClientId) -> bool {
        self.granted.read().expect("granted lock").contains(&client.0)
    }
}

/// May `by` vouch, by this daemon's lights?
///
/// In the file store, and not there by attestation — the module docs say why
/// both halves are load-bearing. A store that cannot be read vouches for
/// nobody: refusing to grant is the safe answer to not knowing.
fn may_vouch(inner: &dyn TrustStore, by: ClientId) -> bool {
    matches!(inner.get(by), Ok(Some(r)) if !r.label.ends_with(ATTESTED_MARKER))
}

/// Ask the sync thread for a refresh, now.
///
/// Send-and-forget: the channel is unbounded so this never blocks, and a
/// receiver that is gone means there is no sync loop to hurry, which is not
/// the caller's problem.
#[derive(Clone)]
pub struct AttestPoke(mpsc::Sender<()>);

impl AttestPoke {
    pub fn poke(&self) {
        let _ = self.0.send(());
    }
}

/// A [`TrustStore`] that fills file-store misses from the attestation set.
///
/// The `AlwaysTrusted` shape: the handshake keeps one store and no
/// security-relevant branch, and what changed is which store the LAN
/// authenticator was handed. Everything but `get` and `insert` delegates.
pub struct AttestedTrustStore {
    inner: Arc<dyn TrustStore>,
    set: Arc<AttestationSet>,
    poke: AttestPoke,
}

impl AttestedTrustStore {
    #[must_use]
    pub fn new(inner: Arc<dyn TrustStore>, set: Arc<AttestationSet>, poke: AttestPoke) -> Self {
        Self { inner, set, poke }
    }
}

impl TrustStore for AttestedTrustStore {
    fn get(&self, client: ClientId) -> Result<Option<TrustRecord>, MeshError> {
        // The file first: a record a person made is never re-derived, and the
        // common case — a device this daemon already knows — costs what it
        // always cost.
        if let Some(record) = self.inner.get(client)? {
            return Ok(Some(record));
        }

        let now = now_ms();
        let inner = self.inner.as_ref();
        if let Some(a) = self.set.granting(client, &|by| may_vouch(inner, by), now) {
            // The one-time recording: from here on this device is in the file
            // like any other, and a control plane outage cannot un-trust it.
            self.set.note_granted(client);
            let record = TrustRecord {
                client,
                label: format!("{}{ATTESTED_MARKER}", a.label),
                paired_at: SystemTime::now(),
                last_seen: None,
            };
            self.inner.insert(record.clone())?;
            tracing::info!(
                client = %client.short(),
                label = %a.label,
                by = %a.by.short(),
                "trusted via account attestation"
            );
            return Ok(Some(record));
        }

        // A miss hurries the next fetch and answers now. NEVER fetch here: this
        // runs inside a handshake, and a handshake that waits on the network is
        // a handshake an attacker can make every connection wait on.
        self.poke.poke();
        Ok(None)
    }

    fn list(&self) -> Result<Vec<TrustRecord>, MeshError> {
        self.inner.list()
    }

    fn insert(&self, mut record: TrustRecord) -> Result<(), MeshError> {
        // Keep the marker on records this daemon admitted by attestation. The
        // pairing queue's approval path re-inserts with the client's own
        // transcript label, and a record that lost the marker there would have
        // quietly gained the authority to vouch.
        if self.set.was_granted(record.client) && !record.label.ends_with(ATTESTED_MARKER) {
            record.label.push_str(ATTESTED_MARKER);
        }
        self.inner.insert(record)
    }

    fn touch(&self, client: ClientId, at: SystemTime) -> Result<(), MeshError> {
        self.inner.touch(client, at)
    }

    fn remove(&self, client: ClientId) -> Result<bool, MeshError> {
        self.inner.remove(client)
    }

    fn describe(&self) -> String {
        self.inner.describe()
    }
}

/// What a fetch returns. The `ControlPlane` trait's shape, for its reason: the
/// binary gets HTTPS, every test gets a closure, and no test opens a socket.
pub trait AttestationSource: Send + Sync {
    /// An `Err` is a transport failure; a response that completed and said no
    /// is `Ok` with the status in it.
    fn fetch(&self) -> std::io::Result<Response>;
}

/// The real one: a bearer GET through the crate that owns TLS.
struct HttpsSource {
    endpoint: Endpoint,
    token: String,
    roots: Roots,
}

impl AttestationSource for HttpsSource {
    fn fetch(&self) -> std::io::Result<Response> {
        zest_cloud::http::get(
            &self.endpoint.host,
            self.endpoint.port,
            &self.endpoint.path,
            &[("authorization", &format!("Bearer {}", self.token))],
            self.roots,
        )
    }
}

/// The sync, built but not yet running.
///
/// Two phases rather than one `start`, because construction order forces it:
/// the [`AttestedTrustStore`] must exist before the `Authenticator` that holds
/// it, but the thread must not run until the one-shot command handlers
/// (`--trusted`, `--forget`, …) have had their chance to return — a listing
/// command that fires a network fetch on the way out is a side effect nobody
/// asked for.
pub struct AttestSync {
    set: Arc<AttestationSet>,
    poke: AttestPoke,
    rx: mpsc::Receiver<()>,
    source: Arc<dyn AttestationSource>,
}

impl AttestSync {
    /// `None` unless this machine is enrolled: no token, no thread, no set.
    ///
    /// An unreadable secret store also answers `None`, with a warning — the
    /// attestation sync is an optional convenience, and refusing to start the
    /// whole daemon over it would make the account outage everyone's outage.
    pub fn prepare(secrets: &dyn SecretStore, base_url: &str, roots: Roots) -> Option<Self> {
        let token = match crate::enroll::stored_token(secrets) {
            Ok(Some(token)) => token,
            Ok(None) => return None,
            Err(e) => {
                tracing::warn!(error = %e, "cannot read the cloud token; attestation sync is off");
                return None;
            }
        };
        let url = format!("{}{ATTESTATIONS_PATH}", base_url.trim_end_matches('/'));
        let endpoint = match Endpoint::parse(&url) {
            Ok(endpoint) => endpoint,
            Err(e) => {
                tracing::warn!(error = %e, url = %url, "attestation sync is off");
                return None;
            }
        };
        Some(Self::with_source(Arc::new(HttpsSource { endpoint, token, roots })))
    }

    /// The same machinery over any source. The seam every test drives.
    #[must_use]
    pub fn with_source(source: Arc<dyn AttestationSource>) -> Self {
        let (tx, rx) = mpsc::channel();
        Self {
            set: Arc::new(AttestationSet::default()),
            poke: AttestPoke(tx),
            rx,
            source,
        }
    }

    #[must_use]
    pub fn set(&self) -> Arc<AttestationSet> {
        Arc::clone(&self.set)
    }

    #[must_use]
    pub fn poke(&self) -> AttestPoke {
        self.poke.clone()
    }

    /// Fetch on start, then every [`REFRESH_INTERVAL`] ±25%, hurried by pokes.
    ///
    /// `trust` is the **file** store, not the wrapper: the loop's questions —
    /// who may vouch, whose record a revocation removes — are about what a
    /// person paired, and asking the wrapper would have a grant answer for its
    /// own authority.
    pub fn spawn(self, trust: Arc<dyn TrustStore>, queue: Arc<PairingQueue>) {
        let Self { set, rx, source, .. } = self;
        let spawned = std::thread::Builder::new()
            .name("zest-daemon-attest".into())
            .spawn(move || {
                loop {
                    let ok = refresh(source.as_ref(), &set, trust.as_ref(), &queue);
                    let last_fetch = std::time::Instant::now();

                    let wait = jittered(if ok { REFRESH_INTERVAL } else { RETRY_INTERVAL });
                    match rx.recv_timeout(wait) {
                        Ok(()) => {
                            // Honour the floor, then collapse the burst: every
                            // poke that arrived while waiting is one question
                            // ("anything new?"), not one fetch each.
                            let floor = POKE_FLOOR.saturating_sub(last_fetch.elapsed());
                            if !floor.is_zero() {
                                std::thread::sleep(floor);
                            }
                            while rx.try_recv().is_ok() {}
                        }
                        Err(mpsc::RecvTimeoutError::Timeout) => {}
                        // Every poke handle is gone, so nothing can ever hurry
                        // this loop again — and the wrapper holding one is the
                        // daemon itself, so this is shutdown.
                        Err(mpsc::RecvTimeoutError::Disconnected) => return,
                    }
                }
            });
        if let Err(e) = spawned {
            tracing::warn!(error = %e, "could not start the attestation sync thread");
        }
    }
}

/// One fetch-and-apply. `false` asks the loop for the long retry.
fn refresh(
    source: &dyn AttestationSource,
    set: &AttestationSet,
    trust: &dyn TrustStore,
    queue: &PairingQueue,
) -> bool {
    let response = match source.fetch() {
        Ok(response) => response,
        Err(e) => {
            tracing::warn!(error = %e, "could not fetch the account's attestations");
            return false;
        }
    };
    if !(200..300).contains(&response.status) {
        // 401 is worth naming: the token was revoked on the account, and every
        // later fetch will say the same until someone re-enrolls.
        tracing::warn!(
            status = response.status,
            "the control plane refused the attestation fetch"
        );
        return false;
    }
    apply(set, trust, queue, &response.body, now_ms())
}

/// Parse, verify, store, revoke, and welcome whoever was waiting.
///
/// Split from [`refresh`] so every test drives it with a string and a clock —
/// the `ControlPlane` discipline, one layer down.
fn apply(
    set: &AttestationSet,
    trust: &dyn TrustStore,
    queue: &PairingQueue,
    body: &str,
    now_ms: u64,
) -> bool {
    #[derive(serde::Deserialize)]
    struct Feed {
        attestations: Vec<String>,
        revoked: Vec<String>,
    }
    let feed: Feed = match serde_json::from_str(body) {
        Ok(feed) => feed,
        Err(e) => {
            tracing::warn!(error = %e, "the attestation feed did not parse");
            return false;
        }
    };

    // Verify at ingestion, once per fetch, so a handshake never pays for a
    // signature check — and skip rather than fail on a bad blob: one garbage
    // entry must not cost the account every good one beside it.
    let mut verified = Vec::with_capacity(feed.attestations.len());
    let mut skipped = 0usize;
    for blob in &feed.attestations {
        match decode_attestation(blob) {
            Some(d) if d.verify(now_ms) => verified.push((d.fields, d.signature)),
            _ => skipped += 1,
        }
    }
    if skipped > 0 {
        tracing::warn!(skipped, "attestations that did not decode or verify were ignored");
    }

    let mut revoked = BTreeSet::new();
    for id in &feed.revoked {
        match parse_id(id) {
            Some(bytes) => {
                revoked.insert(bytes);
            }
            None => tracing::warn!(id = %id, "a revoked id that is not 64 hex was ignored"),
        }
    }

    // Model B, the removal half: a revocation on the account removes the local
    // record outright, locally-paired or not. The person's escape hatch is the
    // machine itself -- pairing there writes a fresh record, and `--forget`
    // still works with the account unreachable.
    for id in &revoked {
        let client = ClientId::from_bytes(*id);
        let Ok(Some(record)) = trust.get(client) else { continue };
        match trust.remove(client) {
            Ok(true) => tracing::info!(
                client = %client.short(),
                label = %record.label,
                "revoked on the account; removed from local trust -- pair at the machine to restore"
            ),
            Ok(false) => {}
            Err(e) => tracing::warn!(
                client = %client.short(),
                error = %e,
                "could not remove a revoked device from local trust"
            ),
        }
    }

    set.replace(verified, revoked);

    // Whoever is parked at NeedsApproval and now has a granting attestation is
    // welcomed here, seconds after the approval, with no person at this
    // machine. `resolve` is `Authenticator::decide`'s body; the connection's
    // own writer applies the decision, which records the trust through the
    // wrapper (so the marker survives -- see `AttestedTrustStore::insert`).
    for request in queue.pending() {
        if set.granting(request.client, &|by| may_vouch(trust, by), now_ms).is_some() {
            set.note_granted(request.client);
            let answered = queue.resolve(request.client, Decision::Approve);
            tracing::info!(
                client = %request.client.short(),
                label = %request.label,
                answered,
                "approved a waiting device from an account attestation"
            );
        }
    }

    true
}

/// 64 lowercase hex characters, or nothing.
fn parse_id(text: &str) -> Option<[u8; 32]> {
    if text.len() != 64 {
        return None;
    }
    let mut out = [0u8; 32];
    for (i, byte) in out.iter_mut().enumerate() {
        *byte = u8::from_str_radix(text.get(i * 2..i * 2 + 2)?, 16).ok()?;
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc::TryRecvError;
    use zest_mesh::attest::{attestation_message, sign_attestation};
    use zest_mesh::identity::ClientIdentity;
    use zest_mesh::keystore::MemoryKeyStore;
    use zest_mesh::trust::MemoryTrustStore;

    fn identity(seed: u8) -> ClientIdentity {
        ClientIdentity::from_secret_bytes(&[seed; 32])
    }

    /// A window spanning the real clock, because [`AttestedTrustStore::get`]
    /// reads `SystemTime::now()` — a fixed 2025 window would expire the moment
    /// the calendar moved past it and fail these tests for no code change.
    fn attested(approver: &ClientIdentity, device: ClientId, label: &str) -> (Attestation, Signature) {
        let now = now_ms();
        let a = Attestation {
            v: 1,
            account: "acct_test".into(),
            device,
            label: label.into(),
            by: approver.client_id(),
            iat: now.saturating_sub(3_600_000),
            exp: now + 86_400_000,
        };
        let sig = sign_attestation(approver, &a).expect("fits");
        (a, sig)
    }

    fn paired(store: &dyn TrustStore, client: ClientId, label: &str) {
        store
            .insert(TrustRecord {
                client,
                label: label.into(),
                paired_at: SystemTime::UNIX_EPOCH,
                last_seen: None,
            })
            .expect("insert");
    }

    fn wrapped(
        inner: Arc<dyn TrustStore>,
    ) -> (AttestedTrustStore, Arc<AttestationSet>, mpsc::Receiver<()>) {
        let set = Arc::new(AttestationSet::default());
        let (tx, rx) = mpsc::channel();
        (AttestedTrustStore::new(inner, Arc::clone(&set), AttestPoke(tx)), set, rx)
    }

    fn blob(a: &Attestation, sig: &Signature) -> String {
        fn b64(bytes: &[u8]) -> String {
            const ALPHABET: &[u8; 64] =
                b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
            let mut out = String::new();
            for chunk in bytes.chunks(3) {
                let b = [chunk[0], *chunk.get(1).unwrap_or(&0), *chunk.get(2).unwrap_or(&0)];
                let n = (u32::from(b[0]) << 16) | (u32::from(b[1]) << 8) | u32::from(b[2]);
                for (i, ch) in [(n >> 18) & 63, (n >> 12) & 63, (n >> 6) & 63, n & 63]
                    .iter()
                    .enumerate()
                {
                    if i <= chunk.len() {
                        out.push(ALPHABET[*ch as usize] as char);
                    }
                }
            }
            out
        }
        let message = attestation_message(a).expect("fits");
        format!("{}.{}", b64(&message), b64(&sig.to_bytes()))
    }

    fn feed(attestations: &[(Attestation, Signature)], revoked: &[ClientId]) -> String {
        let blobs: Vec<String> = attestations.iter().map(|(a, s)| blob(a, s)).collect();
        let ids: Vec<String> = revoked
            .iter()
            .map(|c| c.0.iter().map(|b| format!("{b:02x}")).collect())
            .collect();
        serde_json::to_string(&serde_json::json!({ "attestations": blobs, "revoked": ids }))
            .expect("json")
    }

    #[test]
    fn a_vouched_device_is_recorded_once_and_then_served_from_the_file() {
        let approver = identity(0x11);
        let device = identity(0x22).client_id();
        let inner: Arc<dyn TrustStore> = Arc::new(MemoryTrustStore::new());
        paired(inner.as_ref(), approver.client_id(), "desk-mac");
        let (store, set, _rx) = wrapped(Arc::clone(&inner));
        set.replace(vec![attested(&approver, device, "andy-phone")], BTreeSet::new());

        let record = store
            .get(device)
            .expect("get")
            .expect("the whole point: the account's voucher admits the device");
        assert_eq!(
            record.label, "andy-phone (attested)",
            "the marker names the provenance where a person will read it"
        );
        assert!(
            inner.get(device).expect("get").is_some(),
            "the grant is recorded in the file store, one time"
        );

        // The set emptying -- a control plane outage, a served list that
        // shrank -- must not un-trust a device that was already introduced.
        set.replace(Vec::new(), BTreeSet::new());
        assert!(
            store.get(device).expect("get").is_some(),
            "an introduction outlives the attestation that made it"
        );
    }

    #[test]
    fn expiry_and_revocation_both_refuse() {
        let approver = identity(0x11);
        let device = identity(0x22).client_id();
        let inner: Arc<dyn TrustStore> = Arc::new(MemoryTrustStore::new());
        paired(inner.as_ref(), approver.client_id(), "desk-mac");
        let trusted_by = |by: ClientId| may_vouch(inner.as_ref(), by);

        let set = AttestationSet::default();
        let (a, sig) = attested(&approver, device, "andy-phone");
        set.replace(vec![(a.clone(), sig)], BTreeSet::new());

        assert!(set.granting(device, &trusted_by, now_ms()).is_some(), "in-window grants");
        assert!(
            set.granting(device, &trusted_by, a.exp).is_none(),
            "at exp the window is over -- exp is exclusive, as everywhere else"
        );
        assert!(
            set.granting(device, &trusted_by, a.iat - 1).is_none(),
            "before iat nothing is granted"
        );

        set.replace(vec![(a.clone(), sig)], BTreeSet::from([device.0]));
        assert!(
            set.granting(device, &trusted_by, now_ms()).is_none(),
            "a revoked device is refused however live its attestation"
        );

        // A revoked *approver* takes its vouches with it, even before the
        // fetch that removes its record.
        set.replace(vec![(a, sig)], BTreeSet::from([approver.client_id().0]));
        assert!(
            set.granting(device, &trusted_by, now_ms()).is_none(),
            "a revoked approver's vouch is dead on arrival"
        );
    }

    #[test]
    fn an_attested_in_devices_vouch_grants_nothing() {
        // Non-transitivity. A is hand-paired; B got in by A's attestation; C
        // arrives vouched by B. If B could vouch, one compromised approver key
        // would grow the trusted set without bound, one hop per refresh.
        let a_id = identity(0x11);
        let b_id = identity(0x22);
        let c = identity(0x33).client_id();
        let inner: Arc<dyn TrustStore> = Arc::new(MemoryTrustStore::new());
        paired(inner.as_ref(), a_id.client_id(), "desk-mac");
        let (store, set, _rx) = wrapped(Arc::clone(&inner));

        set.replace(
            vec![
                attested(&a_id, b_id.client_id(), "phone"),
                attested(&b_id, c, "stranger"),
            ],
            BTreeSet::new(),
        );

        assert!(store.get(b_id.client_id()).expect("get").is_some(), "A's vouch admits B");
        assert!(
            store.get(c).expect("get").is_none(),
            "B's record is attestation-granted, so B's vouch admits nobody"
        );

        // And before B ever attaches, its vouch is worth even less: it has no
        // record at all.
        let fresh: Arc<dyn TrustStore> = Arc::new(MemoryTrustStore::new());
        paired(fresh.as_ref(), a_id.client_id(), "desk-mac");
        let (fresh_store, fresh_set, _rx2) = wrapped(Arc::clone(&fresh));
        fresh_set.replace(vec![attested(&b_id, c, "stranger")], BTreeSet::new());
        assert!(
            fresh_store.get(c).expect("get").is_none(),
            "an approver with no local record vouches for nobody"
        );
    }

    #[test]
    fn a_revocation_removes_a_locally_paired_record() {
        // Model B, the full version the user chose: the account's revocation
        // list removes local records outright, hand-paired ones included.
        let victim = identity(0x44).client_id();
        let inner: Arc<dyn TrustStore> = Arc::new(MemoryTrustStore::new());
        paired(inner.as_ref(), victim, "old-laptop");
        let set = AttestationSet::default();
        let queue = PairingQueue::new();

        assert!(
            apply(&set, inner.as_ref(), &queue, &feed(&[], &[victim]), now_ms()),
            "the feed applies"
        );
        assert!(
            inner.get(victim).expect("get").is_none(),
            "revoked on the account means removed from local trust, even for a \
             record a person paired at this machine -- pairing there again is \
             the restore path"
        );
    }

    #[test]
    fn a_miss_pokes_the_sync_without_blocking() {
        let inner: Arc<dyn TrustStore> = Arc::new(MemoryTrustStore::new());
        let (store, _set, rx) = wrapped(inner);
        let stranger = ClientId::from_bytes([0x5a; 32]);

        // `get` returns immediately -- the poke is the *only* network-shaped
        // thing a handshake may cause, and it is send-and-forget.
        assert!(store.get(stranger).expect("get").is_none());
        assert_eq!(rx.try_recv(), Ok(()), "the miss asked the sync loop to hurry");
        assert_eq!(
            rx.try_recv(),
            Err(TryRecvError::Empty),
            "one miss, one poke"
        );
    }

    #[test]
    fn a_machine_that_never_enrolled_starts_no_sync() {
        // No token, no thread, no set: the daemon must behave exactly as it
        // did before this feature for everyone who never touched an account.
        let secrets = MemoryKeyStore::new();
        assert!(
            AttestSync::prepare(&secrets, "https://zesterm.example", Roots::Platform).is_none(),
            "Ok(None) from the token store means no attestation sync at all"
        );
    }

    #[test]
    fn a_waiting_client_is_approved_when_the_refresh_lands() {
        let approver = identity(0x11);
        let device = identity(0x22).client_id();
        let inner: Arc<dyn TrustStore> = Arc::new(MemoryTrustStore::new());
        paired(inner.as_ref(), approver.client_id(), "desk-mac");
        let set = AttestationSet::default();
        let queue = PairingQueue::new();

        // A device parked at NeedsApproval: the handshake queued it and its
        // connection is waiting on the notify.
        let (tx, rx) = mpsc::channel();
        let _handle = queue.submit(
            zest_mesh::pairing::PairingRequest {
                client: device,
                label: "andy-phone".into(),
                code: "123456".into(),
                remote: "192.0.2.7".into(),
                requested_at: std::time::Instant::now(),
            },
            Box::new(move |d| tx.send(d).expect("send")),
        );

        let (a, sig) = attested(&approver, device, "andy-phone");
        assert!(apply(&set, inner.as_ref(), &queue, &feed(&[(a, sig)], &[]), now_ms()));

        assert_eq!(
            rx.try_recv(),
            Ok(Decision::Approve),
            "the parked connection is woken with an approval, no person involved"
        );
        assert!(
            set.was_granted(device),
            "the grant is noted before the decision, so the record the approval \
             path writes keeps the attested marker"
        );
    }

    #[test]
    fn the_approval_paths_record_keeps_the_marker_through_reinsert() {
        // The pairing queue's approval path inserts a record built from the
        // client's own transcript label. Through the wrapper, that insert must
        // keep the marker -- losing it would quietly grant the record the
        // authority to vouch.
        let device = identity(0x22).client_id();
        let inner: Arc<dyn TrustStore> = Arc::new(MemoryTrustStore::new());
        let (store, set, _rx) = wrapped(Arc::clone(&inner));
        set.note_granted(device);

        store
            .insert(TrustRecord {
                client: device,
                label: "andy-phone".into(),
                paired_at: SystemTime::UNIX_EPOCH,
                last_seen: None,
            })
            .expect("insert");
        assert_eq!(
            inner.get(device).expect("get").expect("present").label,
            "andy-phone (attested)",
            "the marker survives the approval path's re-insert"
        );
    }

    #[test]
    fn a_feed_that_does_not_parse_asks_for_the_long_retry() {
        let set = AttestationSet::default();
        let inner = MemoryTrustStore::new();
        let queue = PairingQueue::new();
        assert!(
            !apply(&set, &inner, &queue, "not json", now_ms()),
            "a body that is not the feed is a failed fetch, not an empty account"
        );
    }

    #[test]
    fn a_bad_blob_is_skipped_and_the_good_one_beside_it_still_lands() {
        let approver = identity(0x11);
        let device = identity(0x22).client_id();
        let inner: Arc<dyn TrustStore> = Arc::new(MemoryTrustStore::new());
        paired(inner.as_ref(), approver.client_id(), "desk-mac");
        let set = AttestationSet::default();
        let queue = PairingQueue::new();

        let (a, sig) = attested(&approver, device, "andy-phone");
        let good = blob(&a, &sig);
        let body = serde_json::to_string(
            &serde_json::json!({ "attestations": ["garbage", good], "revoked": [] }),
        )
        .expect("json");

        assert!(apply(&set, inner.as_ref(), &queue, &body, now_ms()));
        assert!(
            set.granting(device, &|by| may_vouch(inner.as_ref(), by), now_ms()).is_some(),
            "one garbage entry must not cost the account every good one beside it"
        );
    }

    #[test]
    fn the_jitter_stays_inside_a_quarter_either_way() {
        let d = Duration::from_secs(300);
        for _ in 0..64 {
            let j = jittered(d);
            assert!(
                j >= d.mul_f64(0.75) && j <= d.mul_f64(1.25),
                "the spread is what stops every daemon polling in step: {j:?}"
            );
        }
    }
}
