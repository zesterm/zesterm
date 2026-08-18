//! `src/keys.rs` against the encoder the native window uses.
//!
//! `crates/zest-input/src/key.rs` is the source of truth for what a keystroke
//! becomes on the wire. `clients/web/packages/input/src/key.ts` is a
//! case-for-case port of it and says so in its header. `zest-mcp`'s `keys.rs`
//! is the **third** implementation, and it exists only because `zest-input`
//! takes `winit` types in its public API while this crate is a headless stdio
//! binary that must not carry a window toolkit.
//!
//! Three copies of one rule is the shape that gave `Grid::drop_scrollback_rows`
//! three different semantics, found one crate at a time. So this file is the
//! mechanism rather than the promise: every name the table accepts is encoded
//! both ways and the bytes compared. A "fix" to one of the quirks below fails
//! here rather than shipping as a fleet-wide disagreement about what Down is.
//!
//! What it cannot reach is reported rather than skipped -- see the two refusal
//! tests at the bottom.
//!
//! `zest_input::key::encode_press` exists for this test. `encode` takes a
//! `winit::KeyEvent`, which has a private platform tail and cannot be built
//! outside winit; `encode_press` takes the same key by its parts and builds the
//! same internal `KeyPress`, so there is still exactly one encoder.

use winit::keyboard::{Key, KeyLocation, ModifiersState, NamedKey};
use zest_core::Modes;
use zest_mcp::keys::{self, Chord, Mods, Named, NAMES};

/// Our name for a key, as winit spells it.
fn reference(named: Named) -> Key {
    Key::Named(match named {
        Named::Enter => NamedKey::Enter,
        Named::Esc => NamedKey::Escape,
        Named::Tab => NamedKey::Tab,
        Named::Backspace => NamedKey::Backspace,
        Named::Delete => NamedKey::Delete,
        Named::Insert => NamedKey::Insert,
        Named::Space => NamedKey::Space,
        Named::Up => NamedKey::ArrowUp,
        Named::Down => NamedKey::ArrowDown,
        Named::Left => NamedKey::ArrowLeft,
        Named::Right => NamedKey::ArrowRight,
        Named::Home => NamedKey::Home,
        Named::End => NamedKey::End,
        Named::PageUp => NamedKey::PageUp,
        Named::PageDown => NamedKey::PageDown,
        Named::F(1) => NamedKey::F1,
        Named::F(2) => NamedKey::F2,
        Named::F(3) => NamedKey::F3,
        Named::F(4) => NamedKey::F4,
        Named::F(5) => NamedKey::F5,
        Named::F(6) => NamedKey::F6,
        Named::F(7) => NamedKey::F7,
        Named::F(8) => NamedKey::F8,
        Named::F(9) => NamedKey::F9,
        Named::F(10) => NamedKey::F10,
        Named::F(11) => NamedKey::F11,
        Named::F(12) => NamedKey::F12,
        Named::F(n) => panic!("the parser admits only f1-f12, got f{n}"),
    })
}

/// The eight ctrl/alt/shift combinations, ours and winit's, from one counter.
fn mods_of(bits: u8) -> (Mods, ModifiersState) {
    let (ctrl, alt, shift) = (bits & 1 != 0, bits & 2 != 0, bits & 4 != 0);
    let mut winit = ModifiersState::empty();
    if ctrl {
        winit |= ModifiersState::CONTROL;
    }
    if alt {
        winit |= ModifiersState::ALT;
    }
    if shift {
        winit |= ModifiersState::SHIFT;
    }
    (Mods { ctrl, alt, shift }, winit)
}

/// The reference encoding, which must exist for every key this table names.
///
/// `None` from `zest-input` means "nothing to send", and no key in our table is
/// allowed to be one -- a name that parses and encodes to nothing is a key that
/// silently does nothing, which is the whole complaint in #345.
fn reference_bytes(key: &Key, mods: ModifiersState, modes: Modes) -> Vec<u8> {
    zest_input::key::encode_press(key, KeyLocation::Standard, mods, modes)
        .unwrap_or_else(|| panic!("zest-input encodes nothing for {key:?} with {mods:?}"))
}

fn prefix(m: Mods) -> String {
    let mut s = String::new();
    if m.ctrl {
        s.push_str("ctrl+");
    }
    if m.alt {
        s.push_str("alt+");
    }
    if m.shift {
        s.push_str("shift+");
    }
    s
}

