//! HTTP + WebSocket server for the overlay page.

use std::io;
use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use axum::body::Bytes;
use axum::extract::connect_info::{ConnectInfo, Connected};
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{DefaultBodyLimit, Path, Request, State};
use axum::http::{HeaderMap, Method, StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Json, Response};
use axum::routing::get;
use axum::serve::IncomingStream;
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{mpsc, watch};

use crate::engine::Command;
use crate::network;
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

/// Serves on `listener` (from [`network::listen`]) until the process ends. While `lan` is
/// on in the state, the port is open to other PCs, which may only watch.
pub async fn serve(
    listener: TcpListener,
    state: watch::Receiver<OverlayState>,
    commands: mpsc::Sender<Command>,
    skins: Skins,
) -> io::Result<()> {
    let port = listener.local_addr()?.port();
    let shared = !listener.local_addr()?.ip().is_loopback();
    let guard = Guard {
        port,
        names: network::names().into(),
        state: state.clone(),
    };
    let app = Router::new()
        .route("/", get(|| web_file("index.html")))
        .route("/traced.js", get(|| web_file("traced.js")))
        .route("/state", get(state_json))
        .route("/network", get(|| async { Json(network::address()) }))
        .route("/ws", get(ws))
        .route("/skins", get(list_skins))
        .route(
            "/skins/{name}",
            get(get_skin).put(put_skin).delete(delete_skin),
        )
        .layer(DefaultBodyLimit::max(skins::MAX_BYTES + 1))
        .layer(middleware::from_fn_with_state(guard, only_this_app))
        .with_state(AppState {
            state: state.clone(),
            commands: commands.clone(),
            skins,
        });
    let listener = Switching {
        inner: Some(listener),
        port,
        shared,
        wanted: lan_setting(state),
        failed: commands,
    };
    axum::serve(listener, app.into_make_service_with_connect_info::<Conn>()).await
}

/// Follows `lan` in the state.
fn lan_setting(mut state: watch::Receiver<OverlayState>) -> watch::Receiver<bool> {
    let (tx, rx) = watch::channel(state.borrow().lan);
    tokio::spawn(async move {
        while state.changed().await.is_ok() {
            let lan = state.borrow_and_update().lan;
            tx.send_if_modified(|old| std::mem::replace(old, lan) != lan);
        }
    });
    rx
}

/// Listens on 127.0.0.1, or on every address while other PCs may show the overlay,
/// switching on the same port when that setting changes. Open connections carry on.
struct Switching {
    /// `None` only while switching.
    inner: Option<TcpListener>,
    port: u16,
    shared: bool,
    wanted: watch::Receiver<bool>,
    failed: mpsc::Sender<Command>,
}

enum Event {
    Connection(io::Result<(TcpStream, SocketAddr)>),
    Setting(bool),
    SettingGone,
}

impl axum::serve::Listener for Switching {
    type Io = TcpStream;
    type Addr = SocketAddr;

    async fn accept(&mut self) -> (TcpStream, SocketAddr) {
        let mut watching = true;
        loop {
            let event = {
                let inner = self.inner.as_ref().expect("listening");
                tokio::select! {
                    accepted = inner.accept() => Event::Connection(accepted),
                    changed = self.wanted.changed(), if watching => match changed {
                        Ok(()) => Event::Setting(*self.wanted.borrow_and_update()),
                        Err(_) => Event::SettingGone,
                    },
                }
            };
            match event {
                Event::Connection(Ok(conn)) => return conn,
                // e.g. out of file handles: wait a little, like axum's own listener
                Event::Connection(Err(_)) => tokio::time::sleep(Duration::from_millis(50)).await,
                Event::Setting(shared) if shared != self.shared => self.switch(shared).await,
                Event::Setting(_) => {}
                Event::SettingGone => watching = false,
            }
        }
    }

    fn local_addr(&self) -> io::Result<SocketAddr> {
        self.inner.as_ref().expect("listening").local_addr()
    }
}

