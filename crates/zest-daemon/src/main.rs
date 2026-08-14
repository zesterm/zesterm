//! `zest-daemon` — serves this machine's terminals.
//!
//! Owns the PTYs and hands their grids to whoever attaches: the local GUI app
//! over a loopback socket, other machines over the LAN or a tunnel. Sessions
//! outlive every client, which is the entire reason this is a separate process.
//! → ADR-007.

use std::sync::Arc;

use zest_cloud::tls::Roots;
use zest_daemon::enroll;
use zest_daemon::{default_socket_path, listen, Authenticator, DaemonConfig, Registry};
use zest_mesh::identity::HostIdentity;
use zest_mesh::keystore::{CredentialStore, MemoryKeyStore};
use zest_mesh::pairing::{Decision, PairingQueue};
use zest_mesh::trust::{FileTrustStore, MemoryTrustStore, TrustStore};
use zest_proto::ClientId;

fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_env("ZESTERM_LOG")
                .unwrap_or_else(|_| "info".into()),
        )
        .init();

    let args: Vec<String> = std::env::args().skip(1).collect();
    let opt = |name: &str| -> Option<String> {
        args.iter().position(|a| a == name).and_then(|i| args.get(i + 1)).cloned()
    };
    let flag = |name: &str| args.iter().any(|a| a == name);

    if flag("--help") || flag("-h") {
        println!(
            "zest-daemon\n\n\
             --socket <path>     where to listen (default: per-user)\n\
             --label <name>      how this machine appears in a fleet listing\n\
             --socket-path       print the default socket path and exit\n\
             --identity          print this host's id and exit\n\
             --ephemeral         use a throwaway key, not the OS keychain\n\
             --enroll <code>     join this machine to an account, using a code\n\
             \x20                   from its devices screen, and exit\n\
             --logout            forget the account token this machine holds\n\
             --account           print what this machine has stored, and exit\n\
             --control-plane <url>\n\
             \x20                   where the accounts API lives\n\
             --trust <hex>       trust a client id without a prompt\n\
             --forget <hex>      remove a trusted client\n\
             --trusted           list trusted clients and exit\n\
             --trust-file <path> where pairings are recorded\n\
             --no-prompt         never ask on stdin; refuse unknown clients\n\
             --listen-lan        serve other machines (off by default)\n\
             --no-shell-integration\n\
             \x20                   do not load the command-block hook into spawned\n\
             \x20                   shells (no file of yours is written either way)\n\
             --lan-bind <addr>   which interface (default 0.0.0.0)\n\
             --lan-port <port>   preferred port (default 7717)\n\
             --listen-ws         serve WebSocket clients -- browsers (off by default)\n\
             --ws-bind <addr>    which interface (default 0.0.0.0)\n\
             --ws-port <port>    preferred port (default 7718)\n\
             --relay <url>       dial this relay so the machine is reachable\n\
             \x20                   from anywhere, without opening a port.\n\
             \x20                   An enrolled machine dials its account's\n\
             \x20                   relay on its own, so this is an override\n\
             \x20                   (--relay-url is the same flag). wss://host,\n\
             \x20                   or http://127.0.0.1:8787 for a relay\n\
             \x20                   running on this machine\n\
             --no-relay          do not dial any relay, even when enrolled.\n\
             \x20                   The machine stays reachable on the LAN and\n\
             \x20                   on loopback, and shows as asleep from away\n\
             --min-delta-interval <ms>\n\
             \x20                   least time between updates for one client\n\
             \x20                   (default 0 -- send as fast as the shell\n\
             \x20                   prints; a client behind a metered link gets\n\
             \x20                   the current grid, never a backlog)\n\n\
             Sessions outlive the clients attached to them. Closing a window\n\
             does not end a shell."
        );
        return;
    }

    let socket = opt("--socket").unwrap_or_else(default_socket_path);
    if flag("--socket-path") {
        println!("{socket}");
        return;
    }

    // --- identity ---------------------------------------------------------
    //
    // The HostId *is* the public key, so this is what makes "is this really my
    // Mac" answerable by asking it to sign a nonce. It replaces a hardcoded
    // [0; 32], which was not merely a placeholder: an all-zero Ed25519 key is a
    // small-order point that `verifying_key` already rejects, so nothing signed
    // under it could ever have verified.
    let ephemeral = flag("--ephemeral");
    let store: Arc<dyn CredentialStore> = if ephemeral {
        // For the edit-run loop. `keyring` keys access to the *binary*, so on
        // macOS every rebuild re-prompts for the keychain -- and the outcome of
        // that is a developer who turns authentication off to get work done.
        Arc::new(MemoryKeyStore::new())
    } else {
        match zest_mesh::keystore::OsKeyStore::new() {
            Ok(s) => Arc::new(s),
            Err(e) => {
                tracing::error!(error = %e, "no credential store; refusing to start");
                eprintln!(
                    "zest-daemon: this machine has no usable credential store.\n\
                     A host that keeps its private key anywhere else is not one\n\
                     worth shipping. Use --ephemeral for a throwaway key."
                );
                std::process::exit(1);
            }
        }
    };

    let identity = match HostIdentity::load_or_create(store.as_ref()) {
        Ok(i) => Arc::new(i),
        Err(e) => {
            tracing::error!(error = %e, "could not load this host's identity");
            std::process::exit(1);
        }
    };

    if flag("--identity") {
        println!("{}", hex(&identity.host_id().0));
        println!("short: {}", identity.host_id().short());
        return;
    }

    // --- the account ------------------------------------------------------
    //
    // Foreground, and then exit, exactly like --trust and --trusted below. A
    // detached daemon has no terminal a one-shot code can be handed to, and the
    // two places it could read one from — a config file, an environment
    // variable — are both places a credential should never be written down. The
    // running daemon picks the token up from the credential store next time it
    // starts. → enroll.rs.
    //
    // The label is settled here rather than further down because enrolment
    // signs it: whatever this machine calls itself in a fleet listing is what
    // the account's devices screen will call it, and the two disagreeing would
    // be a name that changes depending on where you look.
    let label = opt("--label").unwrap_or_else(machine_label);

    if let Some(code) = opt("--enroll") {
        if ephemeral {
            // Refused rather than warned. The key dies with this process, so
            // the account would gain a host row nothing can ever answer for —
            // and the code is one-shot, so it is spent either way and the
            // mistake costs a trip back to the browser.
            eprintln!(
                "zest-daemon: --enroll claims this machine's lasting identity, and\n\
                 --ephemeral is a key that dies with the process. Enrolling one would\n\
                 leave the account listing a host nobody can reach."
            );
            std::process::exit(1);
        }
        let base =
            opt("--control-plane").unwrap_or_else(|| enroll::DEFAULT_CONTROL_PLANE.to_string());
        // The operating system's trust store, which is the only thing that
        // accepts a corporate middlebox's re-signed certificate. Nothing here
        // chooses the bundled list yet; a machine with no trust store at all —
        // a minimal container — is the case that will need a flag for it.
        let http = enroll::HttpsControlPlane::new(Roots::Platform);
        match enroll::enroll(&identity, &code, &label, &base, &http, store.as_ref()) {
            Ok(enrolled) => {
                let account = enrolled.account.unwrap_or_else(|| "this account".into());
                println!(
                    "enrolled \"{label}\" ({}) with {account}",
                    identity.host_id().short()
                );
                println!("the token is kept in {}", store.describe_secret_store());
                // #229: the step nobody could have guessed. Enrolment is what
                // records the intent to be reachable, so say what it now buys
                // — a person who reads only this line should not end up
                // staring at a fleet screen that says asleep.
                println!(
                    "from its next start this machine dials the account's relay, \
                     so it is reachable from anywhere (--no-relay opts out)"
                );
            }
            // The mirror of the app's #228 mapping: a device code fed to
            // --enroll. The generic refusal would say "get a fresh code",
            // which mints another one of the same wrong kind.
            Err(enroll::EnrollError::Refused { ref message, .. }) if message == "wrong_kind" => {
                eprintln!(
                    "zest-daemon: that code is for the app's sign-in — \
                     in the browser use Add a machine instead"
                );
                std::process::exit(1);
            }
            Err(e) => {
                eprintln!("zest-daemon: {e}");
                std::process::exit(1);
            }
        }
        return;
    }

    if flag("--logout") {
        match enroll::forget_token(store.as_ref()) {
            Ok(true) => {
                println!("forgot the token kept in {}", store.describe_secret_store());
                // The cached relay origin goes with the token that justified
                // it. Leaving it behind would have the next start dial an
                // account this machine no longer belongs to -- which the relay
                // would refuse, once every backoff, for ever.
                let cache = zest_daemon::relay_origin::OriginCache::at(
                    zest_daemon::relay_origin::OriginCache::default_path(),
                );
                if let Err(e) = cache.clear() {
                    tracing::warn!(error = %e, path = %cache.path().display(),
                        "could not forget the cached relay origin");
                }
                // Said plainly, because the comfortable reading is the wrong
                // one: this drops *this machine's* copy and nothing else. The
                // account keeps listing the host until it is revoked there.
                println!("the account still lists this host until it is revoked there");
            }
            Ok(false) => println!("no token was stored in {}", store.describe_secret_store()),
            Err(e) => {
                eprintln!("zest-daemon: {e}");
                std::process::exit(1);
            }
        }
        return;
    }

    if flag("--account") {
        println!("host   {}", hex(&identity.host_id().0));
        println!("label  {label}");
        match enroll::stored_token(store.as_ref()) {
            // Presence, never the token. It is a bearer credential — whoever
            // holds it is this machine as far as the control plane is
            // concerned — and a terminal's scrollback is one of the places
            // people copy from without reading.
            Ok(Some(_)) => println!("token  stored in {}", store.describe_secret_store()),
            Ok(None) => println!(
                "token  none. This machine has not enrolled; run --enroll <code> with a \
                 code from the account's devices screen"
            ),
            Err(e) => {
                eprintln!("zest-daemon: {e}");
                std::process::exit(1);
            }
        }
        return;
    }

    // --- trust ------------------------------------------------------------

    let trust: Arc<dyn TrustStore> = if ephemeral {
        // Said out loud rather than ignored: someone who passed --trust-file
        // and got a store that forgets on exit should hear about it here, not
        // discover it when every device re-prompts after a restart.
        if opt("--trust-file").is_some() {
            tracing::warn!("--ephemeral overrides --trust-file; pairings will not be kept");
        }
        Arc::new(MemoryTrustStore::new())
    } else {
        let path = opt("--trust-file").map_or_else(default_trust_path, std::path::PathBuf::from);
        match FileTrustStore::open(&path) {
            Ok(t) => Arc::new(t),
            Err(e) => {
                // Refusing rather than starting with an empty list. Silently
                // forgetting every pairing makes every device prompt again, and
                // a stream of prompts is how someone learns to approve without
                // reading.
                tracing::error!(error = %e, "could not read the trust store");
                eprintln!("zest-daemon: {e}");
                std::process::exit(1);
            }
        }
    };

    // The account's attestations, for a machine that enrolled: prepared here,
    // because the authenticator below must hold the wrapped store, but not
    // *running* yet -- the one-shot handlers between here and the servers
    // (--trust, --forget, --trusted) return before it would be right for a
    // listing command to touch the network. Under --ephemeral the keystore is
    // memory and `prepare` would answer None by itself; the explicit gate says
    // the intent out loud.
    let attest = if ephemeral {
        None
    } else {
        zest_daemon::attest_sync::AttestSync::prepare(
            store.as_ref(),
            &opt("--control-plane")
                .unwrap_or_else(|| enroll::DEFAULT_CONTROL_PLANE.to_string()),
            Roots::Platform,
        )
    };
    // Both names stay live: the wrapper is what serves handshakes, and the
    // file store is what the sync loop reasons about (who may vouch, whose
    // record a revocation removes). Handing the loop the wrapper would let a
    // grant answer for its own authority.
    let file_trust = Arc::clone(&trust);
    let trust: Arc<dyn TrustStore> = match &attest {
        Some(sync) => Arc::new(zest_daemon::attest_sync::AttestedTrustStore::new(
            Arc::clone(&trust),
            sync.set(),
            sync.poke(),
        )),
        None => trust,
    };

    let queue = PairingQueue::new();
    let auth = Arc::new(Authenticator::new(
        Arc::clone(&identity),
        Arc::clone(&trust),
        Arc::clone(&queue),
        label.clone(),
    ));

    if let Some(hex_id) = opt("--trust") {
        match parse_client(&hex_id) {
            Ok(client) => {
                let name = opt("--label-for").unwrap_or_else(|| "trusted by hand".into());
                match auth.trust_now(client, &name) {
                    Ok(()) => println!("trusted {}", client.short()),
                    Err(e) => {
                        eprintln!("zest-daemon: {e}");
                        std::process::exit(1);
                    }
                }
            }
            Err(e) => {
                eprintln!("zest-daemon: {e}");
                std::process::exit(1);
            }
        }
        return;
    }

    if let Some(hex_id) = opt("--forget") {
        match parse_client(&hex_id).and_then(|c| {
            trust.remove(c).map(|existed| (c, existed)).map_err(|e| e.to_string())
        }) {
            Ok((client, true)) => println!("forgot {}", client.short()),
            Ok((client, false)) => println!("{} was not trusted", client.short()),
            Err(e) => {
                eprintln!("zest-daemon: {e}");
                std::process::exit(1);
            }
        }
        return;
    }

    if flag("--trusted") {
        match trust.list() {
            Ok(records) if records.is_empty() => println!("no clients are trusted"),
            Ok(records) => {
                for r in records {
                    println!("{}  {}", r.client.short(), r.label);
                }
            }
            Err(e) => {
                eprintln!("zest-daemon: {e}");
                std::process::exit(1);
            }
        }
        return;
    }

    let listen_lan = flag("--listen-lan");

    // Where the relay comes from, if anywhere (#229). An enrolled machine is a
    // machine its owner wants reachable, so holding a cloud token is the
    // instruction — no flag required, `--no-relay` to opt out. The decision is
    // a pure function over six inputs because a table of six booleans reasoned
    // about in prose is how the undiscoverable `--relay` requirement happened
    // in the first place. → relay_origin.rs.
    let control_plane =
        opt("--control-plane").unwrap_or_else(|| enroll::DEFAULT_CONTROL_PLANE.to_string());
    let origin_cache = zest_daemon::relay_origin::OriginCache::at(
        zest_daemon::relay_origin::OriginCache::default_path(),
    );
    let relay_choice = {
        let explicit = opt("--relay").or_else(|| opt("--relay-url"));
        if flag("--no-relay") && explicit.is_some() {
            // Both given: the safe direction wins, and it is said out loud
            // rather than resolved silently, because the person meant one of
            // them and cannot tell which one took effect from the outside.
            tracing::warn!("--no-relay overrides --relay; this machine will not dial a relay");
        }
        zest_daemon::relay_origin::choose(&zest_daemon::relay_origin::RelayInputs {
            ephemeral,
            explicit,
            no_relay: flag("--no-relay"),
            // Presence only. Reading the token here would be a keychain prompt
            // on the startup path; `stored_token` is the same call `--account`
            // makes and answers `Ok(None)` for a machine that never enrolled.
            enrolled: matches!(enroll::stored_token(store.as_ref()), Ok(Some(_))),
            cached: origin_cache.load(),
        })
    };

    let config = DaemonConfig {
        host: identity.host_id(),
        label,
        local_socket: socket.clone(),
        listen_lan,
        lan_bind: opt("--lan-bind").unwrap_or_else(|| "0.0.0.0".into()),
        lan_port: opt("--lan-port")
            .and_then(|p| p.parse().ok())
            .unwrap_or(zest_daemon::lan::DEFAULT_PORT),
        listen_ws: flag("--listen-ws"),
        ws_bind: opt("--ws-bind").unwrap_or_else(|| "0.0.0.0".into()),
        ws_port: opt("--ws-port")
            .and_then(|p| p.parse().ok())
            .unwrap_or(zest_daemon::ws::DEFAULT_PORT),
        // What this start can dial *now*, with nothing to wait for: the
        // `--relay` flag, or an origin a previous start cached. Still no
        // built-in URL — the relay a machine dials is a property of the
        // account, and a constant here would be a guess presented as
        // configuration. When the answer has to be fetched, this is `None` and
        // the link comes up on a worker below, after the listeners.
        relay: match &relay_choice {
            zest_daemon::relay_origin::RelayChoice::Dial(url) => Some(url.clone()),
            _ => None,
        },
        shell_integration: !flag("--no-shell-integration"),
        // Zero unless asked. The relay transport is what needs a floor, and it
        // sets its own; a flag exists so the effect can be seen on loopback
        // without a Cloudflare account. → `DaemonConfig::min_delta_interval`.
        min_delta_interval: opt("--min-delta-interval")
            .and_then(|ms| ms.parse().ok())
            .map_or(std::time::Duration::ZERO, std::time::Duration::from_millis),
        // The serving path's `--enroll` (issue #227): same URL, same trust
        // anchors, same store — and absent under --ephemeral for the same
        // refusal `--enroll` itself makes above.
        enroll: (!ephemeral).then(|| zest_daemon::EnrollSeam {
            base_url: opt("--control-plane")
                .unwrap_or_else(|| enroll::DEFAULT_CONTROL_PLANE.to_string()),
            http: Arc::new(enroll::HttpsControlPlane::new(Roots::Platform)),
            secrets: Arc::clone(&store) as Arc<dyn zest_mesh::keystore::SecretStore>,
        }),
    };

    tracing::info!(
        socket = %socket,
        label = %config.label,
        host = %identity.host_id().short(),
        trust = %trust.describe(),
        "starting"
    );

    // Every one-shot has returned; this process is the daemon. Fetch on start,
    // then poll, hurried by trust-store misses. → attest_sync.rs.
    if let Some(sync) = attest {
        sync.spawn(Arc::clone(&file_trust), Arc::clone(&queue));
    }

    // Someone has to be able to say yes. Without an approver, an unknown client
    // waits two minutes and is denied -- correct, and useless if there is
    // nobody to ask.
    if !flag("--no-prompt") {
        // Recorded beside the trust store, not on stdout: an authorization
        // decision whose evidence an operator can erase with a `>` is not
        // evidence. → audit.rs.
        let audit = Arc::new(zest_daemon::audit::Audit::beside(
            &opt("--trust-file").map_or_else(default_trust_path, std::path::PathBuf::from),
        ));
        start_approver(&queue, &auth, audit);
    }

    let registry = Arc::new(Registry::new());

    // One gate for every public transport, because what it bounds -- threads
    // held by connections that have not proved themselves -- belongs to the
    // process and not to a socket. → `lan::Gate`.
    let gate = Arc::new(zest_daemon::Gate::new());

    // The LAN, if it was asked for. Held for the life of the process: the
    // advertiser unregisters on drop, so `let _ = ...` would take this machine
    // off every fleet listing the instant it was announced.
    let advertiser = if config.listen_lan {
        match start_lan(&config, &registry, &auth, &gate) {
            Ok(a) => a,
            Err(e) => {
                // Refused, not degraded. Every failure here means this host
                // cannot serve the LAN *correctly*, and serving it incorrectly
                // is how shells get handed out. Loopback keeps running, so the
                // desktop app still works while someone reads the error.
                tracing::error!(error = %e, "not serving the LAN; loopback is unaffected");
                None
            }
        }
    } else {
        None
    };

    // The WebSocket port, if asked for. Same failure posture as the LAN:
    // refused, not degraded, and loopback keeps working while someone reads
    // the error.
    if config.listen_ws {
        if let Err(e) = start_ws(&config, &registry, &auth, &gate) {
            tracing::error!(error = %e, "not serving WebSocket clients; loopback is unaffected");
        }
    }

    // The relay, if asked for. Same posture again, and it matters most here:
    // the relay is the transport that depends on somebody else's cloud being
    // up, and ADR-005 requires a local terminal to survive Cloudflare being
    // unreachable. A relay that will not start must therefore cost this daemon
    // nothing but a log line.
    if let Some(url) = config.relay.clone() {
        if let Err(e) = start_relay(&url, &identity, &config, &registry, &auth, &gate) {
            tracing::error!(
                error = %e,
                "not dialling the relay; loopback and the LAN are unaffected"
            );
        }
    }

    // The account's relay, when this start had nothing cached to dial — and a
    // refresh when it did. **After** everything above, on a thread of its own,
    // because the answer comes from the network and ADR-005 says a local
    // terminal survives Cloudflare being unreachable: the listeners are
    // already serving by the time this runs, and a failure here costs a log
    // line. → relay_origin.rs.
    match &relay_choice {
        zest_daemon::relay_origin::RelayChoice::Off(why) => {
            tracing::debug!(?why, "not dialling a relay");
        }
        choice => {
            let asking = matches!(choice, zest_daemon::relay_origin::RelayChoice::AskAccount);
            // A `--relay` override is not refreshed: the flag is the answer,
            // and asking the account what it thinks would only produce a log
            // line contradicting the command line.
            let refreshing = matches!(choice, zest_daemon::relay_origin::RelayChoice::Dial(_))
                && opt("--relay").or_else(|| opt("--relay-url")).is_none();
            if asking || refreshing {
                start_relay_discovery(
                    asking,
                    origin_cache,
                    &control_plane,
                    &store,
                    &identity,
                    &config,
                    &registry,
                    &auth,
                    &gate,
                );
            }
        }
    }

    // A signal runs no destructors, so without this the goodbye only goes out
    // when the process *returns* -- which a daemon never does. Every pkill,
    // Ctrl+C or service-manager stop then leaves this host on every fleet
    // listing until the record's TTL expires (75 minutes for the PTR record,
    // and mdns-sd exposes no way to shorten it) -- a row that resolves, refuses
    // every dial, and is indistinguishable from a sleeping laptop. That is #22,
    // and it cost two pairing windows in one afternoon.
    //
    // SIGKILL, TerminateProcess and crashes still leak the record; the honest
    // fix for those is reachability-as-presence in the fleet listing, recorded
    // in #22 rather than half-done here.
    let advertiser = Arc::new(std::sync::Mutex::new(advertiser));
    {
        let advertiser = Arc::clone(&advertiser);
        if let Err(e) = ctrlc::set_handler(move || {
            if let Ok(mut slot) = advertiser.lock() {
                if let Some(mut a) = slot.take() {
                    tracing::info!("withdrawing the advertisement before exiting");
                    a.stop();
                }
            }
            std::process::exit(0);
        }) {
            // The daemon still works; it just dies rudely, as it always did.
            tracing::warn!(error = %e, "no signal handler; a killed daemon will advertise until TTL");
        }
    }

    if let Err(e) = listen(&socket, config, registry, auth) {
        tracing::error!(error = %e, "could not listen");
        std::process::exit(1);
    }
}

