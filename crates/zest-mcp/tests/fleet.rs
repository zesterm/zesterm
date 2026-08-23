//! One `ToolSet` reaching **two** machines.
//!
//! Two in-process daemons on loopback TCP behind a `StaticFleet` naming both,
//! so the thing under test is the dispatch — which connection a call lands on,
//! what a machine with no route is told, and what a first dial to an untrusted
//! host does — rather than whether this runner can hear a multicast packet or
//! reach a control plane. The live sources have their own unit tests in
//! `src/fleet.rs`.
//!
//! Loopback would ordinarily mean `Auth::Transport`, where the trust store is
//! not consulted at all. The pairing test deliberately serves `Auth::Proof`
//! instead, because the thing it is about — a person approving a key — cannot
//! happen on a transport that already trusts the caller.

use std::net::{TcpListener, TcpStream};
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde_json::{json, Value};
use zest_daemon::{serve, Auth, Authenticator, DaemonConfig, Registry};
use zest_fleet::FleetHost;
use zest_mcp::{Conn, StaticFleet, ToolSet};
use zest_mesh::identity::{ClientIdentity, HostIdentity};
use zest_mesh::pairing::{Decision, PairingQueue};
use zest_mesh::trust::MemoryTrustStore;

/// A command that exits promptly on either platform, printing one line.
fn quiet_cmd() -> String {
    if cfg!(windows) {
        "cmd.exe /c echo mcp".into()
    } else {
        "/bin/sh -c 'echo mcp'".into()
    }
}

/// One machine: a daemon on a loopback port, and the queue a person answers.
struct Machine {
    addr: String,
    host: zest_proto::HostId,
    label: String,
    pairings: Arc<PairingQueue>,
}

/// Stand a daemon up. `gated` serves `Auth::Proof`, so its trust store decides.
fn machine(label: &str, gated: bool) -> Machine {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let addr = listener.local_addr().expect("addr").to_string();
    let identity = Arc::new(HostIdentity::generate().expect("host key"));
    let host = identity.host_id();
    let pairings = PairingQueue::new();

    let config = DaemonConfig {
        host,
        label: label.into(),
        local_socket: String::new(),
        listen_lan: false,
        lan_bind: "127.0.0.1".into(),
        lan_port: 0,
        listen_ws: false,
        ws_bind: "127.0.0.1".into(),
        ws_port: 0,
        relay: None,
        shell_integration: false,
        min_delta_interval: Duration::ZERO,
        enroll: None,
        offer: Some(zest_daemon::offer::OfferSource::new(zest_daemon::offer::facts(
            format!("{label}-shell"),
        ))),
    };
    let registry = Arc::new(Registry::new());

    // One store and one queue across connections, unlike `live.rs`: an approval
    // has to still be there when the *next* connection asks, which is the whole
    // point of a durable key.
    let trust = Arc::new(MemoryTrustStore::new());
    let queue = Arc::clone(&pairings);
    let name = label.to_string();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(stream) = stream else { break };
            let write = stream.try_clone().expect("clone");
            let a = Arc::new(Authenticator::new(
                Arc::clone(&identity),
                Arc::clone(&trust) as Arc<dyn zest_mesh::trust::TrustStore>,
                Arc::clone(&queue),
                &name,
            ));
            let auth = if gated { Auth::Proof(a) } else { Auth::Transport(a) };
            let config = config.clone();
            let reg = Arc::clone(&registry);
            std::thread::spawn(move || {
                let _ = serve(stream, write, config, reg, auth, "in-process");
            });
        }
    });

    Machine { addr, host, label: label.into(), pairings }
}

/// The connection the server is built with — this process's own machine.
fn dial(addr: &str) -> Conn {
    let stream = TcpStream::connect(addr).expect("connect");
    stream.set_read_timeout(Some(Duration::from_secs(20))).expect("read timeout");
    let identity = Arc::new(ClientIdentity::generate().expect("client key"));
    Conn::open(
        Box::new(stream.try_clone().expect("clone")) as Box<dyn std::io::Read + Send>,
        Box::new(stream),
        &identity,
        "zest-mcp agent",
        None,
    )
    .expect("the loopback path welcomes any client that proves a key")
}

