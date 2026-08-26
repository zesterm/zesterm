//! A file open in a pane (#464).
//!
//! Read-only in this first cut: the caret, undo and ⌘S are their own work, and
//! a viewer earns its place without them — most of the time a path in a
//! traceback is something you want to *look* at.
//!
//! # Why a pane and not a screen
//!
//! A tab already holds any number of panes on any number of hosts (#438), each
//! with its own frame, header, focus and wheel target, so a file in one costs
//! no new layout concept. A full-pane screen would be a single enum-valued
//! field — which cannot express two files at once — and would hide the
//! terminal whose output made you open the file.
//!
//! # Where the bytes come from
//!
//! Never from this process's own disk unless the session runs here. The file
//! lives on the *session's* host, so the read is
//! [`SessionSource::request_file`](crate::source::SessionSource::request_file)
//! and the answer arrives as a wakeup — the same shape #449's directory
//! browser uses, for the same reason: a viewer that silently showed the local
//! file for a remote tab would be worse than one that refused.

use zest_proto::SessionAddr;

/// How far a line is allowed to be scrolled horizontally, as a multiple of the
/// widest line. Past its own end there is nothing to see, and a viewport that
/// can be dragged into empty space reads as a rendering bug.
const OVERSCROLL_COLS: f32 = 2.0;

/// What the pane knows about the file it was pointed at.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LoadState {
    /// The question is on the wire (or on a worker, for a local host).
    Loading,
    Ready,
    /// The host said why it could not be read; shown verbatim, because the
    /// message is the person's next move.
    Failed(String),
}

/// One file, open in one pane.
pub struct EditorPane {
    /// The pane's own address, so it is a distinct hit region and the
    /// persistence layer skips it — see [`crate::tabs::placeholder_addr`].
    pub addr: SessionAddr,
    /// The session whose host holds the file. A pane is not a session, so the
    /// reads are routed through this one; it is also what a later save uses.
    pub origin: SessionAddr,
    /// What was asked for — the correlation for the reply, since the wire has
    /// no request id (#446).
    pub asked: String,
    /// The directory a relative `asked` resolves against, on that host.
    pub cwd: String,
    /// The resolved path, once the host has answered. Disk truth, and what the
    /// header shows: `asked` went through a shell-reported cwd, which anything
    /// that can print can forge.
    pub path: String,
    pub state: LoadState,
    lines: Vec<String>,
    /// Bytes on the host's disk, which is not `lines`' length when truncated.
    pub size: u64,
    /// More file existed than arrived. The pane says so, and a later save is
    /// refused by construction — the host hands back no base hash for a
    /// truncated read, and an empty base means "create, refuse if it exists".
    pub truncated: bool,
    /// A NUL early in the file. Rendered as a refusal rather than as mojibake.
    pub binary: bool,
    pub readonly: bool,
    /// The base a save would be checked against. Empty when there is none.
    pub hash: String,
    /// First visible line.
    pub scroll_line: usize,
    /// Horizontal offset in pixels; a file is not wrapped (see the module doc
    /// on the gutter in `chrome::editor`).
    pub scroll_x: f32,
    /// The widest line, in columns, cached because the scroll clamp needs it
    /// every frame and a long file should not be re-measured for it.
    widest_cols: usize,
}

impl EditorPane {
    /// A pane waiting for its first answer.
    pub fn loading(addr: SessionAddr, origin: SessionAddr, asked: &str, cwd: &str) -> Self {
        Self {
            addr,
            origin,
            asked: asked.to_string(),
            cwd: cwd.to_string(),
            path: asked.to_string(),
            state: LoadState::Loading,
            lines: Vec::new(),
            size: 0,
            truncated: false,
            binary: false,
            readonly: false,
            hash: String::new(),
            scroll_line: 0,
            scroll_x: 0.0,
            widest_cols: 0,
        }
    }

    /// Fill from a host's answer.
    ///
    /// Splitting on `\n` and dropping a trailing `\r` handles both line
    /// endings without a mode: a CRLF file read on a Mac should not show a
    /// stray glyph at every line end, and the ending a save would restore is a
    /// question for the PR that can save.
    pub fn apply(&mut self, reply: FileReply) {
        if !reply.error.is_empty() {
            self.state = LoadState::Failed(reply.error);
            return;
        }
        self.path = reply.path;
        self.size = reply.size;
        self.truncated = reply.truncated;
        self.binary = reply.binary;
        // A truncated read can never be the base for a save, so the pane is
        // read-only whatever the file's permissions say. Saying it here, once,
        // keeps every later caller from having to remember the wire's rule.
        self.readonly = reply.readonly || reply.truncated;
        self.hash = reply.hash;
        self.lines = if reply.binary {
            Vec::new()
        } else {
            let text = String::from_utf8_lossy(&reply.data);
            if text.is_empty() {
                // `"".split('\n')` is one empty piece, not none — so without
                // this an empty file shows a blank first line and a gutter
                // numbered `1`, which is a file that does not exist.
                Vec::new()
            } else {
                let mut lines: Vec<String> = text
                    .split('\n')
                    .map(|l| l.strip_suffix('\r').unwrap_or(l).to_string())
                    .collect();
                // A file ending in a newline splits into a final empty piece
                // that is not a line of the file; one that does not, does not.
                // Dropping it unconditionally would lose the last line of a
                // file with no trailing newline.
                if text.ends_with('\n') {
                    lines.pop();
                }
                lines
            }
        };
        self.widest_cols = self.lines.iter().map(|l| display_cols(l)).max().unwrap_or(0);
        self.state = LoadState::Ready;
    }

