//! zesterm.

// A terminal emulator must not be a console application: built as WINDOWS_CUI,
// launching from Explorer or a shortcut pops a console window that then sits
// behind the terminal. Release builds are GUI-subsystem and attach to the
// parent console when there is one, so CLI flags and logs still work from a
// shell (see `console`). Debug builds keep the console for a simpler dev loop.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod app;
mod block_actions;
mod chrome;
mod console;
mod daemon;
mod fair_mutex;
mod fleet;
mod keymap;
mod pipeline_cache;
mod platform;
mod remote;
mod session;
mod settings_ui;
mod source;
mod status;
mod tabs;
mod tabs_state;

use winit::event_loop::EventLoop;

use app::App;
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

/// The longest `--screenshot-delay` worth honouring — five minutes.
///
/// Not arbitrary caution: the delay becomes `Instant::now() + delay`, and
/// `u64::MAX` milliseconds is a deadline half a billion years out. The process
/// then sits there forever having shown no window, captured nothing and exited
/// with nothing — which is a worse answer to a typo than an error is. (On some
/// platforms that addition panics instead; neither is a good outcome.) Anything
/// under the cap cannot overflow.
const MAX_SCREENSHOT_DELAY_MS: u64 = 5 * 60 * 1000;

/// Milliseconds for `--screenshot-delay`, rejected if past the cap.
///
/// Rejected rather than clamped, for the same reason as `parse_size`: silently
/// doing something other than what was asked is worse than saying no.
fn parse_delay(s: &str) -> Option<std::time::Duration> {
    match s.trim().parse::<u64>().ok()? {
        ms if ms <= MAX_SCREENSHOT_DELAY_MS => Some(std::time::Duration::from_millis(ms)),
        _ => None,
    }
}

/// `<width>x<height>` in logical pixels, as `--screenshot-size` takes it.
///
/// Rejects zero and negatives rather than clamping them: a window of no size
/// produces a valid, empty PNG, and a silently-corrected typo is a screenshot
/// of something other than what was asked for.
fn parse_size(s: &str) -> Option<(f64, f64)> {
    let (w, h) = s.split_once(['x', 'X'])?;
    let (w, h) = (w.trim().parse::<f64>().ok()?, h.trim().parse::<f64>().ok()?);
    (w >= 1.0 && h >= 1.0 && w.is_finite() && h.is_finite()).then_some((w, h))
}

/// Assemble the screenshot flags, or say why they do not make sense together.
///
/// `--screenshot <path>` is the opt-in; the other two only modify it. Without
/// this check they each defaulted the whole struct into existence, so
/// `zesterm --screenshot-delay 400` entered screenshot mode and wrote
/// `zesterm.png` into the working directory — a flag that silently did
/// something entirely different from what it says, and wrote a file to do it.
fn screenshot_from(
    path: Option<std::path::PathBuf>,
    delay: Option<std::time::Duration>,
    size: Option<(f64, f64)>,
) -> Result<Option<app::Screenshot>, &'static str> {
    match path {
        Some(path) => {
            let d = app::Screenshot::default();
            Ok(Some(app::Screenshot {
                path,
                delay: delay.unwrap_or(d.delay),
                size: size.unwrap_or(d.size),
            }))
        }
        None if delay.is_some() || size.is_some() => Err(
            "--screenshot-delay and --screenshot-size only mean something \
             alongside --screenshot <path>",
        ),
        None => Ok(None),
    }
}

/// Command-line flags, collected as a settings layer.
///
/// Built as a `toml::Table` rather than by mutating the resolved config, so a
/// flag participates in the cascade like any other layer — it wins, and it is
/// *recorded* as having won. Without that, the settings UI would show a value it
/// could not explain and the config file would appear to be ignored.
#[derive(Default)]
struct CliLayer {
    table: toml::Table,
}

impl CliLayer {
    fn set(&mut self, key: &str, value: toml::Value) {
        // Keys are dotted; the layer is nested, so walk and create as needed.
        let mut parts = key.split('.').peekable();
        let mut node = &mut self.table;
        while let Some(part) = parts.next() {
            if parts.peek().is_none() {
                node.insert(part.to_string(), value);
                return;
            }
            let entry = node
                .entry(part.to_string())
                .or_insert_with(|| toml::Value::Table(toml::Table::new()));
            match entry.as_table_mut() {
                Some(t) => node = t,
                None => return,
            }
        }
    }

    fn set_str(&mut self, key: &str, value: &str) {
        self.set(key, toml::Value::String(value.to_string()));
    }

    fn set_float(&mut self, key: &str, value: f64) {
        self.set(key, toml::Value::Float(value));
    }

