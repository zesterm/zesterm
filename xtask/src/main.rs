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
        forbidden: &[
            &["wgpu", "winit", "windows", "windows-sys", "tokio", "raw-window-handle"],
            TLS_AND_HTTP,
        ],
        args: &[],
    },
    Boundary {
        krate: "zest-theme",
        forbidden: &[&["wgpu", "winit", "windows", "windows-sys", "tokio"], TLS_AND_HTTP],
        args: &[],
    },
    Boundary {
        krate: "zest-font",
        forbidden: &[&["wgpu", "winit", "tokio"], TLS_AND_HTTP],
        args: &[],
    },
    // Encoding a keystroke needs to know what key was pressed, so `winit` is
    // allowed and a translation layer would serve nobody. Owning a pty or a
    // renderer is not: input turns events into bytes and hands them on.
    Boundary {
        krate: "zest-input",
        forbidden: &[&["wgpu", "tokio", "zest-pty", "zest-render-wgpu"], TLS_AND_HTTP],
        args: &[],
    },
    Boundary { krate: "zest-render-wgpu", forbidden: &[&["winit"]], args: &[] },
    // Settings cross to the web and phone clients as data, so the types and the
    // schema must build without touching a filesystem. Checked with default
    // features off, because with them on the crate legitimately watches files --
    // and a rule that had to allow `windows-sys` would stop meaning anything.
    Boundary {
        krate: "zest-config",
        forbidden: &[
            &["wgpu", "winit", "windows", "windows-sys", "tokio", "notify", "directories"],
            TLS_AND_HTTP,
        ],
        args: &["--no-default-features"],
    },
    // The wire types are read by the daemon, the desktop app acting as a
    // client, and -- through generated bindings -- the browser and the phone.
    // A renderer or a runtime in here would make the contract carry an
    // implementation, which is how a protocol stops being a protocol.
    Boundary {
        krate: "zest-proto",
        forbidden: &[
            &["wgpu", "winit", "windows", "windows-sys", "tokio", "zest-pty"],
            TLS_AND_HTTP,
        ],
        args: &[],
    },
    // Discovery and transport selection decide *how* to reach a host, never
    // what a session is. `zest-core` is reachable from here through the wire
    // types and that is fine; owning a pty or a window is not, because routing
    // that can start a shell has stopped being routing.
    Boundary {
        krate: "zest-mesh",
        forbidden: &[&["wgpu", "winit", "zest-pty", "zest-app", "zest-render-wgpu"], TLS_AND_HTTP],
        args: &[],
    },
    // The TLS and HTTP owner is a transport, and a transport that can reach a
    // pty or a window has stopped being one -- same rule as `zest-mesh`, for the
    // same reason. It is deliberately not forbidden `tokio`: whether the dialler
    // is blocking or async is an open question in ADR-009's implementation, and
    // a boundary that pre-decides it would be a design choice wearing a check's
    // clothing.
    Boundary {
        krate: "zest-cloud",
        forbidden: &[&["wgpu", "winit", "zest-pty", "zest-app", "zest-render-wgpu"]],
        args: &[],
    },
];

/// TLS and HTTP, in the spellings the ecosystem actually offers, forbidden
/// everywhere except `zest-cloud`.
///
/// The point is **not** "keep TLS out of the app" — that would be false, and
/// believing it is the way this rule gets misread. `zest-daemon` will depend on
/// `zest-cloud`, and `zest-app` already depends on `zest-daemon`, so rustls
/// reaches the desktop binary by design; a rule naming either would be red the
/// day the relay dialler lands.
///
/// It buys two things instead. rustls and an HTTP client get exactly **one**
/// owner, so a second cannot creep in beside it — two TLS stacks in one binary
/// is a cost paid quietly and noticed by nobody. And the crates whose smallness
/// is a documented property, the ones that cross to wasm and to the browser and
/// phone clients, stay small.
///
/// `check_deps` matches on the crate-name field only and is a pure deny-list:
/// there is no "allowed only here" form, and `zest-cloud` needs none — being
/// absent from every list is what permits it.
const TLS_AND_HTTP: &[&str] = &[
    "rustls",
    "rustls-platform-verifier",
    "webpki-roots",
    "ureq",
    "reqwest",
    "hyper",
    "native-tls",
    "openssl",
    "openssl-sys",
];

