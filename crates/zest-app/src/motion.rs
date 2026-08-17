//! Springs, and the one rule they all obey: they stop.
//!
//! Terminal motion is *interruption-dominated* — the cursor moves again
//! mid-flight, the wheel fires again mid-scroll — which is why this is a spring
//! and not an easing curve. A spring absorbs a changed target with continuous
//! velocity for free; an easing curve has to be re-based against its new start,
//! and shows a visible velocity discontinuity every time it is. That argument is
//! [#5](https://github.com/zesterm/zesterm/issues/5)'s and it is decisive; it is
//! also why the settings are `motion.spring_response` and
//! `motion.spring_damping` rather than a duration and the name of a curve.
//!
//! **Every animator must provably settle.** An animator that only approaches
//! its target asymptotically burns GPU for ever at 0.01px a frame, and an idle
//! terminal using 0% GPU is a hard requirement here rather than a nicety. So
//! [`Spring::step`] snaps exactly to the target inside an epsilon and reports
//! that it has come to rest, and the caller stops asking for frames. The test
//! that matters is not "does it look smooth" but "does it stop".

/// Rest thresholds, in the units the spring is carrying.
///
/// Rows for the scroll spring, so `1/512` of a row is far below a pixel at any
/// realistic cell height — the eye cannot see it and the renderer cannot draw
/// it, which is exactly the point at which continuing to animate is waste.
/// Velocity has to be small *too*: a spring passing through its target at speed
/// is momentarily within epsilon of it, and snapping there would cut off the
/// overshoot an under-damped spring is supposed to show.
const REST_VALUE: f32 = 1.0 / 512.0;
const REST_VELOCITY: f32 = 1.0 / 64.0;

/// The integrator's fixed inner step: 240Hz.
///
/// A spring integrated once per frame is a different spring at 60Hz and at
/// 144Hz — same numbers, visibly different motion, and the faster display gets
/// the *worse* one. Substepping to a fixed rate makes the feel a property of
/// `response` and `damping` alone, which is what lets those two settings mean
/// something a user can carry between machines.
const SUBSTEP_HZ: f32 = 240.0;

/// A critically-damped-by-default second-order spring.
///
/// Carries one scalar. Callers that animate a point run two.
#[derive(Debug, Clone, Copy, Default)]
pub struct Spring {
    value: f32,
    velocity: f32,
    target: f32,
}

impl Spring {
    /// A spring at rest at `value`.
    #[must_use]
    pub fn at(value: f32) -> Self {
        Self { value, velocity: 0.0, target: value }
    }

    /// Where the animation currently is — what the renderer should draw.
    #[must_use]
    pub fn value(&self) -> f32 {
        self.value
    }

    /// Whether this spring still needs frames.
    #[must_use]
    pub fn moving(&self) -> bool {
        self.value != self.target || self.velocity != 0.0
    }

    /// Aim somewhere new, keeping the current velocity.
    ///
    /// Keeping it is the whole reason this is a spring: a wheel notch arriving
    /// mid-scroll should bend the motion, not restart it.
    pub fn retarget(&mut self, target: f32) {
        self.target = target;
    }

    /// Displace the *current* value, leaving the target and the velocity alone.
    ///
    /// What a wheel notch does to a scroll: the grid has already moved, so the
    /// spring is now that many rows further behind and has to catch up. Keeping
    /// the velocity is the point — a second notch arriving mid-glide should
    /// make the motion faster, not restart it from a standstill.
    pub fn nudge(&mut self, delta: f32) {
        self.value += delta;
    }

    /// Give up on animating and be there now.
    ///
    /// Used when motion is switched off, when the OS asks for reduced motion,
    /// and when a frame ran long — motion is the first thing to sacrifice under
    /// load, since a `cat` flood needs the bytes drawn far more than it needs
    /// the scroll eased.
    pub fn snap_to(&mut self, target: f32) {
        self.value = target;
        self.target = target;
        self.velocity = 0.0;
    }