/// Bind, advertise, and start serving other machines.
///
/// In that order, and the order is the point: the port must be known before it
/// is announced, because SRV is the single source of truth for it and a second
/// source that can disagree produces a connection refused with no obvious
/// cause.
fn start_lan(
    config: &DaemonConfig,
    registry: &Arc<Registry>,
    auth: &Arc<Authenticator>,
    gate: &Arc<zest_daemon::Gate>,
) -> Result<Option<zest_mesh::discovery::mdns::MdnsAdvertiser>, String> {
    // A trust store that cannot persist means every device re-prompts on every
    // reconnect, and a stream of prompts is how someone learns to approve
    // without reading. Checked before the port is opened, not after.
    auth.trust().list().map_err(|e| format!("the trust store is unusable: {e}"))?;

    let listener = zest_daemon::LanListener::bind(&config.lan_bind, config.lan_port)
        .map_err(|e| e.to_string())?;
    let addr = listener.local_addr();

    let advertiser =
        zest_mesh::discovery::mdns::MdnsAdvertiser::start(config.host, &config.label, addr.port())
            .map_err(|e| format!("could not advertise on the LAN: {e}"))?;

    tracing::info!(
        %addr,
        host = %config.host.short(),
        "serving the LAN; unpaired devices will have to be approved"
    );

    let config = config.clone();
    let registry = Arc::clone(registry);
    let auth = Arc::clone(auth);
    let gate = Arc::clone(gate);
    std::thread::Builder::new()
        .name("zest-daemon-lan".into())
        .spawn(move || {
            if let Err(e) = listener.serve_forever(config, registry, auth, gate) {
                tracing::error!(error = %e, "the LAN listener stopped");
            }
        })
        .map_err(|e| e.to_string())?;

    Ok(Some(advertiser))
}

