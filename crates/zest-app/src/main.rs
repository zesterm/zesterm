//! zesterm.

// A terminal emulator must not be a console application: built as WINDOWS_CUI,
// launching from Explorer or a shortcut pops a console window that then sits
// behind the terminal. Release builds are GUI-subsystem and attach to the
// parent console when there is one, so CLI flags and logs still work from a
// shell (see `console`). Debug builds keep the console for a simpler dev loop.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod app;
mod block_actions;
mod block_menu;
mod chrome;
mod cloud;
mod console;
mod fair_mutex;
mod fleet;
mod keymap;
mod launch;
mod launcher;
mod motion;
mod pipeline_cache;
mod platform;
mod profiles_ui;
mod remote;
mod route;
mod session;
mod settings_ui;
mod source;
mod status;
mod tabs;
mod tabs_state;
mod text_field;

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

/// `--screen`'s value, or the message to refuse it with.
fn screen_from(value: Option<&str>) -> Result<app::StartScreen, String> {
    match value.map(str::trim) {
        Some("fleet") => Ok(app::StartScreen::Fleet),
        Some("themes") => Ok(app::StartScreen::Themes),
        Some("settings") => Ok(app::StartScreen::Settings),
        Some("palette") => Ok(app::StartScreen::Palette),
        Some("launcher") => Ok(app::StartScreen::Launcher),
        Some("profiles") => Ok(app::StartScreen::Profiles),
        Some("settings-menu") => Ok(app::StartScreen::SettingsMenu),
        Some("profiles-rename") => Ok(app::StartScreen::ProfilesRename),
        _ => Err(
            "--screen needs one of fleet|themes|settings|settings-menu|palette|launcher|\
             profiles|profiles-rename"
                .into(),
        ),
    }
}

/// `--tabs-position`'s value — exactly the two spellings `tabs.position`
/// deserializes.
///
/// Validated here rather than passed through like `--theme` because the
/// cascade's typed merge fails *wholesale* on a bad enum value: every setting
/// would silently fall back to its default. A theme name that does not exist
/// degrades to the default theme and nothing else; a position that does not
/// exist would take the whole config down with it, so it is refused out loud.
fn tabs_position_from(value: Option<&str>) -> Result<&'static str, &'static str> {
    match value.map(str::trim) {
        Some("top") => Ok("top"),
        Some("left") => Ok("left"),
        _ => Err("--tabs-position needs top or left"),
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

/// Everything the argument loop collected, parsed but not yet acted on.
#[derive(Default)]
struct Flags {
    cli: CliLayer,
    profile: Option<String>,
    startup_probe: bool,
    no_daemon: bool,
    attach_probe: bool,
    new_session: bool,
    attach_addr: Option<String>,
    // Collected separately and assembled by `screenshot_from` afterwards, so
    // the modifiers do not depend on argument order and cannot conjure
    // screenshot mode on their own — `--screenshot-delay 400` alone used to
    // write `zesterm.png`.
    shot_path: Option<std::path::PathBuf>,
    shot_delay: Option<std::time::Duration>,
    shot_size: Option<(f64, f64)>,
    screen: Option<app::StartScreen>,
}

/// The parse ended without producing a run.
enum EarlyExit {
    /// An informational flag (`--help`, `--themes`, ...) printed its answer;
    /// exit cleanly.
    Handled,
    /// A flag was refused; print this to stderr and exit 2.
    Refused(String),
}

/// The value for a flag that requires one. A missing value — or a next token
/// that is itself a flag — is a refusal: silently consuming `--screenshot`
/// as a theme name would mis-parse everything after it, and the error the
/// user then sees names the wrong flag.
fn value_of<'a>(flag: &str, next: Option<&'a String>) -> Result<&'a str, EarlyExit> {
    match next {
        Some(v) if !v.starts_with("--") => Ok(v),
        _ => Err(EarlyExit::Refused(format!("{flag} needs a value"))),
    }
}

