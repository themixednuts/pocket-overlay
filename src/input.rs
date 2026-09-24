//! Input sources. Each runs on its own thread and publishes `RawState` snapshots:
//!
//! - `run_hid`: the real radio. Reads the HID report descriptor the device announces and
//!   decodes with that layout, falling back to EdgeTX's classic layout.
//! - `run_replay`: a text stream of reports (a file made with `--record`, or stdin). This is
//!   also how the black-box tests drive the app like a virtual radio.
//! - `run_demo`: synthetic movement for setting up an OBS scene without the radio.

use std::fs::File;
use std::io::{BufRead, BufReader, BufWriter, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use hidapi::{HidApi, HidDevice};
use tokio::sync::watch;

use crate::edgetx;
use crate::hid::{Layout, Report};

#[derive(Debug, Clone, Default, PartialEq)]
pub struct RawState {
    pub connected: bool,
    pub name: String,
    /// Human-readable description of the report layout in use.
    pub layout: String,
    pub report: Report,
    /// Made-up input from `--demo`: every control moves by itself.
    pub demo: bool,
}

fn publish(tx: &watch::Sender<RawState>, next: RawState) {
    tx.send_if_modified(|old| {
        let changed = *old != next;
        if changed {
            *old = next;
        }
        changed
    });
}

pub fn describe(layout: &Layout) -> String {
    let kind = if *layout == edgetx::classic_layout() {
        "EdgeTX classic"
    } else {
        "custom"
    };
    format!(
        "{kind}: {} axes, {} buttons",
        layout.axis_count(),
        layout.button_count()
    )
}

// ---------------------------------------------------------------------------------------
// real radio

/// What the reconnect loop needs from a USB HID stack. The real one is hidapi; tests use a
/// scripted fake to exercise unplugging, stalls and bad data without hardware.
///
/// Deliberately read-only: there is no way to send anything to the radio, so the overlay
/// can't change its state or disturb a game that is using it at the same time.
pub trait HidBackend {
    /// Devices with this VID/PID currently plugged in: (key for `open`, product name).
    fn list(&mut self, vid: u16, pid: u16) -> Vec<(usize, String)>;
    fn open(&mut self, key: usize) -> Result<Box<dyn HidPort>, String>;
}

pub trait HidPort {
    fn report_descriptor(&self, buf: &mut [u8]) -> Result<usize, String>;
    /// `Ok(0)` when nothing arrived within `timeout_ms`; `Err` once the device is gone.
    fn read_timeout(&self, buf: &mut [u8], timeout_ms: i32) -> Result<usize, String>;
}

/// How long to wait between reconnect attempts and for each read.
#[derive(Debug, Clone, Copy)]
pub struct HidTiming {
    pub reconnect: Duration,
    pub read_timeout_ms: i32,
}

impl Default for HidTiming {
    fn default() -> Self {
        Self {
            reconnect: Duration::from_secs(1),
            read_timeout_ms: 250,
        }
    }
}

/// hidapi, opened so other programs (a sim, a game) can keep using the radio too.
pub struct Hidapi {
    api: HidApi,
    paths: Vec<std::ffi::CString>,
}

impl Hidapi {
    pub fn new() -> Result<Self, String> {
        let api = HidApi::new().map_err(|e| e.to_string())?;
        // hidapi seizes devices exclusively on macOS unless told not to (the
        // `macos-shared-device` feature does this at init too); Windows and Linux always share.
        #[cfg(target_os = "macos")]
        api.set_open_exclusive(false);
        Ok(Self {
            api,
            paths: Vec::new(),
        })
    }

    /// Whether devices get opened exclusively (only ever true on macOS if misconfigured).
    pub fn opens_exclusively(&self) -> bool {
        #[cfg(target_os = "macos")]
        return self.api.get_open_exclusive();
        #[cfg(not(target_os = "macos"))]
        false
    }
}

impl HidBackend for Hidapi {
    fn list(&mut self, vid: u16, pid: u16) -> Vec<(usize, String)> {
        if self.api.refresh_devices().is_err() {
            return Vec::new();
        }
        self.paths.clear();
        let mut found = Vec::new();
        for d in self.api.device_list() {
            if d.vendor_id() == vid && d.product_id() == pid {
                found.push((
                    self.paths.len(),
                    d.product_string().unwrap_or("EdgeTX joystick").to_owned(),
                ));
                self.paths.push(d.path().to_owned());
            }
        }
        found
    }

    fn open(&mut self, key: usize) -> Result<Box<dyn HidPort>, String> {
        let path = self.paths.get(key).ok_or("device went away")?;
        let dev = self.api.open_path(path).map_err(|e| e.to_string())?;
        Ok(Box::new(dev))
    }
}

impl HidPort for HidDevice {
    fn report_descriptor(&self, buf: &mut [u8]) -> Result<usize, String> {
        self.get_report_descriptor(buf).map_err(|e| e.to_string())
    }

    fn read_timeout(&self, buf: &mut [u8], timeout_ms: i32) -> Result<usize, String> {
        HidDevice::read_timeout(self, buf, timeout_ms).map_err(|e| e.to_string())
    }
}

/// Blocking loop: finds the radio by USB VID/PID, publishes every decoded report, and
/// reconnects when it's unplugged. With `record`, every report is also appended to a file
/// `run_replay` can play back.
pub fn run_hid(vid: u16, pid: u16, record: Option<PathBuf>, tx: watch::Sender<RawState>) {
    match Hidapi::new() {
        Ok(mut backend) => run_hid_with(&mut backend, vid, pid, record, HidTiming::default(), tx),
        Err(e) => eprintln!("error: can't use USB devices on this computer: {e}"),
    }
}

/// `run_hid` against any backend. Returns once nothing is listening any more.
pub fn run_hid_with(
    backend: &mut dyn HidBackend,
    vid: u16,
    pid: u16,
    record: Option<PathBuf>,
    timing: HidTiming,
    tx: watch::Sender<RawState>,
) {
    let mut recorder = record.and_then(|p| match Recorder::create(&p) {
        Ok(r) => Some(r),
        Err(e) => {
            eprintln!("can't record to {}: {e}", p.display());
            None
        }
    });
    let mut waiting_logged = false;
    let mut open_error_logged = None;
    while !tx.is_closed() {
        let radios = backend.list(vid, pid);
        match pick_radio(&radios) {
            None => {
                if !waiting_logged {
                    eprintln!(
                        "Waiting for the radio: plug it in over USB and choose \"USB Joystick (HID)\" on its screen."
                    );
                    waiting_logged = true;
                }
            }
            Some((key, name)) => match backend.open(key) {
                Err(e) => {
                    // e.g. another program holds it exclusively; keep trying quietly
                    if open_error_logged.as_ref() != Some(&e) {
                        eprintln!("Found {name} but couldn't open it ({e}); retrying.");
                        open_error_logged = Some(e);
                    }
                }
                Ok(port) => {
                    waiting_logged = false;
                    open_error_logged = None;
                    if radios.len() > 1 {
                        eprintln!(
                            "{} EdgeTX radios are plugged in; using {name}.",
                            radios.len()
                        );
                    }
                    let (layout, descriptor) = device_layout(port.as_ref());
                    eprintln!("Radio connected: {name}");
                    if layout != edgetx::classic_layout() {
                        eprintln!("  it reports a non-standard layout ({})", describe(&layout));
                    }
                    if !name.contains("Pocket") {
                        eprintln!("  this isn't a Pocket; the drawing will still show a Pocket");
                    }
                    if let Some(r) = recorder.as_mut() {
                        r.header(&name, &descriptor);
                    }
                    let gone = read_until_error(
                        port.as_ref(),
                        &name,
                        &layout,
                        timing,
                        recorder.as_mut(),
                        &tx,
                    );
                    if !gone {
                        return; // nobody listening any more
                    }
                    eprintln!("Radio unplugged; waiting for it to come back.");
                    if let Some(r) = recorder.as_mut() {
                        r.line("disconnect");
                    }
                    tx.send_replace(RawState::default());
                    // look again soon: a flaky cable often comes right back
                    std::thread::sleep(timing.reconnect / 10);
                    continue;
                }
            },
        }
        std::thread::sleep(timing.reconnect);
    }
}

/// With several EdgeTX radios plugged in (they all share one USB ID), prefer the Pocket.
fn pick_radio(radios: &[(usize, String)]) -> Option<(usize, String)> {
    radios
        .iter()
        .find(|(_, name)| name.contains("Pocket"))
        .or(radios.first())
        .cloned()
}

/// The layout the device announces (and its descriptor bytes), or EdgeTX classic if it
/// can't be read or parsed.
fn device_layout(dev: &dyn HidPort) -> (Layout, Vec<u8>) {
    let mut buf = [0u8; 4096];
    match dev.report_descriptor(&mut buf) {
        Ok(n) => match Layout::parse(&buf[..n]) {
            Ok(layout) => return (layout, buf[..n].to_vec()),
            Err(e) => {
                eprintln!("couldn't parse the report descriptor ({e}); assuming EdgeTX classic")
            }
        },
        Err(e) => eprintln!("couldn't read the report descriptor ({e}); assuming EdgeTX classic"),
    }
    (
        edgetx::classic_layout(),
        edgetx::CLASSIC_DESCRIPTOR.to_vec(),
    )
}

/// Reads until the device goes away (returns true) or nobody is listening (false).
fn read_until_error(
    dev: &dyn HidPort,
    name: &str,
    layout: &Layout,
    timing: HidTiming,
    mut recorder: Option<&mut Recorder>,
    tx: &watch::Sender<RawState>,
) -> bool {
    let classic = edgetx::classic_layout();
    let (announced_desc, classic_desc) = (describe(layout), describe(&classic));
    let mut warned = false;
    let mut buf = [0u8; 256];
    loop {
        if tx.is_closed() {
            return false;
        }
        let n = match dev.read_timeout(&mut buf, timing.read_timeout_ms) {
            // Nothing new: keep showing the last position rather than guessing.
            Ok(0) => continue,
            Ok(n) => n,
            Err(_) => return true,
        };
        let bytes = &buf[..n];
        if let Some(r) = recorder.as_deref_mut() {
            r.report(bytes);
        }
        // Prefer the announced layout; Windows can rebuild descriptors slightly
        // differently, so fall back to classic when only that one fits.
        let (report, used) = match layout.decode(bytes) {
            Some(r) => (r, &announced_desc),
            None => match classic.decode(bytes) {
                Some(r) => (r, &classic_desc),
                None => {
                    if !warned {
                        eprintln!("ignoring {n}-byte reports that match no known layout");
                        warned = true;
                    }
                    continue;
                }
            },
        };
        publish(
            tx,
            RawState {
                connected: true,
                name: name.to_owned(),
                layout: used.clone(),
                report,
                demo: false,
            },
        );
    }
}

// ---------------------------------------------------------------------------------------
// record / replay

/// One line of a recording.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Line {
    Report(Vec<u8>),
    Descriptor(Vec<u8>),
    Name(String),
    Wait(u64),
    Disconnect,
    Skip,
}