/// Bind and start serving browsers.
///
/// No mDNS advertisement, unlike the LAN: a browser cannot browse mDNS, so
/// the web client is handed this address by its control plane rather than
/// discovering it. Advertising a port nothing can discover through would be
/// one more record to go stale.
fn start_ws(
    config: &DaemonConfig,
    registry: &Arc<Registry>,
    auth: &Arc<Authenticator>,
    gate: &Arc<zest_daemon::Gate>,
) -> Result<(), String> {
    // Same check as the LAN, same reason: a trust store that cannot persist
    // means every device re-prompts on every reconnect.
    auth.trust().list().map_err(|e| format!("the trust store is unusable: {e}"))?;

    let listener = zest_daemon::WsListener::bind(&config.ws_bind, config.ws_port)
        .map_err(|e| e.to_string())?;
    let addr = listener.local_addr();
    tracing::info!(
        %addr,
        host = %config.host.short(),
        "serving WebSocket clients; browsers dial ws://<this host>:{}",
        addr.port()
    );

    let config = config.clone();
    let registry = Arc::clone(registry);
    let auth = Arc::clone(auth);
    let gate = Arc::clone(gate);
    std::thread::Builder::new()
        .name("zest-daemon-ws".into())
        .spawn(move || {
            if let Err(e) = listener.serve_forever(config, registry, auth, gate) {
                tracing::error!(error = %e, "the WebSocket listener stopped");
            }
        })
        .map_err(|e| e.to_string())?;

    Ok(())
}

