//! How a tool call names a session.
//!
//! `SessionAddr` is a 32-byte `HostId` and a `u64`, which spells out as 64 hex
//! characters plus a number. An agent reads and writes ids constantly — every
//! listing, every call, every result echoing back what it acted on — so the
//! full spelling is a measurable and entirely avoidable token cost. The short
//! form is `HostId::short()` (8 hex characters, which the fleet UI and every
//! diagnostic already print) and the session number:
//!
//! ```text
//! 2e2e2e2e:7
//! ```
//!
//! # Ambiguity is refused, never guessed
//!
//! A prefix can match two hosts. Picking one would send an agent's `run` to
//! whichever machine sorted first, which is the kind of wrong that looks right
//! in the transcript. [`Resolver::resolve`] returns [`AddrError::AmbiguousHost`]
//! carrying the candidates so the agent can say which it meant.
//!
//! # Only ids this process minted
//!
//! Terminal output is attacker-controlled: a build log can contain
//! `2e2e2e2e:7`, or a plausible-looking host id, alongside text arguing that it
//! should be used. [`Resolver`] therefore answers only for hosts it has itself
//! listed this session. That is not a formality — it is what stops a *fleet*
//! confused-deputy, where the damage of following an injected instruction is
//! that it lands on a different machine.

use std::collections::BTreeMap;

use zest_proto::{HostId, SessionAddr, SessionId};

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum AddrError {
    #[error("`{0}` is not a session id -- expected <host>:<session>, e.g. 2e2e2e2e:7")]
    Malformed(String),
    #[error("`{0}` is not a session number")]
    BadSession(String),
    #[error("no host matches `{asked}`; this server knows: {known}")]
    UnknownHost { asked: String, known: String },
    #[error("`{asked}` matches {} hosts ({candidates}) -- say which", .count)]
    AmbiguousHost { asked: String, count: usize, candidates: String },
}

/// The hosts this process has listed, and therefore the only ones it will name.
#[derive(Debug, Default)]
pub struct Resolver {
    /// Full hex -> label, so an error can list what *is* known.
    known: BTreeMap<String, (HostId, String)>,
}

/// Full 64-character hex, which `zest-proto` keeps to itself (`hex` is
/// `pub(crate)`). Only ever a lookup key here, never printed.
fn hex32(bytes: &[u8; 32]) -> String {
    use std::fmt::Write as _;
    bytes.iter().fold(String::with_capacity(64), |mut s, b| {
        let _ = write!(s, "{b:02x}");
        s
    })
}

impl Resolver {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Record a host as nameable. Idempotent; the label may be refreshed.
    pub fn learn(&mut self, host: HostId, label: &str) {
        self.known.insert(hex32(&host.0), (host, label.to_string()));
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.known.is_empty()
    }

    /// How this server spells an address back to the agent.
    ///
    /// `SessionAddr`'s own `Display` is already `<short>:<session>`, which is
    /// exactly this form -- so this is a name for the round trip rather than a
    /// second spelling that could drift from [`Resolver::resolve`].
    #[must_use]
    pub fn format(addr: SessionAddr) -> String {
        addr.to_string()
    }

    /// Parse `<host>:<session>` against the hosts this process has listed.
    ///
    /// The host part may be any unambiguous prefix of the hex, so the short
    /// form this server prints round-trips and a full 64-character id also
    /// works for anyone who has one.
    pub fn resolve(&self, s: &str) -> Result<SessionAddr, AddrError> {
        // `rsplit_once`, not `split_once`: the host part is hex and cannot hold
        // a colon today, but bounding on the *last* one means a future label
        // form cannot silently truncate a session number.
        let (host_part, session_part) =
            s.rsplit_once(':').ok_or_else(|| AddrError::Malformed(s.to_string()))?;
        if host_part.is_empty() {
            return Err(AddrError::Malformed(s.to_string()));
        }

        let session: u64 = session_part
            .parse()
            .map_err(|_| AddrError::BadSession(session_part.to_string()))?;

        let host = self.host(host_part)?;
        Ok(SessionAddr { host, session: SessionId(session) })
    }

    /// Resolve a host on its own, for the tools that take one.
    pub fn host(&self, asked: &str) -> Result<HostId, AddrError> {
        let needle = asked.to_ascii_lowercase();

        // An exact label match first, so a person's `--host andy-mac` works and
        // cannot be shadowed by a hex prefix that happens to look like it.
        //
        // **Two hosts may share a label**, and nothing prevents it: a label is
        // whatever the far machine calls itself, so two laptops both named
        // `mac` is a configuration, not a corruption. Falling through to the
        // hex branch would then report `UnknownHost` -- a label that plainly
        // matched two machines reported as matching none -- so the ambiguity is
        // raised here with both candidates, exactly as a hex prefix would be.
        let by_label: Vec<&(HostId, String)> = self
            .known
            .values()
            .filter(|(_, label)| label.to_ascii_lowercase() == needle)
            .collect();
        match by_label.len() {
            1 => return Ok(by_label[0].0),
            0 => {}
            n => return Err(Self::ambiguous(asked, n, &by_label)),
        }

        let matches: Vec<&(HostId, String)> =
            self.known.iter().filter(|(hex, _)| hex.starts_with(&needle)).map(|(_, v)| v).collect();

        match matches.len() {
            1 => Ok(matches[0].0),
            0 => Err(AddrError::UnknownHost {
                asked: asked.to_string(),
                known: self.summary(),
            }),
            n => Err(Self::ambiguous(asked, n, &matches)),
        }
    }

