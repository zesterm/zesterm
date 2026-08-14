//! What this machine can offer a client: what it *is*, and what it can launch.
//!
//! The daemon's half of #262. A client that sets `Hello.watch_hosts` gets a
//! [`HostOffer`] on its first `Sessions` reply and another whenever this
//! machine's config reloads, which is what lets a `+` launcher three time zones
//! away list this machine's own profiles, and a fleet card fill its `os` row
//! with a fact instead of a dash.
//!
//! Split the way the rest of the daemon splits: [`profiles_of`] and
//! [`facts`] are pure over their inputs and carry the tests, while
//! [`OfferSource`] owns the reading and the generation counter that the serve
//! loop diffs against — the same shape `Registry::generation` already uses for
//! the session listing, so a burst of edits coalesces into one push rather
//! than one per event.
//!
//! **This is why `zest-daemon` depends on `zest-config`.** It did not before,
//! and that was deliberate: `DaemonConfig::shell_integration`'s doc comment
//! records the trade as "neither is worth doing before someone needs the
//! switch". Someone does now — a machine that cannot read its own profiles
//! cannot publish them, and the alternative is a client guessing at a config
//! file on a filesystem it has never seen. `cargo xtask check-deps` guards
//! `zest-core`, not this crate, so the wasm fence is untouched.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use zest_config::profiles;
use zest_config::Settings;
use zest_proto::{HostOffer, HostProfile};

/// This machine's launch targets, resolved through its own `profiles.defaults`.
///
/// Resolved here rather than shipped raw, because the client has no cascade to
/// run: it does not hold this machine's config, and a half-specified profile
/// that inherits its command from a `defaults` table the client cannot see
/// would arrive as a row promising nothing.
///
/// `defaults` itself is never listed — `list_profiles` filters it, and
/// launching the layer every profile falls through to is meaningless.
#[must_use]
pub fn profiles_of(settings: &Settings) -> Vec<HostProfile> {
    let root = profiles::root_of(settings);
    profiles::list_profiles(&root)
        .into_iter()
        .map(|name| {
            let meta = profiles::resolve_profile(&root, &name).meta;
            HostProfile {
                name,
                // Empty means "this host's default shell", the same convention
                // `CreateSession.command` uses — so a launch passes it straight
                // through rather than having to know what a blank means twice.
                command: meta.command.unwrap_or_default(),
                starting_directory: meta.starting_directory.unwrap_or_default(),
                icon: meta.icon.unwrap_or_default(),
                color_scheme: meta.color_scheme.unwrap_or_default(),
                tab_color: meta.tab_color,
            }
        })
        .collect()
}

/// The machine's own facts, everything but the profiles.
///
/// `os_version` is best effort and empty when unknown, never a placeholder:
/// design §7's cards show only what is actually known, and a dash pretending
/// to be a fact is exactly what that rule exists to prevent.
#[must_use]
pub fn facts(default_shell: String) -> HostOffer {
    HostOffer {
        os: std::env::consts::OS.to_string(),
        arch: std::env::consts::ARCH.to_string(),
        os_version: os_version(),
        default_shell,
        profiles: Vec::new(),
    }
}

/// The OS version as the machine names itself, for the fleet card's row.
///
/// **The whole `uname -sr` string, not just the release**, because that row is
/// the only place the *kernel's* name appears: `HostOffer::os` is
/// `std::env::consts::OS`, which says `macos` where design §7's card says
/// `Darwin 24.5.0`. A bare `24.5.0` beside `macos` is a worse row than either,
/// and nothing else on the wire carries "Darwin".
///
/// One process spawn per *reload*, not per connection — it lives behind
/// [`OfferSource`]'s cached snapshot. Empty when the platform has no answer
/// this crate can get without a dependency; the row is then simply not drawn.
fn os_version() -> String {
    #[cfg(unix)]
    {
        // Exactly design §7's examples: "Darwin 24.5.0",
        // "Linux 6.8.0-31-generic".
        std::process::Command::new("uname")
            .arg("-sr")
            .output()
            .ok()
            .filter(|o| o.status.success())
            .and_then(|o| String::from_utf8(o.stdout).ok())
            .map(|s| s.trim().to_string())
            .unwrap_or_default()
    }
    #[cfg(windows)]
    {
        // `OsVersion` off `cmd /c ver` would spawn a shell to read a string
        // the API has; neither is worth a dependency for one decorative row,
        // and an honest empty is better than a wrong number. The `os` and
        // `arch` rows still say "windows" / "x86_64".
        String::new()
    }
    #[cfg(not(any(unix, windows)))]
    {
        String::new()
    }
}

