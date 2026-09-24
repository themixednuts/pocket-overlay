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

const RECONNECT_INTERVAL: Duration = Duration::from_secs(1);
const READ_TIMEOUT_MS: i32 = 500;

/// Blocking loop: finds the radio by USB VID/PID, publishes every decoded report, and
/// reconnects when it's unplugged. With `record`, every report is also appended to a file
/// `run_replay` can play back.
pub fn run_hid(vid: u16, pid: u16, record: Option<PathBuf>, tx: watch::Sender<RawState>) {
    let mut api = match HidApi::new() {
        Ok(api) => api,
        Err(e) => {
            eprintln!("failed to initialise HID: {e}");
            return;
        }
    };
    let mut recorder = record.and_then(|p| match Recorder::create(&p) {
        Ok(r) => Some(r),
        Err(e) => {
            eprintln!("can't record to {}: {e}", p.display());
            None
        }
    });
    let mut waiting_logged = false;
    loop {
        match open(&mut api, vid, pid) {
            Some((dev, name)) => {
                waiting_logged = false;
                let (layout, descriptor) = device_layout(&dev);
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
                read_until_error(&dev, &name, &layout, recorder.as_mut(), &tx);
                eprintln!("Radio unplugged; waiting for it to come back.");
                if let Some(r) = recorder.as_mut() {
                    r.line("disconnect");
                }
                tx.send_replace(RawState::default());
            }
            None if !waiting_logged => {
                eprintln!(
                    "Waiting for the radio: plug it in over USB and choose \"USB Joystick (HID)\" on its screen."
                );
                waiting_logged = true;
            }
            None => {}
        }
        std::thread::sleep(RECONNECT_INTERVAL);
    }
}

fn open(api: &mut HidApi, vid: u16, pid: u16) -> Option<(HidDevice, String)> {
    api.refresh_devices().ok()?;
    let info = api
        .device_list()
        .find(|d| d.vendor_id() == vid && d.product_id() == pid)?;
    let name = info
        .product_string()
        .unwrap_or("EdgeTX joystick")
        .to_owned();
    match info.open_device(api) {
        Ok(dev) => Some((dev, name)),
        Err(e) => {
            eprintln!("found {name} but could not open it: {e}");
            None
        }
    }
}

/// The layout the device announces (and its descriptor bytes), or EdgeTX classic if it
/// can't be read or parsed.
fn device_layout(dev: &HidDevice) -> (Layout, Vec<u8>) {
    let mut buf = [0u8; 4096];
    match dev.get_report_descriptor(&mut buf) {
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

fn read_until_error(
    dev: &HidDevice,
    name: &str,
    layout: &Layout,
    mut recorder: Option<&mut Recorder>,
    tx: &watch::Sender<RawState>,
) {
    let classic = edgetx::classic_layout();
    let mut warned = false;
    let mut buf = [0u8; 256];
    loop {
        let n = match dev.read_timeout(&mut buf, READ_TIMEOUT_MS) {
            Ok(0) => continue, // no new report; EdgeTX only sends while the mixer runs
            Ok(n) => n,
            Err(_) => return,
        };
        let bytes = &buf[..n];
        if let Some(r) = recorder.as_deref_mut() {
            r.report(bytes);
        }
        // Prefer the announced layout; Windows can rebuild descriptors slightly
        // differently, so fall back to classic when only that one fits.
        let (report, used) = match layout.decode(bytes) {
            Some(r) => (r, layout),
            None => match classic.decode(bytes) {
                Some(r) => (r, &classic),
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
                layout: describe(used),
                report,
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
