//! Pair with a host over the LAN, and compare the code on both screens.
//!
//! The two-machine check no unit test can perform. Everything about the
//! handshake is tested in-process — including the replay case, which is
//! *easier* to write there — but the one thing that cannot be is a person
//! looking at two screens and deciding the codes match. That comparison is the
//! only defence against a relay until the data plane is encrypted, and it works
//! only if someone actually does it.
//!
//! ```text
//! # on the host
//! zest-daemon --listen-lan
//!
//! # on the other machine
//! cargo run -p zest-daemon --example pair -- --addr 192.168.1.42:7717
//! ```
//!
//! An example rather than a test, for the same reason `mesh_probe` is one: it
//! needs a second box and a human.

use std::io::{Read, Write};
use std::net::TcpStream;
use std::time::{Duration, Instant};

use zest_proto::{frame, ClientMessage, FrameReader, HostMessage, PROTOCOL_VERSION};

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let opt = |name: &str| -> Option<String> {
        args.iter().position(|a| a == name).and_then(|i| args.get(i + 1)).cloned()
    };

    let addr = opt("--addr").unwrap_or_else(|| {
        eprintln!("usage: pair --addr <host:port> [--label <name>] [--expect <64-hex host id>]");
        std::process::exit(2);
    });
    let label = opt("--label").unwrap_or_else(|| "pair-example".into());

    // Optional, and the interesting half: with it, this refuses a host that is
    // not the one named, which is what the host signing first is *for*. An
    // address is a claim; a HostId is checkable.
    let expect = opt("--expect").map(|h| {
        let mut out = [0u8; 32];
        assert!(h.len() == 64, "a host id is 64 hex characters");
        for (i, pair) in h.as_bytes().chunks_exact(2).enumerate() {
            out[i] = u8::from_str_radix(std::str::from_utf8(pair).expect("hex"), 16).expect("hex");
        }
        zest_proto::HostId::from_bytes(out)
    });

    let identity =
        std::sync::Arc::new(zest_mesh::identity::ClientIdentity::generate().expect("client key"));
    println!("this device: {}", identity.client_id().short());
    println!("full id:     {}", hex(&identity.client_id().0));
    println!();
    println!("If the host does not know this device it will ask, and print a code.");
    println!("Compare it with the one below before approving.\n");

    let mut stream = match TcpStream::connect(&addr) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("could not reach {addr}: {e}");
            std::process::exit(1);
        }
    };

    let mut hs =
        zest_mesh::pairing::ClientHandshake::new(std::sync::Arc::clone(&identity), label.clone())
            .expect("client handshake");
    let hello = ClientMessage::Hello {
        version: PROTOCOL_VERSION,
        client: identity.client_id(),
        label: label.clone(),
        nonce: zest_proto::Nonce32::from_bytes(*hs.nonce().as_bytes()),
        dh: zest_proto::Pub32::from_bytes(hs.dh().0),
        watch_sessions: false,
        watch_pairings: false,
        watch_hosts: false,
        watch_signals: false,
    };
    write(&mut stream, None, &hello);

    // Everything from the `Auth` on is sealed, in both directions. Held here
    // rather than inside the loop because a pairing can sit at `AuthPending`
    // for two minutes and the channel has to survive that wait.
    let mut channel: Option<zest_mesh::secure::SecureChannel> = None;
    let mut frames = FrameReader::new();
    let mut buf = vec![0u8; 64 * 1024];
    let deadline = Instant::now() + Duration::from_secs(180);

    loop {
        while let Ok(Some(body)) = frames.next_frame() {
            let body = match channel.as_mut() {
                Some(ch) => match ch.open(&body) {
                    Ok(plain) => plain,
                    Err(e) => {
                        eprintln!("a sealed frame did not open: {e}");
                        std::process::exit(1);
                    }
                },
                None => body,
            };
            match frame::decode::<HostMessage>(&body).expect("decode") {
                HostMessage::Challenge { version, host, label: host_label, nonce, dh, signature } => {
                    let host_sig = zest_mesh::identity::Signature::from_slice(&signature.0)
                        .expect("signature");

                    // Verifies, signs and derives, in that order -- the same
                    // call the app makes, so what this example proves about a
                    // real host is what the app would find.
                    let (sig, transcript, derived) = match hs.on_challenge(
                        expect,
                        &zest_mesh::pairing::Challenge {
                            version,
                            host,
                            label: host_label.clone(),
                            nonce: zest_mesh::identity::Nonce::from_bytes(nonce.0),
                            dh: zest_mesh::secure::DhPublic(dh.0),
                            signature: host_sig,
                        },
                    ) {
                        Ok(v) => v,
                        Err(e) => {
                            eprintln!("the host did not prove itself: {e}");
                            std::process::exit(1);
                        }
                    };
                    let code = zest_mesh::pairing::pairing_code(&transcript)
                        .expect("a transcript that signed has a code");
                    println!("host:        {} ({host_label})", host.short());
                    println!("code:        {}", spaced(&code));
                    println!("\nwaiting for approval...\n");

                    channel = Some(derived);
                    write(
                        &mut stream,
                        channel.as_mut(),
                        &ClientMessage::Auth {
                            signature: zest_proto::Sig64::from_bytes(sig.to_bytes()),
                        },
                    );
                }
                HostMessage::AuthPending { code, expires_in_secs } => {
                    // The host computed the same code independently. If these
                    // two differ, something is relaying.
                    println!("the host is showing: {} ({expires_in_secs}s)", spaced(&code));
                }
                HostMessage::Welcome { host, label, .. } => {
                    println!("\npaired. serving as {} ({label})", host.short());
                    println!("this device is now trusted by that host.");
                    return;
                }
                HostMessage::AuthFailed { reason, message } => {
                    eprintln!("\nrefused: {reason:?} -- {message}");
                    std::process::exit(1);
                }
                other => {
                    eprintln!("unexpected: {other:?}");
                }
            }
        }

        if Instant::now() > deadline {
            eprintln!("nobody answered");
            std::process::exit(1);
        }
        match stream.read(&mut buf) {
            Ok(0) => {
                eprintln!("the host closed the connection");
                std::process::exit(1);
            }
            Ok(n) => frames.feed(&buf[..n]),
            Err(e) => {
                eprintln!("read failed: {e}");
                std::process::exit(1);
            }
        }
    }
}

fn write(
    stream: &mut TcpStream,
    channel: Option<&mut zest_mesh::secure::SecureChannel>,
    msg: &ClientMessage,
) {
    let body = frame::encode_body(msg).expect("encode");
    let body = match channel {
        Some(ch) => ch.seal(&body).expect("seal"),
        None => body,
    };
    let bytes = frame::frame_bytes(&body).expect("frame");
    stream.write_all(&bytes).expect("write");
    stream.flush().expect("flush");
}

/// `428913` as `428 913`, which is how a person reads it aloud.
fn spaced(code: &str) -> String {
    let mid = code.len() / 2;
    format!("{} {}", &code[..mid], &code[mid..])
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}
