//! The real USB reconnect/read loop (`run_hid_with`) driven by a scripted fake device:
//! unplugging, flaky cables, stalls, garbage, a radio held by another program, two radios
//! plugged in, and report rates far above what the Pocket sends (1000/s at most, from
//! EdgeTX's 1 ms mixer period in joystick mode and the endpoint's 1 ms bInterval).

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use pocket_overlay::edgetx::{self, CHANNELS, USB_PID, USB_VID, encode_classic};
use pocket_overlay::input::{HidBackend, HidPort, HidTiming, RawState, run_hid_with};
use tokio::sync::watch;

// ---------------------------------------------------------------------------------------
// the fake USB bus

enum Event {
    Report(Vec<u8>),
    /// The read times out with nothing (the radio went quiet).
    Silence,
    /// A read error without the device disappearing (a glitchy cable).
    Glitch,
}

struct Device {
    name: String,
    plugged: bool,
    /// Bumped on every unplug so ports opened before it start failing.
    generation: u64,
    descriptor: Result<Vec<u8>, String>,
    /// `open` fails this many more times, as if another program had it locked.
    locked_for: usize,
    queue: VecDeque<Event>,
    reads: usize,
}

#[derive(Default)]
struct Bus {
    devices: Vec<Device>,
    opens: usize,
}

#[derive(Clone, Default)]
struct Usb(Arc<Mutex<Bus>>);

impl Usb {
    fn plug(&self, name: &str) -> usize {
        let mut bus = self.0.lock().unwrap();
        bus.devices.push(Device {
            name: name.into(),
            plugged: true,
            generation: 0,
            descriptor: Ok(edgetx::CLASSIC_DESCRIPTOR.to_vec()),
            locked_for: 0,
            queue: VecDeque::new(),
            reads: 0,
        });
        bus.devices.len() - 1
    }
    fn with<R>(&self, dev: usize, f: impl FnOnce(&mut Device) -> R) -> R {
        f(&mut self.0.lock().unwrap().devices[dev])
    }
    fn unplug(&self, dev: usize) {
        self.with(dev, |d| {
            d.plugged = false;
            d.generation += 1;
            d.queue.clear();
        });
    }
    fn replug(&self, dev: usize) {
        self.with(dev, |d| d.plugged = true);
    }
    fn send(&self, dev: usize, event: Event) {
        self.with(dev, |d| d.queue.push_back(event));
    }
    fn report(&self, dev: usize, ch: &[i16; CHANNELS]) {
        self.send(dev, Event::Report(encode_classic(ch).to_vec()));
    }
}

struct Backend(Usb);

impl HidBackend for Backend {
    fn list(&mut self, vid: u16, pid: u16) -> Vec<(usize, String)> {
        assert_eq!((vid, pid), (USB_VID, USB_PID));
        let bus = self.0.0.lock().unwrap();
        bus.devices
            .iter()
            .enumerate()
            .filter(|(_, d)| d.plugged)
            .map(|(i, d)| (i, d.name.clone()))
            .collect()
    }

    fn open(&mut self, key: usize) -> Result<Box<dyn HidPort>, String> {
        let mut bus = self.0.0.lock().unwrap();
        bus.opens += 1;
        let d = &mut bus.devices[key];
        if d.locked_for > 0 {
            d.locked_for -= 1;
            return Err("Access denied".into());
        }
        Ok(Box::new(Port {
            usb: self.0.clone(),
            key,
            generation: d.generation,
        }))
    }
}

struct Port {
    usb: Usb,
    key: usize,
    generation: u64,
}

impl HidPort for Port {
    fn report_descriptor(&self, buf: &mut [u8]) -> Result<usize, String> {
        let d = self.usb.with(self.key, |d| d.descriptor.clone())?;
        buf[..d.len()].copy_from_slice(&d);
        Ok(d.len())
    }

    fn read_timeout(&self, buf: &mut [u8], timeout_ms: i32) -> Result<usize, String> {
        let deadline = Instant::now() + Duration::from_millis(timeout_ms as u64);
        loop {
            let event = self.usb.with(self.key, |d| {
                if !d.plugged || d.generation != self.generation {
                    return Err("The device is not connected.".to_owned());
                }
                d.reads += 1;
                Ok(d.queue.pop_front())
            })?;
            match event {
                Some(Event::Report(bytes)) => {
                    buf[..bytes.len()].copy_from_slice(&bytes);
                    return Ok(bytes.len());
                }
                Some(Event::Glitch) => return Err("I/O error".into()),
                Some(Event::Silence) => {
                    std::thread::sleep(Duration::from_millis(timeout_ms as u64));
                    return Ok(0);
                }
                None if Instant::now() >= deadline => return Ok(0),
                None => std::thread::sleep(Duration::from_micros(200)),
            }
        }
    }
}

// ---------------------------------------------------------------------------------------
// running the real loop against it