/// Park a control link at the relay, and keep it parked.
///
/// Modelled on [`start_ws`] and different in one way that shows in the
/// signature: there is no listener and no port, because this transport dials
/// *out*. Everything else is deliberately the same — the same registry, the
/// same authenticator, and above all the same [`Gate`](zest_daemon::Gate), so
/// that a relayed stream counts against the same mid-handshake budget a LAN
/// connection does.
///
/// The URL is parsed here rather than inside the thread, which is what makes a
/// mistyped `--relay` a message at startup instead of a thread failing forever
/// in a log nobody is reading.
fn start_relay(
    url: &str,
    identity: &Arc<HostIdentity>,
    config: &DaemonConfig,
    registry: &Arc<Registry>,
    auth: &Arc<Authenticator>,
    gate: &Arc<zest_daemon::Gate>,
) -> Result<(), String> {
    // Same check as the LAN and the WebSocket port, same reason: a trust store
    // that cannot persist means every device re-prompts on every reconnect.
    auth.trust().list().map_err(|e| format!("the trust store is unusable: {e}"))?;

    // The operating system's trust store, as `--enroll` uses: it is the only
    // thing that accepts a corporate middlebox's re-signed certificate, and a
    // managed laptop is exactly the machine that needs a relay.
    let relay = zest_daemon::Relay::new(
        url,
        Arc::clone(identity),
        config.clone(),
        Arc::clone(registry),
        Arc::clone(auth),
        Arc::clone(gate),
        Roots::Platform,
    )
    .map_err(|e| e.to_string())?;

    tracing::info!(
        relay = %relay.origin(),
        host = %config.host.short(),
        "dialling the relay; this machine will be reachable through it"
    );

    std::thread::Builder::new()
        .name("zest-daemon-relay".into())
        .spawn(move || relay.run())
        .map_err(|e| e.to_string())?;

    Ok(())
}

