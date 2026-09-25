use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use crate::edgetx;
use crate::learn::{Found, Target};

/// An EdgeTX output channel (1-based, as shown on the radio's MIXES page).
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Source {
    pub ch: usize,
    #[serde(default, skip_serializing_if = "is_false")]
    pub invert: bool,
}

fn is_false(b: &bool) -> bool {
    !*b
}

fn is_true(b: &bool) -> bool {
    *b
}

fn yes() -> bool {
    true
}

const fn ch(ch: usize) -> Option<Source> {
    Some(Source { ch, invert: false })
}

/// Where SB or SC is read from: one channel, like every other control, or a channel per
/// position (some models give each position its own button, for games).
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(untagged)]
pub enum Mapping {
    Channel(Source),
    Positions(Positions),
}

impl Mapping {
    pub fn uses(&self, ch: usize) -> bool {
        match self {
            Mapping::Channel(src) => src.ch == ch,
            Mapping::Positions(p) => p.channels().contains(&Some(ch)),
        }
    }
}

impl From<Source> for Mapping {
    fn from(src: Source) -> Self {
        Mapping::Channel(src)
    }
}

/// A channel per switch position, on while the switch is there; at least two of them.
/// With none on, the switch is in the position that has no channel (the middle, when
/// only the ends have one).
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Positions {
    /// Away from you (EdgeTX's SB↑).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub up: Option<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mid: Option<usize>,
    /// Toward you (SB↓).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub down: Option<usize>,
}

impl Positions {
    /// Up, middle, down: the order positions are numbered in (0, 1, 2).
    pub fn channels(&self) -> [Option<usize>; 3] {
        [self.up, self.mid, self.down]
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Sticks {
    pub left_x: Source,
    pub left_y: Source,
    pub right_x: Source,
    pub right_y: Source,
}

/// The Pocket's switches and pot (from EdgeTX `boards/hw_defs/pocket.json`).
/// Leave one out to draw it as "not mapped".
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Controls {
    #[serde(rename = "SA", default, skip_serializing_if = "Option::is_none")]
    pub sa: Option<Source>,
    #[serde(rename = "SB", default, skip_serializing_if = "Option::is_none")]
    pub sb: Option<Mapping>,
    #[serde(rename = "SC", default, skip_serializing_if = "Option::is_none")]
    pub sc: Option<Mapping>,
    #[serde(rename = "SD", default, skip_serializing_if = "Option::is_none")]
    pub sd: Option<Source>,
    #[serde(rename = "SE", default, skip_serializing_if = "Option::is_none")]
    pub se: Option<Source>,
    #[serde(rename = "S1", default, skip_serializing_if = "Option::is_none")]
    pub s1: Option<Source>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    pub port: u16,
    pub usb_vid: u16,
    pub usb_pid: u16,
    /// Transmitter stick mode (1-4); only changes the THR/RUD/ELE/AIL labels.
    #[serde(default = "default_mode")]
    pub mode: u8,
    /// Skin shown by default (a PNG in the skins folder); none = the built-in drawing.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub skin: Option<String>,
    /// Highlight colour as `#rrggbb`; none = the default green.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub accent: Option<String>,
    /// Show the THR/RUD/ELE/AIL line under the radio.
    #[serde(default = "yes", skip_serializing_if = "is_true")]
    pub show_readout: bool,
    /// Show the channel bars under the radio.
    #[serde(default = "yes", skip_serializing_if = "is_true")]
    pub show_channels: bool,
    /// Show the switch labels (SA, SB... with their positions) around the radio.
    #[serde(default = "yes", skip_serializing_if = "is_true")]
    pub show_labels: bool,
    /// Show the antenna on top of the radio.
    #[serde(default = "yes", skip_serializing_if = "is_true")]
    pub show_antenna: bool,
    /// Draw a soft shadow under the radio, as if it floats over the scene.
    #[serde(default = "yes", skip_serializing_if = "is_true")]
    pub show_shadow: bool,
    /// Let OBS on other PCs in the home network show the overlay (they can only watch).
    #[serde(default, skip_serializing_if = "is_false")]
    pub lan: bool,
    pub sticks: Sticks,
    pub controls: Controls,
    /// How the shadow under the radio looks, if not the default.
    #[serde(default, skip_serializing_if = "Shadow::is_default")]
    pub shadow: Shadow,
    /// Where a switch (or S1) lights up on the overlay, if not where it does by default
    /// (see [`LIGHTS`]): the positions it counts as on, e.g. `SA = ["up"]`.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub lit: BTreeMap<String, Vec<String>>,
}

/// The shadow under the radio: which way it falls and how far, how soft and how dark.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct Shadow {
    /// Which way it falls, in degrees clockwise from straight up (180 = straight down).
    pub angle: u16,
    /// How far it falls, in the drawing's units (about a pixel in the 680-wide OBS source).
    pub distance: u8,
    /// How soft its edge is (the blur's spread, in the same units).
    pub blur: u8,
    /// How dark it is, in percent.
    pub strength: u8,
}

impl Default for Shadow {
    fn default() -> Self {
        Self {
            angle: 180,
            distance: 11,
            blur: 6,
            strength: 60,
        }
    }
}

impl Shadow {
    pub const MAX_DISTANCE: u8 = 30;
    pub const MAX_BLUR: u8 = 20;

