use crate::core::Core;
use crate::html::esc;
use crate::model::*;

pub enum Part {
    Block(String),
    Sessions,
    Variables,
    Agents,
    Proposals,
    Composer,
}

pub async fn layout(core: &Core, notebook: &str) -> String {
    let version = core.changes_so_far();
    let blocks: String = core
        .blocks(notebook)
        .iter()
        .map(|block| block_view(version, core, block))
        .collect();
    let file_error = core
        .notebook_file_error(notebook)
        .map(|error| format!("<p class=\"why stopped\">{}</p>", esc(&error)))
        .unwrap_or_default();
    format!(
        "<div class=\"layout\" hx-ext=\"sse\" sse-connect=\"/n/{n}/events?since={version}\"><aside>{sessions}{variables}{agents}</aside>\
<main><p class=\"note\">Blue edge: an agent wrote it. Green edge: you wrote it. Nothing reaches a session before Run, \
and no output reaches an agent before \"Let agent read\", unless the session's policy says auto.</p>\
<div hidden data-stale sse-swap=\"stale\" hx-swap=\"none\"></div>{file_error}{proposals}\
<div id=\"blocks\" data-append sse-swap=\"block-added\" hx-target=\"this\" hx-swap=\"beforeend\">{blocks}</div>{composer}</main></div>",
        n = esc(notebook),
        sessions = render(core, notebook, Part::Sessions).await,
        variables = render(core, notebook, Part::Variables).await,
        agents = render(core, notebook, Part::Agents).await,
        proposals = render(core, notebook, Part::Proposals).await,
        composer = render(core, notebook, Part::Composer).await,
    )
}

pub fn subtitle(core: &Core, notebook: &str) -> String {
    format!("ai-dev · file: {}", core.notebook_file(notebook).display())
}

pub async fn render(core: &Core, notebook: &str, part: Part) -> String {
    let version = core.changes_so_far();
    match part {
        Part::Block(id) => core
            .block(&id)
            .map(|block| block_view(version, core, &block))
            .unwrap_or_default(),
        Part::Sessions => sessions_panel(version, core, notebook, &core.sessions().await),
        Part::Variables => variables_panel(version, core, notebook),
        Part::Agents => agents_panel(version, core, notebook, &core.session_defs()),
        Part::Proposals => proposals_view(version, core, notebook),
        Part::Composer => composer(version, core, notebook, &core.session_defs()),
    }
}

fn live(id: &str, event: &str, version: u64) -> String {
    format!(
        "id=\"{id}\" data-guard data-v=\"{version}\" sse-swap=\"{event}\" hx-target=\"this\" hx-swap=\"outerHTML\""
    )
}

// sessions

fn sessions_panel(
    version: u64,
    core: &Core,
    notebook: &str,
    sessions: &[(SessionDef, bool)],
) -> String {
    let used = core
        .notebook(notebook)
        .map(|n| n.sessions)
        .unwrap_or_default();
    let rows: String = sessions
        .iter()
        .filter(|(def, _)| used.contains(&def.name))
        .map(|(def, running)| session_row(core, notebook, def, *running))
        .collect();
    let rows = if rows.is_empty() {
        "<p class=\"policy\">None yet. A block's session joins this list when the block is added.</p>".to_string()
    } else {
        rows
    };
    let saved: String = sessions
        .iter()
        .filter(|(def, _)| !used.contains(&def.name))
        .map(|(def, _)| format!("<option>{}</option>", esc(&def.name)))
        .collect();
    let use_saved = if saved.is_empty() {
        String::new()
    } else {
        format!(
            "<select name=\"session\">{saved}</select><button hx-post=\"/n/{}/sessions\" hx-include=\"closest .row\" \
data-working=\"Adding…\">+ use a saved session</button>",
            esc(notebook)
        )
    };
    format!(
        "<div class=\"panel\" {live}><h2>Sessions this notebook uses</h2>{rows}\
<div class=\"row\">{use_saved}<a href=\"/sessions\">+ new definition</a></div></div>",
        live = live("sessions", "sessions", version),
    )
}

