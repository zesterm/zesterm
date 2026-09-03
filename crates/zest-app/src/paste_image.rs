//! A picture on the clipboard, written to a file so that a paste can name it.
//!
//! ⌘V with a screenshot on the clipboard used to write *nothing*: `get_text`
//! fails on an image-only clipboard, and the program on the other side of the
//! pty never learned a paste had happened (#532).
//!
//! What it sends instead is a **path**, not the bytes. A pty carries bytes for
//! a program reading a terminal and no program reads a PNG off one, whereas a
//! path is what a drag-and-drop already delivers — so a shell gets an argument,
//! `open` shows the picture, an editor opens it, and an agent that reads pasted
//! paths attaches it. Nothing here is specific to one consumer.
//!
//! The rejected alternative, because it will look tempting again: send the
//! bracketed-paste markers with an *empty* payload. Windows Terminal does this,
//! not as a feature but because it never special-cases empty text, and at least
//! one agent answers it by reading the clipboard itself. It is one program's
//! undocumented convention, it is gated to two of the three platforms, and it
//! leaves a shell holding nothing.
//!
//! Everything below takes bytes and a directory as parameters and no `arboard`
//! type appears in this module, which is what lets every test run on a CI
//! machine with no clipboard and no display server.

use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

/// The filename prefix that says a file in the directory is ours.
///
/// Load-bearing rather than cosmetic: [`prune`] deletes by it, and on a shared
/// `/tmp` anything without it belongs to somebody else.
const PREFIX: &str = "zesterm-paste-";

/// The largest clipboard image written to disk. Not a downscale — a refusal.
///
/// [`crate::background`] shrinks to `MAX_AXIS` because its destination is a
/// texture sampled onto a pane a fraction of the size, so the extra pixels are
/// detail no pixel can show. Here the destination is unknown: the paste may be
/// feeding an agent that resizes on its own side, or `open`, or an editor. A
/// terminal silently shrinking a screenshot before an editor opens it is a
/// wrong answer wearing success, so the only limit is one that bounds the disk.
const MAX_PIXELS: u64 = 64 << 20;

/// Files older than this go at the next paste. Long enough that a path somebody
/// left in their scrollback still opens tomorrow morning.
const MAX_AGE: Duration = Duration::from_secs(24 * 60 * 60);

/// …and at most this many, whatever their age, so that one busy afternoon
/// cannot fill a small `/tmp`.
const MAX_KEPT: usize = 32;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("the clipboard image has no pixels")]
    Empty,
    #[error("{pixels} pixels is more picture than a paste should write to disk")]
    TooLarge { pixels: u64 },
    #[error("clipboard image is {got} bytes, not the {expected} RGBA8 needs")]
    Malformed { expected: usize, got: usize },
    #[error("{0} holds a character a paste cannot carry")]
    Unpastable(PathBuf),
    #[error("encoding the PNG failed: {0}")]
    Encode(String),
    #[error(transparent)]
    Io(#[from] std::io::Error),
}

/// The whole gesture: clipboard pixels in, the text to paste out.
///
/// `None` is the old behaviour — nothing is written and nothing is sent —
/// reached now only when something actually went wrong, which is a `warn!`
/// rather than a banner because this app has no toast and a failed
/// [`crate::app::App::set_clipboard`] says nothing either.
pub fn text_for_image(rgba: &[u8], width: usize, height: usize) -> Option<String> {
    let dir = paste_dir()?;
    // Before the write, never after: pruning afterwards could count the file
    // that was just produced and delete it out from under the paste naming it.
    prune(&dir);

    let written = u32::try_from(width)
        .ok()
        .zip(u32::try_from(height).ok())
        .ok_or(Error::TooLarge { pixels: u64::MAX })
        .and_then(|(w, h)| write_png_into(&dir, rgba, w, h));

    match written.and_then(|path| Ok((escape_for_paste(&path)?, path))) {
        Ok((text, path)) => {
            tracing::debug!(path = %path.display(), width, height, "clipboard image written for paste");
            Some(text)
        }
        Err(e) => {
            tracing::warn!(error = %e, "clipboard image could not be pasted");
            None
        }
    }
}

