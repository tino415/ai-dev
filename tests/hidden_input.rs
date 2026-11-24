use std::sync::Arc;
use std::time::Duration;

use ai_dev::core::Core;
use ai_dev::model::*;
use tempfile::TempDir;

struct Harness {
    dir: TempDir,
    core: Arc<Core>,
    socket: String,
}

impl Drop for Harness {
    fn drop(&mut self) {
        let _ = std::process::Command::new("tmux")
            .args(["-L", &self.socket, "kill-server"])
            .output();
    }
}

async fn harness_with_bash_session() -> Harness {
    let dir = tempfile::tempdir().unwrap();
    let socket = format!("aidev-test-{}", short_id());
    let core = Core::open(dir.path(), "http://localhost:7471", &socket).unwrap();
    core.create_notebook("nb").unwrap();
    let mut def = SessionDef::from_preset("sh", Preset::Bash, "bash --norc", &[]);
    def.block_timeout_secs = Some(20);
    core.save_session(def).unwrap();
    core.start_session("sh").await.unwrap();
    Harness { dir, core, socket }
}

async fn run_block(core: &Arc<Core>, code: &str) -> Block {
    let block = core.add_block("nb", "sh", code, HUMAN).unwrap();
    core.approve_run(&block.id, None, None).unwrap();
    block
}

