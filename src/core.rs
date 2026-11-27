use std::borrow::Cow;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use sha2::{Digest, Sha256};
use tokio::sync::broadcast;

use crate::model::*;
use crate::notebook_file;
use crate::tmux::{AttachedClient, TerminalSize, Tmux, strip_terminal_escapes};
use crate::vars;

pub type Result<T> = std::result::Result<T, String>;

const START_SETTLE: Duration = Duration::from_millis(500);
const OUTPUT_POLL: Duration = Duration::from_millis(100);
const LIVENESS_CHECK: Duration = Duration::from_secs(1);
const CHANGES_BUFFERED: usize = 256;
const HIDDEN_INPUT_SENT: &str = "‹hidden input sent›";
const HIDDEN_INPUT_PLACEHOLDER: char = '\u{E000}';
const SCREEN_NOT_KEPT: &str = "‹full-screen program, screen not kept›";
const SCREEN_NOW: &str = "‹full-screen program, its screen now›";
const CTRL_C_ECHO: &str = "^C";
const CTRL_C_ECHO_PLACEHOLDER: char = '\u{E001}';
const ALTERNATE_SCREEN_ON: &str = "\x1b[?1049h";
const ALTERNATE_SCREEN_OFF: &str = "\x1b[?1049l";
const PROMPT_SETTLE: Duration = Duration::from_millis(500);

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Change {
    BlockAdded { notebook: String, id: String },
    Block { notebook: String, id: String },
    Sessions,
    Proposals,
    Agents { notebook: String },
    Variables,
    NotebookLoaded { notebook: String },
}

pub struct Admission {
    pub notebook: String,
    pub agent: String,
}

pub enum JoinOutcome {
    Pending { approve_link: String },
    Token(String),
}

struct State {
    persisted: Persisted,
    vars: Variables,
    blocks: Vec<Block>,
    files: HashMap<String, NotebookFile>,
    last_activity: HashMap<String, Instant>,
    proposals: Vec<SessionProposal>,
    joins: Vec<JoinRequest>,
    running: HashMap<String, RunningBlock>,
}

#[derive(Default)]
struct NotebookFile {
    text_as_last_read_or_written: String,
    unloadable: Option<String>,
}

enum OnDisk {
    AsRemembered,
    LoadedAgain,
    Unloadable(String),
}

struct RunningBlock {
    session: String,
    log_offset: usize,
    end_marker_command: String,
    typing: Typing,
    hidden_inputs: Vec<HiddenInput>,
    interrupted_at: Vec<usize>,
}

#[derive(Clone, Copy, PartialEq)]
enum Typing {
    AsOneCommand,
    LineByLine,
}

#[derive(Clone)]
struct HiddenInput {
    sent_at: usize,
    value: String,
}

pub struct Core {
    dir: PathBuf,
    origin: String,
    tmux: Tmux,
    state: Mutex<State>,
    session_locks: Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>,
    changes: broadcast::Sender<Change>,
    changes_sent: AtomicU64,
}

struct Marker {
    head: String,
    tail: String,
}

impl Marker {
    fn new() -> Self {
        Marker {
            head: "AIDEV_".to_string(),
            tail: secret_token()[..16].to_string(),
        }
    }

    fn full(&self) -> String {
        format!("{}{}", self.head, self.tail)
    }

    fn command(&self, template: &str) -> String {
        template
            .replace("{head}", &self.head)
            .replace("{tail}", &self.tail)
    }

    fn block_as_one_command(&self, code: &str, typed: &str) -> String {
        let delimiter = format!("{}BLOCK_{}", self.head, self.tail);
        format!(
            "trap \"trap - URG; {typed}; kill -INT \\$\\$\" URG; eval \"$(cat <<'{delimiter}'\n{code}\n{delimiter}\n{}",
            last_line_of_one_command(typed)
        )
    }
}

fn last_line_of_one_command(typed: &str) -> String {
    format!(")\"; trap - URG; {typed}")
}

pub fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 40
        && name
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || c == '_')
}

pub fn hash_token(token: &str) -> String {
    hex::encode(Sha256::digest(token.as_bytes()))
}

impl Core {
    pub fn open(dir: &Path, origin: &str, tmux_socket: &str) -> Result<Arc<Core>> {
        create_private_dir(&dir.join("notebooks"))?;
        let mut persisted: Persisted = read_json(&dir.join("state.json"))?;
        for notebook in persisted.notebooks.iter_mut() {
            keep_the_latest_token_per_agent(notebook);
        }
        let vars: Variables = read_json(&dir.join("variables.json"))?;
        let core = Core {
            dir: dir.to_path_buf(),
            origin: origin.trim_end_matches('/').to_string(),
            tmux: Tmux::new(tmux_socket, &dir.join("runtime")),
            state: Mutex::new(State {
                persisted,
                vars,
                blocks: Vec::new(),
                files: HashMap::new(),
                last_activity: HashMap::new(),
                proposals: Vec::new(),
                joins: Vec::new(),
                running: HashMap::new(),
            }),
            session_locks: Mutex::new(HashMap::new()),
            changes: broadcast::channel(CHANGES_BUFFERED).0,
            changes_sent: AtomicU64::new(0),
        };
        let mut state = core.state();
        let names: Vec<String> = state
            .persisted
            .notebooks
            .iter()
            .map(|n| n.name.clone())
            .collect();
        for name in names {
            core.compare_with_disk(&mut state, &name);
        }
        drop(state);
        Ok(Arc::new(core))
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    pub fn origin(&self) -> &str {
        &self.origin
    }

    pub fn tmux_socket(&self) -> &str {
        self.tmux.socket()
    }

    pub fn block_link(&self, block: &Block) -> String {
        format!("{}/n/{}#b-{}", self.origin, block.notebook, block.id)
    }

    fn state(&self) -> MutexGuard<'_, State> {
        self.state.lock().expect("state lock")
    }

    pub fn subscribe(&self) -> broadcast::Receiver<Change> {
        self.changes.subscribe()
    }

    pub fn changes_so_far(&self) -> u64 {
        self.changes_sent.load(Ordering::SeqCst)
    }

    fn notify(&self, change: Change) {
        self.changes_sent.fetch_add(1, Ordering::SeqCst);
        let _ = self.changes.send(change);
    }

    pub fn notebook_file(&self, notebook: &str) -> PathBuf {
        notebook_path(&self.dir, notebook)
    }

    // notebooks

    pub fn create_notebook(&self, name: &str) -> Result<()> {
        if !valid_name(name) {
            return Err("names use a-z, 0-9, - and _".into());
        }
        let mut state = self.state();
        if state.persisted.notebooks.iter().any(|n| n.name == name) {
            return Err(format!("notebook {name} exists"));
        }
        state.persisted.notebooks.push(Notebook {
            name: name.to_string(),
            ..Notebook::default()
        });
        self.save_state(&state)?;
        if self.notebook_file(name).exists() {
            self.compare_with_disk(&mut state, name);
            return Ok(());
        }
        self.save_notebook(&mut state, name)
    }

    pub fn load_notebook_file_if_changed(&self, notebook: &str) {
        self.compare_with_disk(&mut self.state(), notebook);
    }

