//! What a build costs an agent, against what it costs the wire.
//!
//! ADR-004 measures *transport*: `cat 1MB` is ~1 MB of pty bytes against ~3 KB
//! of delta. That number gets quoted as though it were the agent-facing saving,
//! and it is not — it says what the *network* carries, not what a model reads.
//! ADR-015 says the two belong side by side. This measures the other one.
//!
//! Four numbers, on one real command:
//!
//! - **pty stream** — everything the child wrote. What a terminal scraping the
//!   stream would put through a model, counting every progress-bar repaint,
//!   cursor move and colour change.
//! - **delta** — the same session as `zest-proto` frames it. ADR-004's number,
//!   re-measured on a command rather than on `cat`.
//! - **`screen` text** — what the tool returns: the final grid, post-VT,
//!   trailing blanks dropped.
//! - **`output` per block** — the same per command, where the shell emits
//!   OSC 133.
//!
//! The saving that matters is the third against the first, and it is **not** a
//! compression ratio. A progress bar rewriting one row four hundred times is
//! one row by the time anything here looks, so the answer is bounded by the
//! size of the grid rather than by how chatty the command was. That is also
//! why the ratio gets *better* the noisier the command is, which a compression
//! figure would not do.
//!
//! # It asks the real reader, not a copy of it
//!
//! The last two numbers come from a [`Replica`] fed the encoder's own output —
//! the same type `screen` and `output` answer from, with the same scrollback
//! bound. A reimplementation here would drift from the tool and quietly report
//! a saving nobody receives.
//!
//! # Why it spawns rather than replaying a fixture
//!
//! #274's PR-E proposed measuring "over the existing recordings". The corpus
//! cannot answer this: its largest recording is 10 KB of `vim`, there is no
//! build in it, and the roadmap asks specifically about `cargo build`,
//! `npm install` and `pytest`. Committing build logs to make a table would pay
//! storage forever for a number that moves with every toolchain.
//!
//! So it runs the command, and the number is about *your* build on *your*
//! machine — the only version of it worth quoting.
//!
//! ```text
//! cargo run -p zest-mcp --example token_probe -- --cmd "cargo build"
//! cargo run -p zest-mcp --example token_probe -- --size 120x30 --cmd "ls -la"
//! ```

use std::io::{Read, Write};
use std::time::{Duration, Instant};

use zest_core::{Modes, Terminal};
use zest_mcp::Replica;
use zest_proto::{CursorState, Encoder, HostId, HostMessage, Seq, SessionAddr, SessionId};
use zest_pty::{CommandSpec, PtySize, PtyTransport};

/// How much scrollback the *host* keeps.
///
/// Generous, and deliberately larger than the replica's: the point is to let
/// the host hold the whole build so the replica's own bound is what limits the
/// answer. A client cannot read more than it was sent, and pretending otherwise
/// would report text no tool could return.
const HOST_SCROLLBACK: usize = 100_000;

/// Bytes per token, roughly, for the two shapes measured here.
///
/// Prose and code tokenise at about four bytes per token. A pty stream does
/// not: it is dense in escape sequences, and `\x1b[38;5;42m` is several tokens
/// of pure punctuation. Two constants rather than one, because using the prose
/// figure for both is what makes the raw stream look cheaper than it is —
/// which would understate the very saving this probe exists to show.
///
/// Estimates, and labelled as such in the output. The byte counts are exact;
/// only the division is not.
const BYTES_PER_TOKEN_TEXT: f64 = 4.0;
const BYTES_PER_TOKEN_VT: f64 = 2.5;