/// Learn the account's relay origin, and bring the link up once it is known.
///
/// On a thread, and after the listeners: the origin comes from the network, so
/// doing it inline would put somebody else's cloud on this daemon's startup
/// path — the one thing ADR-005 says a local terminal must never depend on.
///
/// `dial_when_found` separates the two jobs this does. When nothing was cached
/// the fetch is what brings the link up, so its answer is dialled immediately.
/// When a cached origin is already dialling, the fetch is only a refresh: a
/// changed answer is written down and takes effect on the next start, because
/// tearing a live control link down and rebuilding it under a running fleet is
/// a much larger change than #229, and an account that moves its relay does it
/// approximately never.
///
/// Retries are the attestation sync's cadence and posture — jittered, failing
/// soft, forever — for its reason: the machine that most needs a relay is the
/// laptop that boots on a train, and giving up after one attempt would leave it
/// unreachable until somebody restarted the daemon.
#[allow(clippy::too_many_arguments)]
fn start_relay_discovery(
    dial_when_found: bool,
    cache: zest_daemon::relay_origin::OriginCache,
    control_plane: &str,
    store: &Arc<dyn CredentialStore>,
    identity: &Arc<HostIdentity>,
    config: &DaemonConfig,
    registry: &Arc<Registry>,
    auth: &Arc<Authenticator>,
    gate: &Arc<zest_daemon::Gate>,
) {
    let Some(source) = zest_daemon::relay_origin::HttpsOriginSource::new(
        store.as_ref(),
        control_plane,
        Roots::Platform,
    ) else {
        return;
    };

    let identity = Arc::clone(identity);
    let config = config.clone();
    let registry = Arc::clone(registry);
    let auth = Arc::clone(auth);
    let gate = Arc::clone(gate);
    let spawned = std::thread::Builder::new()
        .name("zest-daemon-relay-origin".into())
        .spawn(move || {
            use zest_daemon::relay_origin::OriginSource as _;
            let mut attempt = 0u32;
            loop {
                match source.relay_origin() {
                    Ok(Some(origin)) => {
                        match cache.store(&origin) {
                            Ok(true) if !dial_when_found => tracing::info!(
                                relay = %origin,
                                "the account moved its relay; dialling it from the next start"
                            ),
                            Ok(_) => {}
                            Err(e) => tracing::warn!(
                                error = %e,
                                "could not cache the relay origin; the next start will ask again"
                            ),
                        }
                        if dial_when_found {
                            if let Err(e) =
                                start_relay(&origin, &identity, &config, &registry, &auth, &gate)
                            {
                                tracing::error!(
                                    error = %e,
                                    relay = %origin,
                                    "not dialling the account's relay; loopback and the LAN are unaffected"
                                );
                            }
                        }
                        return;
                    }
                    Ok(None) => {
                        // An answer, not a failure: this deployment has no
                        // relay. Retrying would be asking a settled question
                        // every five minutes for the life of the process.
                        tracing::info!(
                            "the account publishes no relay; this machine is reachable on the LAN only"
                        );
                        return;
                    }
                    Err(e) => {
                        attempt += 1;
                        tracing::warn!(
                            error = %e,
                            attempt,
                            "could not learn the account's relay origin; will retry"
                        );
                    }
                }
                // Jittered, so every daemon on an account that came back after
                // an outage does not ask again in step.
                std::thread::sleep(retry_backoff(attempt));
            }
        });
    if let Err(e) = spawned {
        tracing::warn!(error = %e, "no thread to learn the account's relay origin");
    }
}