/// This user's pasted-image directory, made private, or `None`.
fn paste_dir() -> Option<PathBuf> {
    // `temp_dir()` and not `$XDG_RUNTIME_DIR`, which the daemon's socket picks:
    // a socket is a few bytes and a screenshot is megabytes, and the runtime
    // directory is a tmpfs sized against RAM that other programs are also
    // living in. The privacy that would have bought is bought below instead.
    paste_dir_under(&std::env::temp_dir())
}

/// The testable half: `root` is a parameter so a test never touches the real
/// directory, where two tests running in parallel would prune each other.
fn paste_dir_under(root: &Path) -> Option<PathBuf> {
    #[cfg(unix)]
    let dir = root.join(format!("zesterm-{}", rustix::process::getuid().as_raw())).join("pasted-images");
    // Windows' `temp_dir()` is already inside the user's profile.
    #[cfg(not(unix))]
    let dir = root.join("zesterm").join("pasted-images");

    let mut builder = std::fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    {
        // The mode goes on the call that creates the directory, never through a
        // umask: umask is process-global and the victims of leaking one are a
        // crate away (#403). A chmod afterwards would be a window; this is not.
        use std::os::unix::fs::DirBuilderExt as _;
        builder.mode(0o700);
    }
    if let Err(e) = builder.create(&dir) {
        tracing::warn!(dir = %dir.display(), error = %e, "no directory to write a pasted image into");
        return None;
    }

    // `create` succeeds on a directory that was already there, and on unix
    // `/tmp` is shared — so a directory somebody else left behind, or a symlink
    // pointing somewhere they can read, would otherwise be written into. That
    // it is *ours* and private is the property worth checking, not that the
    // create returned Ok.
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt as _;
        use std::os::unix::fs::PermissionsExt as _;
        // `symlink_metadata`, so a symlink is judged as a symlink.
        let meta = std::fs::symlink_metadata(&dir).ok()?;
        let ours = meta.is_dir()
            && meta.uid() == rustix::process::getuid().as_raw()
            && meta.permissions().mode() & 0o077 == 0;
        if !ours {
            tracing::warn!(
                dir = %dir.display(),
                "not ours or not private; no picture will be pasted through it"
            );
            return None;
        }
    }
    Some(dir)
}

/// Write one clipboard image into `dir`; return the absolute path written.
fn write_png_into(dir: &Path, rgba: &[u8], width: u32, height: u32) -> Result<PathBuf, Error> {
    let png = encode_png(rgba, width, height)?;

    // Unique per process *and* per call, for `zest_config::save`'s reason: two
    // windows share this directory, and two pastes in the same millisecond
    // would otherwise pick the same name and one would truncate the other.
    // Zero-padded so the names sort chronologically, which is what makes
    // `prune`'s "newest wins" cheap to reason about.
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let millis = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map_or(0, |d| d.as_millis());
    let name = format!(
        "{PREFIX}{millis:013}-{}-{}.png",
        std::process::id(),
        NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    );

    // A sibling scratch file and a rename, `zest_config::save`'s pattern —
    // sibling because a rename is only atomic within one filesystem. Here it
    // buys a second thing: the reader on the other side of the paste opens the
    // file asynchronously, and a half-written PNG has valid magic bytes and a
    // failing decode, which is the wrong-answer-that-looks-like-success shape.
    let tmp = dir.join(format!(".{name}.tmp"));
    let written = (|| -> std::io::Result<()> {
        use std::io::Write as _;
        let mut opts = std::fs::OpenOptions::new();
        opts.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt as _;
            opts.mode(0o600);
        }
        let mut f = opts.open(&tmp)?;
        f.write_all(&png)?;
        f.sync_all()
    })();
    if let Err(e) = written {
        let _ = std::fs::remove_file(&tmp);
        return Err(e.into());
    }

    let path = dir.join(&name);
    if let Err(e) = std::fs::rename(&tmp, &path) {
        let _ = std::fs::remove_file(&tmp);
        return Err(e.into());
    }

    // Absolute, because a consumer that resolves the payload against its own
    // working directory is entitled to refuse a relative one — and because a
    // path pasted into a shell should mean the same thing wherever that shell
    // happens to be standing.
    std::path::absolute(&path).map_err(Into::into)
}

