use std::path::PathBuf;
use std::time::Duration;

use ai_dev::auth::Auth;
use ai_dev::core::Core;

const DEFAULT_LISTEN: &str = "127.0.0.1:7471";
const DEFAULT_TMUX_SOCKET: &str = "ai-dev";

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt::init();
    let result = match std::env::args().nth(1).as_deref() {
        Some("serve") => serve().await,
        Some("enroll") => print_enroll_code(),
        _ => Err("usage: ai-dev serve | enroll".to_string()),
    };
    if let Err(error) = result {
        eprintln!("{error}");
        std::process::exit(1);
    }
}

fn data_dir() -> PathBuf {
    std::env::var_os("AI_DEV_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            PathBuf::from(std::env::var_os("HOME").unwrap_or_default()).join(".local/share/ai-dev")
        })
}

fn origin() -> String {
    std::env::var("AI_DEV_ORIGIN").unwrap_or_else(|_| format!("http://localhost:{}", DEFAULT_LISTEN.rsplit(':').next().unwrap_or("7471")))
}

fn print_enroll_code() -> Result<(), String> {
    let dir = data_dir();
    ai_dev::core::create_private_dir(&dir)?;
    println!("{}", Auth::create_enroll_code(&dir)?);
    Ok(())
}

fn agent_token_file() -> PathBuf {
    std::env::var_os("AI_DEV_AGENT_TOKEN_FILE")
        .map(PathBuf::from)
        .unwrap_or_else(|| data_dir().join("agent_token"))
}

async fn serve() -> Result<(), String> {
    let dir = data_dir();
    let origin = origin();
    let socket = std::env::var("AI_DEV_TMUX_SOCKET").unwrap_or_else(|_| DEFAULT_TMUX_SOCKET.into());
    let core = Core::open(&dir, &origin, &socket)?;
    let auth = std::sync::Arc::new(Auth::new(&dir, &origin, &agent_token_file())?);
    let origin_host = origin
        .split("://")
        .nth(1)
        .unwrap_or_default()
        .split('/')
        .next()
        .unwrap_or_default()
        .to_string();
    let listen = std::env::var("AI_DEV_LISTEN").unwrap_or_else(|_| DEFAULT_LISTEN.into());
    let allowed_hosts = vec![origin_host, "localhost".into(), "127.0.0.1".into(), listen.clone()];
    let app = ai_dev::app_router(std::sync::Arc::clone(&core), auth, allowed_hosts);

    let reaper = std::sync::Arc::clone(&core);
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(Duration::from_secs(10)).await;
            reaper.reap_idle_sessions().await;
        }
    });

    let listener = tokio::net::TcpListener::bind(&listen).await.map_err(|e| e.to_string())?;
    tracing::info!("ai-dev listening on {listen}, data in {}", dir.display());
    axum::serve(listener, app).await.map_err(|e| e.to_string())
}
