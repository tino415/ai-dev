use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use chromiumoxide::cdp::browser_protocol::emulation::SetDeviceMetricsOverrideParams;
use chromiumoxide::cdp::browser_protocol::web_authn::{
    AddVirtualAuthenticatorParams, AuthenticatorProtocol, AuthenticatorTransport, EnableParams,
    VirtualAuthenticatorOptions,
};
use chromiumoxide::{Browser, BrowserConfig, Page};
use futures::StreamExt;
use serde_json::{Value, json};
use tempfile::TempDir;

static ONE_BROWSER_AT_A_TIME: Mutex<()> = Mutex::new(());

const WAIT: Duration = Duration::from_secs(20);

struct Server {
    dir: TempDir,
    child: Child,
    socket: String,
    base: String,
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = Command::new("tmux")
            .args(["-L", &self.socket, "kill-server"])
            .output();
    }
}

fn on_path(program: &str) -> PathBuf {
    std::env::var_os("PATH")
        .and_then(|path| {
            std::env::split_paths(&path)
                .map(|dir| dir.join(program))
                .find(|p| p.exists())
        })
        .unwrap_or_else(|| panic!("{program} is not on PATH; run the tests in nix develop"))
}

fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

impl Server {
    fn start() -> Server {
        let dir = tempfile::tempdir().unwrap();
        let port = free_port();
        let socket = format!("aidev-e2e-{port}");
        std::fs::write(dir.path().join("agent_token"), "e2e-agent-token").unwrap();
        let mut server = Server {
            child: Command::new(env!("CARGO_BIN_EXE_ai-dev"))
                .arg("serve")
                .env_clear()
                .envs(unit_environment(&dir, port, &socket))
                .stdout(Stdio::null())
                .spawn()
                .unwrap(),
            dir,
            socket,
            base: format!("http://localhost:{port}"),
        };
        server.wait_until_listening(port);
        server
    }