/// A crate, the dependencies it must not have, and the feature set to check.
///
/// `forbidden` is a list of *groups* purely so shared sets like [`TLS_AND_HTTP`]
/// appear once by name rather than transcribed into eight lists that then drift.
struct Boundary {
    krate: &'static str,
    forbidden: &'static [&'static [&'static str]],
    args: &'static [&'static str],
}

/// Where the generated JSON Schema is committed.
///
/// Committed rather than generated on demand so editors can pick it up through
/// taplo without a build step, and so a change to the settings shows up as a
/// reviewable diff.
const SCHEMA_PATH: &str = "schemas/zesterm.schema.json";

/// Where the generated TypeScript bindings are committed.
///
/// Committed for the same reason the schema is: the web and phone clients
/// decode against these, and a change to the wire that silently regenerates
/// them is a change nobody reviews. Protocol 2 moved every id from a byte array
/// to a hex string — exactly the kind of change that must show up as a diff.
const BINDINGS_DIR: &str = "crates/zest-proto/bindings";

/// Where the conformance corpus is committed as fixtures.
///
/// The bindings say what the wire *looks* like; these say what it *means*. A
/// client that decodes into the right shapes and applies them wrongly passes
/// every binding check and fails here, which is the whole point of replaying
/// real sessions rather than asserting on types.
///
/// Generated by `cargo run -p zest-proto --example fixture_dump`.
const FIXTURES_DIR: &str = "crates/zest-proto/fixtures";

/// What the web client is handed, generated rather than transcribed.
///
/// `zest_config::schema` states the contract — *"the schema is what the web and
/// phone settings UIs are generated from"* — and `zest_config::ui` lives
/// outside the `fs` feature so the same walk can reach a browser. Until this
/// existed neither actually did: no TypeScript read the schema, and the theme
/// records were hand-copied hex whose own doc comment named this xtask as the
/// fix. A copy nothing checks is a copy that drifts.
const WEB_SETTINGS_DIR: &str = "clients/web/packages/settings/generated";
/// The themes land in `src/` rather than a `generated/` directory because they
/// are TypeScript, not data: emitting `Theme` records means `tsc` fails when
/// the Rust grows a field the hand-written type does not have, which is the
/// drift this is here to catch. JSON would only fail at runtime, if ever.
const WEB_THEME_FILE: &str = "clients/web/packages/theme/src/builtin.generated.ts";

fn main() -> ExitCode {
    let cmd = std::env::args().nth(1);
    match cmd.as_deref() {
        Some("check-deps") => check_deps(),
        Some("schema") => write_schema(false),
        Some("check-schema") => write_schema(true),
        Some("check-bindings") => check_bindings(),
        Some("fixtures") => run_fixture_dump(),
        Some("check-fixtures") => check_fixtures(),
        Some("export-web") => export_web(false),
        Some("check-export-web") => export_web(true),
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
         check-schema   fail if {SCHEMA_PATH} is stale\n  \
         check-bindings fail if {BINDINGS_DIR} is stale\n  \
         fixtures       regenerate {FIXTURES_DIR}\n  \
         check-fixtures fail if {FIXTURES_DIR} is stale\n  \
         export-web     regenerate the web client's schema, UI fields and themes\n  \
         check-export-web fail if any of those is stale"
    );
}

