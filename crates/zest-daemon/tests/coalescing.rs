//! The coalescing floor, measured rather than asserted about.
//!
//! `DaemonConfig::min_delta_interval` is what keeps a relayed session inside
//! ADR-009's arithmetic: unthrottled a busy shell is ~1000 messages/second,
//! which is 125× the bill and — the part that actually matters — a Durable
//! Object that never idles long enough to hibernate. A config field is easy to
//! add and easy to render inert, so the deliverable is this test.
//!
//! It drives `serve()` in-process over a loopback TCP pair rather than through
//! a listener: the floor lives in the writer loop that every transport shares,
//! and a socket with a read timeout is what keeps a failure red instead of a CI
//! job that hangs for an hour (see `tests/lan.rs`).
//!
//! Two properties, and the second is why the first means anything. A rate
//! bound alone passes just as happily on an implementation that throws updates
//! away, which is the one outcome worse than an expensive relay.
//!
//! Measured while writing it, on an M-series Mac in a debug build, so the
//! numbers are not folklore: across several runs this flood delivered
//! **708–1,294 updates with the floor at zero, and 2–5 with it at 30ms** —
//! and the handful reconstruct the host's final screen exactly. Rates are
//! deliberately not quoted: a client reads 64 KiB at a time, so an unthrottled
//! burst is *received* in a fraction of the time it took to produce, and any
//! per-second figure taken here would flatter the wrong side.

use std::io::Read;
use std::net::{TcpListener, TcpStream};
use std::sync::Arc;
use std::time::{Duration, Instant};

use zest_daemon::client::DaemonClient;
use zest_daemon::{serve, Auth, Authenticator, DaemonConfig, Registry};
use zest_mesh::identity::{ClientIdentity, HostIdentity};
use zest_mesh::pairing::PairingQueue;
use zest_mesh::trust::MemoryTrustStore;
use zest_proto::{Applied, Applier, HostMessage};

/// What the relay transport will ask for. → ADR-009.
const FLOOR: Duration = Duration::from_millis(30);

const COLS: u16 = 80;
const ROWS: u16 = 24;

/// How many lines the flood prints. The last one is the marker the test waits
/// for, so this is also the string that proves nothing was lost.
const LINES: u32 = 6000;

/// A child that prints as fast as the pty will take it, then exits.
///
/// A shell loop rather than `yes`, for two reasons: it terminates, and every
/// line is distinguishable, so "the client ended up at the right place" is a
/// question about a specific string rather than about a screenful of `y`.
fn flood_cmd() -> String {
    if cfg!(windows) {
        format!("cmd.exe /c \"for /L %i in (1,1,{LINES}) do @echo line %i\"")
    } else {
        format!(
            "/bin/sh -c 'i=1; while [ $i -le {LINES} ]; do echo \"line $i\"; \
             i=$((i+1)); done'"
        )
    }
}

fn last_line() -> String {
    format!("line {LINES}")
}

/// A daemon serving one connection, over loopback TCP.
/// How long to keep draining after the flood's last line shows up, before
/// calling the client caught up. Short: it is only ever spent once, at the end.
const SETTLE: Duration = Duration::from_millis(300);

fn serve_one(floor: Duration) -> TcpStream {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let addr = listener.local_addr().expect("addr");

    let identity = Arc::new(HostIdentity::generate().expect("host key"));
    let config = DaemonConfig {
        host: identity.host_id(),
        label: "floor-test".into(),
        local_socket: String::new(),
        listen_lan: false,
        lan_bind: "127.0.0.1".into(),
        lan_port: 0,
        listen_ws: false,
        ws_bind: "127.0.0.1".into(),
        ws_port: 0,
        relay: None,
        // A shell hook on a command that is not a shell buys nothing, and
        // writing the shim is a filesystem dependency this test does not need.
        shell_integration: false,
        min_delta_interval: floor,
        enroll: None,
        offer: None,
    };
    let auth = Auth::Transport(Arc::new(Authenticator::new(
        identity,
        Arc::new(MemoryTrustStore::new()),
        PairingQueue::new(),
        "floor-test",
    )));

    std::thread::spawn(move || {
        let (stream, _) = listener.accept().expect("accept");
        let write = stream.try_clone().expect("clone");
        let _ = serve(stream, write, config, Arc::new(Registry::new()), auth, "in-process");
    });

    let client = TcpStream::connect(addr).expect("connect");
    // Any read that waits this long is a hang, and a hung test reports nothing
    // at all. Generous, because it must never fire on a loaded CI runner.
    client.set_read_timeout(Some(Duration::from_secs(20))).expect("read timeout");
    client
}

fn connect(stream: &TcpStream) -> DaemonClient {
    let identity = Arc::new(ClientIdentity::generate().expect("client key"));
    DaemonClient::connect(
        Box::new(stream.try_clone().expect("clone")) as Box<dyn Read + Send>,
        Box::new(stream.try_clone().expect("clone")),
        &identity,
        "floor-test",
        None,
        false,
    )
    .expect("the loopback path welcomes any proved client")
}

/// The screen as text, one string per row, trailing blanks trimmed.
///
/// Text only: a delta that moves nothing but the cursor is still a delta, so
/// comparing cursors would make this test sensitive to whether the shell's
/// last write landed inside the final batch or after it — which is not what it
/// is about.
fn screen(term: &zest_core::Terminal) -> Vec<String> {
    (0..usize::from(ROWS))
        .map(|r| {
            let row: String = (0..usize::from(COLS))
                .map(|c| term.grid().cell(r, c).map_or(' ', |cell| cell.ch))
                .collect();
            row.trim_end().to_string()
        })
        .collect()
}

