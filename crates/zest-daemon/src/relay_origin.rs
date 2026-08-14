//! Whether this daemon dials a relay, and where.
//!
//! An enrolled machine is a machine its owner wants reachable — that is what
//! enrolment *means*, and #229 is what it cost to make them say it twice. Both
//! machines enrolled, both apps signed in, the fleet screen listing everything,
//! and every row reading "asleep", because reachability required a `--relay`
//! flag nobody had heard of. So a daemon holding a cloud token now dials the
//! account's relay by itself.
//!
//! Three things had to be true for that to be safe rather than merely helpful:
//!
//! - **It cannot delay serving.** The origin comes from the network, and ADR-005
//!   says a local terminal survives Cloudflare being unreachable. So the fetch
//!   happens on a worker after the listeners are up, and a failure is a `warn!`
//!   and a retry — never a refusal to start.
//! - **It cannot depend on the network to repeat itself.** The origin is cached
//!   in the config directory, so the *second* start dials immediately, from
//!   disk, with the control plane unreachable. → [`OriginCache`].
//! - **It stays opt-out.** `--no-relay` is off, `--relay <url>` still overrides,
//!   and `--ephemeral` never dials by itself — a throwaway key parking a
//!   control link would advertise a host that dies with the process.
//!
//! The decision itself is [`choose`], a pure function over the six inputs, and
//! it is a function precisely because a table of six booleans reasoned about in
//! prose is how the flag nobody knew about got there in the first place.

use std::path::{Path, PathBuf};

use zest_cloud::http::Endpoint;
use zest_cloud::tls::Roots;
use zest_mesh::keystore::SecretStore;

/// What the daemon should do about a relay.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RelayChoice {
    /// Dial this, now. `--relay` was given, or a cached origin is on disk.
    Dial(String),
    /// Dial the account's relay once the control plane says which — nothing is
    /// cached yet, so this start has to ask before it can park a link.
    AskAccount,
    /// Dial nothing, for this reason.
    Off(Off),
}

/// Why a daemon is not dialling. Named rather than counted, because "my fleet
/// says asleep" is the exact question this enum answers in a log line.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Off {
    /// `--no-relay`.
    OptedOut,
    /// `--ephemeral`, with no explicit `--relay`. A key that dies with the
    /// process would park a link for a host nobody can reach afterwards.
    Ephemeral,
    /// No cloud token: this machine never enrolled, so there is no account to
    /// learn a relay from. The ordinary state of a purely local install.
    NotEnrolled,
    /// The credential store could not be read *and* nothing is cached, so this
    /// start has neither an answer nor a way to ask for one.
    ///
    /// Separate from [`Off::NotEnrolled`] because the two demand opposite
    /// responses: that one is normal, this one is a locked keychain or a
    /// keyring daemon that has not come up, and it is temporary. Collapsing it
    /// into "not enrolled" is exactly how relay dialling would disappear from
    /// an enrolled machine with nothing said.
    TokenUnreadable,
}

/// What reading the cloud token told us.
///
/// Three states, not a `bool`: a store that could not be read is not a store
/// with no token, and [`SecretStore`] returns `Result<Option<_>>` for that
/// reason. → `keystore.rs`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Token {
    /// This machine is enrolled.
    Held,
    /// It never enrolled, or logged out.
    #[default]
    Absent,
    /// The credential store said no — locked keychain, keyring not up yet.
    Unreadable,
}

/// Everything the decision depends on, so the call site cannot smuggle in a
/// seventh input without saying so here.
#[derive(Debug, Clone, Default)]
pub struct RelayInputs {
    pub ephemeral: bool,
    /// `--relay <url>` / `--relay-url <url>`.
    pub explicit: Option<String>,
    /// `--no-relay`.
    pub no_relay: bool,
    /// What reading the cloud token told us.
    pub token: Token,
    /// What the last successful fetch wrote to disk.
    pub cached: Option<String>,
}

