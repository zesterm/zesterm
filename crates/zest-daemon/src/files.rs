//! Reading and writing one file, on this machine, for the built-in editor
//! (#446).
//!
//! Pure and synchronous on purpose. The daemon answers `ReadFile` from its
//! dispatch arm, and a window hosting its own session calls these same
//! functions directly rather than round-tripping through a socket to itself
//! (#434's rule, the `ContextEngine` precedent) — so truncation, hashing and
//! the atomic-rename dance exist once. Two implementations of "is this file
//! too big" is how the two disagree.
//!
//! The work is bounded, which is why it may run on the connection thread at
//! all: a read stops at [`READ_CAP`], and a file past it is never hashed.
//! (`git diff` is the opposite case — a subprocess with a deadline — and gets
//! a worker thread of its own.)

use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};
use zest_proto::HostMessage;

/// How much of a file a [`HostMessage::FileContents`] carries before
/// `truncated` says the rest is there.
///
/// Half of `zest_proto::frame::MAX_FRAME`, leaving room for the message around
/// it — and generous for the thing it exists to serve, since a source file
/// past four megabytes is not being read by a person.
pub const READ_CAP: usize = 4 * 1024 * 1024;

/// How far in a NUL still counts as "this is not text".
///
/// A UTF-16 file or a PNG announces itself in the first line; a NUL deep in an
/// otherwise-textual file is more likely one odd byte than a change of kind.
const SNIFF: usize = 8 * 1024;

/// Lowercase hex of a SHA-256 digest — the form both ends compare.
///
/// `zest_proto::hex` rather than a local loop, because it is already the
/// spelling every fixed-width value on this wire uses.
fn hash_hex(bytes: &[u8]) -> String {
    zest_proto::hex::encode(&Sha256::digest(bytes))
}

/// A refusal, in the shape [`HostMessage::FileContents`] carries one: this
/// message with `error` set, never `HostMessage::Error` — a sessionless
/// `Error` is what an *old* daemon says, and the app reads that as "too old".
fn read_refusal(path: &str, why: String) -> HostMessage {
    HostMessage::FileContents {
        path: path.to_string(),
        data: Vec::new(),
        truncated: false,
        binary: false,
        hash: String::new(),
        size: 0,
        readonly: false,
        error: why,
    }
}

fn write_refusal(path: &str, why: String) -> HostMessage {
    HostMessage::FileWritten {
        path: path.to_string(),
        hash: String::new(),
        conflict: false,
        error: why,
    }
}

/// Where a client's `(path, cwd)` lands on this filesystem.
///
/// A relative path resolves against `cwd` — which came from a shell escape and
/// is therefore a *claim*, not a fact. That is fine here and fatal to trust
/// anywhere else: the worst a forged cwd can do is open the wrong file, and
/// the resolved path travels back in the reply so what the person reads is the
/// disk's answer rather than the shell's.
fn join(path: &str, cwd: &str) -> Result<PathBuf, String> {
    if path.is_empty() {
        return Err("no path given".into());
    }
    let p = Path::new(path);
    if p.is_absolute() {
        return Ok(p.to_path_buf());
    }
    if cwd.is_empty() {
        return Err(format!("{path} is relative and no working directory came with it"));
    }
    Ok(Path::new(cwd).join(p))
}

/// The path a *write* should land on, with symlinks followed.
///
/// Canonicalizing first is what makes the temp-and-rename below replace a
/// symlink's **target** instead of replacing the symlink with a regular file —
/// the difference between saving `~/.zshrc` and quietly detaching it from the
/// dotfiles repo it points into. A file that does not exist yet has no
/// canonical form, so its directory is canonicalized instead.
fn resolve_for_write(path: &str, cwd: &str) -> Result<PathBuf, String> {
    let joined = join(path, cwd)?;
    if let Ok(real) = joined.canonicalize() {
        return Ok(real);
    }
    let parent = joined.parent().ok_or_else(|| format!("{} has no directory", joined.display()))?;
    let name = joined
        .file_name()
        .ok_or_else(|| format!("{} does not name a file", joined.display()))?;
    let real_parent = parent
        .canonicalize()
        .map_err(|e| format!("{}: {e}", if parent.as_os_str().is_empty() { Path::new(".") } else { parent }.display()))?;
    Ok(real_parent.join(name))
}

