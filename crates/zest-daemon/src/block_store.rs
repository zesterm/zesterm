//! What persists is the block, never the scrollback (ADR-020, #529).
//!
//! Every *finished* command block — command, cwd, branch, exit code, start
//! and end wall time, who typed it — is written here on the host that ran
//! it, so `SearchBlocks` can answer for sessions that have ended and for
//! daemons that have since restarted. Nothing else about a session survives:
//! not the rows, not the output, not the grid. There is no column for output
//! text, which is the strongest form of that rule.
//!
//! This replaced a store of evicted scrollback *rows* that was tested for a
//! year and never called. ADR-020 records the five reasons rows are a trap
//! and blocks are not — holes on four eviction paths, reflow renumbering,
//! ED 3 against a store, session ids restarting at one, a fourth copy of the
//! text rule — so they are not repeated here.
//!
//! # Why a database rather than a file
//!
//! The access pattern is "the newest rows whose command contains this",
//! from a file appended to by one thread and read by another, possibly in a
//! process killed mid-write. That is an index and a transaction, and
//! hand-rolling both over a flat file is how the corpus recorder's format
//! would end up in the hot path. WAL, `synchronous = NORMAL`: a torn write
//! is a rollback, never a corrupt file, and a commit does not wait for the
//! disk on every command.
//!
//! # Never on the pty reader's time
//!
//! The reader thread hands finished blocks to [`BlockSink::record`], which
//! `try_send`s them down a bounded channel and returns; a writer thread owns
//! the database. The reader holds the terminal lock across a whole parse
//! chunk, and an fsync under that lock is exactly the keystroke stall
//! ADR-016 built a predicted-echo overlay to hide. A full queue drops the
//! batch and says so once — a few rows of history under a pathological flood
//! against every attached client's output is not a close call.
//!
//! # Substring, not full-text
//!
//! FTS5 is compiled in (`libsqlite3-sys`'s `build.rs` passes
//! `-DSQLITE_ENABLE_FTS5` on the bundled path) and is not used. The live
//! search is a case-folded substring over the command line
//! (`zest_proto::search::Needle`); a trigram index answers nothing for one-
//! and two-character queries and ranks by bm25, so live and stored would be
//! two truths. The store is bounded, and a recency-ordered scan of it costs
//! milliseconds on a worker thread. SQL narrows (`LIKE`, a superset for an
//! ASCII query) and the shared Rust predicate decides.

use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{sync_channel, Receiver, SyncSender, TrySendError};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use rusqlite::{params, Connection, OpenFlags};
use zest_core::BlockId;
use zest_proto::search::Needle;
use zest_proto::{delta::BlockContextPayload, BlockMatch, BlockState, ClientId, HostId, SessionId};

/// One run of one session — the key a stored block belongs to.
///
/// Random rather than `(started_ms, SessionId)`: session ids restart at one
/// on every daemon start, and a clock-and-id pair collides across an
/// `--ephemeral` daemon started beside the real one, besides putting a
/// meaning ("started at") into a key. Never crosses the wire; a stored
/// block's `BlockMatch.session` is `None`, which is the whole point.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct RunId(pub u64);

impl RunId {
    /// A fresh run, from the OS random source. A source that fails is
    /// unheard of on the three platforms this ships on; falling back to the
    /// clock keeps the session rather than refusing to spawn a shell over a
    /// history row.
    #[must_use]
    pub fn mint() -> Self {
        let mut bytes = [0u8; 8];
        if getrandom::fill(&mut bytes).is_ok() {
            return Self(u64::from_le_bytes(bytes));
        }
        tracing::warn!("the OS random source failed; a run id from the clock instead");
        Self(crate::session::unix_ms().wrapping_mul(0x9E37_79B9_7F4A_7C15))
    }
}

/// A command a shell finished, as the store keeps it.
///
/// No output, no venv, no kube, no environment: the columns are the rule.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredBlock {
    pub run: RunId,
    /// The number the session had, for display only ("session 3, last
    /// Tuesday"); never a key, for the reason [`RunId`] exists.
    pub session: SessionId,
    pub id: BlockId,
    pub command: String,
    /// The command was longer than [`MAX_COMMAND_BYTES`] and was cut. A
    /// pasted script is history, but it is not something ⏎ may re-run as if
    /// it were whole.
    pub command_truncated: bool,
    pub cwd: String,
    pub branch: String,
    /// `None` is not zero: a shell that emits OSC 133 D without the status
    /// is common (`zest_core::BlockState`).
    pub exit_code: Option<i32>,
    pub started_ms: u64,
    pub ended_ms: u64,
    pub author: Option<[u8; 32]>,
}

