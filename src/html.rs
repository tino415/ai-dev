const STYLE: &str = r#"
body{font:15px/1.45 system-ui,sans-serif;max-width:60rem;margin:0 auto;padding:1rem;color:#1b1b1b;background:#fafafa}
nav{display:flex;gap:1rem;margin-bottom:1rem;border-bottom:1px solid #ddd;padding-bottom:.5rem}
pre{background:#f0f0f0;padding:.6rem;overflow:auto;white-space:pre-wrap;margin:.3rem 0}
textarea,input,select{font:inherit;padding:.3rem;max-width:100%}
textarea{width:100%;box-sizing:border-box;min-height:5rem;font-family:ui-monospace,monospace}
.block{border:1px solid #ccc;background:#fff;padding:.6rem;margin:.6rem 0;border-radius:4px}
.meta{color:#666;font-size:.85rem}
.state-pending{border-left:4px solid #d9a400}.state-running,.state-queued{border-left:4px solid #1c7ed6}
.state-finished{border-left:4px solid #2f9e44}.state-failed,.state-rejected{border-left:4px solid #c92a2a}
.err{color:#c92a2a}button{font:inherit;padding:.3rem .8rem;margin-right:.3rem;cursor:pointer}
table{border-collapse:collapse}td,th{padding:.2rem .6rem;text-align:left;border-bottom:1px solid #eee}
code{background:#f0f0f0;padding:0 .2rem}
"#;

pub fn esc(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

pub fn page(title: &str, body: &str, script: &str) -> String {
    let nav = if title == "Sign in" {
        String::new()
    } else {
        r#"<nav><a href="/">Notebooks</a><a href="/sessions">Sessions</a><a href="/profiles">Profiles</a>
<form method="post" action="/logout" style="margin-left:auto"><button>Sign out</button></form></nav>"#
            .to_string()
    };
    format!(
        "<!doctype html><html><head><meta charset=utf-8><meta name=viewport content=\"width=device-width,initial-scale=1\">\
<title>{} · ai-dev</title><style>{STYLE}</style></head><body>{nav}{body}<script>{script}</script></body></html>",
        esc(title)
    )
}