/// A fleet row for a machine reachable at `addr` over what this server calls
/// the LAN.
///
/// Through `zest_fleet::fixture` rather than hand-rolled, for the reason that
/// module's doc gives: a `FleetHost` literal in a test is how the next
/// label-keyed lookup stays invisible (#304).
fn row(m: &Machine, local: bool) -> FleetHost {
    let mut h = if local {
        zest_fleet::fixture::local(1, &m.label)
    } else {
        zest_fleet::fixture::host(2, &m.label)
    };
    h.host = m.host;
    if !local {
        h.address = Some(m.addr.clone());
    }
    h
}

fn tools(local: &Machine, fleet: Vec<FleetHost>) -> ToolSet {
    // A memory store, always. The durable `agent-key` lives in the OS
    // credential store, and a test that minted the real one would write into
    // the developer's own keychain as a side effect of `cargo test` -- and
    // would then behave differently on a CI runner that has no store at all,
    // which is exactly how this arrived.
    ToolSet::new(dial(&local.addr), Box::new(StaticFleet::new(fleet)))
        .with_key_store(Arc::new(zest_mesh::keystore::MemoryKeyStore::new()))
}

fn ok(v: Result<Value, zest_mcp::ToolError>) -> Value {
    v.expect("the call succeeds")
}

#[test]
fn the_fleet_lists_every_machine_and_marks_exactly_one_local() {
    let here = machine("studio", false);
    let there = machine("forge", false);
    let mut t = tools(&here, vec![row(&here, true), row(&there, false)]);

    let listed = ok(t.call("hosts", &json!({})));
    let hosts = listed["hosts"].as_array().expect("an array");
    assert_eq!(hosts.len(), 2, "both machines are listed");

    let local: Vec<&Value> = hosts.iter().filter(|h| h["local"] == json!(true)).collect();
    assert_eq!(local.len(), 1, "exactly one machine is this one");
    assert_eq!(local[0]["label"], json!("studio"));
    assert_eq!(local[0]["via"], json!("loopback"));
    // The offer comes from the connection the server already holds, not from
    // the fleet source -- which has no reason to know it.
    assert_eq!(local[0]["default_shell"], json!("studio-shell"));

    let remote = hosts.iter().find(|h| h["label"] == json!("forge")).expect("the second machine");
    assert_eq!(remote["via"], json!("lan"), "an advertised address is dialled directly");
    assert_eq!(remote["reachable"], json!(true));
    assert_eq!(
        remote["connected"],
        json!(false),
        "listing a machine must not dial it -- a fleet of twenty would be twenty handshakes"
    );
}

#[test]
fn a_session_on_a_second_machine_is_read_over_its_own_connection() {
    let here = machine("studio", false);
    let there = machine("forge", false);
    let mut t = tools(&here, vec![row(&here, true), row(&there, false)]);
    ok(t.call("hosts", &json!({})));

    // Created *on* the far machine, and its id names that machine.
    let made = ok(t.call("create_session", &json!({ "host": "forge", "command": quiet_cmd() })));
    let id = made["session"].as_str().expect("an id").to_string();
    assert!(
        id.starts_with(&there.host.short()),
        "a session id carries the machine it lives on: {id}"
    );

    // And it is listed by the machine that owns it, not by this one.
    let theirs = ok(t.call("sessions", &json!({ "host": "forge" })));
    assert!(
        theirs["sessions"].as_array().expect("array").iter().any(|s| s["id"] == json!(id)),
        "the far machine lists the session it was asked to start"
    );
    let mine = ok(t.call("sessions", &json!({ "host": "studio" })));
    assert!(
        !mine["sessions"].as_array().expect("array").iter().any(|s| s["id"] == json!(id)),
        "and this machine does not -- two daemons, two registries"
    );

    // Reading it goes to the same place.
    let screen = ok(t.call("screen", &json!({ "session": id })));
    assert!(screen.get("text").is_some(), "the screen is read over the far connection");

    ok(t.call("close_session", &json!({ "session": id })));
}

