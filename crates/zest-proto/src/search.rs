//! A command block as a search hit, and the one rule every surface applies
//! to decide whether it is one (#527).
//!
//! The daemon answers [`crate::ClientMessage::SearchBlocks`] from the sessions
//! it owns, the app merges the answers of every host it holds a connection to,
//! `zest-mcp` returns them as a tool result, and a durable store (ADR-020)
//! filters its rows through the same predicate before answering. Four
//! readers of one rule, so the rule lives here — on the wire type they all
//! already share — rather than in any one of them. `zest-core` cannot hold it
//! (this struct carries `ts_rs` and `zest-core` builds for `wasm32` without
//! it), and `zest-fleet` must not: the daemon answers a per-host question and
//! should not learn fleet vocabulary to do it.

use serde::{Deserialize, Serialize};

use crate::{delta::BlockContextPayload, delta::BlockState, ClientId, HostId, SessionId};

/// One command block, as a search answer carries it.
///
/// **Not [`crate::BlockPayload`]**, deliberately. That type's three line ids
/// mean something only against the grid that issued them; off it they invite
/// a client to treat a search hit as a keyframe block and look rows up that
/// it does not hold. And a stored block of a session that has ended (ADR-020)
/// has no lines at all, so a shared type would carry a `prompt_line` that is
/// sometimes a fact and sometimes a placeholder — a field nothing can fill
/// reads exactly like one nothing fills (#299).
///
/// Options here are spelled as plain `null`s on the wire, not skipped keys:
/// this message is a reply and never rides a delta, so the bytes an absent
/// key would save are not worth a second spelling for the TypeScript reader
/// to handle.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
pub struct BlockMatch {
    /// Whose block. The daemon fills in its own id, exactly as
    /// `Registry::list` fills `SessionInfo.addr`; a client merging several
    /// hosts' answers keys on it.
    pub host: HostId,
    /// The session that holds the block — `None` for a block whose session
    /// is gone and which only a store remembers. A client may re-run the
    /// command anywhere, but there is nothing to *activate*: ids restart at
    /// one on every daemon start, so a dead session's number names a live
    /// stranger.
    pub session: Option<SessionId>,
    /// `Block::id`, stable for the session's life. With `session`, what
    /// `output` in `zest-mcp` takes to fetch the text this deliberately
    /// omits.
    pub block: u32,
    /// The session's title at answer time, so a row can say *which* zsh when
    /// a host has six. Empty when the session is gone or unnamed.
    #[serde(default)]
    pub title: String,
    pub command: String,
    /// The command was longer than the store keeps and was cut (ADR-020).
    /// History to read, not a thing to re-run: a client must not type the
    /// first four kilobytes of a pasted script as if they were the whole.
    /// Additive; a live block never sets it.
    #[serde(default)]
    pub command_truncated: bool,
    pub cwd: String,
    pub state: BlockState,
    /// Wall clock at OSC 133;C, milliseconds since the Unix epoch — the
    /// host's stamp, so "2m ago" on another machine means something against
    /// a shared epoch.
    #[cfg_attr(feature = "ts", ts(type = "number | null"))]
    pub started_ms: Option<u64>,
    /// Wall clock at OSC 133;D, same epoch. `None` while it runs.
    #[cfg_attr(feature = "ts", ts(type = "number | null"))]
    pub ended_ms: Option<u64>,
    /// Branch, venv, cluster as of the command's start — the same payload
    /// the keyframe carries, so `zest-mcp`'s `context` key reads identically
    /// from `blocks` and from `search_blocks`.
    #[serde(default)]
    pub context: Option<BlockContextPayload>,
    /// The client the daemon saw type it. Provenance, never authorization —
    /// see [`crate::BlockPayload::author`].
    #[serde(default)]
    pub author: Option<ClientId>,
}

/// What a host returns when the request names no limit.
pub const LIMIT_DEFAULT: u32 = 50;
/// The most a host returns whatever is asked. ADR-015: the caller is a
/// model as often as a person, and an argument that can lift a ceiling is
/// not a ceiling.
pub const LIMIT_CAP: u32 = 200;

/// The limit a host actually applies: zero means the default, anything
/// above the cap is the cap.
#[must_use]
pub fn clamp_limit(asked: u32) -> usize {
    let n = if asked == 0 { LIMIT_DEFAULT } else { asked.min(LIMIT_CAP) };
    n as usize
}

/// A query, folded once, ready to be asked of every block in a fleet.
///
/// Case-insensitive substring over the command line, and nothing else: the
/// rule the palette already applies to every row it filters, so a block a
/// host returns is one the local filter would have kept. Whole-string
/// folding rather than `zest_core::search`'s per-character fold, because
/// that one exists so a match never moves a *column* — a search hit maps to
/// no column, so the constraint does not apply and the simpler rule is the
/// honest one. An empty query matches everything, which is what an opening
/// palette asks: the most recent blocks.
///
/// Over `command` only. Matching on `cwd` would surface every `ls` ever run
/// in a directory called `src`; the Sessions group already answers for
/// directories.
///
/// A type rather than a `fn(command, query)`, so the query is folded once
/// per search and not once per block: the predicate runs over every block
/// of every session on every keystroke, and a free function would invite
/// each caller to re-fold inside the loop.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Needle {
    folded: String,
}

