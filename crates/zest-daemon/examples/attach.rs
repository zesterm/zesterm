//! Drive a daemon session from the command line, with no GUI.
//!
//! The cheapest layer at which the whole loop can be watched: connect, create,
//! attach, and print what comes back. When a session renders wrongly in the app,
//! this answers "is it the daemon or the renderer" without involving a window,
//! a GPU or a font — the same job `headless` does for the local path.
//!
//! ```text
//! zest-daemon --socket \\.\pipe\zesterm-demo &
//! cargo run -p zest-daemon --example attach -- --socket \\.\pipe\zesterm-demo --cmd "pwsh -NoLogo -c ls"
//! ```
//!
//! `--addr <host:port>` does the same over TCP, against another machine's
//! daemon — the two-machine bring-up's step between "paired" and "a window":
//! it proves a remote session end to end with no GPU, font or renderer in the
//! picture, exactly as the local form does for the local path. The identity is
//! throwaway, so an unpaired host will prompt and print a code; compare it with
//! the one this prints, the same ritual as `pair`.
//!
//! `--ws <host:port>` does it over the WebSocket transport (the daemon needs
//! `--listen-ws`). This is the layer-isolating tool for the web client: when a
//! session misbehaves in a browser, this says whether the daemon's WebSocket
//! transport or the browser's stack is at fault, with no browser involved.
//!
//! Anything on stdin is forwarded to the session as keystrokes, so a shell can
//! be driven from here — `echo hello` then a failing command is how a shell
//! integration is checked with no GUI in the picture. Type `\r`, not `\n`: it is
//! a terminal on the other end.
//!
//! `--close` ends the session on the way out instead of merely detaching. It is
//! the only way to see the difference from outside the daemon: detaching leaves
//! the child running by design, so "did closing actually end it" is a question
//! about a process, answered with `ps`, not about anything on the wire.

use std::io::{Read, Write};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use zest_daemon::{connect, default_socket_path};
use zest_proto::{frame, ClientMessage, FrameReader, HostMessage, PROTOCOL_VERSION};

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let opt = |name: &str| -> Option<String> {
        args.iter().position(|a| a == name).and_then(|i| args.get(i + 1)).cloned()
    };

    let cmd = opt("--cmd").unwrap_or_default();
    let seconds: u64 = opt("--seconds").and_then(|s| s.parse().ok()).unwrap_or(5);
    // `Detach` and `CloseSession` differ in exactly one observable — whether the
    // child is still running afterwards — and nothing else here can show it.
    let close = args.iter().any(|a| a == "--close");
    // How many keystroke -> delta round trips to measure. ADR-007 claims
    // 50-100us on loopback and nobody had ever checked; the LAN number did not
    // exist at all. Milliseconds, which is all this example printed before, are
    // useless against a microsecond claim.
    let ping: usize = opt("--ping").and_then(|s| s.parse().ok()).unwrap_or(0);

    // Three transports, one loop: the protocol is transport-blind and this
    // example should prove that, not re-litigate it.
    if let Some(addr) = opt("--ws") {
        let stream = match std::net::TcpStream::connect(&addr) {
            Ok(s) => s,
            Err(e) => {
                eprintln!("[attach] could not reach {addr}: {e}");
                eprintln!("[attach] the daemon needs --listen-ws");
                std::process::exit(1);
            }
        };
        let (reader, writer) = match zest_daemon::ws::client::connect(stream) {
            Ok(halves) => halves,
            Err(e) => {
                eprintln!("[attach] the WebSocket upgrade failed: {e}");
                std::process::exit(1);
            }
        };
        eprintln!("[attach] connected to {addr} (websocket)");
        run(reader, Arc::new(Mutex::new(writer)), cmd, seconds, close, ping);
    } else if let Some(addr) = opt("--addr") {
        let stream = match std::net::TcpStream::connect(&addr) {
            Ok(s) => s,
            Err(e) => {
                eprintln!("[attach] could not reach {addr}: {e}");
                eprintln!("[attach] the far daemon needs --listen-lan, and a route");
                std::process::exit(1);
            }
        };
        let writer = stream.try_clone().expect("clone the stream for the writer");
        eprintln!("[attach] connected to {addr} (tcp)");
        run(stream, Arc::new(Mutex::new(writer)), cmd, seconds, close, ping);
    } else {
        let socket = opt("--socket").unwrap_or_else(default_socket_path);
        let stream = match connect(&socket) {
            Ok(s) => s,
            Err(e) => {
                eprintln!("[attach] no daemon at {socket}: {e}");
                eprintln!("[attach] start one with: zest-daemon --socket {socket}");
                std::process::exit(1);
            }
        };
        let writer = stream.try_clone().expect("clone the stream for the writer");
        eprintln!("[attach] connected to {socket}");
        run(stream, Arc::new(Mutex::new(writer)), cmd, seconds, close, ping);
    }
}

