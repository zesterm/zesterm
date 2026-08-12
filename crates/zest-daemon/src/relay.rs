//! The daemon's outbound leg: park a control link at the relay, dial back a
//! socket per pipe.
//!
//! This is the last piece of ADR-009's path and it composes parts that already
//! existed rather than adding a protocol. The relay is
//! `cloud/packages/relay/`; `src/room/control.ts` and `src/index.ts` are the
//! other end of every message named here.
//!
//! ```text
//! daemon  --(1) GET /v1/control?host=<id>-->  relay Worker
//!         <--   {"t":"challenge",…}
//!         --    {"t":"hello",…,"sig":…}--->
//!         <--   {"t":"ready"}                  the link is parked
//!
//!         <--   {"t":"open","pipe":…}          a browser attached
//!         --(2) GET /v1/pipe?host=…&pipe=…-->  a *second* connection
//!         ====  `serve_lan` over those bytes, for that session alone
//! ```
//!
//! # Two connections, and that is the design rather than an implementation
//!
//! The control link is idle by construction: it carries a challenge, a hello,
//! and one `open` per attach. Every byte of a session travels on its own socket,
//! opened because the room asked for it. That is what lets [`serve_lan`] be
//! reused unchanged — one socket is one stream, exactly as on the LAN — and
//! what stops a busy `cat` on one session stalling every other session on the
//! host. → ADR-009.
//!
//! It is also why the handshake watchdog still works: a logical stream is a
//! socket, so cutting one is shutting one down.
//!
//! # What this leg proves, and what it does not
//!
//! The `hello` proves *which machine parked this link*. It proves nothing about
//! the session inside a pipe: that is the ordinary `zest-proto` handshake and
//! the ADR-008 sealed channel, which the relay cannot read and does not verify.
//! The two proofs are deliberately independent — the relay is a dumb pipe —
//! which is why a pipe leg goes through [`serve_lan`] with `Auth::Proof`,
//! exactly like a device on the LAN, and is trusted no further.
//!
//! **The relay's own key is not pinned.** `challenge` carries `relay_key`, and
//! this module reads it, logs it and does nothing else with it. Saying so out
//! loud because the comfortable reading is the other one: a hostile relay can
//! feed this daemon any challenge it likes, and what stops that mattering is
//! that the nonce is signed for `Purpose::Auth` and reveals nothing, and that
//! everything of value inside a pipe is sealed to a key the relay never holds.
//!
//! # The costs worth stating
//!
//! - **A parked link is not free on TLS.** [`zest_cloud::tls`] arms a one-second
//!   read poll for the life of the connection, because that is the only way a
//!   cut reaches a parked reader on Winsock. So a relay-enabled daemon wakes
//!   once a second per TLS connection. The plaintext dialler stands its poll
//!   down as soon as the control handshake completes, which is what the
//!   0%-idle test measures on the LAN; the TLS one has no equivalent switch.
//! - **Every pipe pays a 30 ms delta floor** ([`RELAY_MIN_DELTA_INTERVAL`]).
//!   Incoming messages are billed and an object that never idles never
//!   hibernates. `zest-proto` coalesces on state rather than on queued bytes,
//!   so what a floor drops is intermediate *frames*, never content. →
//!   `DaemonConfig::min_delta_interval`.
//!
//! # Refuse, do not degrade
//!
//! A relay that will not come up logs an error and leaves loopback and the LAN
//! serving, exactly as `--listen-ws` does. Nothing here can take the local
//! terminal down with it — ADR-005 requires the local path to survive
//! Cloudflare being unreachable, and "the relay is optional" has to be true of
//! the failure path and not only of the flag.

use std::io::{self, Read, Write};
use std::net::TcpStream;
use std::sync::mpsc;
use std::sync::Arc;
use std::time::Duration;

use serde::{Deserialize, Serialize};

use zest_cloud::tls::{Roots, TlsDuplex, TlsHandle};
use zest_mesh::identity::{HostIdentity, Nonce, Purpose, NONCE_LEN};

use crate::auth::{Auth, Authenticator};
use crate::lan::{Countdown, Cut, Gate, Refused, Watchdog, HANDSHAKE_TIMEOUT};
use crate::server::{serve_lan, Registry};
use crate::ws::{self, WsMessage};
use crate::DaemonConfig;

/// Where a daemon parks its control link. `cloud/packages/relay/src/routes.ts`.
const CONTROL_PATH: &str = "/v1/control";

/// Where it dials back for one pipe.
const PIPE_PATH: &str = "/v1/pipe";

/// The only control-protocol version this build speaks.
///
/// `room/control.ts`'s `CONTROL_VERSION`. A relay that answers with anything
/// else is one this daemon cannot talk to, and saying so is better than
/// guessing which fields survived.
const CONTROL_VERSION: u16 = 1;

/// `room/control.ts`'s `MAX_LABEL_CHARS`.
///
/// Enforced here as well as there because the relay's answer to a longer label
/// is `malformed`, which reads as a protocol bug in the daemon rather than as
/// "this machine's name is too long" — and the machine would never park a link
/// again.
const MAX_LABEL_CHARS: usize = 128;

/// `room/pipe.ts`'s `PIPE_ID_CHARS`.
const PIPE_ID_CHARS: usize = 32;

/// The keepalive pair the Durable Object answers **without waking**.
///
/// `room/control.ts` declares the same two strings. ADR-009's "an idle host
/// costs nothing" is this and nothing else, which is why the control link is
/// text: `WebSocketRequestResponsePair`'s members are strings.
const KEEPALIVE_REQUEST: &str = "ping";
const KEEPALIVE_RESPONSE: &str = "pong";

/// How often the parked link says `ping`.
///
/// Answered by the runtime rather than by the room, so the interval costs
/// nothing at the object and is chosen entirely against what sits between: NATs
/// and load balancers reap an idle connection, and a daemon that discovers this
/// only when a browser attaches has an outage that lasts until the next redial.
/// Thirty seconds is the usual floor for that. `tools/fake-host.mjs` defaults to
/// five, and says why: it is a demonstration, not a keepalive.
const KEEPALIVE_INTERVAL: Duration = Duration::from_secs(30);

/// The delta floor every relayed session runs under.
///
/// 30 ms, from ADR-009: incoming messages are billed and an object that never
/// idles never hibernates, so a shell printing at full speed must not turn into
/// a message per frame forever. It costs a relayed keystroke up to 30 ms of
/// echo latency and nothing else — `zest-proto` coalesces on *state*, so a
/// client that skips polls receives the current grid rather than a backlog.
pub const RELAY_MIN_DELTA_INTERVAL: Duration = Duration::from_millis(30);

/// How long the daemon waits before its first redial, and the ceiling.
///
/// A relay that is down is down for everyone, so every daemon in the fleet is
/// redialling it at once: the ceiling is what stops a recovering edge being
/// knocked over by its own clients, and the jitter is what stops them arriving
/// in step.
const FIRST_BACKOFF: Duration = Duration::from_secs(1);
const MAX_BACKOFF: Duration = Duration::from_secs(60);

