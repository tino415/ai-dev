use crate::html::{Keyboard, document, esc};
use crate::model::SessionDef;

pub const XTERM: &str = include_str!("static/xterm-6.0.0.js");
pub const XTERM_STYLE: &str = include_str!("static/xterm-6.0.0.css");
pub const XTERM_FIT: &str = include_str!("static/addon-fit-0.11.0.js");
pub const TERMINAL_SCRIPT: &str = include_str!("static/terminal.js");

const STYLE: &str = r#"
.terminal-page { display: flex; flex-direction: column; height: 100vh; height: 100dvh; }
.thead { display: flex; align-items: center; gap: 8px; padding: 8px 12px; border-bottom: 1px solid var(--line); background: var(--panel); }
.thead b { flex: 1; font-family: ui-monospace, monospace; font-size: 14px; }
.banner { background: var(--warn-bg); color: var(--warn); font-size: 13px; padding: 6px 12px; }
.screen { flex: 1; min-height: 0; overflow: hidden; background: #111111; }
.screen .xterm { padding: 4px; }
.screen.ended { opacity: .35; }
.keys { display: flex; flex-wrap: wrap; gap: 4px; padding: 6px; border-top: 1px solid var(--line); background: var(--panel); }
.keys button { flex: 1 0 13%; padding: 8px 0; }
.keys button.on { background: var(--agent); border-color: var(--agent); color: #fff; }
.ended-note { display: grid; gap: 12px; justify-items: center; padding: 24px 12px; text-align: center; color: var(--muted); }
[hidden] { display: none !important; }
"#;

const KEYS: [(&str, &str); 14] = [
    ("esc", "Esc"),
    ("tab", "Tab"),
    ("ctrl", "Ctrl"),
    ("up", "↑"),
    ("down", "↓"),
    ("left", "←"),
    ("right", "→"),
    ("ctrl-c", "^C"),
    ("ctrl-d", "^D"),
    ("|", "|"),
    ("~", "~"),
    ("/", "/"),
    ("-", "-"),
    ("pgup", "PgUp"),
];

pub fn page(def: &SessionDef, running: bool) -> String {
    let name = esc(&def.name);
    let (state, connection, dot) = if running {
        ("connecting", "connecting…", "dot")
    } else {
        ("stopped", "session stopped", "dot off")
    };
    let hidden_while_stopped = if running { "" } else { " hidden" };
    let hidden_while_running = if running { " hidden" } else { "" };
    let keys: String = KEYS
        .iter()
        .map(|(key, label)| {
            format!(
                "<button type=\"button\" data-key=\"{}\">{label}</button>",
                esc(key)
            )
        })
        .collect();
    document(
        &def.name,
        Keyboard::ShrinksThePage,
        &format!(
            "<link rel=\"stylesheet\" href=\"/static/xterm-6.0.0.css\"><style>{STYLE}</style>\
<div class=\"terminal-page\" id=\"terminal\" data-session=\"{name}\" data-state=\"{state}\">\
<div class=\"thead\"><span class=\"{dot}\" id=\"dot\"></span><b>{name}</b>\
<span class=\"policy\" id=\"connection\">{connection}</span><button type=\"button\" id=\"close\">Close</button></div>\
<div class=\"banner\" id=\"banner\" hidden></div>\
<div class=\"screen\" id=\"screen\"{hidden_while_stopped}></div>\
<div class=\"keys\" id=\"keys\"{hidden_while_stopped}>{keys}</div>\
<div class=\"ended-note\" id=\"stopped\"{hidden_while_running}>The session is not running.\
<form method=\"post\" action=\"/sessions/{name}/start\" id=\"start\"><button class=\"primary\">Start session</button></form>\
<span class=\"err\" id=\"start-error\"></span></div>\
<div class=\"ended-note\" id=\"disconnected\" hidden>The connection dropped.\
<button type=\"button\" class=\"primary\" id=\"reconnect\">Reconnect</button></div></div>\
<script src=\"/static/xterm-6.0.0.js\"></script><script src=\"/static/addon-fit-0.11.0.js\"></script>\
<script src=\"/static/terminal.js\"></script>"
        ),
    )
}