    fn set_bool(&mut self, key: &str, value: bool) {
        self.set(key, toml::Value::Boolean(value));
    }

    fn set_array(&mut self, key: &str, values: &[String]) {
        let items = values.iter().cloned().map(toml::Value::String).collect();
        self.set(key, toml::Value::Array(items));
    }

    fn into_table(self) -> toml::Table {
        self.table
    }
}

fn main() -> std::process::ExitCode {
    console::attach_to_parent();

    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_env("ZESTERM_LOG")
                .unwrap_or_else(|_| "info".into()),
        )
        .init();

    // Flags become the strongest layer of the settings cascade rather than
    // mutating a config directly. That is what lets `--size 20` show up in the
    // settings UI as "set by command line" instead of as an unexplained value
    // the config file disagrees with.
    let mut cli = CliLayer::default();
    let mut profile = None;
    let mut startup_probe = false;
    let mut no_daemon = false;
    let mut attach_probe = false;
    let mut new_session = false;
    let mut attach_addr: Option<String> = None;
    // Collected separately and assembled after the loop, so the modifiers do
    // not depend on argument order and cannot conjure screenshot mode on their
    // own — `--screenshot-delay 400` alone used to write `zesterm.png`.
    let mut shot_path: Option<std::path::PathBuf> = None;
    let mut shot_delay: Option<std::time::Duration> = None;
    let mut shot_size: Option<(f64, f64)> = None;
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--theme" => {
                if let Some(v) = args.get(i + 1) {
                    cli.set_str("appearance.theme", v);
                }
                i += 2;
            }
            "--font" => {
                if let Some(v) = args.get(i + 1) {
                    // Prepended, not substituted. `--font "Some Font"` for a
                    // font that turns out not to be installed must still leave
                    // a usable terminal rather than an empty stack.
                    let mut families = vec![v.clone()];
                    families.extend(zest_config::Typography::default().families);
                    cli.set_array("typography.families", &families);
                }
                i += 2;
            }
            "--size" => {
                if let Some(v) = args.get(i + 1).and_then(|s| s.parse::<f64>().ok()) {
                    cli.set_float("typography.size_pt", v);
                }
                i += 2;
            }
            "--opacity" => {
                if let Some(v) = args.get(i + 1).and_then(|s| s.parse::<f64>().ok()) {
                    cli.set_float("window.opacity", v);
                }
                i += 2;
            }
            "--profile" => {
                profile = args.get(i + 1).cloned();
                i += 2;
            }
            "--startup-probe" => {
                startup_probe = true;
                i += 1;
            }
            "--no-daemon" => {
                no_daemon = true;
                i += 1;
            }
            "--attach-probe" => {
                attach_probe = true;
                i += 1;
            }
            "--new-session" => {
                new_session = true;
                i += 1;
            }
            "--attach" => {
                attach_addr = args.get(i + 1).cloned();
                if attach_addr.is_none() {
                    eprintln!("--attach needs <host:port> (see zest-daemon --listen-lan)");
                    std::process::exit(2);
                }
                i += 2;
            }
            "--scroll-on-output" => {
                cli.set_bool("scrolling.scroll_on_output", true);
                i += 1;
            }
            "--screenshot" => {
                let Some(v) = args.get(i + 1) else {
                    eprintln!("--screenshot needs a path");
                    std::process::exit(2);
                };
                shot_path = Some(v.into());
                i += 2;
            }
            "--screenshot-delay" => {
                let Some(d) = args.get(i + 1).and_then(|s| parse_delay(s)) else {
                    eprintln!(
                        "--screenshot-delay needs milliseconds, at most \
                         {MAX_SCREENSHOT_DELAY_MS}"
                    );
                    std::process::exit(2);
                };
                shot_delay = Some(d);
                i += 2;
            }
            "--screenshot-size" => {
                let Some(size) = args.get(i + 1).and_then(|s| parse_size(s)) else {
                    eprintln!("--screenshot-size needs <width>x<height> in logical pixels");
                    std::process::exit(2);
                };
                shot_size = Some(size);
                i += 2;
            }
            "--config" => {
                match zest_config::paths::config_file() {
                    Some(p) => println!("{}", p.display()),
                    None => match zest_config::paths::config_dir() {
                        Some(d) => println!("{} (does not exist yet)", d.join("config.toml").display()),
                        None => println!("no config directory available"),
                    },
                }
                return std::process::ExitCode::SUCCESS;
            }
            "--schema" => {
                println!("{}", zest_config::schema::json_schema_string());
                return std::process::ExitCode::SUCCESS;
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
                cli.set_str("shell.command", &join_command(&rest));
                break;
            }
            "--themes" => {
                for t in zest_theme::builtin::all() {
                    println!("{:<10} {}", t.id, t.name);
                }
                return std::process::ExitCode::SUCCESS;
            }
            // The escape hatch for everything injection cannot reach: ssh,
            // tmux, a container, a shell started inside another shell. Prints
            // the same script the injected shim loads, so the two cannot drift.
            "--shell-integration" => {
                let name = args.get(i + 1).cloned().unwrap_or_default();
                match zest_pty::shell_integration::Shell::detect(&name) {
                    Some(shell) => print!("{}", zest_pty::shell_integration::hook(shell)),
                    None => {
                        eprintln!(
                            "no shell integration for {name:?}.\n\
                             zsh is supported; bash, fish and PowerShell are not yet — \
                             see docs/ROADMAP.md, WS-E."
                        );
                        std::process::exit(2);
                    }
                }
                return std::process::ExitCode::SUCCESS;
            }
            "--help" | "-h" => {
                println!(
                    "zesterm\n\n\
                     --theme <id>      colour theme (see --themes)\n\
                     --font <family>   preferred font family\n\
                     --size <pt>       font size in points\n\
                     --opacity <0..1>  window background opacity\n\
                     --profile <name>  apply a named profile from the config\n\
                     --scroll-on-output\n\
                     \x20                 jump to the bottom on new output\n\
                     -e <command>...   run a command instead of the shell\n\
                     \x20                 (must come last; takes all remaining args)\n\
                     --themes          list built-in themes\n\
                     --shell-integration <shell>\n\
                     \x20                 print the command-block hook, to eval by hand\n\
                     \x20                 (ssh, tmux and subshells; injection covers the rest)\n\
                     --config          print the config file path\n\
                     --startup-probe   report time to first paint, then exit\n\
                     --attach-probe    report what attaching to the daemon cost, then exit\n\
                     --no-daemon       own the pty in this process, do not attach\n\
                     --new-session     start a fresh shell instead of restoring your tabs\n\
                     --screenshot <path>\n\
                     \x20                 render one frame to a PNG and exit, without ever\n\
                     \x20                 showing the window (no screen-capture permission)\n\
                     --screenshot-delay <ms>\n\
                     \x20                 let the shell settle first (default 400)\n\
                     --screenshot-size <WxH>\n\
                     \x20                 window size in logical pixels (default 960x600)\n\
                     --attach <host:port>\n\
                     \x20                 another machine's daemon; its shell in this window\n\
                     \x20                 (the host approves this device on first contact)\n\
                     --schema          print the settings JSON Schema\n\n\
                     Flags are the strongest layer of the settings cascade;\n\
                     everything else lives in the config file."
                );
                return std::process::ExitCode::SUCCESS;
            }
            other => {
                eprintln!("unknown argument: {other}\ntry --help");
                std::process::exit(2);
            }
        }
    }

    // Validated here, before anything is built: a contradiction between flags
    // is knowable from the arguments alone, and finding it out after a config
    // load, an event loop and an `App` have been constructed means unwinding
    // all of it to say something that was true before any of it started.
    let shot = match screenshot_from(shot_path, shot_delay, shot_size) {
        Ok(shot) => shot,
        Err(msg) => {
            eprintln!("{msg}");
            return std::process::ExitCode::from(2);
        }
    };

    // Kept, not consumed: a config file save re-runs the cascade, and the flags
    // have to be replayed on top or `--size 20` would vanish the first time the
    // user edits anything.
    let cli_table = cli.into_table();
    let profile_name = profile;
    let load = zest_config::load(&zest_config::Options {
        profile: profile_name.clone(),
        workspace_dir: std::env::current_dir().ok(),
        cli: Some(cli_table.clone()),
        system_light: false,
    });

    // Reported, never fatal. A terminal that refuses to start because of a typo
    // has locked the user out of the editor they would fix it with.
    for e in &load.errors {
        tracing::error!(error = %e, "config problem; that layer was skipped");
    }
    if let Some(m) = &load.migration {
        tracing::info!(from = m.from, to = m.to, "config migrated");
    }
    for (key, source) in &load.resolved.provenance {
        tracing::debug!(key = %key, source = %source, "setting");
    }

    let event_loop = EventLoop::<Wakeup>::with_user_event()
        .build()
        .expect("create event loop");
    let proxy = event_loop.create_proxy();

    let mut app = App::new(load.resolved, cli_table, profile_name, proxy);
    if startup_probe {
        app = app.with_startup_probe();
    }
    if no_daemon {
        app = app.with_no_daemon();
    }
    if attach_probe {
        app = app.with_attach_probe();
    }
    if new_session {
        app = app.with_new_session();
    }
    if let Some(shot) = shot {
        // In-process by default, and not as a shortcut: on macOS the daemon
        // blocks on a Keychain prompt after every rebuild and the app falls
        // back silently after 2s (see "Traps already paid for"). A screenshot
        // that sometimes waits two seconds and sometimes photographs a
        // half-attached session is not a measurement of anything. `--attach`
        // still wins, for the case where the remote session *is* the subject.
        if attach_addr.is_none() {
            app = app.with_no_daemon();
        }
        app = app.with_screenshot(shot);
    }
    if let Some(addr) = attach_addr {
        // Contradiction, not precedence: one flag says "no daemon anywhere",
        // the other names one to attach to. Guessing which the user meant
        // produces a window whose shell is on the wrong machine.
        if no_daemon {
            eprintln!("--attach and --no-daemon contradict each other");
            return std::process::ExitCode::from(2);
        }
        app = app.with_attach_addr(addr);
    }
    event_loop.run_app(&mut app).expect("run");

    // The screenshot's exit code, carried out of the event loop rather than
    // taken by `process::exit` from inside it: the pty, the clipboard and the
    // saved tab state all want their `Drop`, and `main` returning is the only
    // way they get it. A screenshot that silently did not happen is exactly
    // the failure a caller needs to see, so it must survive the trip.
    let code = app.exit_code();
    drop(app);
    std::process::ExitCode::from(code)
}