/// Answer [`zest_proto::ClientMessage::ReadFile`].
///
/// Everything that can go wrong answers with *why* rather than an empty
/// success, for the reason a directory listing does: an empty file and a
/// refused one must not render the same.
pub fn read_file(path: &str, cwd: &str) -> HostMessage {
    let joined = match join(path, cwd) {
        Ok(p) => p,
        Err(why) => return read_refusal(path, why),
    };
    let real = match joined.canonicalize() {
        Ok(p) => p,
        Err(e) => return read_refusal(&joined.to_string_lossy(), format!("{e}")),
    };
    let shown = real.to_string_lossy().into_owned();

    let meta = match std::fs::metadata(&real) {
        Ok(m) => m,
        Err(e) => return read_refusal(&shown, format!("{e}")),
    };
    if meta.is_dir() {
        return read_refusal(&shown, "that is a directory".into());
    }

    // Read one byte past the cap, which is how "exactly at the cap" and "over
    // it" stay distinguishable without trusting the size the metadata claims —
    // a growing file, /proc, and a pipe all lie about it in different ways.
    let mut data = match std::fs::File::open(&real) {
        Ok(f) => {
            use std::io::Read as _;
            let mut buf = Vec::new();
            match f.take(READ_CAP as u64 + 1).read_to_end(&mut buf) {
                Ok(_) => buf,
                Err(e) => return read_refusal(&shown, format!("{e}")),
            }
        }
        Err(e) => return read_refusal(&shown, format!("{e}")),
    };

    let truncated = data.len() > READ_CAP;
    data.truncate(READ_CAP);
    let binary = data.iter().take(SNIFF).any(|&b| b == 0);

    // A truncated read carries **no hash**, and that is the mechanism rather
    // than an omission: `base_hash` is what a later save is checked against,
    // an empty one means "create, and refuse if it exists", and the file
    // plainly does exist — so a buffer holding only the first four megabytes
    // of a file cannot save over the rest of it. The alternative, hashing a
    // file of any size to hand back a base the client must then be trusted not
    // to use, is both unbounded work and a rule enforced by good intentions.
    let hash = if truncated { String::new() } else { hash_hex(&data) };

    HostMessage::FileContents {
        path: shown,
        data,
        truncated,
        binary,
        hash,
        size: meta.len(),
        readonly: meta.permissions().readonly(),
        error: String::new(),
    }
}

/// Answer [`zest_proto::ClientMessage::WriteFile`].
///
/// Refuses rather than obeys whenever the disk stopped matching `base_hash`,
/// and hands back what *is* there so the client can offer reload-theirs
/// without a second round trip.
pub fn write_file(path: &str, cwd: &str, data: &[u8], base_hash: &str) -> HostMessage {
    let real = match resolve_for_write(path, cwd) {
        Ok(p) => p,
        Err(why) => return write_refusal(path, why),
    };
    let shown = real.to_string_lossy().into_owned();

    let existing = match std::fs::metadata(&real) {
        Ok(m) if m.is_dir() => return write_refusal(&shown, "that is a directory".into()),
        Ok(m) => Some(m),
        Err(_) => None,
    };

    // Every disagreement between what the client last read and what is on disk
    // now comes out here as one `conflict`, carrying the disk's hash. The
    // client has one branch to write instead of four, and each of the four
    // would otherwise have to be told apart from a plain I/O failure.
    match (&existing, base_hash.is_empty()) {
        (Some(_), true) => {
            let disk = std::fs::read(&real).map(|b| hash_hex(&b)).unwrap_or_default();
            return HostMessage::FileWritten {
                path: shown,
                hash: disk,
                conflict: true,
                error: "a file is already there".into(),
            };
        }
        (None, false) => {
            return HostMessage::FileWritten {
                path: shown,
                hash: String::new(),
                conflict: true,
                error: "the file is gone".into(),
            };
        }
        (Some(_), false) => {
            let disk = match std::fs::read(&real) {
                Ok(b) => hash_hex(&b),
                Err(e) => return write_refusal(&shown, format!("{e}")),
            };
            if disk != base_hash {
                return HostMessage::FileWritten {
                    path: shown,
                    hash: disk,
                    conflict: true,
                    error: "the file changed on disk since it was opened".into(),
                };
            }
        }
        (None, true) => {}
    }

    if let Err(why) = replace_contents(&real, data, existing.as_ref()) {
        return write_refusal(&shown, why);
    }

    HostMessage::FileWritten {
        path: shown,
        hash: hash_hex(data),
        conflict: false,
        error: String::new(),
    }
}

