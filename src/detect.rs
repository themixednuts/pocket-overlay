//! Passive channel classification: watches values over time and guesses what kind of
//! control drives each channel, without the user doing anything special.

use serde::Serialize;

use crate::hid::Report;

#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ChannelKind {
    /// Hasn't moved (yet).
    Idle,
    /// Seen at many in-between values: a stick or pot.
    Analog,
    /// Only ever at the two ends.
    Switch2,
    /// Only ever at the two ends and the middle.
    Switch3,
}

/// Values within this distance of -1024 / 0 / +1024 count as a switch position.
const SNAP: i16 = 24;
/// Distinct in-between values needed before we call a channel analog.
const ANALOG_MIN_DISTINCT: usize = 4;

#[derive(Debug, Clone, Default)]
struct Stats {
    /// (min, max) seen so far.
    range: Option<(i16, i16)>,
    low: bool,
    mid: bool,
    high: bool,
    /// Distinct in-between values, capped.
    between: Vec<i16>,
}

#[derive(Debug, Clone, Default)]
pub struct Detector {
    channels: Vec<Stats>,
}

impl Detector {
    pub fn observe(&mut self, report: &Report) {
        let n = report.channel_count();
        if self.channels.len() != n {
            self.channels = vec![Stats::default(); n];
        }
        for (i, s) in self.channels.iter_mut().enumerate() {
            let v = report.channel(i + 1).unwrap_or(0);
            let (lo, hi) = s.range.unwrap_or((v, v));
            s.range = Some((lo.min(v), hi.max(v)));
            if v <= -1024 + SNAP {
                s.low = true;
            } else if v >= 1024 - SNAP {
                s.high = true;
            } else if v.abs() <= SNAP {
                s.mid = true;
            } else if s.between.len() < ANALOG_MIN_DISTINCT && !s.between.contains(&v) {
                s.between.push(v);
            }
        }
    }

    pub fn kinds(&self) -> Vec<ChannelKind> {
        self.channels.iter().map(Stats::kind).collect()
    }

    pub fn reset(&mut self) {
        self.channels.clear();
    }
}

impl Stats {
    fn kind(&self) -> ChannelKind {
        let (lo, hi) = self.range.unwrap_or_default();
        if i32::from(hi) - i32::from(lo) < i32::from(SNAP) {
            ChannelKind::Idle
        } else if self.between.len() >= ANALOG_MIN_DISTINCT {
            ChannelKind::Analog
        } else if self.low && self.high && self.mid {
            ChannelKind::Switch3
        } else if self.low && self.high {
            ChannelKind::Switch2
        } else {
            // Moved, but not enough to tell: most likely a stick nudged a little.
            ChannelKind::Analog
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn report(values: &[i16]) -> Report {
        Report {
            axes: values.to_vec(),
            buttons: vec![],
        }
    }

    #[test]
    fn classifies_each_kind() {
        let mut d = Detector::default();
        // ch1 stick sweep, ch2 2-pos, ch3 3-pos, ch4 untouched, ch5 throttle parked low
        for step in 0..=20 {
            let t = step as i16;
            d.observe(&report(&[
                -1024 + t * 102,
                if t % 7 < 3 { -1024 } else { 1024 },
                [-1024, 0, 1024][(t % 3) as usize],
                0,
                -1024,
            ]));
        }
        use ChannelKind::*;
        assert_eq!(d.kinds(), [Analog, Switch2, Switch3, Idle, Idle]);
    }

    #[test]
    fn slightly_noisy_switch_is_still_a_switch() {
        let mut d = Detector::default();
        for v in [-1024, -1020, 1024, 1019, 3, -2, -1024] {
            d.observe(&report(&[v]));
        }
        assert_eq!(d.kinds(), [ChannelKind::Switch3]);
    }
}