/// One connection to the relay, as the two halves that can be owned separately,
/// plus the way to end it from a third place.
///
/// The halves are boxed rather than generic because the two transports have
/// nothing in common structurally — a TLS connection cannot be `try_clone`d
/// into two sockets — and because this type is what makes the whole leg
/// injectable. The `cut` is separate for the reason [`Cut`] exists: the reader
/// is parked in `read` at exactly the moment a watchdog needs to reach it.
pub struct Wire {
    pub reader: Box<dyn Read + Send>,
    pub writer: Box<dyn Write + Send>,
    pub cut: Arc<dyn Cut>,
}

/// How a connection to the relay is made.
///
/// **Injected, and that is not a testing convenience bolted on afterwards.**
/// The real relay is a Cloudflare Worker: without this seam the only way to
/// exercise anything below is `wrangler dev`, a certificate and a network, none
/// of which belong in `cargo test`. With it, `tests/relay.rs` runs the entire
/// leg — challenge, signature, `open`, dial-back, a `zest-proto` session — in
/// process against a fake relay, and the production path differs only in which
/// closure is passed here.
pub type Dialler = Arc<dyn Fn(&str, u16) -> io::Result<Wire> + Send + Sync>;

/// A cut is a `shutdown` on the socket underneath the TLS session.
///
/// The one line `zest_cloud::tls` says belongs in the crate that owns the
/// trait. There is deliberately no `stand_down`: that transport's read poll is
/// unconditional (its `READ_POLL` explains why), so there is nothing to switch
/// off and pretending otherwise would be a comment claiming more than the code
/// does.
impl Cut for TlsHandle {
    fn cut(&self) {
        TlsHandle::cut(self);
    }
}

/// TLS to the relay, verified against `roots`. The production dialler.
#[must_use]
pub fn tls_dialler(roots: Roots) -> Dialler {
    Arc::new(move |host, port| {
        let duplex = TlsDuplex::connect(host, port, roots)?;
        let cut = Arc::new(duplex.handle());
        let (reader, writer) = duplex.split();
        Ok(Wire { reader: Box::new(reader), writer: Box::new(writer), cut })
    })
}

/// Plain TCP, for a relay on this machine.
///
/// [`RelayOrigin::parse`] admits `http://`/`ws://` for loopback only, so this
/// cannot be pointed at anything across a network — see there for why that is a
/// rule rather than a warning.
#[must_use]
pub fn plaintext_dialler() -> Dialler {
    Arc::new(|host, port| {
        let sock = TcpStream::connect((host, port))?;
        // A terminal's traffic is keystroke-sized; Nagle would hold a keypress
        // back waiting for the next one. Same reason `tls::connect` sets it.
        sock.set_nodelay(true)?;
        // A `Severable` rather than the socket, so a cut reaches a parked
        // reader on Windows too — and so the poll that costs comes back off
        // when the handshake completes.
        let reader = crate::lan::Severable::new(sock)?;
        let writer = reader.try_clone()?;
        let cut = Arc::new(reader.scissors()?);
        Ok(Wire { reader: Box::new(reader), writer: Box::new(writer), cut })
    })
}

/// Where the relay is, in the pieces a dialler and a `Host:` header need.
///
/// Deliberately an **origin** and not a URL: the paths are this module's, so a
/// `--relay` carrying one is refused rather than silently dropped. That is the
/// difference between a mis-typed value that is reported and one that works
/// until somebody wonders why the prefix does nothing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RelayOrigin {
    pub host: String,
    pub port: u16,
    /// Whether the connection is TLS. `false` is loopback-only; see [`parse`].
    ///
    /// [`parse`]: RelayOrigin::parse
    pub tls: bool,
}

/// The scheme's port, when the origin does not name one.
const HTTPS_PORT: u16 = 443;
const HTTP_PORT: u16 = 80;

impl RelayOrigin {
    /// Split `wss://host[:port]`, or say what about it cannot be dialled.
    ///
    /// Four spellings, because two audiences write this down: `wss://` and
    /// `ws://` are what a person copies from the browser client's relay origin,
    /// `https://` and `http://` are what they copy from `wrangler dev`. They
    /// name the same thing — the relay speaks one protocol and it is reached by
    /// an HTTP upgrade either way.
    ///
    /// **Plaintext is refused unless the host is loopback.** `wrangler dev`
    /// serves `http://127.0.0.1:8787` and the runbook in `cloud/README.md` uses
    /// it, so refusing plaintext outright would make the one configuration
    /// anybody can actually run the odd one out. Allowing it anywhere else
    /// would be a downgrade nobody asked for: a control link in the clear
    /// publishes which machines are online and to whom, and gives whatever is
    /// in the middle a `hello` it can drop. Loopback has no middle.
    pub fn parse(url: &str) -> io::Result<Self> {
        let (scheme, rest) = url
            .split_once("://")
            .ok_or_else(|| malformed(url, "it names no scheme, so it names no transport"))?;
        let (tls, default_port) = match scheme {
            "wss" | "https" => (true, HTTPS_PORT),
            "ws" | "http" => (false, HTTP_PORT),
            other => {
                return Err(malformed(url, &format!("{other:?} is not ws, wss, http or https")))
            }
        };

        if url.contains('#') {
            return Err(malformed(url, "a fragment is never sent to a server"));
        }
        // Everything below the authority belongs to this module, so a caller
        // that supplied any of it is describing something other than what will
        // be dialled.
        if let Some(i) = rest.find(['/', '?']) {
            let tail = &rest[i..];
            if tail != "/" {
                return Err(malformed(
                    url,
                    "a relay is an origin: the paths are the daemon's to choose",
                ));
            }
        }
        let authority = rest.split(['/', '?']).next().unwrap_or("");

        if authority.starts_with('[') {
            return Err(malformed(
                url,
                "a bracketed IPv6 literal is not supported; the relay is reached by name",
            ));
        }
        if authority.contains('@') {
            return Err(malformed(url, "userinfo in a URL is not a credential this sends"));
        }

        // `rsplit_once`, so the last colon separates the port -- safe only
        // because the bracketed form is refused above, which is the one shape
        // where the last colon is inside the host.
        let (host, port) = match authority.rsplit_once(':') {
            Some((host, port)) => (
                host,
                port.parse::<u16>()
                    .map_err(|_| malformed(url, &format!("{port:?} is not a port")))?,
            ),
            None => (authority, default_port),
        };
        if host.is_empty() {
            return Err(malformed(url, "there is no host in it"));
        }
        if host.contains(':') {
            return Err(malformed(
                url,
                "the host has a colon in it, so where the port starts is a guess",
            ));
        }
        if !tls && !is_loopback(host) {
            return Err(malformed(
                url,
                "plaintext is only accepted for a relay on this machine; use wss://",
            ));
        }

        Ok(Self { host: host.to_string(), port, tls })
    }

    /// What goes in `Host:`. The port is omitted when it is the scheme's,
    /// because that is what every other client sends and a needless `:443`
    /// is one more thing an edge can route differently.
    #[must_use]
    pub fn host_header(&self) -> String {
        let default = if self.tls { HTTPS_PORT } else { HTTP_PORT };
        if self.port == default {
            self.host.clone()
        } else {
            format!("{}:{}", self.host, self.port)
        }
    }