fn session_row(core: &Core, notebook: &str, def: &SessionDef, running: bool) -> String {
    let (toggle, working) = toggle_label(running);
    format!(
        "<div class=\"session\"><div>{heading}<div class=\"policy\">{policy}{stopped}</div>{attach}</div>\
<button hx-post=\"/n/{nb}/sessions/{n}/{toggle}\" data-working=\"{working}\">{toggle}</button></div>",
        heading = session_heading(def, running),
        attach = attach_line(core, def, running),
        n = esc(&def.name),
        nb = esc(notebook),
        policy = policy_text(def),
        stopped = if running { "" } else { " · not running" },
    )
}

pub fn session_heading(def: &SessionDef, running: bool) -> String {
    format!(
        "<span class=\"dot{off}\"></span><b>{n}</b> <span class=\"kind\">preset {preset}</span>",
        off = if running { "" } else { " off" },
        n = esc(&def.name),
        preset = preset_name(def),
    )
}

pub fn attach_line(core: &Core, def: &SessionDef, running: bool) -> String {
    if !running {
        return String::new();
    }
    format!(
        "<div class=\"policy\"><code>tmux -L {socket} attach -t {n}</code> \
<a href=\"/sessions/{n}/terminal\" target=\"_blank\" rel=\"noopener\">Terminal ↗</a></div>",
        socket = esc(core.tmux_socket()),
        n = esc(&def.name)
    )
}

pub fn toggle_label(running: bool) -> (&'static str, &'static str) {
    if running {
        ("stop", "Stopping…")
    } else {
        ("start", "Starting…")
    }
}

pub fn preset_name(def: &SessionDef) -> String {
    [Preset::Bash, Preset::Iex, Preset::Psql, Preset::Mysql]
        .into_iter()
        .find(|preset| preset.marker_template() == def.marker_template)
        .map(|preset| format!("{preset:?}").to_lowercase())
        .unwrap_or_else(|| "custom".to_string())
}

pub fn policy_text(def: &SessionDef) -> String {
    let run = match def.run_policy {
        Policy::Ask => "ask to run",
        Policy::Auto => "auto run",
    };
    let read = match def.read_policy {
        Policy::Ask => "ask to read",
        Policy::Auto => "auto read",
    };
    format!("{run} · {read}")
}

// variables

fn variables_panel(version: u64, core: &Core, notebook: &str) -> String {
    let profile = core.notebook(notebook).and_then(|n| n.profile);
    let variables = core.variables();
    let profiles: String = std::iter::once(String::new())
        .chain(variables.profiles.keys().cloned())
        .map(|name| {
            let selected = if profile.as_deref().unwrap_or_default() == name {
                " selected"
            } else {
                ""
            };
            let label = if name.is_empty() {
                "none".to_string()
            } else {
                esc(&name)
            };
            format!(
                "<option value=\"{}\"{selected}>{label}</option>",
                esc(&name)
            )
        })
        .collect();
    let overrides = variables
        .notebooks
        .get(notebook)
        .cloned()
        .unwrap_or_default();
    let rows: String = core
        .notebook_variables(notebook)
        .iter()
        .map(|(name, variable)| {
            let value = if variable.secret {
                "<td class=\"secret\">secret: agent sees the name only</td>".to_string()
            } else {
                format!("<td>{}</td>", esc(&variable.value))
            };
            let source = if overrides.contains_key(name) {
                format!(
                    "<input type=\"hidden\" name=\"name\" value=\"{n}\"><button class=\"no\" hx-post=\"/n/{nb}/var/delete\" \
hx-include=\"closest tr\" data-working=\"…\">remove</button>",
                    n = esc(name),
                    nb = esc(notebook)
                )
            } else {
                "<span class=\"policy\">profile</span>".to_string()
            };
            format!("<tr><td>{}</td>{value}<td>{source}</td></tr>", esc(name))
        })
        .collect();
    format!(
        "<div class=\"panel\" {live}><h2>Variables</h2>\
<div class=\"row\"><span class=\"policy\">profile</span><select name=\"profile\">{profiles}</select>\
<button hx-post=\"/n/{nb}/profile\" hx-include=\"closest .row\" data-working=\"Saving…\">Set</button></div>\
<table class=\"vars\">{rows}</table>\
<div class=\"row\"><input name=\"name\" placeholder=\"name\"><input name=\"value\" placeholder=\"value\">\
<label class=\"policy\"><input type=\"checkbox\" name=\"secret\" value=\"1\"> secret</label>\
<button hx-post=\"/n/{nb}/var\" hx-include=\"closest .row\" data-working=\"Saving…\">+ variable</button></div></div>",
        live = live("variables", "variables", version),
        nb = esc(notebook),
    )
}

