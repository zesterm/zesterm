//! What a host says about itself on the wire.
//!
//! Deliberately free of any mDNS type: this module encodes and parses key/value
//! pairs and nothing else, which is what lets every test of the wire format run
//! with a `HashMap` and no socket. The library that actually carries the pairs
//! is one adapter away, in [`super::mdns`].
//!
//! # The rules that keep old and new hosts able to see each other
//!
//! Unknown keys are ignored, a newer `v` is still a host worth listing, and an
//! unparseable record is skipped rather than raised. That last one matters more
//! than it looks: `_zesterm._tcp.local.` is a name anyone on the link can
//! advertise, so malformed records are ambient noise, and a fleet listing that
//! throws when a stranger appears on the café Wi-Fi is worse than useless.

use zest_proto::HostId;

/// The service every zesterm host advertises under.
///
/// `_tcp` because the LAN transport is a TCP socket carrying `zest-proto`
/// frames. The trailing dot is required by both `mdns-sd` and `dns-sd`.
pub const SERVICE_TYPE: &str = "_zesterm._tcp.local.";

/// The current record generation. Bumped only for a change old hosts cannot
/// simply ignore.
pub const TXT_VERSION: u32 = 1;

pub const KEY_VERSION: &str = "v";
pub const KEY_HOST_ID: &str = "id";
pub const KEY_LABEL: &str = "lbl";

/// The longest label that will be advertised, in bytes.
///
/// One DNS character-string caps at 255 bytes and the whole record should fit a
/// single packet. 63 leaves the record around 140 bytes, with room for keys a
/// later version adds.
pub const MAX_LABEL_BYTES: usize = 63;

/// The longest instance name that will be registered, in bytes, before the
/// ` (short-id)` suffix.
const MAX_INSTANCE_LABEL_BYTES: usize = 24;

/// What one host claims about itself, once decoded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Advertisement {
    pub host: HostId,
    pub label: String,
    pub version: u32,
}

impl Advertisement {
    #[must_use]
    pub fn new(host: HostId, label: &str) -> Self {
        Self {
            host,
            label: truncate_bytes(label, MAX_LABEL_BYTES).to_string(),
            version: TXT_VERSION,
        }
    }

    /// The pairs to hand a responder, in a stable order so that a diff of two
    /// packet captures is readable.
    ///
    /// The port is deliberately absent: SRV already carries it, and two sources
    /// of truth that can disagree produce a connection refused with no obvious
    /// cause.
    #[must_use]
    pub fn to_pairs(&self) -> Vec<(String, String)> {
        vec![
            (KEY_VERSION.to_string(), self.version.to_string()),
            (KEY_HOST_ID.to_string(), encode_host_id(self.host)),
            (KEY_LABEL.to_string(), self.label.clone()),
        ]
    }

    /// Decode, given a way to look a key up.
    ///
    /// Takes a lookup closure rather than a concrete map so that this function
    /// never mentions the mDNS library — which is the whole reason the parsing
    /// tests need no network.
    ///
    /// `None` means "not one of ours, skip it": a squatted service name, a
    /// truncated id, a record from something else entirely. Never an error.
    #[must_use]
    pub fn parse<'a>(
        get: impl Fn(&str) -> Option<&'a str>,
        fallback_label: &str,
    ) -> Option<Self> {
        let host = decode_host_id(get(KEY_HOST_ID)?)?;

        // A missing version is generation 1; a present but unreadable one means
        // we are not looking at a zesterm record after all.
        let version = match get(KEY_VERSION) {
            Some(text) => text.parse().ok()?,
            None => TXT_VERSION,
        };

        // A label that is missing or empty falls back to the instance name, so
        // a peer always has something to render. A blank row in a fleet listing
        // reads as a bug in the listing.
        let label = get(KEY_LABEL)
            .filter(|l| !l.is_empty())
            .unwrap_or(fallback_label)
            .to_string();

        Some(Self {
            host,
            label,
            version,
        })
    }
}