/// The longest command stored, in bytes. Holds anything a person typed and
/// cuts pasted scripts; the flag reaches the client so it can refuse to
/// re-run half of one.
pub const MAX_COMMAND_BYTES: usize = 4096;
/// Rows kept, newest by end time. A year of a busy shell is a fraction of
/// this; the bound exists so the file cannot grow without one.
pub const MAX_BLOCKS: u64 = 100_000;
/// Rows kept, by age.
pub const MAX_AGE_MS: u64 = 365 * 24 * 3600 * 1000;
/// The most rows one search reads before the Rust predicate decides. The
/// worst case — a non-ASCII query the SQL cannot narrow, over a full store —
/// is one bounded read, and the answer says the older history was not
/// searched.
pub const SCAN_CAP: usize = 50_000;
/// Batches queued between the reader thread and the writer before a
/// `record` starts dropping.
const QUEUE_DEPTH: usize = 4096;
/// Inserts between prunes.
const PRUNE_EVERY: u64 = 1000;

impl StoredBlock {
    /// The store's form of a block the parser finished. `None` for one that
    /// has not: a running block is not history yet, and a block that never
    /// finishes never becomes any — the wire has no honest state for "ran,
    /// outcome unknown, not running", and ⏎ would re-run a command whose
    /// last outcome was the shell dying under it.
    #[must_use]
    pub fn from_block(run: RunId, session: SessionId, b: &zest_core::Block) -> Option<Self> {
        let zest_core::BlockState::Finished { exit_code } = b.state else { return None };
        let (command, command_truncated) = cut(&b.command);
        Some(Self {
            run,
            session,
            id: b.id,
            command,
            command_truncated,
            cwd: b.cwd.clone(),
            branch: b.context.as_ref().map(|c| c.branch.clone()).unwrap_or_default(),
            exit_code,
            // The daemon stamps `set_now_ms` before every `advance`, so a
            // block finished on this host always carries both. The fallback
            // is for a block a client upserted, which this store never sees.
            started_ms: b.started_ms.unwrap_or(0),
            ended_ms: b.ended_ms.or(b.started_ms).unwrap_or(0),
            author: b.author,
        })
    }

    /// The search form. A stored block's session is gone, so `session` is
    /// `None` and the title is empty; the branch rides the same context
    /// payload a live block's does so a client reads one shape.
    #[must_use]
    pub fn to_match(&self, host: HostId) -> BlockMatch {
        BlockMatch {
            host,
            session: None,
            block: self.id.0,
            title: String::new(),
            command: self.command.clone(),
            command_truncated: self.command_truncated,
            cwd: self.cwd.clone(),
            state: BlockState::Finished { exit_code: self.exit_code },
            started_ms: Some(self.started_ms),
            ended_ms: Some(self.ended_ms),
            context: (!self.branch.is_empty()).then(|| BlockContextPayload {
                branch: self.branch.clone(),
                venv: String::new(),
                kube: String::new(),
            }),
            author: self.author.map(ClientId::from_bytes),
        }
    }
}

/// Bound a command for storage, on a character boundary.
fn cut(command: &str) -> (String, bool) {
    if command.len() <= MAX_COMMAND_BYTES {
        return (command.to_string(), false);
    }
    let mut end = MAX_COMMAND_BYTES;
    while !command.is_char_boundary(end) {
        end -= 1;
    }
    (command[..end].to_string(), true)
}

enum Op {
    Record(Vec<StoredBlock>),
    Flush(SyncSender<()>),
}

/// The reader thread's end of the store: cheap to clone, never blocks.
#[derive(Clone)]
pub struct BlockSink {
    tx: SyncSender<Op>,
    /// Said once per sink, not per batch: a flood that fills the queue would
    /// otherwise log per chunk for as long as it lasts.
    complained: Arc<AtomicBool>,
}

impl BlockSink {
    /// Queue finished blocks for the writer. Returns whether they were
    /// taken; a refusal is logged once and the rows are lost, by design —
    /// see the module doc.
    pub fn record(&self, batch: Vec<StoredBlock>) -> bool {
        if batch.is_empty() {
            return true;
        }
        match self.tx.try_send(Op::Record(batch)) {
            Ok(()) => true,
            Err(TrySendError::Full(_) | TrySendError::Disconnected(_)) => {
                if !self.complained.swap(true, Ordering::AcqRel) {
                    tracing::warn!("the block history writer is not keeping up; some rows were dropped");
                }
                false
            }
        }
    }
}