/// The argument loop, as a function so tests can drive it.
///
/// Flag *composition* is loop behaviour — `--screen` beside `--screenshot`,
/// in either order — and a loop inlined in `main` leaves exactly that
/// untestable. The informational arms print from here; refusals are returned
/// instead of calling `process::exit`, because a test asserting on a refusal
/// must not take the test runner down with it.
fn parse_args(args: &[String]) -> Result<Flags, EarlyExit> {
    let mut f = Flags::default();
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--theme" => {
                f.cli.set_str("appearance.theme", value_of("--theme", args.get(i + 1))?);
                i += 2;
            }
            "--font" => {
                // Prepended, not substituted. `--font "Some Font"` for a
                // font that turns out not to be installed must still leave
                // a usable terminal rather than an empty stack.
                let v = value_of("--font", args.get(i + 1))?;
                let mut families = vec![v.to_string()];
                families.extend(zest_config::Typography::default().families);
                f.cli.set_array("typography.families", &families);
                i += 2;
            }
            "--size" => {
                let v = value_of("--size", args.get(i + 1))?;
                match v.parse::<f64>() {
                    Ok(pt) => f.cli.set_float("typography.size_pt", pt),
                    Err(_) => {
                        return Err(EarlyExit::Refused("--size needs a number of points".into()));
                    }
                }
                i += 2;
            }
            "--opacity" => {
                let v = value_of("--opacity", args.get(i + 1))?;
                match v.parse::<f64>() {
                    Ok(a) => f.cli.set_float("window.opacity", a),
                    Err(_) => {
                        return Err(EarlyExit::Refused("--opacity needs a number".into()));
                    }
                }
                i += 2;
            }
            "--tabs-position" => {
                match tabs_position_from(args.get(i + 1).map(String::as_str)) {
                    // The same mechanism `--theme` uses: a CommandLine-layer
                    // write, so the override wins the cascade and is
                    // *recorded* as having won.
                    Ok(v) => f.cli.set_str("tabs.position", v),
                    Err(msg) => return Err(EarlyExit::Refused(msg.into())),
                }
                i += 2;
            }
            "--screen" => {
                match screen_from(args.get(i + 1).map(String::as_str)) {
                    Ok(s) => f.screen = Some(s),
                    Err(msg) => return Err(EarlyExit::Refused(msg)),
                }
                i += 2;
            }
            "--profile" => {
                f.profile = Some(value_of("--profile", args.get(i + 1))?.to_string());
                i += 2;
            }
            "--startup-probe" => {
                f.startup_probe = true;
                i += 1;
            }
            "--no-daemon" => {
                f.no_daemon = true;
                i += 1;
            }
            "--attach-probe" => {
                f.attach_probe = true;
                i += 1;
            }
            "--new-session" => {
                f.new_session = true;
                i += 1;
            }
            "--attach" => {
                match args.get(i + 1) {
                    Some(v) => f.attach_addr = Some(v.clone()),
                    None => {
                        return Err(EarlyExit::Refused(
                            "--attach needs <host:port> (see zest-daemon --listen-lan)".into(),
                        ));
                    }
                }
                i += 2;
            }
            "--scroll-on-output" => {
                f.cli.set_bool("scrolling.scroll_on_output", true);
                i += 1;
            }
            "--screenshot" => {
                let Some(v) = args.get(i + 1) else {
                    return Err(EarlyExit::Refused("--screenshot needs a path".into()));
                };
                f.shot_path = Some(v.into());
                i += 2;
            }
            "--screenshot-delay" => {
                let Some(d) = args.get(i + 1).and_then(|s| parse_delay(s)) else {
                    return Err(EarlyExit::Refused(format!(
                        "--screenshot-delay needs milliseconds, at most \
                         {MAX_SCREENSHOT_DELAY_MS}"
                    )));
                };
                f.shot_delay = Some(d);
                i += 2;
            }
            "--screenshot-size" => {
                let Some(size) = args.get(i + 1).and_then(|s| parse_size(s)) else {
                    return Err(EarlyExit::Refused(
                        "--screenshot-size needs <width>x<height> in logical pixels".into(),
                    ));
                };
                f.shot_size = Some(size);
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
                return Err(EarlyExit::Handled);
            }
            "--schema" => {
                println!("{}", zest_config::schema::json_schema_string());
                return Err(EarlyExit::Handled);
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
                    return Err(EarlyExit::Refused("-e needs a command".into()));
                }
                f.cli.set_str("shell.command", &join_command(&rest));
                break;
            }
            "--themes" => {
                for t in zest_theme::builtin::all() {
                    println!("{:<10} {}", t.id, t.name);
                }
                return Err(EarlyExit::Handled);
            }
            // The escape hatch for everything injection cannot reach: ssh,
            // tmux, a container, a shell started inside another shell. Prints
            // the same script the injected shim loads, so the two cannot drift.
            "--shell-integration" => {
                let name = args.get(i + 1).cloned().unwrap_or_default();
                match zest_pty::shell_integration::Shell::detect(&name) {
                    Some(shell) => print!("{}", zest_pty::shell_integration::hook(shell)),
                    None => {
                        return Err(EarlyExit::Refused(format!(
                            "no shell integration for {name:?}.\n\
                             zsh and PowerShell (pwsh, powershell) are supported; \
                             bash and fish are not yet, and cmd.exe has no hook to \
                             offer — see docs/ROADMAP.md, WS-E."
                        )));
                    }
                }
                return Err(EarlyExit::Handled);
            }
            "--help" | "-h" => {
                println!(
                    "zesterm\n\n\
                     --theme <id>      colour theme (see --themes)\n\
                     --font <family>   preferred font family\n\
                     --size <pt>       font size in points\n\
                     --opacity <0..1>  window background opacity\n\
                     --tabs-position <top|left>\n\
                     \x20                 tab strip along the top, or a sidebar on the left\n\
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
                     --screen <name>   open on fleet|themes|settings|settings-menu|\n\
                     \x20                 palette|launcher|profiles|profiles-rename\n\
                     \x20                 instead of the terminal\n\
                     \x20                 ('palette' is the ⌘K search, not the keymap's\n\
                     \x20                 command palette; 'launcher' is the + menu over the\n\
                     \x20                 default screen; 'settings-menu' is Settings with\n\
                     \x20                 the theme dropdown open, and 'profiles-rename' is\n\
                     \x20                 Profiles with the name entry open — both states a\n\
                     \x20                 screenshot cannot otherwise reach, since opening\n\
                     \x20                 them takes a click).\n\
                     \x20                 Composes with\n\
                     \x20                 --screenshot; screen content is live state, and\n\
                     \x20                 --screenshot already implies --no-daemon, which\n\
                     \x20                 keeps captures stable\n\
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
                return Err(EarlyExit::Handled);
            }
            other => {
                return Err(EarlyExit::Refused(format!("unknown argument: {other}\ntry --help")));
            }
        }
    }
    Ok(f)
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
    let args: Vec<String> = std::env::args().skip(1).collect();
    let Flags {
        cli,
        profile,
        startup_probe,
        no_daemon,
        attach_probe,
        new_session,
        attach_addr,
        shot_path,
        shot_delay,
        shot_size,
        screen,
    } = match parse_args(&args) {
        Ok(flags) => flags,
        Err(EarlyExit::Handled) => return std::process::ExitCode::SUCCESS,
        Err(EarlyExit::Refused(msg)) => {
            eprintln!("{msg}");
            return std::process::ExitCode::from(2);
        }
    };

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
    if let Some(screen) = screen {
        app = app.with_start_screen(screen);
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
    use super::{
        app, join_command, parse_args, parse_delay, parse_size, screen_from, screenshot_from,
        tabs_position_from, CliLayer, EarlyExit,
    };
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
    fn every_shipped_screen_parses() {
        use app::StartScreen as S;
        // The six arms below are the six surfaces that exist today; a value
        // that stops parsing is a screenshot pipeline that silently stops
        // covering that screen.
        assert_eq!(screen_from(Some("fleet")), Ok(S::Fleet));
        assert_eq!(screen_from(Some("themes")), Ok(S::Themes));
        assert_eq!(screen_from(Some("settings")), Ok(S::Settings));
        assert_eq!(screen_from(Some("palette")), Ok(S::Palette));
        assert_eq!(screen_from(Some("launcher")), Ok(S::Launcher));
        assert_eq!(screen_from(Some("profiles")), Ok(S::Profiles));
    }

    #[test]
    fn a_misspelled_screen_is_refused_with_the_valid_values() {
        let unknown = screen_from(Some("flet")).unwrap_err();
        assert!(unknown.contains("needs one of"), "a typo lists the valid values: {unknown}");
        let missing = screen_from(None).unwrap_err();
        assert!(missing.contains("needs one of"), "a bare --screen lists them too: {missing}");
    }

    #[test]
    fn tabs_position_takes_exactly_what_the_setting_does() {
        // "top" and "left" are the two spellings `tabs.position` deserializes.
        // Anything else written into the CommandLine layer would fail the
        // cascade's typed merge *wholesale* -- every setting silently back to
        // its default -- so junk is refused here, out loud, with exit 2.
        assert_eq!(tabs_position_from(Some("top")), Ok("top"));
        assert_eq!(tabs_position_from(Some("left")), Ok("left"));
        assert!(tabs_position_from(Some("right")).is_err());
        assert!(tabs_position_from(Some("Top")).is_err(), "the setting is case-sensitive, so the flag is too");
        assert!(tabs_position_from(None).is_err(), "a bare --tabs-position is refused, not defaulted");
    }

    #[test]
    fn the_tabs_position_override_wins_the_cascade_and_is_recorded() {
        // The flag writes the CommandLine layer -- the same mechanism --theme
        // uses -- rather than mutating resolved settings: a config-file save
        // re-runs the cascade and replays the flags, so an override living
        // anywhere else would vanish on the user's first edit.
        let mut cli = CliLayer::default();
        cli.set_str("tabs.position", tabs_position_from(Some("left")).expect("valid"));
        let user = zest_config::Layer::parse(zest_config::Source::User, "[tabs]\nposition = \"top\"\n")
            .expect("well-formed user layer");
        let flags = zest_config::Layer::new(zest_config::Source::CommandLine, cli.into_table());
        let r = zest_config::cascade::resolve(&[user, flags]);
        assert_eq!(
            r.settings.tabs.position,
            zest_config::settings::TabsPosition::Left,
            "the command line must beat the user's file"
        );
        assert_eq!(
            r.provenance.get("tabs.position"),
            Some(&zest_config::Source::CommandLine),
            "provenance is what lets the settings UI say 'set by command line'"
        );
    }

    #[test]
    fn a_screen_and_a_screenshot_compose_in_either_order() {
        // Each flag is collected into its own slot and assembled after the
        // loop, so order cannot matter -- but "cannot" is exactly the kind of
        // claim that stops being true when an arm learns to consume an extra
        // argument, so both orders are pinned.
        for order in [
            v(&["--screen", "fleet", "--screenshot", "out.png"]),
            v(&["--screenshot", "out.png", "--screen", "fleet"]),
            v(&["--tabs-position", "left", "--screenshot", "out.png", "--screen", "fleet"]),
        ] {
            let f = parse_args(&order).unwrap_or_else(|_| panic!("valid together: {order:?}"));
            assert_eq!(f.screen, Some(app::StartScreen::Fleet));
            assert_eq!(f.shot_path.as_deref(), Some(std::path::Path::new("out.png")));
        }
    }

    #[test]
    fn a_bad_screen_or_position_is_a_refusal_not_a_run() {
        // Exit 2, exactly like the sibling flags -- a window that opens on
        // the wrong screen is a worse answer to a typo than an error is.
        for bad in [
            v(&["--screen", "bogus"]),
            v(&["--screen"]),
            v(&["--tabs-position", "middle"]),
            v(&["--tabs-position"]),
        ] {
            assert!(
                matches!(parse_args(&bad), Err(EarlyExit::Refused(_))),
                "{bad:?} must be refused"
            );
        }
    }

    #[test]
    fn a_flag_never_eats_the_flag_after_it() {
        // `--theme --screenshot out.png` once set the theme to "--screenshot"
        // and then choked on `out.png` — an error naming the wrong flag,
        // after mis-parsing everything behind it. A value-taking flag whose
        // next token is another flag (or missing) refuses instead.
        for bad in [
            v(&["--theme", "--screenshot", "out.png"]),
            v(&["--theme"]),
            v(&["--font", "--theme", "paper"]),
            v(&["--size", "--opacity", "0.9"]),
            v(&["--size", "abc"]),
            v(&["--opacity", "much"]),
            v(&["--profile", "--no-daemon"]),
        ] {
            assert!(
                matches!(parse_args(&bad), Err(EarlyExit::Refused(_))),
                "{bad:?} must be refused, not silently mis-parsed"
            );
        }
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
