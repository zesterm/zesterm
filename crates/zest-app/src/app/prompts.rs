//! The find bar, the open-file prompt, the directory picker -- and the replies
//! they are waiting on.
//!
//! Moved out of `app/mod.rs` (#554). One module rather than three because what
//! these share is their shape, not their subject: each puts an overlay over the
//! grid, asks something of a session that may be on another machine, and has to
//! still be open -- and still mean the same thing -- when the answer arrives.
//!
//! Their state stays on the parent: `find`, `find_state`, `dir_picker` and
//! `open_file` are `App` fields that the chrome and the keyboard paths read.
//!
//! `wakeup_sender` stayed behind deliberately. It is how *any* worker posts
//! back to the loop, not something these own.

use super::*;

impl App {
    /// Open the cwd chip's directory browser (#439) on `path`.
    pub(super) fn open_dir_picker(&mut self, path: String) {
        // The overlays are mutually exclusive — two panels floating over one
        // grid is two things claiming the keyboard.
        self.picker = None;
        self.palette_ui = None;
        self.launcher = None;
        self.block_menu = None;
        self.dir_picker = Some(DirPickerState {
            path: path.clone(),
            parent: None,
            dirs: Vec::new(),
            loading: true,
            truncated: false,
            error: String::new(),
            filter: TextField::default(),
            selected: 0,
            scroll: 0.0,
            scroll_to_selected: true,
            rows: Vec::new(),
        });
        self.request_dir_listing(path);
        self.mark_chrome_dirty();
    }

    /// Move the open browser to another directory and ask again.
    fn dir_picker_navigate(&mut self, path: String) {
        let Some(p) = self.dir_picker.as_mut() else { return };
        p.path = path.clone();
        p.parent = None;
        p.dirs.clear();
        p.loading = true;
        p.truncated = false;
        p.error.clear();
        p.filter = TextField::default();
        p.selected = 0;
        p.scroll = 0.0;
        p.scroll_to_selected = true;
        self.request_dir_listing(path);
        self.mark_chrome_dirty();
    }

    /// Ask whoever can answer what `path` holds.
    ///
    /// A daemon-backed session is asked over its own wire and answers with
    /// `Wakeup::DirListingReady`; an in-process one is answered on the spot
    /// — the window is its host (#434), so the daemon's own lister runs
    /// here, and both paths produce one shape.
    fn request_dir_listing(&mut self, path: String) {
        let Some(source) = self.tabs.active_source() else { return };
        // Asked first, then the origin decides whether anything may stand in
        // for the answer. Belt and braces on purpose: `request_dirs` says it
        // took the question, and this says only an in-process session may be
        // answered from this machine's disk — so no source returning the
        // wrong bool can make a remote tab list the wrong computer.
        let asked = source.request_dirs(&path);
        let in_process = matches!(source.origin(), crate::source::Origin::InProcess);
        if asked || !in_process {
            return;
        }
        if let zest_proto::HostMessage::DirListing { path, parent, dirs, truncated, error } =
            zest_daemon::server::list_dir(&path)
        {
            self.apply_dir_listing(crate::session::DirListing {
                path,
                parent,
                dirs,
                truncated,
                error,
            });
        }
    }

    /// Open the find bar, or re-select its query if it is already up (⌘F).
    ///
    /// Seeds from a single-line selection, the browsers' convention: the common
    /// reason to select a word and press ⌘F is to look for that word. A
    /// multi-line selection is not a search term and is ignored rather than
    /// flattened into one.
    ///
    /// Not exclusive with the other overlays, unlike `open_file_prompt`:
    /// searching is something you do *to* the grid you are looking at.
    pub(super) fn toggle_find(&mut self) {
        let seed = self
            .tabs
            .active()
            .and_then(|t| t.focused_session().or_else(|| Some(t.source())))
            .and_then(|s| s.terminal().lock().selection_text())
            .filter(|t| !t.is_empty() && !t.contains('\n'));

        let mut field = self.find.take().unwrap_or_default();
        if let Some(seed) = seed.filter(|_| field.text().is_empty()) {
            field.set(seed);
        }
        // Select all so the next keystroke replaces: ⌘F on an open bar means
        // "search for something else", never "append to what is there".
        field.select_all();
        self.find = Some(field);
        self.run_find();
        // Start the pull on the way in: the first page is on the wire while
        // the reader is still typing, rather than after it.
        self.pump_find_history();
        self.mark_chrome_dirty();
        if let Some(w) = self.window.as_ref() {
            w.request_redraw();
        }
    }