#[test]
fn a_listing_with_no_host_covers_what_is_connected_and_dials_nothing() {
    let here = machine("studio", false);
    let there = machine("forge", false);
    let mut t = tools(&here, vec![row(&here, true), row(&there, false)]);
    ok(t.call("hosts", &json!({})));

    // Before anything reaches the far machine, a bare listing is this one's.
    let first = ok(t.call("sessions", &json!({})));
    assert_eq!(
        first["unreadable"].as_array().expect("array").len(),
        0,
        "a machine nothing has connected to is not a machine that failed"
    );

    let made = ok(t.call("create_session", &json!({ "host": "forge", "command": quiet_cmd() })));
    let id = made["session"].as_str().expect("an id").to_string();

    // Naming it connected it, so it now rides along -- growing with the work
    // rather than through a fan-out this call never performs.
    let after = ok(t.call("sessions", &json!({})));
    assert!(
        after["sessions"].as_array().expect("array").iter().any(|s| s["id"] == json!(id)),
        "a machine the agent has worked on is in the bare listing afterwards"
    );

    ok(t.call("close_session", &json!({ "session": id })));
}

#[test]
fn run_isolated_on_a_second_machine_carries_that_machines_process_exit_status() {
    let here = machine("studio", false);
    let there = machine("forge", false);
    let mut t = tools(&here, vec![row(&here, true), row(&there, false)]);
    ok(t.call("hosts", &json!({})));

    // A status only the far daemon can know, read from the child it spawned.
    let cmd = if cfg!(windows) { "cmd.exe /c exit 7" } else { "/bin/sh -c 'exit 7'" };
    let v = ok(t.call("run_isolated", &json!({ "host": "forge", "command": cmd })));
    assert_eq!(v["exit_code"], json!(7), "the far machine's own word: {v}");
    assert_eq!(
        v["exit_code_source"],
        json!("process_exit"),
        "unforgeable, and it crossed a connection to get here"
    );
    assert!(
        v["session"].as_str().expect("an id").starts_with(&there.host.short()),
        "and it ran where it was asked to"
    );
}

#[test]
fn a_session_id_minted_on_one_machine_is_refused_under_the_others_prefix() {
    // The confused deputy that matters in a fleet: terminal output is
    // attacker-controlled, so an id read out of a build log must not be able to
    // move a call onto a different machine. With one host this was untestable
    // -- there was no other machine for a forged prefix to name.
    let here = machine("studio", false);
    let there = machine("forge", false);
    let mut t = tools(&here, vec![row(&here, true), row(&there, false)]);
    ok(t.call("hosts", &json!({})));

    let made = ok(t.call("create_session", &json!({ "host": "forge", "command": quiet_cmd() })));
    let id = made["session"].as_str().expect("an id").to_string();
    let number = id.rsplit_once(':').expect("host:session").1.to_string();

    // The same session number under this machine's prefix names a session that
    // does not exist there -- and must not silently resolve to the far one.
    let forged = format!("{}:{number}", here.host.short());
    assert_ne!(forged, id, "the two machines really do have different ids");
    let answered = t.call("screen", &json!({ "session": forged }));
    match answered {
        Err(_) => {}
        Ok(v) => assert!(
            v.get("text").is_none_or(|_| false),
            "a session number that exists elsewhere must not be served from here: {v}"
        ),
    }

    ok(t.call("close_session", &json!({ "session": id })));
}

