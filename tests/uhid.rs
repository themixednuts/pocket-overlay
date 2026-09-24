//! Linux only: plugs in a virtual RadioMaster Pocket with uhid (the kernel's user-space HID
//! devices), so the real USB path runs against the real kernel HID stack without hardware.
//! pocket-overlay finds the radio by its USB IDs and reads it through hidraw, while "games"
//! read the same radio as a gamepad through evdev, the way SDL and most Linux games do.
//!
//! Checks that everyone sees every report: whoever opened the radio first, with two overlays
//! at once, when a game grabs the gamepad for itself, and across unplugging and plugging it
//! back in.
//!
//! Needs /dev/uhid and the nodes it creates to be readable and writable; CI sets that up with
//! the udev rule we ship (see .github/workflows/ci.yml). Skips otherwise, unless POCKET_UHID=1.
#![cfg(target_os = "linux")]

mod common;

use std::collections::BTreeMap;
use std::fs::{File, OpenOptions};
use std::io::{Read, Write};
use std::os::fd::AsRawFd;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use common::ws::Ws;
use common::{Overlay, Rng, expected_channels, to_hex};
use pocket_overlay::edgetx::{self, AXES, BUTTONS, CHANNELS};
use serde_json::{Value, json};

// <linux/uhid.h>
const UHID_DESTROY: u32 = 1;
const UHID_CREATE2: u32 = 11;
const UHID_INPUT2: u32 = 12;
/// sizeof(struct uhid_event): the type, then its largest request (create2).
const UHID_EVENT_LEN: usize = 4 + 128 + 64 + 64 + 2 + 2 + 4 * 4 + 4096;
const BUS_USB: u16 = 0x03;

// <linux/input.h>
const EV_SYN: u16 = 0;
const EV_KEY: u16 = 1;
const EV_ABS: u16 = 3;
const SYN_REPORT: u16 = 0;
const SYN_DROPPED: u16 = 3;
const EVIOCGRAB: u32 = 0x4004_4590; // _IOW('E', 0x90, int)

const TIMEOUT: Duration = Duration::from_secs(10);

// ---------------------------------------------------------------------------------------
// the virtual radio

/// A virtual Pocket: plugged in from `plug_in` until `unplug` (or drop).
struct VirtualPocket {
    uhid: File,
}

impl VirtualPocket {
    fn plug_in() -> std::io::Result<Self> {
        let mut uhid = OpenOptions::new()
            .read(true)
            .write(true)
            .open("/dev/uhid")?;
        let mut ev = vec![0u8; UHID_EVENT_LEN];
        ev[..4].copy_from_slice(&UHID_CREATE2.to_ne_bytes());
        let name = edgetx::POCKET_PRODUCT.as_bytes();
        ev[4..4 + name.len()].copy_from_slice(name);
        let at = 4 + 128 + 64 + 64; // after name, phys and uniq
        let rd = &edgetx::CLASSIC_DESCRIPTOR;
        ev[at..at + 2].copy_from_slice(&(rd.len() as u16).to_ne_bytes());
        ev[at + 2..at + 4].copy_from_slice(&BUS_USB.to_ne_bytes());
        ev[at + 4..at + 8].copy_from_slice(&u32::from(edgetx::USB_VID).to_ne_bytes());
        ev[at + 8..at + 12].copy_from_slice(&u32::from(edgetx::USB_PID).to_ne_bytes());
        ev[at + 12..at + 16].copy_from_slice(&0x0100u32.to_ne_bytes()); // version
        // country code (0), then the report descriptor
        ev[at + 20..at + 20 + rd.len()].copy_from_slice(rd);
        uhid.write_all(&ev)?;
        Ok(Self { uhid })
    }

    /// Sends one report: exactly the bytes EdgeTX sends for these channels.
    fn send(&mut self, ch: &[i16; CHANNELS]) {
        let report = edgetx::encode_classic(ch);
        let mut ev = vec![0u8; UHID_EVENT_LEN];
        ev[..4].copy_from_slice(&UHID_INPUT2.to_ne_bytes());
        ev[4..6].copy_from_slice(&(report.len() as u16).to_ne_bytes());
        ev[6..6 + report.len()].copy_from_slice(&report);
        self.uhid.write_all(&ev).expect("send a report");
    }