    /// Close the find bar, leaving the grid's own selection alone.
    ///
    /// Design §3 forbids two things lit at once, so the hits go and whatever
    /// was selected before stays selected — Escape out of a search must not
    /// also undo a drag.
    pub(super) fn close_find(&mut self) {
        self.find = None;
        self.find_state = crate::find::FindState::default();
        self.mark_chrome_dirty();
        if let Some(w) = self.window.as_ref() {
            w.request_redraw();
        }
    }

    /// Re-scan the focused pane for the current query.
    pub(super) fn run_find(&mut self) {
        let Some(field) = self.find.as_ref() else { return };
        let needle = field.text().to_string();
        let Some(tab) = self.tabs.active() else {
            self.find_state = crate::find::FindState::default();
            return;
        };
        let session = tab.focused_session().unwrap_or_else(|| tab.source());
        let query = zest_core::search::Query::smart(needle);
        let (found, near) = {
            let term = session.terminal();
            let term = term.lock();
            let found = term.grid().search(&query, zest_core::search::DEFAULT_MATCH_LIMIT);
            (found, term.grid().line_id_at(0))
        };
        self.find_state.case_sensitive = query.case_sensitive;
        self.find_state.accept(found, near);
        self.reveal_find_hit();
    }

    /// Pull another page of the focused pane's history, and record whether
    /// one is on the wire (#545).
    ///
    /// A replica holds only what crossed the wire since it attached, so
    /// without this ⌘F searches the screen and whatever has scrolled since —
    /// which is what a session looks like right after a reattach. Driven per
    /// frame while the bar is open: each page marks the grid dirty, which
    /// re-runs the search and asks for the next, so the count climbs as the
    /// history lands and stops when the host has no more.
    pub(super) fn pump_find_history(&mut self) {
        let state = {
            let Some(tab) = self.tabs.active() else { return };
            let session = tab.focused_session().unwrap_or_else(|| tab.source());
            session.backfill_history()
        };
        self.find_state.fetching = matches!(state, crate::source::HistoryState::Fetching);
    }

    /// Step to the next or previous hit and bring it into view.
    pub(super) fn step_find(&mut self, delta: isize) {
        self.find_state.step(delta);
        self.reveal_find_hit();
        self.mark_chrome_dirty();
        if let Some(w) = self.window.as_ref() {
            w.request_redraw();
        }
    }

    /// Scroll the focused pane so the current hit is on screen.
    fn reveal_find_hit(&mut self) {
        let Some(line) = self.find_state.selected().map(|m| m.start.line) else { return };
        let Some(tab) = self.tabs.active() else { return };
        let session = tab.focused_session().unwrap_or_else(|| tab.source());
        let mut term = session.terminal().lock();
        term.scroll_to_line(line);
    }

    /// The find bar as the layout pass wants it.
    pub(super) fn find_model(&self) -> Option<crate::chrome::model::FindBarModel> {
        let field = self.find.as_ref()?;
        Some(crate::chrome::model::FindBarModel {
            query: field.text().to_string(),
            caret: crate::chrome::model::Caret {
                at: field.caret(),
                selection: field.selection(),
            },
            count: self.find_state.count_label(field.text().is_empty()),
            empty: self.find_state.hits.is_empty() && !field.text().is_empty(),
            case_sensitive: self.find_state.case_sensitive,
            fetching_history: self.find_state.fetching,
        })
    }

    /// Open the "Open file…" prompt (#464).
    ///
    /// Exclusive with the other overlays, like every one of them: the app
    /// enforces at most one open, which is what lets `layout` never rank them.
    pub(super) fn open_file_prompt(&mut self) {
        self.dir_picker = None;
        self.block_menu = None;
        self.open_file = Some(TextField::default());
        self.mark_chrome_dirty();
        if let Some(w) = self.window.as_ref() {
            w.request_redraw();
        }
    }

    /// The prompt as the layout pass wants it, with where a relative path
    /// would land.
    pub(super) fn open_file_model(&self) -> Option<crate::chrome::model::OpenFileModel> {
        let field = self.open_file.as_ref()?;
        let (cwd, host) = self.tabs.active().map_or_else(
            || (String::new(), "this machine".to_string()),
            |tab| {
                let session = tab.focused_session().unwrap_or_else(|| tab.source());
                let host = match session.origin() {
                    crate::source::Origin::Daemon { host, local: false } => host,
                    _ => "this machine".to_string(),
                };
                let term = session.terminal();
                let term = term.lock();
                let cwd = if term.cwd().is_empty() {
                    term.blocks().last().map(|b| b.cwd.clone()).unwrap_or_default()
                } else {
                    term.cwd().to_string()
                };
                (crate::status::shorten_home(&cwd), host)
            },
        );
        Some(crate::chrome::model::OpenFileModel {
            path: field.text().to_string(),
            caret: crate::chrome::model::Caret {
                at: field.caret(),
                selection: field.selection(),
            },
            cwd,
            host,
        })
    }