    /// The dialler this origin's scheme asks for.
    #[must_use]
    pub fn dialler(&self, roots: Roots) -> Dialler {
        if self.tls {
            tls_dialler(roots)
        } else {
            plaintext_dialler()
        }
    }
}

impl std::fmt::Display for RelayOrigin {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}://{}", if self.tls { "wss" } else { "ws" }, self.host_header())
    }
}

/// A host this daemon may reach in the clear.
///
/// Names, not only literals: `wrangler dev` prints `http://localhost:8787` as
/// often as it prints the address, and refusing one spelling of the same
/// machine would be a rule people route around rather than follow. Nothing
/// here resolves — a `localhost` that has been pointed elsewhere is a machine
/// whose resolver is already lying to it about everything else.
fn is_loopback(host: &str) -> bool {
    host.eq_ignore_ascii_case("localhost")
        || host.parse::<std::net::IpAddr>().is_ok_and(|ip| ip.is_loopback())
}

/// Why a configured relay cannot be dialled. `InvalidInput` rather than
/// `InvalidData`: this is something a person typed, not something a peer sent.
fn malformed(url: &str, why: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, format!("{url:?} cannot be dialled: {why}"))
}

// --- the wire shapes -------------------------------------------------------

/// What the daemon sends. The only message it ever originates.
#[derive(Serialize)]
struct Hello<'a> {
    t: &'static str,
    v: u16,
    host: String,
    label: &'a str,
    sig: String,
}

/// What the relay sends.
///
/// `Unknown` rather than an error on an unrecognised `t`, so that a relay which
/// grows a message this build does not know stays usable: the ones here are the
/// whole protocol today and a fourth would be additive by construction.
#[derive(Deserialize)]
#[serde(tag = "t")]
enum Down {
    #[serde(rename = "challenge")]
    Challenge { v: u16, nonce: String, relay_key: String },
    #[serde(rename = "ready")]
    Ready { v: u16 },
    /// The `exp` this carries is **read and not enforced**, which is a decision
    /// rather than an omission. It is absolute epoch milliseconds, so honouring
    /// it means trusting this machine's clock against the relay's — and a
    /// laptop whose clock is more than ten seconds out would refuse every
    /// attach it was ever offered, silently, with the account in perfect order.
    /// The relay enforces its own deadline: a late dial gets a 404 and the
    /// browser is told the host did not come back, which is the truthful answer
    /// and does not depend on anyone's clock.
    #[serde(rename = "open")]
    Open {
        v: u16,
        pipe: String,
        /// Parsed and deliberately not enforced — see above for why a clock
        /// comparison is the wrong tool here.
        ///
        /// Carried anyway because the doc above says this message has it: a
        /// field the contract names and the type drops is a shape change
        /// nothing would notice, and `#[serde(other)]` below would swallow the
        /// whole message rather than the field.
        #[serde(default)]
        exp: Option<u64>,
    },
    /// No `v`, deliberately: a refusal is the one message worth reading from a
    /// relay this build cannot otherwise talk to, and `code` is a string under
    /// any version.
    #[serde(rename = "error")]
    Error { code: String },
    #[serde(other)]
    Unknown,
}

/// `Role::Host` + `Purpose::Auth` over the nonce bytes, as hex.
///
/// The construction is `zest-mesh`'s and there is deliberately nothing new
/// here: `cloud/packages/relay/test/control-golden.test.ts` pins the relay's
/// verifier against a signature this same call produced, so the two
/// implementations cannot drift without a test going red on one side or the
/// other.
fn sign_challenge(identity: &HostIdentity, nonce: &Nonce) -> String {
    hex(&identity.sign(Purpose::Auth, nonce.as_bytes()).to_bytes())
}

/// The `hello` for a challenge, or why the challenge could not be answered.
fn hello_for(identity: &HostIdentity, label: &str, nonce_hex: &str) -> io::Result<String> {
    let nonce = parse_nonce(nonce_hex)?;
    let hello = Hello {
        t: "hello",
        v: CONTROL_VERSION,
        host: hex(&identity.host_id().0),
        // Truncated on a character boundary, never a byte one: the relay reads
        // this as a JavaScript string and a split UTF-8 sequence would not
        // survive `JSON.parse` at all.
        label: truncate_chars(label, MAX_LABEL_CHARS),
        sig: sign_challenge(identity, &nonce),
    };
    serde_json::to_string(&hello).map_err(io::Error::other)
}

fn parse_nonce(text: &str) -> io::Result<Nonce> {
    let bytes = unhex(text)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "the nonce is not hex"))?;
    <[u8; NONCE_LEN]>::try_from(bytes.as_slice()).map(Nonce::from_bytes).map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("a challenge nonce is {NONCE_LEN} bytes; this one is {}", bytes.len()),
        )
    })
}

/// Is this an id this relay could have issued?
///
/// Checked before it reaches a URL, and the check is not only hygiene: the id
/// arrives from the far end and is interpolated into a query string, so a value
/// carrying `&` or `#` would rewrite the request rather than fail it. `ws.rs`
/// refuses CR and LF in a request line and nothing refuses these.
fn pipe_id_is_well_formed(id: &str) -> bool {
    id.len() == PIPE_ID_CHARS && id.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// Truncate to `limit` **UTF-16 code units**, on a character boundary.
///
/// UTF-16, not `char`s, because the far end counts in UTF-16: the Worker
/// refuses `label.length > MAX_LABEL_CHARS` and JavaScript's `.length` is code
/// units. Anything outside the BMP is one scalar value here and two units
/// there, so a label of 65 emoji passes a 128-`char` truncation untouched and
/// is refused as `malformed` by `parseHello`.
///
/// The daemon then reads that error, never parks, and climbs the redial ladder
/// to its ceiling — unreachable for ever, silently, which is the exact outcome
/// this function exists to prevent.
///
/// It is the mirror of a trap this repo already paid for from the other side
/// (AGENTS.md: "a JavaScript client must iterate code points, never
/// `text.length`"). Same confusion, opposite direction.
fn truncate_chars(s: &str, limit: usize) -> &str {
    let mut units = 0usize;
    for (i, c) in s.char_indices() {
        units += c.len_utf16();
        if units > limit {
            return &s[..i];
        }
    }
    s
}

fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    bytes.iter().fold(String::with_capacity(bytes.len() * 2), |mut out, b| {
        let _ = write!(out, "{b:02x}");
        out
    })
}

