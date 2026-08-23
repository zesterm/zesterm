//! Running `git` without letting it hang the daemon.
//!
//! Two callers want this, and they wanted it differently until #453: the
//! context engine's dirty probe (`context::git_dirty`) and the review panel's
//! diff. The skeleton is the interesting part and is easy to get subtly wrong,
//! so it lives here once —
//!
//! * a **drain thread**, because a child that fills its stdout pipe blocks
//!   forever while the parent waits for it to exit, and the parent waits for it
//!   to exit because it is not reading — the classic deadlock, and it needs a
//!   repository big enough to notice;
//! * an output **cap**, so a pathological diff cannot be read into memory
//!   without bound;
//! * a **deadline**, because `git` on a cold network filesystem is not fast and
//!   a daemon that stops answering is worse than one that says it does not
//!   know;
//! * a **reap on every early exit past the spawn**, or the failure path leaks a
//!   subprocess at exactly the moment the machine is under the pressure that
//!   caused the failure.
//!
//! What is deliberately *not* here: a git library. Reading `.git/HEAD` is a
//! file read (`context::git`), and everything else is one bounded subprocess.
//! `git2`/`gix` would be a large dependency, a second implementation of what
//! the user's own git already does, and a source of answers that disagree with
//! the `git` on their `PATH` — worktrees, `includeIf`, and hook-driven state
//! all being places it could.

use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// How long any one `git` invocation gets.
///
/// A diff is allowed longer than the dirty probe's 1.5s: it is asked for by a
/// person opening a panel, once, rather than by a listing that runs whenever a
/// session is enumerated.
pub const DIFF_DEADLINE: Duration = Duration::from_secs(5);

/// What a `git` run produced, when it produced anything.
pub struct GitRun {
    pub out: Vec<u8>,
    /// The child exited 0. A `git diff` in a directory that is not a
    /// repository exits non-zero with an empty stdout, which is a *different*
    /// answer from an empty diff, and the two must not collapse.
    pub ok: bool,
    /// The output reached `cap`; there was more.
    pub truncated: bool,
}

/// Run `git <args>` in `dir`, bounded in every direction.
///
/// `None` means it could not be run at all — no `git` on `PATH`, no thread to
/// drain it, or the deadline elapsed. That is distinct from a run that
/// finished and failed, which comes back as `ok: false`.
pub fn run_git(dir: &str, args: &[&str], cap: u64, deadline: Duration) -> Option<GitRun> {
    use std::io::Read as _;

    let mut child = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        // Swallowed rather than captured: git's chatter on stderr is not an
        // answer, and the exit status already says whether there was one.
        .stderr(Stdio::null())
        .spawn()
        .ok()?;

    let reap = |mut child: std::process::Child| {
        let _ = child.kill();
        let _ = child.wait();
        None
    };
    let Some(stdout) = child.stdout.take() else {
        return reap(child);
    };
    // One byte past the cap, so "exactly at the cap" and "there was more" stay
    // distinguishable without asking the child how much it meant to write.
    let drain = match std::thread::Builder::new()
        .name("zest-daemon-git-drain".into())
        .spawn(move || {
            let mut out = Vec::new();
            let _ = stdout.take(cap + 1).read_to_end(&mut out);
            out
        }) {
        Ok(drain) => drain,
        Err(_) => return reap(child),
    };

    let started = Instant::now();
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break Some(status),
            Ok(None) if started.elapsed() < deadline => {
                std::thread::sleep(Duration::from_millis(25));
            }
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                break None;
            }
        }
    };
    let mut out = drain.join().ok()?;
    let status = status?;

    let truncated = out.len() as u64 > cap;
    out.truncate(cap as usize);
    Some(GitRun { out, ok: status.success(), truncated })
}