    fn is_default(&self) -> bool {
        *self == Self::default()
    }

    pub fn valid(&self) -> bool {
        self.angle < 360
            && self.distance <= Self::MAX_DISTANCE
            && self.blur <= Self::MAX_BLUR
            && self.strength <= 100
    }
}

/// The controls that light up on the overlay: their positions in the order the page numbers
/// them, and where each lights up by default. SA and SD toward you, SB and SC at either end,
/// SE pressed, S1 either side of its middle (S1 is a pot, so "mid" is its dead centre, or
/// the middle of a 3-position switch if the radio is set up to make it one).
pub const LIGHTS: [(&str, &[&str], &[bool]); 6] = [
    ("SA", &["up", "down"], &[false, true]),
    ("SB", &["up", "mid", "down"], &[true, false, true]),
    ("SC", &["up", "mid", "down"], &[true, false, true]),
    ("SD", &["up", "down"], &[false, true]),
    ("SE", &["released", "pressed"], &[false, true]),
    ("S1", &["-", "mid", "+"], &[true, false, true]),
];

fn lights(control: &str) -> Option<(&'static [&'static str], &'static [bool])> {
    LIGHTS
        .iter()
        .find(|(c, ..)| *c == control)
        .map(|&(_, names, default)| (names, default))
}

/// `#rrggbb`, nothing else (the value ends up in the page's CSS).
pub fn valid_colour(s: &str) -> bool {
    s.len() == 7 && s.starts_with('#') && s[1..].bytes().all(|b| b.is_ascii_hexdigit())
}

fn default_mode() -> u8 {
    2
}

impl Default for Config {
    fn default() -> Self {
        Self {
            port: 7878,
            usb_vid: edgetx::USB_VID,
            usb_pid: edgetx::USB_PID,
            mode: default_mode(),
            skin: None,
            accent: None,
            show_readout: true,
            show_channels: true,
            show_labels: true,
            show_antenna: true,
            show_shadow: true,
            lan: false,
            shadow: Shadow::default(),
            lit: BTreeMap::new(),
            // Mode 2, AETR.
            sticks: Sticks {
                left_x: Source {
                    ch: 4,
                    invert: false,
                },
                left_y: Source {
                    ch: 3,
                    invert: false,
                },
                right_x: Source {
                    ch: 1,
                    invert: false,
                },
                right_y: Source {
                    ch: 2,
                    invert: false,
                },
            },
            controls: Controls {
                sa: ch(5),
                sb: ch(6).map(Mapping::from),
                sc: ch(7).map(Mapping::from),
                s1: ch(8),
                sd: ch(9),
                se: ch(10),
            },
        }
    }
}

impl Config {
    /// The OBS Browser Source size for these settings: 680 wide, and tall enough for what
    /// shows under the radio, rounded up to 10. The setup page works it out the same way
    /// (`obsHeight`): the drawing is 682 wide and 653 tall, plus 44 for the readout and 132
    /// for the channel bars.
    pub fn obs_source_size(&self) -> (u32, u32) {
        let tall = 653 + 44 * u32::from(self.show_readout) + 132 * u32::from(self.show_channels);
        (680, (tall * 680).div_ceil(682 * 10) * 10)
    }