pub fn parse_line(line: &str) -> Result<Line, String> {
    let line = line.trim();
    if line.is_empty() || line.starts_with('#') {
        return Ok(Line::Skip);
    }
    let (cmd, rest) = line
        .split_once(' ')
        .map_or((line, ""), |(c, r)| (c, r.trim()));
    match cmd {
        "name" => Ok(Line::Name(rest.to_owned())),
        "wait" => rest
            .parse()
            .map(Line::Wait)
            .map_err(|_| format!("bad wait: {line}")),
        "descriptor" => hex(rest).map(Line::Descriptor),
        "disconnect" => Ok(Line::Disconnect),
        _ => hex(line).map(Line::Report),
    }
}

fn hex(s: &str) -> Result<Vec<u8>, String> {
    let s: String = s.chars().filter(|c| !c.is_whitespace()).collect();
    if !s.len().is_multiple_of(2) {
        return Err(format!("odd-length hex: {s}"));
    }
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&s[i..i + 2], 16).map_err(|_| format!("bad hex: {s}")))
        .collect()
}

fn to_hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

struct Recorder {
    out: BufWriter<File>,
    last: Instant,
}

impl Recorder {
    fn create(path: &Path) -> std::io::Result<Self> {
        let mut out = BufWriter::new(File::create(path)?);
        writeln!(
            out,
            "# pocket-overlay recording - play back with --replay {}",
            path.display()
        )?;
        Ok(Self {
            out,
            last: Instant::now(),
        })
    }

