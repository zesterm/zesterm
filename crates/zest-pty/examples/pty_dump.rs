//! Spawn a command in a pty and dump what comes back.
//!
//! Two jobs:
//!
//! 1. **Diagnosis.** Shows the raw VT byte stream with escapes made visible, so
//!    ConPTY behavior can be inspected directly rather than inferred from a
//!    failing assertion.
//! 2. **Corpus recording.** With `--record <file>`, writes a `.vtrec` of
//!    timestamped byte chunks. Replaying those through `zest-core` is the
//!    highest-value regression test the project has, and capturing them costs
//!    minutes.
//!
//! ```text
//! cargo run -p zest-pty --example pty_dump                      # default shell, interactive
//! cargo run -p zest-pty --example pty_dump -- --cmd "cmd.exe /c echo hi"
//! cargo run -p zest-pty --example pty_dump -- --record vim.vtrec --cmd "vim"
//!
//! # a height drag: fill the screen, shrink, grow back
//! cargo run -p zest-pty --example pty_dump -- --record resize-drag.vtrec \
//!     --cmd "pwsh -NoLogo -c ls; Start-Sleep 6" \
//!     --size 100x30 --resize 100x8 --resize 100x30
//! ```
//!
//! `--resize` may be given more than once and the steps run in order, which is
//! what a *drag* needs: the shrink and the grow answer differently, and it is
//! the grow's repaint that `Grid::settle_restate` turns on (#247).

use std::io::{Read, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use zest_pty::{CommandSpec, PtySize, PtyTransport};

/// Timestamped output chunks, shared between the reader thread and the writer.
type Recording = Arc<Mutex<Vec<(u128, Vec<u8>)>>>;

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mut command_line = None;
    let mut record_path = None;
    let mut raw = false;
    let mut idle_exit = None::<Duration>;
    let mut spawn_size = PtySize::new(120, 30);
    // Each `--resize` with the delay in force when it was parsed, in order.
    let mut resizes: Vec<(PtySize, Duration)> = Vec::new();
    let mut resize_after = Duration::from_millis(1500);

    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--cmd" => {
                command_line = args.get(i + 1).cloned();
                i += 2;
            }
            "--record" => {
                record_path = args.get(i + 1).cloned();
                i += 2;
            }
            // Print bytes verbatim instead of escaping them, so the output can
            // be piped somewhere that understands VT.
            "--raw" => {
                raw = true;
                i += 1;
            }
            // Exit once the stream has been quiet this long. Makes the tool
            // usable for non-interactive capture without hanging.
            "--idle-exit-ms" => {
                idle_exit = args.get(i + 1).and_then(|s| s.parse().ok()).map(Duration::from_millis);
                i += 2;
            }
            // The size the pty is spawned at. Worth stating rather than
            // inheriting the default when a recording is going to be replayed:
            // the grid the replay builds has to start where this one did.
            "--size" => {
                let Some(size) = args.get(i + 1).and_then(|s| parse_size(s)) else {
                    eprintln!("--size wants <cols>x<rows>, e.g. 100x30");
                    std::process::exit(2);
                };
                spawn_size = size;
                i += 2;
            }
            // Resize the pty mid-capture, which is the only way to see what a
            // shell emits in answer. ConPTY repaints on a resize and what that
            // repaint contains decides whether the block index survives it --
            // an ED would take every block on screen with it. Inferring that
            // from a rendered pane is guesswork; this shows the bytes. (#200)
            //
            // **Repeatable, and that is the point rather than a convenience.**
            // A drag is a shrink *and* a grow, and the two answer differently:
            // the shrink's repaint restates what still fits, the grow's restates
            // what it kept and blanks the rest. It is the grow that
            // `Grid::settle_restate` turns on (#247), and one resize per capture
            // cannot reach it -- there is nothing to grow back from.
            "--resize" => {
                let Some(size) = args.get(i + 1).and_then(|s| parse_size(s)) else {
                    eprintln!("--resize wants <cols>x<rows>, e.g. 40x30");
                    std::process::exit(2);
                };
                resizes.push((size, resize_after));
                i += 2;
            }
            // Applies to every `--resize` *after* it on the command line, so a
            // drag can pause differently before each step. Order matters here in
            // a way it does not for the other flags.
            "--resize-after-ms" => {
                let Some(ms) = args.get(i + 1).and_then(|s| s.parse().ok()) else {
                    eprintln!("--resize-after-ms wants milliseconds");
                    std::process::exit(2);
                };
                resize_after = Duration::from_millis(ms);
                i += 2;
            }
            other => {
                eprintln!("unknown argument: {other}");
                std::process::exit(2);
            }
        }
    }

    let mut spec = CommandSpec::default_shell();
    if let Some(cl) = command_line {
        spec.command_line = cl;
    }
    // The size is logged because a `.vtrec` does not record it and a replay
    // needs it: the bytes before the first resize were laid out for this width,
    // so a grid built at another one wraps them somewhere ConPTY never did.
    eprintln!(
        "[pty_dump] spawning at {}x{}: {}",
        spawn_size.cols, spawn_size.rows, spec.command_line
    );

    let mut pty = match zest_pty::NativePty::spawn(&spec, spawn_size) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("[pty_dump] spawn failed: {e}");
            std::process::exit(1);
        }
    };

    let mut reader = pty.take_reader().expect("reader");
    let mut writer = pty.writer();

    let started = Instant::now();
    let last_byte = Arc::new(Mutex::new(Instant::now()));
    let total = Arc::new(Mutex::new(0usize));
    let done = Arc::new(AtomicBool::new(false));

    // Reader thread: drain to EOF. EOF only arrives once the pseudoconsole is
    // closed, so this thread must stay alive across shutdown.
    let recording: Recording = Arc::new(Mutex::new(Vec::new()));
    let rec_w = Arc::clone(&recording);
    let last_w = Arc::clone(&last_byte);
    let total_w = Arc::clone(&total);
    let done_w = Arc::clone(&done);
    let want_record = record_path.is_some();

    let reader_thread = std::thread::spawn(move || {
        let mut buf = [0u8; 8192];
        loop {
            match reader.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => {
                    let chunk = &buf[..n];
                    *last_w.lock().unwrap() = Instant::now();
                    *total_w.lock().unwrap() += n;

                    if want_record {
                        rec_w
                            .lock()
                            .unwrap()
                            .push((started.elapsed().as_micros(), chunk.to_vec()));
                    }

                    let mut out = std::io::stdout().lock();
                    if raw {
                        let _ = out.write_all(chunk);
                    } else {
                        let _ = out.write_all(escape(chunk).as_bytes());
                    }
                    let _ = out.flush();
                }
                Err(e) => {
                    eprintln!("\n[pty_dump] read error: {e}");
                    break;
                }
            }
        }
        done_w.store(true, Ordering::Release);
    });

    // Forward our stdin to the pty, so interactive sessions work.
    std::thread::spawn(move || {
        let mut stdin = std::io::stdin().lock();
        let mut buf = [0u8; 1024];
        while let Ok(n) = stdin.read(&mut buf) {
            if n == 0 || writer.write_all(&buf[..n]).is_err() {
                break;
            }
        }
    });

    // The resize, once the shell has had time to print a prompt and whatever
    // `--cmd` produced. Marked in the stream so the repaint that follows is
    // unambiguous -- everything after this line is the shell's answer to it.
    for (size, after) in &resizes {
        std::thread::sleep(*after);
        eprintln!("\n[pty_dump] --- resize to {}x{} ---", size.cols, size.rows);
        match pty.resize(*size) {
            Ok(()) => {}
            Err(e) => eprintln!("[pty_dump] resize failed: {e}"),
        }
        // Let this repaint finish before the next resize is asked for. Two
        // overlapping repaints are a capture nobody can reason about, and the
        // recording would be of the tool rather than of ConPTY.
        std::thread::sleep(Duration::from_millis(800));
        eprintln!("\n[pty_dump] --- end of the resize repaint ---");
    }

    // Wait for the child, then keep draining until the stream goes quiet --
    // ConPTY paints asynchronously, so process exit does not mean the output
    // has been produced yet.
    let quiet = idle_exit.unwrap_or(Duration::from_millis(400));
    let _ = pty.wait_for_child(None);
    eprintln!("\n[pty_dump] child exited; draining until {}ms of quiet", quiet.as_millis());

    loop {
        std::thread::sleep(Duration::from_millis(20));
        if done.load(Ordering::Acquire) {
            break;
        }
        if last_byte.lock().unwrap().elapsed() > quiet {
            break;
        }
    }

    // Closing the pseudoconsole is what finally gives the reader EOF.
    drop(pty);
    let _ = reader_thread.join();

    let n = *total.lock().unwrap();
    eprintln!("[pty_dump] {n} bytes in {:.2}s", started.elapsed().as_secs_f32());

    if let Some(path) = record_path {
        let rec = recording.lock().unwrap();
        match write_vtrec(&path, &rec) {
            Ok(()) => eprintln!("[pty_dump] wrote {} chunks to {path}", rec.len()),
            Err(e) => eprintln!("[pty_dump] could not write {path}: {e}"),
        }
    }
}

