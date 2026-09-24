//! Guided channel detection: prompts for one control at a time, watches which channels
//! move, and records where the control is mixed.
//!
//! Sticks and S1 are found from one movement that ends at their "positive" end (stick
//! up/right, the end of S1 that should read +100%), so the sign of the held value tells us
//! whether the channel is inverted. Switches are held in each position in turn, so any
//! wiring shows: one channel, copies on several, or a channel per position. Time is passed
//! in by the caller, which keeps this deterministic to test.

use std::cmp::Reverse;

use serde::Serialize;

use crate::config::{Mapping, Positions, Source};
use crate::hid::Report;
use crate::overlay::{is_on, switch_pos};

#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq, Hash)]
pub enum Target {
    #[serde(rename = "left_y")]
    LeftY,
    #[serde(rename = "left_x")]
    LeftX,
    #[serde(rename = "right_y")]
    RightY,
    #[serde(rename = "right_x")]
    RightX,
    SA,
    SB,
    SC,
    SD,
    SE,
    S1,
}

impl Target {
    pub const ALL: [Target; 10] = [
        Target::LeftY,
        Target::LeftX,
        Target::RightY,
        Target::RightX,
        Target::SA,
        Target::SB,
        Target::SC,
        Target::SD,
        Target::SE,
        Target::S1,
    ];

    /// How many positions a switch is held in, `None` for the sticks and S1. They're asked
    /// for toward you (or pressed) first, then away, then the middle.
    pub fn positions(self) -> Option<usize> {
        match self {
            Target::SA | Target::SD | Target::SE => Some(2),
            Target::SB | Target::SC => Some(3),
            _ => None,
        }
    }

    /// What to do next; `phase` counts the positions already held.
    pub fn prompt(self, phase: usize) -> &'static str {
        match (self, phase) {
            (Target::LeftY, _) => "LEFT stick: push fully UP and hold",
            (Target::LeftX, _) => "LEFT stick: push fully RIGHT and hold",
            (Target::RightY, _) => "RIGHT stick: push fully UP and hold",
            (Target::RightX, _) => "RIGHT stick: push fully RIGHT and hold",
            (Target::S1, _) => "S1: roll it to one end, then to the other and hold",
            (Target::SA, 0) => "SA: flip it TOWARD you and hold",
            (Target::SA, _) => "SA: now AWAY from you and hold",
            (Target::SB, 0) => "SB: flip it all the way TOWARD you and hold",
            (Target::SB, 1) => "SB: now all the way AWAY from you and hold",
            (Target::SB, _) => "SB: now to the MIDDLE and hold",
            (Target::SC, 0) => "SC: flip it all the way TOWARD you and hold",
            (Target::SC, 1) => "SC: now all the way AWAY from you and hold",
            (Target::SC, _) => "SC: now to the MIDDLE and hold",
            (Target::SD, 0) => "SD: flip it TOWARD you and hold",
            (Target::SD, _) => "SD: now AWAY from you and hold",
            (Target::SE, 0) => "SE: press and hold",
            (Target::SE, _) => "SE: now let go",
        }
    }

    pub fn hint(self, phase: usize) -> &'static str {
        match (self, phase) {
            (Target::LeftY | Target::LeftX | Target::RightY | Target::RightX, _) => {
                "If it's already there, move it the other way first."
            }
            (Target::S1, _) => "The end you finish on reads +100%.",
            (Target::SE, 0) => "Not mixed to a channel? Skip it.",
            (_, 0) => "Already there? Flip it away and back. Not mixed to a channel? Skip it.",
            _ => "",
        }
    }
}

