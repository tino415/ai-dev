use std::sync::Arc;

use ai_dev::auth::Auth;
use ai_dev::core::{Core, JoinOutcome};
use ai_dev::model::*;
use serde_json::{json, Value};
use tempfile::TempDir;

struct Server {
    _dir: TempDir,
    base: String,
    core: Arc<Core>,
    static_token: String,
    client: reqwest::Client,
}

async fn server() -> Server {
    let dir = tempfile::tempdir().unwrap();
    let origin = "http://localhost:7471";
    let core = Core::open(dir.path(), origin, &format!("aidev-test-{}", short_id())).unwrap();
    let token_file = dir.path().join("agent_token");
    let auth = Arc::new(Auth::new(dir.path(), origin, &token_file).unwrap());
    let static_token = "test-agent-token".to_string();
    std::fs::write(&token_file, &static_token).unwrap();
    let app = ai_dev::app_router(Arc::clone(&core), auth, vec!["127.0.0.1".into(), "localhost".into()]);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    Server {
        _dir: dir,
        base: format!("http://127.0.0.1:{port}"),
        core,
        static_token,
        client: reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .unwrap(),
    }
}

impl Server {
    async fn call_tool(&self, bearer: Option<&str>, name: &str, arguments: Value) -> (u16, Value) {
        let session = self.initialize(bearer).await;
        let mut request = self
            .client
            .post(format!("{}/mcp", self.base))
            .header("accept", "application/json, text/event-stream")
            .json(&json!({
                "jsonrpc": "2.0", "id": 2, "method": "tools/call",
                "params": { "name": name, "arguments": arguments }
            }));
        if let Some(token) = bearer {
            request = request.bearer_auth(token);
        }
        if let Some(session) = session {
            request = request.header("mcp-session-id", session);
        }
        let response = request.send().await.unwrap();
        let status = response.status().as_u16();
        let text = response.text().await.unwrap();
        let payload = text
            .lines()
            .find_map(|line| line.strip_prefix("data: ").filter(|data| data.starts_with('{')))
            .unwrap_or(&text);
        (status, serde_json::from_str(payload).unwrap_or(Value::String(text.clone())))
    }

    async fn initialize(&self, bearer: Option<&str>) -> Option<String> {
        let post = |body: Value, session: Option<&str>| {
            let mut request = self
                .client
                .post(format!("{}/mcp", self.base))
                .header("accept", "application/json, text/event-stream")
                .json(&body);
            if let Some(token) = bearer {
                request = request.bearer_auth(token);
            }
            if let Some(session) = session {
                request = request.header("mcp-session-id", session);
            }
            request
        };
        let response = post(
            json!({
                "jsonrpc": "2.0", "id": 1, "method": "initialize",
                "params": { "protocolVersion": "2025-06-18", "capabilities": {}, "clientInfo": { "name": "test", "version": "0" } }
            }),
            None,
        )
        .send()
        .await
        .unwrap();
        let session = response
            .headers()
            .get("mcp-session-id")
            .and_then(|v| v.to_str().ok())
            .map(str::to_string);
        if response.status().is_success() {
            post(json!({ "jsonrpc": "2.0", "method": "notifications/initialized" }), session.as_deref())
                .send()
                .await
                .unwrap();
        }
        session
    }

    fn tool_text(reply: &Value) -> Value {
        let text = reply["result"]["content"][0]["text"].as_str().unwrap_or_else(|| panic!("no tool text in {reply}"));
        serde_json::from_str(text).unwrap()
    }
}

#[tokio::test]
async fn approving_and_releasing_endpoints_refuse_a_request_without_a_passkey_session() {
    let s = server().await;
    for path in ["/b/x/run", "/b/x/reject", "/b/x/release", "/b/x/hide", "/joins/x/approve", "/proposals/x/start", "/sessions/x/start", "/n/x/tokens/y/revoke"] {
        let response = s.client.post(format!("{}{path}", s.base)).send().await.unwrap();
        assert_eq!(response.status(), 401, "{path}");
    }
    let page = s.client.get(format!("{}/n/anything", s.base)).send().await.unwrap();
    assert_eq!(page.status(), 303);
}