/// Every file `export-web` owns, as `(path, contents)`.
///
/// Built in memory so `export-web` and `check-export-web` are the same code
/// with one branch, the way [`write_schema`] already is. The alternative —
/// [`check_generated`]'s regenerate-then-diff — buys nothing here: these are
/// three files at two paths rather than a directory of unknown contents, and
/// generating in-process means the check needs no nested `cargo run`.
fn web_exports() -> Result<Vec<(String, String)>, String> {
    let fields = zest_config::ui::fields();
    let ui_fields = serde_json::to_string_pretty(&fields)
        .expect("UiField is a plain data type and cannot fail to serialize");

    Ok(vec![
        (format!("{WEB_SETTINGS_DIR}/schema.json"), zest_config::schema::json_schema_string()),
        (format!("{WEB_SETTINGS_DIR}/ui-fields.json"), ui_fields),
        (WEB_THEME_FILE.to_string(), theme_module()?),
    ])
}

/// A TypeScript single-quoted string literal, escaped.
///
/// Single quotes rather than `serde_json::to_string`'s double, to match the
/// surrounding source: this file is read in diffs beside hand-written
/// TypeScript, and one generated record in a different quote style reads as a
/// mistake. The escaping is the part that matters — a theme named `Andy's` or
/// a stray backslash would otherwise close the literal early and emit source
/// that fails to parse, a long way from the builtin that caused it.
fn ts_string(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('\'');
    for ch in s.chars() {
        match ch {
            '\'' => out.push_str("\\'"),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            _ => out.push(ch),
        }
    }
    out.push('\'');
    out
}

/// Whether `id` can be written as a bare `export const <id>`.
///
/// Deliberately stricter than TypeScript allows (no `$`, no leading `_`, no
/// non-ASCII): these are theme ids, they are already all lowercase words, and a
/// generator that accepts more than it needs to is a generator that emits
/// something surprising the first time someone tests the boundary.
fn is_export_name(id: &str) -> bool {
    let mut chars = id.chars();
    chars.next().is_some_and(|c| c.is_ascii_alphabetic())
        && chars.all(|c| c.is_ascii_alphanumeric())
}

/// The built-in themes as a TypeScript module.
///
/// Emitted as source rather than JSON so the records are type-checked against
/// the hand-written `Theme` interface. Only the four fields that interface
/// declares are written: `zest-theme`'s `schema`, `ansi`, `terminal` and
/// `effects` are authoring concerns the client derives from `ui` instead, and
/// widening the TypeScript type to accept them would import a shape no screen
/// reads.
fn theme_module() -> Result<String, String> {
    let mut out = String::from(
        "// @generated by `cargo xtask export-web` -- do not edit.\n\
         //\n\
         // The built-in themes, serialized from `crates/zest-theme/src/builtin.rs`.\n\
         // These were hand-copied hex until this file existed; drift meant the native\n\
         // window and the browser disagreed about what `obsidian` looks like, with\n\
         // nothing to catch it. Run the command above after changing a builtin.\n\
         \n\
         import type { Theme } from './tokens.ts';\n",
    );

    for theme in zest_theme::builtin::all() {
        // The id becomes a bare `export const`, so a hyphenated one -- the
        // obvious next builtin is something like `tokyo-night` -- would emit
        // source that does not parse. `tsc` would catch it, but in the web job,
        // as a syntax error in a generated file, with nothing pointing back at
        // `builtin.rs`. Refuse here instead, where the fix is one line away.
        if !is_export_name(&theme.id) {
            return Err(format!(
                "theme id `{}` cannot be a TypeScript export name.\n  \
                 `export-web` emits each builtin as `export const <id>`, so ids are limited to \
                 ASCII letters and digits starting with a letter.\n  \
                 Rename it in crates/zest-theme/src/builtin.rs, or teach export-web an \
                 id-to-export-name mapping.",
                theme.id,
            ));
        }

        let ui = serde_json::to_value(&theme.ui)
            .expect("UiTokens is a flat record of strings and cannot fail to serialize");
        let entries = ui.as_object().expect("UiTokens serializes as a JSON object");

        out.push_str(&format!(
            "\nexport const {}: Theme = {{\n  id: {},\n  name: {},\n  mode: {},\n  ui: {{\n",
            theme.id,
            ts_string(&theme.id),
            ts_string(&theme.name),
            ts_string(
                serde_json::to_value(theme.mode)
                    .ok()
                    .and_then(|m| m.as_str().map(str::to_owned))
                    .expect("ThemeMode serializes as a string")
                    .as_str()
            ),
        ));
        for (key, value) in entries {
            let hex = value.as_str().expect("every UiTokens field is a serialized Rgba8");
            out.push_str(&format!("    {key}: {},\n", ts_string(hex)));
        }
        out.push_str("  },\n};\n");
    }

    let ids: Vec<String> =
        zest_theme::builtin::all().into_iter().map(|t| t.id.clone()).collect();
    out.push_str(&format!(
        "\n/** All built-ins, in builtin.rs's `IDS` order -- the theme picker's order. */\n\
         export const builtinThemes: readonly Theme[] = [{}];\n\
         \n\
         /** The defaults when nothing is configured (builtin.rs's `DEFAULT_DARK`/`_LIGHT`). */\n\
         export const DEFAULT_DARK = {};\n\
         export const DEFAULT_LIGHT = {};\n",
        ids.join(", "),
        ts_string(zest_theme::builtin::DEFAULT_DARK),
        ts_string(zest_theme::builtin::DEFAULT_LIGHT),
    ));
    Ok(out)
}

