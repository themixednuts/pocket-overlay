//! Black-box rendering tests: the real binary serves the real page to headless Chrome/Edge,
//! a virtual radio (EdgeTX's encoder -> report bytes on stdin) sets physical positions,
//! and we measure the rendered SVG in screen space: where the knobs and gimbal slots are,
//! which way the paddles lean, what's lit, what the text says.
//!
//! Skips (and passes) when no Chromium-based browser is found; set `CHROME` to point at one.

mod common;

use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use common::{Overlay, Radio, Rng, channel_key, expected_channels};
use headless_chrome::protocol::cdp::Emulation;
use headless_chrome::{Browser, LaunchOptions, Tab};
use serde_json::Value;

const ACCENT: &str = "rgb(255, 0, 255)";

fn browser_path() -> Option<PathBuf> {
    let candidates = std::env::var_os("CHROME")
        .map(PathBuf::from)
        .into_iter()
        .chain(
            [
                r"C:\Program Files\Google\Chrome\Application\chrome.exe",
                r"C:\Program Files (x86)\Microsoft\Edge\Application\msedge.exe",
                r"C:\Program Files\Microsoft\Edge\Application\msedge.exe",
                "/usr/bin/google-chrome",
                "/usr/bin/chromium",
                "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome",
            ]
            .map(PathBuf::from),
        );
    let found = candidates.into_iter().find(|p| p.exists());
    if found.is_none() {
        eprintln!("SKIPPED render tests: no Chrome/Edge found (set CHROME)");
    }
    found
}

struct Page {
    _browser: Browser,
    tab: Arc<Tab>,
}

/// Measures everything the tests look at, in CSS pixels, as one JSON string.
const MEASURE: &str = r#"(() => {
  const q = s => document.querySelector(`[data-test="${s}"]`);
  const box = s => { const e = q(s); if (!e) return null; const b = e.getBoundingClientRect();
    return { x: b.x, y: b.y, w: b.width, h: b.height, cx: b.x + b.width / 2, cy: b.y + b.height / 2 }; };
  const fill = e => e ? getComputedStyle(e).fill : null;
  const o = {};
  for (const k of ["knob-L","knob-R","travel-L","travel-R","slot-L","slot-R","tickx-L","ticky-L","tickx-R","ticky-R",
                   "sa-side","sd-side","sb-nub","sc-nub","s1-ribs","se-pad"])
    o[k] = box(k);
  o.fill = {};
  for (const k of ["sa-front","sd-front","sb-nub","sc-nub","se-pad"]) o.fill[k] = fill(q(k));
  o.fill["sa-side"] = fill(q("sa-side").firstElementChild);
  o.fill["sd-side"] = fill(q("sd-side").firstElementChild);
  o.opacity = { "s1-ribs": +getComputedStyle(q("s1-ribs")).opacity, front: +getComputedStyle(document.getElementById("front")).opacity };
  o.text = {};
  for (const k of ["tag-sa","tag-sb","tag-sc","tag-sd","tag-se","tag-s1","readout"]) o.text[k] = q(k) ? q(k).textContent : null;
  o.lcd = [...Array(8)].map((_, i) => ({ bar: box(`lcd-bar-${i + 1}`), zero: box(`lcd-zero-${i + 1}`) }));
  o.ch = [...Array(8)].map((_, i) => ({ bar: box(`ch-bar-${i + 1}`), bg: box(`ch-bg-${i + 1}`), val: q(`ch-val-${i + 1}`).textContent }));
  o.btn = [...Array(24)].map((_, i) => fill(q(`btn-${i + 9}`)));
  const root = document.getElementById("root");
  o.offline = root.classList.contains("offline");
  o.learnTarget = root.dataset.learnTarget || null;
  const f = document.getElementById("focus");
  o.focus = f.style.display === "block" ? box("focus-ring") : null;
  return JSON.stringify(o);
})()"#;

impl Page {
    fn open(ov: &Overlay, query: &str) -> Option<Self> {
        Self::open_with(ov, query, None)
    }

    /// Opens the page in a viewport of exactly this size, like an OBS browser source.
    fn open_sized(ov: &Overlay, query: &str, (width, height): (u32, u32)) -> Option<Self> {
        Self::open_with(ov, query, Some((width, height)))
    }

    fn open_with(ov: &Overlay, query: &str, viewport: Option<(u32, u32)>) -> Option<Self> {
        let path = browser_path()?;
        let opts = LaunchOptions::default_builder()
            .path(Some(path))
            .headless(true)
            // CI runners (Ubuntu 24.04) block the sandbox's user namespaces; it's a test browser
            .sandbox(false)
            .window_size(Some((1400, 1000)))
            .idle_browser_timeout(Duration::from_secs(120))
            .build()
            .unwrap();
        // with every render test starting browsers at once, one sometimes times out starting
        let mut tries = 0;
        let (browser, tab) = loop {
            tries += 1;
            match Browser::new(opts.clone()).and_then(|b| b.new_tab().map(|t| (b, t))) {
                Ok(started) => break started,
                Err(e) if tries < 3 => eprintln!("browser didn't start ({e}); trying again"),
                Err(e) => panic!("browser didn't start: {e}"),
            }
        };
        if let Some((width, height)) = viewport {
            // the window size includes the browser's own frame; this is the page's
            tab.call_method(Emulation::SetDeviceMetricsOverride {
                width,
                height,
                device_scale_factor: 1.0,
                mobile: false,
                scale: None,
                screen_width: None,
                screen_height: None,
                position_x: None,
                position_y: None,
                dont_set_visible_size: None,
                screen_orientation: None,
                viewport: None,
                display_feature: None,
                device_posture: None,
            })
            .unwrap();
        }
        tab.navigate_to(&ov.url(query))
            .unwrap()
            .wait_until_navigated()
            .unwrap();
        Some(Page {
            _browser: browser,
            tab,
        })
    }