/// Retention, a struct so a test can shrink it without inserting a hundred
/// thousand rows.
#[derive(Debug, Clone, Copy)]
struct Caps {
    blocks: u64,
    age_ms: u64,
}

/// The durable block history of this host.
pub struct BlockStore {
    tx: SyncSender<Op>,
    /// A second connection for reads. WAL means the reader never waits on
    /// the writer's transaction, and `Connection` is `!Sync`, so a mutex is
    /// the honest shape rather than a trick.
    read: Mutex<Connection>,
}

impl BlockStore {
    /// Open (or create) the store at `path`. The parent directory is created;
    /// a schema newer than this build's is refused rather than guessed at.
    pub fn open(path: &Path) -> Result<Arc<Self>, rusqlite::Error> {
        // A failure here surfaces as the open failing, with its own reason.
        let _ = create_private(path);
        let flags = OpenFlags::default();
        let writer = Connection::open_with_flags(path, flags)?;
        let reader = Connection::open_with_flags(path, flags)?;
        Self::with(writer, reader, Caps { blocks: MAX_BLOCKS, age_ms: MAX_AGE_MS })
    }

    /// A store in memory, for tests. A shared-cache URI, so the writer's
    /// connection and the reader's see one database; SQLite was built with
    /// `SQLITE_USE_URI` on the bundled path.
    pub fn in_memory() -> Result<Arc<Self>, rusqlite::Error> {
        Self::in_memory_with(MAX_BLOCKS, MAX_AGE_MS)
    }

    fn in_memory_with(blocks: u64, age_ms: u64) -> Result<Arc<Self>, rusqlite::Error> {
        static N: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let n = N.fetch_add(1, Ordering::Relaxed);
        let uri = format!("file:zest-blocks-{}-{n}?mode=memory&cache=shared", std::process::id());
        let flags = OpenFlags::default() | OpenFlags::SQLITE_OPEN_URI;
        let writer = Connection::open_with_flags(&uri, flags)?;
        let reader = Connection::open_with_flags(&uri, flags)?;
        Self::with(writer, reader, Caps { blocks, age_ms })
    }

    fn with(writer: Connection, reader: Connection, caps: Caps) -> Result<Arc<Self>, rusqlite::Error> {
        prepare(&writer)?;
        let (tx, rx) = sync_channel::<Op>(QUEUE_DEPTH);
        std::thread::Builder::new()
            .name("zest-daemon-blocks".into())
            .spawn(move || write_loop(writer, &rx, caps))
            .map_err(|e| rusqlite::Error::InvalidParameterName(format!("no writer thread: {e}")))?;
        Ok(Arc::new(Self { tx, read: Mutex::new(reader) }))
    }

    /// The reader thread's handle.
    #[must_use]
    pub fn sink(&self) -> BlockSink {
        BlockSink { tx: self.tx.clone(), complained: Arc::default() }
    }

    /// The newest stored blocks matching `needle`, newest first, and whether
    /// the scan hit `cap` before it ran out of rows — in which case older
    /// history went unsearched, and the answer should say so.
    ///
    /// The SQL narrows only when it can promise a superset: `LIKE` folds
    /// ASCII case and nothing else, so for a query typed in ASCII every row
    /// the Rust rule accepts also passes `LIKE` (the one exception is a
    /// *command* whose non-ASCII letter folds to ASCII — a Kelvin sign, a
    /// dotted capital I — which no search is worth a full scan per keystroke
    /// to catch). A query with any non-ASCII character as typed skips the
    /// narrowing — decided on the query, not its fold, since a fold can be
    /// ASCII when the query was not — and the scan cap does the bounding.
    pub fn search(&self, needle: &Needle, cap: usize) -> Result<(Vec<StoredBlock>, bool), rusqlite::Error> {
        let db = self.read.lock().expect("block store reader");
        let folded = needle.folded();
        let narrow = !folded.is_empty() && needle.is_ascii();
        let pattern = format!(
            "%{}%",
            folded.replace('\\', "\\\\").replace('%', "\\%").replace('_', "\\_")
        );
        let sql = if narrow {
            "SELECT run_id, block_id, session_id, command, command_truncated, cwd, branch, exit_code, \
             started_ms, ended_ms, author FROM block WHERE command LIKE ?1 ESCAPE '\\' \
             ORDER BY ended_ms DESC, block_id DESC LIMIT ?2"
        } else {
            "SELECT run_id, block_id, session_id, command, command_truncated, cwd, branch, exit_code, \
             started_ms, ended_ms, author FROM block ORDER BY ended_ms DESC, block_id DESC LIMIT ?1"
        };
        let mut stmt = db.prepare_cached(sql)?;
        // One row past the cap, so "capped" means a row *exists* beyond it:
        // a store holding exactly `cap` rows is fully searched and must not
        // say otherwise.
        let fetch = i64::try_from(cap.saturating_add(1)).unwrap_or(i64::MAX);
        let rows = if narrow {
            stmt.query_map(params![pattern, fetch], row_to_block)?
        } else {
            stmt.query_map(params![fetch], row_to_block)?
        };
        let mut scanned = 0usize;
        let mut out = Vec::new();
        for row in rows {
            let b = row?;
            scanned += 1;
            if scanned > cap {
                break;
            }
            if needle.matches(&b.command) {
                out.push(b);
            }
        }
        Ok((out, scanned > cap))
    }