    pub fn notebook_file_error(&self, notebook: &str) -> Option<String> {
        self.state()
            .files
            .get(notebook)
            .and_then(|file| file.unloadable.clone())
    }

    pub fn notebooks(&self) -> Vec<Notebook> {
        self.state().persisted.notebooks.clone()
    }

    pub fn notebook(&self, name: &str) -> Option<Notebook> {
        self.state()
            .persisted
            .notebooks
            .iter()
            .find(|n| n.name == name)
            .cloned()
    }

    pub fn set_notebook_profile(&self, name: &str, profile: Option<String>) -> Result<()> {
        let mut state = self.state();
        let notebook = notebook_mut(&mut state, name)?;
        notebook.profile = profile.filter(|p| !p.is_empty());
        self.save_state(&state)?;
        self.notify(Change::Variables);
        Ok(())
    }

    pub fn use_session(&self, notebook: &str, session: &str) -> Result<()> {
        let mut state = self.state();
        if !state.persisted.sessions.iter().any(|s| s.name == session) {
            return Err("unknown session".into());
        }
        let entry = notebook_mut(&mut state, notebook)?;
        if !entry.sessions.iter().any(|s| s == session) {
            entry.sessions.push(session.to_string());
        }
        self.save_state(&state)?;
        self.notify(Change::Sessions);
        Ok(())
    }

    // sessions

    pub fn session_defs(&self) -> Vec<SessionDef> {
        self.state().persisted.sessions.clone()
    }

    pub async fn sessions(&self) -> Vec<(SessionDef, bool)> {
        let mut listed = Vec::new();
        for def in self.session_defs() {
            let running = self.is_running(&def.name).await;
            listed.push((def, running));
        }
        listed
    }

    pub fn session(&self, name: &str) -> Option<SessionDef> {
        self.state()
            .persisted
            .sessions
            .iter()
            .find(|s| s.name == name)
            .cloned()
    }

    pub fn save_session(&self, def: SessionDef) -> Result<()> {
        if !valid_name(&def.name) {
            return Err("names use a-z, 0-9, - and _".into());
        }
        let mut state = self.state();
        let blocks_whose_buttons_follow_the_policy: Vec<(String, String)> = state
            .blocks
            .iter()
            .filter(|b| b.session == def.name && b.proposed_by_agent() && !b.state.is_dropped())
            .map(|b| (b.notebook.clone(), b.id.clone()))
            .collect();
        match state
            .persisted
            .sessions
            .iter_mut()
            .find(|s| s.name == def.name)
        {
            Some(existing) => *existing = def,
            None => state.persisted.sessions.push(def),
        }
        self.save_state(&state)?;
        drop(state);
        self.notify(Change::Sessions);
        for (notebook, id) in blocks_whose_buttons_follow_the_policy {
            self.notify_block(&notebook, &id);
        }
        Ok(())
    }

    pub async fn delete_session(&self, name: &str) -> Result<()> {
        self.stop_session(name).await;
        let mut state = self.state();
        state.persisted.sessions.retain(|s| s.name != name);
        self.save_state(&state)?;
        self.notify(Change::Sessions);
        Ok(())
    }

    pub async fn is_running(&self, name: &str) -> bool {
        self.tmux.is_alive(name).await
    }

    pub async fn start_session(&self, name: &str) -> Result<()> {
        let def = self.session(name).ok_or("unknown session")?;
        let lock = self.session_lock(name);
        let _guard = lock.lock().await;
        if self.is_running(name).await {
            self.touch(name);
            return Ok(());
        }
        self.tmux.start(name).await?;
        self.touch(name);
        let set = self.effective_variables(def.profile.as_deref(), "");
        let started = match self.run_start_sequence(&def, &set).await {
            Ok(()) => self.outlive_start(name).await,
            failed => failed,
        };
        if let Err(error) = started {
            let error = match self.tmux.dead_pane_output(name).await {
                Some(last_lines) => format!("session exited at start\n{last_lines}"),
                None => error,
            };
            self.stop_session(name).await;
            return Err(vars::mask(&error, &set));
        }
        self.notify(Change::Sessions);
        Ok(())
    }

    async fn run_start_sequence(&self, def: &SessionDef, set: &VariableSet) -> Result<()> {
        if !def.start_command.is_empty() {
            let command = vars::render(&def.start_command, set)?;
            self.tmux.type_lines(&def.name, &command).await?;
        }
        for setup in &def.setup_commands {
            self.run_text(None, def, Typing::LineByLine, set, setup)
                .await
                .map_err(|error| format!("setup failed: {error}"))?;
        }
        Ok(())
    }

    async fn outlive_start(&self, name: &str) -> Result<()> {
        tokio::time::sleep(START_SETTLE).await;
        if self.is_running(name).await {
            Ok(())
        } else {
            Err("session exited at start".into())
        }
    }

    pub async fn stop_session(&self, name: &str) {
        self.tmux.stop(name).await;
        self.state().last_activity.remove(name);
        self.notify(Change::Sessions);
    }

    pub async fn attach_terminal(
        &self,
        name: &str,
        size: TerminalSize,
    ) -> Result<(pty_process::Pty, AttachedClient)> {
        if !self.is_running(name).await {
            return Err(format!("session {name} is not running"));
        }
        self.touch(name);
        self.tmux.attach_client(name, size)
    }

    pub async fn detach_terminal(&self, name: &str, client: AttachedClient) {
        self.tmux.detach_client(name, client).await;
        self.touch(name);
    }

    pub async fn restore_detached_sizes(&self) {
        for def in self.session_defs() {
            if self.is_running(&def.name).await {
                self.tmux.restore_size(&def.name).await;
            }
        }
    }

    pub fn running_block_in(&self, session: &str) -> Option<String> {
        self.state()
            .blocks
            .iter()
            .find(|block| block.session == session && block.state == BlockState::Running)
            .map(|block| block.id.clone())
    }

    pub async fn reap_idle_sessions(&self) {
        let now = Instant::now();
        for def in self.session_defs() {
            let lock = self.session_lock(&def.name);
            let Ok(_busy_starting_or_running) = lock.try_lock() else {
                continue;
            };
            if !self.tmux.is_alive(&def.name).await {
                let was_running = self.state().last_activity.remove(&def.name).is_some();
                if was_running {
                    self.stop_session(&def.name).await;
                }
                continue;
            }
            let last = *self
                .state()
                .last_activity
                .entry(def.name.clone())
                .or_insert(now);
            if self.tmux.has_attached_client(&def.name).await {
                self.touch(&def.name);
            } else if now.duration_since(last) >= Duration::from_secs(def.idle_timeout_secs) {
                self.stop_session(&def.name).await;
            }
        }
    }

    pub fn propose_session(
        &self,
        notebook: &str,
        agent: &str,
        def: SessionDef,
        why: &str,
    ) -> Result<SessionProposal> {
        if !valid_name(&def.name) {
            return Err("names use a-z, 0-9, - and _".into());
        }
        if self.session(&def.name).is_some() {
            return Err(format!("session {} already exists", def.name));
        }
        let proposal = SessionProposal {
            id: short_id(),
            by: agent.to_string(),
            notebook: notebook.to_string(),
            why: required_why(why)?,
            def,
        };
        self.state().proposals.push(proposal.clone());
        self.notify(Change::Proposals);
        Ok(proposal)
    }