    fn line(&mut self, s: &str) {
        let _ = writeln!(self.out, "{s}").and_then(|_| self.out.flush());
    }

    fn header(&mut self, name: &str, descriptor: &[u8]) {
        self.line(&format!("name {name}"));
        self.line(&format!("descriptor {}", to_hex(descriptor)));
        self.last = Instant::now();
    }

    fn report(&mut self, bytes: &[u8]) {
        let dt = self.last.elapsed().as_millis();
        self.last = Instant::now();
        self.line(&format!("wait {dt}\n{}", to_hex(bytes)));
    }
}

/// Plays reports from `path` (`-` for stdin), honouring `wait` lines. Keeps the last
/// state when the input ends.
pub fn run_replay(path: PathBuf, tx: watch::Sender<RawState>) {
    let reader: Box<dyn BufRead> = if path.as_os_str() == "-" {
        Box::new(BufReader::new(std::io::stdin()))
    } else {
        match File::open(&path) {
            Ok(f) => Box::new(BufReader::new(f)),
            Err(e) => {
                eprintln!("can't open {}: {e}", path.display());
                return;
            }
        }
    };
    let mut layout = edgetx::classic_layout();
    let mut state = RawState {
        name: "Replay".into(),
        layout: describe(&layout),
        ..RawState::default()
    };
    for (n, line) in reader.lines().enumerate() {
        let Ok(line) = line else { break };
        match parse_line(&line) {
            Ok(Line::Skip) => {}
            Ok(Line::Name(name)) => state.name = name,
            Ok(Line::Wait(ms)) => std::thread::sleep(Duration::from_millis(ms)),
            Ok(Line::Descriptor(bytes)) => match Layout::parse(&bytes) {
                Ok(l) => {
                    state.layout = describe(&l);
                    layout = l;
                }
                Err(e) => eprintln!("line {}: {e}", n + 1),
            },
            Ok(Line::Disconnect) => {
                state.connected = false;
                publish(
                    &tx,
                    RawState {
                        name: state.name.clone(),
                        ..RawState::default()
                    },
                );
            }
            Ok(Line::Report(bytes)) => match layout.decode(&bytes) {
                Some(report) => {
                    state.connected = true;
                    state.report = report;
                    publish(&tx, state.clone());
                }
                None => eprintln!(
                    "line {}: report doesn't fit the layout ({})",
                    n + 1,
                    state.layout
                ),
            },
            Err(e) => eprintln!("line {}: {e}", n + 1),
        }
    }
}