async fn wait_for(core: &Core, id: &str, wanted: BlockState) -> Block {
    for _ in 0..200 {
        let block = core.block(id).unwrap();
        if block.state == wanted {
            return block;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    panic!("block {id} never reached {wanted:?}: {:?}", core.block(id));
}

async fn wait_for_output_containing(core: &Core, id: &str, text: &str) {
    for _ in 0..200 {
        if core.block(id).unwrap().output.is_some_and(|output| output.contains(text)) {
            return;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    panic!("block {id} never showed {text:?}: {:?}", core.block(id));
}

const ASKS_FOR_A_PASSWORD_AND_DISCARDS_TYPED_AHEAD_TEXT: &str = r#"perl -MPOSIX -e '$t = POSIX::Termios->new; $t->getattr(0); $t->setlflag($t->getlflag & ~ECHO); $t->setattr(0, TCSAFLUSH); print "password: "; $p = <STDIN>; chomp $p; print "\ngot ", length($p), " characters\n"'"#;
const ASKS_FOR_A_WORD_DISCARDS_TYPED_AHEAD_TEXT_AND_ECHOES_IT: &str = r#"perl -MPOSIX -e '$t = POSIX::Termios->new; $t->getattr(0); $t->setattr(0, TCSAFLUSH); print "again: "; $w = <STDIN>; chomp $w; print "you said $w\n"'"#;

#[tokio::test]
async fn hidden_input_answers_a_prompt_and_the_value_stays_out_of_the_output() {
    let h = harness_with_bash_session().await;
    let block = run_block(&h.core, ASKS_FOR_A_PASSWORD_AND_DISCARDS_TYPED_AHEAD_TEXT).await;
    wait_for_output_containing(&h.core, &block.id, "password:").await;

    h.core.send_hidden_input(&block.id, "hunter2hunter2").await.unwrap();
    let done = wait_for(&h.core, &block.id, BlockState::Finished).await;

    let output = done.output.unwrap();
    assert!(output.contains("got 14 characters"), "{output}");
    assert!(output.contains("password: ‹hidden input sent›"), "{output}");
    assert!(!output.contains("hunter2hunter2"));
    assert!(done.hidden_input_sent);
}

#[tokio::test]
async fn a_program_that_echoes_the_hidden_input_shows_it_masked() {
    let h = harness_with_bash_session().await;
    let block = run_block(&h.core, ASKS_FOR_A_WORD_DISCARDS_TYPED_AHEAD_TEXT_AND_ECHOES_IT).await;
    wait_for_output_containing(&h.core, &block.id, "again:").await;

    h.core.send_hidden_input(&block.id, "swordfish").await.unwrap();
    let done = wait_for(&h.core, &block.id, BlockState::Finished).await;

    let output = done.output.unwrap();
    assert!(output.contains("you said ***"), "{output}");
    assert!(!output.contains("swordfish"));
}

#[tokio::test]
async fn the_value_reaches_no_file_under_the_data_directory() {
    let h = harness_with_bash_session().await;
    let block = run_block(&h.core, ASKS_FOR_A_PASSWORD_AND_DISCARDS_TYPED_AHEAD_TEXT).await;
    wait_for_output_containing(&h.core, &block.id, "password:").await;
    h.core.send_hidden_input(&block.id, "tr0ub4dor-and-3").await.unwrap();
    wait_for(&h.core, &block.id, BlockState::Finished).await;

    let found = std::process::Command::new("grep")
        .args(["-rl", "tr0ub4dor-and-3"])
        .arg(h.dir.path())
        .output()
        .unwrap();
    assert_eq!(String::from_utf8_lossy(&found.stdout), "");
}

#[tokio::test]
async fn hidden_input_is_refused_for_a_block_that_is_not_running() {
    let h = harness_with_bash_session().await;
    let block = h.core.add_block("nb", "sh", "echo hi", HUMAN).unwrap();
    assert!(h.core.send_hidden_input(&block.id, "x").await.is_err());
    assert!(h.core.interrupt_block(&block.id).await.is_err());
}

#[tokio::test]
async fn interrupt_ends_a_block_waiting_at_a_prompt() {
    let h = harness_with_bash_session().await;
    let block = run_block(&h.core, ASKS_FOR_A_PASSWORD_AND_DISCARDS_TYPED_AHEAD_TEXT).await;
    wait_for_output_containing(&h.core, &block.id, "password:").await;

    h.core.interrupt_block(&block.id).await.unwrap();

    wait_for(&h.core, &block.id, BlockState::Finished).await;
}

#[tokio::test]
async fn a_running_blocks_output_so_far_is_shown_to_the_owner_only() {
    let h = harness_with_bash_session().await;
    let block = run_block(&h.core, "echo started; sleep 3").await;

    wait_for_output_containing(&h.core, &block.id, "started").await;

    let running = h.core.block(&block.id).unwrap();
    assert_eq!(running.state, BlockState::Running);
    assert_eq!(running.read, ReadDecision::Undecided);
}

#[tokio::test]
async fn block_text_and_secret_values_appear_in_no_process_argument_list() {
    let h = harness_with_bash_session().await;
    h.core
        .set_variable(ai_dev::core::VariableScope::Notebook("nb".into()), "pw", "needle-secret-value", true)
        .unwrap();
    let block = run_block(&h.core, "echo needle-block-text {{pw}}; sleep 4").await;
    wait_for_output_containing(&h.core, &block.id, "needle-block-text").await;

    let arguments = std::process::Command::new("ps").args(["-eo", "args"]).output().unwrap();
    let arguments = String::from_utf8_lossy(&arguments.stdout);
    let tmux_lines: Vec<&str> = arguments.lines().filter(|line| line.contains("tmux")).collect();
    assert!(!tmux_lines.iter().any(|line| line.contains("needle-secret-value")), "{tmux_lines:?}");
    assert!(!tmux_lines.iter().any(|line| line.contains("needle-block-text")), "{tmux_lines:?}");
}

#[tokio::test]
async fn a_value_that_spells_part_of_the_marker_text_leaves_the_marker_intact() {
    let h = harness_with_bash_session().await;
    let block = run_block(&h.core, ASKS_FOR_A_PASSWORD_AND_DISCARDS_TYPED_AHEAD_TEXT).await;
    wait_for_output_containing(&h.core, &block.id, "password:").await;

    h.core.send_hidden_input(&block.id, "input").await.unwrap();
    let done = wait_for(&h.core, &block.id, BlockState::Finished).await;

    assert!(done.output.unwrap().contains("password: ‹hidden input sent›"));
}

#[tokio::test]
async fn read_is_refused_on_a_running_block() {
    let h = harness_with_bash_session().await;
    let block = run_block(&h.core, "echo started; sleep 3").await;
    wait_for_output_containing(&h.core, &block.id, "started").await;

    assert!(h.core.decide_read(&block.id, ReadDecision::Released).is_err());
}
