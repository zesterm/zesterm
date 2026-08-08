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

    // Two transports, one loop: the protocol is transport-blind and this
    // example should prove that, not re-litigate it.
    if let Some(addr) = opt("--addr") {
        let stream = match std::net::TcpStream::connect(&addr) {
            Ok(s) => s,
            Err(e) => {
                eprintln!("[attach] could not reach {addr}: {e}");
                eprintln!("[attach] the far daemon needs --listen-lan, and a route");
                std::process::exit(1);
            }
        };
        let closer = stream.try_clone().expect("clone the stream for the deadline thread");
        eprintln!("[attach] connected to {addr} (tcp)");
        run(stream, closer, cmd, seconds, close, ping);
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
        let closer = stream.try_clone().expect("clone the stream for the deadline thread");
        eprintln!("[attach] connected to {socket}");
        run(stream, closer, cmd, seconds, close, ping);
    }
}

fn run<S: Read + Write + Send + 'static>(
    mut stream: S,
    mut closer: S,
    cmd: String,
    seconds: u64,
    close: bool,
    ping: usize,
) {
    let send = |stream: &mut _, msg: &ClientMessage| {
        let bytes = frame::encode(msg).expect("encode");
        Write::write_all(stream, &bytes).expect("write");
        Write::flush(stream).expect("flush");
    };

    // A throwaway key. This is a diagnostic, not a device: it should not
    // accumulate a pairing on every host it is pointed at.
    let identity = zest_mesh::identity::ClientIdentity::generate().expect("client key");
    let client_nonce = zest_mesh::identity::Nonce::random().expect("nonce");
    eprintln!("[attach] client {}", identity.client_id().short());

    send(
        &mut stream,
        &ClientMessage::Hello {
            version: PROTOCOL_VERSION,
            client: identity.client_id(),
            label: "attach".into(),
            nonce: zest_proto::Nonce32::from_bytes(*client_nonce.as_bytes()),
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
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_secs(seconds));
            if close {
                if let Some(session) = *opened.lock().expect("session slot") {
                    eprintln!("[attach] closing {session}");
                    let bytes = frame::encode(&ClientMessage::CloseSession { session })
                        .expect("encode");
                    let _ = Write::write_all(&mut closer, &bytes);
                    let _ = Write::flush(&mut closer);
                    // The daemon hangs the child up synchronously, so give the
                    // write time to be served before this process disappears.
                    std::thread::sleep(Duration::from_millis(500));
                }
            }
            eprintln!("[attach] done");
            std::process::exit(0);
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
            match frame::decode::<HostMessage>(&body).expect("decode") {
                HostMessage::Welcome { host, label, .. } => {
                    eprintln!("[attach] authenticated to {} ({label})", host.short());
                    if let Some(msg) = create.take() {
                        send(&mut stream, &msg);
                    }
                }
                HostMessage::Sessions { sessions } => {
                    for s in &sessions {
                        eprintln!("[attach] session {} {}x{}", s.addr, s.cols, s.rows);
                    }
                    // Attach to the newest, which is the one just created.
                    if !attached {
                        if let Some(s) = sessions.last() {
                            send(
                                &mut stream,
                                &ClientMessage::Attach { session: s.addr, cols: 100, rows: 30 },
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
                        send(&mut stream, &ClientMessage::Input { session, bytes: vec![b'.'] });
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
                            send(&mut stream, &ClientMessage::Input { session, bytes: vec![b'.'] });
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
                HostMessage::Error { message, .. } => eprintln!("[attach] error: {message}"),
                HostMessage::Scrollback { .. } => {}

                HostMessage::Challenge { host, label, nonce, signature, version } => {
                    eprintln!("[attach] host {} ({label}) challenged", host.short());
                    let transcript = zest_mesh::pairing::Transcript {
                        version,
                        host,
                        client: identity.client_id(),
                        host_nonce: zest_mesh::identity::Nonce::from_bytes(nonce.0),
                        client_nonce,
                        host_label: label,
                        client_label: "attach".into(),
                    };
                    let host_sig = zest_mesh::identity::Signature::from_slice(&signature.0)
                        .expect("signature");
                    // No expected host: this example is pointed at a socket by
                    // hand, so there is no advertisement to have been misled by.
                    if let Err(e) =
                        zest_mesh::pairing::verify_challenge(None, &transcript, &host_sig)
                    {
                        eprintln!("[attach] the host did not prove itself: {e}");
                        std::process::exit(1);
                    }
                    let sig = identity.sign(
                        zest_mesh::identity::Purpose::Auth,
                        &zest_mesh::pairing::auth_transcript(&transcript),
                    );
                    send(
                        &mut stream,
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
            }
        }
    }
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