/// Read and write halves taken separately, because the WebSocket transport
/// cannot hand out one bidirectional value — and a shared writer is what lets
/// the deadline thread close the session on any transport.
fn run<R: Read + Send + 'static, W: Write + Send + 'static>(
    mut stream: R,
    writer: Arc<Mutex<W>>,
    cmd: String,
    seconds: u64,
    close: bool,
    ping: usize,
) {
    // Beside the writer rather than inside it, and always locked *after* it:
    // sealing advances a counter, so two threads that sealed in one order and
    // wrote in the other would produce frames the host cannot open. The
    // deadline thread writes through this same closure, which is why it is a
    // lock at all and not a plain local.
    let sealer: Arc<Mutex<Option<zest_mesh::secure::Sealer>>> = Arc::new(Mutex::new(None));
    // A free function rather than only a closure, so the stdin thread can send
    // through the same path instead of writing its own — the lock ordering above
    // is the sort of thing that must not exist twice.
    let send = |writer: &Arc<Mutex<W>>, msg: &ClientMessage| send_msg(writer, &sealer, msg);

    // A throwaway key. This is a diagnostic, not a device: it should not
    // accumulate a pairing on every host it is pointed at.
    let identity =
        Arc::new(zest_mesh::identity::ClientIdentity::generate().expect("client key"));
    let mut hs = zest_mesh::pairing::ClientHandshake::new(Arc::clone(&identity), "attach")
        .expect("client handshake");
    let mut opener: Option<zest_mesh::secure::Opener> = None;
    eprintln!("[attach] client {}", identity.client_id().short());

    send(
        &writer,
        &ClientMessage::Hello {
            version: PROTOCOL_VERSION,
            client: identity.client_id(),
            label: "attach".into(),
            nonce: zest_proto::Nonce32::from_bytes(*hs.nonce().as_bytes()),
            dh: zest_proto::Pub32::from_bytes(hs.dh().0),
            watch_sessions: false,
            watch_pairings: false,
            watch_hosts: false,
            watch_signals: false,
        },
    );
    // The session is created once the handshake completes, not before: a host
    // that refuses this client should not have started a shell for it.
    let mut create = Some(ClientMessage::CreateSession {
        command: cmd,
        cwd: String::new(),
        cols: 100,
        rows: 30,
    });

    // The deadline is enforced by a separate thread, not by checking between
    // reads: `read` blocks until the daemon sends something, so a quiet session
    // would hold the loop past any deadline the loop itself could notice. That
    // is correct behaviour for a client and wrong for a diagnostic with a time
    // limit.
    // Shared so the deadline thread can end the session the loop attached to.
    let opened: Arc<Mutex<Option<zest_proto::SessionAddr>>> = Arc::new(Mutex::new(None));

    {
        let opened = Arc::clone(&opened);
        let writer = Arc::clone(&writer);
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_secs(seconds));
            if close {
                if let Some(session) = *opened.lock().expect("session slot") {
                    eprintln!("[attach] closing {session}");
                    let bytes = frame::encode(&ClientMessage::CloseSession { session })
                        .expect("encode");
                    let mut w = writer.lock().expect("writer");
                    let _ = Write::write_all(&mut *w, &bytes);
                    let _ = Write::flush(&mut *w);
                    drop(w);
                    // The daemon hangs the child up synchronously, so give the
                    // write time to be served before this process disappears.
                    std::thread::sleep(Duration::from_millis(500));
                }
            }
            eprintln!("[attach] done");
            std::process::exit(0);
        });
    }

    // Forward our stdin to the session, so a shell can actually be driven from
    // here. Without it this example can watch a session but not make one do
    // anything, which is the difference between seeing that a prompt appeared
    // and seeing that a command ran, failed, and was indexed as a block —
    // exactly what a shell-integration change has to be checked against.
    //
    // Waits for the address rather than buffering: input is addressed to a
    // session, and there is no session to address until the keyframe names one.
    {
        let opened = Arc::clone(&opened);
        let writer = Arc::clone(&writer);
        let sealer = Arc::clone(&sealer);
        std::thread::spawn(move || {
            let mut stdin = std::io::stdin().lock();
            let mut buf = [0u8; 1024];
            while let Ok(n) = stdin.read(&mut buf) {
                if n == 0 {
                    break;
                }
                let Some(session) = *opened.lock().expect("session slot") else { continue };
                send_msg(&writer, &sealer, &ClientMessage::Input { session, bytes: buf[..n].to_vec() });
            }
        });
    }

    // Ping state. `pinged` is set once the keyframe names the session, because
    // input has to be addressed to it and the address is not known before then.
    let mut samples: Vec<Duration> = Vec::new();
    let mut sent_at: Option<Instant> = None;
    let mut pinged: Option<zest_proto::SessionAddr> = None;

    let mut reader = FrameReader::new();
    let mut buf = vec![0u8; 64 * 1024];
    let mut attached = false;
    let start = Instant::now();

    loop {
        let n = match Read::read(&mut stream, &mut buf) {
            Ok(0) => break,
            Ok(n) => n,
            Err(e) => {
                eprintln!("[attach] read failed: {e}");
                break;
            }
        };
        reader.feed(&buf[..n]);

        while let Some(body) = reader.next_frame().expect("framing") {
            let body = match opener.as_mut() {
                Some(o) => match o.open(&body) {
                    Ok(plain) => plain,
                    Err(e) => {
                        eprintln!("[attach] a sealed frame did not open: {e}");
                        std::process::exit(1);
                    }
                },
                None => body,
            };
            match frame::decode::<HostMessage>(&body).expect("decode") {
                HostMessage::Welcome { host, label, .. } => {
                    eprintln!("[attach] authenticated to {} ({label})", host.short());
                    if let Some(msg) = create.take() {
                        send(&writer, &msg);
                    }
                }
                HostMessage::Sessions { sessions, .. } => {
                    for s in &sessions {
                        eprintln!("[attach] session {} {}x{}", s.addr, s.cols, s.rows);
                    }
                    // Attach to the newest, which is the one just created.
                    if !attached {
                        if let Some(s) = sessions.last() {
                            send(
                                &writer,
                                &ClientMessage::Attach { session: s.addr, cols: 100, rows: 30, observe: false },
                            );
                            *opened.lock().expect("session slot") = Some(s.addr);
                            attached = true;
                        }
                    }
                }
                HostMessage::Keyframe { rows_data, attrs, modes, seq, blocks, session, .. } => {
                    eprintln!(
                        "[attach] keyframe @seq {}: {} rows, {} attrs, {} blocks, modes {:?}",
                        seq.0,
                        rows_data.len(),
                        attrs.len(),
                        blocks.len(),
                        zest_core::Modes::from_bits_truncate(modes)
                    );
                    if ping > 0 {
                        pinged = Some(session);
                        send(&writer, &ClientMessage::Input { session, bytes: vec![b'.'] });
                        sent_at = Some(Instant::now());
                        continue;
                    }
                    print_rows(&rows_data);
                    print_blocks(&blocks);
                }
                HostMessage::Update { delta, base, seq, .. } => {
                    // One round trip closed. The pty's line discipline echoes
                    // input regardless of what is running, so any long-lived
                    // command works and nothing on the far side has to
                    // cooperate -- which is what makes this a measurement of
                    // the transport rather than of a shell.
                    if ping > 0 {
                        if let Some(t) = sent_at.take() {
                            samples.push(t.elapsed());
                        }
                        if samples.len() >= ping {
                            report(&samples);
                            std::process::exit(0);
                        }
                        if let Some(session) = pinged {
                            send(&writer, &ClientMessage::Input { session, bytes: vec![b'.'] });
                            sent_at = Some(Instant::now());
                        }
                        continue;
                    }
                    eprintln!(
                        "[attach] +{}ms delta {}->{}: {} ops, {} new attrs, {} blocks",
                        start.elapsed().as_millis(),
                        base.0,
                        seq.0,
                        delta.ops.len(),
                        delta.attrs.len(),
                        delta.blocks.len()
                    );
                    // Show what the rows actually became, so this proves content
                    // arrived rather than merely that a message did.
                    for op in &delta.ops {
                        match op {
                            zest_proto::DeltaOp::Row { row, payload } => {
                                let text: String =
                                    payload.runs.iter().map(|r| r.text.as_str()).collect();
                                if !text.trim().is_empty() {
                                    println!("  row {row}: {}", text.trim_end());
                                }
                            }
                            // Printed because this is the layer that answers
                            // "why does the arrow key do nothing on the phone".
                            zest_proto::DeltaOp::Modes { bits } => {
                                println!(
                                    "  modes: {:?}",
                                    zest_core::Modes::from_bits_truncate(*bits)
                                );
                            }
                            _ => {}
                        }
                    }
                    if !delta.blocks.is_empty() {
                        print_blocks(&delta.blocks);
                    }
                }
                HostMessage::Exited { session, code } => {
                    eprintln!("[attach] session {session} exited ({code:?})");
                }
                // Never reached: this example's `Hello` sets `watch_signals:
                // false`, so the daemon sends none. Printed rather than
                // ignored anyway, because a probe whose job is to say which
                // layer is wrong must not be the one thing that swallows a
                // message quietly.
                HostMessage::Attention { session, cause } => {
                    eprintln!("[attach] session {session} asked to be noticed ({cause:?})");
                }
                HostMessage::Progress { session, progress } => {
                    eprintln!("[attach] session {session} progress {progress:?}");
                }
                HostMessage::Error { message, .. } => eprintln!("[attach] error: {message}"),
                HostMessage::Scrollback { .. } => {}

                HostMessage::Challenge { host, label, nonce, dh, signature, version } => {
                    eprintln!("[attach] host {} ({label}) challenged", host.short());
                    let host_sig = zest_mesh::identity::Signature::from_slice(&signature.0)
                        .expect("signature");
                    // No expected host: this example is pointed at a socket by
                    // hand, so there is no advertisement to have been misled by.
                    let (sig, _, channel) = match hs.on_challenge(
                        None,
                        &zest_mesh::pairing::Challenge {
                            version,
                            host,
                            label,
                            nonce: zest_mesh::identity::Nonce::from_bytes(nonce.0),
                            dh: zest_mesh::secure::DhPublic(dh.0),
                            signature: host_sig,
                        },
                    ) {
                        Ok(v) => v,
                        Err(e) => {
                            eprintln!("[attach] the host did not prove itself: {e}");
                            std::process::exit(1);
                        }
                    };
                    let (s, o) = channel.split();
                    *sealer.lock().expect("sealer") = Some(s);
                    opener = Some(o);
                    send(
                        &writer,
                        &ClientMessage::Auth {
                            signature: zest_proto::Sig64::from_bytes(sig.to_bytes()),
                        },
                    );
                }
                HostMessage::AuthPending { code, expires_in_secs } => {
                    // The code to compare with the one on the host's screen.
                    // Not an error: the key is good, and nobody has said yes.
                    eprintln!(
                        "[attach] waiting for approval -- compare code {code} ({expires_in_secs}s)"
                    );
                }
                HostMessage::AuthFailed { reason, message } => {
                    eprintln!("[attach] refused: {reason:?} -- {message}");
                    std::process::exit(1);
                }
                HostMessage::PairingRequested { label, code, remote, .. } => {
                    eprintln!("[attach] {label} at {remote} is asking to pair, code {code}");
                }
                // This probe never sends Enroll; logged rather than silently
                // eaten so a misdirected reply is at least visible.
                HostMessage::EnrollResult { ok, message, .. } => {
                    eprintln!("[attach] unexpected EnrollResult (ok={ok}): {message}");
                }
            }
        }
    }
}