/// Trim a unified diff to `cap` bytes **at a file boundary**.
///
/// Cutting a diff wherever the byte count ran out hands a client half a hunk:
/// a header promising six lines followed by two, which every parser is
/// entitled to treat as corrupt. Dropping whole files instead means the panel
/// shows fewer files, correctly, and says so.
///
/// Returns the kept prefix and whether anything was dropped.
pub fn trim_to_file_boundary(diff: &str, cap: usize) -> (&str, bool) {
    if diff.len() <= cap {
        return (diff, false);
    }
    // The last file header that starts at or before the cap. Searching for it
    // at a line start is what stops a `diff --git` appearing *inside* a hunk
    // — a diff of a diff, which is a real thing to have in a repository —
    // from being mistaken for one.
    let mut cut = 0;
    let mut at = 0;
    for line in diff.split_inclusive('\n') {
        if at > cap {
            break;
        }
        if line.starts_with("diff --git ") && at > 0 {
            cut = at;
        }
        at += line.len();
    }
    if cut == 0 {
        // One file, already over the cap: there is no boundary to cut at, and
        // half of it is worse than none of it.
        return ("", true);
    }
    (&diff[..cut], true)
}

/// How much unified diff a reply carries before `truncated` says there was
/// more. Whole files, never a partial one — see [`trim_to_file_boundary`].
const DIFF_CAP: usize = 1024 * 1024;

/// How many untracked names come back before `untracked_truncated`.
const UNTRACKED_CAP: usize = 1000;

/// The `??` entries of a `git status --porcelain -z`, and whether the list is
/// short of the truth.
///
/// Split out from [`git_diff`] so the awkward case is testable without a
/// repository half a megabyte wide: when the output was capped mid-name, the
/// final entry is a *fragment* — a path that names nothing, which the panel
/// would render as a row that cannot be opened. A complete listing always ends
/// with its terminator, so the absence of one is what identifies the fragment.
fn untracked_names(out: &[u8], truncated: bool) -> (Vec<String>, bool) {
    let mut entries: Vec<&[u8]> = out.split(|&b| b == 0).collect();
    let mut cut = truncated;
    if truncated && !out.ends_with(&[0]) {
        entries.pop();
        cut = true;
    }
    let mut names: Vec<String> = entries
        .into_iter()
        .filter(|e| e.len() > 3 && &e[..3] == b"?? ")
        .map(|e| String::from_utf8_lossy(&e[3..]).into_owned())
        .collect();
    cut |= names.len() > UNTRACKED_CAP;
    names.truncate(UNTRACKED_CAP);
    (names, cut)
}

