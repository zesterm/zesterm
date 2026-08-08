//! An append-only record of who was allowed onto this machine, and on whose say-so.
//!
//! # Why this is not just logging
//!
//! Approving a device grants a shell. The only trace of that decision used to be
//! stdout, and stdout is wherever the operator happened to point it — so when a
//! peer asked *"was a person at the prompt when my device was approved?"*, the
//! honest answer was that the record had been destroyed by a `>` on a restart.
//! An authorization decision whose evidence an operator can erase by accident is
//! not evidence.
//!
//! So this writes beside the trust store rather than to a stream: the trust file
//! says *who is trusted*, this says *how they came to be*. Losing the second
//! makes the first unauditable.
//!
//! # The field that matters
//!
//! `by_person` records whether the answer arrived **after** the prompt was
//! printed. A `y` that predates its question is not consent — it cannot be, since
//! nobody can compare a code they have not been shown — and for one afternoon
//! this daemon could not tell the two apart. → #21.

use std::io::Write as _;
use std::path::{Path, PathBuf};

/// One authorization decision, as it will be read back months later.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    /// A person answered, after seeing the prompt.
    Approved,
    /// A person answered, after seeing the prompt, and said no.
    Refused,
    /// Nobody answered in time.
    Expired,
    /// Input arrived that could not have been an answer to this prompt.
    ///
    /// Kept distinct from `Expired` because they mean opposite things about the
    /// operator: one is nobody home, the other is an answer that was thrown away
    /// on purpose, and somebody may be wondering why their `y` did nothing.
    Discarded,
}

impl Outcome {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Approved => "approved",
            Self::Refused => "refused",
            Self::Expired => "expired",
            Self::Discarded => "discarded",
        }
    }
}

/// Where decisions are recorded.
pub struct Audit {
    path: PathBuf,
}

impl Audit {
    /// Beside the trust store, since the two are read together.
    #[must_use]
    pub fn beside(trust_file: &Path) -> Self {
        let path = trust_file
            .parent()
            .map_or_else(|| PathBuf::from("pairings.log"), |d| d.join("pairings.log"));
        Self { path }
    }

    #[must_use]
    pub fn at(path: PathBuf) -> Self {
        Self { path }
    }

    /// Append one decision.
    ///
    /// Failure is logged and swallowed: a daemon that refused to serve because it
    /// could not write a log would be trading a working fleet for a tidy record.
    /// It is `warn`, not `debug`, because a record nobody notices going missing is
    /// the state this whole file exists to prevent.
    pub fn record(&self, at: std::time::SystemTime, entry: &Entry<'_>) {
        let secs = at
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_secs());
        // One line, tab-separated, oldest first. Deliberately not TOML or JSON:
        // this file is appended to under a lock by a process that may be killed
        // mid-write, and a line-oriented format loses one entry in that case
        // rather than the whole document.
        let line = format!(
            "{secs}\t{}\t{}\t{}\t{}\t{}\t{}\n",
            entry.client,
            entry.label,
            entry.remote,
            entry.code,
            entry.outcome.as_str(),
            if entry.by_person { "by-person" } else { "not-by-person" },
        );

        // The directory may not exist yet: the trust store creates it lazily, and
        // the first decision of a machine's life is recorded *before* the record
        // of who it trusts is written. That ordering cost the very first entry
        // this file was built to keep.
        if let Some(dir) = self.path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }

        let written = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)
            .and_then(|mut f| f.write_all(line.as_bytes()));

        if let Err(e) = written {
            tracing::warn!(
                path = %self.path.display(),
                error = %e,
                "could not record a pairing decision; this machine can no longer \
                 answer who it let in"
            );
        }
    }
}

/// What is recorded about one decision.
pub struct Entry<'a> {
    pub client: &'a str,
    pub label: &'a str,
    pub remote: &'a str,
    pub code: &'a str,
    pub outcome: Outcome,
    /// Whether the answer arrived after the prompt was shown. See the module docs.
    pub by_person: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp() -> PathBuf {
        std::env::temp_dir().join(format!("zest-audit-{}.log", std::process::id()))
    }

    #[test]
    fn a_decision_survives_the_process_that_made_it() {
        // The whole point: this outlives stdout, and outlives a restart.
        let path = temp();
        let _ = std::fs::remove_file(&path);
        let audit = Audit::at(path.clone());
        audit.record(
            std::time::UNIX_EPOCH + std::time::Duration::from_secs(1_700_000_000),
            &Entry {
                client: "ab12cd34",
                label: "andy-phone",
                remote: "10.0.0.7:51234",
                code: "778245",
                outcome: Outcome::Approved,
                by_person: true,
            },
        );

        let text = std::fs::read_to_string(&path).expect("the record must be readable");
        assert!(text.contains("ab12cd34"), "who was approved: {text}");
        assert!(text.contains("778245"), "which code they were approved against: {text}");
        assert!(text.contains("by-person"), "and whether a person did it: {text}");
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn appending_never_loses_an_earlier_decision() {
        // A record that a later write can truncate is the failure this replaces:
        // an afternoon's approvals were unanswerable because a restart used `>`.
        let path = temp().with_extension("append");
        let _ = std::fs::remove_file(&path);
        let audit = Audit::at(path.clone());
        for (i, outcome) in [Outcome::Approved, Outcome::Refused, Outcome::Expired]
            .into_iter()
            .enumerate()
        {
            audit.record(
                std::time::UNIX_EPOCH + std::time::Duration::from_secs(i as u64),
                &Entry {
                    client: "cafe0001",
                    label: "device",
                    remote: "10.0.0.9:1",
                    code: "000000",
                    outcome,
                    by_person: false,
                },
            );
        }
        let text = std::fs::read_to_string(&path).expect("readable");
        assert_eq!(text.lines().count(), 3, "every decision is kept: {text}");
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn a_decision_is_recorded_even_before_the_directory_exists() {
        // The first approval on a fresh machine happens before the trust store
        // has written anything, so nothing has created the directory yet. That
        // is exactly the decision most worth keeping.
        let dir = std::env::temp_dir().join(format!("zest-audit-fresh-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let audit = Audit::at(dir.join("pairings.log"));
        audit.record(
            std::time::UNIX_EPOCH,
            &Entry {
                client: "feed0001",
                label: "first-ever",
                remote: "10.0.0.2:5",
                code: "111111",
                outcome: Outcome::Approved,
                by_person: true,
            },
        );
        let text = std::fs::read_to_string(dir.join("pairings.log"))
            .expect("the first decision must be kept, not lost to a missing directory");
        assert!(text.contains("first-ever"), "{text}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_discarded_answer_is_not_an_expiry() {
        // Opposite meanings for whoever reads this later: nobody was home, versus
        // somebody answered and the answer was deliberately thrown away.
        assert_ne!(Outcome::Discarded.as_str(), Outcome::Expired.as_str());
    }
}
