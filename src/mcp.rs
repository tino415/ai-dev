use std::sync::Arc;

use rmcp::handler::server::wrapper::Parameters;
use rmcp::{ServerHandler, tool, tool_handler, tool_router};
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::core::{Admission, Core, JoinOutcome};
use crate::model::*;

const PAGE_CHARS: usize = 20_000;

#[derive(Clone)]
pub struct Mcp {
    core: Arc<Core>,
}

impl Mcp {
    pub fn new(core: Arc<Core>) -> Self {
        Mcp { core }
    }

    fn admit(&self, token: &str) -> Result<Admission, String> {
        self.core
            .admit(token)
            .ok_or_else(|| "unknown or revoked notebook token, call join_notebook".to_string())
    }
}

fn reply(result: Result<Value, String>) -> String {
    match result {
        Ok(value) => serde_json::to_string_pretty(&value).unwrap_or_default(),
        Err(error) => json!({ "error": error }).to_string(),
    }
}

#[derive(Deserialize, JsonSchema)]
struct JoinArgs {
    /// Notebook to work in, from list_notebooks
    notebook: String,
    /// Your name, shown on every block you propose; a-z, 0-9, - and _
    agent: String,
}

#[derive(Deserialize, JsonSchema)]
struct TokenArgs {
    /// The notebook token from join_notebook
    token: String,
}

#[derive(Deserialize, JsonSchema)]
struct ProposeSessionArgs {
    /// The notebook token from join_notebook
    token: String,
    /// Session name; a-z, 0-9, - and _
    name: String,
    /// One of bash, iex, psql, mysql; fills the marker that ends a block
    preset: String,
    /// Command typed into the new terminal first, for example `kubectl exec -it pod -- iex`
    start_command: Option<String>,
    /// Commands run after the start command, one per line
    setup_commands: Option<Vec<String>>,
    /// Why you need this session, one sentence the owner reads before starting it
    why: String,
}

#[derive(Deserialize, JsonSchema)]
struct ProposeBlockArgs {
    /// The notebook token from join_notebook
    token: String,
    /// Session the block runs in, from list_sessions
    session: String,
    /// Code to type into the session; use {{name}} for variables from list_variables
    code: String,
    /// What you want to learn from this block and why, one sentence the owner reads before running it
    why: String,
}

#[derive(Deserialize, JsonSchema)]
struct ReadBlockArgs {
    /// The notebook token from join_notebook
    token: String,
    /// Block id returned by propose_block
    block_id: String,
    /// Output page, from 1
    page: Option<usize>,
}

