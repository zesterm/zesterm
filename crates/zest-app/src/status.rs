//! Small facts the chrome states, gathered honestly.
//!
//! Everything here is cheap, local, and synchronous, and runs per chrome
//! rebuild (which is event-driven, not per frame). Remote facts —
//! reachability, latency — come from the fleet model instead; this module
//! never opens a socket. (The git-branch probe that used to live here left
//! with the status bar it fed — design §1 deleted the bar, and the prompt
//! line already says the branch.)

/// `/Users/andy/dev/zesterm` → `~/dev/zesterm`, for paths on *this* machine.
/// Remote paths pass through untouched — another machine's home is unknowable
/// from here, and a wrong `~` is worse than a long path.
#[must_use]
pub fn shorten_home(path: &str) -> String {
    if let Some(home) = dirs_home() {
        if let Some(rest) = path.strip_prefix(&home) {
            if rest.is_empty() {
                return "~".to_string();
            }
            if rest.starts_with('/') || rest.starts_with('\\') {
                return format!("~{rest}");
            }
        }
    }
    path.to_string()
}

/// "now", "2m", "12h", "31d" — the sidebar's age column. Coarse on purpose:
/// an age that ticks by the second would redraw a resting sidebar.
#[must_use]
pub fn age_label(elapsed: std::time::Duration) -> String {
    let secs = elapsed.as_secs();
    if secs < 60 {
        "now".to_string()
    } else if secs < 3600 {
        format!("{}m", secs / 60)
    } else if secs < 86_400 {
        format!("{}h", secs / 3600)
    } else {
        format!("{}d", secs / 86_400)
    }
}

/// "now", "2m ago", "12h ago", "yesterday", "3d ago" — the palette's
/// provenance (#527). [`age_label`]'s coarseness with the word the design
/// mock uses for one day: a history row reads as prose where the sidebar's
/// column reads as a number, and "1d ago" is a number pretending to be one.
#[must_use]
pub fn age_words(elapsed: std::time::Duration) -> String {
    let secs = elapsed.as_secs();
    if secs < 60 {
        "now".to_string()
    } else if secs < 86_400 {
        format!("{} ago", age_label(elapsed))
    } else if secs < 2 * 86_400 {
        "yesterday".to_string()
    } else {
        format!("{}d ago", secs / 86_400)
    }
}

fn dirs_home() -> Option<String> {
    std::env::var("HOME").ok().or_else(|| std::env::var("USERPROFILE").ok()).filter(|h| !h.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The mock says `yesterday`, and the day after must not: "2d ago" is
    /// the same coarseness the sidebar already promises.
    #[test]
    fn one_day_is_yesterday_and_two_is_2d_ago() {
        let d = std::time::Duration::from_secs;
        assert_eq!(age_words(d(5)), "now");
        assert_eq!(age_words(d(120)), "2m ago");
        assert_eq!(age_words(d(3 * 3600)), "3h ago");
        assert_eq!(age_words(d(86_400)), "yesterday");
        assert_eq!(age_words(d(2 * 86_400 - 1)), "yesterday");
        assert_eq!(age_words(d(2 * 86_400)), "2d ago");
    }

    #[test]
    fn home_shortening_never_bites_mid_component() {
        let home = dirs_home().unwrap_or_default();
        if home.is_empty() {
            return;
        }
        assert_eq!(shorten_home(&home), "~");
        assert_eq!(shorten_home(&format!("{home}/dev")), "~/dev");
        // `/Users/andysmith` must NOT become `~smith` for home `/Users/andy`.
        let sibling = format!("{home}extra/dev");
        assert_eq!(shorten_home(&sibling), sibling);
    }
}

/// A byte count as a person reads one — "812 B", "4.2 KB", "1.4 MB" (#464).
///
/// Powers of 1024 with the short names, which is what every file manager on
/// every platform shows; the pedantically-correct KiB is not what the person
/// comparing this against their editor's status bar will see.
#[must_use]
pub fn human_bytes(n: u64) -> String {
    const UNITS: [&str; 4] = ["B", "KB", "MB", "GB"];
    let mut v = n as f64;
    let mut unit = 0;
    while v >= 1024.0 && unit + 1 < UNITS.len() {
        v /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{n} B")
    } else if v < 10.0 {
        format!("{v:.1} {}", UNITS[unit])
    } else {
        format!("{v:.0} {}", UNITS[unit])
    }
}

#[cfg(test)]
mod human_bytes_tests {
    use super::human_bytes;

    #[test]
    fn a_size_reads_the_way_a_file_manager_says_it() {
        assert_eq!(human_bytes(0), "0 B");
        assert_eq!(human_bytes(812), "812 B");
        // One decimal below ten, none above: "1.4 MB" is useful, "1.4 KB" of a
        // 1434-byte file is too, and "687 KB" does not need a fraction.
        assert_eq!(human_bytes(1434), "1.4 KB");
        assert_eq!(human_bytes(703_594), "687 KB");
        assert_eq!(human_bytes(1_468_006), "1.4 MB");
        // The last unit does not roll over into one that has no name here.
        assert!(human_bytes(u64::MAX).ends_with(" GB"));
    }
}