    /// Wait until everything queued so far is on disk, or `timeout` passes.
    /// For shutdown and for tests; returns whether the writer answered.
    pub fn flush(&self, timeout: Duration) -> bool {
        let (done, wait) = sync_channel(1);
        if self.tx.try_send(Op::Flush(done)).is_err() {
            return false;
        }
        wait.recv_timeout(timeout).is_ok()
    }

    /// Rows held.
    pub fn len(&self) -> Result<u64, rusqlite::Error> {
        let db = self.read.lock().expect("block store reader");
        db.query_row("SELECT COUNT(*) FROM block", [], |r| r.get::<_, i64>(0))
            .map(|n| u64::try_from(n).unwrap_or(0))
    }

    /// Nothing stored yet.
    pub fn is_empty(&self) -> Result<bool, rusqlite::Error> {
        self.len().map(|n| n == 0)
    }

    /// The column list, for the test that asserts what is *not* here.
    pub fn columns(&self) -> Result<Vec<String>, rusqlite::Error> {
        let db = self.read.lock().expect("block store reader");
        let mut stmt = db.prepare("SELECT name FROM pragma_table_info('block')")?;
        let names = stmt.query_map([], |r| r.get::<_, String>(0))?.collect::<Result<Vec<_>, _>>()?;
        Ok(names)
    }
}

/// Make room for the history and make it private: the directory 0700 when
/// this creates it, the file 0600 from birth. Command history with its
/// directories, branches and authors is not for other accounts on the
/// machine, and a umask of 022 would otherwise hand it to them.
///
/// The file is created here, empty, with its mode — SQLite reads a
/// zero-length file as an empty database, and mirrors the main file's mode
/// onto `-wal` and `-shm` — because a chmod after the open is a window,
/// where a mode on the create is not (#403's lesson: prefer the call that
/// takes the property as an argument). Never umask: process-global, and
/// the victims are a crate away. An existing directory keeps its mode; it
/// may hold other state whose permissions are not this module's to decide.
fn create_private(path: &Path) -> std::io::Result<()> {
    if let Some(dir) = path.parent() {
        let mut builder = std::fs::DirBuilder::new();
        builder.recursive(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::DirBuilderExt;
            builder.mode(0o700);
        }
        builder.create(dir)?;
    }
    let mut file = std::fs::OpenOptions::new();
    file.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        file.mode(0o600);
    }
    match file.open(path) {
        Ok(_) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => Ok(()),
        Err(e) => Err(e),
    }
}

/// This build's schema. A newer file is refused: the columns it added are
/// ones this build would silently drop on the next write.
const SCHEMA_VERSION: i64 = 1;

fn prepare(db: &Connection) -> Result<(), rusqlite::Error> {
    db.pragma_update(None, "journal_mode", "WAL")?;
    db.pragma_update(None, "synchronous", "NORMAL")?;
    let version: i64 = db.pragma_query_value(None, "user_version", |r| r.get(0))?;
    if version > SCHEMA_VERSION {
        return Err(rusqlite::Error::InvalidParameterName(format!(
            "the block history is schema {version}; this build reads {SCHEMA_VERSION}"
        )));
    }
    db.execute_batch(
        "CREATE TABLE IF NOT EXISTS block (
            run_id            INTEGER NOT NULL,
            block_id          INTEGER NOT NULL,
            session_id        INTEGER NOT NULL,
            command           TEXT    NOT NULL,
            command_truncated INTEGER NOT NULL DEFAULT 0,
            cwd               TEXT    NOT NULL,
            branch            TEXT    NOT NULL DEFAULT '',
            exit_code         INTEGER,
            started_ms        INTEGER NOT NULL,
            ended_ms          INTEGER NOT NULL,
            author            BLOB,
            PRIMARY KEY (run_id, block_id)
        );
        CREATE INDEX IF NOT EXISTS block_by_time ON block (ended_ms DESC);",
    )?;
    if version < SCHEMA_VERSION {
        db.pragma_update(None, "user_version", SCHEMA_VERSION)?;
    }
    Ok(())
}

