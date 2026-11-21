use std::convert::Infallible;
use std::future::Future;
use std::sync::Arc;

use axum::extract::{Path, Query, Request, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::middleware::{self, Next};
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{Html, IntoResponse, Redirect, Response};
use axum::routing::{get, post};
use axum::{Form, Json, Router};
use futures::{stream, Stream, StreamExt};
use serde::Deserialize;
use serde_json::{json, Value};
use tokio_stream::wrappers::errors::BroadcastStreamRecvError;
use tokio_stream::wrappers::BroadcastStream;
use webauthn_rs::prelude::{PublicKeyCredential, RegisterPublicKeyCredential};

use crate::auth::{Auth, PASSKEY_SCRIPT};
use crate::core::{Change, Core, VariableScope};
use crate::html::{esc, page, sign_in_page, HTMX, HTMX_SSE, PAGE_SCRIPT};
use crate::model::*;
use crate::notebook_view::{self, attach_line, policy_text, session_heading, toggle_label, Part};

#[derive(Clone)]
pub struct App {
    pub core: Arc<Core>,
    pub auth: Arc<Auth>,
}

type Fail = (StatusCode, String);
type Done = Result<Redirect, Fail>;
type Answer = Result<Response, Fail>;

fn bad(error: String) -> Fail {
    (StatusCode::BAD_REQUEST, error)
}

pub fn router(app: App) -> Router {
    Router::new()
        .route("/", get(home))
        .route("/notebooks", post(create_notebook))
        .route("/n/{name}", get(notebook_page))
        .route("/n/{name}/events", get(notebook_events))
        .route("/n/{name}/block", post(add_block))
        .route("/n/{name}/profile", post(set_profile))
        .route("/n/{name}/var", post(set_notebook_var))
        .route("/n/{name}/var/delete", post(delete_notebook_var))
        .route("/n/{name}/tokens/{id}/revoke", post(revoke_token))
        .route("/n/{name}/sessions", post(use_session))
        .route("/n/{name}/sessions/{session}/start", post(start_notebook_session))
        .route("/n/{name}/sessions/{session}/stop", post(stop_notebook_session))
        .route("/b/{id}/run", post(run_block))
        .route("/b/{id}/rerun", post(rerun_block))
        .route("/b/{id}/input", post(send_hidden_input))
        .route("/b/{id}/interrupt", post(interrupt_block))
        .route("/b/{id}/start-and-run", post(start_and_run_block))
        .route("/b/{id}/cancel-start", post(cancel_start))
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
        .route("/static/htmx-2.0.11.min.js", get(|| asset(SCRIPT, VERSIONED, HTMX.as_bytes())))
        .route("/static/htmx-ext-sse-2.2.4.js", get(|| asset(SCRIPT, VERSIONED, HTMX_SSE.as_bytes())))
        .route("/static/ai-dev.js", get(|| asset(SCRIPT, REVALIDATE, PAGE_SCRIPT.as_bytes())))
        .route("/static/manifest.webmanifest", get(|| asset("application/manifest+json", REVALIDATE, MANIFEST)))
        .route("/static/icon-192.png", get(|| asset("image/png", REVALIDATE, ICON_192)))
        .route("/static/icon-512.png", get(|| asset("image/png", REVALIDATE, ICON_512)))
        .route("/static/icon-maskable-512.png", get(|| asset("image/png", REVALIDATE, ICON_MASKABLE_512)))
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
    let path = request.uri().path();
    let public = PUBLIC_PATHS.contains(&path) || path.starts_with("/static/");
    if public || app.auth.is_logged_in(request.headers()) {
        return next.run(request).await;
    }
    if request.method() == axum::http::Method::GET {
        Redirect::to("/login").into_response()
    } else {
        (StatusCode::UNAUTHORIZED, "sign in with your passkey").into_response()
    }
}

const SCRIPT: &str = "text/javascript; charset=utf-8";
const VERSIONED: &str = "public, max-age=86400";
const REVALIDATE: &str = "no-cache";

const MANIFEST: &[u8] = include_bytes!("static/manifest.webmanifest");
const ICON_192: &[u8] = include_bytes!("static/icon-192.png");
const ICON_512: &[u8] = include_bytes!("static/icon-512.png");
const ICON_MASKABLE_512: &[u8] = include_bytes!("static/icon-maskable-512.png");

async fn asset(content_type: &'static str, cache_control: &'static str, body: &'static [u8]) -> impl IntoResponse {
    (
        [
            (header::CONTENT_TYPE, content_type),
            (header::CACHE_CONTROL, cache_control),
        ],
        body,
    )
}

fn from_htmx(headers: &HeaderMap) -> bool {
    headers.contains_key("hx-request")
}

async fn answer(app: &App, headers: &HeaderMap, notebook: &str, part: Part) -> Response {
    if !from_htmx(headers) {
        let anchor = match &part {
            Part::Block(id) => format!("#b-{id}"),
            _ => String::new(),
        };
        return Redirect::to(&format!("/n/{notebook}{anchor}")).into_response();
    }
    Html(notebook_view::render(&app.core, notebook, part).await).into_response()
}

async fn even_if_the_browser_leaves<T: Send + 'static>(
    work: impl Future<Output = Result<T, String>> + Send + 'static,
) -> Result<T, String> {
    tokio::spawn(work).await.map_err(|error| error.to_string())?
}

