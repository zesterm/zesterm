//! Version migrations, as a chain of pure functions.
//!
//! Each step takes the table from version N to N+1 and knows nothing about any
//! other step. That is what keeps them correct years later: a v1→v2 migration
//! written today still only has to understand v1, no matter how many versions
//! follow it.
//!
//! Migrations run on `toml::Table`, before deserialization. They have to — a
//! renamed field cannot be expressed in the typed form, since the old name no
//! longer exists there.

use crate::settings::CURRENT_SCHEMA_VERSION;

/// One version step.
type Step = fn(&mut toml::Table);

/// The chain, indexed by the version it upgrades *from*.
///
/// Empty at v1 because nothing has needed migrating yet. The machinery exists
/// now so the first rename is a one-line addition rather than a design problem
/// discovered while users are already running the old format.
const STEPS: &[(u32, Step)] = &[];

/// What happened during a migration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Migration {
    pub from: u32,
    pub to: u32,
    /// Human-readable notes, for the toast and the log.
    pub notes: Vec<String>,
}

/// Bring a config table up to the current schema version.
///
/// A file from the *future* is left alone: its unknown keys will warn, which is
/// the right outcome. Downgrading it would destroy settings the user made with a
/// newer build, and refusing to start would strand anyone who tried a newer
/// version once.
pub fn migrate(table: &mut toml::Table) -> Option<Migration> {
    let from = table
        .get("schema_version")
        .and_then(toml::Value::as_integer)
        .and_then(|v| u32::try_from(v).ok())
        .unwrap_or(CURRENT_SCHEMA_VERSION);

    if from >= CURRENT_SCHEMA_VERSION {
        if from > CURRENT_SCHEMA_VERSION {
            tracing::warn!(
                file_version = from,
                supported = CURRENT_SCHEMA_VERSION,
                "config is from a newer zesterm; unknown settings will be ignored"
            );
        }
        return None;
    }

    let mut notes = Vec::new();
    let mut version = from;
    while version < CURRENT_SCHEMA_VERSION {
        match STEPS.iter().find(|(v, _)| *v == version) {
            Some((_, step)) => {
                step(table);
                notes.push(format!("migrated v{version} to v{}", version + 1));
            }
            // A gap in the chain is a programming error, not a user problem.
            // Stopping here leaves the file readable rather than half-migrated.
            None => {
                tracing::error!(version, "no migration step; leaving the config as it is");
                break;
            }
        }
        version += 1;
    }

    table.insert("schema_version".into(), toml::Value::Integer(i64::from(version)));
    Some(Migration { from, to: version, notes })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_current_file_is_left_alone() {
        let mut t: toml::Table = format!("schema_version = {CURRENT_SCHEMA_VERSION}")
            .parse()
            .expect("valid toml");
        assert_eq!(migrate(&mut t), None);
    }

    #[test]
    fn a_file_with_no_version_is_assumed_current() {
        // Every config written by hand starts this way. Treating a missing
        // version as v0 would run every migration over a modern file.
        let mut t: toml::Table = "[typography]\nsize_pt = 14.0".parse().expect("valid toml");
        assert_eq!(migrate(&mut t), None);
    }

    #[test]
    fn a_newer_file_is_not_downgraded() {
        let mut t: toml::Table = "schema_version = 99".parse().expect("valid toml");
        assert_eq!(migrate(&mut t), None);
        assert_eq!(t.get("schema_version").and_then(toml::Value::as_integer), Some(99));
    }

    #[test]
    fn every_step_in_the_chain_is_reachable() {
        // A gap means a file at the missing version can never be upgraded. This
        // is cheap to assert and impossible to notice otherwise.
        for version in (1..CURRENT_SCHEMA_VERSION).step_by(1) {
            assert!(
                STEPS.iter().any(|(v, _)| *v == version),
                "no migration step from v{version}"
            );
        }
    }
}