/// The DNS-SD instance name for a host.
///
/// The short-id suffix makes this collision-free by construction. Two machines
/// both labelled "mac" would otherwise trigger the responder's conflict
/// resolution, which silently renames one to "mac (2)" — and then the name in
/// `dns-sd -B` no longer matches what the user typed, which is a confusing
/// thing to be debugging at 11pm.
///
/// The short id here is **for humans only** and is never parsed back into a
/// [`HostId`]; eight hex characters is four bytes, and reconstructing an
/// identity by truncation is exactly what `zest-proto` rejects. The
/// authoritative id is always the `id` key.
#[must_use]
pub fn instance_name(label: &str, host: HostId) -> String {
    let sanitized = sanitize_instance_label(label);
    format!("{sanitized} ({})", host.short())
}

/// The instance portion of `"andy-mac (1f2a3b4c)._zesterm._tcp.local."`.
#[must_use]
pub fn instance_of(fullname: &str) -> &str {
    fullname
        .split_once("._")
        .map_or(fullname, |(instance, _)| instance)
}

/// `[A-Za-z0-9 _-]` only, collapsed and trimmed, and never empty.
///
/// Dots in particular must go: DNS-SD escapes a `.` inside an instance label as
/// `\.` in the fullname, and round-tripping that escape through our own
/// fullname index is a bug waiting to happen.
fn sanitize_instance_label(label: &str) -> String {
    let mut out = String::with_capacity(label.len());
    for ch in label.chars() {
        if ch.is_ascii_alphanumeric() || matches!(ch, ' ' | '_' | '-') {
            out.push(ch);
        } else if !out.ends_with('-') {
            out.push('-');
        }
    }
    let trimmed = truncate_bytes(out.trim().trim_matches('-'), MAX_INSTANCE_LABEL_BYTES).trim();
    if trimmed.is_empty() {
        "zesterm".to_string()
    } else {
        trimmed.to_string()
    }
}

/// Cut to at most `max` bytes without splitting a character.
fn truncate_bytes(text: &str, max: usize) -> &str {
    if text.len() <= max {
        return text;
    }
    let mut end = max;
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    &text[..end]
}

fn encode_host_id(host: HostId) -> String {
    host.0.iter().fold(String::with_capacity(64), |mut acc, b| {
        use std::fmt::Write as _;
        let _ = write!(acc, "{b:02x}");
        acc
    })
}