// notebooks

async fn home(State(app): State<App>) -> Html<String> {
    let rows: String = app
        .core
        .notebooks()
        .iter()
        .map(|n| format!("<div class=\"session\"><a href=\"/n/{0}\"><b>{0}</b></a></div>", esc(&n.name)))
        .collect();
    Html(page(
        "Notebooks",
        &format!(
            "<div class=\"panel\"><h2>Notebooks</h2>{rows}\
<form method=\"post\" action=\"/notebooks\" class=\"row\"><input name=\"name\" placeholder=\"new notebook\" required>\
<button data-working=\"Creating…\">Create</button></form></div>"
        ),
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
    existing_notebook(&app, &name)?;
    Ok(Html(crate::html::notebook_page(
        &name,
        &notebook_view::subtitle(&app.core, &name),
        &notebook_view::layout(&app.core, &name).await,
    )))
}

fn existing_notebook(app: &App, name: &str) -> Result<Notebook, Fail> {
    app.core.notebook(name).ok_or((StatusCode::NOT_FOUND, "no such notebook".into()))
}

#[derive(Deserialize)]
struct RenderedAt {
    #[serde(default)]
    since: u64,
}

async fn notebook_events(
    State(app): State<App>,
    Path(name): Path<String>,
    Query(rendered): Query<RenderedAt>,
) -> Result<Sse<impl Stream<Item = Result<Event, Infallible>>>, Fail> {
    existing_notebook(&app, &name)?;
    let core = Arc::clone(&app.core);
    let changes = BroadcastStream::new(core.subscribe());
    let missed_before_listening = if core.changes_so_far() > rendered.since {
        render_events(&core, &name, every_part(&core, &name)).await
    } else {
        Vec::new()
    };
    let events = changes
        .then(move |change| {
            let core = Arc::clone(&core);
            let name = name.clone();
            async move { events_for(&core, &name, change).await }
        })
        .flat_map(stream::iter);
    Ok(Sse::new(stream::iter(missed_before_listening).chain(events)).keep_alive(KeepAlive::default()))
}

async fn events_for(
    core: &Core,
    notebook: &str,
    change: Result<Change, BroadcastStreamRecvError>,
) -> Vec<Result<Event, Infallible>> {
    let parts = match change {
        Err(BroadcastStreamRecvError::Lagged(_)) => {
            return vec![Ok(Event::default().event("stale").data("missed changes"))];
        }
        Ok(Change::BlockAdded { notebook: n, id }) if n == notebook => vec![("block-added".to_string(), Part::Block(id))],
        Ok(Change::Block { notebook: n, id }) if n == notebook => vec![(format!("b-{id}"), Part::Block(id))],
        Ok(Change::Sessions) => vec![
            ("sessions".to_string(), Part::Sessions),
            ("composer".to_string(), Part::Composer),
            ("agents".to_string(), Part::Agents),
        ],
        Ok(Change::Proposals) => vec![("proposals".to_string(), Part::Proposals)],
        Ok(Change::Agents { notebook: n }) if n == notebook => vec![("agents".to_string(), Part::Agents)],
        Ok(Change::Variables) => vec![
            ("variables".to_string(), Part::Variables),
            ("agents".to_string(), Part::Agents),
        ],
        Ok(_) => vec![],
    };
    render_events(core, notebook, parts).await
}

fn every_part(core: &Core, notebook: &str) -> Vec<(String, Part)> {
    let mut parts = vec![
        ("sessions".to_string(), Part::Sessions),
        ("variables".to_string(), Part::Variables),
        ("agents".to_string(), Part::Agents),
        ("proposals".to_string(), Part::Proposals),
        ("composer".to_string(), Part::Composer),
    ];
    for block in core.blocks(notebook) {
        parts.push(("block-added".to_string(), Part::Block(block.id.clone())));
        parts.push((format!("b-{}", block.id), Part::Block(block.id)));
    }
    parts
}

async fn render_events(core: &Core, notebook: &str, parts: Vec<(String, Part)>) -> Vec<Result<Event, Infallible>> {
    let mut events = Vec::new();
    for (name, part) in parts {
        let html = notebook_view::render(core, notebook, part).await;
        events.push(Ok(Event::default().event(name).data(html)));
    }
    events
}

#[derive(Deserialize)]
struct AddBlockForm {
    session: String,
    code: String,
}

async fn add_block(
    State(app): State<App>,
    Path(name): Path<String>,
    headers: HeaderMap,
    Form(form): Form<AddBlockForm>,
) -> Answer {
    if form.code.trim().is_empty() {
        return Err(bad("type the code to run first".into()));
    }
    let block = app.core.add_block(&name, &form.session, &form.code, HUMAN).map_err(bad)?;
    app.core.approve_run(&block.id, None, None).map_err(bad)?;
    Ok(answer(&app, &headers, &name, Part::Composer).await)
}

#[derive(Deserialize)]
struct ProfileForm {
    profile: String,
}

async fn set_profile(
    State(app): State<App>,
    Path(name): Path<String>,
    headers: HeaderMap,
    Form(form): Form<ProfileForm>,
) -> Answer {
    app.core.set_notebook_profile(&name, Some(form.profile)).map_err(bad)?;
    Ok(answer(&app, &headers, &name, Part::Variables).await)
}

#[derive(Deserialize)]
struct VarForm {
    name: String,
    value: Option<String>,
    secret: Option<String>,
}

async fn set_notebook_var(
    State(app): State<App>,
    Path(name): Path<String>,
    headers: HeaderMap,
    Form(form): Form<VarForm>,
) -> Answer {
    app.core
        .set_variable(
            VariableScope::Notebook(name.clone()),
            &form.name,
            &form.value.unwrap_or_default(),
            form.secret.is_some(),
        )
        .map_err(bad)?;
    Ok(answer(&app, &headers, &name, Part::Variables).await)
}

async fn delete_notebook_var(
    State(app): State<App>,
    Path(name): Path<String>,
    headers: HeaderMap,
    Form(form): Form<VarForm>,
) -> Answer {
    app.core
        .delete_variable(VariableScope::Notebook(name.clone()), &form.name)
        .map_err(bad)?;
    Ok(answer(&app, &headers, &name, Part::Variables).await)
}

async fn revoke_token(State(app): State<App>, Path((name, id)): Path<(String, String)>, headers: HeaderMap) -> Answer {
    app.core.revoke_token(&name, &id).map_err(bad)?;
    Ok(answer(&app, &headers, &name, Part::Agents).await)
}

#[derive(Deserialize)]
struct SessionChoice {
    session: String,
}

async fn use_session(
    State(app): State<App>,
    Path(name): Path<String>,
    headers: HeaderMap,
    Form(form): Form<SessionChoice>,
) -> Answer {
    app.core.use_session(&name, &form.session).map_err(bad)?;
    Ok(answer(&app, &headers, &name, Part::Sessions).await)
}

async fn start_notebook_session(
    State(app): State<App>,
    Path((name, session)): Path<(String, String)>,
    headers: HeaderMap,
) -> Answer {
    existing_notebook(&app, &name)?;
    let core = Arc::clone(&app.core);
    even_if_the_browser_leaves(async move { core.start_session(&session).await })
        .await
        .map_err(bad)?;
    Ok(answer(&app, &headers, &name, Part::Sessions).await)
}

async fn stop_notebook_session(
    State(app): State<App>,
    Path((name, session)): Path<(String, String)>,
    headers: HeaderMap,
) -> Answer {
    existing_notebook(&app, &name)?;
    app.core.stop_session(&session).await;
    Ok(answer(&app, &headers, &name, Part::Sessions).await)
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

async fn answer_for_block(app: &App, headers: &HeaderMap, id: &str) -> Response {
    let notebook = app.core.block(id).map(|b| b.notebook).unwrap_or_default();
    answer(app, headers, &notebook, Part::Block(id.to_string())).await
}

async fn run_block(State(app): State<App>, Path(id): Path<String>, headers: HeaderMap, Form(form): Form<RunForm>) -> Answer {
    app.core.approve_run(&id, form.code, form.note).map_err(bad)?;
    Ok(answer_for_block(&app, &headers, &id).await)
}

async fn rerun_block(State(app): State<App>, Path(id): Path<String>, headers: HeaderMap) -> Answer {
    app.core.rerun_block(&id).map_err(bad)?;
    Ok(answer_for_block(&app, &headers, &id).await)
}

#[derive(Deserialize)]
struct HiddenInputForm {
    value: String,
}

async fn send_hidden_input(
    State(app): State<App>,
    Path(id): Path<String>,
    headers: HeaderMap,
    Form(form): Form<HiddenInputForm>,
) -> Answer {
    app.core.send_hidden_input(&id, &form.value).await.map_err(bad)?;
    Ok(answer_for_block(&app, &headers, &id).await)
}

async fn interrupt_block(State(app): State<App>, Path(id): Path<String>, headers: HeaderMap) -> Answer {
    app.core.interrupt_block(&id).await.map_err(bad)?;
    Ok(answer_for_block(&app, &headers, &id).await)
}

async fn start_and_run_block(State(app): State<App>, Path(id): Path<String>, headers: HeaderMap) -> Answer {
    let core = Arc::clone(&app.core);
    let block_id = id.clone();
    even_if_the_browser_leaves(async move { core.start_and_run(&block_id).await })
        .await
        .map_err(bad)?;
    Ok(answer_for_block(&app, &headers, &id).await)
}

async fn cancel_start(State(app): State<App>, Path(id): Path<String>, headers: HeaderMap) -> Answer {
    app.core.cancel_start(&id).map_err(bad)?;
    Ok(answer_for_block(&app, &headers, &id).await)
}

async fn reject_block(
    State(app): State<App>,
    Path(id): Path<String>,
    headers: HeaderMap,
    Form(form): Form<RejectForm>,
) -> Answer {
    app.core.reject_block(&id, form.note).map_err(bad)?;
    Ok(answer_for_block(&app, &headers, &id).await)
}

async fn release_block(State(app): State<App>, Path(id): Path<String>, headers: HeaderMap) -> Answer {
    app.core.decide_read(&id, ReadDecision::Released).map_err(bad)?;
    Ok(answer_for_block(&app, &headers, &id).await)
}

async fn hide_block(State(app): State<App>, Path(id): Path<String>, headers: HeaderMap) -> Answer {
    app.core.decide_read(&id, ReadDecision::Hidden).map_err(bad)?;
    Ok(answer_for_block(&app, &headers, &id).await)
}

async fn approve_join(State(app): State<App>, Path(id): Path<String>, headers: HeaderMap) -> Answer {
    let notebook = app.core.join_notebook_of(&id).ok_or_else(|| bad("unknown join request".into()))?;
    app.core.approve_join(&id).map_err(bad)?;
    Ok(answer(&app, &headers, &notebook, Part::Agents).await)
}

async fn deny_join(State(app): State<App>, Path(id): Path<String>, headers: HeaderMap) -> Answer {
    let notebook = app.core.join_notebook_of(&id).ok_or_else(|| bad("unknown join request".into()))?;
    app.core.deny_join(&id).map_err(bad)?;
    Ok(answer(&app, &headers, &notebook, Part::Agents).await)
}

// sessions

async fn sessions_page(State(app): State<App>) -> Html<String> {
    let rows: String = app
        .core
        .sessions()
        .await
        .iter()
        .map(|(def, running)| {
            let (toggle, working) = toggle_label(*running);
            format!(
                "<div class=\"session\"><div>{heading}\
<div class=\"policy\">{status} · {policy} · start: <code>{start}</code></div>{attach}\
<form method=\"post\" action=\"/sessions/{n}/policy\" class=\"row policy\">run <select name=\"run_policy\">{run}</select> \
read <select name=\"read_policy\">{read}</select> idle minutes <input name=\"idle_minutes\" size=\"4\" value=\"{idle}\"> \
profile <input name=\"profile\" size=\"8\" value=\"{profile}\"><button data-working=\"Saving…\">Save</button></form></div>\
<div class=\"row\"><form method=\"post\" action=\"/sessions/{n}/{toggle}\"><button data-working=\"{working}\">{toggle}</button></form>\
<form method=\"post\" action=\"/sessions/{n}/delete\"><button class=\"no\" data-working=\"Deleting…\">delete</button></form></div></div>",
                heading = session_heading(def, *running),
                attach = attach_line(&app.core, def, *running),
                n = esc(&def.name),
                status = if *running { "running" } else { "stopped" },
                policy = policy_text(def),
                start = esc(&def.start_command),
                run = policy_options(def.run_policy),
                read = policy_options(def.read_policy),
                idle = def.idle_timeout_secs / 60,
                profile = esc(&def.profile.clone().unwrap_or_default()),
            )
        })
        .collect();
    let proposals: String = app
        .core
        .proposals()
        .iter()
        .map(|p| {
            format!(
                "<div class=\"block agent\"><div class=\"bhead\"><span class=\"who\">{by}</span> → <b>new session {n}</b> in \
<a href=\"/n/{nb}\">{nb}</a><span class=\"state s-proposed\">proposed session</span></div><div class=\"why\">Why: {why}</div>\
<pre>{start}\n{setup}</pre><div class=\"actions\">\
<form method=\"post\" action=\"/proposals/{id}/start\"><button class=\"primary\" data-working=\"Starting…\">Start session</button></form>\
<form method=\"post\" action=\"/proposals/{id}/reject\"><button class=\"no\" data-working=\"Rejecting…\">Reject</button></form></div></div>",
                n = esc(&p.def.name),
                by = esc(&p.by),
                nb = esc(&p.notebook),
                why = esc(&p.why),
                start = esc(&p.def.start_command),
                setup = esc(&p.def.setup_commands.join("\n")),
                id = p.id
            )
        })
        .collect();
    let body = format!(
        "{proposals}<div class=\"panel\"><h2>Sessions</h2>{rows}</div>\
<div class=\"panel\"><h2>New session</h2><form method=\"post\" action=\"/sessions\" class=\"stack\">\
<div class=\"row\"><input name=\"name\" placeholder=\"name\" required> <select name=\"preset\"><option>bash</option><option>iex</option>\
<option>psql</option><option>mysql</option></select></div>\
<input name=\"start_command\" placeholder=\"start command, e.g. kubectl exec -it pod -- iex\">\
<textarea name=\"setup_commands\" placeholder=\"setup commands, one per line\"></textarea>\
<div><button class=\"ok\" data-working=\"Creating…\">Create</button></div></form></div>"
    );
    Html(page("Sessions", &body))
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
    if app.core.session(&def.name).is_some() {
        return Err(bad(format!("session {} already exists", def.name)));
    }
    app.core.save_session(def).map_err(bad)?;
    Ok(Redirect::to("/sessions"))
}

async fn start_session(State(app): State<App>, Path(name): Path<String>) -> Done {
    let core = Arc::clone(&app.core);
    even_if_the_browser_leaves(async move { core.start_session(&name).await })
        .await
        .map_err(bad)?;
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

fn proposal_notebook(app: &App, id: &str) -> Result<String, Fail> {
    app.core
        .proposals()
        .into_iter()
        .find(|p| p.id == id)
        .map(|p| p.notebook)
        .ok_or_else(|| bad("unknown proposal".into()))
}

async fn start_proposal(State(app): State<App>, Path(id): Path<String>, headers: HeaderMap) -> Answer {
    let notebook = proposal_notebook(&app, &id)?;
    let core = Arc::clone(&app.core);
    even_if_the_browser_leaves(async move { core.approve_proposal(&id).await })
        .await
        .map_err(bad)?;
    Ok(after_proposal(&app, &headers, &notebook).await)
}

async fn reject_proposal(State(app): State<App>, Path(id): Path<String>, headers: HeaderMap) -> Answer {
    let notebook = proposal_notebook(&app, &id)?;
    app.core.reject_proposal(&id).map_err(bad)?;
    Ok(after_proposal(&app, &headers, &notebook).await)
}

async fn after_proposal(app: &App, headers: &HeaderMap, notebook: &str) -> Response {
    if from_htmx(headers) {
        answer(app, headers, notebook, Part::Proposals).await
    } else {
        Redirect::to("/sessions").into_response()
    }
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
                        "<tr><td>{n}</td><td{class}>{value}</td><td><form method=\"post\" action=\"/profiles/{p}/var/delete\">\
<input type=\"hidden\" name=\"name\" value=\"{n}\"><button class=\"no\">remove</button></form></td></tr>",
                        n = esc(name),
                        p = esc(profile),
                        class = if v.secret { " class=\"secret\"" } else { "" },
                        value = if v.secret { "secret".to_string() } else { esc(&v.value) }
                    )
                })
                .collect();
            format!(
                "<div class=\"panel\"><h2>{p}</h2><table class=\"vars\">{rows}</table>\
<form method=\"post\" action=\"/profiles/{p}/var\" class=\"row\"><input name=\"name\" placeholder=\"name\" required>\
<input name=\"value\" placeholder=\"value\"><label class=\"policy\"><input type=\"checkbox\" name=\"secret\" value=\"1\"> secret</label>\
<button data-working=\"Saving…\">Set</button></form></div>",
                p = esc(profile)
            )
        })
        .collect();
    Html(page(
        "Profiles",
        &format!(
            "{sections}<div class=\"panel\"><h2>New profile</h2><form method=\"post\" action=\"/profiles\" class=\"row\">\
<input name=\"name\" placeholder=\"prod\" required><button data-working=\"Creating…\">Create</button></form></div>"
        ),
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
    Html(sign_in_page(
        "Sign in",
        "<div class=\"panel\"><h2>Sign in</h2><button id=\"go\" class=\"primary\">Sign in with passkey</button> \
<span id=\"msg\" class=\"err\"></span><p class=\"policy\">First time? <a href=\"/enroll\">Enroll a passkey</a> \
with a code from <code>ai-dev enroll</code>.</p></div>",
        &format!("{PASSKEY_SCRIPT}\ngo.onclick=()=>login().catch(e=>msg.textContent=e.message);"),
    ))
}

async fn enroll_page() -> Html<String> {
    Html(sign_in_page(
        "Enroll",
        "<div class=\"panel\"><h2>Enroll a passkey</h2><div class=\"row\"><input id=\"code\" placeholder=\"code from ai-dev enroll\">\
<button id=\"go\" class=\"primary\">Enroll</button></div><p id=\"msg\" class=\"err\"></p></div>",
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
