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
mod session;

use winit::event_loop::EventLoop;

use app::{App, Config};
use session::Wakeup;

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
            "-e" | "--command" => {
                if let Some(v) = args.get(i + 1) {
                    config.shell = Some(v.clone());
                }
                i += 2;
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
                     -e <command>      run a command instead of the shell\n\
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
