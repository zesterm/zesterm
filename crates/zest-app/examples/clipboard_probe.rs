//! What this session's clipboard can actually do.
//!
//! An example rather than a test, for `alpha_probe`'s reason: it needs a real
//! display server and its output is a verdict, not an assertion. CI runners
//! have no session, and a test that cannot run there is worse than a probe
//! that says why.
//!
//! The question it answers is genuinely per-machine. `arboard` reaches Wayland
//! through `zwlr_data_control_manager_v1` and PRIMARY needs **version 2** of
//! that protocol, which plenty of compositors do not offer -- so "does
//! middle-click paste the selection here" has no answer that can be read off
//! the source.
//!
//! ```text
//! cargo run -p zest-app --example clipboard_probe
//! ```

fn main() {
    let mut clipboard = match arboard::Clipboard::new() {
        Ok(c) => c,
        Err(e) => {
            println!("clipboard unavailable: {e}");
            println!(
                "\nOn Wayland this means neither the wlr data-control protocol nor an X11 \n\
                 display was reachable. zesterm degrades to no copy/paste at all."
            );
            return;
        }
    };

    println!("=== CLIPBOARD ===");
    match clipboard.get_text() {
        Ok(t) => println!("  reads: {:?}", truncate(&t)),
        Err(e) => println!("  read failed: {e}"),
    }

    #[cfg(all(unix, not(target_os = "macos")))]
    {
        use arboard::{GetExtLinux, LinuxClipboardKind, SetExtLinux};

        println!("\n=== PRIMARY (what middle-click pastes) ===");
        const PROBE: &str = "zesterm-primary-probe";
        match clipboard.set().clipboard(LinuxClipboardKind::Primary).text(PROBE) {
            Ok(()) => println!("  write: ok"),
            Err(e) => {
                println!("  write: UNSUPPORTED ({e})");
                println!(
                    "\n  Middle-click will fall back to CLIPBOARD on this session, and\n  \
                     selecting text will not publish a selection other programs can read."
                );
                return;
            }
        }
        match clipboard.get().clipboard(LinuxClipboardKind::Primary).text() {
            Ok(t) if t == PROBE => println!("  read back: ok"),
            Ok(t) => println!("  read back: MISMATCH {:?}", truncate(&t)),
            Err(e) => println!("  read failed: {e}"),
        }

        // The point of the whole design: writing the selection must not touch
        // what somebody deliberately copied.
        match clipboard.get_text() {
            Ok(t) if t == PROBE => println!("  CLIPBOARD: CLOBBERED -- the two selections are not separate here"),
            Ok(t) => println!("  CLIPBOARD after: {:?} (untouched, as it must be)", truncate(&t)),
            Err(e) => println!("  CLIPBOARD read failed: {e}"),
        }

        println!(
            "\nNote: on Wayland a selection is owned by the live process, so this probe's\n\
             PRIMARY disappears when it exits. zesterm holds its own for as long as the\n\
             window is open, which is what makes the selection readable elsewhere."
        );
    }
}

fn truncate(s: &str) -> String {
    let cut = s.char_indices().nth(48).map_or(s.len(), |(i, _)| i);
    if cut < s.len() { format!("{}…", &s[..cut]) } else { s.to_string() }
}
