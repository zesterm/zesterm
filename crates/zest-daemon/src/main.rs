//! `zest-daemon` — serves this machine's terminals.
//!
//! Owns the PTYs and hands their grids to whoever attaches: the local GUI app
//! over a loopback socket, other machines over the LAN or a tunnel. Sessions
//! outlive every client, which is the entire reason this is a separate process.
//! → ADR-007.

use std::sync::Arc;

use zest_daemon::{default_socket_path, listen, Authenticator, DaemonConfig, Registry};
use zest_mesh::identity::HostIdentity;
use zest_mesh::keystore::{KeyStore, MemoryKeyStore};
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
             --lan-port <port>   preferred port (default 7717)\n\n\
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
    let store: Box<dyn KeyStore> = if ephemeral {
        // For the edit-run loop. `keyring` keys access to the *binary*, so on
        // macOS every rebuild re-prompts for the keychain -- and the outcome of
        // that is a developer who turns authentication off to get work done.
        Box::new(MemoryKeyStore::new())
    } else {
        match zest_mesh::keystore::OsKeyStore::new() {
            Ok(s) => Box::new(s),
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

    let queue = PairingQueue::new();
    let label = opt("--label").unwrap_or_else(machine_label);
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
    let config = DaemonConfig {
        host: identity.host_id(),
        label,
        local_socket: socket.clone(),
        listen_lan,
        lan_bind: opt("--lan-bind").unwrap_or_else(|| "0.0.0.0".into()),
        lan_port: opt("--lan-port")
            .and_then(|p| p.parse().ok())
            .unwrap_or(zest_daemon::lan::DEFAULT_PORT),
        shell_integration: !flag("--no-shell-integration"),
    };

    tracing::info!(
        socket = %socket,
        label = %config.label,
        host = %identity.host_id().short(),
        trust = %trust.describe(),
        "starting"
    );

    // Someone has to be able to say yes. Without an approver, an unknown client
    // waits two minutes and is denied -- correct, and useless if there is
    // nobody to ask.
    if !flag("--no-prompt") {
        start_approver(&queue, &auth);
    }

    let registry = Arc::new(Registry::new());

    // The LAN, if it was asked for. Held for the life of the process: the
    // advertiser unregisters on drop, so `let _ = ...` would take this machine
    // off every fleet listing the instant it was announced.
    let _advertiser = if config.listen_lan {
        match start_lan(&config, &registry, &auth) {
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
    std::thread::Builder::new()
        .name("zest-daemon-lan".into())
        .spawn(move || {
            if let Err(e) = listener.serve_forever(config, registry, auth) {
                tracing::error!(error = %e, "the LAN listener stopped");
            }
        })
        .map_err(|e| e.to_string())?;

    Ok(Some(advertiser))
}

/// Ask on stdin when a device wants to pair.
///
/// One of three front ends over the same queue — this, `--trust`, and later the
/// desktop modal — which is what lets the modal land as a front-end swap rather
/// than a second mechanism.
fn start_approver(queue: &Arc<PairingQueue>, auth: &Arc<Authenticator>) {
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
                            Ok((_, stale)) => tracing::warn!(
                                client = %request.client.short(),
                                discarded = %stale.trim().escape_debug().to_string(),
                                "input that predates this prompt is not an answer to it; \
                                 discarding rather than letting it approve a device \
                                 nobody was shown"
                            ),
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
fn machine_label() -> String {
    for var in ["COMPUTERNAME", "HOSTNAME"] {
        if let Ok(name) = std::env::var(var) {
            if !name.is_empty() {
                return name;
            }
        }
    }
    "unnamed".to_string()
}