/// What a refusal from the relay says, given which host was refused.
///
/// `unknown-host` gets the **full** id, because it is the one refusal whose
/// remedy is mechanical — enrol this key — and the id is otherwise not
/// obtainable from a running daemon at all. Every place it surfaces is the
/// eight-hex short form (the startup line, the dial line, `mesh_probe`, the
/// DNS-SD instance name), `--identity` mints a *different* key under
/// `--ephemeral` and exits, and the one wire field that carries it is a query
/// string, which `wrangler tail` redacts. So a machine could be told exactly
/// what was wrong and still have no way to find the value that fixes it.
///
/// Disclosing it costs nothing: it is a public key, and it has already been
/// sent to the relay in the clear-to-the-relay part of this very connection.
///
/// The other codes keep the short form deliberately. Naming the key does not
/// help someone whose clock is wrong, and a 64-hex string in every reconnect
/// message is noise that trains people to skim the line.
fn refusal_message(code: &str, host: &[u8]) -> String {
    if code == "unknown-host" {
        format!(
            "the relay refused this host: unknown-host. \
             This machine is not enrolled in the account the relay serves. \
             Its id is {}; enrol it with --enroll <code>, using a code from \
             the account's devices screen.",
            hex(host)
        )
    } else {
        format!("the relay refused this host: {code}")
    }
}

fn unhex(text: &str) -> Option<Vec<u8>> {
    if !text.len().is_multiple_of(2) {
        return None;
    }
    text.as_bytes()
        .chunks_exact(2)
        .map(|pair| {
            let s = std::str::from_utf8(pair).ok()?;
            // Lowercase only, matching `fromHex` on the other side: `hosts.id`
            // is stored as `hex()` writes it, so an uppercase spelling of a key
            // is a different primary key rather than the same machine.
            if s.bytes().any(|b| b.is_ascii_uppercase()) {
                return None;
            }
            u8::from_str_radix(s, 16).ok()
        })
        .collect()
}

// --- the leg ---------------------------------------------------------------

/// How one control link ended, and how far it got before it did.
///
/// Two facts rather than one, because they are independent and the caller needs
/// both: a link that parked for six hours and then had its socket cut is a
/// healthy relay, and a link that failed the same way in its first second is
/// not.
struct LinkEnd {
    /// Whether the link ever reached `ready`.
    parked: bool,
    why: io::Result<()>,
}

/// How long to wait before the next dial.
///
/// **The reset is the whole of this type, and it was wrong until a real relay
/// under `wrangler dev` showed it.** The first version reset the ladder only
/// when a parked link closed *cleanly*, which sounds equivalent and is not:
/// almost no link closes cleanly. The relay was restarted, the daemon's reader
/// ended with "the peer went away mid-frame" — an `Err`, not an end of stream —
/// and the next dial waited the twenty seconds the earlier refusals had built,
/// with a healthy relay already listening. That is a machine unreachable for
/// twenty seconds after every deploy, and it is invisible from any test that
/// closes its sockets politely.
struct Ladder {
    first: Duration,
    max: Duration,
    current: Duration,
}

impl Ladder {
    const fn new(first: Duration, max: Duration) -> Self {
        Self { first, max, current: first }
    }

    /// The next step, given whether the attempt that just ended ever parked.
    ///
    /// Deterministic, and the jitter is the caller's — so this can be asserted
    /// as a sequence rather than as a range, which is the difference between a
    /// test that pins the reset rule and one that would have passed without it.
    fn next(&mut self, parked: bool) -> Duration {
        // A link that parked and then ended is an ordinary event — a deploy, a
        // colo moving, a laptop lid — so the next attempt starts from the
        // shortest delay rather than inheriting the ladder an outage built.
        if parked {
            self.current = self.first;
        }
        let step = self.current;
        self.current = (self.current * 2).min(self.max);
        step
    }
}

/// Everything one relay leg needs, and the thread that runs it.
///
/// Built once and shared by every pipe: the registry is the daemon's sessions,
/// the gate is the daemon's mid-handshake budget, and both are process-wide on
/// purpose — a relayed stream must count against the same number a LAN
/// connection does, or the cap is per transport and therefore not a cap.
pub struct Relay {
    origin: RelayOrigin,
    identity: Arc<HostIdentity>,
    dial: Dialler,
    config: DaemonConfig,
    registry: Arc<Registry>,
    auth: Arc<Authenticator>,
    gate: Arc<Gate>,
    first_backoff: Duration,
    max_backoff: Duration,
}

/// A [`Watchdog`] that disarms when it goes out of scope.
///
/// A `Deref` would be tidier and is deliberately not used: the point is that
/// the only way to keep it armed is to keep it alive, and a transparent wrapper
/// invites someone to reach for `disarm` by hand again.
struct DisarmOnDrop(Watchdog);

impl Drop for DisarmOnDrop {
    fn drop(&mut self) {
        self.0.disarm();
    }
}

impl Relay {
    /// Point a leg at `url`, or say why it cannot be dialled.
    ///
    /// The URL is parsed **before** anything is spawned, so a mistyped
    /// `--relay` is a message at startup rather than a thread that fails
    /// forever in the log.
    pub fn new(
        url: &str,
        identity: Arc<HostIdentity>,
        config: DaemonConfig,
        registry: Arc<Registry>,
        auth: Arc<Authenticator>,
        gate: Arc<Gate>,
        roots: Roots,
    ) -> io::Result<Self> {
        let origin = RelayOrigin::parse(url)?;
        let dial = origin.dialler(roots);
        Ok(Self {
            origin,
            identity,
            dial,
            config,
            registry,
            auth,
            gate,
            first_backoff: FIRST_BACKOFF,
            max_backoff: MAX_BACKOFF,
        })
    }

    /// Dial something other than a real relay. See [`Dialler`].
    #[must_use]
    pub fn with_dialler(mut self, dial: Dialler) -> Self {
        self.dial = dial;
        self
    }

    /// Shorten the redial delays. For tests, which cannot wait out a minute.
    #[must_use]
    pub const fn with_backoff(mut self, first: Duration, max: Duration) -> Self {
        self.first_backoff = first;
        self.max_backoff = max;
        self
    }

    #[must_use]
    pub const fn origin(&self) -> &RelayOrigin {
        &self.origin
    }

    /// Keep a control link parked, redialling for as long as the process lives.
    ///
    /// Never returns. Every failure is a redial, because there is no failure
    /// here a daemon should give up on: the relay being unreachable is a
    /// network, an outage or a laptop lid, and all three end.
    pub fn run(self) -> ! {
        let mut ladder = Ladder::new(self.first_backoff, self.max_backoff);
        loop {
            let end = self.one_link();
            match &end.why {
                Ok(()) if end.parked => {
                    tracing::info!(relay = %self.origin, "the relay control link ended; redialling");
                }
                Ok(()) => {
                    tracing::warn!(
                        relay = %self.origin,
                        "the relay closed the control link before it was parked"
                    );
                }
                Err(e) => {
                    tracing::warn!(relay = %self.origin, error = %e, "the relay control link failed");
                }
            }
            std::thread::sleep(jittered(ladder.next(end.parked)));
        }
    }

    /// One control link, from the dial to the end of it.
    ///
    /// Split from [`Self::park`] for one reason, and it is the bug that made
    /// the split: whether the link ever *parked* has to survive the failure
    /// that ended it. A `Result` alone cannot carry both, and the obvious shape
    /// — `Ok(true)` for "parked, then closed cleanly" — quietly reports a
    /// parked link that died the way links actually die as a failed dial. See
    /// [`Ladder`].
    fn one_link(&self) -> LinkEnd {
        let mut parked = false;
        let why = self.park(&mut parked);
        LinkEnd { parked, why }
    }

