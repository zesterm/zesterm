//! Repo automation. Run via `cargo xtask <command>`.

use std::process::{Command, ExitCode};

/// Crates that must stay free of presentation and platform dependencies, and
/// the dependency names that would violate that.
///
/// This is the single most important invariant in the workspace. `zest-core`
/// being UI-free is what lets the daemon, the browser client, and the mobile
/// client share one terminal implementation instead of three that drift.
/// A boundary that isn't checked by CI decays within a month, so it is checked.
const BOUNDARIES: &[Boundary] = &[
    Boundary {
        krate: "zest-core",
        forbidden: &["wgpu", "winit", "windows", "windows-sys", "tokio", "raw-window-handle"],
        args: &[],
    },
    Boundary {
        krate: "zest-theme",
        forbidden: &["wgpu", "winit", "windows", "windows-sys", "tokio"],
        args: &[],
    },
    Boundary { krate: "zest-font", forbidden: &["wgpu", "winit", "tokio"], args: &[] },
    Boundary { krate: "zest-render-wgpu", forbidden: &["winit"], args: &[] },
    // Settings cross to the web and phone clients as data, so the types and the
    // schema must build without touching a filesystem. Checked with default
    // features off, because with them on the crate legitimately watches files --
    // and a rule that had to allow `windows-sys` would stop meaning anything.
    Boundary {
        krate: "zest-config",
        forbidden: &["wgpu", "winit", "windows", "windows-sys", "tokio", "notify", "directories"],
        args: &["--no-default-features"],
    },
    // The wire types are read by the daemon, the desktop app acting as a
    // client, and -- through generated bindings -- the browser and the phone.
    // A renderer or a runtime in here would make the contract carry an
    // implementation, which is how a protocol stops being a protocol.
    Boundary {
        krate: "zest-proto",
        forbidden: &["wgpu", "winit", "windows", "windows-sys", "tokio", "zest-pty"],
        args: &[],
    },
    // Discovery and transport selection decide *how* to reach a host, never
    // what a session is. `zest-core` is reachable from here through the wire
    // types and that is fine; owning a pty or a window is not, because routing
    // that can start a shell has stopped being routing.
    Boundary {
        krate: "zest-mesh",
        forbidden: &["wgpu", "winit", "zest-pty", "zest-app", "zest-render-wgpu"],
        args: &[],
    },
];

/// A crate, the dependencies it must not have, and the feature set to check.
struct Boundary {
    krate: &'static str,
    forbidden: &'static [&'static str],
    args: &'static [&'static str],
}

/// Where the generated JSON Schema is committed.
///
/// Committed rather than generated on demand so editors can pick it up through
/// taplo without a build step, and so a change to the settings shows up as a
/// reviewable diff.
const SCHEMA_PATH: &str = "schemas/zesterm.schema.json";

fn main() -> ExitCode {
    let cmd = std::env::args().nth(1);
    match cmd.as_deref() {
        Some("check-deps") => check_deps(),
        Some("schema") => write_schema(false),
        Some("check-schema") => write_schema(true),
        Some(other) => {
            eprintln!("unknown command: {other}");
            usage();
            ExitCode::FAILURE
        }
        None => {
            usage();
            ExitCode::FAILURE
        }
    }
}

fn usage() {
    eprintln!(
        "usage: cargo xtask <command>\n\ncommands:\n  \
         check-deps     verify crate boundary invariants\n  \
         schema         regenerate {SCHEMA_PATH}\n  \
         check-schema   fail if {SCHEMA_PATH} is stale"
    );
}

/// Write the settings JSON Schema, or check that the committed one matches.
///
/// `check-schema` is what keeps the file from drifting: adding a setting without
/// regenerating leaves the web and phone settings UIs a version behind, and
/// nothing else would notice.
fn write_schema(check_only: bool) -> ExitCode {
    let generated = zest_config::schema::json_schema_string();
    let path = std::path::Path::new(SCHEMA_PATH);

    if check_only {
        let committed = std::fs::read_to_string(path).unwrap_or_default();
        // Compared after normalizing line endings: a checkout with
        // `core.autocrlf` on would otherwise fail every time on Windows.
        if committed.replace("\r\n", "\n").trim() == generated.replace("\r\n", "\n").trim() {
            println!("{SCHEMA_PATH} is up to date");
            return ExitCode::SUCCESS;
        }
        eprintln!("{SCHEMA_PATH} is stale -- run `cargo xtask schema`");
        return ExitCode::FAILURE;
    }

    if let Some(parent) = path.parent() {
        if let Err(e) = std::fs::create_dir_all(parent) {
            eprintln!("could not create {}: {e}", parent.display());
            return ExitCode::FAILURE;
        }
    }
    match std::fs::write(path, format!("{generated}\n")) {
        Ok(()) => {
            println!("wrote {SCHEMA_PATH}");
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("could not write {SCHEMA_PATH}: {e}");
            ExitCode::FAILURE
        }
    }
}

fn check_deps() -> ExitCode {
    let mut violations = Vec::new();

    for Boundary { krate, forbidden, args } in BOUNDARIES {
        let out = match Command::new(env!("CARGO"))
            .args(["tree", "--package", krate, "--edges", "normal", "--prefix", "none"])
            .args(*args)
            .output()
        {
            Ok(o) if o.status.success() => o.stdout,
            Ok(o) => {
                eprintln!("cargo tree failed for {krate}:\n{}", String::from_utf8_lossy(&o.stderr));
                return ExitCode::FAILURE;
            }
            Err(e) => {
                eprintln!("could not run cargo tree: {e}");
                return ExitCode::FAILURE;
            }
        };
        let tree = String::from_utf8_lossy(&out);

        // `cargo tree --prefix none` emits one "name version [(path)]" per line.
        // Match on the name field only, so a crate named e.g. "winit-helper"
        // doesn't trip the "winit" rule.
        for line in tree.lines() {
            let Some(name) = line.split_whitespace().next() else { continue };
            if name == *krate {
                continue;
            }
            if forbidden.contains(&name) {
                violations.push(format!("{krate} depends on {name}"));
            }
        }
    }

    if violations.is_empty() {
        println!("check-deps: all {} boundaries hold", BOUNDARIES.len());
        ExitCode::SUCCESS
    } else {
        eprintln!("check-deps: {} boundary violation(s)", violations.len());
        for v in &violations {
            eprintln!("  - {v}");
        }
        eprintln!(
            "\nThese boundaries exist so zest-core can be shared by the native app, the\n\
             daemon, and the wasm clients. If a dependency genuinely belongs, move the\n\
             code to a crate above the boundary rather than relaxing the rule."
        );
        ExitCode::FAILURE
    }
}