/// RGBA8 → PNG bytes. Pure: no clipboard, no filesystem, no clock.
fn encode_png(rgba: &[u8], width: u32, height: u32) -> Result<Vec<u8>, Error> {
    if width == 0 || height == 0 {
        return Err(Error::Empty);
    }
    let pixels = u64::from(width) * u64::from(height);
    // Guarded *before* the allocation, not after: the point of the limit is not
    // to notice a 4-gigapixel image afterwards.
    if pixels > MAX_PIXELS {
        return Err(Error::TooLarge { pixels });
    }
    // arboard promises RGBA8, but a promise from outside this process is an
    // assumption, and the alternative to checking is a panic in `from_raw`.
    let expected = usize::try_from(pixels * 4).map_err(|_| Error::TooLarge { pixels })?;
    if rgba.len() != expected {
        return Err(Error::Malformed { expected, got: rgba.len() });
    }

    let buf = image::RgbaImage::from_raw(width, height, rgba.to_vec())
        .ok_or(Error::Malformed { expected, got: rgba.len() })?;
    let mut out = Vec::new();
    // PNG named explicitly, for `--screenshot`'s reason: a format inferred from
    // an extension is a format nobody chose.
    image::DynamicImage::ImageRgba8(buf)
        .write_to(&mut std::io::Cursor::new(&mut out), image::ImageFormat::Png)
        .map_err(|e| Error::Encode(e.to_string()))?;
    Ok(out)
}

/// Delete our own stale files in `dir`.
///
/// Cannot fail, and says nothing when it does: tidying is not the gesture, and
/// a paste that worked must not be reported as broken because a file from
/// yesterday could not be removed.
fn prune(dir: &Path) {
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    let now = SystemTime::now();
    let mut ours: Vec<(SystemTime, PathBuf)> = Vec::new();

    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        // The scratch file of a write that crashed is ours too, and would
        // otherwise be the one thing in here that lives forever.
        let mine = (name.starts_with(PREFIX) && name.ends_with(".png"))
            || (name.strip_prefix('.').is_some_and(|n| n.starts_with(PREFIX)) && name.ends_with(".tmp"));
        if !mine {
            continue;
        }
        let Ok(meta) = entry.metadata() else { continue };
        if !meta.is_file() {
            continue;
        }
        let modified = meta.modified().unwrap_or(now);
        if now.duration_since(modified).is_ok_and(|age| age > MAX_AGE) {
            let _ = std::fs::remove_file(entry.path());
            continue;
        }
        ours.push((modified, entry.path()));
    }

    if ours.len() > MAX_KEPT {
        ours.sort_unstable_by(|a, b| b.0.cmp(&a.0)); // newest first
        for (_, path) in &ours[MAX_KEPT..] {
            let _ = std::fs::remove_file(path);
        }
    }
}

/// The path as the program on the other side of the pty must read it.
///
/// We own the filename, so in practice this only ever has the temp *root* to
/// worry about — but a root with a space in it is exactly the case that would
/// otherwise ship broken on somebody else's machine.
#[cfg(unix)]
fn escape_for_paste(path: &Path) -> Result<String, Error> {
    let text = path.to_string_lossy();
    if text.chars().any(char::is_control) {
        // A newline or a tab can never round-trip: a shell splits on one and a
        // reader of pasted paths splits on the other. Refusing is a log line;
        // emitting it is a payload that silently means something else.
        return Err(Error::Unpastable(path.to_path_buf()));
    }
    // A leading backslash on every ASCII character outside a known-inert set.
    // `\<char>` → `<char>` is what a POSIX shell does and what the agents that
    // read pasted paths do, so it is the one spelling both un-escape alike.
    // Non-ASCII is left alone: a shell splits on IFS and metacharacters, not on
    // `é`, and backslashing it only makes the payload unreadable.
    const INERT: &str = "/._-+@%,=:";
    let mut out = String::with_capacity(text.len());
    for ch in text.chars() {
        if ch.is_ascii() && !ch.is_ascii_alphanumeric() && !INERT.contains(ch) {
            out.push('\\');
        }
        out.push(ch);
    }
    Ok(out)
}