    /// `%APPDATA%\pocket-overlay\overlay.toml` on Windows, `~/.config/pocket-overlay/overlay.toml`
    /// elsewhere, so the settings don't depend on which folder the app was started from.
    pub fn default_path() -> PathBuf {
        let base = std::env::var_os("APPDATA")
            .or_else(|| std::env::var_os("XDG_CONFIG_HOME"))
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")));
        match base {
            Some(dir) => dir.join("pocket-overlay").join("overlay.toml"),
            None => PathBuf::from("overlay.toml"),
        }
    }

    /// Loads the config, writing the defaults to `path` first if it doesn't exist.
    pub fn load_or_create(path: &Path) -> Result<Self> {
        if !path.exists() {
            Self::default().save(path)?;
        }
        let text =
            std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
        let cfg: Self =
            toml::from_str(&text).with_context(|| format!("parsing {}", path.display()))?;
        cfg.validate()?;
        Ok(cfg)
    }

    pub fn save(&self, path: &Path) -> Result<()> {
        if let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty()) {
            std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
        }
        // (not to_string_pretty: it spreads `SA = ["up", "down"]` over four lines)
        let text = format!("{HEADER}\n{}", toml::to_string(self)?);
        std::fs::write(path, text).with_context(|| format!("writing {}", path.display()))
    }

    /// Applies what the channel-detection wizard found. Skipped sticks keep their old
    /// channel; skipped switches and the pot become unmapped.
    pub fn apply(&mut self, found: &[Found]) {
        for f in found {
            let (s, c) = (&mut self.sticks, &mut self.controls);
            // only SB and SC have a channel per position
            let one = match f.source {
                Some(Mapping::Channel(src)) => Some(src),
                _ => None,
            };
            match (f.target, one) {
                (Target::SB, _) => c.sb = f.source,
                (Target::SC, _) => c.sc = f.source,
                (Target::LeftX, Some(src)) => s.left_x = src,
                (Target::LeftY, Some(src)) => s.left_y = src,
                (Target::RightX, Some(src)) => s.right_x = src,
                (Target::RightY, Some(src)) => s.right_y = src,
                (Target::LeftX | Target::LeftY | Target::RightX | Target::RightY, None) => {}
                (Target::SA, src) => c.sa = src,
                (Target::SD, src) => c.sd = src,
                (Target::SE, src) => c.se = src,
                (Target::S1, src) => c.s1 = src,
            }
        }
    }

    /// Where `control` lights up: one flag per position (see [`LIGHTS`]).
    pub fn lit_at(&self, control: &str) -> Option<Vec<bool>> {
        let (names, default) = lights(control)?;
        Some(match self.lit.get(control) {
            Some(on) => names.iter().map(|n| on.iter().any(|o| o == n)).collect(),
            None => default.to_vec(),
        })
    }

    /// Makes `control` light up where `at` says, one flag per position. Returns false (and
    /// changes nothing) for a control that doesn't light up or the wrong number of flags.
    pub fn set_lit(&mut self, control: &str, at: &[bool]) -> bool {
        let Some((names, default)) = lights(control).filter(|(n, _)| n.len() == at.len()) else {
            return false;
        };
        if at == default {
            self.lit.remove(control);
        } else {
            let on = names.iter().zip(at).filter(|&(_, &on)| on);
            let on = on.map(|(name, _)| name.to_string()).collect();
            self.lit.insert(control.to_string(), on);
        }
        true
    }

    fn validate(&self) -> Result<()> {
        if !self.shadow.valid() {
            bail!(
                "shadow: angle is 0-359, distance 0-{}, blur 0-{} and strength 0-100",
                Shadow::MAX_DISTANCE,
                Shadow::MAX_BLUR
            );
        }
        for (control, on) in &self.lit {
            let Some((names, _)) = lights(control) else {
                bail!("lit.{control}: only SA, SB, SC, SD, SE and S1 light up");
            };
            if let Some(bad) = on.iter().find(|n| !names.contains(&n.as_str())) {
                bail!("lit.{control}: {bad:?} isn't one of {}", names.join(", "));
            }
        }
        if !(1..=4).contains(&self.mode) {
            bail!("mode must be 1, 2, 3 or 4");
        }
        if let Some(skin) = &self.skin
            && !crate::skins::valid_name(skin)
        {
            bail!("skin {skin:?}: names are 1-40 letters, digits, - or _");
        }
        if let Some(accent) = &self.accent
            && !valid_colour(accent)
        {
            bail!("accent {accent:?}: use a colour like \"#39ff88\"");
        }
        // Any control can be on any channel: models get mixed every which way. On CH9-32
        // (on/off over USB) a stick, SB, SC or S1 just shows two positions.
        let max = edgetx::CHANNELS;
        let (s, c) = (&self.sticks, &self.controls);
        let one = [
            ("sticks.left_x", Some(s.left_x)),
            ("sticks.left_y", Some(s.left_y)),
            ("sticks.right_x", Some(s.right_x)),
            ("sticks.right_y", Some(s.right_y)),
            ("controls.SA", c.sa),
            ("controls.SD", c.sd),
            ("controls.SE", c.se),
            ("controls.S1", c.s1),
        ];
        // (setting, key, channel)
        let mut channels: Vec<(&str, &str, usize)> = one
            .into_iter()
            .filter_map(|(name, src)| Some((name, "ch", src?.ch)))
            .collect();
        for (name, m) in [("controls.SB", c.sb), ("controls.SC", c.sc)] {
            match m {
                Some(Mapping::Channel(src)) => channels.push((name, "ch", src.ch)),
                Some(Mapping::Positions(p)) => {
                    let given = ["up", "mid", "down"].into_iter().zip(p.channels());
                    let given: Vec<_> = given
                        .filter_map(|(key, ch)| Some((name, key, ch?)))
                        .collect();
                    if given.len() < 2 {
                        bail!("{name}: give two or three of up, mid and down, or one ch");
                    }
                    channels.extend(given);
                }
                None => {}
            }
        }
        for (name, key, ch) in channels {
            if !(1..=max).contains(&ch) {
                bail!("{name}: {key} must be 1..={max}");
            }
        }
        Ok(())
    }
}