    pub fn proposals(&self) -> Vec<SessionProposal> {
        self.state().proposals.clone()
    }

    pub fn notebook_proposals(&self, notebook: &str) -> Vec<SessionProposal> {
        self.state()
            .proposals
            .iter()
            .filter(|p| p.notebook == notebook)
            .cloned()
            .collect()
    }

    pub async fn approve_proposal(&self, id: &str) -> Result<()> {
        let proposal = self.take_proposal(id)?;
        let name = proposal.def.name.clone();
        if self.session(&name).is_some() {
            return Err(format!("session {name} already exists"));
        }
        self.save_session(proposal.def)?;
        self.use_session(&proposal.notebook, &name)?;
        self.start_session(&name).await
    }

    pub fn reject_proposal(&self, id: &str) -> Result<()> {
        self.take_proposal(id).map(|_| ())
    }

    fn take_proposal(&self, id: &str) -> Result<SessionProposal> {
        let mut state = self.state();
        let position = state
            .proposals
            .iter()
            .position(|p| p.id == id)
            .ok_or("unknown proposal")?;
        let proposal = state.proposals.remove(position);
        self.notify(Change::Proposals);
        Ok(proposal)
    }

    // variables

    pub fn variables(&self) -> Variables {
        self.state().vars.clone()
    }

    pub fn effective_variables(&self, profile: Option<&str>, notebook: &str) -> VariableSet {
        vars::effective(&self.state().vars, profile, notebook)
    }

    pub fn notebook_variables(&self, notebook: &str) -> VariableSet {
        let profile = self.notebook(notebook).and_then(|n| n.profile);
        self.effective_variables(profile.as_deref(), notebook)
    }

    pub fn create_profile(&self, name: &str) -> Result<()> {
        if !valid_name(name) {
            return Err("names use a-z, 0-9, - and _".into());
        }
        let mut state = self.state();
        state.vars.profiles.entry(name.to_string()).or_default();
        self.save_vars(&state)
    }

    pub fn set_variable(
        &self,
        scope: VariableScope,
        name: &str,
        value: &str,
        secret: bool,
    ) -> Result<()> {
        let mut state = self.state();
        let set = match &scope {
            VariableScope::Profile(profile) => {
                state.vars.profiles.entry(profile.clone()).or_default()
            }
            VariableScope::Notebook(notebook) => {
                state.vars.notebooks.entry(notebook.clone()).or_default()
            }
        };
        set.insert(
            name.to_string(),
            Variable {
                value: value.to_string(),
                secret,
            },
        );
        self.save_vars(&state)
    }

    pub fn delete_variable(&self, scope: VariableScope, name: &str) -> Result<()> {
        let mut state = self.state();
        let set = match &scope {
            VariableScope::Profile(profile) => state.vars.profiles.get_mut(profile),
            VariableScope::Notebook(notebook) => state.vars.notebooks.get_mut(notebook),
        };
        if let Some(set) = set {
            set.remove(name);
        }
        self.save_vars(&state)
    }

    // blocks

    pub fn blocks(&self, notebook: &str) -> Vec<Block> {
        self.state()
            .blocks
            .iter()
            .filter(|b| b.notebook == notebook)
            .cloned()
            .collect()
    }

    pub fn block(&self, id: &str) -> Option<Block> {
        self.state().blocks.iter().find(|b| b.id == id).cloned()
    }

    pub fn add_block(
        self: &Arc<Self>,
        notebook: &str,
        session: &str,
        code: &str,
        author: &str,
    ) -> Result<Block> {
        self.insert_block(notebook, session, code, author, None)
    }

    pub fn propose_block(
        self: &Arc<Self>,
        notebook: &str,
        session: &str,
        code: &str,
        agent: &str,
        why: &str,
    ) -> Result<Block> {
        let why = required_why(why)?;
        self.insert_block(notebook, session, code, agent, Some(why))
    }

    fn insert_block(
        self: &Arc<Self>,
        notebook: &str,
        session: &str,
        code: &str,
        author: &str,
        why: Option<String>,
    ) -> Result<Block> {
        let mut state = self.state();
        let policy = state
            .persisted
            .sessions
            .iter()
            .find(|s| s.name == session)
            .map(|s| s.run_policy)
            .ok_or("unknown session")?;
        notebook_mut(&mut state, notebook)?;
        self.file_as_remembered(&mut state, notebook)?;
        let entry = notebook_mut(&mut state, notebook)?;
        let first_use = !entry.sessions.iter().any(|s| s == session);
        if first_use {
            entry.sessions.push(session.to_string());
        }
        let mut block = Block::new(notebook, session, &normalize_newlines(code), author);
        block.why = why.map(|why| notebook_file::one_line(&why));
        let auto = block.proposed_by_agent() && policy == Policy::Auto;
        if auto {
            block.state = BlockState::Queued;
        }
        state.blocks.push(block.clone());
        self.save_state(&state)?;
        self.save_notebook(&mut state, notebook)?;
        drop(state);
        self.notify(Change::BlockAdded {
            notebook: notebook.to_string(),
            id: block.id.clone(),
        });
        if first_use {
            self.notify(Change::Sessions);
        }
        if auto {
            self.spawn_execution(&block.id);
        }
        Ok(block)
    }

    pub fn rerun_block(self: &Arc<Self>, id: &str) -> Result<()> {
        let mut state = self.state();
        self.block_file_as_remembered(&mut state, id)?;
        let block = block_mut(&mut state, id)?;
        if !matches!(block.state, BlockState::Finished | BlockState::Failed) {
            return Err("block has not finished".into());
        }
        block.state = BlockState::Pending;
        block.output = None;
        block.hidden_input_sent = false;
        block.read = ReadDecision::Undecided;
        let note = block.note.clone();
        self.queue_run(state, id, None, note, false)
    }

    pub async fn start_and_run(self: &Arc<Self>, id: &str) -> Result<()> {
        let block = {
            let mut state = self.state();
            self.block_file_as_remembered(&mut state, id)?;
            block_mut(&mut state, id)?.clone()
        };
        if !matches!(block.state, BlockState::Pending | BlockState::Saved) {
            return Err("block is not waiting for a run".into());
        }
        self.start_session(&block.session).await?;
        self.approve(id, None, block.note, block.release_when_done)
    }

    pub fn cancel_start(&self, id: &str) -> Result<()> {
        block_mut(&mut self.state(), id)?.release_when_done = false;
        self.set_session_stopped(id, false)
    }

    fn set_session_stopped(&self, id: &str, stopped: bool) -> Result<()> {
        let mut state = self.state();
        let block = block_mut(&mut state, id)?;
        block.session_stopped = stopped;
        let notebook = block.notebook.clone();
        drop(state);
        self.notify_block(&notebook, id);
        Ok(())
    }

    pub fn approve_run(
        self: &Arc<Self>,
        id: &str,
        edited_code: Option<String>,
        note: Option<String>,
    ) -> Result<()> {
        self.approve(id, edited_code, note, false)
    }