// agents

fn agents_panel(version: u64, core: &Core, notebook: &str, sessions: &[SessionDef]) -> String {
    let joins: String = core
        .pending_joins(notebook)
        .iter()
        .map(|join| {
            format!(
                "<div class=\"agent-row\" id=\"join-{id}\"><span><b>{agent}</b> asks to join</span><span>\
<button class=\"ok\" hx-post=\"/joins/{id}/approve\" data-working=\"Admitting…\">Admit</button> \
<button class=\"no\" hx-post=\"/joins/{id}/deny\" data-working=\"Denying…\">Deny</button></span></div>",
                id = join.id,
                agent = esc(&join.agent)
            )
        })
        .collect();
    let tokens = core
        .notebook(notebook)
        .map(|n| n.tokens)
        .unwrap_or_default();
    let admitted: String = tokens
        .iter()
        .map(|token| {
            format!(
                "<div class=\"agent-row admitted\"><span><b>{agent}</b> joined</span>\
<button class=\"no\" hx-post=\"/n/{nb}/tokens/{id}/revoke\" data-working=\"Revoking…\">revoke</button></div>",
                agent = esc(&token.agent),
                nb = esc(notebook),
                id = token.id
            )
        })
        .collect();
    let nobody = if joins.is_empty() && tokens.is_empty() {
        format!(
            "<div>no agent yet; one calls join_notebook(\"{}\", its name)</div>",
            esc(notebook)
        )
    } else {
        String::new()
    };
    let session_names: Vec<String> = sessions.iter().map(|def| esc(&def.name)).collect();
    let variable_names: Vec<String> = core
        .notebook_variables(notebook)
        .iter()
        .map(|(name, variable)| format!("{}{}", esc(name), if variable.secret { "*" } else { "" }))
        .collect();
    format!(
        "<div class=\"panel\" {live}><h2>What the agent sees (MCP)</h2><div class=\"mcp\">{joins}{admitted}{nobody}\
<div>list_sessions() → {sessions}</div><div>list_variables() → {variables}</div></div></div>",
        live = live("agents", "agents", version),
        sessions = session_names.join(", "),
        variables = variable_names.join(", "),
    )
}

// proposed sessions

fn proposals_view(version: u64, core: &Core, notebook: &str) -> String {
    let blocks: String = core
        .notebook_proposals(notebook)
        .iter()
        .map(proposal_block)
        .collect();
    format!(
        "<div {}>{blocks}</div>",
        live("proposals", "proposals", version)
    )
}