    fn wait_until_listening(&mut self, port: u16) {
        let deadline = Instant::now() + WAIT;
        while std::net::TcpStream::connect(("127.0.0.1", port)).is_err() {
            assert!(
                self.child.try_wait().unwrap().is_none(),
                "ai-dev exited at start"
            );
            assert!(Instant::now() < deadline, "ai-dev never listened on {port}");
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    fn enroll_code(&self) -> String {
        let port = self.base.rsplit(':').next().unwrap().parse().unwrap();
        let output = Command::new(env!("CARGO_BIN_EXE_ai-dev"))
            .arg("enroll")
            .env_clear()
            .envs(unit_environment(&self.dir, port, &self.socket))
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8(output.stdout).unwrap().trim().to_string()
    }

    fn url(&self, path: &str) -> String {
        format!("{}{path}", self.base)
    }
}

fn unit_environment(dir: &TempDir, port: u16, socket: &str) -> Vec<(String, String)> {
    let tmux_dir = on_path("tmux").parent().unwrap().display().to_string();
    let coreutils_dir = on_path("cat").parent().unwrap().display().to_string();
    let home = std::env::var("HOME").unwrap();
    let user = std::env::var("USER").unwrap_or_default();
    vec![
        ("PATH".into(), format!("{tmux_dir}:{coreutils_dir}")),
        ("HOME".into(), home),
        ("USER".into(), user.clone()),
        ("LOGNAME".into(), user),
        ("AI_DEV_HOME".into(), dir.path().display().to_string()),
        ("AI_DEV_LISTEN".into(), format!("127.0.0.1:{port}")),
        ("AI_DEV_ORIGIN".into(), format!("http://localhost:{port}")),
        (
            "AI_DEV_AGENT_TOKEN_FILE".into(),
            dir.path().join("agent_token").display().to_string(),
        ),
        ("AI_DEV_TMUX_SOCKET".into(), socket.into()),
    ]
}

struct Ui {
    browser: Browser,
    page: Page,
    _profile: TempDir,
}

impl Ui {
    async fn launch() -> Ui {
        let profile = tempfile::tempdir().unwrap();
        let config = BrowserConfig::builder()
            .chrome_executable(on_path("chromium"))
            .new_headless_mode()
            .no_sandbox()
            .user_data_dir(profile.path())
            .window_size(1280, 900)
            .build()
            .unwrap();
        let (browser, mut handler) = Browser::launch(config).await.unwrap();
        tokio::spawn(async move { while handler.next().await.is_some() {} });
        let page = browser.new_page("about:blank").await.unwrap();
        page.execute(EnableParams::default()).await.unwrap();
        let mut authenticator = VirtualAuthenticatorOptions::new(
            AuthenticatorProtocol::Ctap2,
            AuthenticatorTransport::Internal,
        );
        authenticator.has_resident_key = Some(true);
        authenticator.has_user_verification = Some(true);
        authenticator.is_user_verified = Some(true);
        authenticator.automatic_presence_simulation = Some(true);
        page.execute(AddVirtualAuthenticatorParams::new(authenticator))
            .await
            .unwrap();
        Ui {
            browser,
            page,
            _profile: profile,
        }
    }

    async fn enroll(&self, server: &Server) {
        self.open(&server.url("/enroll")).await;
        self.type_into("#code", &server.enroll_code()).await;
        self.click("#go").await;
        self.wait_until(
            "enrolled and signed in",
            "location.pathname === '/' && !!document.querySelector('form[action=\"/notebooks\"]')",
        )
        .await;
    }

    async fn open(&self, url: &str) {
        self.page.goto(url).await.unwrap();
        self.page.wait_for_navigation().await.unwrap();
    }

    async fn click(&self, selector: &str) {
        self.wait_until(
            &format!("{selector} is there"),
            &format!("!!document.querySelector({selector:?})"),
        )
        .await;
        self.page
            .find_element(selector)
            .await
            .unwrap()
            .click()
            .await
            .unwrap();
    }

    async fn type_into(&self, selector: &str, text: &str) {
        self.click(selector).await;
        self.page
            .find_element(selector)
            .await
            .unwrap()
            .type_str(text)
            .await
            .unwrap();
    }

    async fn submit(&self, form: &str, fields: &[(&str, &str)]) {
        for (name, value) in fields {
            self.type_into(&format!("{form} [name={name}]"), value)
                .await;
        }
        self.click(&format!("{form} button")).await;
    }

    async fn type_at_end(&self, selector: &str, text: &str) {
        self.wait_until(
            &format!("{selector} is there"),
            &format!("!!document.querySelector({selector:?})"),
        )
        .await;
        let _: bool = self
            .eval(&format!(
                "(() => {{ const field = document.querySelector({selector:?}); field.focus(); \
                 field.setSelectionRange(field.value.length, field.value.length); return true; }})()"
            ))
            .await;
        self.page
            .find_element(selector)
            .await
            .unwrap()
            .type_str(text)
            .await
            .unwrap();
    }

    async fn value(&self, selector: &str) -> String {
        self.eval(&format!(
            "document.querySelector({selector:?})?.value ?? ''"
        ))
        .await
    }

    async fn post_from_the_page(&self, path: &str, form: &str) {
        let status: u16 = self
            .eval(&format!(
                "fetch({path:?}, {{ method: 'POST', headers: {{ 'content-type': 'application/x-www-form-urlencoded' }}, \
                 body: {form:?}, redirect: 'manual' }}).then(r => r.status)"
            ))
            .await;
        assert!(status < 400, "POST {path}: {status}");
    }

    async fn eval<T: serde::de::DeserializeOwned>(&self, expression: &str) -> T {
        self.page
            .evaluate(expression)
            .await
            .unwrap()
            .into_value()
            .unwrap()
    }

    async fn text(&self, selector: &str) -> String {
        self.eval(&format!(
            "document.querySelector({selector:?})?.innerText ?? ''"
        ))
        .await
    }

    async fn wait_until(&self, what: &str, condition: &str) {
        let deadline = Instant::now() + WAIT;
        loop {
            let met = match self.page.evaluate(format!("!!({condition})")).await {
                Ok(result) => result.into_value::<bool>().unwrap_or(false),
                Err(_) => false,
            };
            if met {
                return;
            }
            if Instant::now() > deadline {
                let body: String = self.eval("document.body?.innerText ?? ''").await;
                panic!(
                    "waited {WAIT:?} for: {what}\npage {:?}:\n{body}",
                    self.page.url().await
                );
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }
}

impl Drop for Ui {
    fn drop(&mut self) {
        if let Some(child) = self.browser.get_mut_child() {
            let _ = child.as_mut_inner().start_kill();
        }
    }
}

async fn signed_in() -> (Server, Ui) {
    let server = Server::start();
    let ui = Ui::launch().await;
    ui.enroll(&server).await;
    (server, ui)
}

async fn open_live_notebook(ui: &Ui, server: &Server, name: &str) {
    ui.open(&server.url("/")).await;
    ui.submit("form[action=\"/notebooks\"]", &[("name", name)])
        .await;
    ui.wait_until(
        "the notebook listens for changes",
        "document.documentElement.dataset.live === 'open'",
    )
    .await;
    let _: bool = ui.eval("window.neverReloaded = true").await;
}

async fn assert_never_reloaded(ui: &Ui) {
    let same: bool = ui.eval("window.neverReloaded === true").await;
    assert!(same, "the page reloaded");
}

#[derive(Clone)]
struct Agent {
    client: reqwest::Client,
    url: String,
}

impl Agent {
    fn of(server: &Server) -> Agent {
        Agent {
            client: reqwest::Client::new(),
            url: server.url("/mcp"),
        }
    }

    async fn call(&self, tool: &str, arguments: Value) -> Value {
        let response = self
            .client
            .post(&self.url)
            .bearer_auth("e2e-agent-token")
            .header("accept", "application/json, text/event-stream")
            .json(&json!({
                "jsonrpc": "2.0", "id": 1, "method": "tools/call",
                "params": { "name": tool, "arguments": arguments }
            }))
            .send()
            .await
            .unwrap();
        let text = response.text().await.unwrap();
        let payload = text
            .lines()
            .find_map(|line| {
                line.strip_prefix("data: ")
                    .filter(|data| data.starts_with('{'))
            })
            .unwrap_or(&text);
        let reply: Value =
            serde_json::from_str(payload).unwrap_or_else(|_| panic!("{tool}: {text}"));
        let tool_text = reply["result"]["content"][0]["text"]
            .as_str()
            .unwrap_or_else(|| panic!("{tool}: {reply}"));
        serde_json::from_str(tool_text).unwrap()
    }

    async fn join(&self, ui: &Ui, notebook: &str, name: &str) -> String {
        let pending = self
            .call(
                "join_notebook",
                json!({ "notebook": notebook, "agent": name }),
            )
            .await;
        let link = pending["approve_link"]
            .as_str()
            .unwrap_or_else(|| panic!("{pending}"));
        let join_id = link.rsplit("#join-").next().unwrap();
        ui.click(&format!("#join-{join_id} button.ok")).await;
        ui.wait_until(
            "the join is admitted",
            &format!("!document.getElementById('join-{join_id}')"),
        )
        .await;
        let joined = self
            .call(
                "join_notebook",
                json!({ "notebook": notebook, "agent": name }),
            )
            .await;
        joined["token"]
            .as_str()
            .unwrap_or_else(|| panic!("{joined}"))
            .to_string()
    }

    async fn propose(&self, token: &str, code: &str, why: &str) -> String {
        let proposed = self
            .call(
                "propose_block",
                json!({ "token": token, "session": "sh", "code": code, "why": why }),
            )
            .await;
        proposed["block_id"]
            .as_str()
            .unwrap_or_else(|| panic!("{proposed}"))
            .to_string()
    }
}

async fn create_bash_session(ui: &Ui, server: &Server, name: &str) {
    ui.open(&server.url("/sessions")).await;
    ui.submit("form[action=\"/sessions\"]", &[("name", name)])
        .await;
    ui.click(&format!("form[action=\"/sessions/{name}/start\"] button"))
        .await;
    ui.wait_until(
        &format!("{name} shows as running"),
        &format!("document.querySelector('form[action=\"/sessions/{name}/stop\"]')"),
    )
    .await;
}

#[tokio::test]
async fn a_session_started_under_the_units_path_keeps_running_while_notebooks_open() {
    let _one = ONE_BROWSER_AT_A_TIME
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let (server, ui) = signed_in().await;

    create_bash_session(&ui, &server, "sh").await;
    ui.open(&server.url("/")).await;
    ui.submit("form[action=\"/notebooks\"]", &[("name", "nb")])
        .await;
    ui.wait_until("the notebook opened", "location.pathname === '/n/nb'")
        .await;
    tokio::time::sleep(Duration::from_secs(2)).await;
    ui.open(&server.url("/sessions")).await;

    let listed = ui.text("form[action=\"/sessions/sh/stop\"]").await;
    assert!(
        !listed.is_empty(),
        "sh is not running: {}",
        ui.text("body").await
    );
}

#[tokio::test]
async fn the_page_terminal_draws_in_a_monospace_font() {
    let _one = ONE_BROWSER_AT_A_TIME
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let (server, ui) = signed_in().await;
    create_bash_session(&ui, &server, "sh").await;

    ui.open(&server.url("/sessions/sh/terminal")).await;
    ui.wait_until(
        "the terminal connected",
        "document.getElementById('terminal').dataset.state === 'connected'",
    )
    .await;

    let widths: Value = ui
        .eval(
            "(() => { const rows = getComputedStyle(document.querySelector('.xterm-rows')); \
             const width = text => { const probe = document.createElement('span'); \
             probe.style.fontFamily = rows.fontFamily; probe.style.fontSize = rows.fontSize; \
             probe.style.whiteSpace = 'pre'; probe.textContent = text.repeat(20); document.body.append(probe); \
             const measured = probe.getBoundingClientRect().width; probe.remove(); return measured; }; \
             return { i: width('i'), w: width('W') }; })()",
        )
        .await;
    assert_eq!(
        widths["i"], widths["w"],
        "i and W differ in width: {widths}"
    );
}

async fn window_height(server: &Server) -> u64 {
    let output = tokio::process::Command::new("tmux")
        .args([
            "-L",
            &server.socket,
            "display",
            "-p",
            "-t",
            "=sh:",
            "#{window_height}",
        ])
        .output()
        .await
        .unwrap();
    assert!(
        output.status.success(),
        "tmux display: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout)
        .unwrap()
        .trim()
        .parse()
        .unwrap()
}

const ROWS_FILL_THEIR_BOX: &str = "(() => { const box = document.getElementById('screen').getBoundingClientRect(); \
    const drawn = document.querySelector('.xterm-screen').getBoundingClientRect(); \
    const cell = drawn.height / document.querySelectorAll('.xterm-rows > div').length; \
    return drawn.height > 0 && drawn.bottom <= box.bottom && drawn.right <= box.right \
    && box.bottom - drawn.bottom < cell + 4.5; })()";

#[tokio::test]
async fn a_shorter_window_refits_the_terminal_whole_and_the_session_follows() {
    let _one = ONE_BROWSER_AT_A_TIME
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let (server, ui) = signed_in().await;
    create_bash_session(&ui, &server, "sh").await;
    ui.open(&server.url("/sessions/sh/terminal")).await;
    ui.wait_until(
        "the terminal connected",
        "document.getElementById('terminal').dataset.state === 'connected'",
    )
    .await;
    ui.wait_until("the rows fill their box", ROWS_FILL_THEIR_BOX)
        .await;

    for height in (500..=532).rev() {
        ui.page
            .execute(SetDeviceMetricsOverrideParams::new(390, height, 2.0, true))
            .await
            .unwrap();
        ui.wait_until(
            &format!("the rows fill their box in a window {height} high"),
            ROWS_FILL_THEIR_BOX,
        )
        .await;
    }
    let rows: u64 = ui
        .eval("document.querySelectorAll('.xterm-rows > div').length")
        .await;
    let below_the_status_line = rows - 1;
    let deadline = Instant::now() + WAIT;
    loop {
        let height = window_height(&server).await;
        if height == below_the_status_line {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "the session stayed {height} rows, the page has {rows}"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

#[tokio::test]
async fn typing_into_the_page_terminal_runs_in_the_session_and_its_output_shows_in_the_rows() {
    let _one = ONE_BROWSER_AT_A_TIME
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let (server, ui) = signed_in().await;
    create_bash_session(&ui, &server, "sh").await;
    ui.wait_until(
        "the Terminal link",
        "!!document.querySelector('a[href=\"/sessions/sh/terminal\"][target=_blank]')",
    )
    .await;

    ui.open(&server.url("/sessions/sh/terminal")).await;
    ui.wait_until(
        "the terminal connected",
        "document.getElementById('terminal').dataset.state === 'connected'",
    )
    .await;
    ui.click(".xterm").await;
    let keyboard = ui
        .page
        .find_element(".xterm-helper-textarea")
        .await
        .unwrap();
    keyboard.type_str("echo e2e-ok").await.unwrap();
    keyboard.press_key("Enter").await.unwrap();

    ui.wait_until(
        "e2e-ok on a row of its own",
        "[...document.querySelectorAll('.xterm-rows > div')].some(row => row.textContent.trim() === 'e2e-ok')",
    )
    .await;
}

#[tokio::test]
async fn on_a_phone_the_key_row_sits_inside_the_window_below_the_terminal() {
    let _one = ONE_BROWSER_AT_A_TIME
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let (server, ui) = signed_in().await;
    create_bash_session(&ui, &server, "sh").await;
    let phone = SetDeviceMetricsOverrideParams::new(390, 844, 2.0, true);
    ui.page.execute(phone).await.unwrap();

    ui.open(&server.url("/sessions/sh/terminal")).await;
    ui.wait_until(
        "the terminal connected",
        "document.getElementById('terminal').dataset.state === 'connected'",
    )
    .await;

    let layout: Value = ui
        .eval(
            "(() => { const keys = document.getElementById('keys').getBoundingClientRect(); \
             const xterm = document.querySelector('.xterm').getBoundingClientRect(); \
             return { window: innerHeight, page: document.documentElement.scrollHeight, \
             keys_top: keys.top, keys_bottom: keys.bottom, xterm_bottom: xterm.bottom }; })()",
        )
        .await;
    let at = |key: &str| layout[key].as_f64().unwrap();
    assert!(at("page") <= at("window"), "the page scrolls: {layout}");
    assert!(
        at("keys_bottom") <= at("window"),
        "the key row ends below the window: {layout}"
    );
    assert!(
        at("xterm_bottom") <= at("keys_top"),
        "the terminal covers the key row: {layout}"
    );
}

#[tokio::test]
async fn an_agent_that_rejoins_is_listed_once_and_its_old_token_stops_working() {
    let _one = ONE_BROWSER_AT_A_TIME
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let (server, ui) = signed_in().await;
    let agent = Agent::of(&server);
    open_live_notebook(&ui, &server, "nb").await;

    let first = agent.join(&ui, "nb", "agent-a").await;
    let second = agent.join(&ui, "nb", "agent-a").await;

    ui.wait_until(
        "one row for agent-a",
        "document.querySelectorAll('#agents .admitted').length === 1",
    )
    .await;
    assert!(ui.text("#agents .admitted").await.contains("agent-a"));
    let refused = agent
        .call("list_variables", json!({ "token": first }))
        .await;
    assert!(
        refused["error"]
            .as_str()
            .unwrap_or_default()
            .contains("token"),
        "{refused}"
    );
    let accepted = agent
        .call("list_variables", json!({ "token": second }))
        .await;
    assert!(accepted["variables"].is_array(), "{accepted}");
    assert_never_reloaded(&ui).await;
}

#[tokio::test]
async fn an_agent_block_runs_and_is_released_from_the_page_while_other_blocks_stream_in() {
    let _one = ONE_BROWSER_AT_A_TIME
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let (server, ui) = signed_in().await;
    let agent = Agent::of(&server);
    create_bash_session(&ui, &server, "sh").await;
    open_live_notebook(&ui, &server, "nb").await;
    let token = agent.join(&ui, "nb", "agent-a").await;

    let id = agent
        .propose(&token, "echo hello-from-$((40 + 2))", "to see the greeting")
        .await;
    let block = format!("#b-{id}");
    ui.wait_until(
        "the block shows its why",
        &format!(
            "document.querySelector('{block} .why')?.innerText.includes('to see the greeting')"
        ),
    )
    .await;
    let streaming = {
        let agent = agent.clone();
        let token = token.clone();
        tokio::spawn(async move {
            for n in 0..8 {
                agent
                    .propose(&token, &format!("echo filler-{n}"), "to keep the page busy")
                    .await;
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        })
    };
    ui.click(&format!("{block} button.primary")).await;
    ui.wait_until(
        "the output shows without a reload",
        &format!("document.querySelector('{block} pre.out')?.innerText.includes('hello-from-42')"),
    )
    .await;
    ui.click(&format!("{block} button.ok")).await;
    ui.wait_until(
        "the block says the agent read it",
        &format!("document.querySelector('{block} .state')?.innerText.includes('agent read')"),
    )
    .await;
    streaming.await.unwrap();
    ui.wait_until(
        "every filler block arrived",
        "document.querySelectorAll('#blocks .block.agent').length === 9",
    )
    .await;

    let read = agent
        .call("read_block", json!({ "token": token, "block_id": id }))
        .await;
    assert!(
        read["output"]["text"]
            .as_str()
            .unwrap_or_default()
            .contains("hello-from-42"),
        "{read}"
    );
    assert_eq!(read["why"], "to see the greeting");
    assert_never_reloaded(&ui).await;
}

#[tokio::test]
async fn text_typed_into_the_page_survives_updates_from_the_server() {
    let _one = ONE_BROWSER_AT_A_TIME
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let (server, ui) = signed_in().await;
    let agent = Agent::of(&server);
    create_bash_session(&ui, &server, "sh").await;
    open_live_notebook(&ui, &server, "nb").await;
    let token = agent.join(&ui, "nb", "agent-a").await;
    let id = agent
        .propose(&token, "echo first", "to see the first line")
        .await;
    let block = format!("#b-{id}");

    ui.type_at_end(&format!("{block} textarea[name=code]"), "-edited")
        .await;
    ui.type_into(&format!("{block} input[name=note]"), "kept note")
        .await;
    ui.type_at_end("#composer textarea[name=code]", "echo from-composer")
        .await;
    ui.type_into("#variables input[name=name]", "region").await;
    ui.type_into("#variables input[name=value]", "eu-1").await;
    agent
        .propose(&token, "echo second", "to see the second line")
        .await;
    ui.post_from_the_page("/n/nb/var", "name=zone&value=z1")
        .await;
    ui.post_from_the_page("/sessions", "name=other&preset=bash")
        .await;
    ui.wait_until("the updates reached the page", "(t => t.includes('zone') && t.includes('other'))(document.querySelector('#agents').innerText)")
        .await;
    ui.wait_until(
        "the second block arrived",
        "document.querySelectorAll('#blocks .block.agent').length === 2",
    )
    .await;

    assert_eq!(
        ui.value(&format!("{block} textarea[name=code]")).await,
        "echo first-edited"
    );
    assert_eq!(
        ui.value(&format!("{block} input[name=note]")).await,
        "kept note"
    );
    assert_eq!(
        ui.value("#composer textarea[name=code]").await,
        "echo from-composer"
    );
    assert_eq!(ui.value("#variables input[name=name]").await, "region");
    assert_eq!(ui.value("#variables input[name=value]").await, "eu-1");

    ui.click("#variables .row:last-child button").await;
    ui.wait_until("both variables are listed", "(t => t.includes('region') && t.includes('zone'))(document.querySelector('#variables table').innerText)")
        .await;
    ui.click(&format!("{block} button.primary")).await;
    ui.wait_until(
        "the edited code ran",
        &format!("document.querySelector('{block} pre.out')?.innerText.includes('first-edited')"),
    )
    .await;
    ui.click("#composer button.ok").await;
    ui.wait_until("the composed block ran", "[...document.querySelectorAll('#blocks .block.me pre.out')].some(o => o.innerText.includes('from-composer'))")
        .await;
    let read = agent
        .call("read_block", json!({ "token": token, "block_id": id }))
        .await;
    assert_eq!(read["edited_by_owner_from"], "echo first", "{read}");
    assert_eq!(read["note"], "kept note", "{read}");
    assert_never_reloaded(&ui).await;
}

#[tokio::test]
async fn run_on_a_stopped_session_offers_to_start_it_and_a_finished_block_runs_again() {
    let _one = ONE_BROWSER_AT_A_TIME
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let (server, ui) = signed_in().await;
    let agent = Agent::of(&server);
    create_bash_session(&ui, &server, "sh").await;
    ui.click("form[action=\"/sessions/sh/stop\"] button").await;
    ui.wait_until(
        "sh shows as stopped",
        "document.querySelector('form[action=\"/sessions/sh/start\"]')",
    )
    .await;
    open_live_notebook(&ui, &server, "nb").await;
    let token = agent.join(&ui, "nb", "agent-a").await;
    let id = agent
        .propose(&token, "echo again-$((40 + 2))", "to run twice")
        .await;
    let block = format!("#b-{id}");
    ui.wait_until(
        "the block arrived",
        &format!("document.querySelector('{block} button.primary')"),
    )
    .await;

    ui.click(&format!("{block} button.primary")).await;
    ui.wait_until(
        "the block says the session is not running",
        &format!(
            "document.querySelector('{block} .stopped')?.innerText.includes('sh is not running')"
        ),
    )
    .await;
    assert!(
        ui.text(&format!("{block} .state"))
            .await
            .contains("proposed")
    );
    ui.click(&format!("{block} button[hx-post$=\"/start-and-run\"]"))
        .await;
    ui.wait_until(
        "the output shows",
        &format!("document.querySelector('{block} pre.out')?.innerText.includes('again-42')"),
    )
    .await;
    ui.click(&format!("{block} button.ok")).await;
    ui.wait_until(
        "the agent read it",
        &format!("document.querySelector('{block} .state')?.innerText.includes('agent read')"),
    )
    .await;

    ui.click(&format!("{block} button[hx-post$=\"/rerun\"]"))
        .await;
    ui.wait_until(
        "the new output waits for read approval",
        &format!("document.querySelector('{block} .state')?.innerText.includes('waiting for read approval') && document.querySelector('{block} pre.out')?.innerText.includes('again-42')"),
    )
    .await;
    let read = agent
        .call("read_block", json!({ "token": token, "block_id": id }))
        .await;
    assert!(!read.to_string().contains("again-42"), "{read}");
    assert_never_reloaded(&ui).await;
}

const RUN_AND_LET_READ: &str = "button[hx-post$=\"/run-and-release\"]";

#[tokio::test]
async fn run_and_let_read_ends_with_the_agent_having_read_the_output() {
    let _one = ONE_BROWSER_AT_A_TIME
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let (server, ui) = signed_in().await;
    let agent = Agent::of(&server);
    create_bash_session(&ui, &server, "sh").await;
    open_live_notebook(&ui, &server, "nb").await;
    let token = agent.join(&ui, "nb", "agent-a").await;
    let id = agent
        .propose(&token, "echo one-click-$((40 + 2))", "to read it at once")
        .await;
    let block = format!("#b-{id}");

    ui.click(&format!("{block} {RUN_AND_LET_READ}")).await;
    ui.wait_until(
        "the badge says the agent read it",
        &format!("document.querySelector('{block} .state')?.innerText === 'ran · agent read'"),
    )
    .await;

    let read = agent
        .call("read_block", json!({ "token": token, "block_id": id }))
        .await;
    assert!(
        read["output"]["text"]
            .as_str()
            .unwrap_or_default()
            .contains("one-click-42"),
        "{read}"
    );
    assert_never_reloaded(&ui).await;
}

#[tokio::test]
async fn run_and_let_read_is_absent_under_an_auto_read_policy_and_on_my_own_block() {
    let _one = ONE_BROWSER_AT_A_TIME
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let (server, ui) = signed_in().await;
    let agent = Agent::of(&server);
    create_bash_session(&ui, &server, "sh").await;
    ui.click("form[action=\"/sessions/sh/stop\"] button").await;
    ui.wait_until(
        "sh shows as stopped",
        "document.querySelector('form[action=\"/sessions/sh/start\"]')",
    )
    .await;
    open_live_notebook(&ui, &server, "nb").await;
    let token = agent.join(&ui, "nb", "agent-a").await;
    let id = agent.propose(&token, "echo agents", "to compare").await;
    let block = format!("#b-{id}");
    ui.click("#composer textarea").await;
    ui.type_into("#composer textarea", "echo mine").await;
    ui.click("#composer button.ok").await;
    ui.wait_until(
        "my block waits for the stopped session",
        "document.querySelector('#blocks .block.me .stopped')",
    )
    .await;

    let on_the_agents_block: bool = ui
        .eval(&format!(
            "!!document.querySelector('{block} {RUN_AND_LET_READ}')"
        ))
        .await;
    let on_my_block: bool = ui
        .eval(&format!(
            "!!document.querySelector('#blocks .block.me {RUN_AND_LET_READ}')"
        ))
        .await;
    assert!(on_the_agents_block);
    assert!(!on_my_block);

    ui.post_from_the_page(
        "/sessions/sh/policy",
        "run_policy=ask&read_policy=auto&idle_minutes=30",
    )
    .await;
    ui.open(&server.url("/n/nb")).await;
    ui.wait_until(
        "the agent's block is there again",
        &format!("document.querySelector('{block} button.primary')"),
    )
    .await;
    let under_auto_read: bool = ui
        .eval(&format!(
            "!!document.querySelector('{block} {RUN_AND_LET_READ}')"
        ))
        .await;
    assert!(!under_auto_read);
}

#[tokio::test]
async fn a_running_block_takes_hidden_input_from_the_page_and_can_be_interrupted() {
    let _one = ONE_BROWSER_AT_A_TIME
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let (server, ui) = signed_in().await;
    let agent = Agent::of(&server);
    create_bash_session(&ui, &server, "sh").await;
    open_live_notebook(&ui, &server, "nb").await;
    let token = agent.join(&ui, "nb", "agent-a").await;
    let asks = r#"perl -MPOSIX -e '$t = POSIX::Termios->new; $t->getattr(0); $t->setlflag($t->getlflag & ~ECHO); $t->setattr(0, TCSAFLUSH); print "password: "; $p = <STDIN>; chomp $p; print "\ngot ", length($p), " characters\n"'"#;
    let first = agent.propose(&token, asks, "needs a password").await;
    let second = agent.propose(&token, asks, "needs a password again").await;
    let (first, second) = (format!("#b-{first}"), format!("#b-{second}"));
    ui.wait_until(
        "the blocks arrived",
        &format!("document.querySelector('{second} button.primary')"),
    )
    .await;

    ui.click(&format!("{first} button.primary")).await;
    ui.wait_until(
        "the prompt shows",
        &format!("document.querySelector('{first} pre.out')?.innerText.includes('password:')"),
    )
    .await;
    ui.type_at_end(
        &format!("{first} input[type=password]"),
        "correct horse battery",
    )
    .await;
    ui.click(&format!("{first} form button[type=submit]")).await;
    ui.wait_until(
        "the block finished with the value hidden",
        &format!("document.querySelector('{first} pre.out')?.innerText.includes('got 21 characters') && document.querySelector('{first} pre.out').innerText.includes('hidden input sent')"),
    )
    .await;
    assert!(!ui.text(&first).await.contains("correct horse"));

    ui.click(&format!("{second} button.primary")).await;
    ui.wait_until(
        "the second prompt shows",
        &format!("document.querySelector('{second} pre.out')?.innerText.includes('password:')"),
    )
    .await;
    ui.click(&format!("{second} button[hx-post$=\"/interrupt\"]"))
        .await;
    ui.wait_until("the interrupted block ended", &format!("document.querySelector('{second} .state')?.innerText.includes('waiting for read approval')")).await;
    assert_never_reloaded(&ui).await;
}