impl Switching {
    async fn switch(&mut self, shared: bool) {
        // a socket on every address can't share the port with the 127.0.0.1 one
        self.inner = None;
        match network::listen(self.port, shared).await {
            Ok(listener) => {
                self.inner = Some(listener);
                self.shared = shared;
                if shared {
                    print_lan_urls(self.port, true);
                } else {
                    eprintln!("Other PCs can no longer show the overlay.");
                }
            }
            Err(e) => {
                let reason = format!("Couldn't open port {} to other PCs: {e}", self.port);
                eprintln!("{reason}");
                let _ = self.failed.send(Command::LanFailed(reason)).await;
                self.shared = false;
                // the port was ours a moment ago; if something grabbed it, keep trying
                self.inner = Some(loop {
                    match network::listen(self.port, false).await {
                        Ok(listener) => break listener,
                        Err(_) => tokio::time::sleep(Duration::from_secs(1)).await,
                    }
                });
            }
        }
    }
}

/// Prints the OBS address for other PCs, and while they're shut out, how to let them in.
pub fn print_lan_urls(port: u16, shared: bool) {
    let network::Address { name, ip } = network::address();
    let urls: Vec<String> = [name, ip.map(|ip| ip.to_string())]
        .into_iter()
        .flatten()
        .map(|host| format!("http://{host}:{port}/"))
        .collect();
    if urls.is_empty() {
        eprintln!("  On another PC:       (no network found)");
        return;
    }
    eprintln!("  On another PC:       {}", urls.join("  or  "));
    if !shared {
        eprintln!(
            "                       (switch on \"Other PC\" in the settings first, or start with --lan)"
        );
    }
}

/// Both ends of a connection.
#[derive(Debug, Clone, Copy)]
pub struct Conn {
    /// This PC's address it came in on.
    local: SocketAddr,
    peer: SocketAddr,
}

impl Conn {
    /// From a browser or OBS on this PC. Only those may change anything: the settings
    /// page is at 127.0.0.1, and other PCs just show the overlay.
    fn this_pc(&self) -> bool {
        self.peer.ip().to_canonical().is_loopback()
    }
}

impl Connected<IncomingStream<'_, Switching>> for Conn {
    fn connect_info(stream: IncomingStream<'_, Switching>) -> Self {
        let peer = *stream.remote_addr();
        let local = stream.io().local_addr().unwrap_or(peer);
        Conn { local, peer }
    }
}

#[derive(Clone)]
struct Guard {
    port: u16,
    /// This PC's names, in lower case (see [`network::names`]).
    names: Arc<[String]>,
    state: watch::Receiver<OverlayState>,
}

async fn only_this_app(
    State(guard): State<Guard>,
    ConnectInfo(conn): ConnectInfo<Conn>,
    req: Request,
    next: Next,
) -> Response {
    let lan = guard.state.borrow().lan;
    let refuse = |why: &'static str| (StatusCode::FORBIDDEN, why).into_response();
    if !conn.this_pc() && !lan {
        // a connection kept open from before sharing was turned off
        return refuse("not shared with other PCs");
    }
    if !allowed(req.headers(), guard.port, &guard.names, conn, lan) {
        return refuse("only pages served by pocket-overlay");
    }
    if !conn.this_pc() && !matches!(*req.method(), Method::GET | Method::HEAD) {
        return refuse("other PCs can only show the overlay");
    }
    next.run(req).await
}