const FAST: HidTiming = HidTiming {
    reconnect: Duration::from_millis(30),
    read_timeout_ms: 20,
};

struct Running {
    rx: watch::Receiver<RawState>,
    thread: Option<JoinHandle<()>>,
}

fn run(usb: &Usb) -> Running {
    let (tx, rx) = watch::channel(RawState::default());
    let usb = usb.clone();
    let thread = std::thread::spawn(move || {
        run_hid_with(&mut Backend(usb), USB_VID, USB_PID, None, FAST, tx);
    });
    Running {
        rx,
        thread: Some(thread),
    }
}

impl Running {
    fn wait(&self, what: &str, pred: impl Fn(&RawState) -> bool) -> RawState {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let s = self.rx.borrow().clone();
            if pred(&s) {
                return s;
            }
            assert!(
                Instant::now() < deadline,
                "timed out waiting for {what}; state: {s:?}"
            );
            std::thread::sleep(Duration::from_micros(300));
        }
    }

    fn showing(&self, ch: &[i16; CHANNELS]) -> RawState {
        let want: Vec<i16> = (1..=CHANNELS)
            .map(|n| {
                let v = ch[n - 1];
                if n <= edgetx::AXES {
                    v.clamp(-1024, 1024)
                } else if v > 0 {
                    1024
                } else {
                    -1024
                }
            })
            .collect();
        self.wait(&format!("channels {:?}", &ch[..10]), |s| {
            s.connected
                && (1..=CHANNELS)
                    .map(|n| s.report.channel(n).unwrap_or(0))
                    .eq(want.iter().copied())
        })
    }

    /// Stops the loop the way the app does on exit: nobody listening any more.
    fn stop(mut self) {
        drop(self.rx);
        let t = self.thread.take().unwrap();
        let deadline = Instant::now() + Duration::from_secs(3);
        while !t.is_finished() {
            assert!(Instant::now() < deadline, "reader didn't stop");
            std::thread::sleep(Duration::from_millis(5));
        }
        t.join().unwrap();
    }
}

fn position(i: i16) -> [i16; CHANNELS] {
    let mut ch = [0i16; CHANNELS];
    ch[0] = (i * 97) % 2049 - 1024;
    ch[1] = -(i * 53 % 1024);
    ch[2] = i % 1024;
    ch[4] = if i % 2 == 0 { 1024 } else { -1024 };
    ch[9] = if i % 3 == 0 { 1024 } else { -1024 };
    ch
}

// ---------------------------------------------------------------------------------------

#[test]
fn reads_what_the_radio_sends() {
    let usb = Usb::default();
    let dev = usb.plug(edgetx::POCKET_PRODUCT);
    let r = run(&usb);
    for i in 0..50 {
        usb.report(dev, &position(i));
        let s = r.showing(&position(i));
        assert_eq!(s.name, edgetx::POCKET_PRODUCT);
        assert_eq!(s.layout, "EdgeTX classic: 8 axes, 24 buttons");
    }
    r.stop();
}

#[test]
fn unplug_and_replug_many_times() {
    let usb = Usb::default();
    let dev = usb.plug(edgetx::POCKET_PRODUCT);
    let r = run(&usb);
    for cycle in 0..25 {
        usb.report(dev, &position(cycle));
        r.showing(&position(cycle));
        usb.unplug(dev);
        // unplugged: nothing stale left on screen
        let s = r.wait("disconnect", |s| !s.connected);
        assert_eq!(s, RawState::default());
        usb.replug(dev);
        // the first report after replugging shows, not the one from before
        usb.report(dev, &position(cycle + 100));
        r.showing(&position(cycle + 100));
    }
    r.stop();
}

#[test]
fn glitchy_cable_recovers() {
    let usb = Usb::default();
    let dev = usb.plug(edgetx::POCKET_PRODUCT);
    let r = run(&usb);
    for i in 0..20 {
        usb.report(dev, &position(i));
        usb.send(dev, Event::Glitch); // read error, device still there
        usb.report(dev, &position(i + 1));
        r.showing(&position(i + 1));
    }
    r.stop();
}

#[test]
fn a_quiet_radio_keeps_its_last_position() {
    let usb = Usb::default();
    let dev = usb.plug(edgetx::POCKET_PRODUCT);
    let r = run(&usb);
    usb.report(dev, &position(7));
    r.showing(&position(7));
    for _ in 0..10 {
        usb.send(dev, Event::Silence);
    }
    std::thread::sleep(Duration::from_millis(300)); // 15 read timeouts
    let s = r.rx.borrow().clone();
    assert!(s.connected, "silence isn't an unplug");
    assert_eq!(s.report.channel(1), Some(position(7)[0]));
    usb.report(dev, &position(8));
    r.showing(&position(8));
    r.stop();
}

