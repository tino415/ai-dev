use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

pub const HUMAN: &str = "human";

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Policy {
    Ask,
    Auto,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Preset {
    Bash,
    Iex,
    Psql,
    Mysql,
}

impl Preset {
    pub fn marker_template(self) -> &'static str {
        match self {
            Preset::Bash => "printf '%s%s\\n' '{head}' '{tail}'",
            Preset::Iex => "IO.puts(\"{head}\" <> \"{tail}\")",
            Preset::Psql => "; SELECT '{head}' || '{tail}';",
            Preset::Mysql => "SELECT CONCAT('{head}', '{tail}');",
        }
    }

    pub fn parse(name: &str) -> Option<Preset> {
        match name {
            "bash" => Some(Preset::Bash),
            "iex" => Some(Preset::Iex),
            "psql" => Some(Preset::Psql),
            "mysql" => Some(Preset::Mysql),
            _ => None,
        }
    }
}

pub const DEFAULT_IDLE_TIMEOUT_SECS: u64 = 30 * 60;
pub const DEFAULT_BLOCK_TIMEOUT_SECS: u64 = 120;

fn default_block_timeout() -> Option<u64> {
    Some(DEFAULT_BLOCK_TIMEOUT_SECS)
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SessionDef {
    pub name: String,
    pub start_command: String,
    pub setup_commands: Vec<String>,
    pub marker_template: String,
    pub profile: Option<String>,
    pub run_policy: Policy,
    pub read_policy: Policy,
    pub idle_timeout_secs: u64,
    #[serde(default = "default_block_timeout")]
    pub block_timeout_secs: Option<u64>,
}

impl SessionDef {
    pub fn from_preset(name: &str, preset: Preset, start_command: &str, setup: &[String]) -> Self {
        SessionDef {
            name: name.to_string(),
            start_command: start_command.to_string(),
            setup_commands: setup.to_vec(),
            marker_template: preset.marker_template().to_string(),
            profile: None,
            run_policy: Policy::Ask,
            read_policy: Policy::Ask,
            idle_timeout_secs: DEFAULT_IDLE_TIMEOUT_SECS,
            block_timeout_secs: default_block_timeout(),
        }
    }

    pub fn reads_a_block_as_one_command(&self) -> bool {
        self.marker_template == Preset::Bash.marker_template()
    }
}

#[derive(Clone, Debug)]
pub struct SessionProposal {
    pub id: String,
    pub by: String,
    pub notebook: String,
    pub why: String,
    pub def: SessionDef,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum BlockState {
    Pending,
    Saved,
    Queued,
    Running,
    Finished,
    Failed,
    Rejected,
    Deleted,
}

impl BlockState {
    pub fn is_dropped(self) -> bool {
        matches!(self, BlockState::Rejected | BlockState::Deleted)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum ReadDecision {
    Undecided,
    Released,
    Hidden,
}

#[derive(Clone, Debug)]
pub struct Block {
    pub id: String,
    pub notebook: String,
    pub session: String,
    pub code: String,
    pub author: String,
    pub why: Option<String>,
    pub state: BlockState,
    pub proposed_code: Option<String>,
    pub note: Option<String>,
    pub output: Option<String>,
    pub read: ReadDecision,
    pub session_stopped: bool,
    pub hidden_input_sent: bool,
    pub release_when_done: bool,
}

impl Block {
    pub fn new(notebook: &str, session: &str, code: &str, author: &str) -> Self {
        Block {
            id: short_id(),
            notebook: notebook.to_string(),
            session: session.to_string(),
            code: code.to_string(),
            author: author.to_string(),
            why: None,
            state: BlockState::Pending,
            proposed_code: None,
            note: None,
            output: None,
            read: ReadDecision::Undecided,
            session_stopped: false,
            hidden_input_sent: false,
            release_when_done: false,
        }
    }

    pub fn proposed_by_agent(&self) -> bool {
        self.author != HUMAN
    }

    pub fn has_the_text_of(&self, other: &Block) -> bool {
        (&self.code, &self.session, &self.why, &self.author)
            == (&other.code, &other.session, &other.why, &other.author)
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct NotebookToken {
    pub id: String,
    pub agent: String,
    pub hash: String,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Notebook {
    pub name: String,
    pub sessions: Vec<String>,
    pub profile: Option<String>,
    pub tokens: Vec<NotebookToken>,
}

#[derive(Clone, Debug)]
pub enum JoinStatus {
    Pending,
    Approved { token: String },
    Delivered,
}

#[derive(Clone, Debug)]
pub struct JoinRequest {
    pub id: String,
    pub notebook: String,
    pub agent: String,
    pub status: JoinStatus,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Variable {
    pub value: String,
    pub secret: bool,
}

pub type VariableSet = BTreeMap<String, Variable>;

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Variables {
    pub profiles: BTreeMap<String, VariableSet>,
    pub notebooks: BTreeMap<String, VariableSet>,
}

#[derive(Default, Serialize, Deserialize)]
pub struct Persisted {
    pub sessions: Vec<SessionDef>,
    pub notebooks: Vec<Notebook>,
    pub passkeys: Vec<webauthn_rs::prelude::Passkey>,
}

pub fn short_id() -> String {
    uuid::Uuid::new_v4().simple().to_string()[..10].to_string()
}

pub fn secret_token() -> String {
    format!(
        "{}{}",
        uuid::Uuid::new_v4().simple(),
        uuid::Uuid::new_v4().simple()
    )
}
