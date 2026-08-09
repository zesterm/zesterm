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
mod daemon_client;
mod fair_mutex;
// The picker (next commit of #23) is the consumer of the snapshot half.
#[allow(dead_code, reason = "the picker consumes the snapshot one commit later")]
mod fleet;
mod pipeline_cache;
mod platform;
mod remote;
mod session;
mod source;
mod tabs;

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

fn main() {
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
            "--config" => {
                match zest_config::paths::config_file() {
                    Some(p) => println!("{}", p.display()),
                    None => match zest_config::paths::config_dir() {
                        Some(d) => println!("{} (does not exist yet)", d.join("config.toml").display()),
                        None => println!("no config directory available"),
                    },
                }
                return;
            }
            "--schema" => {
                println!("{}", zest_config::schema::json_schema_string());
                return;
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
                return;
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
                return;
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
                     --new-session     start a fresh shell, do not pick up an idle one\n\
                     --attach <host:port>\n\
                     \x20                 another machine's daemon; its shell in this window\n\
                     \x20                 (the host approves this device on first contact)\n\
                     --schema          print the settings JSON Schema\n\n\
                     Flags are the strongest layer of the settings cascade;\n\
                     everything else lives in the config file."
                );
                return;
            }
            other => {
                eprintln!("unknown argument: {other}\ntry --help");
                std::process::exit(2);
            }
        }
    }

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

    let mut app = App::new(load.resolved.settings, cli_table, profile_name, proxy);
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
    if let Some(addr) = attach_addr {
        // Contradiction, not precedence: one flag says "no daemon anywhere",
        // the other names one to attach to. Guessing which the user meant
        // produces a window whose shell is on the wrong machine.
        if no_daemon {
            eprintln!("--attach and --no-daemon contradict each other");
            std::process::exit(2);
        }
        app = app.with_attach_addr(addr);
    }
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