/// `<cols>x<rows>`, the shape a person types.
fn parse_size(s: &str) -> Option<PtySize> {
    let (cols, rows) = s.split_once(['x', 'X'])?;
    Some(PtySize::new(cols.trim().parse().ok()?, rows.trim().parse().ok()?))
}

/// Render control bytes visibly so escape sequences can be read.
fn escape(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for &b in bytes {
        match b {
            0x1b => s.push_str("\x1b[7m ESC \x1b[0m"),
            b'\n' => s.push_str("\\n\n"),
            b'\r' => s.push_str("\\r"),
            0x07 => s.push_str("<BEL>"),
            0x08 => s.push_str("<BS>"),
            0x09 => s.push_str("<TAB>"),
            0x00..=0x1f => s.push_str(&format!("<{b:02x}>")),
            _ => s.push(b as char),
        }
    }
    s
}

/// `.vtrec` format, deliberately trivial: a text header then length-prefixed
/// binary chunks. Simple enough to parse in ten lines from a test.
///
/// ```text
/// VTREC1\n
/// <micros:u64le><len:u32le><bytes>...
/// ```
fn write_vtrec(path: &str, chunks: &[(u128, Vec<u8>)]) -> std::io::Result<()> {
    let mut f = std::io::BufWriter::new(std::fs::File::create(path)?);
    f.write_all(b"VTREC1\n")?;
    for (micros, bytes) in chunks {
        f.write_all(&(*micros as u64).to_le_bytes())?;
        f.write_all(&(bytes.len() as u32).to_le_bytes())?;
        f.write_all(bytes)?;
    }
    f.flush()
}