    pub fn approve_run_and_release(
        self: &Arc<Self>,
        id: &str,
        edited_code: Option<String>,
        note: Option<String>,
    ) -> Result<()> {
        self.approve(id, edited_code, note, true)
    }

    fn approve(
        self: &Arc<Self>,
        id: &str,
        edited_code: Option<String>,
        note: Option<String>,
        release_when_done: bool,
    ) -> Result<()> {
        let mut state = self.state();
        self.block_file_as_remembered(&mut state, id)?;
        self.queue_run(state, id, edited_code, note, release_when_done)
    }

    fn queue_run(
        self: &Arc<Self>,
        mut state: MutexGuard<'_, State>,
        id: &str,
        edited_code: Option<String>,
        note: Option<String>,
        release_when_done: bool,
    ) -> Result<()> {
        let block = block_mut(&mut state, id)?;
        if !matches!(block.state, BlockState::Pending | BlockState::Saved) {
            return Err("block is not waiting for a run".into());
        }
        if release_when_done && !block.proposed_by_agent() {
            return Err("only an agent's block waits for a read approval".into());
        }
        block.release_when_done = release_when_done;
        let edited_code = edited_code.map(|code| normalize_newlines(&code));
        if let Some(code) = edited_code.filter(|code| *code != block.code) {
            block.proposed_code = Some(std::mem::replace(&mut block.code, code));
        }
        block.note = note.filter(|n| !n.trim().is_empty());
        block.state = BlockState::Queued;
        block.session_stopped = false;
        let notebook = block.notebook.clone();
        self.save_notebook(&mut state, &notebook)?;
        drop(state);
        self.notify_block(&notebook, id);
        self.spawn_execution(id);
        Ok(())
    }

    pub fn reject_block(&self, id: &str, note: Option<String>) -> Result<()> {
        let mut state = self.state();
        self.block_file_as_remembered(&mut state, id)?;
        let block = block_mut(&mut state, id)?;
        if !matches!(block.state, BlockState::Pending | BlockState::Saved) {
            return Err("block is not waiting for a run".into());
        }
        block.state = BlockState::Rejected;
        block.note = note.filter(|n| !n.trim().is_empty());
        let notebook = block.notebook.clone();
        self.save_notebook(&mut state, &notebook)?;
        drop(state);
        self.notify_block(&notebook, id);
        Ok(())
    }

    pub fn delete_block(&self, id: &str) -> Result<()> {
        let mut state = self.state();
        if block_mut(&mut state, id)?.state == BlockState::Deleted {
            return Ok(());
        }
        self.block_file_as_remembered(&mut state, id)?;
        let block = block_mut(&mut state, id)?;
        if !matches!(block.state, BlockState::Finished | BlockState::Failed) {
            return Err("only a block that has run can be deleted".into());
        }
        block.state = BlockState::Deleted;
        block.code.clear();
        block.output = None;
        block.note = None;
        block.why = None;
        block.proposed_code = None;
        block.read = ReadDecision::Undecided;
        block.hidden_input_sent = false;
        let notebook = block.notebook.clone();
        self.save_notebook(&mut state, &notebook)?;
        drop(state);
        self.notify_block(&notebook, id);
        Ok(())
    }

    pub fn decide_read(&self, id: &str, decision: ReadDecision) -> Result<()> {
        let mut state = self.state();
        let block = block_mut(&mut state, id)?;
        if block.output.is_none()
            || !matches!(block.state, BlockState::Finished | BlockState::Failed)
        {
            return Err("block has no output yet".into());
        }
        block.read = decision;
        let notebook = block.notebook.clone();
        drop(state);
        self.notify_block(&notebook, id);
        Ok(())
    }

    pub fn hold_read(&self, id: &str) -> Result<()> {
        let mut state = self.state();
        let block = block_mut(&mut state, id)?;
        if !matches!(block.state, BlockState::Queued | BlockState::Running) {
            return Err("block is not queued or running".into());
        }
        block.release_when_done = false;
        let notebook = block.notebook.clone();
        drop(state);
        self.notify_block(&notebook, id);
        Ok(())
    }

    fn notify_block(&self, notebook: &str, id: &str) {
        self.notify(Change::Block {
            notebook: notebook.to_string(),
            id: id.to_string(),
        });
    }

    fn spawn_execution(self: &Arc<Self>, id: &str) {
        let core = Arc::clone(self);
        let id = id.to_string();
        tokio::spawn(async move { core.execute_block(&id).await });
    }

    async fn execute_block(&self, id: &str) {
        let Some(block) = self.block(id) else { return };
        let Some(def) = self.session(&block.session) else {
            return self.finish_block(
                &block,
                Err("session was deleted".into()),
                &VariableSet::new(),
            );
        };
        if !self.is_running(&def.name).await {
            self.set_block_state(id, BlockState::Pending);
            let _ = self.set_session_stopped(id, true);
            return;
        }
        let set = self.notebook_variables(&block.notebook);
        let lock = self.session_lock(&def.name);
        let _guard = lock.lock().await;
        self.set_block_state(id, BlockState::Running);
        let typing = match def.reads_a_block_as_one_command() {
            true => Typing::AsOneCommand,
            false => Typing::LineByLine,
        };
        let result = self
            .run_text(Some(id), &def, typing, &set, &block.code)
            .await;
        self.state().running.remove(id);
        self.finish_block(&block, result, &set);
    }

    pub async fn send_hidden_input(&self, id: &str, value: &str) -> Result<()> {
        if value.contains(['\n', '\r']) {
            return Err("hidden input is a single line".into());
        }
        let session = self.remember_hidden_input(id, value)?;
        if let Err(error) = self.tmux.send_hidden_line(&session, value).await {
            self.forget_last_hidden_input(id);
            return Err(error);
        }
        if self.running_typing(id) == Some(Typing::LineByLine) {
            self.retype_end_marker(id, &session).await;
        }
        self.notify_running_block(id);
        Ok(())
    }

    fn running_typing(&self, id: &str) -> Option<Typing> {
        self.state().running.get(id).map(|running| running.typing)
    }

    pub async fn interrupt_block(&self, id: &str) -> Result<()> {
        let session = self.remember_interrupt(id)?;
        if self.running_typing(id) == Some(Typing::AsOneCommand) {
            let _ = self.tmux.send_sigurg_to_the_terminal(&session).await;
        }
        self.tmux.interrupt(&session).await?;
        self.retype_end_marker(id, &session).await;
        Ok(())
    }

    fn remember_interrupt(&self, id: &str) -> Result<String> {
        let mut state = self.state();
        let running = state.running.get_mut(id).ok_or("block is not running")?;
        let at = self
            .tmux
            .log_len(&running.session)
            .saturating_sub(running.log_offset);
        running.interrupted_at.push(at);
        Ok(running.session.clone())
    }

    fn remember_hidden_input(&self, id: &str, value: &str) -> Result<String> {
        let mut state = self.state();
        let running = state.running.get_mut(id).ok_or("block is not running")?;
        let sent_at = self
            .tmux
            .log_len(&running.session)
            .saturating_sub(running.log_offset);
        running.hidden_inputs.push(HiddenInput {
            sent_at,
            value: value.to_string(),
        });
        let session = running.session.clone();
        let block = block_mut(&mut state, id)?;
        block.hidden_input_sent = true;
        block.release_when_done = false;
        Ok(session)
    }