    /// Dial, handshake, and read until the link ends.
    fn park(&self, parked: &mut bool) -> io::Result<()> {
        let wire = (self.dial)(&self.origin.host, self.origin.port)?;
        // The same deadline a LAN connection gets, applied to the side that
        // dialled: a relay that accepts a socket and never challenges would
        // otherwise park this thread for ever. `completed()` below both disarms
        // it and stands the read poll down, which is what keeps an idle daemon
        // idle.
        let watchdog = Watchdog::start_with(Arc::clone(&wire.cut), HANDSHAKE_TIMEOUT);
        // Disarmed however this function leaves, not only where it succeeds.
        //
        // Six `?` sit between here and the disarm at the bottom -- the upgrade,
        // the version checks, the signature, the send -- and each is a path
        // that would leave a thread waiting to cut a connection already gone.
        // That is the false-positive warning `disarm` exists to prevent, plus a
        // thread per failed dial, and a relay that accepts TCP and then fails
        // the handshake produces one every time the ladder retries. A guard is
        // the only shape that covers the path nobody remembers to.
        let watchdog = DisarmOnDrop(watchdog);
        let path = format!("{CONTROL_PATH}?host={}", hex(&self.identity.host_id().0));
        let (mut reader, writer) = ws::client::connect_messages_to(
            wire.reader,
            wire.writer,
            &self.origin.host_header(),
            &path,
            &[],
            None,
        )?;
        tracing::info!(relay = %self.origin, "the relay control link is open; waiting for a challenge");

        let mut writer = Some(writer);
        // Dropping the sender is what stops the keepalive thread, so it lives
        // exactly as long as this function does however this function ends.
        let mut keepalive: Option<mpsc::Sender<()>> = None;

        let outcome = loop {
            let Some(message) = reader.next()? else { break Ok(()) };
            let text = match message {
                WsMessage::Text(text) => text,
                // The control link is text because the free keepalive is
                // defined over strings. A binary frame is a relay speaking
                // something else, and there is nothing to do with it.
                WsMessage::Binary(_) => {
                    break Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "the relay sent a binary frame on the control link",
                    ))
                }
            };
            // Before the parse, and it has to be: the keepalive response is a
            // bare `pong`, which is not JSON.
            if text == KEEPALIVE_RESPONSE {
                continue;
            }

            let message: Down = match serde_json::from_str(&text) {
                Ok(m) => m,
                Err(e) => {
                    break Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        format!("the relay sent something this build cannot read: {e}"),
                    ))
                }
            };

            match message {
                Down::Challenge { v, nonce, relay_key } => {
                    check_version(v)?;
                    // Read, logged, and pinned to nothing. See this module's
                    // header — the sealed channel inside a pipe is what makes
                    // that acceptable, not an omission nobody noticed.
                    tracing::debug!(
                        relay_key = %relay_key.chars().take(8).collect::<String>(),
                        "the relay named a key; this build does not pin it"
                    );
                    let hello = hello_for(&self.identity, &self.config.label, &nonce)?;
                    let Some(writer) = writer.as_mut() else {
                        break Err(io::Error::new(
                            io::ErrorKind::InvalidData,
                            "the relay challenged a link it had already made ready",
                        ));
                    };
                    writer.send_text(&hello)?;
                }
                Down::Ready { v } => {
                    check_version(v)?;
                    *parked = true;
                    // Only now: the handshake is what the deadline covers, and
                    // a parked link that is quiet is the healthy case.
                    watchdog.0.handle().completed();
                    tracing::info!(
                        relay = %self.origin,
                        host = %self.identity.host_id().short(),
                        "the relay control link is parked"
                    );
                    if let Some(writer) = writer.take() {
                        keepalive = Some(start_keepalive(writer, KEEPALIVE_INTERVAL));
                    }
                }
                Down::Open { v, pipe, exp } => {
                    check_version(v)?;
                    // Logged, not obeyed. A deadline in absolute epoch ms is
                    // the relay's clock, and a laptop ten seconds out would
                    // refuse every attach it was offered — but a person reading
                    // why a dial-back was too late wants to see the number the
                    // relay actually set, and the relay's own 404 is what
                    // enforces it.
                    tracing::debug!(
                        relay = %self.origin,
                        pipe = %&pipe[..pipe.len().min(8)],
                        exp = exp.unwrap_or_default(),
                        "the relay asked for a pipe"
                    );
                    if !*parked {
                        break Err(io::Error::new(
                            io::ErrorKind::InvalidData,
                            "the relay asked for a pipe before the link was parked",
                        ));
                    }
                    // One thread per pipe, and the control link goes straight
                    // back to reading: a dial that hangs or is refused must not
                    // hold up the next attach, and the relay is already
                    // counting down its own ten seconds.
                    self.spawn_pipe(pipe);
                }
                // The relay names its refusals on this link precisely so a
                // machine knows whether to re-enrol, fix its clock or upgrade.
                // Passing the code through is the whole value of that decision.
                Down::Error { code } => {
                    break Err(io::Error::new(
                        io::ErrorKind::PermissionDenied,
                        refusal_message(&code, &self.identity.host_id().0),
                    ));
                }
                Down::Unknown => {
                    tracing::debug!("ignoring a control message this build does not know");
                }
            }
        };

        drop(keepalive);
        // `watchdog` is the guard; dropping it disarms.
        outcome
    }

    /// Dial back for one pipe and serve a session down it.
    fn spawn_pipe(&self, pipe: String) {
        if !pipe_id_is_well_formed(&pipe) {
            tracing::warn!("the relay named a pipe id that it could not have issued; ignoring it");
            return;
        }

        // The slot before the socket. A pipe this daemon cannot serve is one it
        // should not spend a connection on either, and the browser learns the
        // truthful thing from the relay's own deadline: the host was asked and
        // did not come back.
        let slot = match self.gate.admit_relayed() {
            Ok(slot) => slot,
            Err(Refused::Busy) => {
                tracing::warn!(
                    pipe = %short(&pipe),
                    "refusing a relay pipe: too many connections are mid-handshake"
                );
                return;
            }
            Err(Refused::Cooling(wait)) => {
                // Unreachable today — `admit_relayed` consults no limiter —
                // and matched rather than caught by a wildcard so that adding
                // one there is a compile error here rather than a pipe that is
                // silently dropped.
                tracing::warn!(pipe = %short(&pipe), ?wait, "refusing a relay pipe: cooling down");
                return;
            }
        };

        let dial = Arc::clone(&self.dial);
        let origin = self.origin.clone();
        let host = hex(&self.identity.host_id().0);
        let registry = Arc::clone(&self.registry);
        let auth = Arc::clone(&self.auth);
        let config = pipe_config(&self.config);

        let spawned = std::thread::Builder::new()
            .name("zest-daemon-relay-pipe".into())
            .spawn(move || {
                if let Err(e) = serve_pipe(&dial, &origin, &host, &pipe, config, registry, auth, slot)
                {
                    tracing::warn!(pipe = %short(&pipe), error = %e, "a relay pipe ended");
                }
            });
        if let Err(e) = spawned {
            tracing::warn!(error = %e, "could not start a thread for a relay pipe");
        }
    }
}

