//! zesterm.

// A terminal emulator must not be a console application: built as WINDOWS_CUI,
// launching from Explorer or a shortcut pops a console window that then sits
// behind the terminal. Release builds are GUI-subsystem and attach to the
// parent console when there is one, so CLI flags and logs still work from a
// shell (see `console`). Debug builds keep the console for a simpler dev loop.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod app;
mod console;
mod fair_mutex;
mod input;
mod mouse;
mod pipeline_cache;
mod platform;
mod session;

use winit::event_loop::EventLoop;

use app::{App, Config};
use session::Wakeup;

/// Rebuild a command line from separate arguments.
///
/// `CreateProcessW` takes one string and re-splits it, so any argument that
/// contained a space has to be re-quoted or it silently becomes two arguments —
/// which is exactly how `-e code --wait "my file.txt"` ends up opening two
/// files.
fn join_command(parts: &[String]) -> String {
    parts
        .iter()
        .map(|p| {
            if p.contains(' ') && !p.starts_with('"') {
                format!("\"{}\"", p.replace('"', "\\\""))
            } else {
                p.clone()
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

fn main() {
    console::attach_to_parent();

    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_env("ZESTERM_LOG")
                .unwrap_or_else(|_| "info".into()),
        )
        .init();

    let mut config = Config::default();
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--theme" => {
                if let Some(v) = args.get(i + 1) {
                    config.theme = v.clone();
                }
                i += 2;
            }
            "--font" => {
                if let Some(v) = args.get(i + 1) {
                    config.font_families.insert(0, v.clone());
                }
                i += 2;
            }
            "--size" => {
                if let Some(v) = args.get(i + 1).and_then(|s| s.parse().ok()) {
                    config.typography.size_pt = v;
                }
                i += 2;
            }
            "--opacity" => {
                if let Some(v) = args.get(i + 1).and_then(|s| s.parse().ok()) {
                    config.opacity = v;
                }
                i += 2;
            }
            // Everything after -e is the command, as xterm and alacritty do.
            //
            // Requiring a single pre-quoted string instead looks equivalent and
            // is not: shells and process launchers routinely split an argument
            // list without re-quoting, so `-e "pwsh -NoLogo"` arrives as two
            // separate arguments and the terminal rejects the second one.
            "-e" | "--command" => {
                let rest: Vec<String> = args[i + 1..].to_vec();
                if rest.is_empty() {
                    eprintln!("-e needs a command");
                    std::process::exit(2);
                }
                config.shell = Some(join_command(&rest));
                break;
            }
            "--themes" => {
                for t in zest_theme::builtin::all() {
                    println!("{:<10} {}", t.id, t.name);
                }
                return;
            }
            "--help" | "-h" => {
                println!(
                    "zesterm\n\n\
                     --theme <id>      colour theme (see --themes)\n\
                     --font <family>   preferred font family\n\
                     --size <pt>       font size in points\n\
                     --opacity <0..1>  window background opacity\n\
                     -e <command>...   run a command instead of the shell\n\
                     \x20                 (must come last; takes all remaining args)\n\
                     --themes          list built-in themes"
                );
                return;
            }
            other => {
                eprintln!("unknown argument: {other}\ntry --help");
                std::process::exit(2);
            }
        }
    }

    let event_loop = EventLoop::<Wakeup>::with_user_event()
        .build()
        .expect("create event loop");
    let proxy = event_loop.create_proxy();

    let mut app = App::new(config, proxy);
    event_loop.run_app(&mut app).expect("run");
}

#[cfg(test)]
mod tests {
    use super::join_command;

    fn v(parts: &[&str]) -> Vec<String> {
        parts.iter().map(|s| (*s).to_string()).collect()
    }

    #[test]
    fn plain_args_join_with_spaces() {
        assert_eq!(join_command(&v(&["pwsh", "-NoLogo", "-NoExit"])), "pwsh -NoLogo -NoExit");
    }

    #[test]
    fn args_with_spaces_are_requoted() {
        // Without this the path becomes two arguments and the child opens the
        // wrong file -- or, more often, nothing at all.
        assert_eq!(
            join_command(&v(&["code", "C:\\My Docs\\a.txt"])),
            "code \"C:\\My Docs\\a.txt\""
        );
    }

    #[test]
    fn already_quoted_args_are_left_alone() {
        assert_eq!(join_command(&v(&["sh", "\"a b\""])), "sh \"a b\"");
    }

    #[test]
    fn embedded_quotes_are_escaped() {
        assert_eq!(join_command(&v(&["say", "he said hi"])), "say \"he said hi\"");
        assert_eq!(join_command(&v(&["say", "a \"b\" c"])), "say \"a \\\"b\\\" c\"");
    }
}
