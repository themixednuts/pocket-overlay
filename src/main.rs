use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use tokio::sync::{mpsc, watch};

use pocket_overlay::config::Config;
use pocket_overlay::detect::{ChannelKind, Detector};
use pocket_overlay::engine::Engine;
use pocket_overlay::input::{self, RawState};
use pocket_overlay::server;

const USAGE: &str = "\
pocket-overlay - OBS overlay for the RadioMaster Pocket (EdgeTX USB joystick)

USAGE: pocket-overlay [options]

Run it with no options, then add http://127.0.0.1:7878/ to OBS as a Browser Source.

  --demo            fake radio input, to set up the OBS scene without the radio
  --monitor         print raw channel values in the terminal instead of serving
  --record <file>   save every report from the radio to a file
  --replay <file>   play a recording back (`-` reads report lines from stdin)
  --config <file>   settings file (default: your user config folder)
  --port <n>        HTTP port (default 7878; 0 picks a free one)
";

enum Input {
    Hid { record: Option<PathBuf> },
    Demo,
    Replay(PathBuf),
}

struct Args {
    input: Input,
    monitor: bool,
    config: PathBuf,
    port: Option<u16>,
}

fn parse_args() -> Result<Args> {
    let mut args = Args {
        input: Input::Hid { record: None },
        monitor: false,
        config: Config::default_path(),
        port: None,
    };
    let mut record = None;
    let mut it = std::env::args().skip(1);
    let value = |flag: &str, it: &mut dyn Iterator<Item = String>| {
        it.next().with_context(|| format!("{flag} needs a value"))
    };
    while let Some(a) = it.next() {
        match a.as_str() {
            "--demo" => args.input = Input::Demo,
            "--replay" => args.input = Input::Replay(value("--replay", &mut it)?.into()),
            "--record" => record = Some(PathBuf::from(value("--record", &mut it)?)),
            "--monitor" => args.monitor = true,
            "--config" => args.config = value("--config", &mut it)?.into(),
            "--port" => args.port = Some(value("--port", &mut it)?.parse().context("--port")?),
            "-h" | "--help" => {
                print!("{USAGE}");
                std::process::exit(0);
            }
            other => bail!("unknown argument {other:?}\n\n{USAGE}"),
        }
    }
    if let Some(path) = record {
        match args.input {
            Input::Hid { .. } => args.input = Input::Hid { record: Some(path) },
            _ => bail!("--record only works with the real radio"),
        }
    }
    Ok(args)
}

fn main() -> std::process::ExitCode {
    let runtime = tokio::runtime::Runtime::new().expect("start async runtime");
    match runtime.block_on(run()) {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("error: {e:#}");
            pause_if_double_clicked();
            std::process::ExitCode::FAILURE
        }
    }
}

/// When the .exe was double-clicked, its console window closes as soon as it exits;
/// keep it open so the error can be read.
#[cfg(windows)]
fn pause_if_double_clicked() {
    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn GetConsoleProcessList(list: *mut u32, count: u32) -> u32;
    }
    let mut ids = [0u32; 4];
    // SAFETY: `ids` is valid for writes of `ids.len()` entries.
    let attached = unsafe { GetConsoleProcessList(ids.as_mut_ptr(), ids.len() as u32) };
    // Only this process on the console: it was opened for us, not by a terminal.
    if attached == 1 {
        eprintln!("Press Enter to close.");
        let _ = std::io::stdin().read_line(&mut String::new());
    }
}

#[cfg(not(windows))]
fn pause_if_double_clicked() {}

async fn run() -> Result<()> {
    let args = parse_args()?;
    let cfg = Config::load_or_create(&args.config)?;
    let (raw_tx, raw_rx) = watch::channel(RawState::default());

    if args.monitor {
        start_input(args.input, &cfg, raw_tx);
        return monitor(raw_rx).await;
    }

    let port = args.port.unwrap_or(cfg.port);
    let (listener, addr) = match server::bind(port).await {
        Ok(bound) => bound,
        Err(e) if e.kind() == std::io::ErrorKind::AddrInUse => bail!(
            "port {port} is already in use. Is pocket-overlay already running? \
             Close it, or start this one with --port <another number>."
        ),
        Err(e) => return Err(e).context("starting the web server"),
    };
    // The black-box tests read the port from the first URL printed here.
    eprintln!("Pocket overlay is running.");
    eprintln!("  OBS Browser Source:  http://{addr}/   (680 x 830, transparent)");
    eprintln!("  Setup:               http://{addr}/?setup=1");
    eprintln!("  Settings file:       {}", args.config.display());

    let engine = Engine::new(cfg.clone(), args.config.clone());
    let (state_tx, state_rx) = watch::channel(engine.state());
    let (cmd_tx, cmd_rx) = mpsc::channel(16);
    tokio::spawn(engine.run(raw_rx, cmd_rx, state_tx));
    start_input(args.input, &cfg, raw_tx);

    let skins = pocket_overlay::skins::Skins::beside(&args.config);
    server::serve(listener, state_rx, cmd_tx, skins).await?;
    Ok(())
}

fn start_input(source: Input, cfg: &Config, tx: watch::Sender<RawState>) {
    match source {
        Input::Demo => std::thread::spawn(move || input::run_demo(tx)),
        Input::Replay(path) => std::thread::spawn(move || input::run_replay(path, tx)),
        Input::Hid { record } => {
            let (vid, pid) = (cfg.usb_vid, cfg.usb_pid);
            std::thread::spawn(move || input::run_hid(vid, pid, record, tx))
        }
    };
}

/// Carriage return + erase line, so the monitor output updates in place.
const CLEAR_LINE: &str = "\r\x1b[2K";

async fn monitor(mut rx: watch::Receiver<RawState>) -> Result<()> {
    eprintln!(
        "monitoring raw channels (EdgeTX units, -1024..1024; ~ analog, 2/3 switch), Ctrl+C to quit"
    );
    let mut detector = Detector::default();
    while rx.changed().await.is_ok() {
        let s = rx.borrow_and_update().clone();
        if !s.connected {
            eprint!("{CLEAR_LINE}no radio connected");
            continue;
        }
        detector.observe(&s.report);
        let kinds = detector.kinds();
        let axes: Vec<String> = s
            .report
            .axes
            .iter()
            .enumerate()
            .map(|(i, v)| format!("CH{}:{v:+5}{}", i + 1, kind_mark(kinds[i])))
            .collect();
        let offset = s.report.axes.len();
        let on: Vec<String> = s
            .report
            .buttons
            .iter()
            .enumerate()
            .filter(|(_, on)| **on)
            .map(|(b, _)| format!("CH{}", b + offset + 1))
            .collect();
        eprint!("{CLEAR_LINE}{}  on:[{}]", axes.join(" "), on.join(" "));
    }
    Ok(())
}

/// One-character tag for what a channel looks like.
fn kind_mark(kind: ChannelKind) -> &'static str {
    match kind {
        ChannelKind::Idle => " ",
        ChannelKind::Analog => "~",
        ChannelKind::Switch2 => "2",
        ChannelKind::Switch3 => "3",
    }
}