/// How often a delta is asked for, by default: one display frame.
///
/// **The delta figure is a function of this and almost nothing else**, which is
/// the single most misreadable thing here. `zest-proto` coalesces on *state*,
/// not on queued bytes: a subscriber holds an encoder shadow and asks for the
/// difference from what it last sent, so a client that asks a hundred times
/// less often gets one delta describing the current grid rather than a backlog
/// of a hundred (`DaemonConfig::min_delta_interval`).
///
/// So asking after every pty read -- `--coalesce-ms 0` -- measures the worst
/// case the protocol allows and not what any client experiences: `seq 1 200000`
/// asks 13,497 times and pays framing on each. ADR-004's ~3 KB-for-1 MB is the
/// coalesced regime, and comparing an uncoalesced number against it reads as a
/// contradiction when it is a different measurement.
///
/// 16 ms is a frame, which is the cadence a client attached to a visible pane
/// actually reads at. `--relay` sets 30.
const DEFAULT_COALESCE_MS: u64 = 16;

/// The framed size of one message, or a stop.
///
/// **Never `unwrap_or(0)`.** A failed encode silently subtracts a whole message
/// from the transport column, and the answer still looks like a measurement --
/// which is the one way this probe could mislead without anybody noticing.
fn framed(msg: &HostMessage) -> usize {
    match zest_proto::frame::encode(msg) {
        Ok(b) => b.len(),
        Err(e) => {
            eprintln!("token_probe: a message would not frame ({e}); the byte counts would be wrong");
            std::process::exit(1);
        }
    }
}

fn cursor(t: &Terminal) -> CursorState {
    let c = t.cursor();
    CursorState {
        row: u16::try_from(c.row).unwrap_or(0),
        col: u16::try_from(c.col).unwrap_or(0),
        visible: t.modes().contains(Modes::SHOW_CURSOR),
        shape: 0,
    }
}

fn flag<'a>(args: &'a [String], name: &str) -> Option<&'a str> {
    args.iter().position(|a| a == name).and_then(|i| args.get(i + 1)).map(String::as_str)
}

fn parse_size(s: &str) -> Option<PtySize> {
    let (w, h) = s.split_once('x')?;
    Some(PtySize::new(w.parse().ok()?, h.parse().ok()?))
}