/// Exactly 64 hex characters, or nothing.
///
/// Never left-padded and never truncated. A short id that got padded out would
/// compare equal to a machine that is not the one that sent it, which is a
/// security bug wearing a parsing bug's clothes.
fn decode_host_id(text: &str) -> Option<HostId> {
    if text.len() != 64 {
        return None;
    }
    let mut bytes = [0u8; 32];
    for (i, byte) in bytes.iter_mut().enumerate() {
        *byte = u8::from_str_radix(text.get(i * 2..i * 2 + 2)?, 16).ok()?;
    }
    Some(HostId::from_bytes(bytes))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn lookup(pairs: &[(String, String)]) -> HashMap<String, String> {
        pairs.iter().cloned().collect()
    }

    fn host(seed: u8) -> HostId {
        HostId::from_bytes([seed; 32])
    }

    #[test]
    fn a_txt_record_survives_the_round_trip() {
        let advertised = Advertisement::new(host(0x1f), "andy-mac");
        let map = lookup(&advertised.to_pairs());
        let parsed = Advertisement::parse(|k| map.get(k).map(String::as_str), "fallback")
            .expect("our own record must parse");
        assert_eq!(
            parsed, advertised,
            "if a host cannot read what it just wrote, nothing else in discovery matters"
        );
    }

    #[test]
    fn an_unknown_key_does_not_make_a_record_unreadable() {
        let mut map = lookup(&Advertisement::new(host(2), "mac").to_pairs());
        map.insert("os".into(), "macos".into());
        map.insert("future".into(), "whatever".into());
        assert!(
            Advertisement::parse(|k| map.get(k).map(String::as_str), "fallback").is_some(),
            "a later version adding a key must not make its hosts vanish from an \
             older version's fleet listing"
        );
    }

    #[test]
    fn a_record_with_a_malformed_host_id_is_skipped_rather_than_raising() {
        let map: HashMap<String, String> =
            [("v".into(), "1".into()), ("id".into(), "not-hex".into())].into();
        assert!(
            Advertisement::parse(|k| map.get(k).map(String::as_str), "fallback").is_none(),
            "the service name is squattable, so a malformed record is link noise; \
             treating link noise as an error is how a fleet listing starts throwing"
        );
    }

    #[test]
    fn a_record_with_no_host_id_is_skipped() {
        let map: HashMap<String, String> = [("lbl".into(), "something-else".into())].into();
        assert!(
            Advertisement::parse(|k| map.get(k).map(String::as_str), "fallback").is_none(),
            "a record with no id names no machine, so there is nothing to list"
        );
    }

    #[test]
    fn a_short_host_id_is_not_padded_out_to_a_different_machine() {
        let map: HashMap<String, String> = [("id".into(), "ab".repeat(16))].into();
        assert!(
            Advertisement::parse(|k| map.get(k).map(String::as_str), "fallback").is_none(),
            "padding 32 hex characters out to 64 invents a machine that never \
             advertised, and the id is also the public key"
        );
    }

    #[test]
    fn a_record_from_a_newer_version_is_still_a_host_on_your_lan() {
        let mut map = lookup(&Advertisement::new(host(3), "future-box").to_pairs());
        map.insert("v".into(), "9".into());
        let parsed = Advertisement::parse(|k| map.get(k).map(String::as_str), "fallback")
            .expect("a good id is enough to list a host");
        assert_eq!(
            parsed.version, 9,
            "hiding a host we might not be able to dial is worse than showing it \
             and failing to dial it"
        );
    }

    #[test]
    fn a_record_with_no_label_falls_back_to_the_instance_name() {
        let map: HashMap<String, String> = [("id".into(), "11".repeat(32))].into();
        let parsed = Advertisement::parse(|k| map.get(k).map(String::as_str), "mac (11111111)")
            .expect("an id alone is a host");
        assert_eq!(
            parsed.label, "mac (11111111)",
            "a blank row in a fleet listing reads as a bug in the listing"
        );
    }

    #[test]
    fn a_label_with_a_dot_cannot_break_the_instance_name() {
        let name = instance_name("andy.mac", host(4));
        assert!(
            !name.contains('.'),
            "DNS-SD escapes a dot inside a label as `\\.` in the fullname, and our \
             own fullname index would then have to un-escape it: {name}"
        );
    }

    #[test]
    fn an_instance_name_is_never_empty_however_odd_the_label() {
        assert_eq!(
            instance_of(&format!("{}._zesterm._tcp.local.", instance_name("...", host(5)))),
            "zesterm (05050505)",
            "a responder given an empty instance name registers something \
             unpredictable, and then nothing can be found by name"
        );
    }

    #[test]
    fn an_instance_name_round_trips_out_of_a_fullname() {
        let name = instance_name("build box", host(6));
        let fullname = format!("{name}.{SERVICE_TYPE}");
        assert_eq!(
            instance_of(&fullname),
            name,
            "a removal event carries only the fullname, so the instance has to be \
             recoverable from it"
        );
    }

    #[test]
    fn an_advertised_record_fits_in_one_packet() {
        let label = "x".repeat(MAX_LABEL_BYTES);
        let pairs = Advertisement::new(host(7), &label).to_pairs();
        let total: usize = pairs.iter().map(|(k, v)| k.len() + v.len() + 2).sum();
        assert!(
            total < 400,
            "a record that needs two packets invites truncation and a TCP \
             fallback, and neither is worth a longer label: {total} bytes"
        );
        for (k, v) in &pairs {
            assert!(
                k.len() + v.len() < 255,
                "one DNS character-string caps at 255 bytes: {k} is over"
            );
        }
    }

    #[test]
    fn a_label_longer_than_the_limit_is_cut_on_a_character_boundary() {
        let advertised = Advertisement::new(host(8), &"é".repeat(60));
        assert!(
            advertised.label.len() <= MAX_LABEL_BYTES && advertised.label.chars().count() > 0,
            "cutting mid-character produces a label that is not UTF-8, which the \
             responder will reject or mangle: {:?}",
            advertised.label
        );
    }
}