#[test]
fn a_floor_bounds_the_message_rate_and_still_lands_on_the_right_grid() {
    let stream = serve_one(FLOOR);
    let mut client = connect(&stream);

    let addr = client.create(&flood_cmd(), "", COLS, ROWS).expect("create");
    let (seq, keyframe) = client.attach(addr, COLS, ROWS).expect("attach");

    let mut term = zest_core::Terminal::new(usize::from(COLS), usize::from(ROWS), 2000);
    let mut applier = Applier::new();
    applier.apply_keyframe(&mut term, &keyframe, seq);

    // Counted from the first update rather than from the attach: the keyframe
    // above is a *reply*, and the interval starts when the first delta goes
    // out.
    let mut updates = 0_usize;
    let mut first = None;
    let mut last = Instant::now();
    let mut host_seq = seq;

    // The marker is the terminating condition, not `Exited`. `Exited` is
    // deliberately *not* throttled, so it can overtake a delta the floor is
    // still holding — and stopping there would compare a client that is
    // legitimately one batch behind. The last line of the flood cannot
    // overtake anything.
    // ...and then everything still behind it, which is not the same thing.
    //
    // The last line *appearing* does not mean the host has finished: under a
    // floor the newline that scrolled it into place can arrive as its own
    // batch. Stopping there compared a client one row short of the host, with
    // a trailing blank -- an assertion that reads as "coalescing lost
    // information" when nothing was lost and the client was simply one update
    // behind. It failed that way once, on a slow Windows runner.
    //
    // So on seeing the last line the read timeout drops to `SETTLE` and this
    // loop drains to it: "the client has everything the host will send"
    // becomes true rather than likely, for one short wait instead of the 20s
    // the real timeout would cost.
    let mut settling = false;
    loop {
        if !settling && screen(&term).iter().any(|row| row.contains(&last_line())) {
            settling = true;
            stream.set_read_timeout(Some(SETTLE)).expect("read timeout");
        }
        let msg = match client.next_message() {
            Ok(m) => m,
            // A read timeout arrives here as a transport error, as does a
            // daemon that hung up. Either way there is nothing more coming;
            // the assertions below say what was missing.
            Err(_) => break,
        };
        match msg {
            HostMessage::Update { base, seq, delta, .. } => {
                updates += 1;
                first.get_or_insert_with(Instant::now);
                last = Instant::now();
                host_seq = seq.0;
                assert_eq!(
                    applier.apply_delta(&mut term, &delta, base.0, seq.0),
                    Applied::Ok,
                    "a delta the daemon sent did not apply to the state it named as its \
                     base, so the floor is dropping updates rather than coalescing them"
                );
            }
            // The subscriber fell far enough behind that a delta chain would
            // cost more than the state it describes — normal under a floor,
            // and still one message.
            HostMessage::Keyframe { seq, cols, rows, rows_data, attrs, cursor, modes, blocks, blocks_from, title, history_clears, .. } => {
                updates += 1;
                first.get_or_insert_with(Instant::now);
                last = Instant::now();
                host_seq = seq.0;
                applier.apply_keyframe(
                    &mut term,
                    &zest_proto::Keyframe {
                        cols,
                        rows,
                        rows_data,
                        attrs,
                        cursor,
                        modes: zest_core::Modes::from_bits_truncate(modes),
                        blocks,
                        blocks_from,
                        title,
                        history_clears,
                    },
                    seq.0,
                );
            }
            _ => {}
        }
    }

    let coalesced = screen(&term);
    assert!(
        coalesced.iter().any(|row| row.contains(&last_line())),
        "the last line the child printed never reached the client, so the floor is \
         losing output rather than coalescing it. Saw {updates} updates: {coalesced:?}"
    );

    let Some(first) = first else { panic!("the flood produced no updates at all") };
    let window = last.duration_since(first);
    // One message per interval, plus the one that goes out immediately because
    // nothing had been sent yet, plus slack for the boundary at each end.
    #[allow(clippy::cast_precision_loss, reason = "a message count, in the thousands at most")]
    let allowed = (window.as_secs_f64() / FLOOR.as_secs_f64()).ceil() as usize + 3;
    assert!(
        updates <= allowed,
        "{updates} updates in {window:?} against a {FLOOR:?} floor, which allows {allowed}. \
         Unthrottled the same flood sends every batch the pty produces — an order of \
         magnitude more requests billed at the relay, and an object that never idles \
         long enough to hibernate"
    );
    // What keeps the bound above from being satisfied by a flood that never
    // happened — and it counts *change*, not elapsed time. A machine fast
    // enough to deliver the whole flood in two messages has coalesced harder,
    // not less, so a guard phrased as "the updates must have spanned N
    // intervals" fails on exactly the runs that prove the point best — as one
    // did, about once in six runs of this suite while it was being written.
    assert!(
        host_seq >= u64::from(LINES),
        "the host reached sequence {host_seq} for a {LINES}-line flood, so almost \
         nothing was printed and {updates} messages were never a saving"
    );

    // The other half, and the reason the bound above is worth having. A
    // reattach is a resync, so this keyframe is the host's own final state —
    // rebuilt from the terminal, not from anything this connection was sent.
    let (ref_seq, reference) = client.attach(addr, COLS, ROWS).expect("reattach");
    let mut authoritative =
        zest_core::Terminal::new(usize::from(COLS), usize::from(ROWS), 2000);
    Applier::new().apply_keyframe(&mut authoritative, &reference, ref_seq);
    assert_eq!(
        coalesced,
        screen(&authoritative),
        "the coalesced stream and the host disagree about the final screen. \
         {updates} messages carried {host_seq} sequence numbers' worth of change: \
         coalescing is allowed to lose frames and never information"
    );

    client.close(addr).expect("close");
}