const HEADER: &str = "\
# pocket-overlay settings. You don't need to edit this: open
# http://127.0.0.1:7878/?setup=1 and use Detect channels, which rewrites it.
#
# `ch` values are the channel numbers on your model's MIXES page (1-32). Over USB, CH1-8
# carry every position and CH9-32 are on/off, so a stick, SB, SC or S1 on CH9-32 shows
# only two positions.
#
# SB and SC can instead have a channel per position, on while the switch is there:
# `up`, `mid` and `down` (two or three of them; with none on, it's the one left out).
#
# [lit] lists where a switch lights up, if you picked that under \"Lights up\" on the
# settings page: `SB = [\"up\", \"down\"]` (SE: released, pressed; S1: \"-\", mid, \"+\").
";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn saves_into_a_new_folder_and_loads_back() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("new").join("folder").join("overlay.toml");
        let created = Config::load_or_create(&path).unwrap();
        assert_eq!(created, Config::default());
        let mut changed = created;
        changed.mode = 1;
        changed.controls.sb = None;
        changed.save(&path).unwrap();
        assert_eq!(Config::load_or_create(&path).unwrap(), changed);
    }

    #[test]
    fn where_switches_light_up_is_saved_by_name_and_only_when_changed() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("overlay.toml");
        let mut cfg = Config::default();
        assert_eq!(cfg.lit_at("SB"), Some(vec![true, false, true]));
        assert!(cfg.set_lit("SB", &[false, true, false]));
        assert!(cfg.set_lit("S1", &[false, false, true]));
        // not a control that lights up, or the wrong number of positions
        assert!(!cfg.set_lit("left_x", &[true, false]));
        assert!(!cfg.set_lit("SA", &[true, true, true]));
        cfg.save(&path).unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(
            text.contains(
                "[lit]
S1 = [\"+\"]
SB = [\"mid\"]
"
            ),
            "{text}"
        );
        let loaded = Config::load_or_create(&path).unwrap();
        assert_eq!(loaded.lit_at("SB"), Some(vec![false, true, false]));
        assert_eq!(loaded.lit_at("S1"), Some(vec![false, false, true]));
        // back to where it lights up anyway: out of the file again
        cfg.set_lit("SB", &[true, false, true]);
        cfg.set_lit("S1", &[true, false, true]);
        assert!(cfg.lit.is_empty());
    }

    #[test]
    fn settings_from_before_mode_existed_still_load() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("overlay.toml");
        let text = toml::to_string(&Config::default())
            .unwrap()
            .replace("mode = 2\n", "");
        assert!(!text.contains("mode"));
        std::fs::write(&path, text).unwrap();
        assert_eq!(Config::load_or_create(&path).unwrap().mode, 2);
    }

    #[test]
    fn bad_settings_are_rejected_with_the_reason() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("overlay.toml");
        let mut no_ch = Config::default();
        no_ch.controls.sb = ch(0).map(Mapping::from);
        let mut past_32 = Config::default();
        past_32.sticks.left_x.ch = 33;
        let positions = |p| {
            let mut cfg = Config::default();
            cfg.controls.sc = Some(Mapping::Positions(p));
            cfg
        };
        let only_up = positions(Positions {
            up: Some(9),
            ..Positions::default()
        });
        let down_past_32 = positions(Positions {
            up: Some(9),
            down: Some(40),
            ..Positions::default()
        });
        let lit = |control: &str, at: &str| {
            let mut cfg = Config::default();
            cfg.lit.insert(control.into(), vec![at.into()]);
            cfg
        };
        for (cfg, reason) in [
            (lit("SA", "mid"), r#"lit.SA: "mid" isn't one of up, down"#),
            (
                lit("left_x", "up"),
                "lit.left_x: only SA, SB, SC, SD, SE and S1 light up",
            ),
            (no_ch, "controls.SB: ch must be 1..=32"),
            (past_32, "sticks.left_x: ch must be 1..=32"),
            (
                only_up,
                "controls.SC: give two or three of up, mid and down",
            ),
            (down_past_32, "controls.SC: down must be 1..=32"),
        ] {
            cfg.save(&path).unwrap();
            let err = format!("{:#}", Config::load_or_create(&path).unwrap_err());
            assert!(err.contains(reason), "{err}");
        }
    }

    #[test]
    fn any_control_can_be_on_an_on_off_channel() {
        // sticks on CH1-4, every switch and the pot on CH9-32, as some models are mixed
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("overlay.toml");
        let mut cfg = Config::default();
        let c = &mut cfg.controls;
        (c.sa, c.sd, c.se, c.s1) = (ch(9), ch(10), ch(11), ch(12));
        (c.sb, c.sc) = (ch(13).map(Mapping::from), ch(14).map(Mapping::from));
        cfg.sticks.right_y.ch = 32;
        cfg.save(&path).unwrap();
        assert_eq!(Config::load_or_create(&path).unwrap(), cfg);
    }

    #[test]
    fn switch_with_a_channel_per_position() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("overlay.toml");
        let mut cfg = Config::default();
        cfg.controls.sb = Some(Mapping::Positions(Positions {
            up: Some(9),
            mid: Some(10),
            down: Some(11),
        }));
        cfg.controls.sc = Some(Mapping::Positions(Positions {
            up: Some(12),
            mid: None,
            down: Some(13),
        }));
        cfg.save(&path).unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(
            text.contains("[controls.SC]\nup = 12\ndown = 13\n"),
            "{text}"
        );
        assert_eq!(Config::load_or_create(&path).unwrap(), cfg);
    }

    #[test]
    fn colours() {
        assert!(valid_colour("#39ff88") && valid_colour("#ABCDEF"));
        for bad in [
            "39ff88", "#39ff8", "#39ff888", "#39fg88", "red", "#fff", "#12345;}", "",
        ] {
            assert!(!valid_colour(bad), "{bad:?}");
        }
    }

    #[test]
    fn obs_sizes_match_the_setup_page() {
        let size = |readout, channels| {
            Config {
                show_readout: readout,
                show_channels: channels,
                ..Config::default()
            }
            .obs_source_size()
        };
        // what the setup page shows (checked in blackbox_render)
        assert_eq!(size(true, true), (680, 830));
        assert_eq!(size(false, true), (680, 790));
        assert_eq!(size(true, false), (680, 700));
        assert_eq!(size(false, false), (680, 660));
    }

    #[test]
    fn default_location_is_a_per_user_folder() {
        let path = Config::default_path();
        if std::env::var_os("APPDATA").is_some() || std::env::var_os("HOME").is_some() {
            assert!(
                path.ends_with(Path::new("pocket-overlay").join("overlay.toml")),
                "{path:?}"
            );
            assert!(path.is_absolute(), "{path:?}");
        }
    }
}