    /// Open `path` in a new pane of the active tab, read from the focused
    /// session's host (#464).
    ///
    /// A relative path resolves against that session's cwd, on *its* machine —
    /// which is the whole reason this goes through the wire rather than
    /// `std::fs`: the tab may be a shell on the build box, and reading the
    /// local file of the same name would be a confident wrong answer.
    pub(super) fn open_file_pane(&mut self, path: &str) {
        let Some(tab) = self.tabs.active() else { return };
        // Read through the focused pane when it is a shell, and through the
        // tab's own session when the focus is already on a file — opening a
        // second file from the first should not need a trip back to a terminal.
        let (origin_addr, cwd) = {
            let session = tab.focused_session().unwrap_or_else(|| tab.source());
            let addr = tab.focused_session().map_or(tab.addr, |_| tab.focused_addr());
            let term = session.terminal();
            let term = term.lock();
            let mut cwd = if term.cwd().is_empty() {
                term.blocks().last().map(|b| b.cwd.clone()).unwrap_or_default()
            } else {
                term.cwd().to_string()
            };
            // A shell that has not run a prompt yet has reported no cwd, and
            // a relative path would be refused for a reason that is true but
            // useless — "no working directory" a second after the window
            // opened. For a session in *this* process the answer is known:
            // the directory this process was started in, which is where its
            // shell was spawned. A remote session gets no such guess; the
            // refusal is the honest answer there, since the directory that
            // matters is on another machine.
            if cwd.is_empty() && matches!(session.origin(), crate::source::Origin::InProcess) {
                cwd = std::env::current_dir()
                    .map(|p| p.to_string_lossy().into_owned())
                    .unwrap_or_default();
            }
            (addr, cwd)
        };

        let addr = crate::tabs::placeholder_addr(self.shared.mint_placeholder());
        let editor = crate::editor::EditorPane::loading(addr, origin_addr, path, &cwd);
        self.adopt_pane(crate::tabs::SplitPane::editor(editor));
        self.request_file_for(addr);
    }

    /// Put the question for pane `addr` on the wire, or answer it here when
    /// the session runs in this process (#434: a window hosting its own
    /// session reads its own filesystem, rather than asking itself over a
    /// socket).
    fn request_file_for(&mut self, addr: zest_proto::SessionAddr) {
        let Some(tab) = self.tabs.active() else { return };
        let Some((_, editor)) = tab
            .panes
            .iter()
            .enumerate()
            .find_map(|(j, p)| p.editor_ref().filter(|e| e.addr == addr).map(|e| (j, e)))
        else {
            return;
        };
        let (asked, cwd) = (editor.asked.clone(), editor.cwd.clone());

        // The source that owns the file's host: the pane the read was opened
        // from, falling back to the tab's own shell.
        let origin = editor.origin;
        let source = (0..tab.pane_count())
            .find(|&i| tab.pane_addr(i) == origin)
            .and_then(|i| tab.pane_session(i))
            .unwrap_or_else(|| tab.source());

        if source.request_file(&asked, &cwd) {
            return;
        }
        // No host to ask: this window *is* the host. Off the UI thread, since
        // a read is a disk and `files::read_file` is the daemon's own — one
        // implementation of the cap, the hash and the binary sniff rather than
        // a second that drifts from it.
        let waker = self.wakeup_sender();
        let cell = std::sync::Arc::clone(&self.local_file_replies);
        let spawned = std::thread::Builder::new().name("zest-file-read".into()).spawn(move || {
            let msg = zest_daemon::files::read_file(&asked, &cwd);
            if let Some(reply) = crate::editor::FileReply::from_host(msg) {
                cell.lock().push((addr, reply));
                waker(crate::session::Wakeup::FileContentsReady);
            }
        });
        if let Err(e) = spawned {
            if let Some(tab) = self.tabs.active_mut() {
                for (_, editor) in tab.editors_mut().filter(|(_, e)| e.addr == addr) {
                    editor.state =
                        crate::editor::LoadState::Failed(format!("no thread to read it: {e}"));
                }
            }
        }
    }