/// The daemon's configuration as one relayed session runs under it.
///
/// A function rather than two lines at the call site so there is something a
/// test can hold: the floor is invisible from the client's end — it changes
/// *when* deltas arrive and never what they say — so a line that quietly went
/// missing would show up as a Cloudflare bill and nothing else.
fn pipe_config(config: &DaemonConfig) -> DaemonConfig {
    // Every relayed session, and only relayed sessions: `min_delta_interval` is
    // zero on loopback and the LAN on purpose, where a floor would buy nothing
    // and cost a keystroke's echo.
    DaemonConfig { min_delta_interval: RELAY_MIN_DELTA_INTERVAL, ..config.clone() }
}

/// The pipe leg, from the dial to the end of the session.
///
/// A free function rather than a method because it runs on its own thread and
/// must own everything it touches; nothing here may borrow the [`Relay`], whose
/// control link is meanwhile still reading.
#[allow(clippy::too_many_arguments, reason = "one thread's worth of owned state, not a seam")]
fn serve_pipe(
    dial: &Dialler,
    origin: &RelayOrigin,
    host: &str,
    pipe: &str,
    config: DaemonConfig,
    registry: Arc<Registry>,
    auth: Arc<Authenticator>,
    slot: Countdown,
) -> io::Result<()> {
    let wire = dial(&origin.host, origin.port)?;
    let watchdog = Watchdog::start_with(Arc::clone(&wire.cut), HANDSHAKE_TIMEOUT);
    let path = format!("{PIPE_PATH}?host={host}&pipe={pipe}");
    let (reader, writer) = ws::client::connect_to(
        wire.reader,
        wire.writer,
        &origin.host_header(),
        &path,
        &[],
        None,
    )?;

    // The address a person sees when they are asked to approve this device, so
    // it has to say that the request arrived through a relay — "from
    // 127.0.0.1" would be a lie in the one place a lie decides something.
    let remote = format!("relay {origin} pipe {}", short(pipe));
    tracing::info!(%remote, "a relay pipe is up; serving it");

    // `Auth::Proof`: the trust store decides, exactly as on the LAN, and this
    // connection may not approve other devices. Nothing the relay said about
    // the pipe substitutes for the peer's own handshake.
    let result = serve_lan(
        reader,
        writer,
        config,
        registry,
        Auth::Proof(auth),
        remote,
        watchdog.handle(),
        slot,
    );
    watchdog.disarm();
    result.map_err(io::Error::other)
}

/// `ping` on an interval, until the sender at the other end of `stop` is
/// dropped.
///
/// A thread rather than a timer on the reader, because the reader is parked in
/// `read` for the whole life of a healthy link — which is the entire point of
/// the link.
fn start_keepalive<W: Write + Send + 'static>(
    mut writer: ws::WsMessageWriter<W>,
    interval: Duration,
) -> mpsc::Sender<()> {
    let (tx, rx) = mpsc::channel::<()>();
    let spawned = std::thread::Builder::new()
        .name("zest-daemon-relay-ping".into())
        .spawn(move || loop {
            // `Disconnected` is the link ending: the sender lives in
            // `one_link`, so this thread cannot outlive the socket it pings.
            match rx.recv_timeout(interval) {
                Err(mpsc::RecvTimeoutError::Timeout) => {}
                _ => return,
            }
            if let Err(e) = writer.send_text(KEEPALIVE_REQUEST) {
                tracing::debug!(error = %e, "the relay keepalive stopped");
                return;
            }
        });
    if let Err(e) = spawned {
        // Not fatal: the link works, it merely risks being reaped while idle.
        tracing::warn!(error = %e, "no keepalive thread; an idle relay link may be dropped");
    }
    tx
}

fn check_version(v: u16) -> io::Result<()> {
    if v == CONTROL_VERSION {
        return Ok(());
    }
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        format!("the relay speaks control protocol {v}, this build speaks {CONTROL_VERSION}"),
    ))
}

/// Enough of an id to correlate two log lines, and never the whole of one.
fn short(id: &str) -> String {
    id.chars().take(8).collect()
}