#[tokio::test]
async fn an_mcp_call_without_the_static_token_is_refused() {
    let s = server().await;
    let (status, _) = s.call_tool(None, "list_sessions", json!({})).await;
    assert_eq!(status, 401);
    let (status, _) = s.call_tool(Some("wrong"), "list_sessions", json!({})).await;
    assert_eq!(status, 401);
}

#[tokio::test]
async fn without_the_notebook_token_an_agent_cannot_propose_or_read() {
    let s = server().await;
    s.core.create_notebook("nb").unwrap();
    s.core
        .save_session(SessionDef::from_preset("sh", Preset::Bash, "", &[]))
        .unwrap();

    for (tool, args) in [
        ("propose_block", json!({ "token": "bogus", "session": "sh", "code": "ls", "why": "to list" })),
        ("read_block", json!({ "token": "bogus", "block_id": "x" })),
        ("list_variables", json!({ "token": "bogus" })),
        ("propose_session", json!({ "token": "bogus", "name": "s2", "preset": "bash", "why": "to compare" })),
    ] {
        let (status, reply) = s.call_tool(Some(&s.static_token), tool, args).await;
        assert_eq!(status, 200, "{tool}: {reply}");
        assert!(Server::tool_text(&reply)["error"].as_str().unwrap().contains("token"), "{tool}");
    }
    assert!(s.core.blocks("nb").is_empty());
}

#[tokio::test]
async fn joined_agent_proposes_and_reads_through_mcp_and_a_revoked_token_stops_working() {
    let s = server().await;
    s.core.create_notebook("nb").unwrap();
    s.core
        .save_session(SessionDef::from_preset("sh", Preset::Bash, "", &[]))
        .unwrap();

    let (_, join) = s
        .call_tool(Some(&s.static_token), "join_notebook", json!({ "notebook": "nb", "agent": "agent-a" }))
        .await;
    assert!(Server::tool_text(&join)["approve_link"].as_str().unwrap().contains("/n/nb#join-"));
    let join_id = s.core.pending_joins("nb")[0].id.clone();
    s.core.approve_join(&join_id).unwrap();
    let (_, joined) = s
        .call_tool(Some(&s.static_token), "join_notebook", json!({ "notebook": "nb", "agent": "agent-a" }))
        .await;
    let token = Server::tool_text(&joined)["token"].as_str().unwrap().to_string();

    let (_, proposed) = s
        .call_tool(
            Some(&s.static_token),
            "propose_block",
            json!({ "token": token, "session": "sh", "code": "ls", "why": "to see the files" }),
        )
        .await;
    let view = Server::tool_text(&proposed);
    assert_eq!(view["state"], "pending");
    assert_eq!(view["why"], "to see the files");
    assert_eq!(view["output"], Value::Null);
    let block_id = view["block_id"].as_str().unwrap().to_string();
    assert_eq!(s.core.block(&block_id).unwrap().author, "agent-a");

    let token_id = s.core.notebook("nb").unwrap().tokens[0].id.clone();
    s.core.revoke_token("nb", &token_id).unwrap();
    let (_, after) = s
        .call_tool(Some(&s.static_token), "read_block", json!({ "token": token, "block_id": block_id }))
        .await;
    assert!(Server::tool_text(&after)["error"].as_str().unwrap().contains("token"));
}

