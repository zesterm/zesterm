//! What the status bar says, gathered honestly.
//!
//! Everything here is cheap, local, and synchronous: a couple of small file
//! reads per chrome rebuild (which is event-driven, not per frame). Remote
//! facts — reachability, latency — come from the fleet model instead; this
//! module never opens a socket.

use std::path::Path;

/// The current branch of the repository containing `dir`, when there is one.
///
/// Reads `.git/HEAD` walking up from `dir` — a file read, not a subprocess,
/// because this runs on every chrome rebuild. Deliberately *not* reported: the
/// dirty `*` the design shows. Answering "is the tree dirty" honestly means
/// `git status`, and a subprocess per rebuild is how a status bar becomes fan
/// noise. A detached HEAD shows as the short hash.
#[must_use]
pub fn git_branch(dir: &Path) -> Option<String> {
    let mut cur = Some(dir);
    while let Some(d) = cur {
        let git = d.join(".git");
        if let Ok(text) = std::fs::read_to_string(git.join("HEAD")) {
            return branch_from_head(&text);
        }
        // A `.git` *file* (worktrees, submodules) points at the real git dir.
        if let Ok(text) = std::fs::read_to_string(&git) {
            let target = text.trim().strip_prefix("gitdir: ")?;
            let head = std::fs::read_to_string(d.join(target).join("HEAD")).ok()?;
            return branch_from_head(&head);
        }
        cur = d.parent();
    }
    None
}

/// The branch (or detached short hash) a HEAD file names.
fn branch_from_head(text: &str) -> Option<String> {
    let text = text.trim();
    if let Some(reference) = text.strip_prefix("ref: ") {
        return Some(reference.strip_prefix("refs/heads/").unwrap_or(reference).to_string());
    }
    // Detached: a bare hash. Eight characters names it without noise.
    if text.len() >= 8 && text.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Some(text[..8].to_string());
    }
    None
}

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

fn dirs_home() -> Option<String> {
    std::env::var("HOME").ok().or_else(|| std::env::var("USERPROFILE").ok()).filter(|h| !h.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn head_parsing_names_branches_and_detached_heads() {
        assert_eq!(
            branch_from_head("ref: refs/heads/feature/x\n").as_deref(),
            Some("feature/x"),
            "the refs/heads/ prefix is plumbing, not a name"
        );
        assert_eq!(
            branch_from_head("0123abcd0123abcd0123abcd0123abcd0123abcd\n").as_deref(),
            Some("0123abcd"),
            "detached HEAD is a short hash, not a blank segment"
        );
        assert_eq!(branch_from_head("something else"), None, "garbage is no segment at all");
    }

    #[test]
    fn the_walk_finds_this_very_repository() {
        // The one filesystem case that needs no fixture: this test runs from
        // inside a git checkout, several directories down from its root.
        let here = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
        assert!(
            git_branch(here).is_some(),
            "walking up from crates/zest-app must find zesterm's own HEAD"
        );
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
