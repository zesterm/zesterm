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

/// Why this path is not something the editor can open, if it is not.
///
/// A directory is the obvious case and the only one the first cut refused.
/// The rest matter more: **opening a FIFO or a character device parks the
/// thread in `read` until somebody writes to it**, and this work runs on the
/// serve loop precisely because it was argued to be bounded. A named pipe in a
/// repo is enough to hang that connection — its session included — with no
/// error anywhere.
fn not_a_regular_file(meta: &std::fs::Metadata) -> Option<&'static str> {
    if meta.is_dir() {
        Some("that is a directory")
    } else if !meta.is_file() {
        // A socket, a FIFO, a block or character device. Named as one kind
        // because the distinction does not change what the editor can do.
        Some("that is not a regular file")
    } else {
        None
    }
}

/// The hash of what is on disk, for a save's conflict check.
///
/// Bounded the same way a read is, and refusing rather than truncating: a
/// hash over the first four megabytes of a larger file would be a base that
/// compares equal to a file it does not describe, which is worse than no
/// answer. A client cannot reach this anyway — the read that would have given
/// it a base for such a file hands back an empty one.
fn disk_hash(real: &Path) -> Result<String, String> {
    use std::io::Read as _;
    let f = std::fs::File::open(real).map_err(|e| format!("{e}"))?;
    let meta = f.metadata().map_err(|e| format!("{e}"))?;
    if let Some(why) = not_a_regular_file(&meta) {
        return Err(why.to_string());
    }
    let mut buf = Vec::new();
    f.take(READ_CAP as u64 + 1).read_to_end(&mut buf).map_err(|e| format!("{e}"))?;
    if buf.len() > READ_CAP {
        return Err("that file is too large for the editor to check against".into());
    }
    Ok(hash_hex(&buf))
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
    /// Enough for any real chain, few enough that a cycle ends here.
    const MAX_HOPS: usize = 16;

    let joined = join(path, cwd)?;
    match joined.canonicalize() {
        Ok(real) => return Ok(real),
        // Only "it is not there yet" earns the fallback below. Taking it for
        // *any* failure — an unreadable directory, say — would write to a
        // path nothing had actually resolved, and report success.
        Err(e) if e.kind() != std::io::ErrorKind::NotFound => {
            return Err(format!("{}: {e}", joined.display()));
        }
        Err(_) => {}
    }

    // Nothing is there — but "nothing is there" also describes a symlink whose
    // target does not exist yet, and `canonicalize` gives up on the whole path
    // rather than telling the two apart. Following the link by hand is what
    // keeps a save landing where the link points, the way a shell's `>` does;
    // writing to the link's own path instead would replace the link with a
    // regular file, which is the dotfile-detaching bug one step later.
    let mut cur = joined.clone();
    let mut hops = 0;
    while let Ok(target) = std::fs::read_link(&cur) {
        hops += 1;
        if hops > MAX_HOPS {
            return Err(format!("{}: too many symbolic links", joined.display()));
        }
        cur = if target.is_absolute() {
            target
        } else {
            cur.parent().unwrap_or(Path::new("")).join(target)
        };
    }

    let parent = cur.parent().ok_or_else(|| format!("{} has no directory", cur.display()))?;
    let name = cur.file_name().ok_or_else(|| format!("{} does not name a file", cur.display()))?;
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
    if let Some(why) = not_a_regular_file(&meta) {
        return read_refusal(&shown, why.into());
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
        Ok(m) => {
            if let Some(why) = not_a_regular_file(&m) {
                return write_refusal(&shown, why.into());
            }
            Some(m)
        }
        Err(_) => None,
    };

    // Every disagreement between what the client last read and what is on disk
    // now comes out here as one `conflict`, carrying the disk's hash. The
    // client has one branch to write instead of four, and each of the four
    // would otherwise have to be told apart from a plain I/O failure.
    match (&existing, base_hash.is_empty()) {
        (Some(_), true) => {
            // A failure here is reported, not defaulted away: an empty hash is
            // the wire's word for "no base", so swallowing a permission error
            // into one would answer a refusal the client cannot act on with a
            // value that says something else entirely.
            let disk = match disk_hash(&real) {
                Ok(h) => h,
                Err(why) => return write_refusal(&shown, why),
            };
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
            let disk = match disk_hash(&real) {
                Ok(h) => h,
                Err(why) => return write_refusal(&shown, why),
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
    fn a_fifo_is_refused_rather_than_opened() {
        // The one that would not have shown up as a wrong answer. Opening a
        // FIFO parks in `read` until somebody writes to it, and this work runs
        // on the serve loop *because* it was argued to be bounded — so a named
        // pipe anywhere a person might click is enough to hang that connection
        // and its session, with nothing logged. The test times out rather than
        // failing if this regresses, which is itself the signal.
        let s = Scratch::new("fifo");
        let fifo = s.join("pipe");
        let c = std::ffi::CString::new(fifo.to_string_lossy().as_bytes()).expect("cstring");
        // SAFETY: a NUL-terminated path in a scratch directory this test owns.
        assert_eq!(unsafe { libc_mkfifo(c.as_ptr()) }, 0, "mkfifo");

        let msg = read_file("pipe", &s.path());
        let (data, _, _, _, _, error) = contents(&msg);
        assert!(data.is_empty());
        assert!(
            error.contains("regular file"),
            "a FIFO is refused by kind, not opened and waited on: {error}"
        );

        // And a save must not reach it either — the same open, the same park.
        let msg = write_file("pipe", &s.path(), b"x", "");
        let (_, _, error) = written(&msg);
        assert!(error.contains("regular file"), "{error}");
    }

    #[cfg(unix)]
    unsafe extern "C" {
        #[link_name = "mkfifo"]
        fn libc_mkfifo(path: *const std::ffi::c_char) -> i32;
    }

    #[test]
    fn a_conflict_check_against_an_oversized_file_refuses_instead_of_reading_it_all() {
        // `write_file` hashes what is on disk to decide whether the base still
        // holds, and that read has to be bounded for the same reason the
        // editor's read is: it happens on the connection thread. Refusing
        // beats truncating — a hash over the first four megabytes would be a
        // base that compares equal to a file it does not describe.
        let s = Scratch::new("bigconflict");
        std::fs::write(s.join("big.txt"), vec![b'q'; READ_CAP + 1]).expect("write");

        let msg = write_file("big.txt", &s.path(), b"small", &hash_hex(b"whatever"));
        let (_, conflict, error) = written(&msg);
        assert!(!conflict, "this is a refusal, not a disagreement about content");
        assert!(error.contains("too large"), "and it says why: {error}");
        assert_eq!(
            std::fs::read(s.join("big.txt")).expect("read").len(),
            READ_CAP + 1,
            "nothing was written"
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_broken_symlink_is_reported_rather_than_written_through() {
        // `resolve_for_write` falls back to "canonicalize the parent" when the
        // target is not there yet. Taking that path for *any* canonicalize
        // failure is how a broken link gets replaced by a regular file at the
        // link's own path — the opposite of the follow-the-target guarantee,
        // reported as success.
        let s = Scratch::new("broken");
        std::os::unix::fs::symlink(s.join("nowhere"), s.join("dangling")).expect("symlink");

        let msg = write_file("dangling", &s.path(), b"x", "");
        let (_, _, error) = written(&msg);
        assert!(error.is_empty(), "a dangling link's target is simply a file that is not there yet: {error}");
        assert_eq!(
            std::fs::read(s.join("nowhere")).expect("read"),
            b"x",
            "so the write lands on the target the link names"
        );
        assert!(
            std::fs::symlink_metadata(s.join("dangling")).expect("meta").file_type().is_symlink(),
            "and the link is still a link"
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_symlink_cycle_ends_in_a_refusal_rather_than_a_loop() {
        // A closed cycle is caught by the kernel first — `canonicalize`
        // answers ELOOP, which is not `NotFound`, so it never reaches the
        // hand-rolled walk. The bound in that walk is the backstop for the
        // other shape, a *dangling* chain long enough to matter, where
        // `canonicalize` says `NotFound` and the links are followed here.
        // Both end in a refusal; which layer refuses is not the contract.
        let s = Scratch::new("cycle");
        std::os::unix::fs::symlink(s.join("b"), s.join("a")).expect("symlink a");
        std::os::unix::fs::symlink(s.join("a"), s.join("b")).expect("symlink b");

        let msg = write_file("a", &s.path(), b"x", "");
        let (_, _, error) = written(&msg);
        assert!(
            error.to_lowercase().contains("symbolic link"),
            "a cycle is refused, and says so: {error}"
        );
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