fn row_to_block(r: &rusqlite::Row<'_>) -> Result<StoredBlock, rusqlite::Error> {
    let run: i64 = r.get(0)?;
    let id: i64 = r.get(1)?;
    let session: i64 = r.get(2)?;
    let exit_code: Option<i64> = r.get(7)?;
    let started: i64 = r.get(8)?;
    let ended: i64 = r.get(9)?;
    let author: Option<Vec<u8>> = r.get(10)?;
    Ok(StoredBlock {
        // Bit-cast back: `u64 as i64` on the way in, so the sign is noise.
        run: RunId(run as u64),
        id: BlockId(u32::try_from(id).unwrap_or(u32::MAX)),
        session: SessionId(u64::try_from(session).unwrap_or(0)),
        command: r.get(3)?,
        command_truncated: r.get::<_, i64>(4)? != 0,
        cwd: r.get(5)?,
        branch: r.get(6)?,
        exit_code: exit_code.and_then(|c| i32::try_from(c).ok()),
        started_ms: u64::try_from(started).unwrap_or(0),
        ended_ms: u64::try_from(ended).unwrap_or(0),
        author: author.and_then(|a| <[u8; 32]>::try_from(a).ok()),
    })
}

/// The writer thread: drain, coalesce into one transaction, commit, prune
/// now and then. Ends when the last sender is gone.
fn write_loop(mut db: Connection, rx: &Receiver<Op>, caps: Caps) {
    if let Err(e) = prune(&db, caps) {
        tracing::warn!(error = %e, "could not prune the block history");
    }
    let mut since_prune: u64 = 0;
    while let Ok(first) = rx.recv() {
        let mut batch = Vec::new();
        let mut flushes = Vec::new();
        let mut take = |op| match op {
            Op::Record(rows) => batch.extend(rows),
            Op::Flush(done) => flushes.push(done),
        };
        take(first);
        while let Ok(op) = rx.try_recv() {
            take(op);
        }
        if !batch.is_empty() {
            match insert(&mut db, &batch) {
                Ok(()) => since_prune += batch.len() as u64,
                Err(e) => tracing::warn!(error = %e, rows = batch.len(), "could not write block history"),
            }
            if since_prune >= PRUNE_EVERY {
                since_prune = 0;
                if let Err(e) = prune(&db, caps) {
                    tracing::warn!(error = %e, "could not prune the block history");
                }
            }
        }
        for done in flushes {
            let _ = done.try_send(());
        }
    }
}

fn insert(db: &mut Connection, rows: &[StoredBlock]) -> Result<(), rusqlite::Error> {
    let tx = db.transaction()?;
    {
        // REPLACE is insurance, not a path: within one run a finished block's
        // id never recurs (`begin_prompt` re-anchors only a prompt that ran
        // nothing, and `next_id` never rewinds). A duplicate becoming an
        // error would poison a session's history for a bug elsewhere.
        let mut stmt = tx.prepare_cached(
            "INSERT OR REPLACE INTO block (run_id, block_id, session_id, command, command_truncated, \
             cwd, branch, exit_code, started_ms, ended_ms, author) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
        )?;
        for b in rows {
            stmt.execute(params![
                b.run.0 as i64,
                i64::from(b.id.0),
                i64::try_from(b.session.0).unwrap_or(i64::MAX),
                b.command,
                i64::from(b.command_truncated),
                b.cwd,
                b.branch,
                b.exit_code,
                i64::try_from(b.started_ms).unwrap_or(i64::MAX),
                i64::try_from(b.ended_ms).unwrap_or(i64::MAX),
                b.author.as_ref().map(|a| a.to_vec()),
            ])?;
        }
    }
    tx.commit()
}

fn prune(db: &Connection, caps: Caps) -> Result<(), rusqlite::Error> {
    let now = crate::session::unix_ms();
    let oldest = i64::try_from(now.saturating_sub(caps.age_ms)).unwrap_or(0);
    db.execute("DELETE FROM block WHERE ended_ms < ?1", params![oldest])?;
    db.execute(
        "DELETE FROM block WHERE rowid IN \
         (SELECT rowid FROM block ORDER BY ended_ms DESC, block_id DESC LIMIT -1 OFFSET ?1)",
        params![i64::try_from(caps.blocks).unwrap_or(i64::MAX)],
    )?;
    Ok(())
}