/// Seal and write one message.
///
/// The sealer is locked *after* the writer, always: sealing advances a counter,
/// so two threads that sealed in one order and wrote in the other would produce
/// frames the host cannot open.
fn send_msg<W: Write>(
    writer: &Arc<Mutex<W>>,
    sealer: &Arc<Mutex<Option<zest_mesh::secure::Sealer>>>,
    msg: &ClientMessage,
) {
    let body = frame::encode_body(msg).expect("encode");
    let mut w = writer.lock().expect("writer");
    let body = match sealer.lock().expect("sealer").as_mut() {
        Some(s) => s.seal(&body).expect("seal"),
        None => body,
    };
    let bytes = frame::frame_bytes(&body).expect("frame");
    Write::write_all(&mut *w, &bytes).expect("write");
    Write::flush(&mut *w).expect("flush");
}

/// Print the command blocks, which is the phone client's whole view.
///
/// Here for the same reason the grid dump is: this example is the layer that
/// answers "is the daemon wrong or is the renderer wrong", and blocks are now
/// something that can be wrong on the far side of the wire. Seeing the command
/// text and exit status here proves the host parsed OSC 133 *and* that it
/// survived encoding — no window, no GPU, no font involved.
fn print_blocks(blocks: &[zest_proto::BlockPayload]) {
    if blocks.is_empty() {
        // Not a failure. A shell with no integration installed emits no
        // markers, and saying so beats printing nothing and looking broken.
        eprintln!("[attach] no command blocks -- the shell is not emitting OSC 133");
        return;
    }
    for b in blocks {
        let status = match b.state {
            zest_proto::BlockState::Prompt => "prompt".to_string(),
            zest_proto::BlockState::Running => "running".to_string(),
            // "?" rather than "0": a shell that reported no status is not a
            // shell that reported success.
            zest_proto::BlockState::Finished { exit_code } => {
                exit_code.map_or_else(|| "exit ?".to_string(), |c| format!("exit {c}"))
            }
        };
        let end = b.end_line.map_or_else(|| "..".to_string(), |e| e.to_string());
        println!("  block {} [{}-{}] {status}  {}", b.id, b.prompt_line, end, b.command);
        if !b.cwd.is_empty() {
            println!("          cwd {}", b.cwd);
        }
    }
}

