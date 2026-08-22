//! Which branch a directory is on, from the files git itself maintains.
//!
//! A file read, never a subprocess: this runs whenever a session's cwd moves
//! and again whenever HEAD changes, and `git status`-grade honesty at that
//! cadence is how a daemon becomes fan noise. The dirty flag stays `None`
//! here for exactly that reason — `GitContext::dirty` documents the bargain.
//!
//! First written for the status bar (`a9ee89b`), deleted with it, and
//! resurrected daemon-side: the probe was always about *where the shell
//! stands*, which is a fact the daemon owns, not the window.

use std::path::{Path, PathBuf};

use zest_proto::GitContext;

/// What the probe found, plus where it found it — the `head` path is what a
/// watcher must hold to notice the next `git switch`.
pub struct GitProbe {
    pub context: GitContext,
    /// The HEAD file itself, resolved through `gitdir:` indirection.
    pub head: PathBuf,
}

/// The repository containing `dir`, when there is one.
///
/// Walks up looking for `.git` — a directory holding `HEAD`, or (worktrees,
/// submodules) a *file* pointing at the real git dir.
#[must_use]
pub fn probe(dir: &Path) -> Option<GitProbe> {
    let mut cur = Some(dir);
    while let Some(d) = cur {
        let git = d.join(".git");
        let head = git.join("HEAD");
        if let Ok(text) = std::fs::read_to_string(&head) {
            return Some(GitProbe { context: parse_head(&text)?, head });
        }
        if let Ok(text) = std::fs::read_to_string(&git) {
            let target = text.trim().strip_prefix("gitdir: ")?;
            // Relative targets are relative to the directory holding the
            // `.git` file, which is how worktree checkouts spell it.
            let head = d.join(target).join("HEAD");
            let text = std::fs::read_to_string(&head).ok()?;
            return Some(GitProbe { context: parse_head(&text)?, head });
        }
        cur = d.parent();
    }
    None
}

/// The branch (or detached short hash) a HEAD file names.
fn parse_head(text: &str) -> Option<GitContext> {
    let text = text.trim();
    if let Some(reference) = text.strip_prefix("ref: ") {
        let branch = reference.strip_prefix("refs/heads/").unwrap_or(reference).to_string();
        return Some(GitContext { branch, detached: false, dirty: None });
    }
    // Detached: a bare hash. Eight characters names it without noise.
    if text.len() >= 8 && text.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Some(GitContext { branch: text[..8].to_string(), detached: true, dirty: None });
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn head_parsing_names_branches_and_detached_heads() {
        let on_branch = parse_head("ref: refs/heads/feature/x\n").expect("a branch");
        assert_eq!(on_branch.branch, "feature/x", "the refs/heads/ prefix is plumbing, not a name");
        assert!(!on_branch.detached);

        let detached =
            parse_head("0123abcd0123abcd0123abcd0123abcd0123abcd\n").expect("a detached head");
        assert_eq!(detached.branch, "0123abcd", "detached HEAD is a short hash, not a blank");
        assert!(detached.detached);

        assert!(parse_head("something else").is_none(), "garbage is no context at all");
    }

    #[test]
    fn the_walk_finds_this_very_repository() {
        // The one filesystem case needing no fixture: this test runs from
        // inside a git checkout, several directories down from its root.
        let here = Path::new(env!("CARGO_MANIFEST_DIR"));
        let found = probe(here).expect("walking up from crates/zest-daemon finds zesterm's HEAD");
        assert!(
            found.head.ends_with("HEAD"),
            "the watcher needs the HEAD file itself, got {}",
            found.head.display()
        );
    }

    #[test]
    fn a_worktree_gitdir_file_is_followed() {
        // A `.git` *file* names the real git dir; the probe must read HEAD
        // there, and report that path as the one to watch.
        let root = std::env::temp_dir().join(format!("zest-git-probe-{}", std::process::id()));
        let real = root.join("repo-store");
        let tree = root.join("tree").join("deep");
        std::fs::create_dir_all(&real).expect("mkdir");
        std::fs::create_dir_all(&tree).expect("mkdir");
        std::fs::write(real.join("HEAD"), "ref: refs/heads/wt-branch\n").expect("write HEAD");
        std::fs::write(root.join("tree").join(".git"), format!("gitdir: {}\n", real.display()))
            .expect("write .git file");

        let found = probe(&tree).expect("the walk crosses the gitdir indirection");
        assert_eq!(found.context.branch, "wt-branch");
        assert_eq!(found.head, real.join("HEAD"), "watch the resolved HEAD, not the .git file");

        let _ = std::fs::remove_dir_all(&root);
    }
}
