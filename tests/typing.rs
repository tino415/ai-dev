use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::time::Duration;

use ai_dev::core::Core;
use ai_dev::model::*;

fn on_path(program: &str) -> PathBuf {
    std::env::var_os("PATH")
        .and_then(|path| {
            std::env::split_paths(&path)
                .map(|dir| dir.join(program))
                .find(|p| p.exists())
        })
        .unwrap_or_else(|| panic!("{program} is not on PATH; run the tests in nix develop"))
}

fn put_a_tmux_that_is_slow_to_press_enter_first_on_path(dir: &Path) {
    let real_tmux = on_path("tmux");
    let wrapper = dir.join("tmux");
    std::fs::write(
        &wrapper,
        format!(
            "#!{}\ncase \" $* \" in *\" paste-buffer \"*) sleep 0.15 ;; esac\nexec {} \"$@\"\n",
            on_path("sh").display(),
            real_tmux.display()
        ),
    )
    .unwrap();
    std::fs::set_permissions(&wrapper, std::fs::Permissions::from_mode(0o755)).unwrap();
    let path = std::env::join_paths(
        std::iter::once(dir.to_path_buf())
            .chain(std::env::split_paths(&std::env::var_os("PATH").unwrap())),
    )
    .unwrap();
    // SAFETY: this file holds one test, so no other thread reads the environment.
    unsafe { std::env::set_var("PATH", path) };
}

struct TmuxServer(String);

impl Drop for TmuxServer {
    fn drop(&mut self) {
        let _ = std::process::Command::new("tmux")
            .args(["-L", &self.0, "kill-server"])
            .output();
    }
}

#[tokio::test]
async fn an_enter_that_reaches_a_busy_session_late_adds_no_blank_line_to_the_output() {
    let bin = tempfile::tempdir().unwrap();
    put_a_tmux_that_is_slow_to_press_enter_first_on_path(bin.path());
    let dir = tempfile::tempdir().unwrap();
    let server = TmuxServer(format!("aidev-test-{}", short_id()));
    let core = Core::open(dir.path(), "http://localhost:7471", &server.0).unwrap();
    core.create_notebook("nb").unwrap();
    let mut def = SessionDef::from_preset("sh", Preset::Bash, "bash --norc", &[]);
    def.block_timeout_secs = Some(20);
    core.save_session(def).unwrap();
    core.start_session("sh").await.unwrap();

    let block = core
        .add_block(
            "nb",
            "sh",
            "sleep 0.05; echo one && sleep 0.5 && echo two",
            HUMAN,
        )
        .unwrap();
    core.approve_run(&block.id, None, None).unwrap();
    let mut output = None;
    for _ in 0..200 {
        let current = core.block(&block.id).unwrap();
        if current.state == BlockState::Finished {
            output = current.output;
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }

    let output = output.expect("the block finished");
    assert_eq!(
        output.trim_end(),
        "sleep 0.05; echo one && sleep 0.5 && echo two\none\ntwo"
    );
}
