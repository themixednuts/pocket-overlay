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

const fn ch(ch: usize) -> Option<Source> {
    Some(Source { ch, invert: false })
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
    pub sb: Option<Source>,
    #[serde(rename = "SC", default, skip_serializing_if = "Option::is_none")]
    pub sc: Option<Source>,
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
    pub sticks: Sticks,
    pub controls: Controls,
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
                sb: ch(6),
                sc: ch(7),
                s1: ch(8),
                sd: ch(9),
                se: ch(10),
            },
        }
    }
}

impl Config {
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
        let text = format!("{HEADER}\n{}", toml::to_string_pretty(self)?);
        std::fs::write(path, text).with_context(|| format!("writing {}", path.display()))
    }

    /// Applies what the channel-detection wizard found. Skipped sticks keep their old
    /// channel; skipped switches and the pot become unmapped.
    pub fn apply(&mut self, found: &[Found]) {
        for f in found {
            let (s, c) = (&mut self.sticks, &mut self.controls);
            match (f.target, f.source) {
                (Target::LeftX, Some(src)) => s.left_x = src,
                (Target::LeftY, Some(src)) => s.left_y = src,
                (Target::RightX, Some(src)) => s.right_x = src,
                (Target::RightY, Some(src)) => s.right_y = src,
                (Target::LeftX | Target::LeftY | Target::RightX | Target::RightY, None) => {}
                (Target::SA, src) => c.sa = src,
                (Target::SB, src) => c.sb = src,
                (Target::SC, src) => c.sc = src,
                (Target::SD, src) => c.sd = src,
                (Target::SE, src) => c.se = src,
                (Target::S1, src) => c.s1 = src,
            }
        }
    }

    fn validate(&self) -> Result<()> {
        if !(1..=4).contains(&self.mode) {
            bail!("mode must be 1, 2, 3 or 4");
        }
        let max = edgetx::AXES + edgetx::BUTTONS;
        let s = &self.sticks;
        for (name, src) in [
            ("sticks.left_x", s.left_x),
            ("sticks.left_y", s.left_y),
            ("sticks.right_x", s.right_x),
            ("sticks.right_y", s.right_y),
        ] {
            if !(1..=edgetx::AXES).contains(&src.ch) {
                bail!("{name}: ch must be 1..=8 (only CH1-8 are analog over USB)");
            }
        }
        let c = &self.controls;
        // (name, source, needs an analog channel)
        let controls = [
            ("SA", c.sa, false),
            ("SB", c.sb, true),
            ("SC", c.sc, true),
            ("SD", c.sd, false),
            ("SE", c.se, false),
            ("S1", c.s1, true),
        ];
        for (name, src, analog) in controls {
            let Some(src) = src else { continue };
            if analog && !(1..=edgetx::AXES).contains(&src.ch) {
                bail!("controls.{name} needs an analog channel: ch must be 1..=8");
            }
            if !(1..=max).contains(&src.ch) {
                bail!("controls.{name}: ch must be 1..={max}");
            }
        }
        Ok(())
    }
}

const HEADER: &str = "\
# pocket-overlay settings. You don't need to edit this: open
# http://127.0.0.1:7878/?setup=1 and use Detect channels, which rewrites it.
#
# `ch` values are the channel numbers on your model's MIXES page. Over USB, CH1-8 are
# analog and CH9-32 are on/off, so sticks, SB, SC and S1 need a channel in 1-8.
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
        let mut cfg = Config::default();
        cfg.controls.sb = Some(Source {
            ch: 12,
            invert: false,
        });
        cfg.save(&path).unwrap();
        let err = format!("{:#}", Config::load_or_create(&path).unwrap_err());
        assert!(err.contains("SB needs an analog channel"), "{err}");
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