    fn eval(&self, js: &str) -> Value {
        let r = self
            .tab
            .evaluate(js, false)
            .unwrap_or_else(|e| panic!("evaluate failed: {e}"));
        r.value.unwrap_or(Value::Null)
    }

    /// Waits until the page has drawn a frame showing exactly these channels.
    fn wait_for_channels(&self, ch: &[i16]) {
        let key = channel_key(ch);
        let deadline = Instant::now() + Duration::from_secs(8);
        loop {
            let shown = self.eval("document.getElementById('root').dataset.channels || ''");
            if shown.as_str().is_some_and(|s| s.starts_with(&key)) {
                return;
            }
            assert!(
                Instant::now() < deadline,
                "page never showed {key}; showing {shown}"
            );
            std::thread::sleep(Duration::from_millis(15));
        }
    }

    fn wait_until(&self, what: &str, js: &str) {
        let deadline = Instant::now() + Duration::from_secs(8);
        while self.eval(js) != Value::Bool(true) {
            assert!(Instant::now() < deadline, "timed out waiting for {what}");
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    fn measure(&self) -> Value {
        serde_json::from_str(self.eval(MEASURE).as_str().expect("measure returns JSON")).unwrap()
    }

    fn screenshot(&self, name: &str) {
        let png = self
            .tab
            .capture_screenshot(
                headless_chrome::protocol::cdp::Page::CaptureScreenshotFormatOption::Png,
                None,
                None,
                true,
            )
            .unwrap();
        let path = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(format!("render-{name}.png"));
        std::fs::write(&path, png).unwrap();
        eprintln!("screenshot: {}", path.display());
    }
}

/// Shows `radio` on the overlay and returns the page's measurements once it's drawn.
fn show(ov: &mut Overlay, page: &Page, radio: &Radio) -> Value {
    ov.radio(radio);
    page.wait_for_channels(&expected_channels(&ov.wiring.channels(radio)));
    page.measure()
}

fn num(v: &Value) -> f64 {
    v.as_f64().unwrap_or_else(|| panic!("not a number: {v}"))
}

fn approx(got: f64, want: f64, tol: f64, what: &str, m: &Value) {
    assert!(
        (got - want).abs() <= tol,
        "{what}: got {got:.4}, want {want:.4} (±{tol})\nmeasured: {m}"
    );
}

/// JavaScript's Math.round (halves round up, not away from zero).
fn js_round(x: f64) -> i64 {
    (x + 0.5).floor() as i64
}

/// The page's signed percentage text: "+42", "0", "-7".
fn pct(v: f32) -> String {
    let n = js_round(f64::from(v) * 100.0);
    if n > 0 {
        format!("+{n}")
    } else {
        n.to_string()
    }
}

// ---------------------------------------------------------------------------------------

#[test]
fn sticks_render_where_they_are_pushed() {
    let mut ov = Overlay::start();
    let Some(page) = Page::open(&ov, "?trail=0&accent=%23ff00ff") else {
        return;
    };
    eprintln!("encoder: {}", ov.encoder_name());

    let rest = show(&mut ov, &page, &Radio::default());
    page.screenshot("neutral");
    // slot height with the drum facing the camera
    let rest_h = |side: &str| num(&rest[format!("slot-{side}").as_str()]["h"]);
    let mut ratios: Vec<f64> = Vec::new();

    let mut cases = Vec::new();
    for (x, y) in [
        (1.0, 0.0),
        (-1.0, 0.0),
        (0.0, 1.0),
        (0.0, -1.0),
        (1.0, 1.0),
        (-1.0, -1.0),
        (0.5, -0.25),
    ] {
        cases.push(Radio {
            left_x: x,
            left_y: y,
            right_x: -y,
            right_y: x,
            ..Radio::default()
        });
    }
    let mut rng = Rng(0xD1CE);
    cases.extend((0..50).map(|_| rng.radio()));

    for r in &cases {
        let m = show(&mut ov, &page, r);
        for (side, sx, sy) in [("L", r.left_x, r.left_y), ("R", r.right_x, r.right_y)] {
            let (sx, sy) = (f64::from(sx), f64::from(sy));
            let travel = &m[format!("travel-{side}").as_str()];
            let knob = &m[format!("knob-{side}").as_str()];
            // knob inside the travel box: left edge = -1, right = +1, top = +1 (up), bottom = -1
            let nx = (num(&knob["cx"]) - num(&travel["x"])) / num(&travel["w"]) * 2.0 - 1.0;
            let ny = 1.0 - (num(&knob["cy"]) - num(&travel["y"])) / num(&travel["h"]) * 2.0;
            approx(nx, sx, 0.01, &format!("{side} stick x"), &m);
            approx(ny, sy, 0.01, &format!("{side} stick y"), &m);
            // value ticks line up with the knob
            approx(
                num(&m[format!("tickx-{side}").as_str()]["cx"]),
                num(&knob["cx"]),
                0.6,
                "x tick",
                &m,
            );
            approx(
                num(&m[format!("ticky-{side}").as_str()]["cy"]),
                num(&knob["cy"]),
                0.6,
                "y tick",
                &m,
            );
            // Like the real gimbal, which tilts: the drum's slot rolls the same way as the knob
            // but less (the stick leans through it), never sideways, and gets thinner as the
            // drum turns away from the camera.
            let slot = &m[format!("slot-{side}").as_str()];
            approx(
                num(&slot["cx"]),
                num(&travel["cx"]),
                0.6,
                "slot never moves sideways",
                &m,
            );
            let knob_dy = num(&knob["cy"]) - num(&travel["cy"]);
            let slot_dy = num(&slot["cy"]) - num(&travel["cy"]);
            if sy.abs() > 0.2 {
                let ratio = slot_dy / knob_dy;
                assert!(
                    ratio > 0.2 && ratio < 0.9,
                    "{side} slot moved {slot_dy:.1}px for a {knob_dy:.1}px knob move\n{m}"
                );
                ratios.push(ratio);
            }
            let h = num(&slot["h"]);
            assert!(h <= rest_h(side) + 0.3, "{side} slot taller than at rest");
            if sy.abs() >= 0.5 {
                assert!(
                    h < rest_h(side) - 0.5,
                    "{side} slot not foreshortened at y {sy}: {h:.2} vs {:.2}\n{m}",
                    rest_h(side)
                );
            }
        }
        let thr = js_round((f64::from(r.left_y) + 1.0) * 50.0);
        let want = format!(
            "THR {thr}%  RUD {}  ELE {}  AIL {}",
            pct(r.left_x),
            pct(r.right_y),
            pct(r.right_x)
        );
        assert_eq!(m["text"]["readout"], want.as_str());
    }
    show(
        &mut ov,
        &page,
        &Radio {
            left_x: 0.7,
            left_y: 0.9,
            right_x: -0.6,
            right_y: -0.8,
            sa: 1,
            sb: 2,
            se: true,
            s1: 0.5,
            ..Radio::default()
        },
    );
    page.screenshot("deflected");
    // the slot's share of the knob's movement is the same everywhere: one rigid tilt
    let first = ratios[0];
    assert!(
        ratios.iter().all(|r| (r - first).abs() < 0.03),
        "slot/knob ratios vary: {ratios:?}"
    );
}

#[test]
fn switches_render_their_positions() {
    let mut ov = Overlay::start();
    // with the optional side views on, so the paddles' lean can be measured too
    let Some(page) = Page::open(&ov, "?sides=1&trail=0&accent=%23ff00ff") else {
        return;
    };
    let arrows2 = ["↑", "↓"];
    let arrows3 = ["↑", "–", "↓"];
    let mut paddle_x = std::collections::HashMap::new();
    let mut nub_h = std::collections::HashMap::new();

    for sa in 0..2u8 {
        for sb in 0..3u8 {
            for sc in 0..3u8 {
                for sd in 0..2u8 {
                    for se in [false, true] {
                        let r = Radio {
                            sa,
                            sb,
                            sc,
                            sd,
                            se,
                            ..Radio::default()
                        };
                        let m = show(&mut ov, &page, &r);
                        let lit = |k: &str| m["fill"][k] == ACCENT;
                        assert_eq!(lit("sa-front"), sa == 1, "SA front lit\n{m}");
                        assert_eq!(lit("sa-side"), sa == 1, "SA side lit");
                        assert_eq!(lit("sd-front"), sd == 1, "SD front lit");
                        assert_eq!(lit("sd-side"), sd == 1, "SD side lit");
                        assert_eq!(lit("sb-nub"), sb != 1, "SB lit off-centre");
                        assert_eq!(lit("sc-nub"), sc != 1, "SC lit off-centre");
                        assert_eq!(lit("se-pad"), se, "SE lit while pressed");
                        assert_eq!(
                            m["text"]["tag-sa"],
                            format!("SA {}", arrows2[sa as usize]).as_str()
                        );
                        assert_eq!(
                            m["text"]["tag-sd"],
                            format!("SD {}", arrows2[sd as usize]).as_str()
                        );
                        assert_eq!(
                            m["text"]["tag-sb"],
                            format!("SB {}", arrows3[sb as usize]).as_str()
                        );
                        assert_eq!(
                            m["text"]["tag-sc"],
                            format!("SC {}", arrows3[sc as usize]).as_str()
                        );
                        assert_eq!(m["text"]["tag-se"], if se { "SE ●" } else { "SE ○" });
                        paddle_x.insert(("sa", sa), num(&m["sa-side"]["cx"]));
                        paddle_x.insert(("sd", sd), num(&m["sd-side"]["cx"]));
                        nub_h.insert(("sb", sb), num(&m["sb-nub"]["h"]));
                        nub_h.insert(("sc", sc), num(&m["sc-nub"]["h"]));
                    }
                }
            }
        }
    }
    // Toward the pilot = toward the radio's face, which in both side views is the side
    // facing the front view in the middle.
    assert!(
        paddle_x[&("sa", 1)] > paddle_x[&("sa", 0)] + 2.0,
        "SA leans toward the face when down"
    );
    assert!(
        paddle_x[&("sd", 1)] < paddle_x[&("sd", 0)] - 2.0,
        "SD (mirrored view) leans toward the face"
    );
    // nubs look taller the more they point at the camera (toward the pilot)
    for n in ["sb", "sc"] {
        assert!(
            nub_h[&(n, 0)] < nub_h[&(n, 1)] && nub_h[&(n, 1)] < nub_h[&(n, 2)],
            "{n} heights {nub_h:?}"
        );
    }
}

#[test]
fn pot_channel_bars_and_buttons() {
    let mut ov = Overlay::start();
    let Some(page) = Page::open(&ov, "?trail=0&accent=%23ff00ff") else {
        return;
    };

    // S1 wheel: ribs roll linearly with the value
    let rib = |m: &Value| (num(&m["s1-ribs"]["cx"]), num(&m["s1-ribs"]["cy"]));
    let c0 = rib(&show(&mut ov, &page, &Radio::default()));
    let c1 = rib(&show(
        &mut ov,
        &page,
        &Radio {
            s1: 1.0,
            ..Radio::default()
        },
    ));
    for v in [-1.0f32, -0.5, 0.25, 0.75] {
        let m = show(
            &mut ov,
            &page,
            &Radio {
                s1: v,
                ..Radio::default()
            },
        );
        let c = rib(&m);
        let v = f64::from(v);
        approx(c.0 - c0.0, v * (c1.0 - c0.0), 0.5, "S1 ribs x", &m);
        approx(c.1 - c0.1, v * (c1.1 - c0.1), 0.5, "S1 ribs y", &m);
        assert_eq!(
            m["text"]["tag-s1"],
            format!("S1 {}", pct(v as f32)).as_str()
        );
    }

    // raw channels: bars, LCD, text and CH9-32 indicators
    let mut rng = Rng(77);
    for _ in 0..25 {
        let mut ch = [0i16; 32];
        for c in &mut ch {
            *c = rng.below(2049) as i16 - 1024;
        }
        ov.channels(&ch);
        let want = expected_channels(&ch);
        page.wait_for_channels(&want);
        let m = page.measure();
        let mut lcd_scale = None;
        for i in 0..8 {
            let v = f64::from(want[i]);
            let e = &m["ch"][i];
            let (bar, bg) = (&e["bar"], &e["bg"]);
            approx(
                num(&bar["w"]) / (num(&bg["w"]) / 2.0),
                v.abs() / 1024.0,
                0.005,
                &format!("CH{} bar length", i + 1),
                &m,
            );
            if v.abs() > 20.0 {
                assert_eq!(
                    num(&bar["cx"]) > num(&bg["cx"]),
                    v > 0.0,
                    "CH{} bar side",
                    i + 1
                );
            }
            assert_eq!(
                e["val"],
                format!("{:.1}%", v / 10.24).as_str(),
                "CH{} text",
                i + 1
            );

            let (lbar, zero) = (&m["lcd"][i]["bar"], &m["lcd"][i]["zero"]);
            if v.abs() > 40.0 {
                assert_eq!(
                    num(&lbar["cx"]) > num(&zero["cx"]),
                    v > 0.0,
                    "LCD CH{} side",
                    i + 1
                );
                let scale = num(&lbar["w"]) / v.abs();
                let s = *lcd_scale.get_or_insert(scale);
                approx(scale, s, s * 0.03, "LCD bars share one scale", &m);
            }
        }
        for b in 0..24 {
            assert_eq!(
                m["btn"][b] == ACCENT,
                want[8 + b] > 0,
                "CH{} indicator",
                b + 9
            );
        }
    }
}

#[test]
fn small_deflections_are_drawn_not_deadzoned() {
    let mut ov = Overlay::start();
    let Some(page) = Page::open(&ov, "?trail=0") else {
        return;
    };
    // 1% to 5% of travel, the region a deadzone would swallow
    for v in [10i16, -10, 20, -31, 41, -51] {
        let f = f32::from(v) / 1024.0;
        let r = Radio {
            left_x: f,
            left_y: -f,
            right_x: -f,
            right_y: f,
            ..Radio::default()
        };
        let m = show(&mut ov, &page, &r);
        for (side, sx, sy) in [("L", r.left_x, r.left_y), ("R", r.right_x, r.right_y)] {
            let travel = &m[format!("travel-{side}").as_str()];
            let knob = &m[format!("knob-{side}").as_str()];
            let nx = (num(&knob["cx"]) - num(&travel["x"])) / num(&travel["w"]) * 2.0 - 1.0;
            let ny = 1.0 - (num(&knob["cy"]) - num(&travel["y"])) / num(&travel["h"]) * 2.0;
            approx(nx, f64::from(sx), 0.002, &format!("{side} x at {v}"), &m);
            approx(ny, f64::from(sy), 0.002, &format!("{side} y at {v}"), &m);
        }
    }
}

#[test]
fn readout_follows_stick_mode() {
    let cfg = pocket_overlay::config::Config {
        mode: 1,
        ..Default::default()
    };
    let text = toml::to_string(&cfg).unwrap();
    let mut ov = Overlay::start_with(Some(&text));
    let Some(page) = Page::open(&ov, "?trail=0") else {
        return;
    };
    // mode 1: throttle and aileron on the right stick, elevator and rudder on the left
    let r = Radio {
        left_x: 0.25,
        left_y: -0.3,
        right_x: -0.75,
        right_y: 0.5,
        ..Radio::default()
    };
    let m = show(&mut ov, &page, &r);
    assert_eq!(m["text"]["readout"], "THR 75%  RUD +25  ELE -30  AIL -75");
}

#[test]
fn disconnect_dims_the_radio() {
    let mut ov = Overlay::start();
    let Some(page) = Page::open(&ov, "?trail=0&accent=%23ff00ff") else {
        return;
    };
    let m = show(
        &mut ov,
        &page,
        &Radio {
            se: true,
            ..Radio::default()
        },
    );
    assert!(!m["offline"].as_bool().unwrap());
    assert_eq!(num(&m["opacity"]["front"]), 1.0);
    assert_eq!(m["btn"][1], ACCENT, "SE's channel (CH10) lit while pressed");
    ov.line("disconnect");
    page.wait_until(
        "offline",
        "document.getElementById('root').classList.contains('offline')",
    );
    let m = page.measure();
    approx(num(&m["opacity"]["front"]), 0.35, 0.01, "dimmed", &m);
    assert_eq!(
        page.eval("document.querySelector('#lcd text').textContent"),
        "NO USB SIGNAL"
    );
    // labels go blank instead of showing positions nobody set
    assert_eq!(m["text"]["readout"], "NO SIGNAL");
    for k in ["tag-sa", "tag-sb", "tag-sc", "tag-sd", "tag-se", "tag-s1"] {
        let name = k.trim_start_matches("tag-").to_uppercase();
        assert_eq!(m["text"][k], format!("{name} ").as_str(), "{k} blank");
    }
    // nothing may look pressed or thrown while there's no signal
    for b in 0..24 {
        assert_ne!(
            m["btn"][b],
            ACCENT,
            "CH{} indicator lit while disconnected",
            b + 9
        );
    }
    for k in ["sa-front", "sd-front", "se-pad"] {
        assert_ne!(m["fill"][k], ACCENT, "{k} lit while disconnected");
    }
}

#[test]
fn setup_page_runs_the_wizard() {
    let mut ov = Overlay::start();
    let Some(page) = Page::open(&ov, "?setup=1&trail=0") else {
        return;
    };
    show(
        &mut ov,
        &page,
        &Radio {
            left_y: -1.0,
            ..Radio::default()
        },
    );

    page.eval("document.getElementById('bStart').click()");
    page.wait_until(
        "wizard start",
        "document.getElementById('root').dataset.learnTarget === 'left_y'",
    );
    let m = page.measure();
    // the highlight ring surrounds the left gimbal
    let (ring, travel) = (&m["focus"], &m["travel-L"]);
    approx(
        num(&ring["cx"]),
        num(&travel["cx"]),
        2.0,
        "focus ring x",
        &m,
    );
    approx(
        num(&ring["cy"]),
        num(&travel["cy"]),
        2.0,
        "focus ring y",
        &m,
    );
    page.screenshot("setup");

    // do what it says: push the left stick up and hold
    show(
        &mut ov,
        &page,
        &Radio {
            left_y: 1.0,
            ..Radio::default()
        },
    );
    page.wait_until(
        "next step",
        "document.getElementById('root').dataset.learnTarget === 'left_x'",
    );
    assert!(
        page.eval("document.getElementById('found').textContent")
            .as_str()
            .unwrap()
            .contains("Left ↕: CH3")
    );

    // skip through the rest with the keyboard shortcut
    for _ in 0..9 {
        page.eval("dispatchEvent(new KeyboardEvent('keydown', { key: 's' }))");
        std::thread::sleep(Duration::from_millis(60));
    }
    page.wait_until(
        "done",
        "document.getElementById('bStart').textContent === 'Detect again'",
    );
    let prompt = page.eval("document.getElementById('prompt').textContent");
    assert!(prompt.as_str().unwrap().starts_with("Saved to"), "{prompt}");
    assert!(
        ov.config_text().contains("[sticks.left_y]\nch = 3"),
        "{}",
        ov.config_text()
    );

    // stick mode is chosen on the page too, no file editing
    page.eval(
        "const s = document.getElementById('modeSel'); s.value = '1'; s.dispatchEvent(new Event('change'))",
    );
    let deadline = Instant::now() + Duration::from_secs(5);
    while !ov.config_text().contains("mode = 1") {
        assert!(
            Instant::now() < deadline,
            "mode not saved: {}",
            ov.config_text()
        );
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// Not a check: saves screenshots of named scenarios to target/tmp/gallery-*.png, for
/// looking at. Run with `cargo test --test blackbox_render gallery -- --ignored`.
#[test]
#[ignore]
fn gallery() {
    let mut ov = Overlay::start();
    ov.line("name Radiomaster Pocket Joystick");
    let Some(page) = Page::open(&ov, "?trail=0") else {
        return;
    };
    // the overlay is transparent; shoot it over a dark scene like OBS would show it
    page.eval("document.body.style.background = '#1b1e22'");
    let scenes = [
        ("1-neutral", Radio::default()),
        (
            "2-up-right",
            Radio {
                left_x: 1.0,
                left_y: 1.0,
                right_x: 1.0,
                right_y: 1.0,
                sa: 1,
                sb: 2,
                sc: 2,
                sd: 1,
                se: true,
                s1: 1.0,
            },
        ),
        (
            "3-down-left",
            Radio {
                left_x: -1.0,
                left_y: -1.0,
                right_x: -1.0,
                right_y: -1.0,
                s1: -1.0,
                ..Radio::default()
            },
        ),
        (
            "4-mixed",
            Radio {
                left_x: 0.3,
                left_y: -0.6,
                right_x: -0.7,
                right_y: 0.4,
                sb: 1,
                sd: 1,
                s1: -0.4,
                ..Radio::default()
            },
        ),
    ];
    for (name, radio) in &scenes {
        show(&mut ov, &page, radio);
        page.screenshot(&format!("gallery-{name}"));
    }
    ov.line("disconnect");
    page.wait_until(
        "offline",
        "document.getElementById('root').classList.contains('offline')",
    );
    page.screenshot("gallery-5-disconnected");

    let mut ov = Overlay::start();
    let Some(page) = Page::open(&ov, "?setup=1&trail=0") else {
        return;
    };
    show(
        &mut ov,
        &page,
        &Radio {
            left_y: -1.0,
            ..Radio::default()
        },
    );
    page.eval("document.getElementById('bStart').click()");
    page.wait_until(
        "wizard",
        "document.getElementById('root').dataset.learnTarget === 'left_y'",
    );
    page.screenshot("gallery-6-setup-wizard");
}

/// Records what was sent and what got drawn for a set of positions, using the same
/// measurements and tolerances as the tests above, to target/tmp/evidence.json.
/// Fails if any check fails. Run with `cargo test --test blackbox_render evidence -- --ignored`.
#[test]
#[ignore]
fn evidence() {
    let mut ov = Overlay::start();
    let Some(page) = Page::open(&ov, "?trail=0") else {
        return;
    };
    let r = Radio::default();
    let mut cases: Vec<(String, Radio)> = vec![
        ("Sticks centred".into(), r),
        ("Right stick full right".into(), Radio { right_x: 1.0, ..r }),
        ("Right stick full up".into(), Radio { right_y: 1.0, ..r }),
        ("Left stick full left".into(), Radio { left_x: -1.0, ..r }),
        (
            "Throttle closed (left stick down)".into(),
            Radio { left_y: -1.0, ..r },
        ),
        (
            "Both sticks in the top-right corner".into(),
            Radio {
                left_x: 1.0,
                left_y: 1.0,
                right_x: 1.0,
                right_y: 1.0,
                ..r
            },
        ),
        (
            "Half-way diagonals".into(),
            Radio {
                left_x: -0.5,
                left_y: 0.5,
                right_x: 0.5,
                right_y: -0.5,
                ..r
            },
        ),
        (
            "SA down, SB middle, SC down, SE held, S1 +40%".into(),
            Radio {
                sa: 1,
                sb: 1,
                sc: 2,
                se: true,
                s1: 0.4,
                ..r
            },
        ),
    ];
    let mut rng = Rng(2026);
    for i in 1..=4 {
        cases.push((format!("Random position {i}"), rng.radio()));
    }

    let arrows2 = ["↑", "↓"];
    let arrows3 = ["↑", "–", "↓"];
    let round3 = |v: f64| (v * 1000.0).round() / 1000.0;
    let mut rows = Vec::new();
    for (name, r) in &cases {
        let m = show(&mut ov, &page, r);
        let drawn = |side: &str| {
            let travel = &m[format!("travel-{side}").as_str()];
            let knob = &m[format!("knob-{side}").as_str()];
            (
                (num(&knob["cx"]) - num(&travel["x"])) / num(&travel["w"]) * 2.0 - 1.0,
                1.0 - (num(&knob["cy"]) - num(&travel["y"])) / num(&travel["h"]) * 2.0,
            )
        };
        let (lx, ly) = drawn("L");
        let (rx, ry) = drawn("R");
        let sent = [r.left_x, r.left_y, r.right_x, r.right_y].map(f64::from);
        let sticks_ok = sent
            .iter()
            .zip([lx, ly, rx, ry])
            .all(|(s, d)| (s - d).abs() <= 0.01);
        // what went over "USB" and what the overlay decoded from it
        let usb = ov.last_report.clone();
        let decoded = page.eval("document.getElementById('root').dataset.channels");
        let decoded: Vec<i64> = decoded
            .as_str()
            .unwrap()
            .split(',')
            .take(10)
            .map(|v| v.parse().unwrap())
            .collect();
        let want_tags = [
            ("tag-sa", format!("SA {}", arrows2[r.sa as usize])),
            ("tag-sb", format!("SB {}", arrows3[r.sb as usize])),
            ("tag-sc", format!("SC {}", arrows3[r.sc as usize])),
            ("tag-sd", format!("SD {}", arrows2[r.sd as usize])),
            ("tag-se", format!("SE {}", if r.se { "●" } else { "○" })),
            ("tag-s1", format!("S1 {}", pct(r.s1))),
        ];
        let switches_ok = want_tags
            .iter()
            .all(|(k, want)| m["text"][*k] == want.as_str());
        let thr = js_round((sent[1] + 1.0) * 50.0);
        let want_readout = format!(
            "THR {thr}%  RUD {}  ELE {}  AIL {}",
            pct(r.left_x),
            pct(r.right_y),
            pct(r.right_x)
        );
        let readout_ok = m["text"]["readout"] == want_readout.as_str();

        rows.push(serde_json::json!({
            "name": name,
            "sent": {
                "left": [r.left_x, r.left_y], "right": [r.right_x, r.right_y],
                "sa": r.sa, "sb": r.sb, "sc": r.sc, "sd": r.sd, "se": r.se, "s1": r.s1,
            },
            "usb": usb,
            "channels": decoded,
            "drawn": {
                "left": [round3(lx), round3(ly)], "right": [round3(rx), round3(ry)],
                "labels": want_tags.iter().map(|(k, _)| m["text"][*k].clone()).collect::<Vec<_>>(),
                "readout": m["text"]["readout"],
            },
            "checks": { "sticks": sticks_ok, "switches": switches_ok, "readout": readout_ok },
        }));
        assert!(sticks_ok && switches_ok && readout_ok, "{name}: {m}");
    }
    let out = serde_json::json!({ "encoder": ov.encoder_name(), "rows": rows });
    let path = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("evidence.json");
    std::fs::write(&path, serde_json::to_string_pretty(&out).unwrap()).unwrap();
    eprintln!("evidence: {}", path.display());
}

// ---------------------------------------------------------------------------------------
// skins

/// What's drawn on top at a point of the drawing's frame, and whether the skin is showing.
const PROBE: &str = r#"(() => {
  const root = document.getElementById("root");
  const at = (x, y) => {
    const p = root.createSVGPoint(); p.x = x; p.y = y;
    const s = p.matrixTransform(root.getScreenCTM());
    const e = document.elementFromPoint(s.x, s.y);
    return e ? (e.dataset.test || e.id || e.tagName) : null;
  };
  const center = sel => { const b = document.querySelector(sel).getBoundingClientRect(); return [b.x + b.width / 2, b.y + b.height / 2]; };
  const top = sel => { const [x, y] = center(sel); const e = document.elementFromPoint(x, y); return e ? (e.dataset.test || e.id) : null; };
  const body = document.getElementById("frontBody"), b = body.getBBox();
  const inBody = (x, y) => { const p = root.createSVGPoint(); p.x = x; p.y = y; return body.isPointInFill(p); };
  let outside = null;
  for (let y = b.y + 3; y < b.y + b.height && !outside; y += 3)
    for (let x = b.x + 3; x < b.x + 80; x += 3)
      if (!inBody(x, y)) { outside = [x, y]; break; }
  return JSON.stringify({
    skinned: root.classList.contains("skinned"),
    skin: root.dataset.skin,
    bodyDisplay: getComputedStyle(body).display,
    shell: at(215, 520),
    outsideOutline: outside ? at(...outside) : "none found",
    speaker: at(501, 703),
    screenBezel: at(430, 540),
    knobL: top('[data-test="knob-L"]'),
    knobR: top('[data-test="knob-R"]'),
  });
})()"#;

fn probe(page: &Page) -> Value {
    serde_json::from_str(page.eval(PROBE).as_str().unwrap()).unwrap()
}

#[test]
fn a_skin_wraps_the_body_and_everything_else_stays_on_top() {
    let mut ov = Overlay::start();
    let (status, _, _) = common::http(
        ov.port,
        "PUT",
        "/skins/red",
        &[("Content-Type", "image/png")],
        &common::RED_PNG,
    );
    assert_eq!(status, 204);
    let Some(page) = Page::open(&ov, "?trail=0&accent=%23ff00ff&skin=red") else {
        return;
    };
    let r = Radio {
        left_x: 0.5,
        left_y: -0.5,
        right_x: -0.25,
        right_y: 0.75,
        sa: 1,
        ..Radio::default()
    };
    let m = show(&mut ov, &page, &r);
    page.wait_until(
        "skin loaded",
        "document.getElementById('root').dataset.skin === 'red'",
    );
    let p = probe(&page);
    assert_eq!(p["skinned"], true, "{p}");
    // like a vinyl skin: on the shell, but cropped to the radio's outline
    assert_eq!(p["shell"], "skin", "the skin covers the shell: {p}");
    assert_ne!(
        p["outsideOutline"], "skin",
        "the skin stays inside the outline: {p}"
    );
    assert_ne!(p["outsideOutline"], "none found", "{p}");
    // the radio's details are drawn over it
    assert_ne!(p["speaker"], "skin", "{p}");
    assert_ne!(p["screenBezel"], "skin", "{p}");
    assert_eq!(p["knobL"], "knob-L", "{p}");
    assert_eq!(p["knobR"], "knob-R", "{p}");
    // and the moving parts still show exactly what the radio is doing
    for (side, sx, sy) in [("L", r.left_x, r.left_y), ("R", r.right_x, r.right_y)] {
        let travel = &m[format!("travel-{side}").as_str()];
        let knob = &m[format!("knob-{side}").as_str()];
        let nx = (num(&knob["cx"]) - num(&travel["x"])) / num(&travel["w"]) * 2.0 - 1.0;
        let ny = 1.0 - (num(&knob["cy"]) - num(&travel["y"])) / num(&travel["h"]) * 2.0;
        approx(nx, f64::from(sx), 0.01, "x with a skin", &m);
        approx(ny, f64::from(sy), 0.01, "y with a skin", &m);
    }
    assert_eq!(m["fill"]["sa-front"], ACCENT, "switches still light up");
    page.screenshot("skinned");
}

#[test]
fn the_saved_skin_shows_without_changing_the_obs_url() {
    let mut ov = Overlay::start();
    common::http(
        ov.port,
        "PUT",
        "/skins/red",
        &[("Content-Type", "image/png")],
        &common::RED_PNG,
    );
    // chosen on the setup page
    let Some(setup) = Page::open(&ov, "?setup=1") else {
        return;
    };
    setup.wait_until(
        "skin listed",
        "[...document.getElementById('skinSel').options].some(o => o.value === 'red')",
    );
    setup.eval("const s = document.getElementById('skinSel'); s.value = 'red'; s.dispatchEvent(new Event('change'))");
    // an OBS page with the plain URL picks it up...
    let Some(obs) = Page::open(&ov, "?trail=0") else {
        return;
    };
    show(&mut ov, &obs, &Radio::default());
    obs.wait_until(
        "skin applied",
        "document.getElementById('root').dataset.skin === 'red'",
    );
    assert_eq!(probe(&obs)["shell"], "skin");
    // ...and ?skin=none still shows the built-in drawing
    let Some(plain) = Page::open(&ov, "?trail=0&skin=none") else {
        return;
    };
    show(&mut ov, &plain, &Radio::default());
    let p = probe(&plain);
    assert_eq!(p["skinned"], false, "{p}");
    // the drawn shell (or the shading drawn over it) is what's on top
    assert!(
        p["shell"] == "frontBody" || p["shell"] == "frontShade",
        "{p}"
    );

    // a skin that doesn't exist (a typo in the OBS URL) leaves the built-in drawing up
    let Some(typo) = Page::open(&ov, "?trail=0&skin=no-such-skin") else {
        return;
    };
    show(&mut ov, &typo, &Radio::default());
    std::thread::sleep(Duration::from_millis(500));
    let p = probe(&typo);
    assert_eq!(p["skinned"], false, "{p}");
    assert!(
        p["shell"] == "frontBody" || p["shell"] == "frontShade",
        "{p}"
    );
}

// ---------------------------------------------------------------------------------------
// accent colour

/// The colour the power light is drawn in (it uses the accent while connected).
const POWER_STROKE: &str = "getComputedStyle(document.getElementById('power')).stroke";

#[test]
fn accent_picked_on_the_setup_page_reaches_obs() {
    let mut ov = Overlay::start();
    let Some(setup) = Page::open(&ov, "?setup=1") else {
        return;
    };
    let Some(obs) = Page::open(&ov, "?trail=0") else {
        return;
    };
    let Some(pinned) = Page::open(&ov, "?trail=0&accent=%23ff00ff") else {
        return;
    };
    show(&mut ov, &obs, &Radio::default());
    let colour_is = |page: &Page, rgb: &str| {
        page.wait_until(rgb, &format!("{POWER_STROKE} === '{rgb}'"));
    };
    colour_is(&obs, "rgb(57, 255, 136)"); // default green

    // a preset swatch
    setup.eval("document.querySelector('[data-test=swatch-00e0ff]').click()");
    colour_is(&obs, "rgb(0, 224, 255)");
    colour_is(&setup, "rgb(0, 224, 255)");
    assert_eq!(
        setup.eval(
            "document.querySelector('[data-test=swatch-00e0ff]').getAttribute('aria-pressed')"
        ),
        "true"
    );
    // any colour from the picker
    setup.eval(
        "const p = document.getElementById('accentPick'); p.value = '#123456'; p.dispatchEvent(new Event('input'))",
    );
    colour_is(&obs, "rgb(18, 52, 86)");
    assert_eq!(
        setup.eval("document.getElementById('accentHex').textContent"),
        "#123456"
    );
    // a scene with ?accent= in its URL keeps its own colour
    colour_is(&pinned, "rgb(255, 0, 255)");
    // back to the default
    setup.eval("document.getElementById('accentReset').click()");
    colour_is(&obs, "rgb(57, 255, 136)");
    assert_eq!(
        setup.eval("document.getElementById('accentReset').disabled"),
        true
    );
    assert!(!ov.config_text().contains("accent"), "{}", ov.config_text());
}

/// Where the radio and the strip under it are on screen, and where the drawing ends.
const LAYOUT: &str = r#"(() => {
  const q = s => document.querySelector(`[data-test="${s}"]`);
  const r = e => { const b = e.getBoundingClientRect(); return { x: b.x, y: b.y, w: b.width, h: b.height, bottom: b.bottom }; };
  const root = document.getElementById("root"), vb = root.viewBox.baseVal, drawn = root.getBBox();
  return JSON.stringify({
    parts: root.dataset.parts, innerWidth, innerHeight,
    body: r(document.getElementById("frontBody")), knob: r(q("knob-L")), ch1: r(q("ch-bg-1")),
    readout: r(q("readout")).w > 0, channels: r(q("ch-bg-1")).w > 0,
    vbBottom: vb.y + vb.height, drawnBottom: drawn.y + drawn.height,
  });
})()"#;