/// Should this daemon dial, and where?
///
/// Precedence, in order, and each step is a decision rather than an accident:
///
/// 1. **`--no-relay` wins over everything**, `--relay` included. An opt-out
///    another flag on the same command line can silently overrule is not one;
///    the caller who passes both gets the safe direction and a warning from the
///    call site.
/// 2. **`--relay` wins over the account**, `--ephemeral` included — dialling a
///    relay on loopback with a throwaway key is exactly how the relay is
///    developed (`--relay http://127.0.0.1:8787`), and refusing it would break
///    the one workflow that exercises this transport.
/// 3. `--ephemeral` otherwise dials nothing.
/// 4. No token, nothing to dial: an unenrolled machine has no account.
/// 5. **Token unreadable, but an origin cached: dial it anyway.** The token
///    authorizes the *fetch*; what authorizes the link is the host key, which
///    a locked credential store has not taken away. A machine that booted
///    before its keyring did is precisely the case the cache exists for, and
///    refusing to dial there would make an enrolled machine silently
///    unreachable for the reason hardest to see from the fleet screen. With
///    nothing cached there is neither an answer nor a way to ask, so it is
///    [`Off::TokenUnreadable`] — named, so a log line can say which.
/// 6. Enrolled: the cached origin if there is one — that is what makes a start
///    with the control plane down still reachable — and otherwise ask.
#[must_use]
pub fn choose(inputs: &RelayInputs) -> RelayChoice {
    if inputs.no_relay {
        return RelayChoice::Off(Off::OptedOut);
    }
    if let Some(url) = &inputs.explicit {
        return RelayChoice::Dial(url.clone());
    }
    if inputs.ephemeral {
        return RelayChoice::Off(Off::Ephemeral);
    }
    match inputs.token {
        Token::Absent => RelayChoice::Off(Off::NotEnrolled),
        Token::Unreadable => inputs
            .cached
            .clone()
            .map_or(RelayChoice::Off(Off::TokenUnreadable), RelayChoice::Dial),
        Token::Held => {
            inputs.cached.clone().map_or(RelayChoice::AskAccount, RelayChoice::Dial)
        }
    }
}

/// The last relay origin the control plane named, on disk.
///
/// A file rather than the credential store, deliberately: an origin is a URL an
/// account publishes, not a secret, and burying it in the keychain would put a
/// macOS prompt on the startup path of a daemon that is only trying to remember
/// where it dialled yesterday. It lives beside `trust.toml` for the same reason
/// that file does — one directory a person can read, diff and delete.
pub struct OriginCache {
    path: PathBuf,
}

impl OriginCache {
    #[must_use]
    pub fn at(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }

    /// Where this user's daemon keeps it.
    #[must_use]
    pub fn default_path() -> PathBuf {
        directories::ProjectDirs::from("dev", "zesterm", "zesterm").map_or_else(
            || PathBuf::from("zesterm-relay-origin"),
            |dirs| dirs.config_dir().join("fleet").join("relay-origin"),
        )
    }

    /// What was cached, or nothing.
    ///
    /// Every failure is `None`, and that is the whole posture of this file: an
    /// unreadable cache means "ask the account again", which costs one fetch.
    /// Refusing to start over a stale convenience file would be the tail
    /// wagging the dog — unlike `trust.toml`, where a corrupt file genuinely
    /// must stop the daemon because serving without it is serving the wrong
    /// people.
    #[must_use]
    pub fn load(&self) -> Option<String> {
        let text = std::fs::read_to_string(&self.path).ok()?;
        let trimmed = text.trim();
        if trimmed.is_empty() {
            return None;
        }
        // Validated on the way *in*, and **by the dialler's own parser**: a
        // hand-edited or truncated file must not become a dial attempt against
        // a garbage authority once per start, forever. `Endpoint::parse` was
        // the wrong authority and accepted more than the dialler does -- it
        // takes a path, so `https://relay.example/v1/control` passed here and
        // then failed in `Relay::new` on every start, which is a cache that
        // validates and never works. One parser, and it is the one that dials.
        crate::relay::RelayOrigin::parse(trimmed).ok()?;
        Some(trimmed.to_string())
    }

