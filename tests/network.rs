//! Black-box tests for showing the overlay on another PC in the home network (the radio
//! on the gaming PC, OBS on the streaming PC). The other PC is played by connections to
//! this PC's own network address, which the server sees as coming from outside 127.0.0.1.

mod common;

use std::net::{SocketAddr, TcpStream};
use std::time::{Duration, Instant};

use common::ws::Ws;
use common::{Overlay, RED_PNG, Radio, expected_channels, http, http_at};
use pocket_overlay::config::Config;
use serde_json::{Value, json};
use tokio_tungstenite::tungstenite::client::IntoClientRequest;

/// This PC's address as another PC reaches it; `None` (and the test passes) without a
/// network.
fn lan(port: u16) -> Option<SocketAddr> {
    let ip = pocket_overlay::network::lan_ip();
    if ip.is_none() {
        eprintln!("SKIPPED: this PC has no network address");
    }
    Some(SocketAddr::new(ip?, port))
}

/// Whether another PC can connect, waiting (up to 5 s) for it to become `want` while
/// the port switches over.
fn reachable(addr: SocketAddr, want: bool) -> bool {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let open = TcpStream::connect_timeout(&addr, Duration::from_millis(500)).is_ok();
        if open == want || Instant::now() > deadline {
            return open;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

fn shared() -> Overlay {
    let cfg = Config {
        lan: true,
        ..Config::default()
    };
    Overlay::start_with(Some(&toml::to_string(&cfg).unwrap()))
}

#[tokio::test(flavor = "multi_thread")]
async fn other_pcs_get_in_only_while_it_is_switched_on() {
    let ov = Overlay::start();
    let Some(addr) = lan(ov.port) else { return };
    let mut here = Ws::connect(ov.port).await;
    assert_eq!(here.last["lan"], json!(false));
    assert!(!reachable(addr, false), "closed to other PCs by default");

    here.send_json(json!({ "cmd": "set_lan", "on": true }))
        .await;
    here.wait_for("sharing on", |s| s["lan"] == json!(true))
        .await;
    assert!(reachable(addr, true), "other PCs can connect");
    assert!(ov.config_text().contains("lan = true"), "saved");
    let (status, _, page) = http_at(addr, "GET", "/", &[], b"");
    assert_eq!(status, 200);
    assert!(String::from_utf8_lossy(&page).contains("<svg"));
    // the settings page asks this for the address to show
    let (status, _, body) = http_at(addr, "GET", "/network", &[], b"");
    assert_eq!(status, 200);
    let network: Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(network["ip"], json!(addr.ip().to_string()));
    assert_eq!(
        network["name"],
        json!(pocket_overlay::network::address().name)
    );

    // switched off: closed again, and a page left open on the other PC is disconnected
    let mut other = Ws::connect_to(&addr.to_string()).await;
    here.send_json(json!({ "cmd": "set_lan", "on": false }))
        .await;
    here.wait_for("sharing off", |s| s["lan"] == json!(false))
        .await;
    assert!(other.closed().await, "the other PC's page is disconnected");
    assert!(!reachable(addr, false), "closed to other PCs");
    assert!(!ov.config_text().contains("lan = "), "saved");
    // this PC carries on
    let (status, _, _) = http(ov.port, "GET", "/state", &[], b"");
    assert_eq!(status, 200);
}

#[tokio::test(flavor = "multi_thread")]
async fn other_pcs_can_only_watch() {
    let mut ov = shared();
    let Some(addr) = lan(ov.port) else { return };
    let host = addr.to_string();
    let mut here = Ws::connect(ov.port).await;
    let mut other = Ws::connect_to(&host).await;

    // the other PC sees the radio, live
    let radio = Radio {
        left_x: 0.5,
        sb: 2,
        se: true,
        ..Radio::default()
    };
    ov.radio(&radio);
    let want = expected_channels(&ov.wiring.channels(&radio));
    let state = other
        .wait_for("the radio", |s| {
            s["channels"].as_array().is_some_and(|ch| {
                ch.iter()
                    .zip(&want)
                    .all(|(a, b)| a.as_i64() == Some(i64::from(*b)))
            })
        })
        .await;
    assert_eq!(state["left"]["x"], json!(0.5));

    // but can't change anything: its commands are dropped...
    other
        .send_json(json!({ "cmd": "set_mode", "mode": 1 }))
        .await;
    other.command("learn_start").await;
    other
        .send_json(json!({ "cmd": "set_lan", "on": false }))
        .await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    here.send_json(json!({ "cmd": "set_accent", "accent": "#ff0000" }))
        .await;
    let state = here
        .wait_for("this PC's change", |s| s["accent"] == json!("#ff0000"))
        .await;
    assert_eq!(state["mode"], json!(2), "mode unchanged");
    assert!(state["learn"].is_null(), "no channel detection");
    assert_eq!(state["lan"], json!(true), "still shared");

    // ...and it can't upload or delete skins
    let png = ("Content-Type", "image/png");
    let (status, _, _) = http_at(addr, "PUT", "/skins/x", &[png], &RED_PNG);
    assert_eq!(status, 403);
    let (status, _, _) = http(ov.port, "PUT", "/skins/x", &[png], &RED_PNG);
    assert_eq!(status, 204, "this PC can");
    let (status, _, _) = http_at(addr, "DELETE", "/skins/x", &[], b"");
    assert_eq!(status, 403);
    let (status, _, _) = http_at(addr, "GET", "/skins/x", &[], b"");
    assert_eq!(status, 200, "but it can show them");
}

#[tokio::test(flavor = "multi_thread")]
async fn other_pcs_use_this_pcs_name_and_nothing_else() {
    let ov = shared();
    let Some(addr) = lan(ov.port) else { return };
    let port = ov.port;
    let get = |host: &str| http_at(addr, "GET", "/state", &[("Host", host)], b"").0;

    // this PC's name, the way OBS on the other PC opens it
    if let Some(name) = pocket_overlay::network::host_name() {
        assert_eq!(get(&format!("{name}:{port}")), 200);
        assert_eq!(get(&format!("{}:{port}", name.to_ascii_uppercase())), 200);
        let short = name.split('.').next().unwrap();
        assert_eq!(get(&format!("{short}.local:{port}")), 200);
        // another website's name pointed at this PC (DNS rebinding)
        assert_eq!(get(&format!("{short}.evil.example:{port}")), 403);
    }
    assert_eq!(get(&format!("evil.example:{port}")), 403);

    // a page from another website, on the other PC's browser
    let (status, _, _) = http_at(
        addr,
        "GET",
        "/state",
        &[("Origin", "http://evil.example")],
        b"",
    );
    assert_eq!(status, 403);
    let mut req = format!("ws://{addr}/ws").into_client_request().unwrap();
    req.headers_mut()
        .insert("Origin", "http://evil.example".parse().unwrap());
    let err = tokio_tungstenite::connect_async(req).await.unwrap_err();
    assert!(err.to_string().contains("403"), "{err}");
    // the overlay's own page, opened from the other PC
    let mut req = format!("ws://{addr}/ws").into_client_request().unwrap();
    req.headers_mut()
        .insert("Origin", format!("http://{addr}").parse().unwrap());
    tokio_tungstenite::connect_async(req)
        .await
        .expect("its own page connects");
}

#[tokio::test(flavor = "multi_thread")]
async fn starts_shared_and_says_where() {
    let ov = shared();
    let Some(addr) = lan(ov.port) else { return };
    assert!(reachable(addr, true));
    let deadline = Instant::now() + Duration::from_secs(5);
    let line = loop {
        let log = ov.log.lock().unwrap().clone();
        if let Some(line) = log.into_iter().find(|l| l.contains("On another PC:")) {
            break line;
        }
        assert!(
            Instant::now() < deadline,
            "no address printed for other PCs"
        );
        std::thread::sleep(Duration::from_millis(20));
    };
    assert!(line.contains(&format!("http://{addr}/")), "{line}");
    if let Some(name) = pocket_overlay::network::address().name {
        assert!(
            line.contains(&format!("http://{name}:{}/", ov.port)),
            "{line}"
        );
    }
}