    #[must_use]
    pub fn lines(&self) -> &[String] {
        &self.lines
    }

    #[must_use]
    pub fn line_count(&self) -> usize {
        self.lines.len()
    }

    /// The file's name, for the pane header.
    #[must_use]
    pub fn title(&self) -> &str {
        let p = if self.path.is_empty() { &self.asked } else { &self.path };
        p.rsplit(['/', '\\']).next().filter(|s| !s.is_empty()).unwrap_or(p)
    }

    /// The directory the file sits in, for the header's second line.
    #[must_use]
    pub fn dir(&self) -> &str {
        let p = if self.path.is_empty() { &self.asked } else { &self.path };
        match p.rfind(['/', '\\']) {
            Some(i) if i > 0 => &p[..i],
            Some(_) => "/",
            None => "",
        }
    }

    /// Scroll by whole lines, clamped so the last line stays on screen.
    ///
    /// `visible` is how many lines the body can show. Clamping to
    /// `len - visible` rather than to `len` is what stops a wheel flick
    /// leaving an empty pane with the file above it.
    pub fn scroll_by(&mut self, lines: f32, visible: usize) {
        let max = self.lines.len().saturating_sub(visible);
        let next = (self.scroll_line as f32 - lines).round();
        self.scroll_line = next.clamp(0.0, max as f32) as usize;
    }

    /// Scroll horizontally, clamped to the widest line plus a little slack.
    pub fn scroll_x_by(&mut self, dx: f32, cell_w: f32, body_w: f32) {
        let content = (self.widest_cols as f32 + OVERSCROLL_COLS) * cell_w;
        let max = (content - body_w).max(0.0);
        self.scroll_x = (self.scroll_x - dx).clamp(0.0, max);
    }

}

/// A host's answer to "read this file", flattened out of the wire message so
/// the app has one shape whether it came over a socket or off the local disk
/// (#434: a window hosting its own session reads its own filesystem).
#[derive(Debug, Clone, Default)]
pub struct FileReply {
    pub path: String,
    pub data: Vec<u8>,
    pub truncated: bool,
    pub binary: bool,
    pub hash: String,
    pub size: u64,
    pub readonly: bool,
    pub error: String,
}

impl FileReply {
    /// Unpack the wire's `FileContents`, or `None` for any other message.
    #[must_use]
    pub fn from_host(msg: zest_proto::HostMessage) -> Option<Self> {
        match msg {
            zest_proto::HostMessage::FileContents {
                path,
                data,
                truncated,
                binary,
                hash,
                size,
                readonly,
                error,
            } => Some(Self { path, data, truncated, binary, hash, size, readonly, error }),
            _ => None,
        }
    }
}

/// Columns a line occupies, counting a tab to the next stop and an East Asian
/// wide character as two — the same width rule the grid uses, so a file and a
/// terminal beside it line up.
#[must_use]
pub fn display_cols(line: &str) -> usize {
    use unicode_width::UnicodeWidthChar as _;
    let mut cols = 0;
    for ch in line.chars() {
        cols += if ch == '\t' { TAB_STOP - (cols % TAB_STOP) } else { ch.width().unwrap_or(0) };
    }
    cols
}

/// Where a tab lands. Eight, because that is what a terminal does and this
/// pane sits beside one; a file whose indentation disagrees with `cat` of the
/// same file would be its own small betrayal.
pub const TAB_STOP: usize = 8;

