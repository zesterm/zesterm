//! Scrollback that outlives the memory holding it.
//!
//! The grid keeps a bounded ring: past `scrollback_limit` rows, the oldest are
//! dropped and are gone. That bound exists because a `Terminal` must be cheap
//! enough to have thousands of, and it is the right bound for the *live* grid —
//! but it means a session's history is smaller than the session, and a client
//! scrolling up hits a wall the host cannot explain.
//!
//! So evicted rows are written here on their way out. `RequestScrollback` is
//! answered from memory first and from this store beyond it, which is invisible
//! to the client: the wire already says "here is what survives", and this simply
//! makes more of it survive.
//!
//! # Why a database rather than a file
//!
//! The access pattern is "give me lines `n..n+m` by id, oldest first", from a
//! file being appended to concurrently, possibly by a process that is killed
//! mid-write. That is an index and a transaction, and hand-rolling both over a
//! flat file is how the corpus recorder's format ends up in the hot path.
//!
//! **Not for the live grid.** Every row on screen stays in memory; this is only
//! reached for what has already scrolled off. A terminal that touched a database
//! to paint a frame would have lost the argument in ADR-004.

use std::path::Path;

use rusqlite::Connection;

/// One row as it left the grid.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Line {
    /// Absolute line id — the same numbering the wire uses, so a client's
    /// `from_line` needs no translation.
    pub id: u64,
    /// The row's text. Attributes are deliberately not stored yet; see below.
    pub text: String,
    /// Whether this row was a continuation of the one above it.
    pub wrapped: bool,
}

/// Evicted scrollback for one session.
pub struct History {
    db: Connection,
}

impl History {
    /// Open or create the store for a session.
    pub fn open(path: &Path) -> Result<Self, rusqlite::Error> {
        let db = Connection::open(path)?;
        Self::prepare(db)
    }

    /// An in-memory store, for tests and for a daemon told not to persist.
    pub fn in_memory() -> Result<Self, rusqlite::Error> {
        Self::prepare(Connection::open_in_memory()?)
    }

    fn prepare(db: Connection) -> Result<Self, rusqlite::Error> {
        // WAL so a reader answering `RequestScrollback` never blocks the writer
        // draining the pty, which is the one path in this crate that must not
        // stall. NORMAL rather than FULL: losing the last few evicted lines to a
        // power cut is not worth an fsync per scrolled line, and those lines are
        // still on the client that was watching.
        db.pragma_update(None, "journal_mode", "WAL")?;
        db.pragma_update(None, "synchronous", "NORMAL")?;
        db.execute_batch(
            "CREATE TABLE IF NOT EXISTS line (
                 id      INTEGER PRIMARY KEY,
                 text    TEXT NOT NULL,
                 wrapped INTEGER NOT NULL
             );",
        )?;
        Ok(Self { db })
    }

    /// Record rows on their way out of the grid.
    ///
    /// `INSERT OR REPLACE` rather than `INSERT`: a reflow renumbers line ids, so
    /// the same id can legitimately arrive twice with different text, and the
    /// later one is the truth. Failing there would mean a resize could poison a
    /// session's history.
    pub fn append(&mut self, lines: &[Line]) -> Result<(), rusqlite::Error> {
        let tx = self.db.transaction()?;
        {
            let mut stmt =
                tx.prepare_cached("INSERT OR REPLACE INTO line (id, text, wrapped) VALUES (?, ?, ?)")?;
            for l in lines {
                stmt.execute(rusqlite::params![l.id, l.text, i64::from(l.wrapped)])?;
            }
        }
        tx.commit()
    }

    /// Lines from `from` onwards, oldest first, at most `count`.
    ///
    /// The same shape as `Grid::lines_by_id`, so the caller can ask both and
    /// concatenate without caring where a row came from.
    pub fn lines_from(&self, from: u64, count: usize) -> Result<Vec<Line>, rusqlite::Error> {
        let mut stmt = self
            .db
            .prepare_cached("SELECT id, text, wrapped FROM line WHERE id >= ? ORDER BY id LIMIT ?")?;
        let rows = stmt.query_map(rusqlite::params![from, count as i64], |r| {
            Ok(Line { id: r.get(0)?, text: r.get(1)?, wrapped: r.get::<_, i64>(2)? != 0 })
        })?;
        rows.collect()
    }

    /// How many lines are stored. For tests and for `--sessions`-style reporting.
    pub fn len(&self) -> Result<u64, rusqlite::Error> {
        self.db.query_row("SELECT COUNT(*) FROM line", [], |r| r.get(0))
    }

    #[allow(clippy::missing_errors_doc)]
    pub fn is_empty(&self) -> Result<bool, rusqlite::Error> {
        Ok(self.len()? == 0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn line(id: u64, text: &str) -> Line {
        Line { id, text: text.to_string(), wrapped: false }
    }

    #[test]
    fn lines_come_back_in_the_order_they_left() {
        // Scrollback is read upwards but stored downwards, and a client prepends
        // what it is given. Out of order here is a screen out of order there.
        let mut h = History::in_memory().expect("open");
        h.append(&[line(1, "first"), line(2, "second"), line(3, "third")]).expect("append");
        let got = h.lines_from(1, 10).expect("read");
        assert_eq!(
            got.iter().map(|l| l.text.as_str()).collect::<Vec<_>>(),
            ["first", "second", "third"],
            "oldest first, by id"
        );
    }

    #[test]
    fn a_client_asking_from_the_middle_gets_the_middle() {
        // `from_line` is an absolute id, not an offset: a phone that holds lines
        // 1..50 asks from 51, and must not be handed 1 again.
        let mut h = History::in_memory().expect("open");
        h.append(&(1..=10).map(|i| line(i, &format!("line {i}"))).collect::<Vec<_>>())
            .expect("append");
        let got = h.lines_from(7, 2).expect("read");
        assert_eq!(got.first().map(|l| l.id), Some(7), "starts where asked");
        assert_eq!(got.len(), 2, "and stops at the count it was given");
    }

    #[test]
    fn a_renumbered_line_replaces_rather_than_conflicts() {
        // Reflow renumbers ids, so the same id legitimately arrives twice with
        // different text. Failing on the second would let a window resize poison
        // a session's history.
        let mut h = History::in_memory().expect("open");
        h.append(&[line(4, "before the resize")]).expect("first");
        h.append(&[line(4, "after the resize")]).expect("second must not fail");
        let got = h.lines_from(4, 1).expect("read");
        assert_eq!(got[0].text, "after the resize", "the later write is the truth");
        assert_eq!(h.len().expect("count"), 1, "and it replaced rather than duplicated");
    }

    #[test]
    fn asking_beyond_the_end_is_empty_rather_than_an_error() {
        // A client that scrolled to the very top asks for what is above it. The
        // honest answer is nothing, not a failure -- the wire has no way to say
        // "that was a silly question" and should not need one.
        let h = History::in_memory().expect("open");
        assert!(h.lines_from(9_000, 10).expect("read").is_empty());
    }

    #[test]
    fn wrapping_survives_the_round_trip() {
        // `wrapped` is what joins rows into a logical line, and it is what
        // copying a wrapped line depends on. A history that loses it turns one
        // pasted command into several.
        let mut h = History::in_memory().expect("open");
        h.append(&[Line { id: 1, text: "a long comm".into(), wrapped: true }]).expect("append");
        assert!(h.lines_from(1, 1).expect("read")[0].wrapped);
    }
}
