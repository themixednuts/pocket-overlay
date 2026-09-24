//! HTTP + WebSocket server for the overlay page.

use std::net::SocketAddr;
use std::time::Duration;

use axum::Router;
use axum::body::Bytes;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{DefaultBodyLimit, Path, Request, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Json, Response};
use axum::routing::get;
use tokio::net::TcpListener;
use tokio::sync::{mpsc, watch};

use crate::engine::Command;
use crate::overlay::OverlayState;
use crate::skins::{self, SkinError, Skins};

/// Page files, embedded at build time. A `web/` folder in the working directory overrides
/// them so the page can be tweaked without rebuilding.
const WEB_FILES: &[(&str, &str, &str)] = &[
    (
        "index.html",
        "text/html; charset=utf-8",
        include_str!("../web/index.html"),
    ),
    (
        "traced.js",
        "text/javascript; charset=utf-8",
        include_str!("../web/traced.js"),
    ),
];

#[derive(Clone)]
struct AppState {
    state: watch::Receiver<OverlayState>,
    commands: mpsc::Sender<Command>,
    skins: Skins,
}

/// Binds (port 0 picks a free port) and returns the address actually bound.
pub async fn bind(port: u16) -> std::io::Result<(TcpListener, SocketAddr)> {
    let listener = TcpListener::bind(SocketAddr::from(([127, 0, 0, 1], port))).await?;
    let addr = listener.local_addr()?;
    Ok((listener, addr))
}

pub async fn serve(
    listener: TcpListener,
    state: watch::Receiver<OverlayState>,
    commands: mpsc::Sender<Command>,
    skins: Skins,
) -> std::io::Result<()> {
    let port = listener.local_addr()?.port();
    let app = Router::new()
        .route("/", get(|| web_file("index.html")))
        .route("/traced.js", get(|| web_file("traced.js")))
        .route("/state", get(state_json))
        .route("/ws", get(ws))
        .route("/skins", get(list_skins))
        .route(
            "/skins/{name}",
            get(get_skin).put(put_skin).delete(delete_skin),
        )
        .layer(DefaultBodyLimit::max(skins::MAX_BYTES + 1))
        .layer(middleware::from_fn_with_state(port, only_this_app))
        .with_state(AppState {
            state,
            commands,
            skins,
        });
    axum::serve(listener, app).await
}

/// Only answers pages this app served itself (OBS, or a browser tab on 127.0.0.1/localhost).
/// Other websites open in the same browser could otherwise reach the local port, and
/// WebSockets and uploads aren't protected by the browser's cross-site rules.
async fn only_this_app(State(port): State<u16>, req: Request, next: Next) -> Response {
    if allowed(req.headers(), port) {
        next.run(req).await
    } else {
        (StatusCode::FORBIDDEN, "only pages served by pocket-overlay").into_response()
    }
}

fn allowed(headers: &HeaderMap, port: u16) -> bool {
    let local =
        |host: &str| host == format!("127.0.0.1:{port}") || host == format!("localhost:{port}");
    let host_ok = headers
        .get(header::HOST)
        .and_then(|h| h.to_str().ok())
        .is_some_and(local);
    let origin_ok = match headers.get(header::ORIGIN) {
        None => true, // not from a web page (OBS itself, curl, the tests)
        Some(o) => o
            .to_str()
            .ok()
            .and_then(|o| o.strip_prefix("http://"))
            .is_some_and(local),
    };
    host_ok && origin_ok
}

async fn web_file(name: &'static str) -> impl IntoResponse {
    let (_, mime, embedded) = WEB_FILES
        .iter()
        .find(|(n, ..)| *n == name)
        .expect("known web file");
    let body = tokio::fs::read_to_string(format!("web/{name}"))
        .await
        .unwrap_or_else(|_| (*embedded).to_owned());
    (
        [
            (header::CONTENT_TYPE, *mime),
            (header::CACHE_CONTROL, "no-store"),
        ],
        body,
    )
}