    /// Move every answer that has landed onto the pane waiting for it.
    pub(super) fn drain_file_replies(&mut self) {
        // Local reads carry the pane they were for, so they route exactly.
        let local: Vec<_> = std::mem::take(&mut *self.local_file_replies.lock());

        // A remote answer is parked on the *source* that asked, and the wire
        // has no request id — the correlation is the echoed path, which comes
        // back canonicalized and so does not match a relative ask. So each
        // answer is carried with the address of the session that produced it,
        // and goes only to a pane that asked *that* session. Without the
        // address a tab split across two machines could hand the build box's
        // answer to a pane waiting on the laptop — the two would look alike
        // and the file would simply be the wrong one.
        //
        // What remains, and is inherent to one cell per source: two files
        // opened on the *same* host in the same instant means the second
        // reply overwrites the first in that cell, and the first pane keeps
        // saying it is opening. Rare enough to name rather than build a queue
        // for.
        let mut remote: Vec<(zest_proto::SessionAddr, crate::editor::FileReply)> = Vec::new();
        if let Some(tab) = self.tabs.active() {
            for i in 0..tab.pane_count() {
                if let Some(reply) = tab.pane_session(i).and_then(|s| s.take_file_contents()) {
                    remote.push((tab.pane_addr(i), reply));
                }
            }
        }
        if local.is_empty() && remote.is_empty() {
            return;
        }

        if let Some(tab) = self.tabs.active_mut() {
            for (addr, reply) in local {
                for (_, editor) in tab.editors_mut().filter(|(_, e)| e.addr == addr) {
                    editor.apply(reply.clone());
                }
            }
            for (from, reply) in remote {
                if let Some((_, editor)) =
                    tab.editors_mut().find(|(_, e)| e.wants_reply_from(from))
                {
                    editor.apply(reply);
                }
            }
        }
        self.mark_chrome_dirty();
        if let Some(w) = self.window.as_ref() {
            w.request_redraw();
        }
    }

    /// An answer landed; keep it only if it is still the question.
    pub(super) fn apply_dir_listing(&mut self, listing: crate::session::DirListing) {
        let Some(p) = self.dir_picker.as_mut() else { return };
        if p.path != listing.path {
            // Navigated on while this was in flight: a stale answer drawn
            // over a newer question is the picker lying about where it is.
            return;
        }
        p.parent = listing.parent;
        p.dirs = listing.dirs;
        p.truncated = listing.truncated;
        p.error = listing.error;
        p.loading = false;
        p.selected = 0;
        p.scroll = 0.0;
        p.scroll_to_selected = true;
        self.mark_chrome_dirty();
    }

    /// Act on the browser's row `i`: the `..` row navigates, a directory
    /// switches the shell there — the same `cd_bytes` and at-prompt gates
    /// the recents menu used, for the same reasons.
    pub(super) fn dir_picker_activate(&mut self, i: usize) {
        let Some(p) = self.dir_picker.as_ref() else { return };
        match p.rows.get(i) {
            Some(None) => {
                if let Some(parent) = p.parent.clone() {
                    self.dir_picker_navigate(parent);
                }
            }
            Some(Some(path)) => {
                let path = path.clone();
                self.dir_picker = None;
                self.mark_chrome_dirty();
                let Some(bytes) = block_actions::cd_bytes(&path) else { return };
                let Some(session) = self.tabs.active_source() else { return };
                // Re-checked at the act, not only at build: the picker can
                // sit open across a state change, and a row a moment ago is
                // not a licence to type into a program now running.
                if !block_actions::at_shell_prompt(&session.terminal().lock()) {
                    return;
                }
                session.write(bytes);
                session.terminal().lock().scroll_to_bottom();
                if let Some(w) = self.window.as_ref() {
                    w.request_redraw();
                }
            }
            None => {}
        }
    }

    /// Browse *into* the browser's row `i` without switching — Tab's verb.
    pub(super) fn dir_picker_descend(&mut self, i: usize) {
        let Some(p) = self.dir_picker.as_ref() else { return };
        match p.rows.get(i) {
            Some(None) => {
                if let Some(parent) = p.parent.clone() {
                    self.dir_picker_navigate(parent);
                }
            }
            Some(Some(path)) => {
                let path = path.clone();
                self.dir_picker_navigate(path);
            }
            None => {}
        }
    }
}