/// Write the web client's generated files, or fail if any is stale.
fn export_web(check_only: bool) -> ExitCode {
    let exports = match web_exports() {
        Ok(e) => e,
        Err(e) => {
            eprintln!("cannot export to the web client: {e}");
            return ExitCode::FAILURE;
        }
    };
    let mut stale = Vec::new();

    for (path, generated) in exports {
        let path = std::path::Path::new(&path);
        // Trailing-newline and CRLF normalization both happen here rather than
        // at the write, so a Windows checkout with `core.autocrlf` on compares
        // equal instead of failing this gate on every file, every run.
        let want = format!("{}\n", generated.trim_end());

        if check_only {
            // Only a missing file counts as empty. Collapsing every read error
            // into "" would report a permission problem or a bad symlink as
            // "stale", sending someone to run `export-web` -- which fails the
            // same way, for a reason the message never mentioned.
            let have = match std::fs::read_to_string(path) {
                Ok(text) => text,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
                Err(e) => {
                    eprintln!("could not read {}: {e}", path.display());
                    return ExitCode::FAILURE;
                }
            };
            if have.replace("\r\n", "\n") != want {
                stale.push(path.display().to_string());
            }
            continue;
        }

        if let Some(parent) = path.parent() {
            if let Err(e) = std::fs::create_dir_all(parent) {
                eprintln!("could not create {}: {e}", parent.display());
                return ExitCode::FAILURE;
            }
        }
        if let Err(e) = std::fs::write(path, &want) {
            eprintln!("could not write {}: {e}", path.display());
            return ExitCode::FAILURE;
        }
        println!("wrote {}", path.display());
    }

    if !check_only {
        return ExitCode::SUCCESS;
    }
    if stale.is_empty() {
        println!("{WEB_SETTINGS_DIR} and {WEB_THEME_FILE} are up to date");
        return ExitCode::SUCCESS;
    }
    eprintln!("the web client's generated files are stale -- run `cargo xtask export-web`");
    for path in stale {
        eprintln!("  stale: {path}");
    }
    ExitCode::FAILURE
}

/// Regenerate the fixtures, for the same reason `schema` exists beside
/// `check-schema`: the gate tells you it is stale, and this is what fixes it.
fn run_fixture_dump() -> ExitCode {
    for args in FIXTURE_GENERATORS {
        match std::process::Command::new(env!("CARGO")).args(*args).status() {
            Ok(s) if s.success() => {}
            Ok(s) => {
                eprintln!("{} failed ({s})", args.join(" "));
                return ExitCode::FAILURE;
            }
            Err(e) => {
                eprintln!("could not run cargo: {e}");
                return ExitCode::FAILURE;
            }
        }
    }
    ExitCode::SUCCESS
}