fn proposal_block(proposal: &SessionProposal) -> String {
    let def = &proposal.def;
    let start = labelled("start", &def.start_command);
    let setup = labelled("setup", &def.setup_commands.join("\n"));
    format!(
        "<div class=\"block agent\" id=\"p-{id}\"><div class=\"bhead\"><span class=\"who\">{by}</span> → <b>new session</b>\
<span class=\"state s-proposed\">proposed session</span></div><div class=\"why\">Why: {why}</div>\
<div class=\"label\">name · preset</div><pre>{name} · {preset}</pre>{start}{setup}\
<div class=\"label\">policy</div><pre>{policy}</pre>\
<div class=\"actions\"><button class=\"primary\" hx-post=\"/proposals/{id}/start\" data-working=\"Starting…\">Start session</button>\
<button class=\"no\" hx-post=\"/proposals/{id}/reject\" data-working=\"Rejecting…\">Reject</button></div></div>",
        id = proposal.id,
        by = esc(&proposal.by),
        why = esc(&proposal.why),
        name = esc(&def.name),
        preset = preset_name(def),
        policy = policy_text(def),
    )
}

fn labelled(label: &str, code: &str) -> String {
    if code.trim().is_empty() {
        String::new()
    } else {
        format!(
            "<div class=\"label\">{label}</div><pre>{}</pre>",
            code_html(code)
        )
    }
}

// blocks

fn block_view(version: u64, core: &Core, block: &Block) -> String {
    let live = live(
        &format!("b-{}", block.id),
        &format!("b-{}", block.id),
        version,
    );
    if block.state.is_dropped() {
        return format!("<div {live} hidden></div>");
    }
    let by_agent = block.proposed_by_agent();
    let (badge, state) = state_badge(block);
    let why = block
        .why
        .as_ref()
        .map(|why| format!("<div class=\"why\">Why: {}</div>", esc(why)))
        .unwrap_or_default();
    let agent_view = if by_agent {
        format!(
            "<div class=\"agentview\">read_block → {}</div>",
            esc(&what_the_agent_reads(block))
        )
    } else {
        String::new()
    };
    format!(
        "<div class=\"block {side}\" {live}><div class=\"bhead\"><span class=\"who\">{who}</span> → <b>{session}</b>\
<span class=\"state {badge}\">{state}</span></div>{why}{body}{output}{hold_read}{hidden_input}{agent_view}</div>",
        side = if by_agent { "agent" } else { "me" },
        who = if by_agent {
            esc(&block.author)
        } else {
            "me".to_string()
        },
        session = esc(&block.session),
        body = code_and_actions(block, reads_wait_for_a_click(core, block)),
        output = output_view(block),
        hold_read = hold_read_button(block),
        hidden_input = hidden_input_form(block),
    )
}

fn reads_wait_for_a_click(core: &Core, block: &Block) -> bool {
    block.proposed_by_agent()
        && core
            .session(&block.session)
            .is_some_and(|def| def.read_policy == Policy::Ask)
}

fn state_badge(block: &Block) -> (&'static str, String) {
    let outcome = if block.state == BlockState::Failed {
        "failed"
    } else {
        "ran"
    };
    let done = if block.state == BlockState::Failed {
        "s-bad"
    } else {
        "s-done"
    };
    match block.state {
        BlockState::Pending => ("s-proposed", "proposed".to_string()),
        BlockState::Saved => ("s-draft", "from the notebook file".to_string()),
        BlockState::Queued | BlockState::Running if block.release_when_done => (
            "s-proposed",
            format!("{} · agent reads when done", state_word(block.state)),
        ),
        BlockState::Queued | BlockState::Running => {
            ("s-proposed", state_word(block.state).to_string())
        }
        _ if !block.proposed_by_agent() => (done, outcome.to_string()),
        _ => match block.read {
            ReadDecision::Undecided => ("s-read", format!("{outcome} · waiting for read approval")),
            ReadDecision::Released => (done, format!("{outcome} · agent read")),
            ReadDecision::Hidden => (done, format!("{outcome} · kept hidden")),
        },
    }
}

fn state_word(state: BlockState) -> &'static str {
    if state == BlockState::Queued {
        "queued"
    } else {
        "running"
    }
}