/// Five minutes, then fifteen, jittered ±25%.
///
/// The attestation sync's ladder, deliberately: both ask the same control
/// plane on behalf of the same machine, and two schedules would mean two
/// answers to "how hard does an enrolled daemon lean on the account".
fn retry_backoff(attempt: u32) -> std::time::Duration {
    let base = if attempt <= 1 {
        std::time::Duration::from_secs(5 * 60)
    } else {
        std::time::Duration::from_secs(15 * 60)
    };
    let fraction = zest_mesh::identity::Nonce::random()
        .map_or(0.5, |n| f64::from(n.as_bytes()[0]) / 255.0);
    base.mul_f64(0.75 + 0.5 * fraction)
}

/// Ask on stdin when a device wants to pair.
///
/// One of three front ends over the same queue — this, `--trust`, and later the
/// desktop modal — which is what lets the modal land as a front-end swap rather
/// than a second mechanism.
fn start_approver(
    queue: &Arc<PairingQueue>,
    auth: &Arc<Authenticator>,
    audit: Arc<zest_daemon::audit::Audit>,
) {
    let queue_for_hook = Arc::clone(queue);
    let auth = Arc::clone(auth);

    // Woken by the queue rather than polling: a daemon that wakes to check for
    // nothing is a laptop that does not sleep.
    let (tx, rx) = std::sync::mpsc::channel::<()>();
    queue.on_request(move || {
        let _ = tx.send(());
    });

    std::thread::Builder::new()
        .name("zest-daemon-approver".into())
        .spawn(move || {
            // Lines, with the moment each arrived.
            //
            // The timestamp is the whole fix. `Stdin` is globally buffered, so a
            // surplus line outlives the prompt it was meant for and is handed to
            // the next `read_line` — which is how a device nobody looked at was
            // approved 81 microseconds after connecting, by a `y` typed four
            // minutes earlier for a different device on another machine (#21).
            //
            // Checking that the request is still pending does not catch it: the
            // request *is* pending, that is why it is being asked about. The
            // thing that is wrong is the order — the answer predates the
            // question. **Consent cannot precede the prompt**, because you
            // cannot compare a code you have not been shown.
            let (lines_tx, lines) = std::sync::mpsc::channel::<(std::time::Instant, String)>();
            std::thread::Builder::new()
                .name("zest-daemon-stdin".into())
                .spawn(move || {
                    use std::io::BufRead as _;
                    let stdin = std::io::stdin();
                    for line in stdin.lock().lines() {
                        let Ok(line) = line else { break };
                        if lines_tx.send((std::time::Instant::now(), line)).is_err() {
                            break;
                        }
                    }
                })
                .ok();

            while rx.recv().is_ok() {
                let waiting = queue_for_hook.pending();
                // A notification with nothing pending means the request was
                // already resolved -- expired, or answered elsewhere. Worth
                // seeing, because it is one of the ways a prompt and the queue
                // can disagree about what is being asked.
                if waiting.is_empty() {
                    tracing::debug!("woken with no pending request");
                }
                for request in waiting {
                    // Both halves are shown, because they answer different
                    // questions: `short()` says what claims to be connecting,
                    // the code says that this is the connection you are looking
                    // at right now.
                    println!(
                        "\npair \"{}\" ({}) from {}?\n  code {}\n  [y/N] ",
                        request.label,
                        request.client.short(),
                        request.remote,
                        spaced(&request.code),
                    );
                    let asked_at = std::time::Instant::now();
                    let mut line = String::new();
                    // `Ok(0)` *is* EOF -- `read_line` never reports it as an
                    // error. Testing only `is_err()` meant a daemon whose stdin
                    // is /dev/null, which is exactly how the app spawns it and
                    // how launchd and systemd start it, fell through with an
                    // empty line and denied every request instantly. The user
                    // saw "the request was declined" the moment they tried to
                    // pair, and the comment here claimed the opposite.
                    // Anything already waiting was typed before this prompt
                    // existed, so it is not an answer to it.
                    let answer = loop {
                        match lines.recv() {
                            Ok((at, text)) if at >= asked_at => break Some(text),
                            Ok((_, stale)) => {
                                audit.record(
                                    std::time::SystemTime::now(),
                                    &zest_daemon::audit::Entry {
                                        client: &request.client.short(),
                                        label: &request.label,
                                        remote: &request.remote,
                                        code: &request.code,
                                        outcome: zest_daemon::audit::Outcome::Discarded,
                                        by_person: false,
                                    },
                                );
                                tracing::warn!(
                                client = %request.client.short(),
                                discarded = %stale.trim().escape_debug().to_string(),
                                "input that predates this prompt is not an answer to it; \
                                 discarding rather than letting it approve a device \
                                 nobody was shown"
                                );
                            }
                            Err(_) => break None,
                        }
                    };
                    match answer.map(|a| { line = a; 1usize }).ok_or(()) {
                        Err(()) | Ok(0) => {
                            tracing::warn!(
                                "no stdin to prompt on; pairing requests will time out. \
                                 Use --trust <id> to pair without a prompt, or \
                                 --no-prompt to refuse immediately."
                            );
                            return;
                        }
                        Ok(_) => {}
                    }
                    // The answer is for *this* request, or it is for nothing.
                    //
                    // An answer that arrives after its request has gone is not a
                    // decision about whatever came next -- it is a decision about
                    // something that no longer exists, and carrying it forward is
                    // what made a stale `y` grant a shell. Re-checking here costs
                    // one lookup and closes that door even if a line is somehow
                    // still buffered.
                    if !queue_for_hook.pending().iter().any(|r| r.client == request.client) {
                        tracing::warn!(
                            client = %request.client.short(),
                            answer = %line.trim().escape_debug().to_string(),
                            "an answer arrived for a request that is no longer pending; \
                             discarding it rather than applying it to another device"
                        );
                        continue;
                    }
                    let approve = matches!(line.trim(), "y" | "Y" | "yes");
                    // Instrumented because answers have gone missing (#21): a
                    // pairing that fails because the approval was swallowed is
                    // indistinguishable, from every log this daemon writes, from
                    // one where nobody answered. Logging what was *read* and for
                    // *which* request is what tells those apart -- an empty line
                    // here means something consumed the answer, and a client id
                    // that is not the one on screen means the queue moved under
                    // the prompt.
                    tracing::info!(
                        client = %request.client.short(),
                        answer = %line.trim().escape_debug().to_string(),
                        bytes = line.len(),
                        approve,
                        "pairing answer read"
                    );
                    // `by_person` is true because the line reached here only by
                    // arriving after the prompt was printed -- which is the
                    // whole distinction #21 was about.
                    audit.record(
                        std::time::SystemTime::now(),
                        &zest_daemon::audit::Entry {
                            client: &request.client.short(),
                            label: &request.label,
                            remote: &request.remote,
                            code: &request.code,
                            outcome: if approve {
                                zest_daemon::audit::Outcome::Approved
                            } else {
                                zest_daemon::audit::Outcome::Refused
                            },
                            by_person: true,
                        },
                    );
                    auth.decide(
                        request.client,
                        if approve { Decision::Approve } else { Decision::Deny },
                    );
                }
            }
        })
        .ok();
}

