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
use crate::tmux::{strip_terminal_escapes, Tmux};
use crate::vars;

pub type Result<T> = std::result::Result<T, String>;

pub const BLOCK_TIMEOUT: Duration = Duration::from_secs(120);
const START_SETTLE: Duration = Duration::from_millis(500);
const OUTPUT_POLL: Duration = Duration::from_millis(100);
const LIVENESS_CHECK: Duration = Duration::from_secs(1);
const CHANGES_BUFFERED: usize = 256;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Change {
    BlockAdded { notebook: String, id: String },
    Block { notebook: String, id: String },
    Sessions,
    Proposals,
    Agents { notebook: String },
    Variables,
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
    last_activity: HashMap<String, Instant>,
    proposals: Vec<SessionProposal>,
    joins: Vec<JoinRequest>,
}

pub struct Core {
    dir: PathBuf,
    origin: String,
    tmux: Tmux,
    block_timeout: Duration,
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
        let blocks = persisted
            .notebooks
            .iter()
            .flat_map(|notebook| load_blocks(dir, &notebook.name))
            .collect();
        Ok(Arc::new(Core {
            dir: dir.to_path_buf(),
            origin: origin.trim_end_matches('/').to_string(),
            tmux: Tmux::new(tmux_socket, &dir.join("runtime")),
            block_timeout: BLOCK_TIMEOUT,
            state: Mutex::new(State {
                persisted,
                vars,
                blocks,
                last_activity: HashMap::new(),
                proposals: Vec::new(),
                joins: Vec::new(),
            }),
            session_locks: Mutex::new(HashMap::new()),
            changes: broadcast::channel(CHANGES_BUFFERED).0,
            changes_sent: AtomicU64::new(0),
        }))
    }

    pub fn with_block_timeout(self: Arc<Self>, timeout: Duration) -> Arc<Core> {
        let core = Arc::into_inner(self).expect("core is not shared yet");
        Arc::new(Core {
            block_timeout: timeout,
            ..core
        })
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
        self.save_notebook(&state, name)
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
        match state.persisted.sessions.iter_mut().find(|s| s.name == def.name) {
            Some(existing) => *existing = def,
            None => state.persisted.sessions.push(def),
        }
        self.save_state(&state)?;
        self.notify(Change::Sessions);
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
            self.run_text(def, set, setup)
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

    pub async fn reap_idle_sessions(&self) {
        let now = Instant::now();
        for def in self.session_defs() {
            let lock = self.session_lock(&def.name);
            let Ok(_busy_starting_or_running) = lock.try_lock() else { continue };
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

    pub fn propose_session(&self, notebook: &str, agent: &str, def: SessionDef, why: &str) -> Result<SessionProposal> {
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

    pub fn set_variable(&self, scope: VariableScope, name: &str, value: &str, secret: bool) -> Result<()> {
        let mut state = self.state();
        let set = match &scope {
            VariableScope::Profile(profile) => state.vars.profiles.entry(profile.clone()).or_default(),
            VariableScope::Notebook(notebook) => state.vars.notebooks.entry(notebook.clone()).or_default(),
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

    pub fn add_block(self: &Arc<Self>, notebook: &str, session: &str, code: &str, author: &str) -> Result<Block> {
        self.insert_block(notebook, session, code, author, None)
    }

    pub fn propose_block(self: &Arc<Self>, notebook: &str, session: &str, code: &str, agent: &str, why: &str) -> Result<Block> {
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
        let entry = notebook_mut(&mut state, notebook)?;
        let first_use = !entry.sessions.iter().any(|s| s == session);
        if first_use {
            entry.sessions.push(session.to_string());
        }
        let mut block = Block::new(notebook, session, &normalize_newlines(code), author);
        block.why = why;
        let auto = block.proposed_by_agent() && policy == Policy::Auto;
        if auto {
            block.state = BlockState::Queued;
        }
        state.blocks.push(block.clone());
        self.save_state(&state)?;
        self.save_notebook(&state, notebook)?;
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
        let note = self.reset_for_rerun(id)?;
        self.approve_run(id, None, note)
    }

    fn reset_for_rerun(&self, id: &str) -> Result<Option<String>> {
        let mut state = self.state();
        let block = block_mut(&mut state, id)?;
        if !matches!(block.state, BlockState::Finished | BlockState::Failed) {
            return Err("block has not finished".into());
        }
        block.state = BlockState::Pending;
        block.output = None;
        block.read = ReadDecision::Undecided;
        Ok(block.note.clone())
    }

    pub async fn start_and_run(self: &Arc<Self>, id: &str) -> Result<()> {
        let block = self.block(id).ok_or("unknown block")?;
        if !matches!(block.state, BlockState::Pending | BlockState::Saved) {
            return Err("block is not waiting for a run".into());
        }
        self.start_session(&block.session).await?;
        self.approve_run(id, None, block.note)
    }

    pub fn cancel_start(&self, id: &str) -> Result<()> {
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

    pub fn approve_run(self: &Arc<Self>, id: &str, edited_code: Option<String>, note: Option<String>) -> Result<()> {
        let mut state = self.state();
        let block = block_mut(&mut state, id)?;
        if !matches!(block.state, BlockState::Pending | BlockState::Saved) {
            return Err("block is not waiting for a run".into());
        }
        let edited_code = edited_code.map(|code| normalize_newlines(&code));
        if let Some(code) = edited_code.filter(|code| *code != block.code) {
            block.proposed_code = Some(std::mem::replace(&mut block.code, code));
        }
        block.note = note.filter(|n| !n.trim().is_empty());
        block.state = BlockState::Queued;
        block.session_stopped = false;
        let notebook = block.notebook.clone();
        self.save_notebook(&state, &notebook)?;
        drop(state);
        self.notify_block(&notebook, id);
        self.spawn_execution(id);
        Ok(())
    }

    pub fn reject_block(&self, id: &str, note: Option<String>) -> Result<()> {
        let mut state = self.state();
        let block = block_mut(&mut state, id)?;
        if !matches!(block.state, BlockState::Pending | BlockState::Saved) {
            return Err("block is not waiting for a run".into());
        }
        block.state = BlockState::Rejected;
        block.note = note.filter(|n| !n.trim().is_empty());
        let notebook = block.notebook.clone();
        self.save_notebook(&state, &notebook)?;
        drop(state);
        self.notify_block(&notebook, id);
        Ok(())
    }

    pub fn decide_read(&self, id: &str, decision: ReadDecision) -> Result<()> {
        let mut state = self.state();
        let block = block_mut(&mut state, id)?;
        if block.output.is_none() {
            return Err("block has no output yet".into());
        }
        block.read = decision;
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
            return self.finish_block(id, Err("session was deleted".into()), &VariableSet::new());
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
        let result = self.run_text(&def, &set, &block.code).await;
        self.finish_block(id, result, &set);
    }

    fn finish_block(&self, id: &str, result: Result<String>, set: &VariableSet) {
        let mut state = self.state();
        let read_policy = state
            .blocks
            .iter()
            .find(|b| b.id == id)
            .and_then(|b| state.persisted.sessions.iter().find(|s| s.name == b.session))
            .map(|s| s.read_policy);
        let Ok(block) = block_mut(&mut state, id) else { return };
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
        if read_policy == Some(Policy::Auto) && block.proposed_by_agent() {
            block.read = ReadDecision::Released;
        }
        let notebook = block.notebook.clone();
        drop(state);
        self.notify_block(&notebook, id);
    }

    fn set_block_state(&self, id: &str, new_state: BlockState) {
        let mut state = self.state();
        let Ok(block) = block_mut(&mut state, id) else { return };
        block.state = new_state;
        let notebook = block.notebook.clone();
        drop(state);
        self.notify_block(&notebook, id);
    }

    async fn run_text(&self, def: &SessionDef, set: &VariableSet, code: &str) -> Result<String> {
        if !self.is_running(&def.name).await {
            return Err(format!("session {} is not running", def.name));
        }
        let rendered = vars::render(code, set)?;
        let marker = Marker::new();
        let typed = marker.command(&def.marker_template);
        let offset = self.tmux.log_len(&def.name);
        self.tmux.type_lines(&def.name, &rendered).await?;
        self.tmux.type_lines(&def.name, &typed).await?;
        self.touch(&def.name);
        let deadline = Instant::now() + self.block_timeout;
        let mut next_liveness_check = Instant::now() + LIVENESS_CHECK;
        loop {
            self.touch(&def.name);
            let text = self.tmux.log_since(&def.name, offset);
            if let Some(end) = text.find(&marker.full()) {
                self.touch(&def.name);
                let output = clean_output(&text[..end], &marker, &typed, Ended::AtTheMarker);
                return Ok(vars::mask(&output, set));
            }
            if Instant::now() >= next_liveness_check {
                if !self.is_running(&def.name).await {
                    let partial = clean_output(&text, &marker, &typed, Ended::Early);
                    return Err(format!("session {} exited\n{partial}", def.name));
                }
                next_liveness_check = Instant::now() + LIVENESS_CHECK;
            }
            if Instant::now() >= deadline {
                let partial = clean_output(&text, &marker, &typed, Ended::Early);
                return Err(format!(
                    "no end marker after {}s, the session may still be busy\n{partial}",
                    self.block_timeout.as_secs()
                ));
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
        let existing = state
            .joins
            .iter_mut()
            .find(|j| j.notebook == notebook && j.agent == agent && !matches!(j.status, JoinStatus::Delivered));
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
        join.status = JoinStatus::Approved { token: token.clone() };
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
        let notebook = state.joins.iter().find(|j| j.id == id).map(|j| j.notebook.clone());
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
        self.state().persisted.notebooks.iter().find_map(|notebook| {
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

    pub fn update_passkey_counter(&self, result: &webauthn_rs::prelude::AuthenticationResult) -> Result<()> {
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

    fn save_notebook(&self, state: &State, notebook: &str) -> Result<()> {
        let blocks: Vec<Block> = state
            .blocks
            .iter()
            .filter(|b| b.notebook == notebook)
            .cloned()
            .collect();
        std::fs::write(
            notebook_path(&self.dir, notebook),
            notebook_file::serialize(notebook, &blocks),
        )
        .map_err(|e| e.to_string())
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

fn clean_output(raw: &str, marker: &Marker, typed: &str, ended: Ended) -> String {
    let text = strip_terminal_escapes(raw);
    let lines: Vec<&str> = text.lines().collect();
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
            without_typed_ahead_echo(line, typed).filter(|line| !line.contains(&marker.head))
        })
        .collect::<Vec<_>>()
        .join("\n")
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

fn load_blocks(dir: &Path, notebook: &str) -> Vec<Block> {
    let text = std::fs::read_to_string(notebook_path(dir, notebook)).unwrap_or_default();
    notebook_file::parse(&text)
        .into_iter()
        .map(|saved| {
            let mut block = Block::new(notebook, &saved.session, &saved.code, &saved.author);
            block.id = saved.id;
            block.why = saved.why;
            block.state = BlockState::Saved;
            block
        })
        .collect()
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
    notebook.tokens.retain(|token| seen.insert(token.agent.clone()));
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
            assert!(typed.contains(&marker.head) && typed.contains(&marker.tail), "{preset:?}");
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

        let output = clean_output(&raw, &marker, &typed, Ended::AtTheMarker);

        assert_eq!(output.trim_end(), "uname -s && echo checks-passed\nLinux\nchecks-passed");
    }

    #[test]
    fn output_the_typed_ahead_marker_lands_on_is_kept_when_the_session_exits_first() {
        let marker = Marker::new();
        let typed = marker.command(Preset::Bash.marker_template());
        let raw = format!("uname -s; exit\r\n\x1b[?2004l\r{typed}Linux\r\nexit\r\n");

        let output = clean_output(&raw, &marker, &typed, Ended::Early);

        assert_eq!(output.trim_end(), "uname -s; exit\nLinux\nexit");
    }
}