/// The offer this machine publishes, and a generation that moves when it
/// changes.
///
/// Cheap to clone (one `Arc`), because it rides on `DaemonConfig` and every
/// connection holds a copy. The generation is what the serve loop diffs
/// against, exactly as it does for `Registry::generation`: a connection that
/// has already sent the current generation sends nothing, so a chatty session
/// listing does not drag the profile list along behind it.
#[derive(Debug, Clone)]
pub struct OfferSource {
    inner: Arc<Inner>,
}

#[derive(Debug)]
struct Inner {
    offer: Mutex<HostOffer>,
    generation: AtomicU64,
}

impl OfferSource {
    /// Build one from an offer already assembled — the seam the tests use, and
    /// what keeps this type free of the filesystem.
    #[must_use]
    pub fn new(offer: HostOffer) -> Self {
        Self {
            inner: Arc::new(Inner {
                offer: Mutex::new(offer),
                generation: AtomicU64::new(1),
            }),
        }
    }

    /// Read this machine's config and build the offer from it.
    ///
    /// Never fails: `zest_config::load` reports a broken layer and skips it
    /// rather than refusing, so a half-typed config in an editor with autosave
    /// costs this machine its profile rows for a keystroke, and never its
    /// daemon.
    #[must_use]
    pub fn from_config(default_shell: String) -> Self {
        Self::new(read_config(default_shell))
    }

    #[must_use]
    pub fn generation(&self) -> u64 {
        self.inner.generation.load(Ordering::Acquire)
    }

    #[must_use]
    pub fn snapshot(&self) -> HostOffer {
        self.inner.offer.lock().expect("offer mutex").clone()
    }

    /// Replace the offer, bumping the generation **only if it differs**.
    ///
    /// The equality check is the whole point rather than an optimisation: a
    /// file watcher fires several times for one save on every platform, and
    /// without it each of those would push an identical profile list to every
    /// attached client on the fleet.
    pub fn set(&self, offer: HostOffer) -> bool {
        let mut held = self.inner.offer.lock().expect("offer mutex");
        if *held == offer {
            return false;
        }
        *held = offer;
        self.inner.generation.fetch_add(1, Ordering::AcqRel);
        true
    }

    /// Re-read the config and publish what changed.
    pub fn reload(&self, default_shell: String) -> bool {
        self.set(read_config(default_shell))
    }
}

fn read_config(default_shell: String) -> HostOffer {
    let load = zest_config::load(&zest_config::Options::default());
    for e in &load.errors {
        tracing::warn!(error = %e, "config layer skipped while building this host's offer");
    }
    let mut offer = facts(default_shell);
    offer.profiles = profiles_of(&load.resolved.settings);
    offer
}

#[cfg(test)]
mod tests {
    use super::*;

    fn settings_with(entries: &[(&str, &str)]) -> Settings {
        let mut s = Settings::default();
        for (name, toml) in entries {
            s.profiles.insert((*name).to_string(), toml.parse().expect("valid toml"));
        }
        s
    }