/// Print the grid as text, trailing blanks trimmed.
fn print_rows(rows: &[zest_proto::RowPayload]) {
    println!("┌{}┐", "─".repeat(100));
    for row in rows {
        let line: String = row.runs.iter().map(|r| r.text.as_str()).collect();
        let line = line.trim_end();
        println!("│{line}{}│", " ".repeat(100usize.saturating_sub(line.chars().count())));
    }
    println!("└{}┘", "─".repeat(100));
}

/// Percentiles from the round trips, and the arithmetic said out loud.
///
/// p99 rather than a mean: a mean hides the tail, and the tail is what a typist
/// feels. A round trip here is keystroke bytes on the wire to the delta carrying
/// their echo -- it does not include the renderer, so it is a floor for
/// input-to-paint rather than the number a person experiences.
fn report(samples: &[Duration]) {
    let mut sorted: Vec<u128> = samples.iter().map(Duration::as_micros).collect();
    sorted.sort_unstable();
    let at = |q: f64| sorted[((sorted.len() as f64 - 1.0) * q).round() as usize];
    println!("samples={}", sorted.len());
    println!("min_us={}", sorted[0]);
    println!("p50_us={}", at(0.50));
    println!("p99_us={}", at(0.99));
    println!("max_us={}", sorted[sorted.len() - 1]);
}