#[tokio::test]
async fn secret_values_never_reach_list_variables() {
    let s = server().await;
    s.core.create_notebook("nb").unwrap();
    s.core.create_profile("prod").unwrap();
    s.core
        .set_variable(ai_dev::core::VariableScope::Profile("prod".into()), "pw", "hunter2-secret", true)
        .unwrap();
    s.core
        .set_variable(ai_dev::core::VariableScope::Profile("prod".into()), "ns", "prod-ns", false)
        .unwrap();
    s.core.set_notebook_profile("nb", Some("prod".into())).unwrap();
    s.core.request_join("nb", "agent-a").unwrap();
    s.core.approve_join(&s.core.pending_joins("nb")[0].id).unwrap();
    let JoinOutcome::Token(token) = s.core.request_join("nb", "agent-a").unwrap() else { panic!() };

    let (_, reply) = s
        .call_tool(Some(&s.static_token), "list_variables", json!({ "token": token }))
        .await;

    let text = reply.to_string();
    assert!(!text.contains("hunter2-secret"));
    assert!(text.contains("prod-ns"));
}

#[tokio::test]
async fn an_mcp_call_needs_no_session_so_it_survives_a_restart() {
    let s = server().await;
    assert_eq!(s.initialize(Some(&s.static_token)).await, None);
    for session in [None, Some("from-before-the-restart")] {
        let mut request = s
            .client
            .post(format!("{}/mcp", s.base))
            .header("accept", "application/json, text/event-stream")
            .bearer_auth(&s.static_token)
            .json(&json!({
                "jsonrpc": "2.0", "id": 1, "method": "tools/call",
                "params": { "name": "list_notebooks", "arguments": {} }
            }));
        if let Some(session) = session {
            request = request.header("mcp-session-id", session);
        }
        let response = request.send().await.unwrap();
        assert_eq!(response.status(), 200, "{session:?}");
        let reply: Value = response.json().await.unwrap();
        assert!(reply["result"]["content"][0]["text"].is_string(), "{session:?}: {reply}");
    }
}

#[tokio::test]
async fn an_mcp_call_is_refused_while_the_token_file_is_missing() {
    let s = server().await;
    std::fs::remove_file(s._dir.path().join("agent_token")).unwrap();
    let (status, _) = s.call_tool(Some(&s.static_token), "list_sessions", json!({})).await;
    assert_eq!(status, 401);
}

#[tokio::test]
async fn the_manifest_and_its_icons_answer_without_a_login() {
    let s = server().await;
    let manifest = s.client.get(format!("{}/static/manifest.webmanifest", s.base)).send().await.unwrap();
    assert_eq!(manifest.status(), 200);
    assert_eq!(manifest.headers()["content-type"], "application/manifest+json");
    let manifest: Value = manifest.json().await.unwrap();
    let icons = manifest["icons"].as_array().unwrap();
    let sizes: Vec<&str> = icons.iter().map(|icon| icon["sizes"].as_str().unwrap()).collect();
    assert!(sizes.contains(&"192x192") && sizes.contains(&"512x512"), "{sizes:?}");
    for icon in icons {
        let response = s.client.get(format!("{}{}", s.base, icon["src"].as_str().unwrap())).send().await.unwrap();
        assert_eq!(response.status(), 200, "{icon}");
        assert_eq!(response.headers()["content-type"], "image/png");
        let png = response.bytes().await.unwrap();
        let (width, height) = (u32::from_be_bytes(png[16..20].try_into().unwrap()), u32::from_be_bytes(png[20..24].try_into().unwrap()));
        assert_eq!(format!("{width}x{height}"), icon["sizes"], "{icon}");
    }
}

#[tokio::test]
async fn the_login_page_links_the_manifest_and_icons() {
    let s = server().await;
    let page = s.client.get(format!("{}/login", s.base)).send().await.unwrap().text().await.unwrap();
    for tag in [
        "<link rel=\"manifest\" href=\"/static/manifest.webmanifest\">",
        "<meta name=\"theme-color\" content=\"#3b5bdb\" media=\"(prefers-color-scheme: light)\">",
        "<link rel=\"icon\" href=\"/static/icon-192.png\">",
        "<link rel=\"apple-touch-icon\" href=\"/static/icon-192.png\">",
    ] {
        assert!(page.contains(tag), "{tag}");
    }
}