    fn unplug(mut self) {
        let mut ev = vec![0u8; UHID_EVENT_LEN];
        ev[..4].copy_from_slice(&UHID_DESTROY.to_ne_bytes());
        self.uhid.write_all(&ev).expect("unplug");
    }
}

/// The radio's hidraw node (what the overlay opens) and evdev node (what games open).
struct Nodes {
    hidraw: PathBuf,
    event: PathBuf,
}

/// Waits for the kernel to finish setting up the radio.
fn wait_for_nodes() -> Nodes {
    let deadline = Instant::now() + TIMEOUT;
    loop {
        if let Some(nodes) = find_nodes() {
            return nodes;
        }
        assert!(
            Instant::now() < deadline,
            "the kernel never made hidraw and evdev nodes for the virtual radio (hid-generic and evdev loaded?)"
        );
        std::thread::sleep(Duration::from_millis(50));
    }
}

fn find_nodes() -> Option<Nodes> {
    // HID devices are named bus:vendor:product.instance, e.g. 0003:1209:4F54.0007
    let prefix = format!("0003:{:04X}:{:04X}.", edgetx::USB_VID, edgetx::USB_PID);
    let dev = std::fs::read_dir("/sys/bus/hid/devices")
        .ok()?
        .flatten()
        .find(|d| d.file_name().to_string_lossy().starts_with(&prefix))?
        .path();
    let hidraw = first_entry(&dev.join("hidraw"), "hidraw")?;
    let event = first_entry(&first_entry(&dev.join("input"), "input")?, "event")?;
    Some(Nodes {
        hidraw: Path::new("/dev").join(hidraw.file_name()?),
        event: Path::new("/dev/input").join(event.file_name()?),
    })
}

fn first_entry(dir: &Path, prefix: &str) -> Option<PathBuf> {
    std::fs::read_dir(dir)
        .ok()?
        .flatten()
        .map(|e| e.path())
        .find(|p| {
            p.file_name()
                .is_some_and(|n| n.to_string_lossy().starts_with(prefix))
        })
}

// ---------------------------------------------------------------------------------------
// a game

/// Everything a game knows after a report: (event type, code) -> value.
type Frame = BTreeMap<(u16, u16), i32>;

/// A game reading the radio as a gamepad through evdev. Keeps the whole state after every
/// report (each SYN_REPORT).
struct Game {
    file: File,
    frames: Arc<Mutex<Vec<Frame>>>,
    dropped: Arc<AtomicBool>,
    gone: Arc<AtomicBool>,
}

impl Game {
    fn open(node: &Path) -> Self {
        // udev may still be setting the node's permissions
        let deadline = Instant::now() + Duration::from_secs(5);
        let file = loop {
            match File::open(node) {
                Ok(f) => break f,
                Err(_) if Instant::now() < deadline => {
                    std::thread::sleep(Duration::from_millis(50))
                }
                Err(e) => panic!("a game can't open {}: {e}", node.display()),
            }
        };
        let frames = Arc::new(Mutex::new(Vec::new()));
        let dropped = Arc::new(AtomicBool::new(false));
        let gone = Arc::new(AtomicBool::new(false));
        let mut reader = file.try_clone().unwrap();
        let (frames2, dropped2, gone2) = (frames.clone(), dropped.clone(), gone.clone());
        std::thread::spawn(move || {
            let size = std::mem::size_of::<libc::input_event>();
            let mut buf = vec![0u8; size * 64];
            let mut state = Frame::new();
            loop {
                let n = match reader.read(&mut buf) {
                    Ok(n) if n > 0 => n,
                    _ => break, // ENODEV: unplugged
                };
                for ev in buf[..n].chunks_exact(size) {
                    // struct input_event ends with type (u16), code (u16) and value (i32)
                    let t = size - 8;
                    let kind = u16::from_ne_bytes([ev[t], ev[t + 1]]);
                    let code = u16::from_ne_bytes([ev[t + 2], ev[t + 3]]);
                    let value = i32::from_ne_bytes(ev[t + 4..t + 8].try_into().unwrap());
                    match (kind, code) {
                        (EV_SYN, SYN_REPORT) => frames2.lock().unwrap().push(state.clone()),
                        (EV_SYN, SYN_DROPPED) => dropped2.store(true, Ordering::SeqCst),
                        (EV_KEY | EV_ABS, _) => {
                            state.insert((kind, code), value);
                        }
                        _ => {}
                    }
                }
            }
            gone2.store(true, Ordering::SeqCst);
        });
        Game {
            file,
            frames,
            dropped,
            gone,
        }
    }