    fn forget_last_hidden_input(&self, id: &str) {
        let mut state = self.state();
        let Some(running) = state.running.get_mut(id) else {
            return;
        };
        running.hidden_inputs.pop();
        let none_left = running.hidden_inputs.is_empty();
        if let (true, Ok(block)) = (none_left, block_mut(&mut state, id)) {
            block.hidden_input_sent = false;
        }
    }

    async fn retype_end_marker(&self, id: &str, session: &str) {
        tokio::time::sleep(PROMPT_SETTLE).await;
        let command = self
            .state()
            .running
            .get(id)
            .map(|running| running.end_marker_command.clone());
        if let Some(command) = command {
            let _ = self.tmux.type_lines(session, &command).await;
        }
    }

    fn notify_running_block(&self, id: &str) {
        if let Some(block) = self.block(id) {
            self.notify_block(&block.notebook, id);
        }
    }

    fn publish_output_so_far(&self, id: &str, output: String) {
        let mut state = self.state();
        let Ok(block) = block_mut(&mut state, id) else {
            return;
        };
        if block.state != BlockState::Running || block.output.as_deref() == Some(&output) {
            return;
        }
        block.output = Some(output);
        let notebook = block.notebook.clone();
        drop(state);
        self.notify_block(&notebook, id);
    }

    fn with_hidden_inputs_masked(
        &self,
        id: Option<&str>,
        raw: &str,
        clean: impl FnOnce(&str) -> String,
    ) -> String {
        let hidden = self.hidden_inputs(id);
        let interrupted_at = self.interrupted_at(id);
        let marked = with_input_marks(raw, &hidden, &interrupted_at);
        with_hidden_values_masked(&clean(&marked), &hidden)
            .replace(HIDDEN_INPUT_PLACEHOLDER, HIDDEN_INPUT_SENT)
    }

    fn interrupted_at(&self, id: Option<&str>) -> Vec<usize> {
        id.and_then(|id| {
            self.state()
                .running
                .get(id)
                .map(|running| running.interrupted_at.clone())
        })
        .unwrap_or_default()
    }

    fn hidden_inputs(&self, id: Option<&str>) -> Vec<HiddenInput> {
        id.and_then(|id| {
            self.state()
                .running
                .get(id)
                .map(|running| running.hidden_inputs.clone())
        })
        .unwrap_or_default()
    }

    async fn with_the_screen_of_a_full_screen_program(
        &self,
        id: &str,
        session: &str,
        output_so_far: String,
    ) -> String {
        let Some(at) = output_so_far.rfind(SCREEN_NOT_KEPT) else {
            return output_so_far;
        };
        let (before, notice_line) = output_so_far.split_at(at);
        if notice_line.contains('\n') || !self.tmux.is_on_the_alternate_screen(session).await {
            return output_so_far;
        }
        let Some(screen) = self.tmux.screen(session).await else {
            return output_so_far;
        };
        let screen = with_hidden_values_masked(
            &screen_text_without_drawing(&screen),
            &self.hidden_inputs(Some(id)),
        );
        let notice_line = notice_line.replacen(SCREEN_NOT_KEPT, SCREEN_NOW, 1);
        match screen.is_empty() {
            true => format!("{before}{notice_line}"),
            false => format!("{before}{notice_line}\n{screen}"),
        }
    }

    fn finish_block(&self, ran: &Block, result: Result<String>, set: &VariableSet) {
        let id = ran.id.as_str();
        let mut state = self.state();
        let read_policy = state
            .persisted
            .sessions
            .iter()
            .find(|s| s.name == ran.session)
            .map(|s| s.read_policy);
        let Ok(block) = block_mut(&mut state, id) else {
            return;
        };
        let notebook = block.notebook.clone();
        if !block.has_the_text_of(ran) {
            block.state = BlockState::Saved;
            block.output = None;
            block.release_when_done = false;
            drop(state);
            return self.notify_block(&notebook, id);
        }
        match result {
            Ok(output) => {
                block.output = Some(output);
                block.state = BlockState::Finished;
            }
            Err(error) => {
                block.output = Some(vars::mask(&error, set));
                block.state = BlockState::Failed;
            }
        }
        let released_by_a_click = std::mem::take(&mut block.release_when_done);
        let released_by_policy = read_policy == Some(Policy::Auto);
        if (released_by_policy || released_by_a_click) && block.proposed_by_agent() {
            block.read = ReadDecision::Released;
        }
        drop(state);
        self.notify_block(&notebook, id);
    }

    fn set_block_state(&self, id: &str, new_state: BlockState) {
        let mut state = self.state();
        let Ok(block) = block_mut(&mut state, id) else {
            return;
        };
        block.state = new_state;
        let notebook = block.notebook.clone();
        drop(state);
        self.notify_block(&notebook, id);
    }

    async fn run_text(
        &self,
        block_id: Option<&str>,
        def: &SessionDef,
        typing: Typing,
        set: &VariableSet,
        code: &str,
    ) -> Result<String> {
        if !self.is_running(&def.name).await {
            return Err(format!("session {} is not running", def.name));
        }
        let rendered = vars::render(code, set)?;
        let marker = Marker::new();
        let typed = marker.command(&def.marker_template);
        let offset = self.tmux.log_len(&def.name);
        if let Some(id) = block_id {
            self.state().running.insert(
                id.to_string(),
                RunningBlock {
                    session: def.name.clone(),
                    log_offset: offset,
                    end_marker_command: typed.clone(),
                    typing,
                    hidden_inputs: Vec::new(),
                    interrupted_at: Vec::new(),
                },
            );
        }
        match typing {
            Typing::AsOneCommand => {
                let command = marker.block_as_one_command(&rendered, &typed);
                self.tmux.type_lines(&def.name, &command).await?;
            }
            Typing::LineByLine => {
                self.tmux.type_lines(&def.name, &rendered).await?;
                self.tmux.type_lines(&def.name, &typed).await?;
            }
        }
        self.touch(&def.name);
        let deadline = def
            .block_timeout_secs
            .and_then(|secs| Some((secs, Instant::now().checked_add(Duration::from_secs(secs))?)));
        let mut next_liveness_check = Instant::now() + LIVENESS_CHECK;
        loop {
            self.touch(&def.name);
            let text = self.tmux.log_since(&def.name, offset);
            if let Some(end) = text.find(&marker.full()) {
                self.touch(&def.name);
                let output = self.with_hidden_inputs_masked(block_id, &text[..end], |raw| {
                    clean_output(raw, &marker, &typed, typing, Ended::AtTheMarker)
                });
                if let Some(id) = block_id {
                    self.state().running.remove(id);
                }
                return Ok(vars::mask(&output, set));
            }
            let output_so_far = || {
                self.with_hidden_inputs_masked(block_id, &text, |raw| {
                    clean_output(raw, &marker, &typed, typing, Ended::Early)
                })
            };
            if Instant::now() >= next_liveness_check {
                if !self.is_running(&def.name).await {
                    return Err(format!("session {} exited\n{}", def.name, output_so_far()));
                }
                next_liveness_check = Instant::now() + LIVENESS_CHECK;
            }
            if let Some((secs, _)) = deadline.filter(|(_, at)| Instant::now() >= *at) {
                return Err(format!(
                    "no end marker after {secs}s, the session may still be busy\n{}",
                    output_so_far()
                ));
            }
            if let Some(id) = block_id {
                let so_far = self
                    .with_the_screen_of_a_full_screen_program(id, &def.name, output_so_far())
                    .await;
                let so_far = vars::mask(&so_far, set);
                if !so_far.trim().is_empty() {
                    self.publish_output_so_far(id, so_far);
                }
            }
            tokio::time::sleep(OUTPUT_POLL).await;
        }
    }

