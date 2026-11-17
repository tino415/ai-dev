use std::sync::Arc;

use axum::extract::{Path, Request, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::middleware::{self, Next};
use axum::response::{Html, IntoResponse, Redirect, Response};
use axum::routing::{get, post};
use axum::{Form, Json, Router};
use serde::Deserialize;
use serde_json::{json, Value};
use webauthn_rs::prelude::{PublicKeyCredential, RegisterPublicKeyCredential};

use crate::auth::{Auth, PASSKEY_SCRIPT};
use crate::core::{Core, VariableScope};
use crate::html::{esc, page};
use crate::model::*;

#[derive(Clone)]
pub struct App {
    pub core: Arc<Core>,
    pub auth: Arc<Auth>,
}

type Fail = (StatusCode, String);
type Done = Result<Redirect, Fail>;

fn bad(error: String) -> Fail {
    (StatusCode::BAD_REQUEST, error)
}

pub fn router(app: App) -> Router {
    Router::new()
        .route("/", get(home))
        .route("/notebooks", post(create_notebook))
        .route("/n/{name}", get(notebook_page))
        .route("/n/{name}/blocks", get(blocks_fragment))
        .route("/n/{name}/block", post(add_block))
        .route("/n/{name}/profile", post(set_profile))
        .route("/n/{name}/var", post(set_notebook_var))
        .route("/n/{name}/var/delete", post(delete_notebook_var))
        .route("/n/{name}/tokens/{id}/revoke", post(revoke_token))
        .route("/b/{id}/run", post(run_block))
        .route("/b/{id}/reject", post(reject_block))
        .route("/b/{id}/release", post(release_block))
        .route("/b/{id}/hide", post(hide_block))
        .route("/joins/{id}/approve", post(approve_join))
        .route("/joins/{id}/deny", post(deny_join))
        .route("/sessions", get(sessions_page).post(create_session))
        .route("/sessions/{name}/start", post(start_session))
        .route("/sessions/{name}/stop", post(stop_session))
        .route("/sessions/{name}/delete", post(delete_session))
        .route("/sessions/{name}/policy", post(update_session))
        .route("/proposals/{id}/start", post(start_proposal))
        .route("/proposals/{id}/reject", post(reject_proposal))
        .route("/profiles", get(profiles_page).post(create_profile))
        .route("/profiles/{name}/var", post(set_profile_var))
        .route("/profiles/{name}/var/delete", post(delete_profile_var))
        .route("/login", get(login_page))
        .route("/enroll", get(enroll_page))
        .route("/logout", post(logout))
        .route("/auth/register/start", post(register_start))
        .route("/auth/register/finish", post(register_finish))
        .route("/auth/login/start", post(login_start))
        .route("/auth/login/finish", post(login_finish))
        .layer(middleware::from_fn_with_state(app.clone(), require_login))
        .with_state(app)
}

const PUBLIC_PATHS: [&str; 6] = [
    "/login",
    "/enroll",
    "/auth/register/start",
    "/auth/register/finish",
    "/auth/login/start",
    "/auth/login/finish",
];

async fn require_login(State(app): State<App>, request: Request, next: Next) -> Response {
    let public = PUBLIC_PATHS.contains(&request.uri().path());
    if public || app.auth.is_logged_in(request.headers()) {
        return next.run(request).await;
    }
    if request.method() == axum::http::Method::GET {
        Redirect::to("/login").into_response()
    } else {
        (StatusCode::UNAUTHORIZED, "sign in with your passkey").into_response()
    }
}

// notebooks

async fn home(State(app): State<App>) -> Html<String> {
    let rows: String = app
        .core
        .notebooks()
        .iter()
        .map(|n| format!("<li><a href=\"/n/{0}\">{0}</a></li>", esc(&n.name)))
        .collect();
    Html(page(
        "Notebooks",
        &format!(
            "<h1>Notebooks</h1><ul>{rows}</ul><form method=post action=/notebooks>\
<input name=name placeholder=\"new notebook\" required> <button>Create</button></form>"
        ),
        "",
    ))
}

#[derive(Deserialize)]
struct NameForm {
    name: String,
}

async fn create_notebook(State(app): State<App>, Form(form): Form<NameForm>) -> Done {
    app.core.create_notebook(&form.name).map_err(bad)?;
    Ok(Redirect::to(&format!("/n/{}", form.name)))
}

async fn notebook_page(State(app): State<App>, Path(name): Path<String>) -> Result<Html<String>, Fail> {
    let notebook = app.core.notebook(&name).ok_or((StatusCode::NOT_FOUND, "no such notebook".into()))?;
    let sessions = app.core.sessions().await;
    let session_options: String = sessions
        .iter()
        .map(|(def, _)| format!("<option>{0}</option>", esc(&def.name)))
        .collect();
    let profile_options: String = std::iter::once(String::new())
        .chain(app.core.variables().profiles.keys().cloned())
        .map(|p| {
            let selected = if Some(&p) == notebook.profile.as_ref() || (p.is_empty() && notebook.profile.is_none()) {
                " selected"
            } else {
                ""
            };
            format!("<option{selected}>{}</option>", esc(&p))
        })
        .collect();
    let joins: String = app
        .core
        .pending_joins(&name)
        .iter()
        .map(|j| {
            format!(
                "<div class=block id=\"join-{id}\"><b>{agent}</b> asks to join this notebook \
<form method=post style=display:inline action=\"/joins/{id}/approve\"><button>Admit</button></form>\
<form method=post style=display:inline action=\"/joins/{id}/deny\"><button>Deny</button></form></div>",
                id = j.id,
                agent = esc(&j.agent)
            )
        })
        .collect();
    let tokens: String = notebook
        .tokens
        .iter()
        .map(|t| {
            format!(
                "<li>{} <form method=post style=display:inline action=\"/n/{}/tokens/{}/revoke\"><button>Revoke</button></form></li>",
                esc(&t.agent),
                esc(&name),
                t.id
            )
        })
        .collect();
    let overrides: String = app
        .core
        .variables()
        .notebooks
        .get(&name)
        .map(|set| {
            set.iter()
                .map(|(var, v)| {
                    format!(
                        "<tr><td>{}</td><td>{}</td><td><form method=post action=\"/n/{}/var/delete\"><input type=hidden name=name value=\"{}\"><button>Remove</button></form></td></tr>",
                        esc(var),
                        if v.secret { "(secret)".to_string() } else { esc(&v.value) },
                        esc(&name),
                        esc(var)
                    )
                })
                .collect()
        })
        .unwrap_or_default();
    let body = format!(
        "<h1>{n}</h1>{joins}\
<h2>Blocks</h2><div id=blocks>{blocks}</div>\
<h3>Your own block</h3><form method=post action=\"/n/{n}/block\"><select name=session>{session_options}</select>\
<textarea name=code required></textarea><button>Add block</button></form>\
<h2>Variables</h2><form method=post action=\"/n/{n}/profile\">Profile <select name=profile>{profile_options}</select> <button>Set</button></form>\
<table>{overrides}</table>\
<form method=post action=\"/n/{n}/var\"><input name=name placeholder=name required> <input name=value placeholder=value> <label><input type=checkbox name=secret value=1> secret</label> <button>Override</button></form>\
<h2>Agents</h2><ul>{tokens}</ul>",
        n = esc(&name),
        blocks = render_blocks(&app.core, &name),
    );
    let script = format!(
        "setInterval(async()=>{{const box=document.getElementById('blocks');\
if(box.contains(document.activeElement)&&document.activeElement.matches('textarea,input'))return;\
const r=await fetch('/n/{}/blocks');if(r.ok)box.innerHTML=await r.text();}},3000);",
        esc(&name)
    );
    Ok(Html(page(&name, &body, &script)))
}

async fn blocks_fragment(State(app): State<App>, Path(name): Path<String>) -> Html<String> {
    Html(render_blocks(&app.core, &name))
}

fn render_blocks(core: &Core, notebook: &str) -> String {
    core.blocks(notebook)
        .iter()
        .filter(|block| block.state != BlockState::Rejected)
        .map(|block| render_block(core, block))
        .collect()
}

fn render_block(core: &Core, block: &Block) -> String {
    let state = format!("{:?}", block.state).to_lowercase();
    let mut html = format!(
        "<div class=\"block state-{state}\" id=\"b-{id}\"><div class=meta>{session} · {author} · {state}</div>",
        id = block.id,
        session = esc(&block.session),
        author = esc(&block.author)
    );
    if block.state == BlockState::Pending {
        html.push_str(&format!(
            "<form method=post action=\"/b/{id}/run\"><textarea name=code>{code}</textarea>\
<input name=note placeholder=\"note for the agent\" style=\"width:60%\"> \
<button>Run</button><button formaction=\"/b/{id}/reject\">Reject</button></form>",
            id = block.id,
            code = esc(&block.code)
        ));
    } else {
        html.push_str(&format!("<pre>{}</pre>", esc(&block.code)));
        if let Some(original) = &block.proposed_code {
            html.push_str(&format!("<div class=meta>edited from the proposal:</div><pre>{}</pre>", esc(original)));
        }
        if let Some(note) = &block.note {
            html.push_str(&format!("<div class=meta>note: {}</div>", esc(note)));
        }
    }
    if let Some(output) = &block.output {
        let class = if block.state == BlockState::Failed { " class=err" } else { "" };
        html.push_str(&format!("<pre{class}>{}</pre>", esc(output)));
        if block.proposed_by_agent() {
            html.push_str(&render_read_controls(core, block));
        }
    }
    html.push_str("</div>");
    html
}

fn render_read_controls(_core: &Core, block: &Block) -> String {
    match block.read {
        ReadDecision::Undecided => format!(
            "<form method=post style=display:inline action=\"/b/{id}/release\"><button>Release output to the agent</button></form>\
<form method=post style=display:inline action=\"/b/{id}/hide\"><button>Keep hidden</button></form>",
            id = block.id
        ),
        ReadDecision::Released => "<div class=meta>released to the agent</div>".to_string(),
        ReadDecision::Hidden => "<div class=meta>kept hidden from the agent</div>".to_string(),
    }
}

#[derive(Deserialize)]
struct AddBlockForm {
    session: String,
    code: String,
}

async fn add_block(State(app): State<App>, Path(name): Path<String>, Form(form): Form<AddBlockForm>) -> Done {
    app.core.add_block(&name, &form.session, &form.code, MARTIN).map_err(bad)?;
    Ok(Redirect::to(&format!("/n/{name}")))
}

#[derive(Deserialize)]
struct ProfileForm {
    profile: String,
}

async fn set_profile(State(app): State<App>, Path(name): Path<String>, Form(form): Form<ProfileForm>) -> Done {
    app.core.set_notebook_profile(&name, Some(form.profile)).map_err(bad)?;
    Ok(Redirect::to(&format!("/n/{name}")))
}

#[derive(Deserialize)]
struct VarForm {
    name: String,
    value: Option<String>,
    secret: Option<String>,
}

async fn set_notebook_var(State(app): State<App>, Path(name): Path<String>, Form(form): Form<VarForm>) -> Done {
    app.core
        .set_variable(
            VariableScope::Notebook(name.clone()),
            &form.name,
            &form.value.unwrap_or_default(),
            form.secret.is_some(),
        )
        .map_err(bad)?;
    Ok(Redirect::to(&format!("/n/{name}")))
}

async fn delete_notebook_var(State(app): State<App>, Path(name): Path<String>, Form(form): Form<VarForm>) -> Done {
    app.core
        .delete_variable(VariableScope::Notebook(name.clone()), &form.name)
        .map_err(bad)?;
    Ok(Redirect::to(&format!("/n/{name}")))
}

async fn revoke_token(State(app): State<App>, Path((name, id)): Path<(String, String)>) -> Done {
    app.core.revoke_token(&name, &id).map_err(bad)?;
    Ok(Redirect::to(&format!("/n/{name}")))
}

// blocks

#[derive(Deserialize)]
struct RunForm {
    code: Option<String>,
    note: Option<String>,
}

#[derive(Deserialize)]
struct RejectForm {
    note: Option<String>,
}

fn block_page(app: &App, id: &str) -> String {
    let notebook = app.core.block(id).map(|b| b.notebook).unwrap_or_default();
    format!("/n/{notebook}#b-{id}")
}

async fn run_block(State(app): State<App>, Path(id): Path<String>, Form(form): Form<RunForm>) -> Done {
    app.core.approve_run(&id, form.code, form.note).map_err(bad)?;
    Ok(Redirect::to(&block_page(&app, &id)))
}

async fn reject_block(State(app): State<App>, Path(id): Path<String>, Form(form): Form<RejectForm>) -> Done {
    app.core.reject_block(&id, form.note).map_err(bad)?;
    Ok(Redirect::to(&block_page(&app, &id)))
}

async fn release_block(State(app): State<App>, Path(id): Path<String>) -> Done {
    app.core.decide_read(&id, ReadDecision::Released).map_err(bad)?;
    Ok(Redirect::to(&block_page(&app, &id)))
}

async fn hide_block(State(app): State<App>, Path(id): Path<String>) -> Done {
    app.core.decide_read(&id, ReadDecision::Hidden).map_err(bad)?;
    Ok(Redirect::to(&block_page(&app, &id)))
}

async fn approve_join(State(app): State<App>, Path(id): Path<String>) -> Done {
    app.core.approve_join(&id).map_err(bad)?;
    Ok(Redirect::to("/"))
}

async fn deny_join(State(app): State<App>, Path(id): Path<String>) -> Done {
    app.core.deny_join(&id).map_err(bad)?;
    Ok(Redirect::to("/"))
}

// sessions

async fn sessions_page(State(app): State<App>) -> Html<String> {
    let socket = esc(app.core.tmux_socket());
    let rows: String = app
        .core
        .sessions()
        .await
        .iter()
        .map(|(def, running)| {
            let toggle = if *running { "stop" } else { "start" };
            let profile = def.profile.clone().unwrap_or_default();
            format!(
                "<div class=block><b>{n}</b> · {status} · attach: <code>tmux -L {socket} attach -t {n}</code>\
<form method=post action=\"/sessions/{n}/{toggle}\" style=display:inline><button>{toggle}</button></form>\
<form method=post action=\"/sessions/{n}/delete\" style=display:inline><button>delete</button></form>\
<form method=post action=\"/sessions/{n}/policy\">run <select name=run_policy>{run}</select> read <select name=read_policy>{read}</select> \
idle minutes <input name=idle_minutes size=4 value=\"{idle}\"> profile <input name=profile size=8 value=\"{profile}\"> <button>Save</button></form>\
<div class=meta>start: <code>{start}</code></div></div>",
                n = esc(&def.name),
                status = if *running { "running" } else { "stopped" },
                run = policy_options(def.run_policy),
                read = policy_options(def.read_policy),
                idle = def.idle_timeout_secs / 60,
                profile = esc(&profile),
                start = esc(&def.start_command),
            )
        })
        .collect();
    let proposals: String = app
        .core
        .proposals()
        .iter()
        .map(|p| {
            format!(
                "<div class=block><b>{n}</b> proposed by {by} in {nb}<pre>{start}\n{setup}</pre>\
<form method=post style=display:inline action=\"/proposals/{id}/start\"><button>Start</button></form>\
<form method=post style=display:inline action=\"/proposals/{id}/reject\"><button>Reject</button></form></div>",
                n = esc(&p.def.name),
                by = esc(&p.by),
                nb = esc(&p.notebook),
                start = esc(&p.def.start_command),
                setup = esc(&p.def.setup_commands.join("\n")),
                id = p.id
            )
        })
        .collect();
    let body = format!(
        "<h1>Sessions</h1>{proposals}{rows}<h2>New session</h2>\
<form method=post action=/sessions>\
<input name=name placeholder=name required> <select name=preset><option>bash</option><option>iex</option><option>psql</option><option>mysql</option></select><br>\
<input name=start_command placeholder=\"start command, e.g. kubectl exec -it pod -- iex\" style=\"width:100%\"><br>\
<textarea name=setup_commands placeholder=\"setup commands, one per line\"></textarea><button>Create</button></form>"
    );
    Html(page("Sessions", &body, ""))
}

fn policy_options(current: Policy) -> String {
    [Policy::Ask, Policy::Auto]
        .iter()
        .map(|policy| {
            let label = format!("{policy:?}").to_lowercase();
            let selected = if *policy == current { " selected" } else { "" };
            format!("<option{selected}>{label}</option>")
        })
        .collect()
}

#[derive(Deserialize)]
struct NewSessionForm {
    name: String,
    preset: String,
    start_command: Option<String>,
    setup_commands: Option<String>,
}

async fn create_session(State(app): State<App>, Form(form): Form<NewSessionForm>) -> Done {
    let preset = Preset::parse(&form.preset).ok_or_else(|| bad("unknown preset".into()))?;
    let setup: Vec<String> = form
        .setup_commands
        .unwrap_or_default()
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(str::to_string)
        .collect();
    let def = SessionDef::from_preset(&form.name, preset, form.start_command.as_deref().unwrap_or_default(), &setup);
    app.core.save_session(def).map_err(bad)?;
    Ok(Redirect::to("/sessions"))
}

async fn start_session(State(app): State<App>, Path(name): Path<String>) -> Done {
    app.core.start_session(&name).await.map_err(bad)?;
    Ok(Redirect::to("/sessions"))
}

async fn stop_session(State(app): State<App>, Path(name): Path<String>) -> Done {
    app.core.stop_session(&name).await;
    Ok(Redirect::to("/sessions"))
}

async fn delete_session(State(app): State<App>, Path(name): Path<String>) -> Done {
    app.core.delete_session(&name).await.map_err(bad)?;
    Ok(Redirect::to("/sessions"))
}

#[derive(Deserialize)]
struct PolicyForm {
    run_policy: String,
    read_policy: String,
    idle_minutes: u64,
    profile: Option<String>,
}

async fn update_session(State(app): State<App>, Path(name): Path<String>, Form(form): Form<PolicyForm>) -> Done {
    let mut def = app.core.session(&name).ok_or_else(|| bad("unknown session".into()))?;
    def.run_policy = parse_policy(&form.run_policy);
    def.read_policy = parse_policy(&form.read_policy);
    def.idle_timeout_secs = form.idle_minutes.max(1) * 60;
    def.profile = form.profile.filter(|p| !p.is_empty());
    app.core.save_session(def).map_err(bad)?;
    Ok(Redirect::to("/sessions"))
}

fn parse_policy(text: &str) -> Policy {
    if text == "auto" {
        Policy::Auto
    } else {
        Policy::Ask
    }
}

async fn start_proposal(State(app): State<App>, Path(id): Path<String>) -> Done {
    app.core.approve_proposal(&id).await.map_err(bad)?;
    Ok(Redirect::to("/sessions"))
}

async fn reject_proposal(State(app): State<App>, Path(id): Path<String>) -> Done {
    app.core.reject_proposal(&id).map_err(bad)?;
    Ok(Redirect::to("/sessions"))
}

// profiles

async fn profiles_page(State(app): State<App>) -> Html<String> {
    let sections: String = app
        .core
        .variables()
        .profiles
        .iter()
        .map(|(profile, set)| {
            let rows: String = set
                .iter()
                .map(|(name, v)| {
                    format!(
                        "<tr><td>{n}</td><td>{value}</td><td><form method=post action=\"/profiles/{p}/var/delete\"><input type=hidden name=name value=\"{n}\"><button>Remove</button></form></td></tr>",
                        n = esc(name),
                        p = esc(profile),
                        value = if v.secret { "(secret)".to_string() } else { esc(&v.value) }
                    )
                })
                .collect();
            format!(
                "<h2>{p}</h2><table>{rows}</table>\
<form method=post action=\"/profiles/{p}/var\"><input name=name placeholder=name required> <input name=value placeholder=value> \
<label><input type=checkbox name=secret value=1> secret</label> <button>Set</button></form>",
                p = esc(profile)
            )
        })
        .collect();
    Html(page(
        "Profiles",
        &format!(
            "<h1>Profiles</h1>{sections}<h2>New profile</h2><form method=post action=/profiles>\
<input name=name placeholder=\"prod\" required> <button>Create</button></form>"
        ),
        "",
    ))
}

async fn create_profile(State(app): State<App>, Form(form): Form<NameForm>) -> Done {
    app.core.create_profile(&form.name).map_err(bad)?;
    Ok(Redirect::to("/profiles"))
}

async fn set_profile_var(State(app): State<App>, Path(name): Path<String>, Form(form): Form<VarForm>) -> Done {
    app.core
        .set_variable(VariableScope::Profile(name), &form.name, &form.value.unwrap_or_default(), form.secret.is_some())
        .map_err(bad)?;
    Ok(Redirect::to("/profiles"))
}

async fn delete_profile_var(State(app): State<App>, Path(name): Path<String>, Form(form): Form<VarForm>) -> Done {
    app.core.delete_variable(VariableScope::Profile(name), &form.name).map_err(bad)?;
    Ok(Redirect::to("/profiles"))
}

// login

async fn login_page() -> Html<String> {
    Html(page(
        "Sign in",
        "<h1>ai-dev</h1><button id=go>Sign in with passkey</button> <span id=msg class=err></span>\
<p>First time? <a href=/enroll>Enroll a passkey</a> with a code from <code>ai-dev enroll</code>.</p>",
        &format!("{PASSKEY_SCRIPT}\ngo.onclick=()=>login().catch(e=>msg.textContent=e.message);"),
    ))
}

async fn enroll_page() -> Html<String> {
    Html(page(
        "Sign in",
        "<h1>Enroll a passkey</h1><input id=code placeholder=\"code from ai-dev enroll\"> <button id=go>Enroll</button> <span id=msg class=err></span>",
        &format!("{PASSKEY_SCRIPT}\ngo.onclick=()=>enroll(code.value).catch(e=>msg.textContent=e.message);"),
    ))
}

async fn logout(State(app): State<App>, headers: HeaderMap) -> impl IntoResponse {
    app.auth.close_session(&headers);
    Redirect::to("/login")
}

#[derive(Deserialize)]
struct CodeBody {
    code: Option<String>,
}

#[derive(Deserialize)]
struct FinishBody<T> {
    challenge_id: String,
    credential: T,
}

async fn register_start(State(app): State<App>, headers: HeaderMap, Json(body): Json<CodeBody>) -> Result<Json<Value>, Fail> {
    let (challenge_id, options) = app
        .auth
        .start_register(&headers, &body.code.unwrap_or_default(), &app.core.passkeys())
        .map_err(|e| (StatusCode::UNAUTHORIZED, e))?;
    Ok(Json(json!({ "challenge_id": challenge_id, "options": options })))
}

async fn register_finish(
    State(app): State<App>,
    Json(body): Json<FinishBody<RegisterPublicKeyCredential>>,
) -> Result<Response, Fail> {
    let (passkey, cookie) = app.auth.finish_register(&body.challenge_id, &body.credential).map_err(bad)?;
    app.core.add_passkey(passkey).map_err(bad)?;
    Ok(([(header::SET_COOKIE, cookie)], Json(json!({ "ok": true }))).into_response())
}

async fn login_start(State(app): State<App>) -> Result<Json<Value>, Fail> {
    let (challenge_id, options) = app
        .auth
        .start_login(&app.core.passkeys())
        .map_err(|e| (StatusCode::UNAUTHORIZED, e))?;
    Ok(Json(json!({ "challenge_id": challenge_id, "options": options })))
}

async fn login_finish(State(app): State<App>, Json(body): Json<FinishBody<PublicKeyCredential>>) -> Result<Response, Fail> {
    let (result, cookie) = app
        .auth
        .finish_login(&body.challenge_id, &body.credential)
        .map_err(|e| (StatusCode::UNAUTHORIZED, e))?;
    app.core.update_passkey_counter(&result).map_err(bad)?;
    Ok(([(header::SET_COOKIE, cookie)], Json(json!({ "ok": true }))).into_response())
}
