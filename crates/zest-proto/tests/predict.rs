//! The echo predictor replayed against `fixtures/predict.json`.
//!
//! The fixture is hand-authored, one scenario per rule in `predict.rs`'s
//! module docs, and the TypeScript port replays the same file
//! (`clients/web/packages/proto/test/predict.test.ts`). Two decoders reading
//! one fixture is what keeps the native app and the browser agreeing about
//! which glyphs are guesses — the alternative is two rule sets that drift the
//! way the three keyframe take-back rules did (#313).

use serde::Deserialize;
use zest_proto::{CursorState, Delta, Key, Policy, Predictor};

#[derive(Deserialize)]
struct Fixture {
    schema: u32,
    scenarios: Vec<Scenario>,
}

#[derive(Deserialize)]
struct Scenario {
    name: String,
    policy: String,
    remote_hint: bool,
    cols: u16,
    cursor: Pos,
    steps: Vec<Step>,
}

#[derive(Deserialize, Clone, Copy)]
struct Pos {
    row: u16,
    col: u16,
}

#[derive(Deserialize)]
struct Step {
    at: u64,
    input: Option<Input>,
    delta: Option<Delta>,
    keyframe: Option<KeyframeStep>,
    #[serde(default)]
    tick: bool,
    expect: Expect,
}

#[derive(Deserialize)]
#[serde(tag = "key", rename_all = "lowercase")]
enum Input {
    Printable { ch: char },
    Backspace,
    Other,
}

#[derive(Deserialize)]
struct KeyframeStep {
    cursor: Pos,
    cols: u16,
    alt_screen: bool,
}

#[derive(Deserialize)]
struct Expect {
    overlay: Vec<(u16, u16, char)>,
    pending: usize,
    showing: bool,
    latency_ms: Option<f32>,
}

fn cursor(p: Pos) -> CursorState {
    CursorState { row: p.row, col: p.col, visible: true, shape: 0 }
}

#[test]
fn every_scenario_in_the_fixture_replays() {
    let raw = include_str!("../fixtures/predict.json");
    let f: Fixture = serde_json::from_str(raw).expect("predict.json parses");
    assert_eq!(f.schema, 1, "fixture schema moved; update both replayers together");
    assert!(!f.scenarios.is_empty());

    for sc in &f.scenarios {
        let policy = match sc.policy.as_str() {
            "auto" => Policy::Auto,
            "always" => Policy::Always,
            "off" => Policy::Off,
            other => panic!("{}: unknown policy {other}", sc.name),
        };
        let mut p = Predictor::new(policy);
        p.set_remote_hint(sc.remote_hint);
        p.on_keyframe(cursor(sc.cursor), sc.cols, false);

        for (i, step) in sc.steps.iter().enumerate() {
            let here = format!("{} step {i} (at {})", sc.name, step.at);
            if let Some(input) = &step.input {
                let key = match input {
                    Input::Printable { ch } => Key::Printable(*ch),
                    Input::Backspace => Key::Backspace,
                    Input::Other => Key::Other,
                };
                p.on_input(key, step.at);
            }
            if let Some(d) = &step.delta {
                p.reconcile(d, step.at);
            }
            if let Some(k) = &step.keyframe {
                p.on_keyframe(cursor(k.cursor), k.cols, k.alt_screen);
            }
            if step.tick {
                p.tick(step.at);
            }

            let got: Vec<(u16, u16, char)> =
                p.overlay().iter().map(|x| (x.row, x.col, x.ch)).collect();
            assert_eq!(got, step.expect.overlay, "{here}: overlay");
            assert_eq!(p.pending().len(), step.expect.pending, "{here}: pending count");
            assert_eq!(p.showing(), step.expect.showing, "{here}: showing");
            match (p.echo_latency_ms(), step.expect.latency_ms) {
                (None, None) => {}
                (Some(g), Some(w)) => {
                    assert!((g - w).abs() < 0.01, "{here}: latency {g} != {w}")
                }
                (g, w) => panic!("{here}: latency {g:?}, expected {w:?}"),
            }
        }
    }
}

#[test]
fn the_caret_sits_after_the_last_guess() {
    let mut p = Predictor::new(Policy::Always);
    p.on_keyframe(cursor(Pos { row: 2, col: 5 }), 80, false);
    assert_eq!(p.caret(), None, "no guess, the grid's cursor is the caret");
    p.on_input(Key::Printable('a'), 0);
    p.on_input(Key::Printable('b'), 1);
    assert_eq!(p.caret(), Some((2, 7)), "the line must read as typed, caret included");
}