#[test]
fn the_strip_under_the_radio_can_be_turned_off() {
    let mut ov = Overlay::start();
    let Some(setup) = Page::open(&ov, "?setup=1") else {
        return;
    };
    // the OBS source size the README gives
    let Some(obs) = Page::open_sized(&ov, "?trail=0", (680, 830)) else {
        return;
    };
    // a scene that keeps the bars but not the readout, whatever is saved
    let Some(pinned) = Page::open(&ov, "?trail=0&channels=1&readout=0") else {
        return;
    };
    show(&mut ov, &obs, &Radio::default());
    let layout = |page: &Page| -> Value {
        serde_json::from_str(page.eval(LAYOUT).as_str().unwrap()).unwrap()
    };
    let parts_are = |page: &Page, parts: &str| {
        page.wait_until(
            parts,
            &format!("document.getElementById('root').dataset.parts === '{parts}'"),
        );
    };
    let hint_is = |size: &str| {
        setup.wait_until(
            size,
            &format!("document.querySelector('[data-test=obs-size]').textContent === '{size}'"),
        );
    };
    let toggle = |which: &str| {
        setup.eval(&format!(
            "document.querySelector('[data-test=show-{which}]').click()"
        ));
    };
    // OBS draws the 682-wide front view 680 wide
    let px = 680.0 / 682.0;

    // the stick mode only names the readout's values, so it's greyed out without it
    let mode_usable = |usable: bool| {
        setup.wait_until(
            "stick mode greyed out with the readout",
            &format!(
                "document.querySelector('[data-test=mode-select]').disabled === {}",
                !usable
            ),
        );
    };

    parts_are(&obs, "11");
    parts_are(&pinned, "01");
    hint_is("680 × 830");
    mode_usable(true);
    let full = layout(&obs);
    eprintln!("both shown: {full}");
    assert!(
        full["readout"] == true && full["channels"] == true,
        "{full}"
    );
    assert_eq!(full["innerWidth"], 680, "{full}");
    // nothing cut off at the README's size
    assert!(
        num(&full["drawnBottom"]) <= num(&full["vbBottom"]) + 0.5,
        "{full}"
    );
    assert!(
        num(&full["ch1"]["bottom"]) <= num(&full["innerHeight"]),
        "{full}"
    );

    // both off on the setup page: just the controller
    toggle("readout");
    toggle("channels");
    parts_are(&obs, "00");
    hint_is("680 × 660");
    mode_usable(false);
    let bare = layout(&obs);
    eprintln!("just the controller: {bare}");
    obs.screenshot("just-the-controller");
    assert!(
        bare["readout"] == false && bare["channels"] == false,
        "{bare}"
    );
    // the radio doesn't move or change size in the scene
    for part in ["body", "knob"] {
        for d in ["x", "y", "w", "h"] {
            approx(
                num(&bare[part][d]),
                num(&full[part][d]),
                0.5,
                &format!("{part}.{d}"),
                &bare,
            );
        }
    }
    // the drawing ends right under the radio, and fits the suggested size
    let spare = num(&bare["vbBottom"]) - num(&bare["drawnBottom"]);
    assert!(
        (-0.5..20.0).contains(&spare),
        "{spare} spare at the bottom: {bare}"
    );
    assert!(num(&bare["body"]["bottom"]) <= 660.0, "{bare}");
    let text = ov.config_text();
    assert!(
        text.contains("show_readout = false") && text.contains("show_channels = false"),
        "{text}"
    );
    // the URL still wins for that one scene
    parts_are(&pinned, "01");

    // bars back without the readout: they move up into its place
    toggle("channels");
    parts_are(&obs, "01");
    hint_is("680 × 790");
    let bars = layout(&obs);
    approx(
        num(&bars["ch1"]["y"]),
        num(&full["ch1"]["y"]) - 38.0 * px,
        0.5,
        "bars move up",
        &bars,
    );
    approx(
        num(&bars["knob"]["y"]),
        num(&full["knob"]["y"]),
        0.5,
        "knob stays",
        &bars,
    );

    // back to everything, and the settings file forgets it
    toggle("readout");
    parts_are(&obs, "11");
    hint_is("680 × 830");
    mode_usable(true);
    assert!(!ov.config_text().contains("show_"), "{}", ov.config_text());
}
