const STYLE: &str = r#"
:root {
  --bg: #f6f5f2; --panel: #ffffff; --ink: #1d1d1b; --muted: #6b6a66; --line: #e2e0da;
  --agent: #3b5bdb; --agent-bg: #eef1fd; --me: #2b8a3e; --me-bg: #ebf7ee;
  --warn: #b35c00; --warn-bg: #fff4e6; --bad: #c92a2a; --code: #f1f0ec;
}
@media (prefers-color-scheme: dark) {
  :root:not([data-theme="light"]) {
    --bg: #161615; --panel: #1f1f1d; --ink: #ecebe6; --muted: #9c9a94; --line: #34332f;
    --agent: #91a7ff; --agent-bg: #232a45; --me: #8ce99a; --me-bg: #1e3324;
    --warn: #ffa94d; --warn-bg: #3a2a17; --bad: #ff8787; --code: #262624;
  }
}
* { box-sizing: border-box; }
body { margin: 0; background: var(--bg); color: var(--ink); font: 15px/1.45 system-ui, sans-serif; }
a { color: var(--agent); }
header { padding: 12px 16px; border-bottom: 1px solid var(--line); background: var(--panel); display: flex; gap: 12px; align-items: baseline; flex-wrap: wrap; }
header h1 { font-size: 17px; margin: 0; }
header .sub { color: var(--muted); font-size: 13px; }
header nav { margin-left: auto; display: flex; gap: 12px; align-items: baseline; font-size: 13px; }
header nav form { margin: 0; }
.layout { display: grid; grid-template-columns: 300px 1fr; gap: 16px; padding: 16px; max-width: 1200px; margin: 0 auto; }
@media (max-width: 800px) { .layout { grid-template-columns: 1fr; } }
.single { padding: 16px; max-width: 900px; margin: 0 auto; }
footer.meta { margin-top: 2rem; padding: 8px 16px; border-top: 1px solid var(--line); color: var(--muted); font-size: 12px; }
.panel { background: var(--panel); border: 1px solid var(--line); border-radius: 10px; padding: 12px; margin-bottom: 16px; }
.panel h2 { font-size: 13px; text-transform: uppercase; letter-spacing: .05em; color: var(--muted); margin: 0 0 8px; }
.session { display: flex; justify-content: space-between; align-items: center; padding: 6px 0; border-top: 1px solid var(--line); gap: 8px; }
.session:first-of-type { border-top: 0; }
.session b { font-family: ui-monospace, monospace; font-size: 14px; }
.kind { font-size: 11px; padding: 1px 6px; border-radius: 4px; background: var(--code); color: var(--muted); }
.dot { width: 8px; height: 8px; border-radius: 50%; background: var(--me); display: inline-block; margin-right: 6px; }
.dot.off { background: var(--muted); }
.policy { font-size: 12px; color: var(--muted); }
.policy code { font-size: 11px; word-break: break-all; }
table.vars { width: 100%; border-collapse: collapse; font-size: 13px; }
table.vars td { padding: 4px 0; border-top: 1px solid var(--line); vertical-align: top; }
table.vars td:first-child { font-family: ui-monospace, monospace; padding-right: 8px; white-space: nowrap; }
table.vars td:nth-child(2) { font-family: ui-monospace, monospace; color: var(--muted); word-break: break-all; }
table.vars td:nth-child(3) { text-align: right; }
.secret { color: var(--warn) !important; }
.block { background: var(--panel); border: 1px solid var(--line); border-radius: 10px; margin-bottom: 16px; overflow: hidden; }
.block.agent { border-left: 4px solid var(--agent); }
.block.me { border-left: 4px solid var(--me); }
.bhead { display: flex; gap: 8px; align-items: center; flex-wrap: wrap; padding: 8px 12px; font-size: 13px; border-bottom: 1px solid var(--line); }
.who { font-weight: 600; }
.agent .who { color: var(--agent); }
.me .who { color: var(--me); }
.state { margin-left: auto; font-size: 12px; padding: 2px 8px; border-radius: 10px; }
.s-proposed { background: var(--agent-bg); color: var(--agent); }
.s-read { background: var(--warn-bg); color: var(--warn); }
.s-done { background: var(--me-bg); color: var(--me); }
.s-bad { background: var(--warn-bg); color: var(--bad); }
.s-draft { background: var(--code); color: var(--muted); }
.why { padding: 8px 12px 0; font-size: 13px; color: var(--muted); }
pre { margin: 8px 12px; padding: 10px; background: var(--code); border-radius: 6px; overflow-x: auto; font: 13px/1.4 ui-monospace, monospace; white-space: pre-wrap; }
pre .var { color: var(--agent); font-weight: 600; }
pre.out { background: transparent; border: 1px dashed var(--line); }
pre.out.failed { color: var(--bad); }
textarea.code { display: block; width: calc(100% - 24px); margin: 8px 12px; padding: 10px; background: var(--code); color: var(--ink); border: 1px solid transparent; border-radius: 6px; font: 13px/1.4 ui-monospace, monospace; resize: vertical; }
textarea.code:focus { border-color: var(--agent); outline: none; }
.label { font-size: 11px; text-transform: uppercase; letter-spacing: .05em; color: var(--muted); margin: 8px 12px 0; }
.actions { display: flex; gap: 8px; flex-wrap: wrap; align-items: center; padding: 4px 12px 12px; }
.actions input { flex: 1; min-width: 12rem; }
input, select, textarea { font: inherit; font-size: 13px; padding: 5px 8px; border-radius: 6px; border: 1px solid var(--line); background: var(--panel); color: var(--ink); }
button { font: inherit; font-size: 13px; padding: 6px 12px; border-radius: 6px; border: 1px solid var(--line); background: var(--panel); color: var(--ink); cursor: pointer; }
button:disabled { opacity: .6; cursor: progress; }
button.primary { background: var(--agent); border-color: var(--agent); color: #fff; }
button.ok { background: var(--me); border-color: var(--me); color: #fff; }
button.no { color: var(--bad); }
.agentview { font: 12px/1.4 ui-monospace, monospace; background: var(--code); border-radius: 6px; padding: 8px 10px; margin: 0 12px 12px; color: var(--muted); }
.note { color: var(--muted); font-size: 13px; margin: 0 0 16px; }
.mcp { font: 12px/1.5 ui-monospace, monospace; }
.mcp > div { padding: 2px 0; }
.mcp .agent-row { display: flex; justify-content: space-between; align-items: center; gap: 8px; border-top: 1px solid var(--line); padding: 6px 0; font-family: system-ui, sans-serif; font-size: 13px; }
.row { display: flex; gap: 8px; flex-wrap: wrap; align-items: center; margin-top: 8px; }
.row input[name=name], .row input[name=value] { flex: 1; min-width: 6rem; }
.stack { display: grid; gap: 8px; }
.flash { position: fixed; left: 16px; right: 16px; bottom: 16px; max-width: 760px; margin: 0 auto; background: var(--panel); border: 1px solid var(--bad); border-radius: 10px; padding: 8px 12px; display: flex; gap: 8px; align-items: flex-start; box-shadow: 0 4px 16px rgba(0,0,0,.15); }
.flash[hidden] { display: none; }
.flash pre { flex: 1; margin: 0; color: var(--bad); background: transparent; padding: 4px 0; }
.err { color: var(--bad); }
code { font: 12px ui-monospace, monospace; background: var(--code); padding: 1px 4px; border-radius: 4px; }
"#;

pub const HTMX: &str = include_str!("static/htmx-2.0.11.min.js");
pub const HTMX_SSE: &str = include_str!("static/htmx-ext-sse-2.2.4.js");
pub const PAGE_SCRIPT: &str = include_str!("static/ai-dev.js");

const SCRIPTS: &str = "<script src=\"/static/htmx-2.0.11.min.js\"></script>\
<script src=\"/static/htmx-ext-sse-2.2.4.js\"></script><script src=\"/static/ai-dev.js\"></script>";

const NAV: &str = "<nav><a href=\"/\">Notebooks</a><a href=\"/sessions\">Sessions</a><a href=\"/profiles\">Profiles</a>\
<form method=\"post\" action=\"/logout\"><button>Sign out</button></form></nav>";

const FLASH: &str =
    "<div id=\"flash\" class=\"flash\" role=\"alert\" hidden><pre></pre><button type=\"button\">Close</button></div>";

pub fn esc(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

pub fn page(title: &str, body: &str) -> String {
    document(
        title,
        &format!(
            "<header><h1>{}</h1>{NAV}</header><main class=\"single\">{body}</main>{}{FLASH}{SCRIPTS}",
            esc(title),
            footer()
        ),
    )
}

pub fn notebook_page(title: &str, sub: &str, layout: &str) -> String {
    document(
        title,
        &format!(
            "<header><h1>{}</h1><span class=\"sub\">{}</span>{NAV}</header>{layout}{}{FLASH}{SCRIPTS}",
            esc(title),
            esc(sub),
            footer()
        ),
    )
}

pub fn sign_in_page(title: &str, body: &str, script: &str) -> String {
    document(
        title,
        &format!(
            "<header><h1>ai-dev</h1></header><main class=\"single\">{body}</main>{}<script>{script}</script>",
            footer()
        ),
    )
}

const COMMIT_URL: &str = "https://github.com/tino415/ai-dev/commit/";

fn footer() -> String {
    footer_for(option_env!("AI_DEV_REV"))
}

fn footer_for(rev: Option<&str>) -> String {
    let version = concat!("ai-dev ", env!("CARGO_PKG_VERSION"));
    match rev {
        Some(rev) => {
            let (commit, dirty) = rev.strip_suffix("-dirty").map_or((rev, ""), |commit| (commit, "-dirty"));
            let short: String = commit.chars().take(7).collect();
            format!(
                "<footer class=\"meta\">{version} · <a href=\"{COMMIT_URL}{}\">{}{dirty}</a></footer>",
                esc(commit),
                esc(&short)
            )
        }
        None => format!("<footer class=\"meta\">{version}</footer>"),
    }
}

#[cfg(test)]
mod tests {
    use super::footer_for;

    #[test]
    fn a_clean_rev_shows_seven_characters_linking_to_its_commit() {
        assert_eq!(
            footer_for(Some("89356b3c0ffee0000000000000000000000000ab")),
            "<footer class=\"meta\">ai-dev 0.1.0 · <a href=\"https://github.com/tino415/ai-dev/commit/89356b3c0ffee0000000000000000000000000ab\">89356b3</a></footer>"
        );
    }

    #[test]
    fn a_dirty_rev_says_dirty_and_links_to_the_commit_it_started_from() {
        assert_eq!(
            footer_for(Some("89356b3c0ffee0000000000000000000000000ab-dirty")),
            "<footer class=\"meta\">ai-dev 0.1.0 · <a href=\"https://github.com/tino415/ai-dev/commit/89356b3c0ffee0000000000000000000000000ab\">89356b3-dirty</a></footer>"
        );
    }

    #[test]
    fn no_rev_shows_the_version_alone() {
        assert_eq!(footer_for(None), "<footer class=\"meta\">ai-dev 0.1.0</footer>");
    }
}

pub fn document(title: &str, body: &str) -> String {
    format!(
        "<!doctype html><html lang=\"en\"><head><meta charset=\"utf-8\">\
<meta name=\"viewport\" content=\"width=device-width,initial-scale=1\">\
<meta name=\"theme-color\" content=\"#3b5bdb\" media=\"(prefers-color-scheme: light)\">\
<meta name=\"theme-color\" content=\"#161615\" media=\"(prefers-color-scheme: dark)\">\
<link rel=\"manifest\" href=\"/static/manifest.webmanifest\">\
<link rel=\"icon\" href=\"/static/icon-192.png\">\
<link rel=\"apple-touch-icon\" href=\"/static/icon-192.png\">\
<title>{} · ai-dev</title><style>{STYLE}</style></head><body>{body}</body></html>",
        esc(title)
    )
}