    fn touch(&self, session: &str) {
        self.state()
            .last_activity
            .insert(session.to_string(), Instant::now());
    }

    fn session_lock(&self, session: &str) -> Arc<tokio::sync::Mutex<()>> {
        self.session_locks
            .lock()
            .expect("locks")
            .entry(session.to_string())
            .or_default()
            .clone()
    }

    // agents

    pub fn request_join(&self, notebook: &str, agent: &str) -> Result<JoinOutcome> {
        if !valid_name(agent) || agent == HUMAN {
            return Err("agent names use a-z, 0-9, - and _, and are not 'human'".into());
        }
        let mut state = self.state();
        notebook_mut(&mut state, notebook)?;
        let existing = state.joins.iter_mut().find(|j| {
            j.notebook == notebook && j.agent == agent && !matches!(j.status, JoinStatus::Delivered)
        });
        match existing {
            Some(join) => match join.status.clone() {
                JoinStatus::Approved { token } => {
                    join.status = JoinStatus::Delivered;
                    Ok(JoinOutcome::Token(token))
                }
                _ => Ok(self.pending_outcome(notebook, &join.id)),
            },
            None => {
                let join = JoinRequest {
                    id: short_id(),
                    notebook: notebook.to_string(),
                    agent: agent.to_string(),
                    status: JoinStatus::Pending,
                };
                let outcome = self.pending_outcome(notebook, &join.id);
                state.joins.push(join);
                drop(state);
                self.notify(Change::Agents {
                    notebook: notebook.to_string(),
                });
                Ok(outcome)
            }
        }
    }

    fn pending_outcome(&self, notebook: &str, join_id: &str) -> JoinOutcome {
        JoinOutcome::Pending {
            approve_link: format!("{}/n/{}#join-{}", self.origin, notebook, join_id),
        }
    }

    pub fn pending_joins(&self, notebook: &str) -> Vec<JoinRequest> {
        self.state()
            .joins
            .iter()
            .filter(|j| j.notebook == notebook && matches!(j.status, JoinStatus::Pending))
            .cloned()
            .collect()
    }

    pub fn approve_join(&self, id: &str) -> Result<()> {
        let mut state = self.state();
        let join = state
            .joins
            .iter_mut()
            .find(|j| j.id == id && matches!(j.status, JoinStatus::Pending))
            .ok_or("unknown join request")?;
        let token = secret_token();
        join.status = JoinStatus::Approved {
            token: token.clone(),
        };
        let (notebook, agent) = (join.notebook.clone(), join.agent.clone());
        let entry = notebook_mut(&mut state, &notebook)?;
        entry.tokens.retain(|t| t.agent != agent);
        entry.tokens.push(NotebookToken {
            id: short_id(),
            agent,
            hash: hash_token(&token),
        });
        self.save_state(&state)?;
        drop(state);
        self.notify(Change::Agents { notebook });
        Ok(())
    }

    pub fn deny_join(&self, id: &str) -> Result<()> {
        let mut state = self.state();
        let notebook = state
            .joins
            .iter()
            .find(|j| j.id == id)
            .map(|j| j.notebook.clone());
        state.joins.retain(|j| j.id != id);
        drop(state);
        if let Some(notebook) = notebook {
            self.notify(Change::Agents { notebook });
        }
        Ok(())
    }

    pub fn revoke_token(&self, notebook: &str, token_id: &str) -> Result<()> {
        let mut state = self.state();
        notebook_mut(&mut state, notebook)?
            .tokens
            .retain(|t| t.id != token_id);
        self.save_state(&state)?;
        drop(state);
        self.notify(Change::Agents {
            notebook: notebook.to_string(),
        });
        Ok(())
    }

    pub fn join_notebook_of(&self, join_id: &str) -> Option<String> {
        self.state()
            .joins
            .iter()
            .find(|j| j.id == join_id)
            .map(|j| j.notebook.clone())
    }

    pub fn admit(&self, token: &str) -> Option<Admission> {
        let hash = hash_token(token);
        self.state()
            .persisted
            .notebooks
            .iter()
            .find_map(|notebook| {
                notebook
                    .tokens
                    .iter()
                    .find(|t| t.hash == hash)
                    .map(|t| Admission {
                        notebook: notebook.name.clone(),
                        agent: t.agent.clone(),
                    })
            })
    }

    // passkeys

    pub fn passkeys(&self) -> Vec<webauthn_rs::prelude::Passkey> {
        self.state().persisted.passkeys.clone()
    }

    pub fn add_passkey(&self, passkey: webauthn_rs::prelude::Passkey) -> Result<()> {
        let mut state = self.state();
        state.persisted.passkeys.push(passkey);
        self.save_state(&state)
    }

    pub fn update_passkey_counter(
        &self,
        result: &webauthn_rs::prelude::AuthenticationResult,
    ) -> Result<()> {
        let mut state = self.state();
        for passkey in state.persisted.passkeys.iter_mut() {
            passkey.update_credential(result);
        }
        self.save_state(&state)
    }

    // persistence

    fn save_state(&self, state: &State) -> Result<()> {
        write_json(&self.dir.join("state.json"), &state.persisted)
    }

    fn save_vars(&self, state: &State) -> Result<()> {
        write_json(&self.dir.join("variables.json"), &state.vars)?;
        self.notify(Change::Variables);
        Ok(())
    }

    fn save_notebook(&self, state: &mut State, notebook: &str) -> Result<()> {
        let blocks: Vec<Block> = state
            .blocks
            .iter()
            .filter(|b| b.notebook == notebook)
            .cloned()
            .collect();
        let text = notebook_file::serialize(notebook, &blocks);
        std::fs::write(notebook_path(&self.dir, notebook), &text).map_err(|e| e.to_string())?;
        state.files.insert(
            notebook.to_string(),
            NotebookFile {
                text_as_last_read_or_written: text,
                unloadable: None,
            },
        );
        Ok(())
    }

    fn block_file_as_remembered(&self, state: &mut State, block_id: &str) -> Result<()> {
        let notebook = block_mut(state, block_id)?.notebook.clone();
        self.file_as_remembered(state, &notebook)
    }

    fn file_as_remembered(&self, state: &mut State, notebook: &str) -> Result<()> {
        match self.compare_with_disk(state, notebook) {
            OnDisk::AsRemembered => Ok(()),
            OnDisk::LoadedAgain => Err(format!(
                "{notebook}.md changed on disk. It was loaded again and nothing ran."
            )),
            OnDisk::Unloadable(error) => Err(error),
        }
    }

