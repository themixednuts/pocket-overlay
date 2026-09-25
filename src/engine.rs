//! Ties input, config, channel detection and the wizard together and publishes the
//! overlay state. All mutable app state lives in this one task.

use std::path::PathBuf;
use std::time::{Duration, Instant};

use serde::Deserialize;
use tokio::sync::{mpsc, watch};

use crate::config::{Config, MAX_S1_DEADZONE, Shadow};
use crate::detect::Detector;
use crate::input::RawState;
use crate::learn::{LearnStatus, Learner};
use crate::overlay::{self, OverlayState};

/// Commands the page can send over the WebSocket, e.g. `{"cmd":"learn_start"}`.
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
#[serde(tag = "cmd", rename_all = "snake_case")]
pub enum Command {
    LearnStart,
    LearnSkip,
    LearnCancel,
    /// Dismiss the summary shown after the wizard finishes.
    LearnClose,
    /// `{"cmd":"set_mode","mode":1}`: transmitter stick mode, saved to the config.
    SetMode {
        mode: u8,
    },
    /// `{"cmd":"set_skin","skin":"name"}` (or `null` for the built-in drawing), saved.
    SetSkin {
        skin: Option<String>,
    },
    /// Sent by the server after a skin file was added, replaced or removed.
    SkinsChanged,
    /// `{"cmd":"set_accent","accent":"#ff8800"}` (or `null` for the default), saved.
    SetAccent {
        accent: Option<String>,
    },
    /// `{"cmd":"set_show","readout":true,"channels":false,"labels":true,"antenna":true,
    /// "shadow":true}`: what shows under and around the radio (the switch labels, antenna and
    /// shadow, if left out).
    SetShow {
        readout: bool,
        channels: bool,
        #[serde(default = "yes")]
        labels: bool,
        #[serde(default = "yes")]
        antenna: bool,
        #[serde(default = "yes")]
        shadow: bool,
    },
    /// `{"cmd":"set_shadow","angle":180,"distance":11,"blur":6,"strength":60}`: how the
    /// shadow under the radio looks (see `config::Shadow`), saved.
    SetShadow {
        angle: u16,
        distance: u8,
        blur: u8,
        strength: u8,
    },
    /// `{"cmd":"set_lit","control":"SB","at":[true,false,true]}`: where a switch lights up,
    /// one flag per position (see `config::LIGHTS`).
    SetLit {
        control: String,
        at: Vec<bool>,
    },
    /// `{"cmd":"set_s1_deadzone","percent":10}`: how far either side of its middle S1
    /// counts as the middle, saved.
    SetS1Deadzone {
        percent: u8,
    },
    /// `{"cmd":"set_lan","on":true}`: let OBS on other PCs in the network show the overlay.
    SetLan {
        on: bool,
    },
    /// From the server: the port couldn't be opened to other PCs, with the reason.
    #[serde(skip)]
    LanFailed(String),
}

fn yes() -> bool {
    true
}

const TICK: Duration = Duration::from_millis(50);

pub struct Engine {
    config: Config,
    config_path: PathBuf,
    detector: Detector,
    learner: Option<Learner>,
    /// Summary of the last finished wizard run, shown until dismissed.
    finished: Option<LearnStatus>,
    raw: RawState,
    start: Instant,
    skin_rev: u32,
    lan_error: Option<String>,
}

impl Engine {
    pub fn new(config: Config, config_path: PathBuf) -> Self {
        Self {
            config,
            config_path,
            detector: Detector::default(),
            learner: None,
            finished: None,
            raw: RawState::default(),
            start: Instant::now(),
            skin_rev: 0,
            lan_error: None,
        }
    }

    pub fn state(&self) -> OverlayState {
        let learn = self
            .learner
            .as_ref()
            .map(Learner::status)
            .or_else(|| self.finished.clone());
        let mut state = overlay::map(
            &self.config,
            &self.raw,
            self.detector.kinds(),
            learn,
            self.skin_rev,
        );
        state.lan_error = self.lan_error.clone();
        state
    }

    fn now_ms(&self) -> u64 {
        self.start.elapsed().as_millis() as u64
    }

    fn on_raw(&mut self, raw: RawState) {
        if raw.connected {
            self.detector.observe(&raw.report);
        }
        self.raw = raw;
        self.step_learner();
    }

    fn step_learner(&mut self) {
        let now = self.now_ms();
        let Some(learner) = self.learner.as_mut() else {
            return;
        };
        if self.raw.connected {
            learner.update(now, &self.raw.report);
        }
        if learner.is_done() {
            let mut summary = learner.status();
            self.config.apply(learner.found());
            summary.saved = Some(match self.config.save(&self.config_path) {
                Ok(()) => format!("Saved to {}", self.config_path.display()),
                Err(e) => format!("Couldn't save: {e:#}"),
            });
            eprintln!(
                "channel detection finished: {}",
                summary.saved.as_deref().unwrap_or("")
            );
            self.finished = Some(summary);
            self.learner = None;
        }
    }

