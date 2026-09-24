//! Guided channel detection: prompts for one control at a time, watches which channel
//! moves, and records its channel number and direction.
//!
//! Every prompt ends with the control held at its "positive" end (stick up/right, switch
//! toward you, SE pressed, S1 at the end that should read +100%), so the sign of the held
//! value tells us whether the channel is inverted. Time is passed in by the caller, which
//! keeps this deterministic to test.

use serde::Serialize;

use crate::config::Source;
use crate::hid::Report;

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

    pub fn prompt(self) -> &'static str {
        match self {
            Target::LeftY => "LEFT stick: push fully UP and hold",
            Target::LeftX => "LEFT stick: push fully RIGHT and hold",
            Target::RightY => "RIGHT stick: push fully UP and hold",
            Target::RightX => "RIGHT stick: push fully RIGHT and hold",
            Target::SA => "SA: flip it away, then TOWARD you",
            Target::SB => "SB: flip it away, then all the way TOWARD you",
            Target::SC => "SC: flip it away, then all the way TOWARD you",
            Target::SD => "SD: flip it away, then TOWARD you",
            Target::SE => "SE: press and hold",
            Target::S1 => "S1: roll it to one end, then to the other and hold",
        }
    }

    pub fn hint(self) -> &'static str {
        match self {
            Target::LeftY | Target::LeftX | Target::RightY | Target::RightX => {
                "If it's already there, move it the other way first."
            }
            Target::S1 => "The end you finish on reads +100%.",
            _ => "Skip it if it isn't mixed to a channel.",
        }
    }

    /// Sticks, 3-position switches and the pot need a full-resolution (analog) channel.
    pub fn needs_analog(self) -> bool {
        !matches!(self, Target::SA | Target::SD | Target::SE)
    }
}

/// A channel counts as "moved" once its range this step reaches this...
const MIN_RANGE: i32 = 900;
/// ...and it's being held at least this far from center...
const MIN_HELD: i16 = 800;
/// ...within this much jitter...
const HOLD_JITTER: i16 = 60;
/// ...for this long.
pub const HOLD_MS: u64 = 400;
/// The runner-up must have moved less than this fraction of the winner, or we wait.
const AMBIGUITY: f32 = 0.6;

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct Found {
    pub target: Target,
    /// `None` when the step was skipped.
    pub source: Option<Source>,
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

#[derive(Debug, Clone)]
pub struct Learner {
    steps: Vec<Target>,
    found: Vec<Found>,
    /// Per-channel (min, max) since the current step started.
    ranges: Vec<(i16, i16)>,
    /// Latest value of every channel; the next step's ranges start from here.
    last: Vec<i16>,
    /// (channel, value when the hold started, time it started)
    hold: Option<(usize, i16, u64)>,
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
        self.ranges = self.last.iter().map(|&v| (v, v)).collect();
        self.hold = None;
        self.ambiguous = false;
    }

    fn taken(&self, ch: usize) -> bool {
        self.found
            .iter()
            .any(|f| f.source.is_some_and(|s| s.ch == ch))
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
            self.ranges = self.last.iter().map(|&v| (v, v)).collect();
        }
        for (r, &v) in self.ranges.iter_mut().zip(&self.last) {
            *r = (r.0.min(v), r.1.max(v));
        }

        let axes = report.axes.len();
        let mut best: Option<(usize, i32)> = None;
        let mut second = 0;
        for (i, (lo, hi)) in self.ranges.iter().enumerate() {
            let ch = i + 1;
            if self.taken(ch) || (target.needs_analog() && ch > axes) {
                continue;
            }
            let range = i32::from(*hi) - i32::from(*lo);
            match best {
                Some((_, b)) if range <= b => second = second.max(range),
                _ => {
                    second = second.max(best.map_or(0, |b| b.1));
                    best = Some((ch, range));
                }
            }
        }

        let Some((ch, range)) = best else {
            self.hold = None;
            return;
        };
        self.ambiguous = range >= MIN_RANGE && second as f32 >= range as f32 * AMBIGUITY;
        let v = report.channel(ch).unwrap_or(0);
        if range < MIN_RANGE || self.ambiguous || v.abs() < MIN_HELD {
            self.hold = None;
            return;
        }
        match self.hold {
            Some((hch, hv, since)) if hch == ch && (v - hv).abs() <= HOLD_JITTER => {
                if now_ms.saturating_sub(since) >= HOLD_MS {
                    let source = Source { ch, invert: v < 0 };
                    self.finish_step(Found {
                        target,
                        source: Some(source),
                    });
                }
            }
            _ => self.hold = Some((ch, v, now_ms)),
        }
    }

    pub fn status(&self) -> LearnStatus {
        let target = self.current();
        let hold = self.hold.map_or(0.0, |(_, _, since)| {
            (self.last_now.saturating_sub(since) as f32 / HOLD_MS as f32).min(1.0)
        });
        LearnStatus {
            step: self.found.len(),
            total: self.steps.len(),
            target,
            prompt: target.map_or("All done", Target::prompt),
            hint: target.map_or("", Target::hint),
            candidate: self.hold.map(|(ch, ..)| ch),
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

    /// Holds `r` long enough to complete a step.
    fn hold(l: &mut Learner, t: &mut u64, r: &Report) {
        for _ in 0..6 {
            *t += 100;
            l.update(*t, r);
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
                source: Some(Source {
                    ch: 2,
                    invert: true
                })
            }
        );

        hold(&mut l, &mut t, &report(&[0, -1024], &[true]));
        assert_eq!(
            l.found()[1].source,
            Some(Source {
                ch: 3,
                invert: false
            })
        );
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
    fn analog_targets_ignore_button_channels() {
        let mut l = Learner::new(vec![Target::SB]);
        let mut t = 0;
        l.update(t, &report(&[0], &[false]));
        hold(&mut l, &mut t, &report(&[0], &[true]));
        assert!(!l.is_done(), "a button can't be a 3-position switch");
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
}