    fn compare_with_disk(&self, state: &mut State, notebook: &str) -> OnDisk {
        let remembered = state.files.remove(notebook).unwrap_or_default();
        let checked = read_notebook_text(
            &self.dir,
            notebook,
            &remembered.text_as_last_read_or_written,
        )
        .and_then(|text| loadable_blocks(state, notebook, &text).map(|saved| (text, saved)));
        let (on_disk, text) = match checked {
            Ok((text, _)) if text == remembered.text_as_last_read_or_written => {
                (OnDisk::AsRemembered, text)
            }
            Ok((text, saved)) => {
                load_blocks(state, notebook, saved);
                (OnDisk::LoadedAgain, text)
            }
            Err(error) => (
                OnDisk::Unloadable(error),
                remembered.text_as_last_read_or_written,
            ),
        };
        let unloadable = match &on_disk {
            OnDisk::Unloadable(error) => Some(error.clone()),
            _ => None,
        };
        if matches!(on_disk, OnDisk::LoadedAgain) || unloadable != remembered.unloadable {
            self.notify(Change::NotebookLoaded {
                notebook: notebook.to_string(),
            });
        }
        state.files.insert(
            notebook.to_string(),
            NotebookFile {
                text_as_last_read_or_written: text,
                unloadable,
            },
        );
        on_disk
    }
}

pub enum VariableScope {
    Profile(String),
    Notebook(String),
}

enum Ended {
    AtTheMarker,
    Early,
}

fn clean_output(raw: &str, marker: &Marker, typed: &str, typing: Typing, ended: Ended) -> String {
    let text = strip_terminal_escapes(&without_full_screen_drawings(raw));
    let all_lines: Vec<&str> = text.lines().collect();
    let lines = match typing {
        Typing::AsOneCommand => after_the_echo_of_one_command(&all_lines, typed),
        Typing::LineByLine => &all_lines,
    };
    let echoed_after_the_prompt = match ended {
        Ended::AtTheMarker => lines.iter().rposition(|line| line.contains(&marker.head)),
        Ended::Early => None,
    };
    lines
        .iter()
        .enumerate()
        .filter_map(|(index, line)| {
            if Some(index) == echoed_after_the_prompt {
                return None;
            }
            without_typed_ahead_echo(line, typed)
                .and_then(without_the_echo_of_ctrl_c)
                .filter(|line| !line.contains(&marker.head))
        })
        .collect::<Vec<_>>()
        .join("\n")
}

enum InputMark {
    HiddenInput,
    Interrupt,
}

fn with_input_marks(raw: &str, hidden: &[HiddenInput], interrupted_at: &[usize]) -> String {
    let mut marks: Vec<(usize, InputMark)> = hidden
        .iter()
        .map(|input| (input.sent_at, InputMark::HiddenInput))
        .chain(interrupted_at.iter().map(|at| (*at, InputMark::Interrupt)))
        .collect();
    marks.sort_by_key(|(at, _)| std::cmp::Reverse(*at));
    let mut marked = raw.to_string();
    for (at, mark) in marks {
        if !marked.is_char_boundary(at) {
            continue;
        }
        match mark {
            InputMark::HiddenInput => marked.insert(at, HIDDEN_INPUT_PLACEHOLDER),
            InputMark::Interrupt => {
                if let Some(echo) = marked[at..].find(CTRL_C_ECHO).map(|found| at + found) {
                    marked.replace_range(
                        echo..echo + CTRL_C_ECHO.len(),
                        &CTRL_C_ECHO_PLACEHOLDER.to_string(),
                    );
                }
            }
        }
    }
    marked
}

fn without_the_echo_of_ctrl_c(line: Cow<'_, str>) -> Option<Cow<'_, str>> {
    if !line.contains(CTRL_C_ECHO_PLACEHOLDER) {
        return Some(line);
    }
    let output = line.replace(CTRL_C_ECHO_PLACEHOLDER, "");
    (!output.is_empty()).then_some(Cow::Owned(output))
}

fn without_full_screen_drawings(raw: &str) -> String {
    let mut kept = String::with_capacity(raw.len());
    let mut rest = raw;
    while let Some(start) = rest.find(ALTERNATE_SCREEN_ON) {
        kept.push_str(&rest[..start]);
        let drawn = &rest[start + ALTERNATE_SCREEN_ON.len()..];
        let (drawing, after) = match drawn.find(ALTERNATE_SCREEN_OFF) {
            Some(end) => (&drawn[..end], &drawn[end + ALTERNATE_SCREEN_OFF.len()..]),
            None => (drawn, ""),
        };
        let last_line = kept.rsplit('\n').next().unwrap_or_default();
        if !strip_terminal_escapes(last_line).is_empty() {
            kept.push('\n');
        }
        kept.push_str(SCREEN_NOT_KEPT);
        for _ in drawing.matches(HIDDEN_INPUT_PLACEHOLDER) {
            kept.push(' ');
            kept.push(HIDDEN_INPUT_PLACEHOLDER);
        }
        kept.push('\n');
        rest = after;
    }
    kept.push_str(rest);
    kept
}

fn screen_text_without_drawing(screen: &str) -> String {
    screen
        .lines()
        .map(|line| {
            line.chars()
                .filter(|c| !('\u{2500}'..='\u{257F}').contains(c))
                .collect::<String>()
        })
        .map(|line| line.trim().to_string())
        .filter(|line| !line.is_empty())
        .collect::<Vec<_>>()
        .join("\n")
}

fn with_hidden_values_masked(text: &str, hidden: &[HiddenInput]) -> String {
    let mut values: Vec<&str> = hidden
        .iter()
        .map(|input| input.value.as_str())
        .filter(|value| !value.is_empty())
        .collect();
    values.sort_by_key(|value| std::cmp::Reverse(value.len()));
    values.into_iter().fold(text.to_string(), |masked, value| {
        masked.replace(value, "***")
    })
}

fn after_the_echo_of_one_command<'a, 'b>(lines: &'a [&'b str], typed: &str) -> &'a [&'b str] {
    let last_line = last_line_of_one_command(typed);
    match lines.iter().rposition(|line| line.contains(&last_line)) {
        Some(index) => &lines[index + 1..],
        None => lines,
    }
}

fn without_typed_ahead_echo<'a>(line: &'a str, typed: &str) -> Option<Cow<'a, str>> {
    if !line.contains(typed) {
        return Some(Cow::Borrowed(line));
    }
    let output = line.replace(typed, "");
    (!output.is_empty()).then_some(Cow::Owned(output))
}

fn notebook_mut<'a>(state: &'a mut State, name: &str) -> Result<&'a mut Notebook> {
    state
        .persisted
        .notebooks
        .iter_mut()
        .find(|n| n.name == name)
        .ok_or_else(|| format!("unknown notebook {name}"))
}

fn block_mut<'a>(state: &'a mut State, id: &str) -> Result<&'a mut Block> {
    state
        .blocks
        .iter_mut()
        .find(|b| b.id == id)
        .ok_or_else(|| "unknown block".to_string())
}

fn notebook_path(dir: &Path, notebook: &str) -> PathBuf {
    dir.join("notebooks").join(format!("{notebook}.md"))
}