    /// Remember `origin`, atomically. Returns whether anything changed.
    ///
    /// Temp file, **fsync**, rename — `trust.rs`'s `write_private` discipline,
    /// reimplemented rather than reused because that helper is private to
    /// `zest-mesh` and this file needs no `0600` (an origin is a URL an account
    /// publishes, not a secret).
    ///
    /// The `sync_all` is the load-bearing line and not ceremony: a rename can
    /// land before the bytes do, so without it a hard stop leaves the new name
    /// pointing at an empty or half-written file — and this file matters most
    /// exactly then, on the boot after the machine went down, when nobody is
    /// reading the logs and the only symptom is a fleet row saying asleep.
    pub fn store(&self, origin: &str) -> std::io::Result<bool> {
        if self.load().as_deref() == Some(origin) {
            return Ok(false);
        }
        if let Some(dir) = self.path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let tmp = self.path.with_extension("tmp");
        write_durable(&tmp, format!("{origin}\n").as_bytes())?;
        std::fs::rename(&tmp, &self.path)?;
        Ok(true)
    }

    /// Forget it — for `--logout`, where the token that justified it is gone.
    pub fn clear(&self) -> std::io::Result<()> {
        match std::fs::remove_file(&self.path) {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            other => other,
        }
    }

    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }
}

/// Write `bytes` to `path` and make them durable before returning.
///
/// The half of `trust.rs`'s `write_private` that is about crash safety rather
/// than permissions. See [`OriginCache::store`] for why the fsync is the point.
fn write_durable(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    use std::io::Write as _;
    let mut f = std::fs::File::create(path)?;
    f.write_all(bytes)?;
    f.sync_all()
}

/// One `GET /api/me`, for the relay origin the account publishes.
///
/// A trait for `enroll::ControlPlane`'s reason: the binary gets HTTPS and every
/// test gets a closure, and no test opens a socket.
pub trait OriginSource: Send + Sync {
    /// `Ok(None)` is a deployment with no relay — an answer, not a failure, and
    /// not something to retry into. `Err` is a fetch that did not complete.
    fn relay_origin(&self) -> Result<Option<String>, String>;
}

/// The real one: bearer `GET /api/me`, through the crate that owns TLS.
pub struct HttpsOriginSource {
    endpoint: Endpoint,
    token: String,
    roots: Roots,
}

impl HttpsOriginSource {
    /// `None` when this machine never enrolled, or when the control-plane URL
    /// cannot be requested — both are "there is nothing to ask", and neither is
    /// worth failing a startup over.
    pub fn new(secrets: &dyn SecretStore, base_url: &str, roots: Roots) -> Option<Self> {
        let token = match crate::enroll::stored_token(secrets) {
            Ok(Some(token)) => token,
            Ok(None) => return None,
            Err(e) => {
                tracing::warn!(error = %e, "cannot read the cloud token; not asking for a relay");
                return None;
            }
        };
        let url = format!("{}{ME_PATH}", base_url.trim_end_matches('/'));
        match Endpoint::parse(&url) {
            Ok(endpoint) => Some(Self { endpoint, token, roots }),
            Err(e) => {
                tracing::warn!(error = %e, url = %url, "not asking for a relay origin");
                None
            }
        }
    }
}

/// Where a machine principal learns its account's relay.
///
/// `/api/me` and not `/api/hosts`: that listing refuses host tokens on purpose
/// — a machine serving shells has no business enumerating its owner's other
/// machines — and widening it to carry one string would trade a real boundary
/// for a field. `/api/bootstrap` is the browser's answer and needs a session
/// cookie, which a daemon does not have.
pub const ME_PATH: &str = "/api/me";

impl OriginSource for HttpsOriginSource {
    fn relay_origin(&self) -> Result<Option<String>, String> {
        let response = zest_cloud::http::get(
            &self.endpoint.host,
            self.endpoint.port,
            &self.endpoint.path,
            &[("authorization", &format!("Bearer {}", self.token))],
            self.roots,
        )
        .map_err(|e| e.to_string())?;
        if !(200..300).contains(&response.status) {
            // 401 is the one worth naming: the token was revoked on the
            // account, and every later fetch says the same until someone
            // re-enrols.
            return Err(format!("the control plane answered {}", response.status));
        }
        parse_relay_origin(&response.body)
    }
}

