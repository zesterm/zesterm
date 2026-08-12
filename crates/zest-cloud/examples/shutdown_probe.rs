//! Does `shutdown` unpark a `read` that is already parked — on *this* machine?
//!
//! ```sh
//! cargo run -p zest-cloud --example shutdown_probe
//! ```
//!
//! The question [`READ_POLL`](../src/tls.rs) exists to answer, asked at the
//! layer it actually lives at: a bare `TcpStream`, no TLS, no daemon, no crates
//! beyond `std`. In the tradition of `examples/attach.rs` and `mesh_probe` —
//! answer "which layer is wrong" without the layers above it.
//!
//! # Why an example rather than a test
//!
//! It measures the *platform*, not this workspace, and its interesting outcomes
//! are wall-clock ones — "still parked after five seconds" takes five seconds
//! and asserts nothing about our code. The regression tests that hold our own
//! behaviour live in `zest_daemon::lan`; this exists so the next person can
//! re-run the measurement those tests were written from instead of citing it.
//!
//! # The thing it is really here to prevent
//!
//! **What the peer does when you cut decides what you observe, and the two
//! answers are indistinguishable in a log.** A peer that closes its half when
//! your FIN arrives sends one back, and that *remote* close ends your parked
//! read on every platform, in microseconds — whether or not the `shutdown` did
//! anything locally. Every convenient stand-in for a peer closes on EOF by
//! default, so a rig built the obvious way measures the wrong thing and reports
//! success. #126 measured ten clean cycles that way and concluded the read poll
//! was redundant; removing it then hung a control link for ever against a peer
//! that stayed up. Hence the second half of the table: the rows are the same
//! cut, and only the top one is about `shutdown`.
use std::io::Read;
use std::net::{Shutdown, TcpListener, TcpStream};
use std::sync::mpsc;
use std::time::{Duration, Instant};

/// How long a reader is given to come back before it is called parked.
///
/// Long enough that several polls would have elapsed, so "still parked" cannot
/// be a slow wake-up.
const PATIENCE: Duration = Duration::from_secs(5);

/// The poll `zest_cloud::tls::READ_POLL` and `zest_daemon::lan::READ_POLL` arm,
/// mirrored rather than imported: this file is deliberately readable as a
/// self-contained question about the platform.
const POLL: Duration = Duration::from_secs(1);

/// When the cut is issued, relative to the reader parking.
///
/// Deliberately *not* a multiple of [`POLL`]: if the cut landed on a poll
/// boundary, "the shutdown woke it" and "the poll elapsed" would predict the
/// same instant and the probe would answer nothing. With this offset a wake at
/// ~0 ms is the cut and a wake at ~700 ms is the poll.
const CUT_AFTER: Duration = Duration::from_millis(300);

fn main() {
    println!("a parked reader, cut by `shutdown(Both)` on a second handle\n");
    println!("cut issued {CUT_AFTER:?} after the read parks, so an elapsed {POLL:?} poll");
    println!(
        "shows up ~{}ms after the cut and the shutdown itself at ~0ms\n",
        POLL.saturating_sub(CUT_AFTER).as_millis()
    );

    println!("-- peer alive and silent: what a handshake watchdog exists to cut --");
    case("accepted socket, no timeout", Park::Accepted, None, Peer::Silent);
    case("accepted socket, poll armed", Park::Accepted, Some(POLL), Peer::Silent);
    case("dialled socket,  no timeout", Park::Dialled, None, Peer::Silent);
    case("dialled socket,  poll armed", Park::Dialled, Some(POLL), Peer::Silent);

    println!("\n-- peer closes when the FIN arrives: what a naive test rig measures --");
    case("accepted socket, no timeout", Park::Accepted, None, Peer::ClosesOnEof);
    case("dialled socket,  no timeout", Park::Dialled, None, Peer::ClosesOnEof);
}

/// Which end of the connection the parked reader sits on.
///
/// Crossed because it was a live hypothesis for why two measurements on one
/// machine disagreed — a watchdog cuts an *accepted* socket on the LAN and a
/// *dialled* one at the relay. It is not the answer, and the probe says so
/// rather than leaving it to be guessed at again.
enum Park {
    Accepted,
    Dialled,
}

enum Peer {
    /// Held open and never read from: alive, and saying nothing.
    Silent,
    /// Reads until our FIN arrives, then drops — sending a FIN back.
    ClosesOnEof,
}

fn case(name: &str, park: Park, timeout: Option<Duration>, peer: Peer) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind loopback");
    let addr = listener.local_addr().expect("the bound address");
    let dialled = TcpStream::connect(addr).expect("connect");
    let (accepted, _) = listener.accept().expect("accept");

    let (reader, far_end) = match park {
        Park::Accepted => (accepted, dialled),
        Park::Dialled => (dialled, accepted),
    };

    // Kept alive for the whole case in the `Silent` arm: if the far end were
    // dropped, its close would end the read and every row would look the same.
    let _far_end = match peer {
        Peer::Silent => Some(far_end),
        Peer::ClosesOnEof => {
            let mut far_end = far_end;
            std::thread::spawn(move || {
                let mut sink = [0u8; 64];
                // Returns as soon as our FIN lands; the drop that follows closes
                // this half and sends one back.
                let _ = far_end.read(&mut sink);
            });
            None
        }
    };

    if let Some(timeout) = timeout {
        // Armed *before* the reader can park, which is the ordering the whole
        // design rests on — see `READ_POLL`.
        reader.set_read_timeout(Some(timeout)).expect("arm the poll");
    }
    // A second handle, as a watchdog holds: the reader's own is inside the
    // syscall at exactly the moment the cut is needed.
    let scissors = reader.try_clone().expect("a second handle");

    let (woke, parked) = mpsc::channel();
    std::thread::spawn(move || {
        let mut reader = reader;
        let mut buf = [0u8; 64];
        let started = Instant::now();
        loop {
            let outcome = match reader.read(&mut buf) {
                Ok(0) => "end of stream".to_string(),
                Ok(n) => format!("{n} bytes — the peer said something?!"),
                // An elapsed poll. The real readers check a `severed` flag here
                // and this one has none, so it re-parks: without that the
                // timeout would end the read by itself and the armed rows would
                // measure the timeout rather than the cut.
                Err(e)
                    if matches!(
                        e.kind(),
                        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                    ) =>
                {
                    if started.elapsed() < PATIENCE {
                        continue;
                    }
                    "still parked, polling".to_string()
                }
                Err(e) => format!("{} (os error {:?})", e.kind(), e.raw_os_error()),
            };
            let _ = woke.send(outcome);
            return;
        }
    });

    std::thread::sleep(CUT_AFTER);
    let cut_at = Instant::now();
    let _ = scissors.shutdown(Shutdown::Both);

    match parked.recv_timeout(PATIENCE) {
        Ok(outcome) => println!(
            "{name:<30} woke {:>7.1} ms after the cut   {outcome}",
            cut_at.elapsed().as_secs_f64() * 1000.0
        ),
        Err(_) => println!("{name:<30} STILL PARKED after {PATIENCE:?} — the shutdown did nothing"),
    }
}
