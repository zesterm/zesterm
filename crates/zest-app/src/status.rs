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

fn dirs_home() -> Option<String> {
    std::env::var("HOME").ok().or_else(|| std::env::var("USERPROFILE").ok()).filter(|h| !h.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

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
