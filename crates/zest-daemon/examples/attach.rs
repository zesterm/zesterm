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

use std::io::{Read, Write};
use std::time::{Duration, Instant};

use zest_daemon::{connect, default_socket_path};
use zest_proto::{frame, ClientId, ClientMessage, FrameReader, HostMessage, PROTOCOL_VERSION};

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let opt = |name: &str| -> Option<String> {
        args.iter().position(|a| a == name).and_then(|i| args.get(i + 1)).cloned()
    };

    let socket = opt("--socket").unwrap_or_else(default_socket_path);
    let cmd = opt("--cmd").unwrap_or_default();
    let seconds: u64 = opt("--seconds").and_then(|s| s.parse().ok()).unwrap_or(5);

    let mut stream = match connect(&socket) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("[attach] no daemon at {socket}: {e}");
            eprintln!("[attach] start one with: zest-daemon --socket {socket}");
            std::process::exit(1);
        }
    };
    eprintln!("[attach] connected to {socket}");

    let send = |stream: &mut _, msg: &ClientMessage| {
        let bytes = frame::encode(msg).expect("encode");
        Write::write_all(stream, &bytes).expect("write");
        Write::flush(stream).expect("flush");
    };

    send(
        &mut stream,
        &ClientMessage::Hello {
            version: PROTOCOL_VERSION,
            client: ClientId::from_bytes([0xa7; 32]),
            label: "attach".into(),
            // A fixed value, because this example holds no key and the daemon
            // does not yet challenge. It is deliberately not random: a nonce
            // that looks fresh here would suggest this is doing something it
            // is not.
            nonce: zest_proto::Nonce32::from_bytes([0xa7; 32]),
        },
    );
    send(
        &mut stream,
        &ClientMessage::CreateSession { command: cmd, cwd: String::new(), cols: 100, rows: 30 },
    );

    // The deadline is enforced by a separate thread, not by checking between
    // reads: `read` blocks until the daemon sends something, so a quiet session
    // would hold the loop past any deadline the loop itself could notice. That
    // is correct behaviour for a client and wrong for a diagnostic with a time
    // limit.
    std::thread::spawn(move || {
        std::thread::sleep(Duration::from_secs(seconds));
        eprintln!("[attach] done");
        std::process::exit(0);
    });

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
                    eprintln!("[attach] host {} ({label})", host.short());
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
                            attached = true;
                        }
                    }
                }
                HostMessage::Keyframe { rows_data, attrs, modes, .. } => {
                    eprintln!(
                        "[attach] keyframe: {} rows, {} attrs, modes {:?}",
                        rows_data.len(),
                        attrs.len(),
                        zest_core::Modes::from_bits_truncate(modes)
                    );
                    print_rows(&rows_data);
                }
                HostMessage::Update { delta, .. } => {
                    eprintln!(
                        "[attach] +{}ms delta: {} ops, {} new attrs",
                        start.elapsed().as_millis(),
                        delta.ops.len(),
                        delta.attrs.len()
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
                }
                HostMessage::Exited { session, code } => {
                    eprintln!("[attach] session {session} exited ({code:?})");
                }
                HostMessage::Error { message, .. } => eprintln!("[attach] error: {message}"),
                HostMessage::Scrollback { .. } => {}

                // This example holds no key, so it cannot answer a challenge.
                // It says so and stops rather than retrying: a client looping
                // against an authenticating host is how a log fills up.
                HostMessage::Challenge { host, label, .. } => {
                    eprintln!(
                        "[attach] {} ({label}) wants a signed challenge; this example has no key",
                        host.short()
                    );
                    std::process::exit(1);
                }
                HostMessage::AuthPending { code, expires_in_secs } => {
                    eprintln!("[attach] waiting for approval, code {code} ({expires_in_secs}s)");
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