/// Read `relayOrigin` out of an `/api/me` answer.
///
/// Split out so the parsing is tested against literal bodies rather than
/// through a socket: `null`, absent and empty all mean "no relay here", and a
/// daemon must not tell them apart — an account with no relay is a deployment
/// choice, not a failure to retry into.
pub fn parse_relay_origin(body: &str) -> Result<Option<String>, String> {
    #[derive(serde::Deserialize)]
    struct Me {
        #[serde(rename = "relayOrigin")]
        relay_origin: Option<String>,
    }
    let me: Me = serde_json::from_str(body).map_err(|e| format!("{e}"))?;
    Ok(me
        .relay_origin
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn inputs() -> RelayInputs {
        RelayInputs { token: Token::Held, ..RelayInputs::default() }
    }

    #[test]
    fn an_enrolled_machine_dials_without_being_told_to() {
        // The whole of #229 in one assertion: a machine that holds a cloud
        // token and was given no flags at all becomes reachable. Before this,
        // the answer here was "nothing", and the fleet screen said asleep for
        // every machine its owner had just enrolled.
        assert_eq!(choose(&inputs()), RelayChoice::AskAccount);

        let cached = RelayInputs { cached: Some("wss://relay.example".into()), ..inputs() };
        assert_eq!(
            choose(&cached),
            RelayChoice::Dial("wss://relay.example".into()),
            "with an origin on disk it dials immediately -- which is what makes \
             a start with the control plane unreachable still reachable"
        );
    }

    #[test]
    fn an_unenrolled_machine_dials_nothing() {
        // The ordinary local install, and it must be untouched by this feature:
        // no account, no token, nothing to ask, no network call at startup.
        let solo = RelayInputs { token: Token::Absent, ..RelayInputs::default() };
        assert_eq!(choose(&solo), RelayChoice::Off(Off::NotEnrolled));

        // Even with a stale cache: `--logout` clears the cache precisely so
        // this state means "stop dialling", and a machine that never enrolled
        // has no account whose relay it could be parked at.
        let logged_out = RelayInputs {
            token: Token::Absent,
            cached: Some("wss://relay.example".into()),
            ..RelayInputs::default()
        };
        assert_eq!(choose(&logged_out), RelayChoice::Off(Off::NotEnrolled));
    }

    #[test]
    fn a_locked_credential_store_still_dials_what_it_cached() {
        // The row that was missing, and the bug behind it: collapsing an
        // unreadable store into "not enrolled" made relay dialling vanish from
        // an enrolled machine with nothing said. What authorizes the *link* is
        // the host key, not the token -- the token only authorizes the fetch --
        // so a machine that booted before its keyring did stays reachable.
        let locked = RelayInputs {
            token: Token::Unreadable,
            cached: Some("wss://relay.example".into()),
            ..RelayInputs::default()
        };
        assert_eq!(
            choose(&locked),
            RelayChoice::Dial("wss://relay.example".into()),
            "a locked keychain at boot is exactly when the cache earns its place"
        );

        // With nothing cached there is neither an answer nor a way to ask for
        // one -- but it is its own reason, so the log can say which.
        let blind = RelayInputs { token: Token::Unreadable, ..RelayInputs::default() };
        assert_eq!(choose(&blind), RelayChoice::Off(Off::TokenUnreadable));
        assert_ne!(
            choose(&blind),
            RelayChoice::Off(Off::NotEnrolled),
            "a store that could not be read is not a store with no token, and the \
             two demand opposite responses"
        );
    }

    #[test]
    fn no_relay_beats_every_other_input() {
        // An opt-out that another flag on the same line can overrule is not an
        // opt-out. Both spellings of "dial" lose to it.
        let opted_out = RelayInputs {
            no_relay: true,
            explicit: Some("wss://relay.example".into()),
            cached: Some("wss://cached.example".into()),
            ..inputs()
        };
        assert_eq!(choose(&opted_out), RelayChoice::Off(Off::OptedOut));
    }

    #[test]
    fn an_explicit_relay_wins_over_the_account_and_over_ephemeral() {
        // `--relay http://127.0.0.1:8787 --ephemeral` is how the relay itself
        // is developed, so refusing it would break the one workflow that
        // exercises this transport end to end.
        let dev = RelayInputs {
            ephemeral: true,
            explicit: Some("http://127.0.0.1:8787".into()),
            ..RelayInputs::default()
        };
        assert_eq!(choose(&dev), RelayChoice::Dial("http://127.0.0.1:8787".into()));

        let override_account = RelayInputs {
            explicit: Some("wss://mine.example".into()),
            cached: Some("wss://account.example".into()),
            ..inputs()
        };
        assert_eq!(
            choose(&override_account),
            RelayChoice::Dial("wss://mine.example".into()),
            "the flag is an override, so it must beat what the account says"
        );
    }

    #[test]
    fn an_ephemeral_daemon_never_dials_by_itself() {
        // A key that dies with the process would park a control link for a host
        // that stops existing when the edit-run loop rebuilds.
        let dev = RelayInputs { ephemeral: true, cached: Some("wss://x.example".into()), ..inputs() };
        assert_eq!(choose(&dev), RelayChoice::Off(Off::Ephemeral));
    }

    fn tempdir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir()
            .join(format!("zesterm-relay-origin-{}-{}", name, std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("mkdir");
        dir
    }

    #[test]
    fn the_cache_round_trips_and_reports_change() {
        let dir = tempdir("roundtrip");
        let cache = OriginCache::at(dir.join("nested").join("relay-origin"));

        assert_eq!(cache.load(), None, "nothing cached yet");
        assert!(cache.store("wss://relay.example").expect("store"), "the first write changes it");
        assert_eq!(cache.load().as_deref(), Some("wss://relay.example"));

        assert!(
            !cache.store("wss://relay.example").expect("store"),
            "the same answer is not a change -- a refresh that rewrote the file \
             every five minutes would be a disk write per poll for a constant"
        );
        assert!(
            cache.store("wss://moved.example").expect("store"),
            "a different origin is a change, and the account gets to move its relay"
        );
        assert_eq!(cache.load().as_deref(), Some("wss://moved.example"));

        cache.clear().expect("clear");
        assert_eq!(cache.load(), None, "logging out forgets where to dial");
        cache.clear().expect("clearing twice is not an error");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_garbage_cache_is_ignored_rather_than_dialled() {
        // Validated on the way in, so a hand-edited or half-written file costs
        // one fetch instead of a dial against a garbage authority on every
        // start, for ever.
        let dir = tempdir("garbage");
        let path = dir.join("relay-origin");
        for junk in [
            "",
            "   \n",
            "not a url",
            "ftp://relay.example",
            "wss://relay.example/#frag",
            // The one a laxer validator let through: `Endpoint::parse` accepts
            // a path, `RelayOrigin::parse` does not, so this used to pass here
            // and then fail in `Relay::new` on every single start -- a cache
            // that validates and never works.
            "https://relay.example/v1/control",
            "wss://relay.example/v1/control",
            "wss://relay.example?host=x",
            // Plaintext off loopback is refused by the dialler, so it must be
            // refused here too rather than cached and rejected later.
            "ws://relay.example",
        ] {
            std::fs::write(&path, junk).expect("write");
            assert_eq!(
                OriginCache::at(&path).load(),
                None,
                "a cache reading {junk:?} must not become a dial"
            );
        }

        // Every spelling the dialler accepts survives the round trip, because
        // the validator here *is* the dialler's: the two deployed forms, and
        // the loopback plaintext the runbook uses for `wrangler dev`.
        for good in [
            "wss://relay.example",
            "https://relay.example",
            "wss://relay.example:8443",
            "wss://relay.example/",
            "http://127.0.0.1:8787",
        ] {
            std::fs::write(&path, format!("{good}\n")).expect("write");
            assert_eq!(
                OriginCache::at(&path).load().as_deref(),
                Some(good),
                "the cache must accept exactly what `Relay::new` will dial"
            );
            crate::relay::RelayOrigin::parse(good)
                .expect("and the dialler must accept what the cache returned");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_account_with_no_relay_is_an_answer_and_not_a_failure() {
        // `null`, absent and empty all mean the same thing, and none of them is
        // worth retrying into: a deployment without a relay is a choice.
        assert_eq!(parse_relay_origin(r#"{"relayOrigin":null}"#), Ok(None));
        assert_eq!(parse_relay_origin(r#"{"user":null}"#), Ok(None));
        assert_eq!(parse_relay_origin(r#"{"relayOrigin":"  "}"#), Ok(None));
        assert_eq!(
            parse_relay_origin(
                r#"{"user":null,"principal":{"kind":"host"},"relayOrigin":"wss://relay.example"}"#
            ),
            Ok(Some("wss://relay.example".into())),
            "the real shape of a machine's /api/me answer"
        );
        assert!(parse_relay_origin("not json").is_err(), "a body that is not the answer is a failure");
    }
}