    #[test]
    fn a_published_profile_carries_what_defaults_gave_it() {
        // The client has no cascade to run — it does not hold this machine's
        // config — so a profile that inherits its command from `defaults` must
        // arrive with that command already folded in, or the launcher row it
        // becomes promises nothing.
        let settings = settings_with(&[
            ("defaults", "command = \"zsh -l\"\nicon = \"star\""),
            ("bare", ""),
            ("own", "command = \"wsl.exe -d Ubuntu\"\ntab_color = 3"),
        ]);
        let published = profiles_of(&settings);

        let by = |name: &str| {
            published.iter().find(|p| p.name == name).unwrap_or_else(|| panic!("{name} published"))
        };
        assert_eq!(by("bare").command, "zsh -l", "Defaults' command travels with the profile");
        assert_eq!(by("bare").icon, "star", "and so does its icon");
        assert_eq!(by("own").command, "wsl.exe -d Ubuntu", "its own command outranks Defaults'");
        assert_eq!(by("own").tab_color, Some(3));
    }

    #[test]
    fn the_reserved_defaults_table_is_never_published() {
        // It is the layer every profile falls through to, not a launch target:
        // a client offering it as a row would offer a shell nobody described.
        let settings = settings_with(&[("defaults", "command = \"zsh\""), ("real", "")]);
        let published = profiles_of(&settings);
        let names: Vec<&str> = published.iter().map(|p| p.name.as_str()).collect();
        assert_eq!(names, ["real"]);
    }

    #[test]
    fn a_profile_with_no_command_anywhere_publishes_an_empty_one() {
        // Empty is the wire's word for "this host's default shell", the same
        // convention `CreateSession.command` uses — so the launch passes it
        // straight through instead of a client having to guess what a blank
        // means in one place and not the other. Emphatically *not* the
        // client's own shell, which is how a Mac ends up asked to run pwsh.
        let published = profiles_of(&settings_with(&[("plain", "")]));
        assert_eq!(published[0].command, "");
        assert_eq!(published[0].starting_directory, "");
    }

    #[test]
    fn a_config_with_no_profiles_offers_none_rather_than_a_placeholder() {
        assert!(profiles_of(&Settings::default()).is_empty());
    }

    #[test]
    fn the_generation_moves_on_a_real_change_and_not_on_a_repeat() {
        // A file watcher fires several times for one save on every platform.
        // Without the equality check each of those pushes an identical profile
        // list to every attached client on the fleet.
        let source = OfferSource::new(facts("zsh".into()));
        let first = source.generation();

        assert!(!source.set(facts("zsh".into())), "an identical offer is not a change");
        assert_eq!(source.generation(), first, "and the generation must not move for it");

        assert!(source.set(facts("pwsh -NoLogo".into())), "a different offer is");
        assert!(source.generation() > first, "and it moves the generation");
        assert_eq!(source.snapshot().default_shell, "pwsh -NoLogo");
    }

    #[test]
    fn the_facts_name_this_machine_and_never_invent_a_version() {
        let offer = facts("zsh -l".into());
        assert_eq!(offer.os, std::env::consts::OS);
        assert_eq!(offer.arch, std::env::consts::ARCH);
        assert_eq!(offer.default_shell, "zsh -l");
        // Best effort, and the rule everywhere is the same: a real answer or
        // an empty string, never a placeholder that renders as a fact.
        assert!(
            offer.os_version.is_empty() || !offer.os_version.trim().is_empty(),
            "an unknown version is empty, never whitespace or a dash"
        );
    }

    #[cfg(unix)]
    #[test]
    fn the_version_row_carries_the_kernel_name_as_well_as_the_release() {
        // Asserted rather than commented, because the comment and the code
        // drifted once already: `uname -r` alone gives `24.5.0`, and this row
        // is the *only* place the kernel's name reaches a client —
        // `HostOffer::os` is `std::env::consts::OS`, which says `macos` where
        // design §7's card says `Darwin 24.5.0`.
        let version = facts(String::new()).os_version;
        assert!(!version.is_empty(), "unix has `uname`");
        let kernel = std::process::Command::new("uname")
            .arg("-s")
            .output()
            .expect("uname -s");
        let kernel = String::from_utf8(kernel.stdout).expect("utf8");
        let kernel = kernel.trim();
        assert!(
            version.starts_with(kernel),
            "the row must lead with the kernel name ({kernel}), got {version:?}"
        );
        assert!(
            version.len() > kernel.len(),
            "and carry the release after it, got {version:?}"
        );
    }
}