/// Everything that writes into [`FIXTURES_DIR`].
///
/// Two, since protocol 3. `handshake_dump` lives in `zest-mesh` because
/// `zest-proto` has no crypto dependency and must not gain one, but it writes
/// here because this is the directory the web tests already read. Adding a
/// third generator means adding it to this list, and forgetting to is exactly
/// the failure `check_generated` guards against by running all of them.
const FIXTURE_GENERATORS: &[&[&str]] = &[
    &["run", "-p", "zest-proto", "--example", "fixture_dump"],
    &["run", "-p", "zest-mesh", "--example", "handshake_dump"],
];

/// Fail if the committed TypeScript bindings do not match the Rust.
///
/// `ts-rs` writes its output as a side effect of running the tests under the
/// `ts` feature, so there is nothing to call directly — the generator has to be
/// run and its output compared.
fn check_bindings() -> ExitCode {
    check_generated(
        BINDINGS_DIR,
        "ts",
        &[&["test", "-p", "zest-proto", "--features", "ts"]],
        "cargo test -p zest-proto --features ts",
    )
}

/// Fail if the committed conformance fixtures do not match the corpus.
///
/// The same mechanism as [`check_bindings`], for the same reason: these are
/// generated files a second implementation is checked against, so a wire change
/// that silently rewrites them is a change nobody reviews.
fn check_fixtures() -> ExitCode {
    check_generated(FIXTURES_DIR, "json", FIXTURE_GENERATORS, "cargo xtask fixtures")
}

/// Regenerate `dir` in place and fail if anything changed.
///
/// Regenerating over the working tree and comparing against what git had, rather
/// than writing to a scratch directory and diffing two trees: it catches the
/// same drift with half the moving parts, because CI checks out clean. The cost
/// is that a *local* failure leaves the regenerated files in place — which is
/// what you wanted anyway, since the fix is to commit them.
fn check_generated(dir: &str, ext: &str, generators: &[&[&str]], fix: &str) -> ExitCode {
    let path = std::path::Path::new(dir);
    let before = read_generated(path, ext);

    // **Every** generator, not just the first. A directory written by two
    // programs and regenerated by one would compare a fresh half against a
    // stale half and report success -- which is worse than no gate, because it
    // reads as "checked".
    for cargo_args in generators {
        let status = std::process::Command::new(env!("CARGO")).args(*cargo_args).status();
        match status {
            Ok(s) if s.success() => {}
            // Named, not just counted: with more than one generator writing
            // this directory, "regenerating failed" leaves whoever is reading
            // CI to guess which one.
            Ok(s) => {
                eprintln!("regenerating {dir} failed ({s}): cargo {}", cargo_args.join(" "));
                return ExitCode::FAILURE;
            }
            Err(e) => {
                eprintln!("could not run `cargo {}`: {e}", cargo_args.join(" "));
                return ExitCode::FAILURE;
            }
        }
    }

    let after = read_generated(path, ext);
    if before == after {
        println!("{dir} is up to date ({} files)", after.len());
        return ExitCode::SUCCESS;
    }

    eprintln!("{dir} is stale -- run `{fix}` and commit the result");
    for (name, generated) in &after {
        match before.get(name) {
            None => eprintln!("  new:     {name}"),
            Some(old) if old != generated => eprintln!("  changed: {name}"),
            Some(_) => {}
        }
    }
    for name in before.keys() {
        if !after.contains_key(name) {
            eprintln!("  removed: {name}");
        }
    }
    ExitCode::FAILURE
}