/// `428913` as `428 913`, which is how a person reads it aloud.
fn spaced(code: &str) -> String {
    let mid = code.len() / 2;
    format!("{} {}", &code[..mid], &code[mid..])
}

/// Where pairings are recorded.
///
/// A `fleet/` subdirectory rather than the config root: `zest-config` watches
/// the config directory non-recursively, so a pairing write here cannot even
/// produce an event the watcher has to filter out.
fn default_trust_path() -> std::path::PathBuf {
    directories::ProjectDirs::from("dev", "zesterm", "zesterm").map_or_else(
        || std::path::PathBuf::from("zesterm-trust.toml"),
        |dirs| dirs.config_dir().join("fleet").join("trust.toml"),
    )
}

fn parse_client(text: &str) -> Result<ClientId, String> {
    let text = text.trim();
    if text.len() != 64 {
        return Err(format!(
            "a client id is 64 hex characters; got {} in {text:?}",
            text.len()
        ));
    }
    let mut out = [0u8; 32];
    for (i, pair) in text.as_bytes().chunks_exact(2).enumerate() {
        let s = std::str::from_utf8(pair).map_err(|_| "not hex".to_string())?;
        out[i] = u8::from_str_radix(s, 16).map_err(|_| format!("not hex: {s}"))?;
    }
    Ok(ClientId::from_bytes(out))
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// A human name for this machine, for the fleet listing.
/// What this machine calls itself.
///
/// The fleet listing's label, and now the name a device enrols under — so
/// getting it wrong is not cosmetic: every Mac in the fleet showed as
/// `unnamed`.
///
/// It used to read `COMPUTERNAME` then `HOSTNAME`. **`HOSTNAME` is a shell
/// variable, not an exported one**: bash and zsh set it for themselves and
/// never put it in the environment, so a daemon — which is spawned, not run
/// from a prompt — never sees it. `COMPUTERNAME` is Windows-only. On this Mac
/// neither is present, so the fallback was not a fallback, it was the answer.
///
/// `uname()` is what actually knows, and `rustix` is already a dependency.
/// The environment variables stay ahead of it, because a person who exports
/// one is asking for it deliberately.
fn machine_label() -> String {
    machine_label_from(|k| std::env::var(k).ok())
}

/// What the operating system calls this machine, asked directly.
///
/// Both arms exist because the test that matters runs with no environment at
/// all, and a platform that could only answer from `COMPUTERNAME` would fail
/// it — which is how the original bug got in: Windows genuinely does export
/// that variable, so the unix hole was invisible from there.
#[cfg(unix)]
fn os_hostname() -> Option<String> {
    Some(rustix::system::uname().nodename().to_string_lossy().into_owned())
}

#[cfg(windows)]
fn os_hostname() -> Option<String> {
    use windows_sys::Win32::System::SystemInformation::{
        ComputerNameDnsHostname, GetComputerNameExW,
    };

    let mut len: u32 = 0;
    // First call sizes the buffer; it is expected to fail with the length set.
    unsafe { GetComputerNameExW(ComputerNameDnsHostname, std::ptr::null_mut(), &mut len) };
    if len == 0 {
        return None;
    }

    let mut buf = vec![0u16; len as usize];
    let ok = unsafe { GetComputerNameExW(ComputerNameDnsHostname, buf.as_mut_ptr(), &mut len) };
    if ok == 0 {
        return None;
    }
    buf.truncate(len as usize);
    Some(String::from_utf16_lossy(&buf))
}

/// The lookup, with the environment injected.
///
/// Split so the fallback can be tested against the environment a daemon
/// actually has — neither variable set — without mutating process-global state
/// from a test that runs in parallel with others. With the variables set, the
/// broken version passed too, so testing it any other way proves nothing.
fn machine_label_from(env: impl Fn(&str) -> Option<String>) -> String {
    for var in ["COMPUTERNAME", "HOSTNAME"] {
        if let Some(name) = env(var) {
            if !name.is_empty() {
                return name;
            }
        }
    }

    if let Some(name) = os_hostname() {
        // Trim the domain: `andy-mac.local` is the mDNS form, and the label is
        // for a person reading a list rather than for resolution.
        let short = name.split('.').next().unwrap_or("");
        if !short.is_empty() {
            return short.to_string();
        }
    }

    "unnamed".to_string()
}

#[cfg(test)]
mod label_tests {
    use super::machine_label_from;

    #[test]
    fn a_machine_knows_its_own_name_without_help_from_the_environment() {
        // The bug this replaced: `HOSTNAME` is a *shell* variable and is never
        // exported, and `COMPUTERNAME` is Windows-only -- so on macOS the
        // lookup fell through to "unnamed" every time. That was already the
        // daemon's `--label` default, so every Mac in the fleet listing showed
        // as `unnamed`, and it would have become the name every Mac enrolled
        // under.
        //
        // The empty environment is the point: with either variable set, the
        // broken version passed too.
        let label = machine_label_from(|_| None);
        assert_ne!(
            label, "unnamed",
            "a machine must name itself from uname(), not from a shell variable \
             a daemon never receives"
        );
        assert!(!label.contains('.'), "the domain is trimmed: `{label}`");
        assert!(!label.is_empty());
    }

    #[test]
    fn an_exported_name_still_wins() {
        // Someone who exports one is asking for it deliberately.
        assert_eq!(
            machine_label_from(|k| (k == "HOSTNAME").then(|| "chosen-by-hand".to_string())),
            "chosen-by-hand"
        );
        assert_eq!(
            machine_label_from(|k| (k == "COMPUTERNAME").then(|| "win-box".to_string())),
            "win-box"
        );
    }

    #[test]
    fn an_empty_variable_is_not_a_name() {
        // An exported-but-empty HOSTNAME is common in stripped environments and
        // must not win over the OS.
        //
        // Compared against the *injected* empty environment, not against
        // `machine_label()`. That reads the real process environment, so on any
        // machine where `COMPUTERNAME` or `HOSTNAME` is genuinely set the two
        // sides differ and the test fails for a reason that has nothing to do
        // with what it is checking. It passes on CI only because neither
        // variable is set there -- a test whose result depends on the
        // environment of whoever runs it.
        assert_ne!(machine_label_from(|_| Some(String::new())), "");
        assert_eq!(
            machine_label_from(|_| Some(String::new())),
            machine_label_from(|_| None),
            "an empty variable must fall through exactly as an absent one does"
        );
    }
}
