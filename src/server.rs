//! HTTP + WebSocket server for the overlay page.

use std::net::SocketAddr;

use axum::Router;
use axum::extract::State;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::http::header;
use axum::response::{IntoResponse, Json};
use axum::routing::get;
use tokio::net::TcpListener;
use tokio::sync::{mpsc, watch};

use crate::engine::Command;
use crate::overlay::OverlayState;

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
) -> std::io::Result<()> {
    let app = Router::new()
        .route("/", get(|| web_file("index.html")))
        .route("/traced.js", get(|| web_file("traced.js")))
        .route("/state", get(state_json))
        .route("/ws", get(ws))
        .with_state(AppState { state, commands });
    axum::serve(listener, app).await
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

async fn ws(ws: WebSocketUpgrade, State(app): State<AppState>) -> impl IntoResponse {
    ws.on_upgrade(move |socket| session(socket, app))
}

/// Pushes every state change to the page and forwards its commands to the engine.
async fn session(mut socket: WebSocket, mut app: AppState) {
    loop {
        let json =
            serde_json::to_string(&*app.state.borrow_and_update()).expect("state serializes");
        if socket.send(Message::Text(json.into())).await.is_err() {
            return;
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
    }
}