// ---------------------------------------------------------------------------------------
// demo

/// Fake radio: builds the bytes EdgeTX would send and decodes them like a real report.
pub fn run_demo(tx: watch::Sender<RawState>) {
    let start = Instant::now();
    let layout = edgetx::classic_layout();
    loop {
        let t = start.elapsed().as_secs_f32();
        let ch = |v: f32| (v.clamp(-1.0, 1.0) * 1024.0) as i16;
        let step = |period: f32, n: u32| {
            let phase = ((t / period) as u32) % n;
            -1.0 + 2.0 * phase as f32 / (n - 1) as f32
        };
        let mut channels = [0i16; edgetx::CHANNELS];
        // sticks trace a figure-eight and a circle, throttle sweeps slowly
        channels[0] = ch((t * 1.1).sin() * 0.9); // ail
        channels[1] = ch((t * 1.1).cos() * 0.9); // ele
        channels[2] = ch((t * 0.35).sin() * 1.1); // thr
        channels[3] = ch((t * 1.4).sin() * (t * 0.7).cos() * 0.8); // rud
        channels[4] = ch(step(3.0, 2)); // SA
        channels[5] = ch(step(2.0, 3)); // SB
        channels[6] = ch(step(2.5, 3)); // SC
        channels[7] = ch((t * 0.5).sin()); // S1
        channels[8] = ch(step(4.0, 2)); // SD
        channels[9] = if (t % 3.0) < 0.4 { 1024 } else { -1024 }; // SE
        let report = layout
            .decode(&edgetx::encode_classic(&channels))
            .expect("demo report decodes");
        publish(
            &tx,
            RawState {
                connected: true,
                name: "Demo radio".into(),
                layout: describe(&layout),
                report,
                demo: true,
            },
        );
        std::thread::sleep(Duration::from_millis(16));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_recording_lines() {
        assert_eq!(parse_line("  # hi"), Ok(Line::Skip));
        assert_eq!(parse_line("wait 16"), Ok(Line::Wait(16)));
        assert_eq!(
            parse_line("name Radiomaster Pocket Joystick"),
            Ok(Line::Name("Radiomaster Pocket Joystick".into()))
        );
        assert_eq!(parse_line("disconnect"), Ok(Line::Disconnect));
        assert_eq!(parse_line("00ff 10"), Ok(Line::Report(vec![0, 0xff, 0x10])));
        assert_eq!(
            parse_line("descriptor 0501"),
            Ok(Line::Descriptor(vec![5, 1]))
        );
        assert!(parse_line("0g").is_err());
        assert!(parse_line("abc").is_err());
    }
}