fn code_and_actions(block: &Block, reads_wait_for_a_click: bool) -> String {
    if matches!(block.state, BlockState::Pending | BlockState::Saved) {
        let rows = block.code.lines().count().clamp(2, 20);
        let (run_class, note) = if block.proposed_by_agent() {
            (
                "primary",
                "<input name=\"note\" placeholder=\"note for the agent (optional)\">",
            )
        } else {
            ("ok", "")
        };
        let run_and_release = if reads_wait_for_a_click {
            format!(
                "<button class=\"ok\" hx-post=\"/b/{id}/run-and-release\" hx-include=\"closest .block\" data-working=\"Running…\">Run and let read</button>",
                id = block.id
            )
        } else {
            String::new()
        };
        return format!(
            "<textarea name=\"code\" class=\"code\" rows=\"{rows}\" spellcheck=\"false\">{code}</textarea>\
<div class=\"actions\"><button class=\"{run_class}\" hx-post=\"/b/{id}/run\" hx-include=\"closest .block\" data-working=\"Running…\">Run</button>{run_and_release}\
<button class=\"no\" hx-post=\"/b/{id}/reject\" hx-include=\"closest .block\" data-working=\"Rejecting…\">Reject</button>{note}</div>{stopped}",
            code = esc(&block.code),
            id = block.id,
            stopped = session_stopped_line(block),
        );
    }
    let edited = block
        .proposed_code
        .as_ref()
        .map(|original| {
            format!(
                "<div class=\"label\">the agent proposed</div><pre>{}</pre>",
                code_html(original)
            )
        })
        .unwrap_or_default();
    let note = block
        .note
        .as_ref()
        .map(|note| format!("<div class=\"why\">Note to the agent: {}</div>", esc(note)))
        .unwrap_or_default();
    let rerun = if matches!(block.state, BlockState::Finished | BlockState::Failed) {
        format!(
            "<div class=\"actions\"><button hx-post=\"/b/{id}/rerun\" data-working=\"Running…\">Run again</button>\
<button class=\"no\" hx-post=\"/b/{id}/delete\" hx-confirm=\"Remove this block and its output from the notebook? The session log keeps what it printed.\" data-working=\"Deleting…\">Delete</button></div>",
            id = block.id
        )
    } else {
        String::new()
    };
    format!("<pre>{}</pre>{edited}{note}{rerun}", code_html(&block.code))
}

fn hold_read_button(block: &Block) -> String {
    let queued_or_running = matches!(block.state, BlockState::Queued | BlockState::Running);
    if !(block.release_when_done && queued_or_running) {
        return String::new();
    }
    format!(
        "<div class=\"actions\"><button hx-post=\"/b/{id}/hold-read\" data-working=\"…\">Ask me before the agent reads</button></div>",
        id = block.id
    )
}

fn hidden_input_form(block: &Block) -> String {
    if block.state != BlockState::Running {
        return String::new();
    }
    format!(
        "<form class=\"actions\" hx-post=\"/b/{id}/input\" autocomplete=\"off\">\
<input type=\"password\" name=\"value\" placeholder=\"hidden input, such as a password\" autocomplete=\"off\" required>\
<button type=\"submit\" data-working=\"Sending…\">Send</button>\
<button class=\"no\" type=\"button\" hx-post=\"/b/{id}/interrupt\" hx-params=\"none\" data-working=\"Interrupting…\">Interrupt</button></form>",
        id = block.id
    )
}

fn session_stopped_line(block: &Block) -> String {
    if !block.session_stopped {
        return String::new();
    }
    format!(
        "<div class=\"why stopped\">Session {session} is not running</div>\
<div class=\"actions\"><button class=\"primary\" hx-post=\"/b/{id}/start-and-run\" data-working=\"Starting…\">Start {session} and run</button>\
<button class=\"no\" hx-post=\"/b/{id}/cancel-start\" data-working=\"…\">Cancel</button></div>",
        session = esc(&block.session),
        id = block.id,
    )
}