    /// One spelling of "say which", so the label and hex branches cannot drift
    /// into reporting the same situation two different ways.
    fn ambiguous(asked: &str, count: usize, matches: &[&(HostId, String)]) -> AddrError {
        AddrError::AmbiguousHost {
            asked: asked.to_string(),
            count,
            candidates: matches
                .iter()
                .map(|(h, l)| format!("{} ({l})", h.short()))
                .collect::<Vec<_>>()
                .join(", "),
        }
    }

    fn summary(&self) -> String {
        if self.known.is_empty() {
            return "nothing yet -- call `hosts` first".into();
        }
        self.known
            .values()
            .map(|(h, l)| format!("{} ({l})", h.short()))
            .collect::<Vec<_>>()
            .join(", ")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn host(byte: u8) -> HostId {
        HostId::from_bytes([byte; 32])
    }

    fn resolver() -> Resolver {
        let mut r = Resolver::new();
        r.learn(host(0x2e), "andy-win");
        r.learn(host(0xab), "andy-mac");
        r
    }

    #[test]
    fn the_short_form_this_server_prints_round_trips() {
        // The two halves must agree or every id the agent echoes back is
        // rejected, which reads to a model as the session having vanished.
        let r = resolver();
        let addr = SessionAddr { host: host(0x2e), session: SessionId(7) };
        let printed = Resolver::format(addr);
        assert_eq!(printed, "2e2e2e2e:7");
        assert_eq!(r.resolve(&printed), Ok(addr));
    }

    #[test]
    fn a_full_hex_id_still_works() {
        let r = resolver();
        let full = format!("{}:{}", hex32(&host(0xab).0), 3);
        assert_eq!(
            r.resolve(&full),
            Ok(SessionAddr { host: host(0xab), session: SessionId(3) }),
            "a caller holding a full id must not have to shorten it"
        );
    }

    #[test]
    fn an_ambiguous_prefix_is_refused_rather_than_picked() {
        // The one that matters in a fleet: guessing sends `run` to whichever
        // host sorted first, and the transcript looks entirely reasonable.
        let mut r = Resolver::new();
        r.learn(HostId::from_bytes([0x2e; 32]), "one");
        let mut second = [0x2e; 32];
        second[8] = 0xff;
        r.learn(HostId::from_bytes(second), "two");

        let err = r.resolve("2e:1").expect_err("a prefix matching two hosts must not resolve");
        assert!(
            matches!(err, AddrError::AmbiguousHost { count: 2, .. }),
            "expected an ambiguity naming both candidates, got {err:?}"
        );
    }

    #[test]
    fn a_host_this_process_never_listed_is_refused() {
        // The confused-deputy guard: a host id read out of terminal output --
        // a build log, a filename -- names nothing this server will act on.
        let r = resolver();
        let err = r.resolve("deadbeef:1").expect_err("an unlisted host must not resolve");
        assert!(
            matches!(err, AddrError::UnknownHost { .. }),
            "expected UnknownHost, got {err:?}"
        );
    }

    #[test]
    fn two_hosts_with_one_label_are_ambiguous_rather_than_unknown() {
        // Nothing stops two machines calling themselves the same thing -- a
        // label is whatever the far host says. Before this the duplicate fell
        // through to the hex branch, which no label can match, so a name that
        // plainly matched two machines was reported as matching none.
        let mut r = Resolver::new();
        r.learn(host(0x11), "mac");
        r.learn(host(0x22), "mac");

        let err = r.host("mac").expect_err("a label two hosts share must not resolve");
        match err {
            AddrError::AmbiguousHost { count, ref candidates, .. } => {
                assert_eq!(count, 2);
                assert!(
                    candidates.contains("11111111") && candidates.contains("22222222"),
                    "both candidates must be named so the caller can pick: {candidates}"
                );
            }
            other => panic!("expected AmbiguousHost, got {other:?}"),
        }
    }

    #[test]
    fn a_label_resolves_and_is_not_shadowed_by_hex() {
        let r = resolver();
        assert_eq!(r.host("andy-mac"), Ok(host(0xab)));
        assert_eq!(r.host("ANDY-MAC"), Ok(host(0xab)), "labels are matched case-insensitively");
    }

    #[test]
    fn a_missing_colon_is_a_parse_error_not_a_host_lookup() {
        let r = resolver();
        assert_eq!(
            r.resolve("2e2e2e2e"),
            Err(AddrError::Malformed("2e2e2e2e".into())),
            "without this the error would say `no host matches` for a session id that \
             simply lost its number"
        );
    }

    #[test]
    fn a_session_that_is_not_a_number_says_so() {
        let r = resolver();
        assert_eq!(r.resolve("2e2e2e2e:seven"), Err(AddrError::BadSession("seven".into())));
    }

    #[test]
    fn an_empty_resolver_names_the_first_call_to_make() {
        // A model that gets "no host matches" with no list retries the same
        // wrong id; one that is told to call `hosts` does the useful thing.
        let r = Resolver::new();
        let err = r.resolve("2e:1").expect_err("nothing is known yet");
        let msg = err.to_string();
        assert!(msg.contains("hosts"), "the error must name the call that fixes it: {msg}");
    }
}