/// Live and stored, as one answer: a live session's finished blocks are in
/// the store too, so the same `(run, block)` is dropped from the stored side
/// — the live row wins because it carries the session `⇧⏎` and `output`
/// need. Newest first, `limit` applied to the union, and `truncated` about
/// the union.
#[must_use]
pub fn merge(
    live: Vec<(RunId, BlockMatch)>,
    stored: Vec<StoredBlock>,
    host: HostId,
    limit: usize,
) -> (Vec<BlockMatch>, bool) {
    let seen: std::collections::HashSet<(RunId, u32)> =
        live.iter().map(|(run, m)| (*run, m.block)).collect();
    let mut out: Vec<BlockMatch> = live.into_iter().map(|(_, m)| m).collect();
    out.extend(stored.iter().filter(|s| !seen.contains(&(s.run, s.id.0))).map(|s| s.to_match(host)));
    zest_proto::search::rank(&mut out);
    let truncated = out.len() > limit;
    out.truncate(limit);
    (out, truncated)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn block(id: u32, command: &str, ended: u64) -> zest_core::Block {
        zest_core::Block {
            id: BlockId(id),
            prompt_line: 0,
            output_line: Some(1),
            end_line: Some(2),
            state: zest_core::BlockState::Finished { exit_code: Some(0) },
            command: command.into(),
            cwd: "/home/a".into(),
            started_ms: Some(ended.saturating_sub(5)),
            ended_ms: Some(ended),
            context: None,
            author: None,
        }
    }

    fn stored(run: u64, id: u32, command: &str, ended: u64) -> StoredBlock {
        StoredBlock::from_block(RunId(run), SessionId(1), &block(id, command, ended)).expect("finished")
    }

    fn settled(store: &BlockStore) {
        assert!(store.flush(Duration::from_secs(5)), "the writer thread answered");
    }

    /// The store must obey the palette's rule, or live and stored disagree
    /// about the same command.
    #[test]
    fn a_finished_block_is_found_by_substring_case_insensitively() {
        let store = BlockStore::in_memory().expect("open");
        store.sink().record(vec![stored(1, 1, "Cargo Build --release", 100), stored(1, 2, "ls", 200)]);
        settled(&store);
        let (hits, capped) = store.search(&Needle::new("CARGO b"), SCAN_CAP).expect("search");
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].command, "Cargo Build --release");
        assert!(!capped);
        let (all, _) = store.search(&Needle::new(""), SCAN_CAP).expect("search");
        assert_eq!(all.iter().map(|b| b.command.as_str()).collect::<Vec<_>>(), ["ls", "Cargo Build --release"], "newest first");
    }

    /// Only a finished block is history; the reader's watermark relies on
    /// this returning `None` for the tail that is still running.
    #[test]
    fn a_running_block_is_not_stored() {
        let mut b = block(1, "sleep 9", 100);
        b.state = zest_core::BlockState::Running;
        b.ended_ms = None;
        assert!(StoredBlock::from_block(RunId(1), SessionId(1), &b).is_none());
        b.state = zest_core::BlockState::Prompt;
        assert!(StoredBlock::from_block(RunId(1), SessionId(1), &b).is_none());
    }

    /// The REPLACE insurance: within one run an id never recurs, and a bug
    /// elsewhere that made one recur must not poison the whole history with
    /// a constraint error.
    #[test]
    fn the_same_run_and_id_replaces_rather_than_duplicates() {
        let store = BlockStore::in_memory().expect("open");
        store.sink().record(vec![stored(7, 1, "first", 100)]);
        store.sink().record(vec![stored(7, 1, "second", 150)]);
        settled(&store);
        assert_eq!(store.len().expect("count"), 1);
        let (hits, _) = store.search(&Needle::new(""), SCAN_CAP).expect("search");
        assert_eq!(hits[0].command, "second");
        // Another run, same id: a different block.
        store.sink().record(vec![stored(8, 1, "third", 160)]);
        settled(&store);
        assert_eq!(store.len().expect("count"), 2);
    }

    /// The count cap keeps the newest by end time, whatever order they were
    /// written in.
    #[test]
    fn the_cap_prunes_the_oldest_by_end_time() {
        let store = BlockStore::in_memory_with(3, MAX_AGE_MS).expect("open");
        // Written newest first, so "oldest" is a fact about the stamp and not
        // about insertion order; more than PRUNE_EVERY rows so a prune runs.
        let rows: Vec<StoredBlock> = (0..PRUNE_EVERY + 5)
            .map(|i| {
                let now = crate::session::unix_ms();
                stored(1, i as u32, &format!("cmd {i}"), now - i * 1000)
            })
            .collect();
        store.sink().record(rows);
        settled(&store);
        assert_eq!(store.len().expect("count"), 3, "three newest kept");
        let (hits, _) = store.search(&Needle::new(""), SCAN_CAP).expect("search");
        assert_eq!(hits.iter().map(|b| b.id.0).collect::<Vec<_>>(), [0, 1, 2]);
    }

    /// Age is measured from the end stamp, so a year-old row goes however
    /// few rows the store holds.
    #[test]
    fn a_year_old_block_is_pruned() {
        let store = BlockStore::in_memory_with(MAX_BLOCKS, 1000).expect("open");
        let now = crate::session::unix_ms();
        let mut rows = vec![stored(1, 0, "old", now - 10_000), stored(1, 1, "new", now)];
        rows.extend((2..PRUNE_EVERY as u32 + 2).map(|i| stored(1, i, "filler", now)));
        store.sink().record(rows);
        settled(&store);
        let (hits, _) = store.search(&Needle::new("old"), SCAN_CAP).expect("search");
        assert!(hits.is_empty(), "the old row went: {hits:?}");
        let (hits, _) = store.search(&Needle::new("new"), SCAN_CAP).expect("search");
        assert_eq!(hits.len(), 1);
    }

    /// A pasted script is cut on a character boundary and says so; a
    /// client must not re-run the first four kilobytes of it as if whole.
    #[test]
    fn a_pasted_script_is_cut_and_says_so() {
        // 'é' is two bytes; placed so the byte cap falls inside it.
        let mut script = "a".repeat(MAX_COMMAND_BYTES - 1);
        script.push('é');
        script.push_str(" && rm -rf build");
        let s = stored(1, 1, &script, 100);
        assert!(s.command_truncated);
        assert_eq!(s.command.len(), MAX_COMMAND_BYTES - 1, "cut before the split character");
        assert!(s.command.chars().all(|c| c == 'a'));
        let short = stored(1, 2, "ls", 100);
        assert!(!short.command_truncated);
        assert!(s.to_match(HostId::from_bytes([1; 32])).command_truncated, "the flag reaches the wire");
    }

    /// The strongest form of "never output": there is no column for it.
    #[test]
    fn search_never_returns_output() {
        let store = BlockStore::in_memory().expect("open");
        let columns = store.columns().expect("schema");
        for forbidden in ["output", "text", "rows", "lines", "venv", "kube", "env"] {
            assert!(!columns.iter().any(|c| c == forbidden), "no `{forbidden}` column: {columns:?}");
        }
        assert!(columns.iter().any(|c| c == "command"));
    }

    /// "Older history went unsearched" must mean a row exists past the cap,
    /// not that the scan happened to fill it: a store holding exactly `cap`
    /// rows is fully searched, and a zero cap searched nothing at all.
    #[test]
    fn the_scan_is_capped_only_when_a_row_lies_beyond_the_cap() {
        let store = BlockStore::in_memory().expect("open");
        store.sink().record((0..3).map(|i| stored(1, i, "ls", 100 + u64::from(i))).collect());
        settled(&store);
        let (hits, capped) = store.search(&Needle::new(""), 3).expect("search");
        assert_eq!(hits.len(), 3);
        assert!(!capped, "exactly the cap is fully searched");
        let (hits, capped) = store.search(&Needle::new(""), 2).expect("search");
        assert_eq!(hits.len(), 2);
        assert!(capped, "one row lies beyond a cap of two");
        let (hits, capped) = store.search(&Needle::new("ls"), 0).expect("search");
        assert!(hits.is_empty());
        assert!(capped, "a zero cap searched nothing, and there is something");
        let (hits, capped) = store.search(&Needle::new("zzz"), 2).expect("search");
        assert!(hits.is_empty() && !capped, "an ASCII query the SQL narrowed to nothing scanned nothing");
        let (hits, capped) = store.search(&Needle::new("日"), 2).expect("search");
        assert!(hits.is_empty() && capped, "unnarrowed, capped is about rows scanned, not rows matched");
    }

    /// Command history is not for the other accounts on a machine: a fresh
    /// history is 0600 in a 0700 directory from the moment it exists, and
    /// SQLite gives the WAL the same mode.
    #[cfg(unix)]
    #[test]
    fn a_new_history_is_private_from_birth() {
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!("zest-blocks-private-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let path = dir.join("state").join("blocks.sqlite");
        let store = BlockStore::open(&path).expect("open");
        store.sink().record(vec![stored(1, 1, "ls", 1)]);
        settled(&store);
        let mode = |p: &Path| std::fs::metadata(p).expect("exists").permissions().mode() & 0o777;
        assert_eq!(mode(&path), 0o600, "the file");
        assert_eq!(mode(path.parent().expect("dir")), 0o700, "the directory this created");
        let wal = dir.join("state").join("blocks.sqlite-wal");
        if wal.exists() {
            assert_eq!(mode(&wal), 0o600, "the WAL follows the main file's mode");
        }
        drop(store);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The narrowing must be decided on the query as typed: a Kelvin sign
    /// folds to `k`, and a decision on the fold would send it down the
    /// ASCII path, which is fine for the pattern and wrong for the claim
    /// the doc makes. It matches either way; what this pins is that
    /// `Needle::is_ascii` is the fact the store reads.
    #[test]
    fn a_query_that_folds_to_ascii_is_still_not_narrowed() {
        let store = BlockStore::in_memory().expect("open");
        store.sink().record(vec![stored(1, 1, "make", 100), stored(1, 2, "ls", 200)]);
        settled(&store);
        let needle = Needle::new("\u{212a}");
        assert!(!needle.is_ascii() && needle.folded() == "k");
        let (hits, _) = store.search(&needle, SCAN_CAP).expect("search");
        assert_eq!(hits.iter().map(|b| b.command.as_str()).collect::<Vec<_>>(), ["make"]);
    }

    /// `LIKE` folds ASCII only, so it is a superset only for an ASCII query;
    /// a non-ASCII one must skip the narrowing and still match.
    #[test]
    fn a_non_ascii_query_skips_the_sql_narrowing_and_still_matches() {
        let store = BlockStore::in_memory().expect("open");
        store.sink().record(vec![stored(1, 1, "echo 日本語", 100), stored(1, 2, "echo x_y%z", 200)]);
        settled(&store);
        let (hits, _) = store.search(&Needle::new("日本"), SCAN_CAP).expect("search");
        assert_eq!(hits.len(), 1);
        // And LIKE's own wildcards are escaped on the ASCII path.
        let (hits, _) = store.search(&Needle::new("x_y%z"), SCAN_CAP).expect("search");
        assert_eq!(hits.len(), 1);
        let (hits, _) = store.search(&Needle::new("xzy"), SCAN_CAP).expect("search");
        assert!(hits.is_empty(), "`_` is not a wildcard in a query");
    }

    /// The live row carries the session id `⇧⏎` and `output` need; the
    /// stored copy of the same block is the one that yields.
    #[test]
    fn merge_prefers_the_live_row_and_dedupes_on_run_and_id() {
        let host = HostId::from_bytes([1; 32]);
        let live_match =
            BlockMatch::from_block(host, Some(SessionId(3)), "zsh", &block(5, "make", 100));
        let live = vec![(RunId(9), live_match)];
        let stored_rows = vec![stored(9, 5, "make", 100), stored(9, 4, "older", 50), stored(2, 5, "other run", 75)];
        let (out, truncated) = merge(live, stored_rows, host, 10);
        assert!(!truncated);
        assert_eq!(
            out.iter().map(|m| (m.command.as_str(), m.session.is_some())).collect::<Vec<_>>(),
            [("make", true), ("other run", false), ("older", false)],
            "one `make`, the live one; a different run's block 5 is its own"
        );
    }

    /// The limit and its flag describe the union, not either half.
    #[test]
    fn merge_applies_the_limit_after_the_union() {
        let host = HostId::from_bytes([1; 32]);
        let live = vec![(RunId(1), BlockMatch::from_block(host, Some(SessionId(1)), "", &block(1, "a", 300)))];
        let stored_rows = vec![stored(2, 1, "b", 200), stored(2, 2, "c", 100)];
        let (out, truncated) = merge(live, stored_rows, host, 2);
        assert!(truncated);
        assert_eq!(out.iter().map(|m| m.command.as_str()).collect::<Vec<_>>(), ["a", "b"]);
    }

    /// The reader thread holds the terminal lock across a parse; a `record`
    /// that could park it there is ADR-016's keystroke stall. Structure
    /// asserted by behaviour: a queue that nobody drains never blocks the
    /// caller.
    #[test]
    fn record_never_blocks_the_caller() {
        let (tx, _rx) = sync_channel::<Op>(1);
        let sink = BlockSink { tx, complained: Arc::default() };
        assert!(sink.record(vec![stored(1, 1, "a", 1)]), "the one slot takes the first");
        let started = std::time::Instant::now();
        assert!(!sink.record(vec![stored(1, 2, "b", 2)]), "a full queue refuses rather than waits");
        assert!(started.elapsed() < Duration::from_millis(100));
        assert!(sink.record(Vec::new()), "nothing to record is not a refusal");
    }
}