/// Put `data` at `real`, atomically as far as a reader is concerned.
///
/// A sibling temp file, then a rename: a reader either sees the whole old file
/// or the whole new one, never the half-written middle that a plain
/// truncate-and-write exposes — and a crash costs the temp file rather than
/// the user's source. The temp is a sibling because a rename across
/// filesystems is a copy, and `std::env::temp_dir()` is routinely on another
/// one.
///
/// What this deliberately does not preserve: ownership and extended
/// attributes, which a rename replaces along with the inode. That is the same
/// trade `vim`'s default `backupcopy=auto` makes, and the same one every
/// editor that values atomicity makes.
fn replace_contents(real: &Path, data: &[u8], existing: Option<&std::fs::Metadata>) -> Result<(), String> {
    use std::io::Write as _;

    let dir = real.parent().ok_or_else(|| format!("{} has no directory", real.display()))?;
    let name = real.file_name().and_then(|n| n.to_str()).unwrap_or("file");

    // Unique per process and per call: two windows saving two files in one
    // directory at the same moment must not meet in the same temp.
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let n = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let tmp = dir.join(format!(".{name}.zest-{}-{n}", std::process::id()));

    let write = || -> std::io::Result<()> {
        let mut f = std::fs::File::create(&tmp)?;
        f.write_all(data)?;
        // Before the rename, not after: a rename that beats its own data to
        // disk is how a power cut leaves an empty file where the old one was.
        f.sync_all()
    };
    if let Err(e) = write() {
        let _ = std::fs::remove_file(&tmp);
        return Err(format!("{e}"));
    }

    // The mode the file already had, carried onto its replacement — otherwise
    // saving an executable script makes it unexecutable, which reads as the
    // script breaking rather than the editor doing it.
    if let Some(meta) = existing {
        let _ = std::fs::set_permissions(&tmp, meta.permissions());
    }

    // `std::fs::rename` replaces an existing destination on both platforms
    // (Windows goes through `MoveFileEx` with `MOVEFILE_REPLACE_EXISTING`), so
    // no per-platform arm is needed here.
    if let Err(e) = std::fs::rename(&tmp, real) {
        let _ = std::fs::remove_file(&tmp);
        return Err(format!("{e}"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A scratch directory that cleans itself up, named per test so two
    /// running at once cannot share one.
    struct Scratch(PathBuf);
    impl Scratch {
        fn new(tag: &str) -> Self {
            let p = std::env::temp_dir().join(format!("zest-files-{tag}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&p);
            std::fs::create_dir_all(&p).expect("scratch");
            // Canonicalized because macOS's temp dir is a symlink into
            // /private, and every path this module returns has been through
            // `canonicalize` — comparing against the uncanonicalized form
            // fails on one platform only, which is the worst kind of test.
            Self(p.canonicalize().expect("canonical scratch"))
        }
        fn join(&self, name: &str) -> PathBuf {
            self.0.join(name)
        }
        fn path(&self) -> String {
            self.0.to_string_lossy().into_owned()
        }
    }
    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn contents(msg: &HostMessage) -> (&[u8], bool, bool, &str, u64, &str) {
        let HostMessage::FileContents { data, truncated, binary, hash, size, error, .. } = msg
        else {
            panic!("a read answers with contents: {msg:?}");
        };
        (data, *truncated, *binary, hash, *size, error)
    }

    fn written(msg: &HostMessage) -> (&str, bool, &str) {
        let HostMessage::FileWritten { hash, conflict, error, .. } = msg else {
            panic!("a write answers with a written: {msg:?}");
        };
        (hash, *conflict, error)
    }

    #[test]
    fn a_file_reads_back_whole_with_a_hash_over_it() {
        let s = Scratch::new("read");
        std::fs::write(s.join("a.txt"), b"hello\n").expect("write");

        let msg = read_file("a.txt", &s.path());
        let (data, truncated, binary, hash, size, error) = contents(&msg);
        assert!(error.is_empty(), "a plain read does not refuse: {error}");
        assert_eq!(data, b"hello\n");
        assert!(!truncated);
        assert!(!binary);
        assert_eq!(size, 6, "the size is the file's, not the excerpt's");
        assert_eq!(hash, hash_hex(b"hello\n"), "the hash is over the content, so a save can check it");
    }

    #[test]
    fn a_relative_path_resolves_against_the_cwd_and_comes_back_absolute() {
        let s = Scratch::new("rel");
        std::fs::create_dir_all(s.join("sub")).expect("mkdir");
        std::fs::write(s.join("sub/b.txt"), b"x").expect("write");

        let HostMessage::FileContents { path, error, .. } = read_file("sub/b.txt", &s.path()) else {
            panic!("contents");
        };
        assert!(error.is_empty());
        assert_eq!(
            PathBuf::from(&path),
            s.join("sub/b.txt").canonicalize().expect("canonical"),
            "the reply names the file the host actually opened, not what was asked"
        );

        // The cwd is a shell's claim; without one, a relative path is not a
        // question the daemon can answer, and saying so beats guessing.
        let (_, _, _, _, _, error) = {
            let msg = read_file("sub/b.txt", "");
            let out = contents(&msg);
            (out.0.to_vec(), out.1, out.2, out.3.to_string(), out.4, out.5.to_string())
        };
        assert!(error.contains("relative"), "a relative path with no cwd says why: {error}");
    }

    #[test]
    fn a_missing_file_and_a_directory_each_say_why_rather_than_reading_empty() {
        let s = Scratch::new("why");
        let msg = read_file("nope.txt", &s.path());
        let (data, _, _, _, _, error) = contents(&msg);
        assert!(data.is_empty());
        assert!(!error.is_empty(), "a missing file is not an empty one");

        std::fs::create_dir_all(s.join("d")).expect("mkdir");
        let msg = read_file("d", &s.path());
        let (_, _, _, _, _, error) = contents(&msg);
        assert!(error.contains("directory"), "a directory says so: {error}");
    }

    #[test]
    fn a_nul_early_in_the_file_reads_as_binary_but_still_sends_its_bytes() {
        let s = Scratch::new("bin");
        std::fs::write(s.join("b.bin"), [0x89, 0x50, 0x00, 0x4e]).expect("write");

        let msg = read_file("b.bin", &s.path());
        let (data, _, binary, _, _, error) = contents(&msg);
        assert!(error.is_empty());
        assert!(binary, "a NUL in the first bytes is the sniff this exists for");
        assert_eq!(data.len(), 4, "binary is guidance, not a refusal — the bytes still come");
    }

    #[test]
    fn a_file_past_the_cap_is_truncated_and_carries_no_base_to_save_against() {
        let s = Scratch::new("cap");
        let big = vec![b'z'; READ_CAP + 10];
        std::fs::write(s.join("big.txt"), &big).expect("write");

        let msg = read_file("big.txt", &s.path());
        let (data, truncated, _, hash, size, error) = contents(&msg);
        assert!(error.is_empty());
        assert!(truncated, "more existed than was sent, and it is said rather than cut silently");
        assert_eq!(data.len(), READ_CAP);
        assert_eq!(size, (READ_CAP + 10) as u64, "the size is the whole file's");
        assert!(
            hash.is_empty(),
            "a truncated read hands back no base, so a buffer holding four megabytes \
             of a larger file cannot later save over the rest of it"
        );
    }

    #[test]
    fn a_write_lands_and_hands_back_the_base_for_the_next_one() {
        let s = Scratch::new("write");
        std::fs::write(s.join("w.txt"), b"one").expect("write");
        let base = hash_hex(b"one");

        let msg = write_file("w.txt", &s.path(), b"two", &base);
        let (hash, conflict, error) = written(&msg);
        assert!(error.is_empty(), "{error}");
        assert!(!conflict);
        assert_eq!(hash, hash_hex(b"two"), "the reply's hash is the next save's base");
        assert_eq!(std::fs::read(s.join("w.txt")).expect("read"), b"two");
    }

    #[test]
    fn a_file_that_moved_underneath_is_refused_and_the_disk_wins() {
        let s = Scratch::new("conflict");
        std::fs::write(s.join("c.txt"), b"mine").expect("write");
        let stale = hash_hex(b"what I opened");

        let msg = write_file("c.txt", &s.path(), b"theirs", &stale);
        let (hash, conflict, error) = written(&msg);
        assert!(conflict, "a base that no longer describes the disk refuses: {error}");
        assert_eq!(
            hash,
            hash_hex(b"mine"),
            "the refusal carries what *is* there, so reload-theirs costs no second round trip"
        );
        assert_eq!(
            std::fs::read(s.join("c.txt")).expect("read"),
            b"mine",
            "and nothing was written"
        );
    }

    #[test]
    fn creating_over_something_that_already_exists_is_a_conflict_too() {
        let s = Scratch::new("exists");
        std::fs::write(s.join("e.txt"), b"already").expect("write");

        // An empty base means "create it"; the client believed nothing was
        // there. One branch for every way the disk disagreed.
        let msg = write_file("e.txt", &s.path(), b"new", "");
        let (hash, conflict, _) = written(&msg);
        assert!(conflict);
        assert_eq!(hash, hash_hex(b"already"));
        assert_eq!(std::fs::read(s.join("e.txt")).expect("read"), b"already");

        // And a real create still works.
        let msg = write_file("fresh.txt", &s.path(), b"new", "");
        let (_, conflict, error) = written(&msg);
        assert!(!conflict, "{error}");
        assert_eq!(std::fs::read(s.join("fresh.txt")).expect("read"), b"new");
    }

    #[test]
    fn a_file_deleted_underneath_is_a_conflict_not_a_silent_recreate() {
        let s = Scratch::new("gone");
        let msg = write_file("ghost.txt", &s.path(), b"x", &hash_hex(b"was here"));
        let (_, conflict, error) = written(&msg);
        assert!(conflict, "the client believed a file was there: {error}");
        assert!(!s.join("ghost.txt").exists(), "and it is not recreated behind their back");
    }

    #[cfg(unix)]
    #[test]
    fn saving_an_executable_leaves_it_executable() {
        use std::os::unix::fs::PermissionsExt as _;
        let s = Scratch::new("mode");
        let p = s.join("run.sh");
        std::fs::write(&p, b"#!/bin/sh\n").expect("write");
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).expect("chmod");

        let msg = write_file("run.sh", &s.path(), b"#!/bin/sh\necho hi\n", &hash_hex(b"#!/bin/sh\n"));
        let (_, conflict, error) = written(&msg);
        assert!(!conflict && error.is_empty(), "{error}");
        let mode = std::fs::metadata(&p).expect("meta").permissions().mode();
        assert_eq!(
            mode & 0o777,
            0o755,
            "a rename replaces the inode, so the mode has to be carried across — \
             otherwise saving a script is what makes it stop running"
        );
    }

    #[cfg(unix)]
    #[test]
    fn saving_through_a_symlink_writes_its_target_and_keeps_the_link() {
        let s = Scratch::new("symlink");
        std::fs::write(s.join("real.txt"), b"old").expect("write");
        std::os::unix::fs::symlink(s.join("real.txt"), s.join("link.txt")).expect("symlink");

        let msg = write_file("link.txt", &s.path(), b"new", &hash_hex(b"old"));
        let (_, conflict, error) = written(&msg);
        assert!(!conflict && error.is_empty(), "{error}");
        assert_eq!(
            std::fs::read(s.join("real.txt")).expect("read"),
            b"new",
            "the target is what gets written"
        );
        assert!(
            std::fs::symlink_metadata(s.join("link.txt")).expect("meta").file_type().is_symlink(),
            "and the link is still a link — replacing it with a regular file is how \
             a dotfile quietly detaches from the repo it pointed into"
        );
    }
}