impl Needle {
    #[must_use]
    pub fn new(query: &str) -> Self {
        Self { folded: query.to_lowercase() }
    }

    /// The query as folded, for a store that narrows in SQL before asking
    /// [`Self::matches`] to decide.
    #[must_use]
    pub fn folded(&self) -> &str {
        &self.folded
    }

    /// Does `command` match?
    #[must_use]
    pub fn matches(&self, command: &str) -> bool {
        if self.folded.is_empty() {
            return true;
        }
        command.to_lowercase().contains(&self.folded)
    }
}

/// When a block happened, for ordering: its end if it has one, its start
/// while it runs, zero for a block from a host too old to stamp either.
#[must_use]
pub fn recency(m: &BlockMatch) -> u64 {
    m.ended_ms.or(m.started_ms).unwrap_or(0)
}

/// Newest first; among blocks with one stamp, the higher block id first.
///
/// The tie is ordinary, not a corner: a shell that prints two short commands
/// in one read chunk finishes both under one millisecond stamp, and a sort
/// on the stamp alone leaves them in index order — oldest first, the wrong
/// way round for the newest-first list every surface promises. Ids count up
/// within a session, so the id is the tiebreak that means "later"; across
/// sessions with one stamp it is merely deterministic.
pub fn rank(matches: &mut [BlockMatch]) {
    matches.sort_by_key(|m| std::cmp::Reverse((recency(m), m.block)));
}

impl BlockMatch {
    /// The search form of a block the host parsed.
    #[must_use]
    pub fn from_block(
        host: HostId,
        session: Option<SessionId>,
        title: &str,
        b: &zest_core::Block,
    ) -> Self {
        Self {
            host,
            session,
            block: b.id.0,
            title: title.to_string(),
            command: b.command.clone(),
            command_truncated: false,
            cwd: b.cwd.clone(),
            state: match b.state {
                zest_core::BlockState::Prompt => BlockState::Prompt,
                zest_core::BlockState::Running => BlockState::Running,
                zest_core::BlockState::Finished { exit_code } => {
                    BlockState::Finished { exit_code }
                }
            },
            started_ms: b.started_ms,
            ended_ms: b.ended_ms,
            context: b.context.as_ref().map(|c| BlockContextPayload {
                branch: c.branch.clone(),
                venv: c.venv.clone(),
                kube: c.kube.clone(),
            }),
            author: b.author.map(ClientId::from_bytes),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hit(id: u32, started: Option<u64>, ended: Option<u64>) -> BlockMatch {
        BlockMatch {
            host: HostId::from_bytes([1; 32]),
            session: Some(SessionId(1)),
            block: id,
            title: String::new(),
            command: format!("cmd {id}"),
            command_truncated: false,
            cwd: "/".into(),
            state: match ended {
                Some(_) => BlockState::Finished { exit_code: Some(0) },
                None => BlockState::Running,
            },
            started_ms: started,
            ended_ms: ended,
            context: None,
            author: None,
        }
    }

    /// The one rule daemon, app, mcp and the store share: if it drifted in
    /// one of them, a block a host returned would vanish from the palette's
    /// own filter, or the reverse.
    #[test]
    fn an_empty_query_matches_every_command_and_case_never_matters() {
        let hit = |command: &str, query: &str| Needle::new(query).matches(command);
        assert!(hit("cargo build", ""), "an opening palette asks for everything");
        assert!(hit("Cargo Build --release", "cargo b"));
        assert!(hit("cargo build", "CARGO"), "the query folds too");
        assert!(!hit("cargo build", "cargo t"));
        assert!(hit("ls src", "src"), "substring, not word");
        assert!(!hit("", "x"));
    }

    /// The daemon, the fleet merge and the tool must all agree on order, or
    /// the same query lists the same blocks differently on two surfaces.
    #[test]
    fn ranking_is_newest_first_and_a_running_block_sorts_by_its_start() {
        let mut m = vec![
            hit(1, Some(100), Some(200)),
            hit(2, Some(500), None),
            hit(3, Some(300), Some(400)),
            hit(4, None, None),
            // Two commands finished in one read chunk share a stamp; the
            // later id is the later command.
            hit(5, Some(100), Some(200)),
        ];
        rank(&mut m);
        assert_eq!(
            m.iter().map(|b| b.block).collect::<Vec<_>>(),
            [2, 3, 5, 1, 4],
            "a running block sits where it started; a shared stamp breaks on id; an unstamped one sinks"
        );
    }

    /// A ceiling the caller can lift is not a ceiling (ADR-015).
    #[test]
    fn a_zero_limit_is_the_default_and_the_cap_holds() {
        assert_eq!(clamp_limit(0), LIMIT_DEFAULT as usize);
        assert_eq!(clamp_limit(7), 7);
        assert_eq!(clamp_limit(u32::MAX), LIMIT_CAP as usize);
    }
}