#[tool_router]
impl Mcp {
    #[tool(
        description = "Ask to join a notebook. Returns a link for the owner to approve; call again after the approval to receive your notebook token."
    )]
    async fn join_notebook(&self, Parameters(args): Parameters<JoinArgs>) -> String {
        reply(self.core.request_join(&args.notebook, &args.agent).map(|outcome| match outcome {
            JoinOutcome::Pending { approve_link } => json!({
                "status": "waiting for the owner to approve",
                "approve_link": approve_link,
                "next": "tell the owner in your chat, then call join_notebook again once they say they approved",
            }),
            JoinOutcome::Token(token) => json!({ "status": "joined", "token": token }),
        }))
    }

    #[tool(description = "List notebooks you can ask to join.")]
    async fn list_notebooks(&self) -> String {
        let names: Vec<String> = self.core.notebooks().into_iter().map(|n| n.name).collect();
        reply(Ok(json!({ "notebooks": names })))
    }

    #[tool(
        description = "List sessions (terminals) blocks can run in, with their run and read policies."
    )]
    async fn list_sessions(&self) -> String {
        let sessions: Vec<Value> = self
            .core
            .sessions()
            .await
            .into_iter()
            .map(|(def, running)| {
                json!({
                    "name": def.name,
                    "running": running,
                    "run_policy": def.run_policy,
                    "read_policy": def.read_policy,
                    "idle_timeout_secs": def.idle_timeout_secs,
                })
            })
            .collect();
        reply(Ok(json!({ "sessions": sessions })))
    }

    #[tool(
        description = "List variables usable as {{name}} in blocks. Secret values are never shown; the tool fills them in when the block runs."
    )]
    async fn list_variables(&self, Parameters(args): Parameters<TokenArgs>) -> String {
        reply(self.admit(&args.token).map(|admission| {
            let variables: Vec<Value> = self
                .core
                .notebook_variables(&admission.notebook)
                .into_iter()
                .map(|(name, variable)| {
                    if variable.secret {
                        json!({ "name": name, "secret": true })
                    } else {
                        json!({ "name": name, "secret": false, "value": variable.value })
                    }
                })
                .collect();
            json!({ "variables": variables })
        }))
    }

    #[tool(
        description = "Propose a new session for the owner to start or reject. Returns a link to the proposal."
    )]
    async fn propose_session(&self, Parameters(args): Parameters<ProposeSessionArgs>) -> String {
        reply(self.admit(&args.token).and_then(|admission| {
            let preset =
                Preset::parse(&args.preset).ok_or("preset must be bash, iex, psql or mysql")?;
            let def = SessionDef::from_preset(
                &args.name,
                preset,
                args.start_command.as_deref().unwrap_or_default(),
                &args.setup_commands.unwrap_or_default(),
            );
            self.core
                .propose_session(&admission.notebook, &admission.agent, def, &args.why)?;
            Ok(json!({
                "status": "proposed, waiting for the owner to start it",
                "link": format!("{}/n/{}", self.core.origin(), admission.notebook),
            }))
        }))
    }

    #[tool(
        description = "Propose a block of code to run in a session. It runs only after the owner clicks Run (or the session's run policy is auto). Returns the block id and a link; tell the owner in your chat you are waiting, then end your turn."
    )]
    async fn propose_block(&self, Parameters(args): Parameters<ProposeBlockArgs>) -> String {
        reply(self.admit(&args.token).and_then(|admission| {
            let block = self.core.propose_block(
                &admission.notebook,
                &args.session,
                &args.code,
                &admission.agent,
                &args.why,
            )?;
            Ok(self.block_view(&block, 1))
        }))
    }

    #[tool(
        description = "Read a block's state. Output is returned only after the owner released it; large output comes in pages. If the owner edited your code or left a note, they are shown here."
    )]
    async fn read_block(&self, Parameters(args): Parameters<ReadBlockArgs>) -> String {
        reply(self.admit(&args.token).and_then(|admission| {
            let block = self
                .core
                .block(&args.block_id)
                .filter(|block| block.notebook == admission.notebook)
                .ok_or("unknown block in this notebook")?;
            Ok(self.block_view(&block, args.page.unwrap_or(1)))
        }))
    }
}

impl Mcp {
    fn block_view(&self, block: &Block, page: usize) -> Value {
        if block.state == BlockState::Deleted {
            return json!({ "block_id": block.id, "state": block.state, "output": "the owner deleted this block" });
        }
        let mut view = json!({
            "block_id": block.id,
            "link": self.core.block_link(block),
            "session": block.session,
            "state": block.state,
            "why": block.why,
            "code": block.code,
        });
        if let Some(original) = &block.proposed_code {
            view["edited_by_owner_from"] = json!(original);
        }
        if let Some(note) = &block.note {
            view["note"] = json!(note);
        }
        if block.hidden_input_sent {
            view["owner_sent_hidden_input"] = json!(
                "the owner typed hidden input, such as a password, into this block; its value is not shown"
            );
        }
        view["output"] = match (block.state, block.read, &block.output) {
            (BlockState::Queued | BlockState::Running, _, _) => Value::Null,
            (_, ReadDecision::Released, Some(output)) => paged(output, page),
            (_, ReadDecision::Hidden, Some(_)) => json!("the owner kept the output hidden"),
            (_, _, Some(_)) => json!("finished, waiting for the owner to release the output"),
            (_, _, None) => Value::Null,
        };
        view
    }
}

fn paged(output: &str, page: usize) -> Value {
    let chars: Vec<char> = output.chars().collect();
    let pages = chars.len().div_ceil(PAGE_CHARS).max(1);
    let page = page.clamp(1, pages);
    let text: String = chars
        .iter()
        .skip((page - 1) * PAGE_CHARS)
        .take(PAGE_CHARS)
        .collect();
    json!({ "page": page, "pages": pages, "text": text })
}

#[tool_handler]
impl ServerHandler for Mcp {}