#[allow(clippy::too_many_lines)]
fn main() -> std::process::ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.iter().any(|a| a == "--help" || a == "-h") {
        eprintln!(
            "usage: token_probe --cmd \"<command>\" [--size <cols>x<rows>]\n\
             \n\
             Runs the command on a real pty and reports what it costs four ways:\n\
             the raw pty stream, the framed deltas, the `screen` tool's text,\n\
             and `output` per command block.\n\
             \n\
             --cmd \"<program>\"  run a program directly; no shell, no blocks.\n\
             --run \"<command>\"  type it into a shell, so `output` has blocks\n\
             to report -- OSC 133 comes from precmd/preexec hooks, which do\n\
             not fire for `zsh -c`.\n\
             --coalesce-ms <n>  how often a delta is asked for; default \
             {DEFAULT_COALESCE_MS}.\n\
             0 asks after every read, which is the worst case the protocol\n\
             allows and not what any client sees -- deltas coalesce on state,\n\
             so asking less often yields fewer and not larger ones."
        );
        return std::process::ExitCode::SUCCESS;
    }

    let size = flag(&args, "--size").and_then(parse_size).unwrap_or(PtySize::new(120, 30));
    let coalesce = Duration::from_millis(
        flag(&args, "--coalesce-ms").and_then(|s| s.parse().ok()).unwrap_or(DEFAULT_COALESCE_MS),
    );
    // `--run` drives the *shell*, which is the only way to reach `output`:
    // OSC 133 comes from a shell's precmd/preexec hooks, and those do not fire
    // for `zsh -c`. `--cmd` spawns the program directly and measures a session
    // with no blocks in it at all.
    let typed = flag(&args, "--run");
    let command = match (flag(&args, "--cmd"), typed) {
        (Some(c), None) => c.to_string(),
        (None, Some(_)) => CommandSpec::default_shell().command_line,
        (Some(_), Some(_)) => {
            eprintln!("token_probe: --cmd runs a program, --run types into a shell; pick one");
            return std::process::ExitCode::FAILURE;
        }
        (None, None) => {
            eprintln!("token_probe needs --cmd \"<program>\" or --run \"<command>\"; see --help");
            return std::process::ExitCode::FAILURE;
        }
    };

    let mut spec = CommandSpec::default_shell();
    spec.command_line = command.clone();
    // Blocks, where the shell can carry them. Without this `output` is always
    // "no OSC 133" and the fourth number never gets exercised at all -- which
    // is how a bug in counting it would survive every run.
    if let Some(dir) = std::env::var_os("TMPDIR").map(std::path::PathBuf::from) {
        spec.enable_shell_integration(&dir);
    }
    let mut pty = match zest_pty::NativePty::spawn(&spec, size) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("token_probe: spawn failed: {e}");
            return std::process::ExitCode::FAILURE;
        }
    };
    let mut reader = pty.take_reader().expect("a reader");

    // Typed as a person would, after the shell has drawn a prompt -- a command
    // written before the hooks load produces no block, which is the same
    // failure #363 chases in the live suite.
    if let Some(line) = typed {
        let mut writer = pty.writer();
        let line = line.to_string();
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(700));
            let _ = writer.write_all(format!("{line}\r").as_bytes());
            let _ = writer.flush();
            std::thread::sleep(Duration::from_millis(400));
            let _ = writer.write_all(b"exit\r");
            let _ = writer.flush();
        });
    }

    let mut term = Terminal::new(usize::from(size.cols), usize::from(size.rows), HOST_SCROLLBACK);
    let mut enc = Encoder::new();
    let addr = SessionAddr { host: HostId::from_bytes([0x2e; 32]), session: SessionId(1) };

    // Counted as the daemon sends it: a real framed `HostMessage`, not the
    // encoder's inner struct.
    let k = enc.keyframe(term.grid(), cursor(&term), term.modes(), term.title(), term.blocks());
    let mut delta_bytes = framed(&HostMessage::Keyframe {
        session: addr,
        seq: Seq(term.seq()),
        cols: k.cols,
        rows: k.rows,
        rows_data: k.rows_data.clone(),
        attrs: k.attrs.clone(),
        cursor: k.cursor,
        modes: k.modes.bits(),
        blocks: k.blocks.clone(),
        blocks_from: k.blocks_from,
        title: k.title.clone(),
        history_clears: k.history_clears,
    });

    // The client, so the last two numbers are what a tool would actually
    // return rather than a second reading of the host's grid.
    let mut replica = Replica::new(addr, &k, term.seq());

    let mut pty_bytes = 0usize;
    let mut updates = 0usize;
    let started = Instant::now();
    let mut since_delta = Instant::now();
    let mut buf = [0u8; 8192];

    eprintln!("token_probe: {}x{}, running: {command}", size.cols, size.rows);

    loop {
        match reader.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => {
                pty_bytes += n;
                term.advance(&buf[..n]);

                // Ask on a cadence, because that is what a subscriber does and
                // what the delta figure is a function of. Nothing is lost by
                // not asking: the encoder shadow means the next delta describes
                // the whole difference since the last one.
                if since_delta.elapsed() < coalesce {
                    continue;
                }
                since_delta = Instant::now();
                let d =
                    enc.delta(term.grid(), cursor(&term), term.modes(), term.title(), term.blocks());
                // The **client's** sequence, not the host's. `base` says what
                // the receiver already holds, so reading it off the host makes
                // every delta claim to follow one the client never applied.
                let base = replica.seq();
                // `blocks` too: `Delta` carries block upserts in their own
                // field rather than as a `DeltaOp`, so a batch that only marks
                // a command finished has no ops at all. Skipping those desyncs
                // the replica's block list and undercounts `output` -- one of
                // the four numbers this exists to report.
                if d.ops.is_empty() && d.attrs.is_empty() && d.blocks.is_empty() {
                    continue;
                }
                updates += 1;
                delta_bytes += framed(&HostMessage::Update {
                    session: addr,
                    base: Seq(base),
                    seq: Seq(term.seq()),
                    delta: d.clone(),
                });
                // A `false` here is `Applied::NeedsKeyframe`: the replica has
                // fallen behind and everything it reports afterwards describes
                // a screen the host does not have. That would understate the
                // last two numbers silently, which is the one failure this
                // probe must not have -- so it stops rather than prints.
                if !replica.apply(&d, base, term.seq()) {
                    eprintln!(
                        "token_probe: the replica desynced after {pty_bytes} bytes. A real \
                         client answers this with `RequestKeyframe`; there is no daemon here \
                         to ask, and continuing would measure a screen nobody holds."
                    );
                    return std::process::ExitCode::FAILURE;
                }
            }
            // A signal landing while a read is parked is not the end of the
            // stream, and treating it as one truncates the measurement.
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
            // Anything else is a failure, not an ending. `PtyReader` already
            // normalizes EOF to `Ok(0)` on both platforms -- the Unix drain
            // thread turns `EIO` into it and the Windows reader does the same
            // with `ERROR_BROKEN_PIPE` -- so a real error here means the
            // measurement is short by an unknown amount, and reporting it
            // anyway would be reporting a number that is simply wrong.
            Err(e) => {
                eprintln!("token_probe: read failed after {pty_bytes} bytes: {e}");
                return std::process::ExitCode::FAILURE;
            }
        }
    }
    // The last delta, unconditionally: the loop skips whatever fell inside the
    // final window, and without this the screen a model reads is the one from
    // up to `coalesce` before the command ended -- which for a fast command is
    // no screen at all.
    let d = enc.delta(term.grid(), cursor(&term), term.modes(), term.title(), term.blocks());
    if !d.ops.is_empty() || !d.attrs.is_empty() || !d.blocks.is_empty() {
        let base = replica.seq();
        updates += 1;
        delta_bytes += framed(&HostMessage::Update {
            session: addr,
            base: Seq(base),
            seq: Seq(term.seq()),
            delta: d.clone(),
        });
        assert!(replica.apply(&d, base, term.seq()), "the final delta must apply");
    }
    let elapsed = started.elapsed();

    let screen_bytes = replica.screen_text().len();
    let blocks = replica.blocks();
    let output_bytes: usize = blocks
        .iter()
        .filter_map(|b| replica.block_rows(b.id))
        // `join`, not a sum of lengths plus one each: `output` returns the rows
        // joined by newlines with none trailing, so counting a separator per row
        // overstates every block by one.
        .map(|rows| rows.join("\n").len())
        .sum();

    let tok = |bytes: usize, per: f64| (bytes as f64 / per).round() as usize;
    let pct = |part: usize| {
        if pty_bytes == 0 {
            0.0
        } else {
            100.0 * part as f64 / pty_bytes as f64
        }
    };
    let row = |label: &str, bytes: usize, per: f64| {
        println!("{:<24} {:>12} {:>12} {:>8.2}%", label, bytes, tok(bytes, per), pct(bytes));
    };

    println!();
    println!("command   {command}");
    println!(
        "grid      {}x{}   {:.1}s   {updates} deltas at {}ms   {} block(s)",
        size.cols,
        size.rows,
        elapsed.as_secs_f64(),
        coalesce.as_millis(),
        blocks.len()
    );
    println!();
    println!("{:<24} {:>12} {:>12} {:>9}", "", "bytes", "~tokens", "of pty");
    row("pty stream", pty_bytes, BYTES_PER_TOKEN_VT);
    row("delta (transport)", delta_bytes, BYTES_PER_TOKEN_VT);
    row("screen text (model)", screen_bytes, BYTES_PER_TOKEN_TEXT);
    if blocks.is_empty() {
        println!("{:<24} {:>12}", "output", "no OSC 133");
    } else {
        row("output, all blocks", output_bytes, BYTES_PER_TOKEN_TEXT);
    }
    println!();
    println!(
        "Tokens are estimated at ~{BYTES_PER_TOKEN_TEXT} bytes each for text and \
         ~{BYTES_PER_TOKEN_VT} for a pty\nstream, which is denser in punctuation; \
         the byte counts are exact. `delta` is\nwhat the wire carries (ADR-004); \
         `screen` and `output` are what a model reads\n(ADR-015), and are bounded \
         by the grid rather than by how much was printed."
    );

    std::process::ExitCode::SUCCESS
}
