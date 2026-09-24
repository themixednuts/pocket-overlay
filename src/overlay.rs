//! Turns a decoded report into the state the overlay page draws.

use serde::Serialize;

use crate::config::{Config, Controls, Source, Sticks};
use crate::detect::ChannelKind;
use crate::edgetx;
use crate::hid::Report;
use crate::input::RawState;
use crate::learn::LearnStatus;

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct OverlayState {
    pub connected: bool,
    pub name: String,
    pub layout: String,
    pub left: Stick,
    pub right: Stick,
    pub sa: Option<u8>,
    pub sb: Option<u8>,
    pub sc: Option<u8>,
    pub sd: Option<u8>,
    pub se: Option<u8>,
    /// -1..=1, `None` when S1 isn't mapped.
    pub s1: Option<f32>,
    /// Every channel in EdgeTX units (-1024..=1024), at least 32 of them.
    pub channels: Vec<i16>,
    /// What each channel looks like it's driven by, from watching it.
    pub kinds: Vec<ChannelKind>,
    /// Transmitter mode, for labelling the sticks.
    pub mode: u8,
    /// Current channel assignments, for the setup view.
    pub sticks: Sticks,
    pub controls: Controls,
    /// Present while the channel-detection wizard runs (and after, until dismissed).
    pub learn: Option<LearnStatus>,
}

#[derive(Debug, Clone, Copy, Serialize, PartialEq)]
pub struct Stick {
    pub x: f32,
    pub y: f32,
}

pub fn map(
    cfg: &Config,
    raw: &RawState,
    kinds: Vec<ChannelKind>,
    learn: Option<LearnStatus>,
) -> OverlayState {
    let r = &raw.report;
    let s = &cfg.sticks;
    let c = &cfg.controls;
    let stick = |x, y| Stick {
        x: value(r, x),
        y: value(r, y),
    };
    let sw = |src: Option<Source>, positions| src.map(|src| switch_pos(value(r, src), positions));
    let count = r.channel_count().max(edgetx::CHANNELS);

    OverlayState {
        connected: raw.connected,
        name: raw.name.clone(),
        layout: raw.layout.clone(),
        left: stick(s.left_x, s.left_y),
        right: stick(s.right_x, s.right_y),
        sa: sw(c.sa, 2),
        sb: sw(c.sb, 3),
        sc: sw(c.sc, 3),
        sd: sw(c.sd, 2),
        se: sw(c.se, 2),
        s1: c.s1.map(|src| value(r, src)),
        channels: (1..=count).map(|ch| r.channel(ch).unwrap_or(0)).collect(),
        kinds,
        mode: cfg.mode,
        sticks: cfg.sticks.clone(),
        controls: cfg.controls.clone(),
        learn,
    }
}

/// Channel value scaled to -1..=1 (0 if the channel doesn't exist).
fn value(r: &Report, src: Source) -> f32 {
    let v = f32::from(r.channel(src.ch).unwrap_or(0)) / 1024.0;
    if src.invert { -v } else { v }
}

/// EdgeTX mixes a switch as -100 in the up (away from pilot) position, 0 middle, +100 down.
fn switch_pos(v: f32, positions: u8) -> u8 {
    match positions {
        3 if v < -0.33 => 0,
        3 if v > 0.33 => 2,
        3 => 1,
        _ if v > 0.0 => 1,
        _ => 0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn switch_thresholds() {
        assert_eq!([-1.0, 0.0, 1.0].map(|v| switch_pos(v, 3)), [0, 1, 2]);
        assert_eq!([-1.0, 1.0].map(|v| switch_pos(v, 2)), [0, 1]);
    }

    #[test]
    fn default_mapping() {
        let mut buttons = vec![false; 24];
        buttons[1] = true; // CH10 (SE) on, CH9 (SD) off
        let report = Report {
            axes: vec![512, -512, 1024, -1024, 1024, 0, -1024, 256],
            buttons,
        };
        let raw = RawState {
            connected: true,
            name: "r".into(),
            layout: String::new(),
            report,
        };
        let s = map(&Config::default(), &raw, vec![], None);
        assert_eq!(s.left, Stick { x: -1.0, y: 1.0 });
        assert_eq!(s.right, Stick { x: 0.5, y: -0.5 });
        assert_eq!(
            (s.sa, s.sb, s.sc, s.sd, s.se, s.s1),
            (Some(1), Some(1), Some(0), Some(0), Some(1), Some(0.25))
        );
        assert_eq!(s.channels.len(), 32);
        assert_eq!(s.channels[9], 1024);
    }

    #[test]
    fn inverted_and_missing_channels() {
        let mut cfg = Config::default();
        cfg.sticks.left_y.invert = true;
        let raw = RawState::default(); // disconnected: no channels at all
        let s = map(&cfg, &raw, vec![], None);
        assert_eq!(s.left.y, 0.0);
        assert_eq!(s.channels, vec![0; 32]);

        let report = Report {
            axes: vec![0, 0, 1024],
            buttons: vec![],
        };
        let s = map(
            &cfg,
            &RawState {
                report,
                ..RawState::default()
            },
            vec![],
            None,
        );
        assert_eq!(s.left.y, -1.0);
    }
}
