//! A working terminal with no window.
//!
//! Spawns a shell, feeds its output through the VT interpreter, and prints the
//! resulting grid. This is the integration checkpoint before any GPU work: if
//! the text here is right, everything upstream of the renderer is correct, and
//! remaining bugs are rendering bugs.
//!
//! It is also, not incidentally, the shape the M3 daemon takes — a terminal
//! with no window that someone else draws.
//!
//! ```text
//! cargo run -p zest-app --example headless
//! cargo run -p zest-app --example headless -- --cmd "pwsh -NoLogo -NoProfile -c ls"
//! ```

use std::io::Read;
use std::sync::mpsc;
use std::time::{Duration, Instant};

use zest_core::{Terminal, TermEvent};
use zest_pty::{CommandSpec, PtySize, PtyTransport};

const COLS: u16 = 100;
const ROWS: u16 = 30;

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mut spec = CommandSpec::default_shell();
    if let Some(i) = args.iter().position(|a| a == "--cmd") {
        if let Some(cmd) = args.get(i + 1) {
            spec.command_line = cmd.clone();
        }
    }
    let show_raw = args.iter().any(|a| a == "--raw");

    eprintln!("[headless] {} at {COLS}x{ROWS}", spec.command_line);

    let mut pty = match zest_pty::NativePty::spawn(&spec, PtySize::new(COLS, ROWS)) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("[headless] spawn failed: {e}");
            std::process::exit(1);
        }
    };

    let mut reader = pty.take_reader().expect("reader");
    let mut writer = pty.writer();

    // The reader thread does nothing but move bytes. Parsing happens on the
    // main thread here for simplicity; in the real app the parser lives with
    // the reader and mutates shared state under a fair mutex.
    let (tx, rx) = mpsc::channel::<Vec<u8>>();
    std::thread::spawn(move || {
        let mut buf = [0u8; 8192];
        loop {
            match reader.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    if tx.send(buf[..n].to_vec()).is_err() {
                        break;
                    }
                }
            }
        }
    });

    let mut term = Terminal::new(COLS as usize, ROWS as usize, 1000);
    let mut total = 0usize;
    let start = Instant::now();
    let mut last_byte = Instant::now();

    // Drain until the stream goes quiet. ConPTY paints asynchronously, so
    // "the child exited" is not the same as "the output has arrived".
    loop {
        match rx.recv_timeout(Duration::from_millis(50)) {
            Ok(chunk) => {
                total += chunk.len();
                last_byte = Instant::now();
                if show_raw {
                    eprintln!("[raw] {:?}", String::from_utf8_lossy(&chunk));
                }
                term.advance(&chunk);

                // Anything the terminal owes the program, write back. Without
                // this, a DSR or OSC-11 query hangs whatever asked.
                for event in term.take_events() {
                    match event {
                        TermEvent::Reply(bytes) => {
                            use std::io::Write;
                            let _ = writer.write_all(&bytes);
                        }
                        TermEvent::Title(t) => eprintln!("[headless] title: {t}"),
                        TermEvent::Bell => eprintln!("[headless] bell"),
                        _ => {}
                    }
                }
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
            Err(mpsc::RecvTimeoutError::Timeout) => {
                if last_byte.elapsed() > Duration::from_millis(600) {
                    break;
                }
            }
        }
    }

    drop(pty);

    let grid = term.grid();
    println!("\n┌{}┐", "─".repeat(COLS as usize));
    for row in 0..grid.rows() {
        let text = grid.row(row).text();
        let padded: String = text.chars().take(COLS as usize).collect();
        let width = padded.chars().count();
        println!("│{padded}{}│", " ".repeat((COLS as usize).saturating_sub(width)));
    }
    println!("└{}┘", "─".repeat(COLS as usize));

    // Colour lives in the cells, not in the text, so a text-only dump cannot
    // tell "the program sent no colour" apart from "we dropped it" -- and those
    // have completely different fixes.
    if args.iter().any(|a| a == "--colors") {
        println!("\n[headless] non-default backgrounds by row:");
        for row in 0..grid.rows() {
            let r = grid.row(row);
            let mut runs: Vec<String> = Vec::new();
            let mut col = 0usize;
            while col < grid.cols() {
                let bg = r.get(col).copied().unwrap_or_default().bg;
                let start = col;
                while col < grid.cols()
                    && r.get(col).copied().unwrap_or_default().bg == bg
                {
                    col += 1;
                }
                if !matches!(bg, zest_core::Color::Default) {
                    runs.push(format!("{start}..{col} {bg:?}"));
                }
            }
            if !runs.is_empty() {
                println!("  row {row}: {}", runs.join(", "));
            }
        }
    }

    let cursor = term.cursor();
    eprintln!(
        "[headless] {total} bytes in {:.2}s | seq {} | cursor {},{} | scrollback {} | modes {:?}",
        start.elapsed().as_secs_f32(),
        term.seq(),
        cursor.row,
        cursor.col,
        grid.scrollback_len(),
        term.modes(),
    );
}