/// Every `*.<ext>` file in `dir`, keyed by name, with line endings normalized.
fn read_generated(
    dir: &std::path::Path,
    ext: &str,
) -> std::collections::BTreeMap<String, String> {
    let mut out = std::collections::BTreeMap::new();
    let Ok(entries) = std::fs::read_dir(dir) else { return out };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().is_some_and(|e| e == ext) {
            if let (Some(name), Ok(text)) =
                (path.file_name().and_then(|n| n.to_str()), std::fs::read_to_string(&path))
            {
                out.insert(name.to_string(), text.replace("\r\n", "\n"));
            }
        }
    }
    out
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
            if forbidden.iter().any(|group| group.contains(&name)) {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_portable_crates_still_forbid_tls_and_http() {
        // `zest-mesh` stands in for the set: it is the crate most likely to
        // grow a "just one small HTTP call" for the relay, and the whole family
        // is one grouped slice, so losing it here means losing it everywhere.
        //
        // The failure this guards against is a tidy-up, not a bug: the names
        // forbid dependencies nothing in the workspace has yet, so every entry
        // reads as dead weight until the day one of them would have fired.
        let mesh = BOUNDARIES
            .iter()
            .find(|b| b.krate == "zest-mesh")
            .expect("zest-mesh has a boundary and the fleet is what would reach for HTTP");
        for name in TLS_AND_HTTP {
            assert!(
                mesh.forbidden.iter().any(|group| group.contains(name)),
                "zest-mesh no longer forbids `{name}`; TLS and HTTP have exactly one owner",
            );
        }
    }

    #[test]
    fn the_tls_owner_is_not_fenced_out_of_its_own_job() {
        // The deny-list has no "allowed only here" form, so `zest-cloud` is
        // permitted TLS by being absent from every list -- including its own.
        // Adding it there would fence the crate out of the reason it exists,
        // and the check would still pass today, when the crate has no
        // dependencies at all.
        let cloud = BOUNDARIES
            .iter()
            .find(|b| b.krate == "zest-cloud")
            .expect("zest-cloud has a boundary of its own");
        for name in TLS_AND_HTTP {
            assert!(
                !cloud.forbidden.iter().any(|group| group.contains(name)),
                "zest-cloud forbids `{name}`, which is the one crate meant to have it",
            );
        }
    }

    #[test]
    fn a_quote_or_backslash_in_a_theme_name_is_escaped_not_emitted_raw() {
        // The generator writes TypeScript source. An unescaped apostrophe
        // closes the literal early and the file stops parsing -- in the web
        // job, as a syntax error in a generated file, with nothing pointing
        // back at the builtin that caused it.
        assert_eq!(ts_string("Andy's"), r"'Andy\'s'");
        assert_eq!(ts_string(r"back\slash"), r"'back\\slash'");
        assert_eq!(ts_string("two\nlines"), r"'two\nlines'");
        assert_eq!(ts_string("#0b0f1a"), "'#0b0f1a'");
    }

    #[test]
    fn every_shipped_theme_id_can_be_an_export_name() {
        // The real guard: if a builtin is ever added whose id is hyphenated --
        // `tokyo-night` is the obvious next one -- `export const tokyo-night`
        // does not parse, and this fails before anyone sees tsc's version of
        // the complaint.
        for theme in zest_theme::builtin::all() {
            assert!(
                is_export_name(&theme.id),
                "builtin `{}` cannot be written as `export const <id>`",
                theme.id,
            );
        }
    }

    #[test]
    fn export_names_reject_what_would_not_parse() {
        assert!(is_export_name("obsidian"));
        assert!(is_export_name("solarized2"));
        assert!(!is_export_name("tokyo-night"), "a hyphen is a minus in an identifier");
        assert!(!is_export_name(""), "an empty id has no export to name");
        assert!(!is_export_name("2cool"), "an identifier cannot start with a digit");
        assert!(!is_export_name("my theme"), "a space ends the identifier");
    }

    #[test]
    fn the_generated_theme_module_is_syntactically_plausible() {
        // Not a TypeScript parser -- just the shape that would break silently:
        // one `export const` per builtin, the type annotation intact, and no
        // doubled quotes from a format string that already quoted its value.
        let module = theme_module().expect("the shipped builtins all export cleanly");
        for theme in zest_theme::builtin::all() {
            assert!(
                module.contains(&format!("export const {}: Theme = {{", theme.id)),
                "no export for `{}`",
                theme.id,
            );
        }
        assert!(!module.contains("''"), "a doubled quote means a value was quoted twice");
        assert!(module.contains("export const DEFAULT_DARK = 'obsidian';"));
    }
}