fn output_view(block: &Block) -> String {
    let Some(output) = &block.output else {
        return String::new();
    };
    let undecided = block.proposed_by_agent() && block.read == ReadDecision::Undecided;
    let running = block.state == BlockState::Running;
    let label = match (running, undecided) {
        (true, _) => "output so far (only you see it)",
        (false, true) => "output (only you see it so far)",
        (false, false) => "output",
    };
    let failed = if block.state == BlockState::Failed {
        " failed"
    } else {
        ""
    };
    let actions = if running {
        String::new()
    } else if undecided {
        format!(
            "<div class=\"actions\"><button class=\"ok\" hx-post=\"/b/{id}/release\" data-working=\"Releasing…\">Let agent read</button>\
<button class=\"no\" hx-post=\"/b/{id}/hide\" data-working=\"Hiding…\">Keep hidden</button></div>",
            id = block.id
        )
    } else {
        String::new()
    };
    format!(
        "<div class=\"label\">{label}</div><pre class=\"out{failed}\">{}</pre>{actions}",
        esc(output)
    )
}

fn what_the_agent_reads(block: &Block) -> String {
    match (block.state, block.read, &block.output) {
        (BlockState::Pending | BlockState::Saved, _, _) => "waiting for you to run it".to_string(),
        (BlockState::Queued | BlockState::Running, _, _) => {
            format!("{:?}, no output yet", block.state).to_lowercase()
        }
        (_, ReadDecision::Released, Some(_)) => "the output above".to_string(),
        (_, ReadDecision::Hidden, Some(_)) => "\"the owner kept the output hidden\"".to_string(),
        (_, _, Some(_)) => "\"finished, waiting for the owner to release the output\"".to_string(),
        (_, _, None) => "no output".to_string(),
    }
}

// composer

fn composer(version: u64, core: &Core, notebook: &str, sessions: &[SessionDef]) -> String {
    let live = live("composer", "composer", version);
    if sessions.is_empty() {
        return format!(
            "<div class=\"block me\" {live}><div class=\"bhead\"><span class=\"who\">me</span></div>\
<p class=\"why\">No sessions yet. <a href=\"/sessions\">Define one</a> to run blocks in it.</p><p></p></div>"
        );
    }
    let chosen = session_i_last_ran_in(core, notebook)
        .or_else(|| {
            core.notebook(notebook)
                .and_then(|n| n.sessions.first().cloned())
        })
        .unwrap_or_else(|| sessions[0].name.clone());
    let options: String = sessions
        .iter()
        .map(|def| {
            let selected = if def.name == chosen { " selected" } else { "" };
            format!("<option{selected}>{}</option>", esc(&def.name))
        })
        .collect();
    format!(
        "<div class=\"block me\" {live}><div class=\"bhead\"><span class=\"who\">me</span> → <select name=\"session\">{options}</select>\
<span class=\"state s-draft\">draft</span></div>\
<textarea name=\"code\" class=\"code\" rows=\"3\" spellcheck=\"false\" placeholder=\"code to run; {{{{name}}}} fills a variable\"></textarea>\
<div class=\"actions\"><button class=\"ok\" hx-post=\"/n/{nb}/block\" hx-include=\"closest .block\" data-working=\"Running…\">Run</button></div></div>",
        nb = esc(notebook),
    )
}

fn session_i_last_ran_in(core: &Core, notebook: &str) -> Option<String> {
    core.blocks(notebook)
        .into_iter()
        .rev()
        .filter(|block| !block.state.is_dropped())
        .find(|block| block.author == HUMAN)
        .map(|block| block.session)
}

fn code_html(code: &str) -> String {
    let mut html = String::new();
    let mut rest = code;
    while let Some(start) = rest.find("{{") {
        let Some(length) = rest[start..].find("}}") else {
            break;
        };
        let end = start + length + 2;
        html.push_str(&esc(&rest[..start]));
        html.push_str(&format!(
            "<span class=\"var\">{}</span>",
            esc(&rest[start..end])
        ));
        rest = &rest[end..];
    }
    html.push_str(&esc(rest));
    html
}
