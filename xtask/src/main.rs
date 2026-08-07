//! Repo automation. Run via `cargo xtask <command>`.

use std::process::{Command, ExitCode};

/// Crates that must stay free of presentation and platform dependencies, and
/// the dependency names that would violate that.
///
/// This is the single most important invariant in the workspace. `zest-core`
/// being UI-free is what lets the daemon, the browser client, and the mobile
/// client share one terminal implementation instead of three that drift.
/// A boundary that isn't checked by CI decays within a month, so it is checked.
const BOUNDARIES: &[(&str, &[&str])] = &[
    ("zest-core", &["wgpu", "winit", "windows", "windows-sys", "tokio", "raw-window-handle"]),
    ("zest-theme", &["wgpu", "winit", "windows", "windows-sys", "tokio"]),
    ("zest-font", &["wgpu", "winit", "tokio"]),
    ("zest-render-wgpu", &["winit"]),
];

fn main() -> ExitCode {
    let cmd = std::env::args().nth(1);
    match cmd.as_deref() {
        Some("check-deps") => check_deps(),
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
    eprintln!("usage: cargo xtask <command>\n\ncommands:\n  check-deps   verify crate boundary invariants");
}

fn check_deps() -> ExitCode {
    let mut violations = Vec::new();

    for (krate, forbidden) in BOUNDARIES {
        let out = match Command::new(env!("CARGO"))
            .args(["tree", "--package", krate, "--edges", "normal", "--prefix", "none"])
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