#[test]
fn a_machine_with_no_route_is_listed_with_the_reason_rather_than_hidden() {
    let here = machine("studio", false);
    // Known to the account, and nothing can reach it: no address, and this
    // server is signed out so no ticket can be minted.
    let mut asleep = zest_fleet::fixture::host(9, "attic");
    asleep.presence = zest_mesh::discovery::Presence::Unseen;
    asleep.address = None;
    asleep.enrolled = true;
    let mut t = tools(&here, vec![row(&here, true), asleep]);

    let listed = ok(t.call("hosts", &json!({})));
    let row = listed["hosts"]
        .as_array()
        .expect("array")
        .iter()
        .find(|h| h["label"] == json!("attic"))
        .expect("it is listed, not dropped");
    assert_eq!(row["reachable"], json!(false));
    assert_eq!(row["via"], json!(null));
    let why = row["unreachable_because"].as_str().expect("a reason");
    assert!(
        why.contains("signed out"),
        "the reason names the act that would change it, not just the state: {why}"
    );

    // And a call that names it refuses with the same words, so a listing and a
    // refusal cannot disagree about why.
    let err = t
        .call("create_session", &json!({ "host": "attic" }))
        .expect_err("nothing can reach it");
    assert!(err.to_string().contains("signed out"), "{err}");
}

#[test]
fn a_first_dial_to_an_untrusted_machine_answers_with_the_code_and_leaves_the_prompt_standing() {
    // The load-bearing one. `PendingHandle::Drop` cancels the request, so the
    // obvious design -- refuse at once by hanging up, let the person approve,
    // retry -- deletes the very prompt it is asking them to answer. The dial
    // therefore stays parked while the call returns the code.
    let here = machine("studio", false);
    let there = machine("forge", true); // Auth::Proof: the trust store decides
    let mut t = tools(&here, vec![row(&here, true), row(&there, false)]);
    ok(t.call("hosts", &json!({})));

    let started = Instant::now();
    let err = t
        .call("sessions", &json!({ "host": "forge" }))
        .expect_err("this agent has never been approved there");
    let said = err.to_string();
    assert!(
        started.elapsed() < zest_mesh::pairing::APPROVAL_TIMEOUT,
        "the call answers rather than waiting out the approval window"
    );
    assert!(said.contains("approve"), "it says what is happening: {said}");

    // The prompt is still standing -- which is the assertion the whole design
    // exists for. A hung-up dial would have cancelled it.
    let pending = wait_for(|| {
        let p = there.pairings.pending();
        (!p.is_empty()).then_some(p)
    })
    .expect("the far machine is still asking somebody");
    assert_eq!(pending.len(), 1, "one dial, one prompt");
    let code = pending[0].code.clone();
    assert!(said.contains(&code), "and the agent was told the digits to compare: {said}");

    // Somebody approves. The next call finds the connection waiting.
    there.pairings.resolve(pending[0].client, Decision::Approve);
    let listed = wait_for(|| t.call("sessions", &json!({ "host": "forge" })).ok())
        .expect("the approved dial lands");
    assert!(listed.get("sessions").is_some(), "and it is a real listing: {listed}");
}

#[test]
fn a_second_call_while_one_dial_is_pending_does_not_queue_a_second_prompt() {
    // The host resolves a pairing by `ClientId`, so a second dial from this
    // same key would raise a second dialog that the first approval answers
    // anyway. Two dialogs for one decision is how people learn to click through
    // them.
    let here = machine("studio", false);
    let there = machine("forge", true);
    let mut t = tools(&here, vec![row(&here, true), row(&there, false)]);
    ok(t.call("hosts", &json!({})));

    let _ = t.call("sessions", &json!({ "host": "forge" })).expect_err("pending");
    wait_for(|| (!there.pairings.pending().is_empty()).then_some(())).expect("asked once");
    let _ = t.call("sessions", &json!({ "host": "forge" })).expect_err("still pending");
    let _ = t.call("create_session", &json!({ "host": "forge" })).expect_err("still pending");

    assert_eq!(
        there.pairings.pending().len(),
        1,
        "three calls, one decision to make"
    );
}