/// Every base, every modifier combination, DECCKM both ways.
///
/// 27 bases x 8 combinations x 2 modes. Milliseconds, and the only thing
/// standing between three copies of this table and three behaviours.
#[test]
fn every_named_key_agrees_with_the_encoder_the_window_uses() {
    let mut bases: Vec<Named> = NAMES.iter().map(|(_, n)| *n).collect();
    bases.extend((1..=12).map(Named::F));
    bases.dedup();

    let mut checked = 0usize;
    for modes in [Modes::empty(), Modes::APP_CURSOR] {
        for named in &bases {
            for bits in 0..8u8 {
                let (mine, theirs) = mods_of(bits);

                // Spell it the way a caller would, and parse it, so the grammar
                // is exercised by the same sweep rather than trusted.
                let spelled = format!("{}{}", prefix(mine), name_of(*named));
                let chord: Chord = spelled
                    .parse()
                    .unwrap_or_else(|e| panic!("`{spelled}` must parse: {e}"));
                assert_eq!(chord.mods, mine, "`{spelled}` parsed the wrong modifiers");

                let ours = keys::encode(&chord, modes);
                let reference = reference_bytes(&reference(*named), theirs, modes);

                assert_eq!(
                    ours, reference,
                    "`{spelled}` under {modes:?}: this table and zest-input disagree.\n  \
                     zest-mcp:   {ours:?}\n  zest-input: {reference:?}"
                );
                checked += 1;
            }
        }
    }
    assert!(checked >= 400, "the sweep must actually cover the table, ran {checked}");
}

/// The name to spell a base with. Canonical, not an alias.
fn name_of(named: Named) -> String {
    match named {
        Named::F(n) => format!("f{n}"),
        other => NAMES
            .iter()
            .find(|(_, n)| *n == other)
            .map(|(s, _)| (*s).to_string())
            .unwrap_or_else(|| panic!("{other:?} has no name in NAMES")),
    }
}

/// `ctrl+<letter>` and `alt+<letter>`, which go through the character path.
#[test]
fn control_and_alt_characters_agree() {
    for ch in b'a'..=b'z' {
        let ch = char::from(ch);
        for (spelled, bits) in [(format!("ctrl+{ch}"), 1u8), (format!("alt+{ch}"), 2)] {
            let (_, theirs) = mods_of(bits);
            let chord: Chord = spelled.parse().expect("a control chord parses");
            let key = Key::Character(ch.to_string().into());
            assert_eq!(
                keys::encode(&chord, Modes::empty()),
                reference_bytes(&key, theirs, Modes::empty()),
                "`{spelled}` disagrees"
            );
        }
    }
}

/// What the sweep cannot reach, stated rather than silently uncovered.
///
/// winit hands the encoder `Key::Character("A")` for Shift+A -- the character a
/// *layout* produced -- and this table has no layout. So the grammar refuses
/// the spelling outright, and there is nothing to compare bytes for. Asserting
/// the refusal is the honest version of a skip.
#[test]
fn shifted_characters_are_refused_rather_than_guessed() {
    assert!(
        "shift+a".parse::<Chord>().is_err(),
        "this table cannot know what a shifted key produces on someone else's layout"
    );
}

/// Likewise a bare modifier: `zest-input` returns `None` for it -- a modifier
/// press must not wake the pty at all -- and our grammar has no spelling for
/// one. Both halves, so neither can quietly grow a byte.
#[test]
fn a_bare_modifier_encodes_to_nothing_on_both_sides() {
    assert!("ctrl".parse::<Chord>().is_err(), "a modifier alone is not a chord here");
    assert_eq!(
        zest_input::key::encode_press(
            &Key::Named(NamedKey::Control),
            KeyLocation::Standard,
            ModifiersState::empty(),
            Modes::empty(),
        ),
        None,
        "a bare modifier must not wake the pty"
    );
}

/// The base set is the one the tool description advertises.
///
/// A key added to the table and left out of the schema is one no model will
/// ever send; the reverse is one that refuses when sent.
#[test]
fn the_table_and_its_vocabulary_agree() {
    for (name, _) in NAMES {
        assert!(
            keys::VOCABULARY.contains(name) || is_alias(name),
            "`{name}` is in the table but not in the vocabulary a refusal prints"
        );
    }
}

fn is_alias(name: &str) -> bool {
    matches!(name, "escape" | "return" | "del" | "ins" | "pgup" | "pgdn" | "pgdown")
}