#[cfg(test)]
mod tests {
    use super::{app, join_command, parse_delay, parse_size, screenshot_from};
    use std::time::Duration;

    #[test]
    fn the_modifiers_cannot_turn_screenshot_mode_on_by_themselves() {
        // `--screenshot-delay 400` used to default the whole struct into
        // existence, so a flag that says "wait a bit" instead started
        // screenshot mode and wrote `zesterm.png` into the working directory.
        assert!(screenshot_from(None, Some(Duration::from_millis(400)), None).is_err());
        assert!(screenshot_from(None, None, Some((800.0, 600.0))).is_err());
        assert!(
            matches!(screenshot_from(None, None, None), Ok(None)),
            "no screenshot flags at all is not an error, it is an ordinary run"
        );
    }

    #[test]
    fn an_absurd_screenshot_delay_is_refused_rather_than_waited_out() {
        // `u64::MAX` parses fine as milliseconds and becomes a deadline half a
        // billion years out, so the process showed no window, captured nothing
        // and never exited -- measured, not theorised: it sat there for the
        // full three minutes it was given before being killed. The cap is what
        // makes a typo an error instead of a hang.
        assert_eq!(parse_delay(&u64::MAX.to_string()), None);
        assert_eq!(parse_delay(&(super::MAX_SCREENSHOT_DELAY_MS + 1).to_string()), None);
        assert_eq!(
            parse_delay(&super::MAX_SCREENSHOT_DELAY_MS.to_string()),
            Some(Duration::from_millis(super::MAX_SCREENSHOT_DELAY_MS)),
            "the cap itself is allowed; it is a ceiling, not a wall just below one"
        );
        assert_eq!(parse_delay("400"), Some(Duration::from_millis(400)));
        assert_eq!(parse_delay("not-a-number"), None);
    }