#[test]
fn a_machine_that_answers_slowly_is_not_reported_as_a_pairing_prompt() {
    // A dial can outlast its budget for two quite different reasons, and only
    // one of them has digits to compare. A silent peer is *slow*; saying
    // "approve this agent, the code is " with nothing after it reads as a
    // pairing flow gone wrong and sends a person looking for a prompt that was
    // never raised.
    let here = machine("studio", false);

    // Accepts the connection and then says nothing at all -- so the handshake
    // starts, never completes, and no pairing is ever queued.
    let mute = TcpListener::bind("127.0.0.1:0").expect("bind");
    let mute_addr = mute.local_addr().expect("addr").to_string();
    std::thread::spawn(move || {
        let mut held = Vec::new();
        for stream in mute.incoming() {
            match stream {
                Ok(s) => held.push(s),
                Err(_) => break,
            }
        }
    });

    let mut silent = zest_fleet::fixture::host(7, "mute");
    silent.address = Some(mute_addr);
    let mut t = tools(&here, vec![row(&here, true), silent]);
    ok(t.call("hosts", &json!({})));

    for attempt in 0..2 {
        let err = t
            .call("sessions", &json!({ "host": "mute" }))
            .expect_err("a peer that never speaks never connects");
        let said = err.to_string();
        assert!(
            !said.contains("code to compare"),
            "attempt {attempt}: nobody was asked to approve anything: {said}"
        );
        assert!(
            said.contains("not finished answering") || said.contains("has not answered"),
            "attempt {attempt}: it says it is still waiting, and on what: {said}"
        );
    }
}

/// A credential store that refuses everything, as a headless Linux box with no
/// Secret Service does.
struct NoStore;

impl zest_mesh::keystore::KeyStore for NoStore {
    fn load(
        &self,
        _name: &str,
    ) -> Result<Option<zest_mesh::keystore::Zeroizing<[u8; zest_mesh::keystore::SECRET_LEN]>>, zest_mesh::MeshError>
    {
        Err(zest_mesh::MeshError::Identity("no default store has been set".into()))
    }

    fn store(
        &self,
        _name: &str,
        _secret: &[u8; zest_mesh::keystore::SECRET_LEN],
    ) -> Result<(), zest_mesh::MeshError> {
        Err(zest_mesh::MeshError::Identity("no default store has been set".into()))
    }

    fn delete(&self, _name: &str) -> Result<(), zest_mesh::MeshError> {
        Err(zest_mesh::MeshError::Identity("no default store has been set".into()))
    }

    fn describe(&self) -> String {
        "no store".into()
    }
}

#[test]
fn a_machine_with_no_credential_store_still_reaches_the_fleet_and_says_what_it_costs() {
    // Refusing here would make the fleet unreachable from exactly the machines
    // most likely to be driven by an agent -- a headless box, a container, a
    // locked keychain. What is actually lost is durability, not reach: one key
    // per process still pairs and still holds for the life of this server.
    let here = machine("studio", false);
    let there = machine("forge", false);
    let mut t = ToolSet::new(dial(&here.addr), Box::new(StaticFleet::new(vec![
        row(&here, true),
        row(&there, false),
    ])))
    .with_key_store(Arc::new(NoStore));

    let listed = ok(t.call("hosts", &json!({})));
    // Nothing has needed a key yet, so nothing has been said yet.
    assert!(
        listed["notes"].as_array().expect("array").is_empty(),
        "the store is only touched by a remote dial"
    );

    let made = ok(t.call("create_session", &json!({ "host": "forge", "command": quiet_cmd() })));
    let id = made["session"].as_str().expect("an id").to_string();
    assert!(id.starts_with(&there.host.short()), "it reached the second machine anyway");

    let after = ok(t.call("hosts", &json!({})));
    let notes = after["notes"].as_array().expect("array");
    assert!(
        notes.iter().any(|n| n.as_str().is_some_and(|n| n.contains("ask again next launch"))),
        "and it says what that cost, where the person approving it will read it: {notes:?}"
    );

    ok(t.call("close_session", &json!({ "session": id })));
}

/// Poll `f` until it answers, up to a few seconds.
///
/// A poll rather than a sleep because what is being waited for is another
/// thread's handshake, and the interesting failure is that it never happens
/// rather than that it is slow.
fn wait_for<T>(mut f: impl FnMut() -> Option<T>) -> Option<T> {
    let give_up = Instant::now() + Duration::from_secs(10);
    loop {
        if let Some(v) = f() {
            return Some(v);
        }
        if Instant::now() >= give_up {
            return None;
        }
        std::thread::sleep(Duration::from_millis(25));
    }
}