fn read_notebook_text(dir: &Path, notebook: &str, remembered: &str) -> Result<String> {
    match std::fs::read_to_string(notebook_path(dir, notebook)) {
        Ok(text) => Ok(text),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound && remembered.is_empty() => {
            Ok(String::new())
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            Err(format!("{notebook}.md is missing"))
        }
        Err(e) => Err(format!("{notebook}.md: {e}")),
    }
}

fn loadable_blocks(
    state: &State,
    notebook: &str,
    text: &str,
) -> Result<Vec<notebook_file::SavedBlock>> {
    let saved = notebook_file::parse(text).map_err(|error| error.message(notebook))?;
    let elsewhere = saved.iter().find_map(|saved| {
        state
            .blocks
            .iter()
            .find(|b| b.notebook != notebook && b.id == saved.id)
    });
    match elsewhere {
        Some(block) => Err(format!(
            "{notebook}.md: block id {} is used in notebook {}",
            block.id, block.notebook
        )),
        None => Ok(saved),
    }
}

fn load_blocks(state: &mut State, notebook: &str, saved: Vec<notebook_file::SavedBlock>) {
    let (mut held, others): (Vec<Block>, Vec<Block>) = std::mem::take(&mut state.blocks)
        .into_iter()
        .partition(|b| b.notebook == notebook);
    state.blocks = others;
    let file_ids: std::collections::HashSet<String> =
        saved.iter().map(|saved| saved.id.clone()).collect();
    let dropped_ones_the_file_never_lists = held
        .iter()
        .filter(|b| b.state.is_dropped() && !file_ids.contains(&b.id))
        .cloned()
        .collect::<Vec<_>>();
    for saved in saved {
        let from_file = saved_block(notebook, saved);
        let block = match held
            .iter()
            .position(|b| b.id == from_file.id)
            .map(|at| held.swap_remove(at))
        {
            Some(held) if !held.state.is_dropped() && held.has_the_text_of(&from_file) => held,
            Some(held) if matches!(held.state, BlockState::Queued | BlockState::Running) => Block {
                code: from_file.code,
                session: from_file.session,
                why: from_file.why,
                author: from_file.author,
                ..held
            },
            _ => from_file,
        };
        state.blocks.push(block);
    }
    state.blocks.extend(dropped_ones_the_file_never_lists);
}

fn saved_block(notebook: &str, saved: notebook_file::SavedBlock) -> Block {
    let mut block = Block::new(notebook, &saved.session, &saved.code, &saved.author);
    block.id = saved.id;
    block.why = saved.why;
    block.state = BlockState::Saved;
    block
}

fn required_why(why: &str) -> Result<String> {
    match why.trim() {
        "" => Err("why is required: say what you want to learn and why".into()),
        why => Ok(why.to_string()),
    }
}

fn keep_the_latest_token_per_agent(notebook: &mut Notebook) {
    let mut seen = std::collections::HashSet::new();
    notebook.tokens.reverse();
    notebook
        .tokens
        .retain(|token| seen.insert(token.agent.clone()));
    notebook.tokens.reverse();
}

fn normalize_newlines(code: &str) -> String {
    code.replace("\r\n", "\n")
}

fn read_json<T: serde::de::DeserializeOwned + Default>(path: &Path) -> Result<T> {
    match std::fs::read(path) {
        Ok(bytes) => serde_json::from_slice(&bytes)
            .map_err(|e| format!("{} is corrupt, fix or remove it: {e}", path.display())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(T::default()),
        Err(e) => Err(format!("{}: {e}", path.display())),
    }
}

pub fn create_private_dir(path: &Path) -> Result<()> {
    use std::os::unix::fs::DirBuilderExt;
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(path)
        .map_err(|e| e.to_string())
}

fn write_json<T: serde::Serialize>(path: &Path, value: &T) -> Result<()> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    let bytes = serde_json::to_vec_pretty(value).map_err(|e| e.to_string())?;
    let temp = path.with_extension("json.tmp");
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(&temp)
        .map_err(|e| e.to_string())?;
    file.write_all(&bytes).map_err(|e| e.to_string())?;
    file.sync_all().map_err(|e| e.to_string())?;
    std::fs::rename(&temp, path).map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn typed_marker_command_never_contains_the_marker_itself() {
        for preset in [Preset::Bash, Preset::Iex, Preset::Psql, Preset::Mysql] {
            let marker = Marker::new();
            let typed = marker.command(preset.marker_template());
            assert!(!typed.contains(&marker.full()), "{preset:?}");
            assert!(
                typed.contains(&marker.head) && typed.contains(&marker.tail),
                "{preset:?}"
            );
        }
    }

    #[test]
    fn output_the_typed_ahead_marker_lands_on_is_kept() {
        let marker = Marker::new();
        let typed = marker.command(Preset::Bash.marker_template());
        let raw = format!(
            "uname -s && echo checks-passed\r\n\x1b[?2004l\r{typed}Linux\r\nchecks-passed\r\n\r\n\x1b[?2004h\r\r\n\
\x1b[1;32m[\x1b]0;me@box: ~\x07me@box:~]$\x1b[0m {typed}\r\n\x1b[?2004l\r"
        );

        let output = clean_output(
            &raw,
            &marker,
            &typed,
            Typing::LineByLine,
            Ended::AtTheMarker,
        );

        assert_eq!(
            output.trim_end(),
            "uname -s && echo checks-passed\nLinux\nchecks-passed"
        );
    }

    #[test]
    fn output_the_typed_ahead_marker_lands_on_is_kept_when_the_session_exits_first() {
        let marker = Marker::new();
        let typed = marker.command(Preset::Bash.marker_template());
        let raw = format!("uname -s; exit\r\n\x1b[?2004l\r{typed}Linux\r\nexit\r\n");

        let output = clean_output(&raw, &marker, &typed, Typing::LineByLine, Ended::Early);

        assert_eq!(output.trim_end(), "uname -s; exit\nLinux\nexit");
    }

    #[test]
    fn the_terminals_echo_of_ctrl_c_after_an_interrupt_stays_out_of_the_output() {
        let marker = Marker::new();
        let typed = marker.command(Preset::Bash.marker_template());
        let raw = "intr = ^C\r\nup\r\n^Ccaught\r\nstarted\r\n^C\r\n";
        let interrupted_at = [raw.find("up").unwrap(), raw.find("started").unwrap()];

        let marked = with_input_marks(raw, &[], &interrupted_at);
        let output = clean_output(&marked, &marker, &typed, Typing::LineByLine, Ended::Early);

        assert_eq!(output, "intr = ^C\nup\ncaught\nstarted");
    }

    #[test]
    fn what_a_full_screen_program_drew_becomes_one_notice() {
        let marker = Marker::new();
        let typed = marker.command(Preset::Bash.marker_template());
        let raw = "before\r\n\x1b[?1049h\x1b[5;10H┌──┐\x1b[6;10H│ PIN │\x1b[?1049lafter\r\n";

        let output = clean_output(raw, &marker, &typed, Typing::LineByLine, Ended::AtTheMarker);

        assert_eq!(
            output,
            "before\n‹full-screen program, screen not kept›\nafter"
        );
    }
}