    /// Advance by `dt` seconds. Returns whether the spring is *still moving*.
    ///
    /// `response` is roughly the time to reach the target and `damping` is the
    /// ratio — 1.0 critically damped, below that overshoots — which is exactly
    /// how `motion.spring_response` and `motion.spring_damping` are documented.
    pub fn step(&mut self, dt: f32, response: f32, damping: f32) -> bool {
        if !self.moving() {
            return false;
        }
        // A pathological `dt` — a breakpoint, a laptop lid, a stalled frame —
        // must not be integrated as real time: it would fling the spring
        // somewhere absurd and then spend frames coming back. Treat anything
        // beyond a few frames as a stall and arrive instead.
        if !dt.is_finite() || dt <= 0.0 || dt > 0.25 {
            self.snap_to(self.target);
            return false;
        }
        let response = response.clamp(0.01, 2.0);
        let damping = damping.clamp(0.1, 2.0);
        // Angular frequency from the response time, the standard
        // (response, damping ratio) parameterization.
        let omega = std::f32::consts::TAU / response;

        let steps = (dt * SUBSTEP_HZ).ceil().max(1.0);
        let h = dt / steps;
        for _ in 0..steps as u32 {
            // Semi-implicit Euler: velocity first, then position from the
            // *new* velocity. Explicit Euler gains energy at these step sizes
            // and an under-damped spring slowly diverges instead of settling,
            // which is the failure this whole module is written to avoid.
            let accel = -omega * omega * (self.value - self.target)
                - 2.0 * damping * omega * self.velocity;
            self.velocity += accel * h;
            self.value += self.velocity * h;
        }

        if (self.value - self.target).abs() <= REST_VALUE && self.velocity.abs() <= REST_VELOCITY {
            self.snap_to(self.target);
            return false;
        }
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const RESPONSE: f32 = 0.16;
    const CRITICAL: f32 = 1.0;

    /// Run a spring to rest, or give up. Returns the frames it took.
    fn settle(s: &mut Spring, dt: f32, damping: f32) -> u32 {
        for frame in 1..=100_000 {
            if !s.step(dt, RESPONSE, damping) {
                return frame;
            }
        }
        panic!("a spring that never stops is the bug this module exists to prevent");
    }

    #[test]
    fn a_spring_reaches_rest_and_says_so() {
        // The load-bearing property. An animator that only approaches its
        // target asymptotically burns GPU for ever at 0.01px a frame, and
        // 0%-idle is a hard requirement rather than a nicety.
        for damping in [0.2, 0.5, CRITICAL, 1.5, 2.0] {
            let mut s = Spring::at(0.0);
            s.retarget(10.0);
            let frames = settle(&mut s, 1.0 / 60.0, damping);
            assert!(!s.moving(), "damping {damping} must come to rest");
            assert_eq!(s.value(), 10.0, "and land exactly on the target, not near it");
            assert!(frames < 600, "damping {damping} took {frames} frames -- ten seconds is a hang");
        }
    }

    #[test]
    fn it_settles_from_a_standing_start_and_from_speed() {
        // A spring given velocity toward its target overshoots and has to come
        // back; one thrown away from it has further to travel. Both must stop.
        for velocity in [-40.0, -5.0, 5.0, 40.0] {
            let mut s = Spring::at(0.0);
            s.velocity = velocity;
            s.retarget(3.0);
            settle(&mut s, 1.0 / 60.0, CRITICAL);
            assert!(!s.moving(), "starting velocity {velocity} must still settle");
            assert_eq!(s.value(), 3.0);
        }
    }

    #[test]
    fn refresh_rate_does_not_change_the_feel() {
        // The reason for substepping. Integrated once per frame, the same
        // numbers are a different spring at 60Hz and at 144Hz -- and the
        // faster display gets the worse one, which is the wrong way round.
        let run = |dt: f32, frames: u32| {
            let mut s = Spring::at(0.0);
            s.retarget(1.0);
            for _ in 0..frames {
                s.step(dt, RESPONSE, CRITICAL);
            }
            s.value()
        };
        // A sixth of a second of motion, sampled three ways.
        let at_60 = run(1.0 / 60.0, 10);
        let at_144 = run(1.0 / 144.0, 24);
        let at_240 = run(1.0 / 240.0, 40);
        assert!(
            (at_60 - at_144).abs() < 0.02 && (at_144 - at_240).abs() < 0.02,
            "same elapsed time must look the same: 60Hz {at_60}, 144Hz {at_144}, 240Hz {at_240}"
        );
    }

    #[test]
    fn a_stalled_frame_arrives_rather_than_flinging() {
        // A breakpoint, a closed lid, a stalled GPU. Integrating a two-second
        // `dt` as real time throws the spring somewhere absurd and then spends
        // frames crawling back, which reads as the terminal lurching when it
        // wakes up.
        let mut s = Spring::at(0.0);
        s.retarget(5.0);
        assert!(!s.step(2.0, RESPONSE, CRITICAL), "a stall ends the animation");
        assert_eq!(s.value(), 5.0);

        // And the degenerate ones cannot produce a NaN that poisons the value
        // for the rest of the session.
        let mut s = Spring::at(0.0);
        s.retarget(5.0);
        assert!(!s.step(f32::NAN, RESPONSE, CRITICAL));
        assert!(s.value().is_finite());
    }

    #[test]
    fn a_spring_at_rest_asks_for_no_frames() {
        // What `anim_deadline` reads. A spring on its target must report `false`
        // without being stepped at all, or an idle window schedules a wake for
        // an animation that has nothing to do.
        let mut s = Spring::at(4.0);
        assert!(!s.moving());
        assert!(!s.step(1.0 / 60.0, RESPONSE, CRITICAL));
        s.retarget(4.0);
        assert!(!s.moving(), "retargeting to where it already is is not motion");
    }

    #[test]
    fn retargeting_mid_flight_keeps_the_velocity() {
        // The whole reason this is a spring rather than an easing curve: a
        // wheel notch arriving mid-scroll bends the motion instead of
        // restarting it, with no velocity discontinuity to see.
        let mut s = Spring::at(0.0);
        s.retarget(10.0);
        for _ in 0..5 {
            s.step(1.0 / 60.0, RESPONSE, CRITICAL);
        }
        let flying = s.velocity;
        assert!(flying > 0.0, "the fixture must actually be in motion");
        s.retarget(20.0);
        assert_eq!(s.velocity, flying, "retargeting must not discard momentum");
        settle(&mut s, 1.0 / 60.0, CRITICAL);
        assert_eq!(s.value(), 20.0);
    }

    #[test]
    fn snapping_ends_the_animation_immediately() {
        // Motion off, reduce-motion on, or a missed frame budget.
        let mut s = Spring::at(0.0);
        s.retarget(9.0);
        s.step(1.0 / 60.0, RESPONSE, CRITICAL);
        s.snap_to(9.0);
        assert!(!s.moving());
        assert_eq!(s.value(), 9.0);
    }

    #[test]
    fn an_underdamped_spring_actually_overshoots() {
        // Otherwise the rest threshold is simply cutting every spring off early
        // and `spring_damping` would be a setting with no observable effect --
        // which is the class of bug this whole sweep is closing.
        let mut s = Spring::at(0.0);
        s.retarget(1.0);
        let mut peak: f32 = 0.0;
        for _ in 0..240 {
            s.step(1.0 / 240.0, RESPONSE, 0.3);
            peak = peak.max(s.value());
        }
        assert!(peak > 1.0, "damping 0.3 must overshoot; peaked at {peak}");
    }
}