/// A channel counts as "moved" once its range this step reaches this...
const MIN_RANGE: i32 = 900;
/// ...and it's being held at least this far from center...
const MIN_HELD: i16 = 800;
/// ...within this much jitter...
const HOLD_JITTER: i32 = 60;
/// ...for this long.
pub const HOLD_MS: u64 = 400;
/// The runner-up must have moved less than this fraction of the winner, or we wait.
const AMBIGUITY: f32 = 0.6;
/// A switch is in a new position once a channel has changed this much...
const MIN_STEP: i32 = 400;
/// ...except a middle that looks like an end on every channel (a switch on one on/off
/// channel): that's taken once the switch has stayed put this long after the ends.
pub const MIDDLE_WAIT_MS: u64 = 2000;
/// Channels within this of each other (or of mirroring each other) all step long carry
/// the same control, e.g. a stick mixed to two channels.
const TWIN: i32 = 4;

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct Found {
    pub target: Target,
    /// `None` when the step was skipped. Only SB and SC get a channel per position.
    pub source: Option<Mapping>,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct LearnStatus {
    pub step: usize,
    pub total: usize,
    /// `None` once every step is done.
    pub target: Option<Target>,
    pub prompt: &'static str,
    pub hint: &'static str,
    /// Channel currently being held, if any.
    pub candidate: Option<usize>,
    /// 0..=1 progress of the hold.
    pub hold: f32,
    /// More than one channel is moving.
    pub ambiguous: bool,
    pub found: Vec<Found>,
    /// Set by the engine once the result has been written to the config file.
    pub saved: Option<String>,
}

#[derive(Debug, Clone, Copy)]
struct Hold {
    /// The channel that moved, if one did (shown on the page).
    ch: Option<usize>,
    /// Its value when the hold started.
    value: i16,
    since: u64,
    /// How long it has to stay put.
    ms: u64,
}

#[derive(Debug, Clone)]
pub struct Learner {
    steps: Vec<Target>,
    found: Vec<Found>,
    /// Per-channel (min, max) since the current step started.
    ranges: Vec<(i16, i16)>,
    /// Latest value of every channel; the next step starts from here.
    last: Vec<i16>,
    /// Every channel at each switch position held so far this step.
    held: Vec<Vec<i16>>,
    /// Every channel when the current hold started (switches).
    still: Vec<i16>,
    /// For each pair of channels (`i * n + j`): bit 1 once they've differed this step,
    /// bit 2 once they've stopped mirroring each other.
    apart: Vec<u8>,
    hold: Option<Hold>,
    ambiguous: bool,
    last_now: u64,
}

impl Default for Learner {
    fn default() -> Self {
        Self::new(Target::ALL.to_vec())
    }
}

impl Learner {
    pub fn new(steps: Vec<Target>) -> Self {
        Self {
            steps,
            found: Vec::new(),
            ranges: Vec::new(),
            last: Vec::new(),
            held: Vec::new(),
            still: Vec::new(),
            apart: Vec::new(),
            hold: None,
            ambiguous: false,
            last_now: 0,
        }
    }

    pub fn current(&self) -> Option<Target> {
        self.steps.get(self.found.len()).copied()
    }

    pub fn is_done(&self) -> bool {
        self.current().is_none()
    }

    pub fn found(&self) -> &[Found] {
        &self.found
    }

    pub fn skip(&mut self) {
        if let Some(target) = self.current() {
            self.finish_step(Found {
                target,
                source: None,
            });
        }
    }

    fn finish_step(&mut self, found: Found) {
        self.found.push(found);
        self.begin_step();
    }

    fn begin_step(&mut self) {
        let n = self.last.len();
        self.ranges = self.last.iter().map(|&v| (v, v)).collect();
        self.held.clear();
        self.apart = vec![0; n * n];
        self.hold = None;
        self.ambiguous = false;
    }

    /// Channels no earlier step has claimed, lowest first (so CH1-8 before CH9-32).
    fn free(&self) -> impl Iterator<Item = usize> + '_ {
        let taken = |ch| {
            self.found
                .iter()
                .any(|f| f.source.is_some_and(|m| m.uses(ch)))
        };
        (1..=self.last.len()).filter(move |&ch| !taken(ch))
    }

    /// Feed the latest report (call on every report and periodically, so holds complete
    /// even when the radio stops sending changes).
    pub fn update(&mut self, now_ms: u64, report: &Report) {
        self.last_now = now_ms;
        let Some(target) = self.current() else { return };

        self.last = (1..=report.channel_count())
            .map(|ch| report.channel(ch).unwrap_or(0))
            .collect();
        if self.ranges.len() != self.last.len() {
            // the first report, or a radio with a different layout: start the step afresh
            self.begin_step();
        }
        for (r, &v) in self.ranges.iter_mut().zip(&self.last) {
            *r = (r.0.min(v), r.1.max(v));
        }
        match target.positions() {
            Some(n) => self.walk(now_ms, target, n),
            None => self.movement(now_ms, target, report.axes.len()),
        }
    }

    /// Sticks and S1: the channel that moved furthest, once it's held near an end.
    fn movement(&mut self, now_ms: u64, target: Target, axes: usize) {
        self.note_twins();
        // A control on both kinds of channel is read from CH1-8, which carry every
        // position; CH9-32 (on/off) only count when nothing on CH1-8 moved.
        let (mut best, mut second) = self.busiest(|ch| ch <= axes);
        if best.is_none_or(|(_, range)| range < MIN_RANGE) {
            (best, second) = self.busiest(|_| true);
        }

        let Some((ch, range)) = best else {
            self.hold = None;
            return;
        };
        self.ambiguous = range >= MIN_RANGE && second as f32 >= range as f32 * AMBIGUITY;
        let v = self.last[ch - 1];
        if range < MIN_RANGE || self.ambiguous || v.abs() < MIN_HELD {
            self.hold = None;
            return;
        }
        match self.hold {
            Some(h)
                if h.ch == Some(ch) && (i32::from(v) - i32::from(h.value)).abs() <= HOLD_JITTER =>
            {
                if now_ms.saturating_sub(h.since) >= h.ms {
                    let source = Source { ch, invert: v < 0 };
                    self.finish_step(Found {
                        target,
                        source: Some(source.into()),
                    });
                }
            }
            _ => {
                self.hold = Some(Hold {
                    ch: Some(ch),
                    value: v,
                    since: now_ms,
                    ms: HOLD_MS,
                })
            }
        }
    }

    /// Among the free channels `allowed` lets through: the one that moved most this step
    /// with its range, and the range of the runner-up that isn't a copy of it.
    fn busiest(&self, allowed: impl Fn(usize) -> bool) -> (Option<(usize, i32)>, i32) {
        let moved = |ch: usize| {
            let (lo, hi) = self.ranges[ch - 1];
            i32::from(hi) - i32::from(lo)
        };
        let best = self
            .free()
            .filter(|&ch| allowed(ch))
            .map(|ch| (ch, moved(ch)))
            .max_by_key(|&(ch, range)| (range, Reverse(ch)));
        let second = best.map_or(0, |(b, _)| {
            let others = self
                .free()
                .filter(|&ch| allowed(ch) && ch != b && !self.twins(ch, b));
            others.map(moved).max().unwrap_or(0)
        });
        (best, second)
    }

    /// Notes which pairs of channels stopped matching, or mirroring, each other.
    fn note_twins(&mut self) {
        let n = self.last.len();
        for i in 0..n {
            for j in i + 1..n {
                let apart = &mut self.apart[i * n + j];
                let (a, b) = (i32::from(self.last[i]), i32::from(self.last[j]));
                if (a - b).abs() > TWIN {
                    *apart |= 1;
                }
                if (a + b).abs() > TWIN {
                    *apart |= 2;
                }
            }
        }
    }

    /// The two channels have matched, or mirrored, each other all step.
    fn twins(&self, a: usize, b: usize) -> bool {
        let (n, i, j) = (self.last.len(), a.min(b) - 1, a.max(b) - 1);
        self.apart[i * n + j] != 3
    }

    /// Switches: each position is held in turn; then the positions say how it's wired.
    fn walk(&mut self, now_ms: u64, target: Target, n: usize) {
        // How far a channel has moved: for the first position, at all since the step began
        // (it may have been there already, so flipped away and back); after that, from
        // the position before.
        let change = |ch: usize| match self.held.last() {
            None => {
                let (lo, hi) = self.ranges[ch - 1];
                i32::from(hi) - i32::from(lo)
            }
            Some(from) => (i32::from(self.last[ch - 1]) - i32::from(from[ch - 1])).abs(),
        };
        let moved = self
            .free()
            .map(|ch| (ch, change(ch)))
            .filter(|&(_, d)| d >= MIN_STEP)
            .max_by_key(|&(ch, d)| (d, Reverse(ch)))
            .map(|(ch, _)| ch);
        // the middle, asked for last, may look like an end: then it's taken after a wait
        let unseen_middle = n == 3 && self.held.len() == 2 && self.ends_on_one_signal();
        if moved.is_none() && !unseen_middle {
            self.hold = None;
            return;
        }
        let still = self.hold.is_some()
            && self.free().all(|ch| {
                (i32::from(self.last[ch - 1]) - i32::from(self.still[ch - 1])).abs() <= HOLD_JITTER
            });
        match self.hold {
            Some(h) if still => {
                if now_ms.saturating_sub(h.since) < h.ms {
                    return;
                }
                self.held.push(self.last.clone());
                self.hold = None;
                if self.held.len() == n {
                    match self.wiring(n) {
                        Some(m) => self.finish_step(Found {
                            target,
                            source: Some(m),
                        }),
                        // nothing reads it (a stray movement?): ask again, from here
                        None => self.begin_step(),
                    }
                }
            }
            _ => {
                self.still = self.last.clone();
                self.hold = Some(Hold {
                    ch: moved,
                    value: moved.map_or(0, |ch| self.last[ch - 1]),
                    since: now_ms,
                    ms: if moved.is_some() {
                        HOLD_MS
                    } else {
                        MIDDLE_WAIT_MS
                    },
                });
            }
        }
    }

    /// Only one signal told the ends apart (one channel, or exact copies of it). Two
    /// different ones, like a channel per end, always show the middle too.
    fn ends_on_one_signal(&self) -> bool {
        let (toward, away) = (&self.held[0], &self.held[1]);
        let differ: Vec<usize> = self
            .free()
            .filter(|&ch| (i32::from(toward[ch - 1]) - i32::from(away[ch - 1])).abs() >= MIN_STEP)
            .collect();
        let same = |a: i16, b: i16| (i32::from(a) - i32::from(b)).abs() <= TWIN;
        differ.first().is_some_and(|&first| {
            differ.iter().all(|&ch| {
                same(toward[ch - 1], toward[first - 1]) && same(away[ch - 1], away[first - 1])
            })
        })
    }

    /// How a switch is wired, from every channel at each position it was held in.
    fn wiring(&self, n: usize) -> Option<Mapping> {
        // channel `ch` at position `p` (0 = away); held toward first, then away, then middle
        let at = |p: usize, ch: usize| {
            let phase = if p == n - 1 {
                0
            } else if p == 0 {
                1
            } else {
                2
            };
            self.held[phase][ch - 1]
        };
        let apart = |ch, p, q| (i32::from(at(p, ch)) - i32::from(at(q, ch))).abs() >= MIN_STEP;
        let free: Vec<usize> = self.free().collect();

        // one channel that reads every position (the usual mix; CH1-8 come first)
        for &ch in &free {
            for invert in [false, true] {
                let reads = (0..n).all(|p| {
                    let v = f32::from(at(p, ch)) / 1024.0;
                    usize::from(switch_pos(if invert { -v } else { v }, n as u8)) == p
                });
                if reads && (1..n).all(|p| apart(ch, p, p - 1)) {
                    return Some(Source { ch, invert }.into());
                }
            }
        }
        // a channel per position, on only while the switch is there (two or three of them)
        if n == 3 {
            let only = |p: usize| {
                free.iter().copied().find(|&ch| {
                    (0..n).all(|q| {
                        q == p || (is_on(at(p, ch)) && !is_on(at(q, ch)) && apart(ch, p, q))
                    })
                })
            };
            let positions = Positions {
                up: only(0),
                mid: only(1),
                down: only(2),
            };
            if positions.channels().iter().flatten().count() >= 2 {
                return Some(Mapping::Positions(positions));
            }
        }
        // one channel that tells the ends apart: on/off, so the middle reads like one end
        let toward = n - 1;
        let ch = free.into_iter().find(|&ch| apart(ch, toward, 0))?;
        let invert = at(toward, ch) < at(0, ch);
        Some(Source { ch, invert }.into())
    }

    pub fn status(&self) -> LearnStatus {
        let target = self.current();
        let phase = self.held.len();
        let hold = self.hold.map_or(0.0, |h| {
            (self.last_now.saturating_sub(h.since) as f32 / h.ms as f32).min(1.0)
        });
        LearnStatus {
            step: self.found.len(),
            total: self.steps.len(),
            target,
            prompt: target.map_or("All done", |t| t.prompt(phase)),
            hint: target.map_or("", |t| t.hint(phase)),
            candidate: self.hold.and_then(|h| h.ch),
            hold,
            ambiguous: self.ambiguous,
            found: self.found.clone(),
            saved: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn report(axes: &[i16], buttons: &[bool]) -> Report {
        Report {
            axes: axes.to_vec(),
            buttons: buttons.to_vec(),
        }
    }

    /// Holds `r` for `ms`, updating every 100 ms.
    fn keep(l: &mut Learner, t: &mut u64, r: &Report, ms: u64) {
        for _ in 0..ms / 100 {
            *t += 100;
            l.update(*t, r);
        }
    }

    /// Holds `r` long enough to complete a hold.
    fn hold(l: &mut Learner, t: &mut u64, r: &Report) {
        keep(l, t, r, 600);
    }

    fn one(ch: usize, invert: bool) -> Option<Mapping> {
        Some(Source { ch, invert }.into())
    }

    /// Walks a switch through its positions: each is the buttons (CH2 up) at that position,
    /// toward you first, then away, then the middle.
    fn walk(l: &mut Learner, t: &mut u64, positions: &[&[bool]]) {
        for buttons in positions {
            hold(l, t, &report(&[0], buttons));
        }
    }

    #[test]
    fn learns_channel_and_direction() {
        let mut l = Learner::new(vec![Target::LeftY, Target::SE]);
        let mut t = 0;
        // Throttle channel 2 is reversed in the model: parked at +1024, pushed up reads -1024.
        l.update(t, &report(&[0, 1024], &[false]));
        hold(&mut l, &mut t, &report(&[0, -1024], &[false]));
        assert_eq!(
            l.found()[0],
            Found {
                target: Target::LeftY,
                source: one(2, true),
            }
        );

        assert_eq!(l.status().prompt, "SE: press and hold");
        hold(&mut l, &mut t, &report(&[0, -1024], &[true]));
        assert_eq!(l.status().prompt, "SE: now let go");
        hold(&mut l, &mut t, &report(&[0, -1024], &[false]));
        assert_eq!(l.found()[1].source, one(3, false));
        assert!(l.is_done());
    }

    #[test]
    fn waits_while_ambiguous_and_skips() {
        let mut l = Learner::new(vec![Target::RightX, Target::S1]);
        let mut t = 0;
        l.update(t, &report(&[0, 0], &[]));
        hold(&mut l, &mut t, &report(&[1024, 900], &[])); // diagonal: both moved
        assert!(l.status().ambiguous);
        assert_eq!(l.current(), Some(Target::RightX));
        l.skip();
        assert_eq!(
            l.found()[0],
            Found {
                target: Target::RightX,
                source: None
            }
        );
        assert_eq!(l.current(), Some(Target::S1));
    }

    #[test]
    fn a_stick_on_two_channels_is_one_control() {
        // the same stick mixed to CH1 and CH2 (e.g. two aileron servos), then mirrored
        for sign in [1, -1] {
            let mut l = Learner::new(vec![Target::RightX]);
            let mut t = 0;
            for v in [0, 300, 700] {
                t += 50;
                l.update(t, &report(&[v, sign * v], &[]));
            }
            hold(&mut l, &mut t, &report(&[1024, sign * 1024], &[]));
            assert_eq!(l.found()[0].source, one(1, false), "sign {sign}");
        }
    }

    #[test]
    fn two_sticks_pushed_together_are_still_ambiguous() {
        // same end, but a hand never moves two sticks in lockstep
        let mut l = Learner::new(vec![Target::RightX]);
        let mut t = 0;
        l.update(t, &report(&[0, 0], &[]));
        l.update(50, &report(&[300, 700], &[]));
        t += 50;
        hold(&mut l, &mut t, &report(&[1024, 1024], &[]));
        assert!(l.status().ambiguous);
        assert!(!l.is_done());
    }

    #[test]
    fn jitter_restarts_hold() {
        let mut l = Learner::new(vec![Target::S1]);
        l.update(0, &report(&[-1024], &[]));
        l.update(100, &report(&[1000], &[]));
        l.update(300, &report(&[900], &[]));
        l.update(600, &report(&[1000], &[]));
        assert!(!l.is_done());
        l.update(1000, &report(&[1000], &[]));
        assert!(l.is_done());
    }

    #[test]
    fn three_position_switch_on_one_analog_channel() {
        for invert in [false, true] {
            let s = if invert { -1 } else { 1 };
            let mut l = Learner::new(vec![Target::SB]);
            let mut t = 0;
            l.update(t, &report(&[-s * 1024], &[]));
            assert_eq!(
                l.status().prompt,
                "SB: flip it all the way TOWARD you and hold"
            );
            hold(&mut l, &mut t, &report(&[s * 1024], &[]));
            assert_eq!(
                l.status().prompt,
                "SB: now all the way AWAY from you and hold"
            );
            hold(&mut l, &mut t, &report(&[-s * 1024], &[]));
            assert_eq!(l.status().prompt, "SB: now to the MIDDLE and hold");
            hold(&mut l, &mut t, &report(&[0], &[]));
            assert_eq!(l.found()[0].source, one(1, invert));
        }
    }

    #[test]
    fn analog_channel_wins_over_a_copy_on_a_button() {
        // SB mixed to CH1 and to a button: CH1 has every position
        let mut l = Learner::new(vec![Target::SB]);
        let mut t = 0;
        l.update(t, &report(&[-1024], &[false]));
        hold(&mut l, &mut t, &report(&[1024], &[true]));
        hold(&mut l, &mut t, &report(&[-1024], &[false]));
        hold(&mut l, &mut t, &report(&[0], &[false]));
        assert_eq!(l.found()[0].source, one(1, false));
    }

    #[test]
    fn a_channel_per_position() {
        let p = |up, mid, down| Some(Mapping::Positions(Positions { up, mid, down }));
        // (buttons at rest, then toward / away / middle, what's found)
        let cases: [(&[bool], [&[bool]; 3], _); 4] = [
            // CH2 up, CH3 middle, CH4 down
            (
                &[true, false, false],
                [
                    &[false, false, true],
                    &[true, false, false],
                    &[false, true, false],
                ],
                p(Some(2), Some(3), Some(4)),
            ),
            // CH2 up, CH3 down, the middle has none
            (
                &[true, false],
                [&[false, true], &[true, false], &[false, false]],
                p(Some(2), None, Some(3)),
            ),
            // CH2 middle, CH3 down, up has none
            (
                &[false, false],
                [&[false, true], &[false, false], &[true, false]],
                p(None, Some(2), Some(3)),
            ),
            // CH2 up, CH3 middle, down has none
            (
                &[true, false],
                [&[false, false], &[true, false], &[false, true]],
                p(Some(2), Some(3), None),
            ),
        ];
        for (rest, walked, want) in cases {
            let mut l = Learner::new(vec![Target::SC]);
            let mut t = 0;
            l.update(t, &report(&[0], rest));
            walk(&mut l, &mut t, &walked);
            assert_eq!(l.found()[0].source, want, "{walked:?}");
        }
    }

    #[test]
    fn three_position_switch_on_one_button_waits_out_the_middle() {
        // the middle looks like away (off) on the only channel it's on
        for (toward, away) in [(true, false), (false, true)] {
            let mut l = Learner::new(vec![Target::SB]);
            let mut t = 0;
            l.update(t, &report(&[0], &[away]));
            walk(&mut l, &mut t, &[&[toward], &[away]]);
            hold(&mut l, &mut t, &report(&[0], &[away]));
            assert!(
                !l.is_done(),
                "a normal hold isn't enough: they may not be there yet"
            );
            keep(&mut l, &mut t, &report(&[0], &[away]), MIDDLE_WAIT_MS);
            assert_eq!(l.found()[0].source, one(2, !toward));
        }
    }

    #[test]
    fn a_channel_per_end_never_waits_out_the_middle() {
        // CH2 up, CH3 down: both ends on, so the middle (neither) has to be seen
        let mut l = Learner::new(vec![Target::SB]);
        let mut t = 0;
        l.update(t, &report(&[0], &[true, false]));
        walk(&mut l, &mut t, &[&[false, true], &[true, false]]);
        keep(
            &mut l,
            &mut t,
            &report(&[0], &[true, false]),
            3 * MIDDLE_WAIT_MS,
        );
        assert!(!l.is_done(), "still waiting for the middle");
        hold(&mut l, &mut t, &report(&[0], &[false, false]));
        assert!(l.is_done());
    }

    #[test]
    fn two_position_switch_on_a_button_per_position() {
        // SA up on CH2, down on CH3: either tells it; the first one is used, reversed
        let mut l = Learner::new(vec![Target::SA]);
        let mut t = 0;
        l.update(t, &report(&[0], &[true, false]));
        assert_eq!(l.status().prompt, "SA: flip it TOWARD you and hold");
        walk(&mut l, &mut t, &[&[false, true], &[true, false]]);
        assert_eq!(l.found()[0].source, one(2, true));
    }

    #[test]
    fn a_switch_already_toward_you_has_to_move_first() {
        let mut l = Learner::new(vec![Target::SA]);
        let mut t = 0;
        l.update(t, &report(&[1024], &[]));
        keep(&mut l, &mut t, &report(&[1024], &[]), 2000);
        assert_eq!(l.status().prompt, "SA: flip it TOWARD you and hold");
        // away and back, as the hint says
        l.update(t + 50, &report(&[-1024], &[]));
        t += 50;
        walk_axis(&mut l, &mut t, &[1024, -1024]);
        assert_eq!(l.found()[0].source, one(1, false));
    }

    fn walk_axis(l: &mut Learner, t: &mut u64, values: &[i16]) {
        for &v in values {
            hold(l, t, &report(&[v], &[]));
        }
    }
}
