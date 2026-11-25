use std::sync::Arc;
use std::time::{Duration, Instant};

use ai_dev::auth::Auth;
use ai_dev::core::Core;
use ai_dev::model::*;
use ai_dev::tmux::strip_terminal_escapes;
use futures::{SinkExt, StreamExt};
use serde_json::{json, Value};
use tempfile::TempDir;
use tokio::net::TcpStream;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::{Error, Message};
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream};

const ORIGIN: &str = "http://localhost:7471";
const WAIT: Duration = Duration::from_secs(10);

type Socket = WebSocketStream<MaybeTlsStream<TcpStream>>;

struct Server {
    _dir: TempDir,
    address: String,
    core: Arc<Core>,
    cookie: String,
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = std::process::Command::new("tmux")
            .args(["-L", self.core.tmux_socket(), "kill-server"])
            .output();
    }
}

async fn server_with_a_shell_session() -> Server {
    let dir = tempfile::tempdir().unwrap();
    let core = Core::open(dir.path(), ORIGIN, &format!("aidev-test-{}", short_id())).unwrap();
    let auth = Arc::new(Auth::new(dir.path(), ORIGIN, &dir.path().join("agent_token")).unwrap());
    let cookie = auth.open_session().split(';').next().unwrap().to_string();
    core.create_notebook("nb").unwrap();
    core.save_session(SessionDef::from_preset("sh", Preset::Bash, "bash --norc", &[]))
        .unwrap();
    let app = ai_dev::app_router(Arc::clone(&core), auth, vec!["127.0.0.1".into(), "localhost".into()]);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap().to_string();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    Server { _dir: dir, address, core, cookie }
}

async fn server_with_a_running_shell() -> Server {
    let server = server_with_a_shell_session().await;
    server.core.start_session("sh").await.unwrap();
    server
}

impl Server {
    async fn open_terminal(&self, origin: &str, signed_in: bool, cols: u16, rows: u16) -> Result<Socket, Error> {
        let url = format!("ws://{}/sessions/sh/terminal/socket?cols={cols}&rows={rows}", self.address);
        let mut request = url.into_client_request().unwrap();
        request.headers_mut().insert("origin", origin.parse().unwrap());
        if signed_in {
            request.headers_mut().insert("cookie", self.cookie.parse().unwrap());
        }
        tokio_tungstenite::connect_async(request).await.map(|(socket, _)| socket)
    }

    async fn window(&self, format: &str) -> String {
        let output = tokio::process::Command::new("tmux")
            .args(["-L", self.core.tmux_socket(), "display", "-p", "-t", "=sh:", format])
            .output()
            .await
            .unwrap();
        String::from_utf8_lossy(&output.stdout).trim().to_string()
    }

    async fn wait_for_window(&self, format: &str, expected: &str) {
        let deadline = Instant::now() + WAIT;
        loop {
            let now = self.window(format).await;
            if now == expected {
                return;
            }
            assert!(Instant::now() < deadline, "window {format} stayed {now}, expected {expected}");
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }
}

fn handshake_status(result: Result<Socket, Error>) -> u16 {
    match result {
        Ok(_) => 101,
        Err(Error::Http(response)) => response.status().as_u16(),
        Err(other) => panic!("the handshake failed without an answer: {other}"),
    }
}

async fn screen_until(socket: &mut Socket, what: &str, shows: impl Fn(&str) -> bool) -> String {
    let deadline = tokio::time::Instant::now() + WAIT;
    let mut bytes = Vec::new();
    loop {
        let screen = strip_terminal_escapes(&String::from_utf8_lossy(&bytes));
        if shows(&screen) {
            return screen;
        }
        match tokio::time::timeout_at(deadline, socket.next()).await {
            Ok(Some(Ok(Message::Binary(chunk)))) => bytes.extend_from_slice(&chunk),
            Ok(Some(Ok(_))) => {}
            other => panic!("waited for {what}, got {other:?}; screen so far:\n{screen}"),
        }
    }
}

async fn next_state(socket: &mut Socket, what: &str, wanted: impl Fn(&Value) -> bool) -> Value {
    let deadline = tokio::time::Instant::now() + WAIT;
    loop {
        match tokio::time::timeout_at(deadline, socket.next()).await {
            Ok(Some(Ok(Message::Text(text)))) => {
                let state: Value = serde_json::from_str(&text).unwrap();
                if wanted(&state) {
                    return state;
                }
            }
            Ok(Some(Ok(_))) => {}
            other => panic!("waited for {what}, got {other:?}"),
        }
    }
}

fn printed_by_the_shell(screen: &str, text: &str) -> bool {
    screen.match_indices(text).any(|(at, _)| !screen[..at].ends_with("echo "))
}

#[tokio::test]
async fn typing_through_the_socket_reaches_the_shell_and_its_output_comes_back() {
    let server = server_with_a_running_shell().await;
    let mut socket = server.open_terminal(ORIGIN, true, 100, 30).await.unwrap();

    socket.send(Message::binary(b"echo t-ok\r".to_vec())).await.unwrap();

    screen_until(&mut socket, "t-ok printed by the shell", |screen| printed_by_the_shell(screen, "t-ok")).await;
}

#[tokio::test]
async fn a_handshake_from_another_origin_gets_no_switch() {
    let server = server_with_a_running_shell().await;

    let status = handshake_status(server.open_terminal("http://evil.example", true, 80, 24).await);

    assert_eq!(status, 403);
}

#[tokio::test]
async fn a_signed_out_handshake_gets_no_switch() {
    let server = server_with_a_running_shell().await;

    let status = handshake_status(server.open_terminal(ORIGIN, false, 80, 24).await);

    assert_ne!(status, 101);
}

#[tokio::test]
async fn a_stopped_session_gets_no_switch() {
    let server = server_with_a_shell_session().await;

    let status = handshake_status(server.open_terminal(ORIGIN, true, 80, 24).await);

    assert_eq!(status, 409);
}

#[tokio::test]
async fn the_session_fits_the_page_while_open_and_goes_back_to_220_by_50_after() {
    let server = server_with_a_running_shell().await;
    let mut socket = server.open_terminal(ORIGIN, true, 100, 30).await.unwrap();
    server.wait_for_window("#{window_width}", "100").await;

    socket
        .send(Message::text(json!({ "resize": { "cols": 50, "rows": 20 } }).to_string()))
        .await
        .unwrap();
    server.wait_for_window("#{window_width}", "50").await;

    socket.close(None).await.unwrap();
    server.wait_for_window("#{window_width}x#{window_height}", "220x50").await;

    let _second = server.open_terminal(ORIGIN, true, 60, 25).await.unwrap();
    server.wait_for_window("#{window_width}", "60").await;
}

#[tokio::test]
async fn the_page_hears_which_block_runs_and_when_the_session_stops() {
    let server = server_with_a_running_shell().await;
    let mut socket = server.open_terminal(ORIGIN, true, 80, 24).await.unwrap();
    next_state(&mut socket, "no block running", |state| state == &json!({ "running_block": null })).await;

    let block = server.core.add_block("nb", "sh", "sleep 3", HUMAN).unwrap();
    server.core.approve_run(&block.id, None, None).unwrap();
    next_state(&mut socket, "the block running", |state| state == &json!({ "running_block": block.id })).await;

    server.core.stop_session("sh").await;
    next_state(&mut socket, "the session stopped", |state| state == &json!({ "session": "stopped" })).await;
}