    #[test]
    fn the_modifiers_apply_whichever_order_they_arrive_in() {
        // They are collected and assembled after the parse loop precisely so
        // `--screenshot-size 800x600 --screenshot out.png` works as well as the
        // other order -- argument order is not something a caller should have
        // to know.
        let shot = screenshot_from(Some("out.png".into()), None, Some((800.0, 600.0)))
            .expect("valid")
            .expect("screenshot mode");
        assert_eq!(shot.size, (800.0, 600.0));
        assert_eq!(shot.delay, app::Screenshot::default().delay, "unset keeps the default");
    }

    fn v(parts: &[&str]) -> Vec<String> {
        parts.iter().map(|s| (*s).to_string()).collect()
    }

    #[test]
    fn screenshot_sizes_parse_both_spellings() {
        assert_eq!(parse_size("1200x800"), Some((1200.0, 800.0)));
        assert_eq!(parse_size("1200X800"), Some((1200.0, 800.0)), "capital X too");
        assert_eq!(parse_size(" 640 x 480 "), Some((640.0, 480.0)), "spaces are forgiven");
    }

    #[test]
    fn a_degenerate_screenshot_size_is_refused_not_clamped() {
        // Clamping would hand back a PNG of *something*, and a screenshot of
        // something other than what was asked for is worse than an error.
        assert_eq!(parse_size("0x600"), None);
        assert_eq!(parse_size("-100x600"), None);
        assert_eq!(parse_size("1200"), None, "no separator at all");
        assert_eq!(parse_size("widexhigh"), None);
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
