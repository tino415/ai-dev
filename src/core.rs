use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use sha2::{Digest, Sha256};

use crate::model::*;
use crate::notebook_file;
use crate::tmux::{strip_terminal_escapes, Tmux};
use crate::vars;

pub type Result<T> = std::result::Result<T, String>;

pub const BLOCK_TIMEOUT: Duration = Duration::from_secs(120);

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
        let persisted: Persisted = read_json(&dir.join("state.json"))?;
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
        self.save_state(&state)
    }

    // sessions

    pub async fn sessions(&self) -> Vec<(SessionDef, bool)> {
        let defs = self.state().persisted.sessions.clone();
        let mut listed = Vec::new();
        for def in defs {
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
        self.save_state(&state)
    }

    pub async fn delete_session(&self, name: &str) -> Result<()> {
        self.stop_session(name).await;
        let mut state = self.state();
        state.persisted.sessions.retain(|s| s.name != name);
        self.save_state(&state)
    }

    pub async fn is_running(&self, name: &str) -> bool {
        self.tmux.is_alive(name).await
    }

    pub async fn start_session(&self, name: &str) -> Result<()> {
        let def = self.session(name).ok_or("unknown session")?;
        if self.is_running(name).await {
            self.touch(name);
            return Ok(());
        }
        self.tmux.start(name).await?;
        self.touch(name);
        let lock = self.session_lock(name);
        let _guard = lock.lock().await;
        match self.run_start_sequence(&def).await {
            Ok(()) => Ok(()),
            Err(error) => {
                self.stop_session(name).await;
                Err(error)
            }
        }
    }

    async fn run_start_sequence(&self, def: &SessionDef) -> Result<()> {
        let set = self.effective_variables(def.profile.as_deref(), "");
        if !def.start_command.is_empty() {
            let command = vars::render(&def.start_command, &set)?;
            self.tmux.type_lines(&def.name, &command).await?;
        }
        for setup in &def.setup_commands {
            self.run_text(def, &set, setup)
                .await
                .map_err(|error| format!("setup failed: {error}"))?;
        }
        Ok(())
    }

    pub async fn stop_session(&self, name: &str) {
        self.tmux.stop(name).await;
        self.state().last_activity.remove(name);
    }

    pub async fn reap_idle_sessions(&self) {
        let now = Instant::now();
        let defs = self.state().persisted.sessions.clone();
        for def in defs {
            if !self.tmux.is_alive(&def.name).await {
                self.state().last_activity.remove(&def.name);
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

    pub fn propose_session(&self, notebook: &str, agent: &str, def: SessionDef) -> Result<SessionProposal> {
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
            def,
        };
        self.state().proposals.push(proposal.clone());
        Ok(proposal)
    }

    pub fn proposals(&self) -> Vec<SessionProposal> {
        self.state().proposals.clone()
    }

    pub async fn approve_proposal(&self, id: &str) -> Result<()> {
        let proposal = self.take_proposal(id)?;
        let name = proposal.def.name.clone();
        if self.session(&name).is_some() {
            return Err(format!("session {name} already exists"));
        }
        self.save_session(proposal.def)?;
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
        Ok(state.proposals.remove(position))
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
        let mut state = self.state();
        let policy = state
            .persisted
            .sessions
            .iter()
            .find(|s| s.name == session)
            .map(|s| s.run_policy)
            .ok_or("unknown session")?;
        let entry = notebook_mut(&mut state, notebook)?;
        if !entry.sessions.iter().any(|s| s == session) {
            entry.sessions.push(session.to_string());
        }
        let mut block = Block::new(notebook, session, &normalize_newlines(code), author);
        let auto = block.proposed_by_agent() && policy == Policy::Auto;
        if auto {
            block.state = BlockState::Queued;
        }
        state.blocks.push(block.clone());
        self.save_state(&state)?;
        self.save_notebook(&state, notebook)?;
        drop(state);
        if auto {
            self.spawn_execution(&block.id);
        }
        Ok(block)
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
        let notebook = block.notebook.clone();
        self.save_notebook(&state, &notebook)?;
        drop(state);
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
        self.save_notebook(&state, &notebook)
    }

    pub fn decide_read(&self, id: &str, decision: ReadDecision) -> Result<()> {
        let mut state = self.state();
        block_mut(&mut state, id)?.read = decision;
        Ok(())
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
    }

    fn set_block_state(&self, id: &str, new_state: BlockState) {
        if let Ok(block) = block_mut(&mut self.state(), id) {
            block.state = new_state;
        }
    }

    async fn run_text(&self, def: &SessionDef, set: &VariableSet, code: &str) -> Result<String> {
        if !self.is_running(&def.name).await {
            return Err(format!("session {} is not running", def.name));
        }
        let rendered = vars::render(code, set)?;
        let marker = Marker::new();
        let offset = self.tmux.log_len(&def.name);
        self.tmux.type_lines(&def.name, &rendered).await?;
        self.tmux
            .type_lines(&def.name, &marker.command(&def.marker_template))
            .await?;
        self.touch(&def.name);
        let deadline = Instant::now() + self.block_timeout;
        loop {
            self.touch(&def.name);
            let text = self.tmux.log_since(&def.name, offset);
            if let Some(end) = text.find(&marker.full()) {
                self.touch(&def.name);
                return Ok(vars::mask(&clean_output(&text[..end], &marker), set));
            }
            if Instant::now() >= deadline {
                let partial = clean_output(&text, &marker);
                return Err(format!(
                    "no end marker after {}s, the session may still be busy\n{partial}",
                    self.block_timeout.as_secs()
                ));
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
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
        notebook_mut(&mut state, &notebook)?.tokens.push(NotebookToken {
            id: short_id(),
            agent,
            hash: hash_token(&token),
        });
        self.save_state(&state)
    }

    pub fn deny_join(&self, id: &str) -> Result<()> {
        self.state().joins.retain(|j| j.id != id);
        Ok(())
    }

    pub fn revoke_token(&self, notebook: &str, token_id: &str) -> Result<()> {
        let mut state = self.state();
        notebook_mut(&mut state, notebook)?
            .tokens
            .retain(|t| t.id != token_id);
        self.save_state(&state)
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
        write_json(&self.dir.join("variables.json"), &state.vars)
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

fn clean_output(raw: &str, marker: &Marker) -> String {
    strip_terminal_escapes(raw)
        .lines()
        .filter(|line| !line.contains(&marker.head))
        .collect::<Vec<_>>()
        .join("\n")
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
            block.state = BlockState::Saved;
            block
        })
        .collect()
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
}