/// Answer [`zest_proto::ClientMessage::GitDiff`]: what is uncommitted in the
/// repository containing `cwd`.
///
/// Staged *and* unstaged, against HEAD, in one diff — the panel shows what
/// would be committed if you committed everything, which is the question a
/// person opening it is asking. Two lists they have to add up in their head is
/// the shape this deliberately avoids.
pub fn git_diff(cwd: &str) -> zest_proto::HostMessage {
    use zest_proto::HostMessage;

    let refuse = |why: String| HostMessage::GitDiffResult {
        cwd: cwd.to_string(),
        repo_root: String::new(),
        diff: String::new(),
        truncated: false,
        untracked: Vec::new(),
        untracked_truncated: false,
        error: why,
    };

    let Some(root_run) = run_git(cwd, &["rev-parse", "--show-toplevel"], 8 * 1024, DIFF_DEADLINE)
    else {
        return refuse("git could not be run here".into());
    };
    if !root_run.ok {
        return refuse("that is not a git repository".into());
    }
    let repo_root = String::from_utf8_lossy(&root_run.out).trim().to_string();
    if repo_root.is_empty() {
        return refuse("that is not a git repository".into());
    }

    // A repository with no commits has no HEAD to diff against, and `git diff
    // HEAD` fails outright rather than treating it as empty. Falling back to
    // the worktree-versus-index diff is the closest true answer; what it
    // cannot show — files already staged in that fresh repo — is exactly what
    // the untracked list does not cover either, and a first commit is a
    // narrow enough case not to grow a third invocation for.
    let born = run_git(&repo_root, &["rev-parse", "-q", "--verify", "HEAD"], 1024, DIFF_DEADLINE)
        .is_some_and(|r| r.ok);
    let args: &[&str] = if born {
        &["diff", "HEAD", "--no-color", "--no-ext-diff"]
    } else {
        &["diff", "--no-color", "--no-ext-diff"]
    };

    let Some(diff_run) = run_git(&repo_root, args, DIFF_CAP as u64 + 1, DIFF_DEADLINE) else {
        return refuse("git did not answer in time".into());
    };
    if !diff_run.ok {
        return refuse("git could not describe the changes here".into());
    }
    let raw = String::from_utf8_lossy(&diff_run.out).into_owned();
    let (kept, dropped) = trim_to_file_boundary(&raw, DIFF_CAP);
    let diff = kept.to_string();
    let truncated = dropped || diff_run.truncated;

    // `-z` because a filename may hold a space or a newline, and the
    // line-oriented form of this output cannot be parsed when it does — which
    // is the case a person hits once and never forgets.
    let mut untracked: Vec<String> = Vec::new();
    let mut untracked_truncated = false;
    if let Some(st) = run_git(&repo_root, &["status", "--porcelain", "-z"], 512 * 1024, DIFF_DEADLINE)
        .filter(|st| st.ok)
    {
        let (names, cut) = untracked_names(&st.out, st.truncated);
        untracked = names;
        untracked_truncated = cut;
    }

    HostMessage::GitDiffResult {
        cwd: cwd.to_string(),
        repo_root,
        diff,
        truncated,
        untracked,
        untracked_truncated,
        error: String::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_diff_is_trimmed_between_files_and_never_inside_one() {
        let diff = "diff --git a/one b/one\n@@ -1 +1 @@\n-a\n+b\n\
                    diff --git a/two b/two\n@@ -1 +1 @@\n-c\n+d\n";
        let first = "diff --git a/one b/one\n@@ -1 +1 @@\n-a\n+b\n";

        let (kept, dropped) = trim_to_file_boundary(diff, diff.len());
        assert_eq!(kept, diff, "a diff that fits is not touched");
        assert!(!dropped);

        // A cap landing inside the second file keeps only the first, whole.
        let (kept, dropped) = trim_to_file_boundary(diff, first.len() + 10);
        assert_eq!(kept, first, "the cut lands on the file boundary, not the cap");
        assert!(dropped);
    }

    #[test]
    fn a_single_file_larger_than_the_cap_is_dropped_rather_than_halved() {
        let diff = "diff --git a/big b/big\n@@ -1 +1 @@\n-aaaaaaaaaaaaaaaa\n+bbbbbbbbbbbbbbbb\n";
        let (kept, dropped) = trim_to_file_boundary(diff, 30);
        assert!(kept.is_empty(), "half a hunk is not a diff any parser should be handed");
        assert!(dropped, "and the client is told there was more");
    }

    #[test]
    fn a_diff_containing_a_diff_is_not_cut_at_the_inner_one() {
        // A repository holding patch files is not exotic, and the inner
        // `diff --git` arrives as `+diff --git ...` — a content line. Cutting
        // there would split the outer file's hunk.
        let diff = "diff --git a/p.patch b/p.patch\n@@ -1 +2 @@\n+diff --git a/x b/x\n+@@ -1 +1 @@\n\
                    diff --git a/z b/z\n@@ -1 +1 @@\n-q\n+r\n";
        let outer = "diff --git a/p.patch b/p.patch\n@@ -1 +2 @@\n+diff --git a/x b/x\n+@@ -1 +1 @@\n";
        let (kept, dropped) = trim_to_file_boundary(diff, outer.len() + 5);
        assert_eq!(kept, outer, "only a header at the start of a line is a boundary");
        assert!(dropped);
    }

    /// One spelling of a path, for comparing git's against the filesystem's.
    ///
    /// **Git speaks its own path dialect on Windows** and this is the whole
    /// reason the helper exists: `rev-parse --show-toplevel` answers
    /// `C:/Users/…` while Rust's `canonicalize` answers
    /// `\\?\C:\Users\…` — same directory, three differences (the UNC prefix,
    /// the separator, and the case a temp path arrives in). Comparing them
    /// literally passes on unix and fails only on Windows, which is the
    /// expensive kind of test to write.
    fn same_path(p: &str) -> String {
        p.trim_start_matches(r"\\?\").replace('\\', "/").to_lowercase()
    }

    /// A throwaway repository, or `None` when this machine has no git.
    ///
    /// `-c` for identity rather than `git config`, and `commit` only where a
    /// test needs a HEAD: a CI runner has no `user.email`, and the failure
    /// reads as a broken feature rather than a missing global.
    struct Repo(std::path::PathBuf);
    impl Repo {
        fn new(tag: &str) -> Option<Self> {
            let p = std::env::temp_dir().join(format!("zest-gitdiff-{tag}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&p);
            std::fs::create_dir_all(&p).ok()?;
            let p = p.canonicalize().ok()?;
            let run = run_git(&p.to_string_lossy(), &["init"], 64 * 1024, DIFF_DEADLINE)?;
            run.ok.then_some(Self(p))
        }
        fn path(&self) -> String {
            self.0.to_string_lossy().into_owned()
        }
        fn git(&self, args: &[&str]) -> bool {
            run_git(&self.path(), args, 64 * 1024, DIFF_DEADLINE).is_some_and(|r| r.ok)
        }
        fn commit(&self, msg: &str) -> bool {
            self.git(&["add", "-A"])
                && self.git(&[
                    "-c",
                    "user.email=t@example.invalid",
                    "-c",
                    "user.name=t",
                    "commit",
                    "-q",
                    "-m",
                    msg,
                ])
        }
        fn write(&self, name: &str, body: &str) {
            std::fs::write(self.0.join(name), body).expect("write");
        }
        fn result(&self) -> zest_proto::HostMessage {
            git_diff(&self.path())
        }
    }
    impl Drop for Repo {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn parts(
        msg: &zest_proto::HostMessage,
    ) -> (&str, &str, bool, &[String], &str) {
        let zest_proto::HostMessage::GitDiffResult {
            repo_root, diff, truncated, untracked, error, ..
        } = msg
        else {
            panic!("a diff answers with a diff result: {msg:?}");
        };
        (repo_root, diff, *truncated, untracked, error)
    }

    #[test]
    fn a_change_shows_up_staged_or_not_and_untracked_files_come_by_name() {
        let Some(repo) = Repo::new("changes") else { return };
        repo.write("tracked.txt", "one\n");
        if !repo.commit("first") {
            return;
        }

        repo.write("tracked.txt", "two\n");
        repo.write("fresh.txt", "brand new\n");

        let msg = repo.result();
        let (root, diff, _, untracked, error) = parts(&msg);
        assert!(error.is_empty(), "{error}");
        assert_eq!(
            same_path(root),
            same_path(&repo.path()),
            "the root is where the diff's paths are relative to"
        );
        assert!(diff.contains("tracked.txt"), "the modified file is in the diff:\n{diff}");
        assert!(diff.contains("+two"), "with its new content:\n{diff}");
        assert_eq!(untracked, ["fresh.txt".to_string()], "and the new file is named, not diffed");
        assert!(
            !diff.contains("brand new"),
            "an untracked file has no index entry to diff against, so its content \
             is the panel's second question and not this one's answer"
        );

        // Staging it must not make it vanish: the panel shows what would be
        // committed if everything were, so `diff HEAD` is the right question
        // and `git diff` alone would have gone quiet here.
        assert!(repo.git(&["add", "tracked.txt"]));
        let msg = repo.result();
        let (_, diff, _, _, _) = parts(&msg);
        assert!(
            diff.contains("+two"),
            "a staged change is still an uncommitted one:\n{diff}"
        );
    }

    #[test]
    fn a_filename_with_a_space_survives_the_untracked_listing() {
        // The reason the listing is `-z`: the line-oriented form of this
        // output cannot be parsed when a name holds a space or a newline, and
        // git quotes it into something else again.
        let Some(repo) = Repo::new("spaces") else { return };
        repo.write("a.txt", "x\n");
        if !repo.commit("first") {
            return;
        }
        repo.write("two words.txt", "y\n");

        let msg = repo.result();
        let (_, _, _, untracked, _) = parts(&msg);
        assert_eq!(
            untracked,
            ["two words.txt".to_string()],
            "the name comes back whole and unquoted"
        );
    }

    #[test]
    fn a_repository_with_no_commits_still_answers() {
        // `git diff HEAD` fails outright when HEAD does not resolve, so
        // without the unborn fallback a fresh repo reads as an error rather
        // than as a repo with nothing committed.
        let Some(repo) = Repo::new("unborn") else { return };
        repo.write("new.txt", "hello\n");

        let msg = repo.result();
        let (root, _, _, untracked, error) = parts(&msg);
        assert!(error.is_empty(), "a repo with no commits is not a failure: {error}");
        assert!(!root.is_empty());
        assert_eq!(untracked, ["new.txt".to_string()]);
    }

    #[test]
    fn a_directory_that_is_not_a_repository_says_so() {
        let dir = std::env::temp_dir().join(format!("zest-notrepo-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("mkdir");

        let msg = git_diff(&dir.to_string_lossy());
        let (_, diff, _, _, error) = parts(&msg);
        // A temp directory *inside* somebody's repository would answer with a
        // root; only assert the two channels stay consistent, never that this
        // machine's /tmp is or is not in one.
        if error.is_empty() {
            assert!(diff.is_empty() || diff.contains("diff --git"));
        } else {
            assert!(error.contains("repository"), "and it says which way: {error}");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_clean_tree_is_an_empty_diff_and_not_an_error() {
        // The distinction the `error` field exists for: nothing to show and
        // could-not-look must not render the same.
        let Some(repo) = Repo::new("clean") else { return };
        repo.write("a.txt", "x\n");
        if !repo.commit("first") {
            return;
        }

        let msg = repo.result();
        let (_, diff, truncated, untracked, error) = parts(&msg);
        assert!(error.is_empty(), "{error}");
        assert!(diff.is_empty(), "nothing changed:\n{diff}");
        assert!(untracked.is_empty());
        assert!(!truncated);
    }

    #[test]
    fn a_listing_cut_mid_filename_drops_the_fragment_rather_than_naming_it() {
        // Only `??` rows are ours; the modified one is git's answer to a
        // different question and must not appear.
        let whole = b"?? one.txt\0 M tracked.txt\0?? two words.txt\0";
        let (names, cut) = untracked_names(whole, false);
        assert_eq!(names, ["one.txt".to_string(), "two words.txt".to_string()]);
        assert!(!cut);

        // Capped mid-name: no trailing NUL, so the tail is a fragment. Naming
        // it would put a row in the panel that opens nothing.
        let clipped = b"?? one.txt\0?? two wor";
        let (names, cut) = untracked_names(clipped, true);
        assert_eq!(names, ["one.txt".to_string()], "the fragment is dropped, not reported");
        assert!(cut, "and the client is told the list is short");

        // Capped exactly on a boundary: everything present is whole, and the
        // only thing missing is what never arrived.
        let aligned = b"?? one.txt\0";
        let (names, cut) = untracked_names(aligned, true);
        assert_eq!(names, ["one.txt".to_string()]);
        assert!(cut);
    }

    #[test]
    fn a_run_that_cannot_start_is_not_a_run_that_failed() {
        // `None` and `ok: false` mean different things and one test says so:
        // a directory that is not a repository *ran* git, which is why the
        // panel can say "not a repository" rather than "git is missing".
        let tmp = std::env::temp_dir();
        let Some(run) = run_git(&tmp.to_string_lossy(), &["rev-parse", "--show-toplevel"], 4096, DIFF_DEADLINE)
        else {
            // No git on this machine: the distinction is untestable here, and
            // skipping beats a red run on a box without git.
            return;
        };
        // The temp dir may or may not sit inside somebody's repository, so the
        // assertion is only that the two channels are separate and populated
        // consistently — never that a particular machine is or is not in one.
        assert_eq!(
            run.ok,
            !run.out.is_empty(),
            "a successful rev-parse prints a root and a failed one prints nothing"
        );
    }
}