async fn state_json(State(app): State<AppState>) -> Json<OverlayState> {
    Json(app.state.borrow().clone())
}

// ---------------------------------------------------------------------------------------
// skins

async fn list_skins(State(app): State<AppState>) -> Json<Vec<String>> {
    Json(app.skins.list())
}

async fn get_skin(State(app): State<AppState>, Path(name): Path<String>) -> Response {
    match app.skins.read(&name) {
        Some(png) => (
            [
                (header::CONTENT_TYPE, "image/png"),
                (header::CACHE_CONTROL, "no-store"),
            ],
            png,
        )
            .into_response(),
        None => (StatusCode::NOT_FOUND, "no such skin").into_response(),
    }
}

async fn put_skin(State(app): State<AppState>, Path(name): Path<String>, body: Bytes) -> Response {
    match app.skins.save(&name, &body) {
        Ok(()) => {
            let _ = app.commands.send(Command::SkinsChanged).await;
            StatusCode::NO_CONTENT.into_response()
        }
        Err(e) => skin_error(e),
    }
}

async fn delete_skin(State(app): State<AppState>, Path(name): Path<String>) -> Response {
    match app.skins.remove(&name) {
        Ok(true) => {
            if app.state.borrow().skin.as_deref() == Some(name.as_str()) {
                let _ = app.commands.send(Command::SetSkin { skin: None }).await;
            }
            let _ = app.commands.send(Command::SkinsChanged).await;
            StatusCode::NO_CONTENT.into_response()
        }
        Ok(false) => (StatusCode::NOT_FOUND, "no such skin").into_response(),
        Err(e) => skin_error(e),
    }
}

fn skin_error(e: SkinError) -> Response {
    let status = match e {
        SkinError::BadName | SkinError::NotPng => StatusCode::BAD_REQUEST,
        SkinError::TooBig => StatusCode::PAYLOAD_TOO_LARGE,
        SkinError::Io(_) => StatusCode::INTERNAL_SERVER_ERROR,
    };
    (status, e.to_string()).into_response()
}

// ---------------------------------------------------------------------------------------
// live state

async fn ws(ws: WebSocketUpgrade, State(app): State<AppState>) -> impl IntoResponse {
    ws.on_upgrade(move |socket| session(socket, app))
}

/// Fastest a page is updated: ~120/s (~64/s on Windows, whose timers tick every 15.6 ms),
/// at least one per 60 Hz frame. The radio can report up to 1000 times a second; in
/// between, only the newest state is kept, never a queue.
pub const MIN_FRAME: Duration = Duration::from_millis(8);
/// A page that stops reading for this long is dropped (it reconnects by itself).
const SEND_TIMEOUT: Duration = Duration::from_secs(5);

/// Pushes state changes to the page and forwards its commands to the engine.
async fn session(mut socket: WebSocket, mut app: AppState) {
    loop {
        let json =
            serde_json::to_string(&*app.state.borrow_and_update()).expect("state serializes");
        let sent_at = tokio::time::Instant::now();
        match tokio::time::timeout(SEND_TIMEOUT, socket.send(Message::Text(json.into()))).await {
            Ok(Ok(())) => {}
            _ => return,
        }
        loop {
            tokio::select! {
                changed = app.state.changed() => {
                    if changed.is_err() {
                        return;
                    }
                    break;
                }
                msg = socket.recv() => match msg {
                    Some(Ok(Message::Text(text))) => match serde_json::from_str::<Command>(&text) {
                        // internal notifications don't come from pages
                        Ok(Command::SkinsChanged) => {}
                        Ok(cmd) => {
                            let _ = app.commands.send(cmd).await;
                        }
                        Err(e) => eprintln!("ignoring bad command {text:?}: {e}"),
                    },
                    Some(Ok(_)) => {}
                    Some(Err(_)) | None => return,
                },
            }
        }
        // let a burst settle into one update per frame
        tokio::time::sleep_until(sent_at + MIN_FRAME).await;
    }
}
