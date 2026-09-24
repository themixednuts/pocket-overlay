//! Black-box tests for skins and for the server only answering its own pages: upload,
//! list, fetch, choose and remove skins over HTTP and the WebSocket, and reject bad input
//! and requests from other websites.

mod common;

use common::ws::Ws;
use common::{Overlay, RED_PNG, http};
use serde_json::json;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;

#[tokio::test(flavor = "multi_thread")]
async fn upload_choose_and_remove_a_skin() {
    let ov = Overlay::start();
    let mut ws = Ws::connect(ov.port).await;
    let rev = ws.last["skin_rev"].as_u64().unwrap();
    assert_eq!(ws.last["skin"], json!(null));

    let (status, _, _) = http(
        ov.port,
        "PUT",
        "/skins/neon-2",
        &[("Content-Type", "image/png")],
        &RED_PNG,
    );
    assert_eq!(status, 204);
    // open pages are told to reload skins
    ws.wait_for("skin_rev bump", |s| s["skin_rev"].as_u64() > Some(rev))
        .await;

    let (status, _, body) = http(ov.port, "GET", "/skins", &[], b"");
    assert_eq!(status, 200);
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(&body).unwrap(),
        json!(["neon-2"])
    );
    let (status, head, body) = http(ov.port, "GET", "/skins/neon-2", &[], b"");
    assert_eq!(status, 200);
    assert!(
        head.to_ascii_lowercase()
            .contains("content-type: image/png"),
        "{head}"
    );
    assert_eq!(body, RED_PNG);

    // choosing it is saved, so OBS shows it without changing its URL
    ws.send_json(json!({ "cmd": "set_skin", "skin": "neon-2" }))
        .await;
    ws.wait_for("skin chosen", |s| s["skin"] == json!("neon-2"))
        .await;
    assert!(
        ov.config_text().contains("skin = \"neon-2\""),
        "{}",
        ov.config_text()
    );

    // removing the chosen skin goes back to the built-in drawing
    let (status, _, _) = http(ov.port, "DELETE", "/skins/neon-2", &[], b"");
    assert_eq!(status, 204);
    ws.wait_for("skin cleared", |s| s["skin"] == json!(null))
        .await;
    let (_, _, body) = http(ov.port, "GET", "/skins", &[], b"");
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(&body).unwrap(),
        json!([])
    );
    let (status, _, _) = http(ov.port, "DELETE", "/skins/neon-2", &[], b"");
    assert_eq!(status, 404);
}

#[tokio::test(flavor = "multi_thread")]
async fn bad_skins_are_refused() {
    let ov = Overlay::start();
    let png = ("Content-Type", "image/png");
    // a name that would escape the skins folder
    let (status, _, _) = http(ov.port, "PUT", "/skins/..%2F..%2Foverlay", &[png], &RED_PNG);
    assert_eq!(status, 400);
    // not a PNG
    let (status, _, body) = http(ov.port, "PUT", "/skins/fake", &[png], b"GIF89a not really");
    assert_eq!(status, 400);
    assert!(String::from_utf8_lossy(&body).contains("isn't a PNG"));
    // too big
    let mut huge = RED_PNG.to_vec();
    huge.resize(pocket_overlay::skins::MAX_BYTES + 10, 0);
    let (status, _, _) = http(ov.port, "PUT", "/skins/huge", &[png], &huge);
    assert_eq!(status, 413);
    // a bad name chosen over the WebSocket is ignored, and the settings file is untouched
    let mut ws = Ws::connect(ov.port).await;
    ws.send_json(json!({ "cmd": "set_skin", "skin": "../../etc/passwd" }))
        .await;
    ws.send_json(json!({ "cmd": "set_mode", "mode": 1 })).await;
    let state = ws.wait_for("mode 1", |s| s["mode"] == json!(1)).await;
    assert_eq!(state["skin"], json!(null));
    let (_, _, body) = http(ov.port, "GET", "/skins", &[], b"");
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(&body).unwrap(),
        json!([])
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn other_websites_are_refused() {
    let ov = Overlay::start();
    let port = ov.port.to_string();
    let own = format!("http://127.0.0.1:{port}");
    let png = ("Content-Type", "image/png");

    // a page on another site in the same browser
    let (status, _, _) = http(
        ov.port,
        "PUT",
        "/skins/x",
        &[png, ("Origin", "http://evil.example")],
        &RED_PNG,
    );
    assert_eq!(status, 403);
    // DNS rebinding: another site's name pointed at 127.0.0.1
    let (status, _, _) = http(
        ov.port,
        "GET",
        "/state",
        &[("Host", &format!("evil.example:{port}"))],
        b"",
    );
    assert_eq!(status, 403);
    // the app's own pages are fine, by either local name
    let (status, _, _) = http(
        ov.port,
        "PUT",
        "/skins/x",
        &[png, ("Origin", &own)],
        &RED_PNG,
    );
    assert_eq!(status, 204);
    let local = format!("localhost:{port}");
    let (status, _, _) = http(ov.port, "GET", "/", &[("Host", &local)], b"");
    assert_eq!(status, 200);

    // WebSockets aren't covered by the browser's cross-site rules: the server checks itself
    let mut req = format!("ws://127.0.0.1:{port}/ws")
        .into_client_request()
        .unwrap();
    req.headers_mut()
        .insert("Origin", "http://evil.example".parse().unwrap());
    let err = tokio_tungstenite::connect_async(req).await.unwrap_err();
    assert!(err.to_string().contains("403"), "{err}");
    let mut req = format!("ws://127.0.0.1:{port}/ws")
        .into_client_request()
        .unwrap();
    req.headers_mut().insert("Origin", own.parse().unwrap());
    tokio_tungstenite::connect_async(req)
        .await
        .expect("own page connects");
}
