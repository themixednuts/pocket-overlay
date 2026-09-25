//! Turns a decoded report into the state the overlay page draws.

use std::collections::BTreeMap;

use serde::Serialize;

use crate::config::{Config, Controls, LIGHTS, Mapping, Positions, Shadow, Source, Sticks};
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
    /// Default skin, and a counter bumped whenever skin files change (so pages reload it).
    pub skin: Option<String>,
    pub skin_rev: u32,
    /// Highlight colour (`#rrggbb`), `None` for the default.
    pub accent: Option<String>,
    /// Which parts show under the radio.
    pub show_readout: bool,
    pub show_channels: bool,
    /// And the switch labels around it, its antenna and its shadow.
    pub show_labels: bool,
    pub show_antenna: bool,
    pub show_shadow: bool,
    pub shadow: Shadow,
    /// Other PCs in the home network may show the overlay, and why that failed if it did.
    pub lan: bool,
    pub lan_error: Option<String>,
    /// Running `--demo`: there's no radio, so no channels to detect.
    pub demo: bool,
    /// Current channel assignments, for the setup view.
    pub sticks: Sticks,
    pub controls: Controls,
    /// Where each switch (and S1) lights up: `"SB": [true, false, true]`, one flag per
    /// position (up, mid, down; SE released, pressed; S1 below, at, above its middle).
    pub lit: BTreeMap<&'static str, Vec<bool>>,
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
    skin_rev: u32,
) -> OverlayState {
    let r = &raw.report;
    let s = &cfg.sticks;
    let c = &cfg.controls;
    let stick = |x, y| Stick {
        x: value(r, x),
        y: value(r, y),
    };
    let sw = |src: Option<Source>, positions| src.map(|src| switch_pos(value(r, src), positions));
    let sw3 = |m: Option<Mapping>| {
        m.map(|m| match m {
            Mapping::Channel(src) => switch_pos(value(r, src), 3),
            Mapping::Positions(p) => position(p, r),
        })
    };
    let count = r.channel_count().max(edgetx::CHANNELS);

    OverlayState {
        connected: raw.connected,
        name: raw.name.clone(),
        layout: raw.layout.clone(),
        left: stick(s.left_x, s.left_y),
        right: stick(s.right_x, s.right_y),
        sa: sw(c.sa, 2),
        sb: sw3(c.sb),
        sc: sw3(c.sc),
        sd: sw(c.sd, 2),
        se: sw(c.se, 2),
        s1: c.s1.map(|src| value(r, src)),
        channels: (1..=count).map(|ch| r.channel(ch).unwrap_or(0)).collect(),
        kinds,
        mode: cfg.mode,
        skin: cfg.skin.clone(),
        skin_rev,
        accent: cfg.accent.clone(),
        show_readout: cfg.show_readout,
        show_channels: cfg.show_channels,
        show_labels: cfg.show_labels,
        show_antenna: cfg.show_antenna,
        show_shadow: cfg.show_shadow,
        shadow: cfg.shadow,
        lan: cfg.lan,
        lan_error: None,
        demo: raw.demo,
        sticks: cfg.sticks.clone(),
        controls: cfg.controls.clone(),
        lit: LIGHTS
            .iter()
            .filter_map(|&(control, ..)| Some((control, cfg.lit_at(control)?)))
            .collect(),
        learn,
    }
}

/// Channel value scaled to -1..=1 (0 if the channel doesn't exist).
fn value(r: &Report, src: Source) -> f32 {
    let v = f32::from(r.channel(src.ch).unwrap_or(0)) / 1024.0;
    if src.invert { -v } else { v }
}

/// EdgeTX mixes a switch as -weight in the up (away from pilot) position, exactly 0 in the
/// middle and +weight down. Reading "clearly not zero" instead of a fraction of full scale
/// keeps switches right even when the model's mix weight is small (down to ~10%).
pub(crate) const SWITCH_MIDDLE: f32 = 0.1;

pub(crate) fn switch_pos(v: f32, positions: u8) -> u8 {
    match positions {
        3 if v < -SWITCH_MIDDLE => 0,
        3 if v > SWITCH_MIDDLE => 2,
        3 => 1,
        _ if v > 0.0 => 1,
        _ => 0,
    }
}

/// A channel per position: "on" the same way a switch reads "clearly not zero".
pub(crate) fn is_on(v: i16) -> bool {
    f32::from(v) / 1024.0 > SWITCH_MIDDLE
}

/// Where a switch with a channel per position is: the position whose channel is on, or,
/// with none on, the one without a channel (the middle when only the ends have one).
fn position(p: Positions, r: &Report) -> u8 {
    let chans = p.channels();
    let on = chans
        .iter()
        .position(|ch| ch.is_some_and(|ch| is_on(r.channel(ch).unwrap_or(0))));
    let pos = on.or_else(|| chans.iter().position(Option::is_none));
    pos.unwrap_or(1) as u8
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
    fn low_mix_weights_still_read_right() {
        // SB mixed at 15% and 50%: EdgeTX sends -w, 0, +w
        for w in [0.15, 0.5] {
            assert_eq!(
                [-w, 0.0, w].map(|v| switch_pos(v, 3)),
                [0, 1, 2],
                "weight {w}"
            );
            assert_eq!([-w, w].map(|v| switch_pos(v, 2)), [0, 1], "weight {w}");
        }
    }

    #[test]
    fn channel_per_position() {
        let report = |on: &[usize]| Report {
            axes: vec![0; 8],
            buttons: (9..=32).map(|ch| on.contains(&ch)).collect(),
        };
        let p = |up, mid, down| Positions { up, mid, down };
        let read = |ps: Positions, on: &[usize]| position(ps, &report(on));
        // up and down only: neither on is the middle
        let ends = p(Some(9), None, Some(11));
        assert_eq!(
            [read(ends, &[9]), read(ends, &[]), read(ends, &[11])],
            [0, 1, 2]
        );
        // all three
        let all = p(Some(9), Some(10), Some(11));
        assert_eq!(
            [read(all, &[9]), read(all, &[10]), read(all, &[11])],
            [0, 1, 2]
        );
        // any two: the one left out is where it is with neither on
        let low = p(None, Some(10), Some(11));
        assert_eq!(
            [read(low, &[]), read(low, &[10]), read(low, &[11])],
            [0, 1, 2]
        );
        let high = p(Some(9), Some(10), None);
        assert_eq!(
            [read(high, &[9]), read(high, &[10]), read(high, &[])],
            [0, 1, 2]
        );
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
            demo: false,
        };
        let s = map(&Config::default(), &raw, vec![], None, 0);
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
        let s = map(&cfg, &raw, vec![], None, 0);
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
            0,
        );
        assert_eq!(s.left.y, -1.0);
    }
}