/// Only answers pages this app served itself: addressed to 127.0.0.1 or localhost, or,
/// while other PCs may show the overlay, to this PC's name or the address the request
/// came in on. Anything else is another website: one open in the same browser (they can
/// reach local ports, and WebSockets and uploads aren't covered by the browser's
/// cross-site rules), or a website's own name pointed at this PC (DNS rebinding).
fn allowed(headers: &HeaderMap, port: u16, names: &[String], conn: Conn, lan: bool) -> bool {
    let ours = |host: &str| {
        let Some(host) = host.strip_suffix(&format!(":{port}")) else {
            return false;
        };
        let literal = host
            .strip_prefix('[')
            .and_then(|h| h.strip_suffix(']'))
            .unwrap_or(host);
        match literal.parse::<IpAddr>() {
            Ok(ip) => {
                let ip = ip.to_canonical();
                ip.is_loopback() || (lan && ip == conn.local.ip().to_canonical())
            }
            Err(_) => {
                let host = host.to_ascii_lowercase();
                host == "localhost" || (lan && names.contains(&host))
            }
        }
    };
    let host_ok = headers
        .get(header::HOST)
        .and_then(|h| h.to_str().ok())
        .is_some_and(ours);
    let origin_ok = match headers.get(header::ORIGIN) {
        None => true, // not from a web page (OBS itself, curl, the tests)
        Some(o) => o
            .to_str()
            .ok()
            .and_then(|o| o.strip_prefix("http://"))
            .is_some_and(ours),
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

async fn ws(
    ws: WebSocketUpgrade,
    ConnectInfo(conn): ConnectInfo<Conn>,
    State(app): State<AppState>,
) -> impl IntoResponse {
    let watch_only = !conn.this_pc();
    ws.on_upgrade(move |socket| session(socket, app, watch_only))
}

/// Fastest a page is updated: ~120/s (~64/s on Windows, whose timers tick every 15.6 ms),
/// at least one per 60 Hz frame. The radio can report up to 1000 times a second; in
/// between, only the newest state is kept, never a queue.
pub const MIN_FRAME: Duration = Duration::from_millis(8);
/// A page that stops reading for this long is dropped (it reconnects by itself).
const SEND_TIMEOUT: Duration = Duration::from_secs(5);

/// Pushes state changes to the page and forwards its commands to the engine. Pages on
/// other PCs (`watch_only`) can't send commands, and are closed when sharing stops.
async fn session(mut socket: WebSocket, mut app: AppState, watch_only: bool) {
    loop {
        if watch_only && !app.state.borrow().lan {
            return;
        }
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
                        Ok(_) if watch_only => {}
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

#[cfg(test)]
mod tests {
    use super::*;

    const PORT: u16 = 7878;

    /// A request that came in on 192.168.1.20 from `peer`.
    fn conn(peer: &str) -> Conn {
        Conn {
            local: "192.168.1.20:7878".parse().unwrap(),
            peer: SocketAddr::new(peer.parse().unwrap(), 50000),
        }
    }

    fn check(host: &str, origin: Option<&str>, lan: bool) -> bool {
        let mut headers = HeaderMap::new();
        headers.insert(header::HOST, host.parse().unwrap());
        if let Some(origin) = origin {
            headers.insert(header::ORIGIN, origin.parse().unwrap());
        }
        let names = ["gaming-pc".to_owned(), "gaming-pc.local".to_owned()];
        allowed(&headers, PORT, &names, conn("192.168.1.30"), lan)
    }

    #[test]
    fn answers_this_pc_by_loopback_always() {
        for lan in [false, true] {
            for host in [
                "127.0.0.1:7878",
                "localhost:7878",
                "LOCALHOST:7878",
                "[::1]:7878",
            ] {
                assert!(check(host, None, lan), "{host}, lan {lan}");
            }
            assert!(check("127.0.0.1:7878", Some("http://127.0.0.1:7878"), lan));
        }
    }

    #[test]
    fn answers_its_name_and_address_only_while_shared() {
        for host in [
            "gaming-pc:7878",
            "GAMING-PC:7878",
            "gaming-pc.local:7878",
            "192.168.1.20:7878",
        ] {
            assert!(!check(host, None, false), "{host} while not shared");
            assert!(check(host, None, true), "{host} while shared");
            assert!(
                check(host, Some(&format!("http://{host}")), true),
                "{host} page"
            );
        }
    }

    #[test]
    fn refuses_other_websites() {
        for lan in [false, true] {
            // another site's name pointed at this PC (DNS rebinding)
            for host in [
                "evil.example:7878",
                "gaming-pc.evil.example:7878",
                "192.168.1.21:7878",
            ] {
                assert!(!check(host, None, lan), "{host}, lan {lan}");
            }
            // a page from another site, in the same browser or on the other PC
            for origin in ["http://evil.example", "https://gaming-pc:7878", "null"] {
                assert!(
                    !check("gaming-pc:7878", Some(origin), lan),
                    "{origin}, lan {lan}"
                );
            }
            // the right name on the wrong port is someone else's page
            assert!(!check("gaming-pc:80", None, lan));
            assert!(!check("127.0.0.1:80", None, lan));
        }
    }

    #[test]
    fn only_loopback_is_this_pc() {
        for peer in ["127.0.0.1", "::1", "::ffff:127.0.0.1"] {
            assert!(conn(peer).this_pc(), "{peer}");
        }
        // another PC, and this PC through its own network address (only the settings page
        // at 127.0.0.1 changes things)
        for peer in [
            "192.168.1.30",
            "::ffff:192.168.1.30",
            "fe80::1",
            "192.168.1.20",
        ] {
            assert!(!conn(peer).this_pc(), "{peer}");
        }
    }
}