    fn on_command(&mut self, cmd: Command) {
        match cmd {
            Command::LearnStart => {
                // nothing to detect without a radio, and the demo moves every control at once
                if !self.raw.connected || self.raw.demo {
                    return;
                }
                self.finished = None;
                self.learner = Some(Learner::default());
                self.step_learner();
            }
            Command::LearnSkip => {
                if let Some(l) = self.learner.as_mut() {
                    l.skip();
                }
                self.step_learner();
            }
            Command::LearnCancel => self.learner = None,
            Command::LearnClose => self.finished = None,
            Command::SetMode { mode } => {
                if (1..=4).contains(&mode) && mode != self.config.mode {
                    self.config.mode = mode;
                    self.save("the stick mode");
                }
            }
            Command::SetSkin { skin } => {
                let ok = skin.as_deref().is_none_or(crate::skins::valid_name);
                if ok && skin != self.config.skin {
                    self.config.skin = skin;
                    self.save("the skin choice");
                }
            }
            Command::SkinsChanged => self.skin_rev += 1,
            Command::SetShow {
                readout,
                channels,
                labels,
                antenna,
                shadow,
            } => {
                let c = &mut self.config;
                let shown = [
                    c.show_readout,
                    c.show_channels,
                    c.show_labels,
                    c.show_antenna,
                    c.show_shadow,
                ];
                if [readout, channels, labels, antenna, shadow] != shown {
                    c.show_readout = readout;
                    c.show_channels = channels;
                    c.show_labels = labels;
                    c.show_antenna = antenna;
                    c.show_shadow = shadow;
                    self.save("what shows under and around the radio");
                }
            }
            Command::SetShadow {
                angle,
                distance,
                blur,
                strength,
            } => {
                let shadow = Shadow {
                    angle,
                    distance,
                    blur,
                    strength,
                };
                if shadow.valid() && shadow != self.config.shadow {
                    self.config.shadow = shadow;
                    self.save("the shadow");
                }
            }
            Command::SetLit { control, at } => {
                let changed = self.config.lit_at(&control).is_some_and(|now| now != at);
                if changed && self.config.set_lit(&control, &at) {
                    self.save(&format!("where {control} lights up"));
                }
            }
            Command::SetS1Deadzone { percent } => {
                if percent <= MAX_S1_DEADZONE && percent != self.config.s1_deadzone {
                    self.config.s1_deadzone = percent;
                    self.save("S1's deadzone");
                }
            }
            Command::SetLan { on } => {
                self.lan_error = None;
                if on != self.config.lan {
                    self.config.lan = on;
                    self.save("sharing with other PCs");
                }
            }
            Command::LanFailed(reason) => {
                self.lan_error = Some(reason);
                self.config.lan = false;
                self.save("sharing with other PCs");
            }
            Command::SetAccent { accent } => {
                let accent = accent.map(|a| a.to_ascii_lowercase());
                let ok = accent.as_deref().is_none_or(crate::config::valid_colour);
                if ok && accent != self.config.accent {
                    self.config.accent = accent;
                    self.save("the accent colour");
                }
            }
        }
    }

    fn save(&self, what: &str) {
        if let Err(e) = self.config.save(&self.config_path) {
            eprintln!("couldn't save {what}: {e:#}");
        }
    }

    /// Runs until every command sender is dropped.
    pub async fn run(
        mut self,
        mut raw_rx: watch::Receiver<RawState>,
        mut cmd_rx: mpsc::Receiver<Command>,
        state_tx: watch::Sender<OverlayState>,
    ) {
        let mut tick = tokio::time::interval(TICK);
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        // A finished replay closes the input; keep serving the last state and commands.
        let mut input_open = true;
        loop {
            tokio::select! {
                changed = raw_rx.changed(), if input_open => {
                    if changed.is_err() {
                        input_open = false;
                        continue;
                    }
                    let raw = raw_rx.borrow_and_update().clone();
                    self.on_raw(raw);
                }
                cmd = cmd_rx.recv() => match cmd {
                    Some(cmd) => self.on_command(cmd),
                    None => return,
                },
                // Holds must complete even if the radio stops reporting changes.
                _ = tick.tick(), if self.learner.is_some() => self.step_learner(),
            }
            let next = self.state();
            state_tx.send_if_modified(|old| {
                let changed = *old != next;
                if changed {
                    *old = next;
                }
                changed
            });
        }
    }
}