#[test]
fn garbage_reports_are_skipped() {
    let usb = Usb::default();
    let dev = usb.plug(edgetx::POCKET_PRODUCT);
    let r = run(&usb);
    usb.report(dev, &position(1));
    r.showing(&position(1));
    for junk in [vec![], vec![1, 2, 3], vec![0xff; 18], vec![0x55; 40]] {
        usb.send(dev, Event::Report(junk));
    }
    usb.report(dev, &position(2));
    let s = r.showing(&position(2));
    assert!(s.connected);
    r.stop();
}

#[test]
fn waits_while_another_program_has_it_locked() {
    let usb = Usb::default();
    let dev = usb.plug(edgetx::POCKET_PRODUCT);
    usb.with(dev, |d| d.locked_for = 5);
    let r = run(&usb);
    usb.report(dev, &position(3));
    r.showing(&position(3));
    assert!(
        usb.0.lock().unwrap().opens >= 6,
        "kept retrying until it could open"
    );
    r.stop();
}

#[test]
fn unreadable_descriptor_falls_back_to_the_pocket_layout() {
    let usb = Usb::default();
    let dev = usb.plug(edgetx::POCKET_PRODUCT);
    usb.with(dev, |d| d.descriptor = Err("not supported".into()));
    let r = run(&usb);
    usb.report(dev, &position(4));
    let s = r.showing(&position(4));
    assert_eq!(s.layout, "EdgeTX classic: 8 axes, 24 buttons");
    r.stop();
}

#[test]
fn prefers_the_pocket_when_two_radios_are_plugged_in() {
    let usb = Usb::default();
    let other = usb.plug("Radiomaster TX16S Joystick");
    let pocket = usb.plug(edgetx::POCKET_PRODUCT);
    let r = run(&usb);
    usb.report(other, &position(10));
    usb.report(pocket, &position(11));
    let s = r.showing(&position(11));
    assert_eq!(s.name, edgetx::POCKET_PRODUCT);
    r.stop();
}

#[test]
fn keeps_up_far_beyond_the_radios_report_rate() {
    const N: i16 = 20_000;
    let usb = Usb::default();
    let dev = usb.plug(edgetx::POCKET_PRODUCT);
    let r = run(&usb);
    usb.report(dev, &position(0));
    r.showing(&position(0));

    // queue a burst as if the radio had sent it, then time how long it takes to drain
    let mut last = [0i16; CHANNELS];
    for i in 1..=N {
        last = [0; CHANNELS];
        last[0] = (i % 2048) - 1024;
        last[7] = i % 1000;
        usb.report(dev, &last);
    }
    let start = Instant::now();
    r.showing(&last);
    let rate = f64::from(N) / start.elapsed().as_secs_f64();
    eprintln!("decoded and published {rate:.0} reports/s");
    assert!(
        rate > 5_000.0,
        "only {rate:.0} reports/s; the radio can send 1000/s"
    );
    r.stop();
}

#[test]
fn newer_reports_are_never_overtaken_by_older_ones() {
    // A 1000 Hz ramp: anyone watching must only ever see it move forward.
    let usb = Usb::default();
    let dev = usb.plug(edgetx::POCKET_PRODUCT);
    let r = run(&usb);
    let feeder = {
        let usb = usb.clone();
        std::thread::spawn(move || {
            for v in -1024..=1024i16 {
                let mut ch = [0i16; CHANNELS];
                ch[0] = v;
                usb.report(dev, &ch);
                if v % 8 == 0 {
                    std::thread::sleep(Duration::from_millis(8)); // ~1000 reports/s
                }
            }
        })
    };
    let mut seen = Vec::new();
    let mut rx = r.rx.clone();
    let deadline = Instant::now() + Duration::from_secs(10);
    while seen.last() != Some(&1024) {
        assert!(
            Instant::now() < deadline,
            "ramp didn't finish: {:?}",
            seen.last()
        );
        if rx.has_changed().unwrap_or(false) {
            let s = rx.borrow_and_update().clone();
            if s.connected {
                seen.push(s.report.channel(1).unwrap());
            }
        } else {
            std::thread::sleep(Duration::from_micros(200));
        }
    }
    feeder.join().unwrap();
    assert!(
        seen.windows(2).all(|w| w[0] <= w[1]),
        "went backwards: {seen:?}"
    );
    assert!(seen.len() > 50, "only {} updates seen", seen.len());
    drop(rx);
    r.stop();
}

#[test]
fn stops_when_nobody_is_listening() {
    let usb = Usb::default();
    let dev = usb.plug(edgetx::POCKET_PRODUCT);
    let r = run(&usb);
    usb.report(dev, &position(1));
    r.showing(&position(1));
    r.stop(); // panics if the loop keeps running
}

/// On macOS, hidapi opens devices exclusively unless told otherwise, which would take the
/// radio away from a game or sim. Make sure the app shares it.
#[cfg(target_os = "macos")]
#[test]
fn macos_opens_the_radio_shared() {
    let backend = pocket_overlay::input::Hidapi::new().expect("hidapi");
    assert!(!backend.opens_exclusively());
}