    /// Takes the gamepad for itself (EVIOCGRAB), as some games do: other evdev readers stop
    /// getting its events.
    fn grab(&self) {
        let grab: libc::c_int = 1;
        // SAFETY: EVIOCGRAB takes an int by value; the fd is open for as long as `self`.
        let r = unsafe { libc::ioctl(self.file.as_raw_fd(), EVIOCGRAB as _, grab) };
        assert_eq!(r, 0, "EVIOCGRAB: {}", std::io::Error::last_os_error());
    }

    /// Waits until the game's latest state passes `done`, and returns every state it had.
    fn frames_until(&self, done: impl Fn(&Frame) -> bool, what: &str) -> Vec<Frame> {
        let deadline = Instant::now() + TIMEOUT;
        loop {
            let frames = self.frames.lock().unwrap().clone();
            if frames.last().is_some_and(&done) {
                assert!(
                    !self.dropped.load(Ordering::SeqCst),
                    "the kernel dropped events for a game ({what})"
                );
                return frames;
            }
            assert!(
                Instant::now() < deadline,
                "a game never saw the last report ({what}); its state: {:?}",
                frames.last()
            );
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    /// Waits until the game has had `n` states in all, and returns them. For small reports,
    /// which the kernel delivers in one piece.
    fn frames(&self, n: usize, what: &str) -> Vec<Frame> {
        let deadline = Instant::now() + TIMEOUT;
        loop {
            let frames = self.frames.lock().unwrap().clone();
            if frames.len() >= n {
                assert!(
                    !self.dropped.load(Ordering::SeqCst),
                    "the kernel dropped events for a game ({what})"
                );
                return frames;
            }
            assert!(
                Instant::now() < deadline,
                "a game saw {} reports, expected {n} ({what})",
                frames.len()
            );
            std::thread::sleep(Duration::from_millis(10));
        }
    }
}

/// Which evdev code each channel became for a game, learned by moving one at a time.
struct Mapping {
    axes: Vec<(u16, u16)>,
    buttons: Vec<(u16, u16)>,
}

impl Mapping {
    /// What a game should see for these channels.
    fn frame(&self, ch: &[i16; CHANNELS]) -> Frame {
        let mut f = Frame::new();
        for (i, code) in self.axes.iter().enumerate() {
            f.insert(*code, i32::from(ch[i].clamp(-1024, 1024)) + 1024);
        }
        for (j, code) in self.buttons.iter().enumerate() {
            f.insert(*code, i32::from(ch[AXES + j] > 0));
        }
        f
    }

    /// A game's state, reading codes it hasn't had an event for yet as 0 (the kernel's
    /// starting value).
    fn full(&self, got: &Frame) -> Frame {
        self.frame(&[0; CHANNELS])
            .keys()
            .map(|code| (*code, got.get(code).copied().unwrap_or(0)))
            .collect()
    }

    /// Checks `game` saw every one of these reports, in order, after its first `skip` frames.
    ///
    /// The kernel splits a report with more events than it budgets per packet (16 for this
    /// gamepad; a Pocket report can make about 56: 8 axes, plus a scan code and a key for each
    /// button that changes) into several SYN_REPORTs, so a game can also see in-between states
    /// for a moment. Each of those may only mix the report before (`before`, then each one
    /// sent) with the next. That's the kernel, the same for every program; the overlay reads
    /// whole reports from hidraw.
    fn assert_saw(
        &self,
        game: &Game,
        skip: usize,
        before: &[i16; CHANNELS],
        sent: &[[i16; CHANNELS]],
        what: &str,
    ) {
        let last = self.frame(sent.last().unwrap());
        let frames = game.frames_until(|f| self.full(f) == last, what);
        let mut frames = frames[skip..].iter();
        let mut prev = self.frame(before);
        for (k, ch) in sent.iter().enumerate() {
            let want = self.frame(ch);
            loop {
                let got = frames
                    .next()
                    .unwrap_or_else(|| panic!("{what}: never saw report {k} of {}", sent.len()));
                if self.full(got) == want {
                    break;
                }
                for (code, v) in got {
                    assert!(
                        want.get(code) == Some(v) || prev.get(code) == Some(v),
                        "{what}: before report {k}, {code:?} was {v}: neither {:?} nor {:?}",
                        prev.get(code),
                        want.get(code)
                    );
                }
            }
            prev = want;
        }
    }
}

// ---------------------------------------------------------------------------------------
// the radio's movements

/// A random position. The kernel smooths gamepad changes smaller than twice its "fuzz" (8
/// steps on this 0..2048 range) before any game sees them, and the point here is to compare
/// exact values, so every axis moves at least 32 steps from `prev`, and from 0 (where a
/// freshly plugged-in radio starts).
fn next_position(rng: &mut Rng, prev: &[i16; CHANNELS], flip_buttons: bool) -> [i16; CHANNELS] {
    let mut ch = [0i16; CHANNELS];
    for (v, prev) in ch.iter_mut().zip(prev).take(AXES) {
        *v = loop {
            let v = rng.below(2017) as i16 - 992;
            if (v - prev).abs() >= 32 {
                break v;
            }
        };
    }
    for (v, prev) in ch.iter_mut().zip(prev).skip(AXES) {
        let on = if flip_buttons {
            *prev <= 0
        } else {
            rng.below(2) == 1
        };
        *v = if on { 1024 } else { -1024 };
    }
    ch
}

/// The radio, and every report it has sent since it was first plugged in.
struct Radio {
    dev: VirtualPocket,
    sent: Vec<[i16; CHANNELS]>,
    rng: Rng,
}

impl Radio {
    fn send(&mut self, ch: [i16; CHANNELS]) {
        self.dev.send(&ch);
        self.sent.push(ch);
    }

    fn last(&self) -> [i16; CHANNELS] {
        self.sent
            .last()
            .copied()
            .unwrap_or([-1024; CHANNELS])
            .map(|v| v.clamp(-1024, 1024))
    }

    /// Moves everything `n` times at about 500 reports a second (the Pocket can do 1000).
    /// With `flip_buttons`, the first report also flips every button, so a game that opened
    /// late has seen every control once.
    fn wiggle(&mut self, n: usize, flip_buttons: bool) -> Vec<[i16; CHANNELS]> {
        let mut out = Vec::new();
        for k in 0..n {
            let prev = self.last();
            let ch = next_position(&mut self.rng, &prev, flip_buttons && k == 0);
            self.send(ch);
            out.push(ch);
            std::thread::sleep(Duration::from_millis(2));
        }
        out
    }
}

/// Moves each control on its own and records which evdev code it became. The game must
/// have opened the radio before its first report (so everything starts at 0).
fn learn_mapping(radio: &mut Radio, game: &Game) -> Mapping {
    // every axis to its own value; raw values 24, 274, 524 ...
    let mut ch = [-1024i16; CHANNELS];
    for (i, v) in ch.iter_mut().take(AXES).enumerate() {
        *v = -1000 + 250 * i as i16;
    }
    radio.send(ch);
    let frame = game.frames(1, "axes").pop().unwrap();
    let mut axes = vec![None; AXES];
    for (&(kind, code), &v) in &frame {
        if kind == EV_ABS && (v - 24) % 250 == 0 {
            axes[((v - 24) / 250) as usize] = Some((kind, code));
        }
    }
    let axes: Vec<_> = axes
        .into_iter()
        .enumerate()
        .map(|(i, c)| c.unwrap_or_else(|| panic!("a game can't see axis {i}: {frame:?}")))
        .collect();

    // then each button on its own
    let mut buttons = Vec::new();
    for j in 0..BUTTONS {
        let mut ch = ch;
        ch[AXES + j] = 1024;
        radio.send(ch);
        let frame = game.frames(2 + j, "buttons").pop().unwrap();
        let on: Vec<_> = frame
            .iter()
            .filter(|&(&(kind, _), &v)| kind == EV_KEY && v == 1)
            .map(|(code, _)| *code)
            .collect();
        assert_eq!(on.len(), 1, "button {} alone: {frame:?}", j + 1);
        buttons.push(on[0]);
    }
    Mapping { axes, buttons }
}

// ---------------------------------------------------------------------------------------
// the overlay

/// An overlay reading the real radio, recording every report it reads.
struct RecordingOverlay {
    ov: Overlay,
    record: PathBuf,
    _dir: tempfile::TempDir,
}

impl RecordingOverlay {
    fn start() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let record = dir.path().join("seen.txt");
        let ov = Overlay::launch(&["--record", record.to_str().unwrap()], None);
        Self {
            ov,
            record,
            _dir: dir,
        }
    }

    /// Waits for the overlay's `n`th log line containing `text`.
    fn wait_log(&self, text: &str, n: usize) {
        let deadline = Instant::now() + TIMEOUT;
        loop {
            let log = self.ov.log.lock().unwrap().clone();
            if log.iter().filter(|l| l.contains(text)).count() >= n {
                return;
            }
            assert!(
                Instant::now() < deadline,
                "overlay never said {text:?} ({n}x):\n{}",
                log.join("\n")
            );
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    /// The overlay has the radio open: every report from now on reaches it.
    fn wait_connected(&self, times: usize) {
        self.wait_log(
            &format!("Radio connected: {}", edgetx::POCKET_PRODUCT),
            times,
        );
    }

    /// Waits until the overlay has read every report sent, and checks they're exactly those
    /// (same bytes, same order, none missing).
    fn assert_read_all(&self, sent: &[[i16; CHANNELS]], what: &str) {
        let want: Vec<String> = sent
            .iter()
            .map(|ch| to_hex(&edgetx::encode_classic(ch)).to_lowercase())
            .collect();
        let deadline = Instant::now() + TIMEOUT;
        let got = loop {
            let got = self.recorded("");
            if got.len() >= want.len() || Instant::now() > deadline {
                break got;
            }
            std::thread::sleep(Duration::from_millis(20));
        };
        assert_eq!(got.len(), want.len(), "{what}: reports the overlay read");
        for (k, (g, w)) in got.iter().zip(&want).enumerate() {
            assert_eq!(g, w, "{what}: report {k} of {}", want.len());
        }
    }

    /// Lines of the recording: reports (bare hex) with `prefix` "", or e.g. "descriptor ".
    fn recorded(&self, prefix: &str) -> Vec<String> {
        std::fs::read_to_string(&self.record)
            .unwrap_or_default()
            .lines()
            .filter_map(|l| l.strip_prefix(prefix))
            .filter(|l| !l.is_empty() && l.bytes().all(|b| b.is_ascii_hexdigit()))
            .map(str::to_lowercase)
            .collect()
    }

    /// What the overlay's page shows right now matches these channels.
    async fn assert_shows(&self, ch: &[i16; CHANNELS], what: &str) -> Value {
        // a fresh connection each time: a page that stops reading for 5 s gets dropped
        let mut ws = Ws::connect(self.ov.port).await;
        let want = expected_channels(ch);
        ws.wait_for(what, |s| {
            s["connected"] == json!(true)
                && s["name"] == json!(edgetx::POCKET_PRODUCT)
                && s["channels"].as_array().is_some_and(|got| {
                    got.iter()
                        .zip(want)
                        .all(|(g, w)| g.as_i64() == Some(i64::from(w)))
                })
        })
        .await
    }
}

// ---------------------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn overlay_and_games_share_the_radio() {
    let dev = match VirtualPocket::plug_in() {
        Ok(dev) => dev,
        Err(e) if std::env::var("POCKET_UHID").as_deref() == Ok("1") => {
            panic!("can't plug in a virtual radio: /dev/uhid: {e}")
        }
        Err(e) => {
            eprintln!("SKIPPED: can't plug in a virtual radio (/dev/uhid: {e})");
            return;
        }
    };
    let nodes = wait_for_nodes();
    eprintln!(
        "virtual radio: {} and {}",
        nodes.hidraw.display(),
        nodes.event.display()
    );
    let mut radio = Radio {
        dev,
        sent: Vec::new(),
        rng: Rng(0x0b5),
    };

    // A game already has the radio open when the overlays start.
    let early_game = Game::open(&nodes.event);
    let overlay = RecordingOverlay::start();
    overlay.wait_connected(1);
    // It found the radio by its USB IDs and read its descriptor from the kernel.
    assert_eq!(
        overlay.recorded("descriptor "),
        [to_hex(&edgetx::CLASSIC_DESCRIPTOR).to_lowercase()]
    );
    // A second overlay (or anything else reading the radio over hidraw) at the same time.
    let overlay2 = RecordingOverlay::start();
    overlay2.wait_connected(1);

    let mapping = learn_mapping(&mut radio, &early_game);
    let before = radio.last();
    let moves = radio.wiggle(300, false);
    mapping.assert_saw(&early_game, 1 + BUTTONS, &before, &moves, "early game");
    for (ov, name) in [(&overlay, "overlay"), (&overlay2, "second overlay")] {
        ov.assert_read_all(&radio.sent, name);
        ov.assert_shows(&radio.last(), name).await;
    }

    // A game that opens the radio later and grabs the gamepad for itself: grabbing is an
    // evdev thing (other games stop getting events), the overlays read hidraw.
    let greedy_game = Game::open(&nodes.event);
    greedy_game.grab();
    let before = radio.last();
    let moves = radio.wiggle(100, true);
    mapping.assert_saw(&greedy_game, 0, &before, &moves, "grabbing game");
    for (ov, name) in [(&overlay, "overlay"), (&overlay2, "second overlay")] {
        ov.assert_read_all(&radio.sent, &format!("{name}, with the gamepad grabbed"));
        ov.assert_shows(&radio.last(), name).await;
    }

    // Unplugged with everything running...
    radio.dev.unplug();
    for ov in [&overlay, &overlay2] {
        ov.wait_log("Radio unplugged", 1);
        let mut ws = Ws::connect(ov.ov.port).await;
        ws.wait_for("unplugged", |s| s["connected"] == json!(false))
            .await;
    }
    let deadline = Instant::now() + TIMEOUT;
    while !greedy_game.gone.load(Ordering::SeqCst) {
        assert!(
            Instant::now() < deadline,
            "the game never noticed the unplug"
        );
        std::thread::sleep(Duration::from_millis(20));
    }

    // ...and plugged back in: the overlays come back on their own, next to a new game.
    radio.dev = VirtualPocket::plug_in().expect("plug back in");
    let nodes = wait_for_nodes();
    let new_game = Game::open(&nodes.event);
    overlay.wait_connected(2);
    overlay2.wait_connected(2);
    let before = radio.last();
    let moves = radio.wiggle(100, true);
    mapping.assert_saw(&new_game, 0, &before, &moves, "game after replugging");
    for (ov, name) in [(&overlay, "overlay"), (&overlay2, "second overlay")] {
        ov.assert_read_all(&radio.sent, &format!("{name}, after replugging"));
        ov.assert_shows(&radio.last(), name).await;
    }
}