/// Verbatim on Windows — and this is not laziness.
///
/// The separator *is* the escape character there. `C:\Users\A B\x.png` escaped
/// POSIX-style becomes `C:\Users\A\ B\x.png`, and anything that un-escapes
/// `\<char>` → `<char>` reads that as `C:UsersA Bx.png`: every separator eaten.
/// Neither `cmd` nor PowerShell uses backslash escaping, so there is nothing to
/// escape *for*. A Windows temp root with a space in it stays imperfect for the
/// shell case; corrupting the path to fix it would be worse.
#[cfg(not(unix))]
fn escape_for_paste(path: &Path) -> Result<String, Error> {
    let text = path.to_string_lossy();
    if text.chars().any(char::is_control) {
        return Err(Error::Unpastable(path.to_path_buf()));
    }
    Ok(text.into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A scratch directory of our own. Never [`paste_dir`]: two tests sharing
    /// the real one would prune each other's files and fail in whichever order
    /// libtest happened to pick.
    fn scratch(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir()
            .join(format!("zesterm-paste-test-{}-{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("scratch dir");
        dir
    }

    /// Three by two, so a transposed width and height cannot pass.
    fn pixels() -> (Vec<u8>, u32, u32) {
        let mut rgba = Vec::new();
        for i in 0..6u8 {
            rgba.extend_from_slice(&[i * 40, 255 - i * 40, i, 255]);
        }
        (rgba, 3, 2)
    }

    #[test]
    fn the_written_file_is_a_png_by_its_magic_bytes() {
        let dir = scratch("magic");
        let (rgba, w, h) = pixels();
        let path = write_png_into(&dir, &rgba, w, h).expect("write");

        // This is the sniff every consumer of a pasted path performs, run in
        // our own process: a file with an image extension whose *content* is
        // not an image is refused on the other side, silently.
        let reader = image::ImageReader::open(&path)
            .expect("open")
            .with_guessed_format()
            .expect("guess");
        assert_eq!(
            reader.format(),
            Some(image::ImageFormat::Png),
            "a pasted path is read by its magic bytes, not its extension"
        );

        let decoded = reader.decode().expect("decode").to_rgba8();
        assert_eq!((decoded.width(), decoded.height()), (w, h), "the picture must survive the round trip");
        assert_eq!(decoded.as_raw().as_slice(), rgba.as_slice(), "PNG is lossless; the pixels must match exactly");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_returned_path_is_absolute() {
        let dir = scratch("absolute");
        let (rgba, w, h) = pixels();
        let path = write_png_into(&dir, &rgba, w, h).expect("write");
        assert!(
            path.is_absolute(),
            "a consumer resolves a relative payload against its own working directory, \
             which is not the one the paste came from"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_file_name_has_nothing_a_shell_would_touch() {
        let dir = scratch("name");
        let (rgba, w, h) = pixels();
        let path = write_png_into(&dir, &rgba, w, h).expect("write");
        let name = path.file_name().expect("name").to_string_lossy().into_owned();
        assert!(
            name.chars().all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '-'),
            "we choose this name, so it must never be the reason a paste needs quoting: {name}"
        );
        assert!(name.starts_with(PREFIX), "prune deletes by this prefix, so a name without it lives forever");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn two_pastes_in_the_same_millisecond_do_not_collide() {
        let dir = scratch("collide");
        let (rgba, w, h) = pixels();
        let first = write_png_into(&dir, &rgba, w, h).expect("first");
        let second = write_png_into(&dir, &rgba, w, h).expect("second");
        assert_ne!(first, second, "two windows share this directory; a shared name is one paste truncating the other");
        assert!(first.exists() && second.exists(), "both files must still be there");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_buffer_that_is_not_four_bytes_a_pixel_is_refused() {
        let dir = scratch("malformed");
        // arboard promises RGBA8; the alternative to checking is a panic.
        let err = write_png_into(&dir, &[0, 0, 0], 3, 2).expect_err("must refuse");
        assert!(matches!(err, Error::Malformed { .. }), "got {err:?}");
        assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 0, "a refusal must leave no scratch file behind");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_zero_sized_image_is_refused() {
        let err = encode_png(&[], 0, 0).expect_err("must refuse");
        assert!(matches!(err, Error::Empty), "got {err:?}");
    }

    #[test]
    fn an_absurd_image_is_refused_before_it_is_allocated() {
        // The point of the limit is not to notice afterwards.
        let err = encode_png(&[], 100_000, 100_000).expect_err("must refuse");
        assert!(matches!(err, Error::TooLarge { .. }), "got {err:?}");
    }

    #[test]
    fn a_path_a_paste_cannot_survive_is_refused() {
        let path = PathBuf::from(format!("{}x{}y.png", main_separator_root(), '\n'));
        let err = escape_for_paste(&path).expect_err("must refuse");
        assert!(
            matches!(err, Error::Unpastable(_)),
            "a newline in the payload is read as the end of the path, so emitting it means something else: {err:?}"
        );
    }

    fn main_separator_root() -> String {
        if cfg!(unix) { "/tmp/".into() } else { "C:\\Temp\\".into() }
    }

    /// The consumer's half of the contract, mirrored here so the assertions are
    /// round trips rather than literals somebody has to eyeball. A POSIX shell
    /// and every agent that reads pasted paths both implement exactly this.
    #[cfg(unix)]
    fn unescape(text: &str) -> String {
        let mut out = String::with_capacity(text.len());
        let mut chars = text.chars();
        while let Some(ch) = chars.next() {
            if ch == '\\' {
                if let Some(next) = chars.next() {
                    out.push(next);
                }
            } else {
                out.push(ch);
            }
        }
        out
    }

    /// The one link no unit test can reach: a real picture, on a real
    /// clipboard, through the real directory.
    ///
    /// Ignored for `mesh_probe`'s reason -- CI has no clipboard, and a machine
    /// with nothing copied answers exactly like one that cannot read pictures,
    /// so as a gate it would be noise. Run it by hand after copying an image:
    ///
    /// ```text
    /// cargo test -p zest-app -- --ignored a_real_clipboard_picture
    /// ```
    #[test]
    #[ignore = "needs a picture on this machine's clipboard"]
    fn a_real_clipboard_picture_becomes_a_path_that_opens() {
        let mut clipboard = arboard::Clipboard::new().expect("a clipboard");
        let image = clipboard.get_image().expect("copy a picture first");
        let text = text_for_image(&image.bytes, image.width, image.height)
            .expect("a picture on the clipboard must produce something to paste");

        #[cfg(unix)]
        let path = PathBuf::from(unescape(&text));
        #[cfg(not(unix))]
        let path = PathBuf::from(&text);

        let decoded = image::ImageReader::open(&path)
            .expect("the pasted path must name a file")
            .with_guessed_format()
            .expect("guess")
            .decode()
            .expect("and that file must be a picture");
        assert_eq!(
            (decoded.width() as usize, decoded.height() as usize),
            (image.width, image.height),
            "the file the paste names must be the picture that was copied"
        );
        println!("pasted: {text}");
    }

    #[cfg(unix)]
    mod unix {
        use super::*;

        #[test]
        fn escaping_leaves_an_ordinary_path_alone() {
            let path = Path::new("/tmp/zesterm-501/pasted-images/zesterm-paste-1.png");
            assert_eq!(
                escape_for_paste(path).unwrap(),
                path.to_str().unwrap(),
                "the common case must not be dressed up in backslashes nobody needs"
            );
        }

        #[test]
        fn a_space_in_the_temp_root_is_escaped_and_un_escapes_to_itself() {
            let path = Path::new("/tmp/a b/zesterm-paste-1.png");
            let escaped = escape_for_paste(path).unwrap();
            assert_eq!(escaped, "/tmp/a\\ b/zesterm-paste-1.png");
            assert_eq!(
                unescape(&escaped),
                path.to_str().unwrap(),
                "the round trip is the assertion; the literal above is only the illustration"
            );
        }

        #[test]
        fn every_shell_metacharacter_in_the_root_is_escaped() {
            for meta in ['$', '`', '"', '\'', ';', '&', '(', ')', '*', '?', '[', ']', '{', '}', '!', '#', '~', '<', '>', '|', '\\', ' '] {
                let path = PathBuf::from(format!("/tmp/a{meta}b/zesterm-paste-1.png"));
                let escaped = escape_for_paste(&path).unwrap();
                assert!(
                    escaped.contains(&format!("a\\{meta}b")),
                    "{meta:?} reaches a shell as syntax unless it is escaped: {escaped}"
                );
                assert_eq!(unescape(&escaped), path.to_str().unwrap(), "and it must still un-escape to the path");
            }
        }

        #[test]
        fn a_private_dir_is_created_with_no_group_or_other_bits() {
            use std::os::unix::fs::PermissionsExt as _;
            let root = scratch("private");
            let dir = paste_dir_under(&root).expect("dir");
            let mode = std::fs::metadata(&dir).unwrap().permissions().mode();
            assert_eq!(mode & 0o077, 0, "a screenshot is not for the rest of the machine to read: {mode:o}");
            let _ = std::fs::remove_dir_all(&root);
        }

        #[test]
        fn a_world_writable_directory_someone_else_left_is_refused() {
            use std::os::unix::fs::PermissionsExt as _;
            let root = scratch("squatted");
            // `create` succeeds on a directory that is already there, so the
            // mode on the create proves nothing about the one we end up with.
            let dir = root.join(format!("zesterm-{}", rustix::process::getuid().as_raw())).join("pasted-images");
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o777)).unwrap();
            assert!(
                paste_dir_under(&root).is_none(),
                "on a shared /tmp, a directory anyone can write is one anyone can swap a file in"
            );
            let _ = std::fs::remove_dir_all(&root);
        }
    }

    #[cfg(not(unix))]
    #[test]
    fn a_windows_path_is_emitted_verbatim() {
        let path = Path::new(r"C:\Users\a b\zesterm-paste-1.png");
        assert_eq!(
            escape_for_paste(path).unwrap(),
            r"C:\Users\a b\zesterm-paste-1.png",
            "the separator is the escape character here; escaping it eats the path"
        );
    }

    fn age(path: &Path, by: Duration) {
        let when = SystemTime::now() - by;
        std::fs::File::options()
            .write(true)
            .open(path)
            .expect("open to set times")
            .set_times(std::fs::FileTimes::new().set_modified(when))
            .expect("set times");
    }

    #[test]
    fn pruning_removes_only_our_own_stale_files() {
        let dir = scratch("prune");
        let fresh = dir.join(format!("{PREFIX}0000000000001-1-0.png"));
        let stale = dir.join(format!("{PREFIX}0000000000002-1-0.png"));
        let orphan = dir.join(format!(".{PREFIX}0000000000003-1-0.png.tmp"));
        let stranger = dir.join("notes.txt");
        for path in [&fresh, &stale, &orphan, &stranger] {
            std::fs::write(path, b"x").unwrap();
        }
        std::fs::create_dir(dir.join(format!("{PREFIX}a-directory.png"))).unwrap();
        age(&stale, MAX_AGE + Duration::from_secs(60));
        age(&orphan, MAX_AGE + Duration::from_secs(60));

        prune(&dir);

        assert!(fresh.exists(), "a file from this morning is still worth the path somebody kept");
        assert!(!stale.exists(), "yesterday's is not");
        assert!(!orphan.exists(), "the scratch file of a crashed write is ours too, and lives forever otherwise");
        assert!(stranger.exists(), "on a shared /tmp, anything without our prefix belongs to somebody else");
        assert!(dir.join(format!("{PREFIX}a-directory.png")).exists(), "prune removes files, not directories");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn pruning_keeps_at_most_the_cap_and_keeps_the_newest() {
        let dir = scratch("cap");
        let mut newest = None;
        for i in 0..MAX_KEPT + 3 {
            let path = dir.join(format!("{PREFIX}{i:013}-1-0.png"));
            std::fs::write(&path, b"x").unwrap();
            // Descending age, so the last one written is the newest.
            age(&path, Duration::from_secs((MAX_KEPT + 3 - i) as u64 * 60));
            newest = Some(path);
        }

        prune(&dir);

        let left = std::fs::read_dir(&dir).unwrap().count();
        assert_eq!(left, MAX_KEPT, "one busy afternoon must not fill a small /tmp");
        assert!(newest.unwrap().exists(), "and the one just pasted is the last that may go");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn pruning_a_directory_that_is_not_there_is_not_an_error() {
        // Tidying is not the gesture: a paste must not fail because of it.
        prune(&std::env::temp_dir().join("zesterm-paste-test-absent-directory"));
    }
}
