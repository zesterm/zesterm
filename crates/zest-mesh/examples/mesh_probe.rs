//! Advertise this machine, browse for the others, and print what comes back.
//!
//! The two-machine check no unit test can perform. Everything in `zest-mesh`
//! that a socket can break — whether our multicast leaves the box, whether a
//! peer's TXT record parses, whether a lid closing greys a row out — shows up
//! here and nowhere else.
//!
//! ```text
//! cargo run -p zest-mesh --example mesh_probe -- --label andy-mac
//! cargo run -p zest-mesh --example mesh_probe -- --browse-only
//! cargo run -p zest-mesh --example mesh_probe -- --advertise-only --label build-box
//! cargo run -p zest-mesh --example mesh_probe -- --seconds 20
//! ```
//!
//! Cross-check the wire format against an implementation that is not ours,
//! because a bug where two zesterm processes agree on a wrong format is
//! invisible to a zesterm-only test:
//!
//! ```text
//! dns-sd -B _zesterm._tcp                        # macOS
//! dns-sd -L "andy-mac (1f2a3b4c)" _zesterm._tcp  # the SRV port and TXT keys
//! avahi-browse -r _zesterm._tcp                  # Linux
//! ```
//!
//! The line to read first is `self-visible`. `no` after a few seconds means our
//! own advertisement is not coming back through multicast loopback — a
//! firewall, or an interface with no multicast — which looks exactly like "the
//! other machine is not running" if you only look at the peer list.

use std::time::{Duration, Instant};

use zest_mesh::discovery::mdns::{node, MdnsAdvertiser, MdnsDiscovery};
use zest_mesh::discovery::{Discovery, Presence};
use zest_mesh::identity::HostIdentity;
use zest_mesh::keystore::MemoryKeyStore;

const DEFAULT_PORT: u16 = 7717;

struct Options {
    label: String,
    port: u16,
    advertise: bool,
    browse: bool,
    seconds: u64,
}

fn main() {
    let options = match parse_args() {
        Ok(options) => options,
        Err(message) => {
            eprintln!("{message}\n");
            eprintln!(
                "usage: mesh_probe [--label NAME] [--port N] [--seconds N] \
                 [--browse-only | --advertise-only]"
            );
            std::process::exit(2);
        }
    };

    // A per-run identity from an in-memory store: this is a diagnostic, and it
    // has no business minting a key into the machine's real keychain or
    // adopting the daemon's. Two runs on one box are therefore two hosts, which
    // is what makes the loopback check below meaningful.
    let identity = HostIdentity::load_or_create(&MemoryKeyStore::new())
        .expect("an in-memory store cannot fail");
    let host = identity.host_id();

    println!("host  {}  ({})", host.short(), zest_mesh::identity::HOST_KEY_ALGORITHM);
    println!("label {}", options.label);

    let (advertiser, discovery) = match (options.advertise, options.browse) {
        (true, true) => match node(host, &options.label, options.port) {
            Ok((a, d)) => (Some(a), Some(d)),
            Err(e) => fail(&e),
        },
        (true, false) => match MdnsAdvertiser::start(host, &options.label, options.port) {
            Ok(a) => (Some(a), None),
            Err(e) => fail(&e),
        },
        (false, true) => {
            let mut d = MdnsDiscovery::new(host);
            if let Err(e) = d.start() {
                fail(&e);
            }
            (None, Some(d))
        }
        (false, false) => {
            eprintln!("--browse-only and --advertise-only cannot both be given");
            std::process::exit(2);
        }
    };

    if let Some(a) = &advertiser {
        println!("advertising as {}", a.fullname());
    }
    println!();

    let deadline = Instant::now() + Duration::from_secs(options.seconds);
    while Instant::now() < deadline {
        std::thread::sleep(Duration::from_secs(1));
        if let Some(d) = &discovery {
            render(d);
        }
    }
}

fn render(discovery: &MdnsDiscovery) {
    let records = discovery.records();
    println!(
        "── self-visible: {}   peers: {}",
        if discovery.self_seen() { "yes" } else { "no " },
        records.len()
    );
    if records.is_empty() {
        let hint = if discovery.self_seen() {
            "our multicast is reaching the link, so nothing else is advertising"
        } else {
            "our own advertisement has not come back — suspect a firewall or an \
             interface with no multicast, not the other machine"
        };
        println!("   ({hint})");
        return;
    }
    println!("   {:<10} {:<16} {:<9} {:<10} ENDPOINTS", "ID", "LABEL", "PRESENCE", "LAST SEEN");
    for record in records {
        let presence = match record.presence {
            Presence::Online => "online",
            Presence::Away => "away",
            Presence::Unseen => "unseen",
        };
        let last_seen = record
            .last_seen
            .map_or_else(|| "-".to_string(), |t| format!("{:.1}s", t.elapsed().as_secs_f32()));
        let endpoints = if record.peer.endpoints.is_empty() {
            "(none)".to_string()
        } else {
            record
                .peer
                .endpoints
                .iter()
                .map(|e| e.address.clone())
                .collect::<Vec<_>>()
                .join(", ")
        };
        println!(
            "   {:<10} {:<16} {:<9} {:<10} {}",
            record.peer.host.short(),
            record.peer.label,
            presence,
            last_seen,
            endpoints
        );
    }
}

fn fail(error: &zest_mesh::MeshError) -> ! {
    eprintln!("mesh_probe: {error}");
    std::process::exit(1);
}

fn parse_args() -> Result<Options, String> {
    let mut options = Options {
        label: default_label(),
        port: DEFAULT_PORT,
        advertise: true,
        browse: true,
        seconds: 60,
    };
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--label" => {
                options.label = args.next().ok_or("--label needs a value")?;
            }
            "--port" => {
                options.port = args
                    .next()
                    .ok_or("--port needs a value")?
                    .parse()
                    .map_err(|_| "--port must be a number")?;
            }
            "--seconds" => {
                options.seconds = args
                    .next()
                    .ok_or("--seconds needs a value")?
                    .parse()
                    .map_err(|_| "--seconds must be a number")?;
            }
            "--browse-only" => options.advertise = false,
            "--advertise-only" => options.browse = false,
            other => return Err(format!("unknown argument: {other}")),
        }
    }
    Ok(options)
}

fn default_label() -> String {
    std::env::var("HOSTNAME")
        .or_else(|_| std::env::var("COMPUTERNAME"))
        .unwrap_or_else(|_| "zesterm-probe".to_string())
}
