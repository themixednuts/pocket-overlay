//! Black-box tests over the WebSocket: drive the real binary like a USB radio (report bytes
//! on stdin, produced by EdgeTX's own encoder when available) and check the state it
//! publishes matches what the physical controls were doing.

mod common;

use std::time::Duration;

use common::ws::Ws;
use common::{Overlay, Radio, Rng, Wiring, expected_channels, to_hex};
use pocket_overlay::learn::Target;
use serde_json::{Value, json};

fn channels_of(state: &Value) -> Vec<i64> {
    state["channels"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_i64().unwrap())
        .collect()
}

fn is_showing(state: &Value, ch: &[i16]) -> bool {
    let got = channels_of(state);
    got.len() >= ch.len() && got.iter().zip(ch).all(|(a, b)| *a == i64::from(*b))
}

/// Sends the radio's position and waits until the overlay has published it.
async fn show(ov: &mut Overlay, ws: &mut Ws, radio: &Radio) -> Value {
    ov.radio(radio);
    let ch = expected_channels(&ov.wiring.channels(radio));
    ws.wait_for("the new channels", |s| is_showing(s, &ch))
        .await
}

/// The published state reads back exactly what the physical controls are doing.
fn assert_reads(state: &Value, r: &Radio) {
    let f = |v: &Value| v.as_f64().unwrap() as f32;
    let close = |a: f32, b: f32| (a - b).abs() < 1e-4;
    let sticks = [
        ("left.x", f(&state["left"]["x"]), r.left_x),
        ("left.y", f(&state["left"]["y"]), r.left_y),
        ("right.x", f(&state["right"]["x"]), r.right_x),
        ("right.y", f(&state["right"]["y"]), r.right_y),
        ("s1", f(&state["s1"]), r.s1),
    ];
    for (name, got, want) in sticks {
        assert!(
            close(got, want),
            "{name}: overlay {got}, radio {want}\n{state:#}"
        );
    }
    let switches = [
        ("sa", r.sa),
        ("sb", r.sb),
        ("sc", r.sc),
        ("sd", r.sd),
        ("se", r.se as u8),
    ];
    for (name, want) in switches {
        assert_eq!(state[name], json!(want), "{name}\n{state:#}");
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn physical_controls_round_trip() {
    let mut ov = Overlay::start();
    eprintln!("encoder: {}", ov.encoder_name());
    let mut ws = Ws::connect(ov.port).await;

    let full = |v: f32, p2: u8, p3: u8| Radio {
        left_x: v,
        left_y: v,
        right_x: v,
        right_y: v,
        sa: p2,
        sb: p3,
        sc: p3,
        sd: p2,
        se: p2 == 1,
        s1: v,
    };
    let mut cases = vec![
        Radio::default(),
        full(1.0, 1, 2),
        full(-1.0, 0, 0),
        full(0.0, 1, 1),
    ];
    // one stick direction at a time, to catch swapped or reversed axes
    for (i, v) in [
        (0, 1.0),
        (1, 1.0),
        (2, 1.0),
        (3, 1.0),
        (0, -0.5),
        (1, -0.5),
        (2, -0.5),
        (3, -0.5),
    ] {
        let mut r = Radio::default();
        *[&mut r.left_x, &mut r.left_y, &mut r.right_x, &mut r.right_y][i] = v;
        cases.push(r);
    }
    let mut rng = Rng(0x5eed);
    cases.extend((0..200).map(|_| rng.radio()));

    for radio in &cases {
        let state = show(&mut ov, &mut ws, radio).await;
        assert!(state["connected"].as_bool().unwrap());
        assert_reads(&state, radio);
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn every_channel_is_exact_including_out_of_range() {
    let mut ov = Overlay::start();
    let mut ws = Ws::connect(ov.port).await;
    let mut rng = Rng(99);
    for _ in 0..100 {
        let mut ch = [0i16; 32];
        for c in &mut ch {
            *c = rng.below(3001) as i16 - 1500;
        }
        ov.channels(&ch);
        let want = expected_channels(&ch);
        let state = ws.wait_for("channels", |s| is_showing(s, &want)).await;
        assert_eq!(channels_of(&state).len(), 32);
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn uses_the_layout_the_device_announces() {
    let mut ov = Overlay::start();
    let mut ws = Ws::connect(ov.port).await;
    // Report ID 1: four signed 16-bit axes (-32767..32767), then 8 buttons.
    let desc = [
        0x05, 0x01, 0x09, 0x04, 0xA1, 0x01, 0x85, 0x01, //
        0x09, 0x30, 0x09, 0x31, 0x09, 0x32, 0x09, 0x33, //
        0x16, 0x01, 0x80, 0x26, 0xFF, 0x7F, 0x75, 0x10, 0x95, 0x04, 0x81, 0x02, //
        0x05, 0x09, 0x19, 0x01, 0x29, 0x08, 0x15, 0x00, 0x25, 0x01, 0x75, 0x01, 0x95, 0x08, 0x81,
        0x02, //
        0xC0,
    ];
    ov.line(&format!("descriptor {}", to_hex(&desc)));
    let mut report = vec![1u8];
    for v in [32767i16, -32767, 0, 16384] {
        report.extend(v.to_le_bytes());
    }
    report.push(0b1000_0001); // buttons 1 and 8
    ov.line(&to_hex(&report));

    let state = ws
        .wait_for("custom layout", |s| {
            s["layout"] == "custom: 4 axes, 8 buttons"
        })
        .await;
    let ch = channels_of(&state);
    assert_eq!(
        ch[..12],
        [
            1024, -1024, 0, 512, 1024, -1024, -1024, -1024, -1024, -1024, -1024, 1024
        ]
    );
    assert_eq!(ch.len(), 32, "padded to 32 channels");
    // default config: left stick vertical = CH3, right stick horizontal = CH1
    assert_eq!(state["left"]["y"], json!(0.0));
    assert_eq!(state["right"]["x"], json!(1.0));
}

#[tokio::test(flavor = "multi_thread")]
async fn disconnect_and_reconnect() {
    let mut ov = Overlay::start();
    let mut ws = Ws::connect(ov.port).await;
    let radio = Radio {
        left_y: 0.5,
        sa: 1,
        ..Radio::default()
    };
    show(&mut ov, &mut ws, &radio).await;
    ov.line("disconnect");
    let state = ws
        .wait_for("disconnect", |s| s["connected"] == json!(false))
        .await;
    assert!(
        channels_of(&state).iter().all(|v| *v == 0),
        "no stale values while disconnected"
    );
    let state = show(&mut ov, &mut ws, &radio).await;
    assert_eq!(state["connected"], json!(true));
    assert_reads(&state, &radio);
}

#[tokio::test(flavor = "multi_thread")]
async fn classifies_channels_from_movement() {
    let mut ov = Overlay::start();
    let mut ws = Ws::connect(ov.port).await;
    let mut rng = Rng(3);
    for i in 0..40 {
        let radio = Radio {
            left_x: rng.unit(),
            left_y: rng.unit(),
            right_x: rng.unit(),
            right_y: rng.unit(),
            sa: (i % 2) as u8,
            sb: (i % 3) as u8,
            se: i % 5 == 0,
            ..Radio::default()
        };
        show(&mut ov, &mut ws, &radio).await;
    }
    let kinds: Vec<String> = ws.last["kinds"]
        .as_array()
        .unwrap()
        .iter()
        .map(|k| k.as_str().unwrap().into())
        .collect();
    // CH1-4 sticks, CH5 SA, CH6 SB, CH7 SC (untouched), CH8 S1 (untouched), CH9 SD, CH10 SE
    assert_eq!(
        kinds[..10],
        [
            "analog", "analog", "analog", "analog", "switch2", "switch3", "idle", "idle", "idle",
            "switch2"
        ]
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn replays_a_recording_with_its_timing() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("flight.txt");
    let mut ch = [0i16; 32];
    let first = pocket_overlay::edgetx::encode_classic(&ch);
    ch[2] = 1024; // throttle up
    ch[9] = 1024; // SE pressed
    let second = pocket_overlay::edgetx::encode_classic(&ch);
    let text = format!(
        "# recorded\nname Radiomaster Pocket Joystick\ndescriptor {}\nwait 0\n{}\nwait 700\n{}\n",
        to_hex(&pocket_overlay::edgetx::CLASSIC_DESCRIPTOR),
        to_hex(&first),
        to_hex(&second)
    );
    std::fs::write(&file, text).unwrap();

    let launched = std::time::Instant::now();
    let ov = Overlay::launch(&["--replay", file.to_str().unwrap()], None);
    let mut ws = Ws::connect(ov.port).await;
    let state = ws
        .wait_for("first report", |s| s["connected"] == json!(true))
        .await;
    assert_eq!(state["name"], "Radiomaster Pocket Joystick");
    assert_eq!(state["layout"], "EdgeTX classic: 8 axes, 24 buttons");
    assert_eq!(
        state["left"]["y"],
        json!(0.0),
        "second report not played yet"
    );
    let state = ws
        .wait_for("second report", |s| s["left"]["y"] == json!(1.0))
        .await;
    assert!(
        launched.elapsed() >= Duration::from_millis(700),
        "the wait line was honoured"
    );
    assert_eq!(state["se"], json!(1));
    // the replay has ended: the last state stays up
    tokio::time::sleep(Duration::from_millis(300)).await;
    let state = ws.wait_for("still there", |_| true).await;
    assert_eq!(state["connected"], json!(true));
}

#[tokio::test(flavor = "multi_thread")]
async fn demo_mode_moves_everything() {
    let ov = Overlay::launch(&["--demo"], None);
    let mut ws = Ws::connect(ov.port).await;
    let first = ws.wait_for("demo", |s| s["connected"] == json!(true)).await;
    let moved = ws
        .wait_for("sticks to move", |s| {
            s["left"] != first["left"] && s["right"] != first["right"]
        })
        .await;
    assert_eq!(moved["name"], "Demo radio");
    // every control shows up mapped within a few seconds of animation
    let state = ws
        .wait_for("all controls", |s| {
            ["sa", "sb", "sc", "sd", "se", "s1"]
                .iter()
                .all(|k| !s[*k].is_null())
        })
        .await;
    assert!(channels_of(&state).len() >= 32);
}

// ---------------------------------------------------------------------------------------
// channel-detection wizard

/// Picks one analog control out of the radio.
type Axis = fn(&mut Radio) -> &mut f32;

/// Performs what the wizard asks for, like a person holding the radio would.
async fn act(ov: &mut Overlay, radio: &mut Radio, target: Target) {
    let pause = || tokio::time::sleep(Duration::from_millis(80));
    // a person moving one axis nudges the other one a little
    let (axis, crosstalk): (Axis, Axis) = match target {
        Target::LeftX => (|r| &mut r.left_x, |r| &mut r.left_y),
        Target::LeftY => (|r| &mut r.left_y, |r| &mut r.left_x),
        Target::RightX => (|r| &mut r.right_x, |r| &mut r.right_y),
        Target::RightY => (|r| &mut r.right_y, |r| &mut r.right_x),
        _ => (|r| &mut r.s1, |r| &mut r.s1),
    };
    match target {
        Target::LeftX | Target::LeftY | Target::RightX | Target::RightY | Target::S1 => {
            let before = *crosstalk(radio);
            let nudged = before + if before > 0.5 { -0.06 } else { 0.06 };
            for v in [-1.0, -0.4, 0.3, 1.0] {
                *axis(radio) = v;
                if target != Target::S1 {
                    *crosstalk(radio) = nudged;
                }
                ov.radio(radio);
                pause().await;
            }
        }
        Target::SA | Target::SD | Target::SB | Target::SC => {
            let (pos, last): (fn(&mut Radio) -> &mut u8, u8) = match target {
                Target::SA => (|r| &mut r.sa, 1),
                Target::SD => (|r| &mut r.sd, 1),
                Target::SB => (|r| &mut r.sb, 2),
                _ => (|r| &mut r.sc, 2),
            };
            for p in [0, last] {
                *pos(radio) = p;
                ov.radio(radio);
                pause().await;
            }
        }
        Target::SE => {
            radio.se = true;
            ov.radio(radio);
        }
    }
}

/// Lets go after a step: sticks recentre (throttle stays where it is), SE springs back.
fn release(radio: &mut Radio, target: Target) {
    match target {
        Target::LeftX | Target::LeftY => {
            radio.left_x = 0.0;
            radio.left_y = radio.left_y.round();
        }
        Target::RightX | Target::RightY => (radio.right_x, radio.right_y) = (0.0, 0.0),
        Target::SE => radio.se = false,
        _ => {}
    }
}

fn parse_target(v: &Value) -> Option<Target> {
    Target::ALL
        .into_iter()
        .find(|t| serde_json::to_value(t).unwrap() == *v)
}

fn target_of(state: &Value) -> Option<Target> {
    parse_target(&state["learn"]["target"])
}

async fn learn_random_wiring(seed: u64) {
    let mut ov = Overlay::start();
    let mut rng = Rng(seed);
    ov.wiring = Wiring::random(&mut rng);
    eprintln!("seed {seed}: wiring {:?}", ov.wiring);
    let mut ws = Ws::connect(ov.port).await;

    // at rest: throttle low, switches away from the pilot, S1 at one end
    let mut radio = Radio {
        left_y: -1.0,
        s1: -1.0,
        ..Radio::default()
    };
    show(&mut ov, &mut ws, &radio).await;

    ws.command("learn_start").await;
    let mut state = ws.wait_for("wizard start", |s| !s["learn"].is_null()).await;
    while let Some(target) = target_of(&state) {
        let step = state["learn"]["step"].as_u64().unwrap();
        act(&mut ov, &mut radio, target).await;
        state = ws
            .wait_for(&format!("{target:?} to be detected"), |s| {
                s["learn"]["step"].as_u64() != Some(step)
            })
            .await;
        release(&mut radio, target);
        ov.radio(&radio);
    }
    assert!(
        state["learn"]["saved"]
            .as_str()
            .unwrap()
            .starts_with("Saved"),
        "{state:#}"
    );

    // what it found is exactly how the radio is wired...
    for target in Target::ALL {
        let src = ov.wiring.get(target).unwrap();
        let found = state["learn"]["found"]
            .as_array()
            .unwrap()
            .iter()
            .find(|f| parse_target(&f["target"]) == Some(target))
            .unwrap();
        assert_eq!(found["source"]["ch"], json!(src.ch), "{target:?} channel");
        assert_eq!(
            found["source"]["invert"].as_bool().unwrap_or(false),
            src.invert,
            "{target:?} direction"
        );
    }
    // ...it was written to the config file...
    let saved: toml::Value = toml::from_str(&ov.config_text()).unwrap();
    assert_eq!(
        saved["sticks"]["left_y"]["ch"].as_integer().unwrap() as usize,
        ov.wiring.get(Target::LeftY).unwrap().ch
    );
    assert_eq!(
        saved["controls"]["SE"]["ch"].as_integer().unwrap() as usize,
        ov.wiring.get(Target::SE).unwrap().ch
    );

    // ...and from now on the overlay reads the physical controls correctly.
    ws.command("learn_close").await;
    for _ in 0..40 {
        let radio = rng.radio();
        let state = show(&mut ov, &mut ws, &radio).await;
        assert_reads(&state, &radio);
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn wizard_learns_random_wiring_1() {
    learn_random_wiring(1).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn wizard_learns_random_wiring_2() {
    learn_random_wiring(0xC0FFEE).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn wizard_learns_random_wiring_3() {
    learn_random_wiring(424242).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn wizard_refuses_on_off_channel_for_three_position_switch() {
    let mut ov = Overlay::start();
    // SB mixed to CH12, which USB can only send as on/off: its middle position would be lost.
    ov.wiring.0.retain(|(t, _)| *t != Target::SB);
    ov.wiring.0.push((
        Target::SB,
        pocket_overlay::config::Source {
            ch: 12,
            invert: false,
        },
    ));
    let mut ws = Ws::connect(ov.port).await;
    let mut radio = Radio::default();
    show(&mut ov, &mut ws, &radio).await;

    ws.command("learn_start").await;
    let mut state = ws.wait_for("wizard", |s| !s["learn"].is_null()).await;
    while target_of(&state) != Some(Target::SB) {
        let step = state["learn"]["step"].as_u64().unwrap();
        ws.command("learn_skip").await;
        state = ws
            .wait_for("skip", |s| s["learn"]["step"].as_u64() != Some(step))
            .await;
    }
    act(&mut ov, &mut radio, Target::SB).await;
    tokio::time::sleep(Duration::from_millis(900)).await; // well past the hold time
    let state = ws.wait_for("latest", |_| true).await;
    assert_eq!(
        target_of(&state),
        Some(Target::SB),
        "must not accept CH12 for SB\n{state:#}"
    );

    ws.command("learn_skip").await;
    let state = ws
        .wait_for("SB skipped", |s| target_of(s) == Some(Target::SC))
        .await;
    let sb = state["learn"]["found"]
        .as_array()
        .unwrap()
        .iter()
        .find(|f| parse_target(&f["target"]) == Some(Target::SB))
        .unwrap();
    assert_eq!(sb["source"], Value::Null);
}

#[tokio::test(flavor = "multi_thread")]
async fn stick_mode_is_set_from_the_page_and_saved() {
    let mut ov = Overlay::start();
    let mut ws = Ws::connect(ov.port).await;
    show(&mut ov, &mut ws, &Radio::default()).await;
    assert_eq!(ws.last["mode"], json!(2));

    ws.send_json(json!({ "cmd": "set_mode", "mode": 7 })).await; // not a mode: ignored
    ws.send_json(json!({ "cmd": "set_mode", "mode": 1 })).await;
    ws.wait_for("mode 1", |s| s["mode"] == json!(1)).await;
    let saved: toml::Value = toml::from_str(&ov.config_text()).unwrap();
    assert_eq!(saved["mode"].as_integer(), Some(1));
}

#[tokio::test(flavor = "multi_thread")]
async fn wizard_skip_and_cancel() {
    let mut ov = Overlay::start();
    let mut ws = Ws::connect(ov.port).await;
    show(&mut ov, &mut ws, &Radio::default()).await;

    // cancel: nothing changes
    ws.command("learn_start").await;
    ws.wait_for("wizard", |s| !s["learn"].is_null()).await;
    ws.command("learn_cancel").await;
    let state = ws.wait_for("cancel", |s| s["learn"].is_null()).await;
    assert_eq!(state["controls"]["SA"]["ch"], json!(5));

    // skip everything: sticks keep their channels, switches become unmapped
    ws.command("learn_start").await;
    for _ in 0..Target::ALL.len() {
        ws.command("learn_skip").await;
    }
    let state = ws
        .wait_for("all skipped", |s| {
            s["learn"]["target"].is_null() && !s["learn"].is_null()
        })
        .await;
    assert_eq!(state["sticks"]["left_y"]["ch"], json!(3));
    assert_eq!(state["controls"], json!({}));
    assert_eq!(state["sa"], Value::Null);
    assert_eq!(state["s1"], Value::Null);
    ws.command("learn_close").await;
    ws.wait_for("closed", |s| s["learn"].is_null()).await;
}