/// `line` with tabs expanded to spaces, for drawing.
///
/// Expanded rather than measured-around because the chrome's text runs are
/// shaped strings, not cells: a literal tab would be shaped by the font, which
/// gives it whatever advance the face happens to carry (often zero).
#[must_use]
pub fn expand_tabs(line: &str) -> std::borrow::Cow<'_, str> {
    if !line.contains('\t') {
        return std::borrow::Cow::Borrowed(line);
    }
    use unicode_width::UnicodeWidthChar as _;
    let mut out = String::with_capacity(line.len() + TAB_STOP);
    let mut cols = 0;
    for ch in line.chars() {
        if ch == '\t' {
            let pad = TAB_STOP - (cols % TAB_STOP);
            out.extend(std::iter::repeat_n(' ', pad));
            cols += pad;
        } else {
            out.push(ch);
            cols += ch.width().unwrap_or(0);
        }
    }
    std::borrow::Cow::Owned(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pane() -> EditorPane {
        EditorPane::loading(
            crate::tabs::placeholder_addr(1),
            crate::tabs::placeholder_addr(2),
            "src/main.rs",
            "/repo",
        )
    }

    fn reply(body: &str) -> FileReply {
        FileReply {
            path: "/repo/src/main.rs".into(),
            data: body.as_bytes().to_vec(),
            size: body.len() as u64,
            hash: "abc".into(),
            ..Default::default()
        }
    }

    #[test]
    fn a_file_with_no_trailing_newline_keeps_its_last_line() {
        // The off-by-one that a `split('\n')` invites in both directions: a
        // file ending in a newline splits into a final empty piece that is not
        // a line, and one that does not must not lose its last.
        let mut p = pane();
        p.apply(reply("one\ntwo\n"));
        assert_eq!(p.lines(), ["one".to_string(), "two".to_string()]);

        let mut p = pane();
        p.apply(reply("one\ntwo"));
        assert_eq!(p.lines(), ["one".to_string(), "two".to_string()]);

        // And an empty file is empty, not one blank line.
        let mut p = pane();
        p.apply(reply(""));
        assert_eq!(p.line_count(), 0);
    }

    #[test]
    fn crlf_does_not_leave_a_glyph_at_every_line_end() {
        let mut p = pane();
        p.apply(reply("one\r\ntwo\r\n"));
        assert_eq!(p.lines(), ["one".to_string(), "two".to_string()]);
    }

    #[test]
    fn a_truncated_read_is_read_only_whatever_the_permissions_say() {
        // The wire hands back no hash for a truncated read, so a save could
        // never succeed anyway; the pane says so up front rather than letting
        // someone type into a buffer that cannot be written.
        let mut p = pane();
        p.apply(FileReply { truncated: true, hash: String::new(), ..reply("half a file\n") });
        assert!(p.readonly, "a truncated read cannot be a save's base");
        assert!(p.truncated);
    }

    #[test]
    fn a_refusal_is_kept_verbatim_and_leaves_no_content() {
        let mut p = pane();
        p.apply(FileReply { error: "that is a directory".into(), ..Default::default() });
        assert_eq!(p.state, LoadState::Failed("that is a directory".into()));
        assert_eq!(p.line_count(), 0);
    }

    #[test]
    fn a_binary_file_is_refused_rather_than_rendered_as_mojibake() {
        let mut p = pane();
        p.apply(FileReply { binary: true, ..reply("\u{0}\u{1}\u{2}") });
        assert!(p.binary);
        assert_eq!(p.line_count(), 0, "the bytes arrived, but they are not lines");
    }

    #[test]
    fn scrolling_stops_with_the_last_line_in_view() {
        let mut p = pane();
        p.apply(reply("1\n2\n3\n4\n5\n6\n7\n8\n9\n10\n"));

        // Ten lines, six visible: the furthest down is line five at the top,
        // never line ten with five blank rows under it.
        p.scroll_by(-100.0, 6);
        assert_eq!(p.scroll_line, 4);

        p.scroll_by(100.0, 6);
        assert_eq!(p.scroll_line, 0, "and it stops at the top too");

        // A file shorter than the body does not scroll at all.
        let mut p = pane();
        p.apply(reply("1\n2\n"));
        p.scroll_by(-10.0, 6);
        assert_eq!(p.scroll_line, 0);
    }


    #[test]
    fn a_tab_reaches_the_next_stop_rather_than_counting_as_one_column() {
        assert_eq!(display_cols("\tx"), TAB_STOP + 1);
        assert_eq!(display_cols("ab\tx"), TAB_STOP + 1);
        // Exactly on a stop: a full tab, not zero.
        assert_eq!(display_cols(&format!("{}\tx", "y".repeat(TAB_STOP))), TAB_STOP * 2 + 1);
        // A wide character is two columns, as it is in the grid beside it.
        assert_eq!(display_cols("漢字"), 4);

        assert_eq!(expand_tabs("ab\tx"), format!("ab{}x", " ".repeat(TAB_STOP - 2)));
        assert_eq!(expand_tabs("plain"), "plain", "a line with no tab is not copied");
    }

    #[test]
    fn the_header_names_the_file_and_its_directory() {
        let mut p = pane();
        p.apply(reply("x\n"));
        assert_eq!(p.title(), "main.rs");
        assert_eq!(p.dir(), "/repo/src");

        // Before an answer arrives the header still has to say something, and
        // what was asked for is the only thing there is.
        let p = pane();
        assert_eq!(p.title(), "main.rs");
    }
}