/// `d` plus up to a quarter of itself.
///
/// Every daemon in the fleet redials the same relay, so a bare exponential
/// ladder has them all arrive in step and arrive again in step: the outage
/// recovers into a thundering herd of its own clients.
fn jittered(d: Duration) -> Duration {
    let Ok(nonce) = Nonce::random() else { return d };
    let fraction = u32::from(nonce.as_bytes()[0]);
    d + (d / 4).mul_f64(f64::from(fraction) / 255.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `HostIdentity::from_secret_bytes(&[7; 32])` — the machine
    /// `cloud/packages/relay/test/control-golden.test.ts` pins against.
    fn golden_identity() -> HostIdentity {
        HostIdentity::from_secret_bytes(&[7; 32])
    }

    /// `Nonce::from_bytes(core::array::from_fn(|i| i as u8))`, as the golden
    /// writes it.
    const GOLDEN_NONCE: &str = "000102030405060708090a0b0c0d0e0f101112131415161718191a1b1c1d1e1f";

    /// That machine signing that nonce for `Role::Host` + `Purpose::Auth`.
    const GOLDEN_SIG: &str = "568139271f5285187ab820a71165d8201341a5a6b304ea13164a04f82fc4b003\
                              15dbaeb745d39abd282c23b78b6f68a1a696ee582ec2d2feeabadc474c2fdb02";

    const GOLDEN_HOST: &str = "ea4a6c63e29c520abef5507b132ec5f9954776aebebe7b92421eea691446d22c";

    #[test]
    fn the_hello_this_daemon_sends_is_the_one_the_relay_pinned() {
        // The whole cross-implementation contract, from this side. The relay
        // verifies with `@noble/ed25519` against these exact bytes and shares
        // no code with this file, so without a golden on both sides a drift
        // surfaces at bring-up as "the relay refused: bad-signature" with
        // neither end able to say which of them moved.
        let hello = hello_for(&golden_identity(), "andy-mac", GOLDEN_NONCE)
            .expect("the golden nonce is well-formed hex");
        let value: serde_json::Value = serde_json::from_str(&hello).expect("hello is JSON");

        assert_eq!(value["t"], "hello");
        assert_eq!(value["v"], 1, "`room/control.ts` refuses anything else as `version`");
        assert_eq!(value["host"], GOLDEN_HOST, "the id is the public key, lowercase hex");
        assert_eq!(
            value["sig"], GOLDEN_SIG,
            "control-golden.test.ts verifies this exact signature; a byte of difference \
             refuses every control link this daemon will ever park"
        );
    }

    #[test]
    fn a_hello_is_inside_the_relays_message_bound() {
        // `MAX_CONTROL_MESSAGE_CHARS` is 1024 and the relay applies it *before*
        // `JSON.parse`, so a hello that outgrew it would be refused unread —
        // and the label is the only field a person controls.
        let hello = hello_for(&golden_identity(), &"y".repeat(500), GOLDEN_NONCE).expect("hello");
        assert!(
            hello.len() <= 1024,
            "a hello must fit `MAX_CONTROL_MESSAGE_CHARS`, and this one is {} chars",
            hello.len()
        );
        let value: serde_json::Value = serde_json::from_str(&hello).expect("hello is JSON");
        assert_eq!(
            value["label"].as_str().expect("a label").chars().count(),
            MAX_LABEL_CHARS,
            "a longer label is truncated here rather than refused as `malformed` there, \
             which would stop this machine parking a link at all"
        );
    }

    #[test]
    fn a_label_is_truncated_on_a_character_boundary() {
        // A byte-wise cut lands inside a multi-byte sequence, and the relay
        // reads this with `JSON.parse` — which never sees a lone surrogate,
        // because `serde_json` would have refused to serialize it. The failure
        // would be a panic in `truncate_chars`, not a bad message.
        //
        // **In UTF-16 code units, because that is what the far end counts.**
        // This assertion used to demand `chars().count() == MAX_LABEL_CHARS`,
        // which every emoji passes and the Worker then refuses: `.length` is
        // code units, so 65 emoji are 130 of them against a 128 bound. The
        // daemon reads `malformed`, never parks, and climbs the ladder to its
        // ceiling — permanently unreachable, from a label. The old assertion
        // did not catch that; it *specified* it.
        let emoji = "🙂".repeat(MAX_LABEL_CHARS + 10);
        let kept = truncate_chars(&emoji, MAX_LABEL_CHARS);
        assert_eq!(
            kept.chars().map(char::len_utf16).sum::<usize>(),
            MAX_LABEL_CHARS,
            "the bound the relay applies is UTF-16 code units, so this is the count that has \
             to fit -- a label measured in scalar values is refused as malformed and the link \
             never parks again"
        );
        assert!(kept.is_char_boundary(kept.len()));

        // And a label that is already short enough survives whole, so the fix
        // is not "truncate everything".
        assert_eq!(truncate_chars("andy-mac", MAX_LABEL_CHARS), "andy-mac");
    }

    #[test]
    fn a_signature_is_over_the_nonce_and_nothing_else_can_stand_in_for_it() {
        // The replay the challenge exists to stop. A nonce differing in one
        // byte is a different challenge, and if these ever matched the daemon
        // would be signing something the nonce does not fix.
        let id = golden_identity();
        let other = format!("{}00", &GOLDEN_NONCE[..GOLDEN_NONCE.len() - 2]);
        assert_ne!(
            sign_challenge(&id, &parse_nonce(GOLDEN_NONCE).expect("hex")),
            sign_challenge(&id, &parse_nonce(&other).expect("hex")),
            "one signature answering two challenges is one that can be captured and replayed"
        );
    }

    #[test]
    fn a_nonce_that_is_not_thirty_two_bytes_of_lowercase_hex_is_refused() {
        // Each of these is a challenge this daemon must not answer, and the
        // reason is the same in every case: what would be signed is not what
        // the relay asked about.
        let too_long = format!("{GOLDEN_NONCE}00");
        let upper = GOLDEN_NONCE.to_uppercase();
        for (bad, why) in [
            ("", "empty"),
            ("00", "far too short"),
            (&GOLDEN_NONCE[..62], "one byte short"),
            (too_long.as_str(), "one byte long"),
            (upper.as_str(), "uppercase, which is a different string to the relay"),
            ("zz01020304050607", "not hex at all"),
        ] {
            assert!(parse_nonce(bad).is_err(), "a nonce that is {why} was accepted");
        }
        assert!(parse_nonce(GOLDEN_NONCE).is_ok(), "and the real one is accepted");
    }

    #[test]
    fn a_pipe_id_that_could_rewrite_the_request_is_refused() {
        // The id arrives from the far end and is interpolated into a query
        // string. `ws.rs` refuses CR and LF in a request line; nothing refuses
        // `&`, so a relay that named `aaaa…&host=<someone else>` would have the
        // daemon dial a pipe for another machine's room.
        assert!(pipe_id_is_well_formed(&"a".repeat(PIPE_ID_CHARS)));
        assert!(pipe_id_is_well_formed("0123456789abcdef0123456789abcdef"));
        let too_long = "a".repeat(PIPE_ID_CHARS + 1);
        for bad in [
            "",
            "0123456789abcdef",
            too_long.as_str(),
            "0123456789abcdef0123456789abcde&",
            "0123456789ABCDEF0123456789ABCDEF",
            "0123456789abcdef0123456789abcd f",
        ] {
            assert!(!pipe_id_is_well_formed(bad), "{bad:?} was accepted as a pipe id");
        }
    }

    #[test]
    fn an_origin_names_a_host_a_port_and_a_transport() {
        assert_eq!(
            RelayOrigin::parse("wss://relay.example").expect("wss"),
            RelayOrigin { host: "relay.example".into(), port: 443, tls: true },
            "an absent port is the scheme's"
        );
        assert_eq!(
            RelayOrigin::parse("https://relay.example:8443").expect("https"),
            RelayOrigin { host: "relay.example".into(), port: 8443, tls: true },
            "https and wss name the same thing; the relay is reached by an HTTP upgrade"
        );
        assert_eq!(
            RelayOrigin::parse("http://127.0.0.1:8787").expect("the runbook's own value"),
            RelayOrigin { host: "127.0.0.1".into(), port: 8787, tls: false },
            "`wrangler dev` serves exactly this, and cloud/README.md's runbook uses it"
        );
        assert_eq!(
            RelayOrigin::parse("ws://localhost:8787/").expect("a trailing slash is an origin"),
            RelayOrigin { host: "localhost".into(), port: 8787, tls: false }
        );
    }

    #[test]
    fn plaintext_is_refused_for_anything_that_is_not_this_machine() {
        // The one rule here that is a security boundary rather than a parse.
        // A control link in the clear publishes which machines are online and
        // to whom, and hands whatever is in the middle a `hello` it can drop.
        for url in ["http://relay.example:8787", "ws://192.168.1.5:8787", "ws://example.test"] {
            let e = RelayOrigin::parse(url)
                .err()
                .unwrap_or_else(|| panic!("{url} was accepted in the clear"));
            assert!(
                e.to_string().contains("plaintext"),
                "the refusal must say what is wrong: {e}"
            );
        }
        // And the loopback exception really is only loopback.
        assert!(RelayOrigin::parse("http://127.0.0.2:8787").is_ok(), "all of 127/8 is this machine");
        assert!(RelayOrigin::parse("ws://LocalHost:8787").is_ok(), "a host name is case-insensitive");
    }

    #[test]
    fn an_origin_is_an_origin_and_not_a_url() {
        // Every one of these is something a person could reasonably type, and
        // every one of them would otherwise be silently dropped — the paths
        // below the authority belong to this module.
        for (url, why) in [
            ("wss://relay.example/v1/control", "a path is not the caller's to choose"),
            ("wss://relay.example/?env=staging", "nor is a query"),
            ("wss://relay.example#frag", "a fragment is never sent"),
            ("wss://user:pw@relay.example", "userinfo is not a credential this sends"),
            ("wss://[::1]:8787", "a bracketed literal is where the port guess breaks"),
            ("relay.example:8787", "no scheme names no transport"),
            ("ftp://relay.example", "and an unknown one names the wrong transport"),
            ("wss://relay.example:https", "a port that is not a number is not a port"),
            ("wss://", "there is no host in it"),
        ] {
            assert!(RelayOrigin::parse(url).is_err(), "{url:?} was accepted: {why}");
        }
    }

    #[test]
    fn the_default_port_is_left_out_of_the_host_header() {
        // What every other client sends, and one less thing for an edge to
        // route differently.
        assert_eq!(RelayOrigin::parse("wss://relay.example").unwrap().host_header(), "relay.example");
        assert_eq!(
            RelayOrigin::parse("wss://relay.example:8443").unwrap().host_header(),
            "relay.example:8443"
        );
        assert_eq!(
            RelayOrigin::parse("http://127.0.0.1:8787").unwrap().host_header(),
            "127.0.0.1:8787",
            "losing this dials 80 on the same box and reports the local Worker as unreachable"
        );
    }

    #[test]
    fn every_control_message_the_relay_sends_is_understood() {
        // Copied from `room/control.ts` and `room/pipe.ts` rather than
        // paraphrased: these are the exact strings those files build.
        let cases = [
            r#"{"t":"challenge","v":1,"nonce":"aa","relay_key":"bb"}"#,
            r#"{"t":"ready","v":1}"#,
            r#"{"t":"open","v":1,"pipe":"cc","exp":1234567890123}"#,
            r#"{"t":"error","v":1,"code":"unknown-host"}"#,
        ];
        for text in cases {
            let parsed: Down = serde_json::from_str(text).expect(text);
            assert!(
                !matches!(parsed, Down::Unknown),
                "{text} was not recognised, so the daemon would ignore it in production"
            );
        }
    }

    #[test]
    fn a_message_from_a_later_relay_is_ignored_rather_than_fatal() {
        // Additive by construction: a relay that grows a fifth message must not
        // knock every daemon in the fleet off its control link on deploy.
        let parsed: Down = serde_json::from_str(r#"{"t":"nudge","v":1}"#).expect("still JSON");
        assert!(matches!(parsed, Down::Unknown));
    }

    #[test]
    fn a_control_version_this_build_does_not_speak_is_refused_by_name() {
        let e = check_version(2).expect_err("version 2 is not this build's");
        assert!(
            e.to_string().contains("this build speaks 1"),
            "the error has to name both numbers, or the answer is 'upgrade what?': {e}"
        );
    }

    #[test]
    fn a_relayed_session_runs_under_the_delta_floor_whatever_the_daemon_defaults_to() {
        // ADR-009's dominant cost term. Incoming messages are billed and an
        // object that never idles never hibernates, so a shell printing at full
        // speed down a relay must not become a message per frame for ever. The
        // floor is invisible from the client's end — it changes when deltas
        // arrive, never what they say — so nothing but this notices it going.
        let base = DaemonConfig {
            host: zest_proto::HostId::from_bytes([1; 32]),
            label: "relay-test".into(),
            local_socket: String::new(),
            listen_lan: false,
            lan_bind: "127.0.0.1".into(),
            lan_port: 0,
            listen_ws: false,
            ws_bind: "127.0.0.1".into(),
            ws_port: 0,
            relay: Some("wss://relay.example".into()),
            shell_integration: true,
            min_delta_interval: Duration::ZERO,
        };
        assert_eq!(
            pipe_config(&base).min_delta_interval,
            Duration::from_millis(30),
            "a relay pipe must coalesce, whatever loopback and the LAN are set to"
        );
        assert_eq!(
            pipe_config(&base).label,
            base.label,
            "and nothing else about the daemon changes because a session is relayed"
        );
    }

    /// The ladder's steps, in milliseconds.
    fn ladder_steps(parked: &[bool]) -> Vec<u64> {
        let mut ladder = Ladder::new(Duration::from_millis(100), Duration::from_millis(800));
        parked.iter().map(|&p| ladder.next(p).as_millis() as u64).collect()
    }

    #[test]
    fn the_backoff_ladder_climbs_and_stops_climbing() {
        // A relay that is down is down for every daemon in the fleet, so an
        // unbounded ladder is not the risk — an unbounded herd is.
        assert_eq!(
            ladder_steps(&[false; 6]),
            vec![100, 200, 400, 800, 800, 800],
            "the delay must double to the ceiling and then hold"
        );
    }

    #[test]
    fn a_link_that_parked_resets_the_ladder_however_it_then_died() {
        // The bug a real relay under `wrangler dev` found, and the reason
        // `LinkEnd` carries two facts. Three refusals build the ladder; then a
        // link parks and its socket is cut — which is an `Err`, not a clean end
        // of stream, and is how a link ends in practice. Without the reset the
        // machine is unreachable for the twenty seconds the refusals earned,
        // with a healthy relay already listening.
        assert_eq!(
            ladder_steps(&[false, false, false, true, true]),
            vec![100, 200, 400, 100, 100],
            "a link that reached `ready` must redial at the shortest delay"
        );
    }

    #[test]
    fn the_jitter_only_ever_adds_and_never_more_than_a_quarter() {
        // Every daemon in the fleet redials the same relay, so a bare ladder
        // has them arrive in step and arrive again in step: the outage recovers
        // into a thundering herd of its own clients.
        let d = Duration::from_millis(100);
        let mut distinct = std::collections::HashSet::new();
        for _ in 0..64 {
            let j = jittered(d);
            assert!(j >= d && j <= d + d / 4, "jitter left the window: {j:?}");
            distinct.insert(j);
        }
        assert!(
            distinct.len() > 1,
            "a constant `jitter` spreads nothing, and every daemon still arrives together"
        );
    }

    #[test]
    fn unknown_host_names_the_key_that_has_to_be_enrolled() {
        let key = [0x9eu8; 32];
        let id = "9e".repeat(32);
        let msg = refusal_message("unknown-host", &key);
        assert!(
            msg.contains(&id),
            "the one refusal whose remedy is `enrol this key` has to say which key, \
             because a running daemon has no other way to report it: every other \
             surface is the eight-hex short form, and --identity mints a different \
             key under --ephemeral. Got: {msg}"
        );
        assert!(
            msg.contains("--enroll"),
            "an id with no instruction is a hex string someone has to go and look up. Got: {msg}"
        );
    }

    #[test]
    fn other_refusals_do_not_carry_the_key() {
        // Naming the key does not help someone whose clock is wrong, and a
        // 64-hex string on every reconnect is what trains people to skim the
        // line that will later matter.
        let key = [0x9eu8; 32];
        let id = "9e".repeat(32);
        for code in ["bad-signature", "stale", "unsupported-version"] {
            let msg = refusal_message(code, &key);
            assert!(
                !msg.contains(&id),
                "{code} does not become actionable by naming the key. Got: {msg}"
            );
            assert!(msg.contains(code), "the code is the diagnosis; {msg} drops it");
        }
    }
}
