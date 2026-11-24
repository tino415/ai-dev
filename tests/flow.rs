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

fn harness() -> Harness {
    let dir = tempfile::tempdir().unwrap();
    let socket = format!("aidev-test-{}", short_id());
    let core = Core::open(dir.path(), "http://localhost:7471", &socket)
        .unwrap()
        .with_block_timeout(Duration::from_secs(20));
    Harness { dir, core, socket }
}

async fn bash_session(core: &Arc<Core>, name: &str) {
    core.save_session(SessionDef::from_preset(name, Preset::Bash, "bash --norc", &[]))
        .unwrap();
    core.start_session(name).await.unwrap();
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

#[tokio::test]
async fn bash_block_returns_its_full_output_without_the_marker() {
    let h = harness();
    h.core.create_notebook("nb").unwrap();
    bash_session(&h.core, "sh").await;

    let block = h.core.add_block("nb", "sh", "echo hello\nseq 1 300", HUMAN).unwrap();
    h.core.approve_run(&block.id, None, None).unwrap();
    let done = wait_for(&h.core, &block.id, BlockState::Finished).await;

    let output = done.output.unwrap();
    assert!(output.contains("hello"));
    assert!(output.contains("\n300"));
    assert!(!output.contains("AIDEV_"));
}

#[tokio::test]
async fn a_line_ending_in_a_semicolon_reaches_the_session_as_written() {
    let h = harness();
    h.core.create_notebook("nb").unwrap();
    bash_session(&h.core, "sh").await;

    let block = h.core.add_block("nb", "sh", "echo semi \\;", HUMAN).unwrap();
    h.core.approve_run(&block.id, None, None).unwrap();
    let done = wait_for(&h.core, &block.id, BlockState::Finished).await;

    assert!(done.output.unwrap().contains("\nsemi ;"));
}

#[tokio::test]
async fn agent_block_waits_for_the_run_click_and_output_for_the_release() {
    let h = harness();
    h.core.create_notebook("nb").unwrap();
    bash_session(&h.core, "sh").await;

    let block = h.core.add_block("nb", "sh", "echo proposed", "agent-a").unwrap();
    tokio::time::sleep(Duration::from_millis(800)).await;
    let waiting = h.core.block(&block.id).unwrap();
    assert_eq!(waiting.state, BlockState::Pending);
    assert!(waiting.output.is_none());

    h.core.approve_run(&block.id, None, None).unwrap();
    let done = wait_for(&h.core, &block.id, BlockState::Finished).await;
    assert_eq!(done.read, ReadDecision::Undecided);

    h.core.decide_read(&block.id, ReadDecision::Released).unwrap();
    assert_eq!(h.core.block(&block.id).unwrap().read, ReadDecision::Released);
}

#[tokio::test]
async fn auto_policies_run_and_release_without_a_click_for_agents() {
    let h = harness();
    h.core.create_notebook("nb").unwrap();
    bash_session(&h.core, "sh").await;
    let mut def = h.core.session("sh").unwrap();
    def.run_policy = Policy::Auto;
    def.read_policy = Policy::Auto;
    h.core.save_session(def).unwrap();

    let block = h.core.add_block("nb", "sh", "echo automatic", "agent-a").unwrap();
    let done = wait_for(&h.core, &block.id, BlockState::Finished).await;

    assert!(done.output.unwrap().contains("automatic"));
    assert_eq!(done.read, ReadDecision::Released);
}

#[tokio::test]
async fn a_failed_agent_block_is_released_under_an_auto_read_policy() {
    let h = harness();
    h.core.create_notebook("nb").unwrap();
    bash_session(&h.core, "sh").await;
    let mut def = h.core.session("sh").unwrap();
    def.run_policy = Policy::Auto;
    def.read_policy = Policy::Auto;
    h.core.save_session(def).unwrap();

    let block = h.core.add_block("nb", "sh", "echo {{undefined}}", "agent-a").unwrap();
    let done = wait_for(&h.core, &block.id, BlockState::Failed).await;

    assert_eq!(done.read, ReadDecision::Released);
}

#[tokio::test]
async fn an_agent_cannot_join_as_human_or_replace_an_existing_session() {
    let h = harness();
    h.core.create_notebook("nb").unwrap();
    h.core
        .save_session(SessionDef::from_preset("prod", Preset::Bash, "", &[]))
        .unwrap();

    assert!(h.core.request_join("nb", HUMAN).is_err());
    let clash = SessionDef::from_preset("prod", Preset::Bash, "", &[]);
    assert!(h.core.propose_session("nb", "agent-a", clash, "to compare").is_err());
}

#[tokio::test]
async fn edited_code_runs_and_keeps_the_proposal_and_note() {
    let h = harness();
    h.core.create_notebook("nb").unwrap();
    bash_session(&h.core, "sh").await;

    let block = h.core.add_block("nb", "sh", "echo original", "agent-a").unwrap();
    h.core
        .approve_run(&block.id, Some("echo edited".into()), Some("use this".into()))
        .unwrap();
    let done = wait_for(&h.core, &block.id, BlockState::Finished).await;

    assert!(done.output.unwrap().contains("edited"));
    assert_eq!(done.proposed_code.as_deref(), Some("echo original"));
    assert_eq!(done.note.as_deref(), Some("use this"));
}

#[tokio::test]
async fn secret_values_are_filled_at_run_time_and_masked_everywhere() {
    let h = harness();
    h.core.create_notebook("nb").unwrap();
    h.core.create_profile("prod").unwrap();
    h.core
        .set_variable(ai_dev::core::VariableScope::Profile("prod".into()), "pw", "s3cr3t-value", true)
        .unwrap();
    h.core.set_notebook_profile("nb", Some("prod".into())).unwrap();
    bash_session(&h.core, "sh").await;

    let block = h.core.add_block("nb", "sh", "echo {{pw}}", "agent-a").unwrap();
    h.core.approve_run(&block.id, None, None).unwrap();
    let done = wait_for(&h.core, &block.id, BlockState::Finished).await;

    let output = done.output.unwrap();
    assert!(output.contains("***"));
    assert!(!output.contains("s3cr3t-value"));
    let notebook_file = std::fs::read_to_string(h.dir.path().join("notebooks/nb.md")).unwrap();
    assert!(!notebook_file.contains("s3cr3t-value"));
    assert!(notebook_file.contains("{{pw}}"));
}

#[tokio::test]
async fn unknown_variable_fails_the_block_without_typing_anything() {
    let h = harness();
    h.core.create_notebook("nb").unwrap();
    bash_session(&h.core, "sh").await;

    let block = h.core.add_block("nb", "sh", "echo {{missing}}", HUMAN).unwrap();
    h.core.approve_run(&block.id, None, None).unwrap();
    let failed = wait_for(&h.core, &block.id, BlockState::Failed).await;

    assert!(failed.output.unwrap().contains("missing"));
}

#[tokio::test]
async fn notebook_file_holds_blocks_and_no_output_and_survives_a_restart() {
    let h = harness();
    h.core.create_notebook("nb").unwrap();
    bash_session(&h.core, "sh").await;
    let block = h.core.add_block("nb", "sh", "echo visible-only-in-memory", HUMAN).unwrap();
    h.core.approve_run(&block.id, None, None).unwrap();
    wait_for(&h.core, &block.id, BlockState::Finished).await;

    let file = std::fs::read_to_string(h.dir.path().join("notebooks/nb.md")).unwrap();
    assert!(file.contains("echo visible-only-in-memory"));
    assert!(!file.contains("AIDEV_"));
    assert_eq!(file.matches("visible-only-in-memory").count(), 1);

    let reopened = Core::open(h.dir.path(), "http://localhost:7471", &h.socket).unwrap();
    let blocks = reopened.blocks("nb");
    assert_eq!(blocks.len(), 1);
    assert_eq!(blocks[0].state, BlockState::Saved);
    assert!(blocks[0].output.is_none());
}

#[tokio::test]
async fn two_notebooks_share_one_session_and_keep_their_outputs_apart() {
    let h = harness();
    h.core.create_notebook("one").unwrap();
    h.core.create_notebook("two").unwrap();
    bash_session(&h.core, "sh").await;

    let first = h.core.add_block("one", "sh", "sleep 1; echo first-output", HUMAN).unwrap();
    let second = h.core.add_block("two", "sh", "echo second-output", HUMAN).unwrap();
    h.core.approve_run(&first.id, None, None).unwrap();
    tokio::time::sleep(Duration::from_millis(300)).await;
    h.core.approve_run(&second.id, None, None).unwrap();

    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(h.core.block(&second.id).unwrap().state, BlockState::Queued);

    let first_done = wait_for(&h.core, &first.id, BlockState::Finished).await;
    let second_done = wait_for(&h.core, &second.id, BlockState::Finished).await;
    let (first_out, second_out) = (first_done.output.unwrap(), second_done.output.unwrap());
    assert!(first_out.contains("first-output") && !first_out.contains("second-output"));
    assert!(second_out.contains("second-output") && !second_out.contains("first-output"));
}

#[tokio::test]
async fn a_session_idle_past_its_timeout_is_stopped() {
    let h = harness();
    bash_session(&h.core, "sh").await;
    let mut def = h.core.session("sh").unwrap();
    def.idle_timeout_secs = 1;
    h.core.save_session(def).unwrap();
    assert!(h.core.is_running("sh").await);

    tokio::time::sleep(Duration::from_millis(1500)).await;
    h.core.reap_idle_sessions().await;

    assert!(!h.core.is_running("sh").await);
}

#[tokio::test]
async fn revoked_notebook_tokens_are_refused() {
    let h = harness();
    h.core.create_notebook("nb").unwrap();
    let ai_dev::core::JoinOutcome::Pending { .. } = h.core.request_join("nb", "agent-a").unwrap() else {
        panic!("expected a pending join");
    };
    let join_id = h.core.pending_joins("nb")[0].id.clone();
    assert!(h.core.pending_joins("nb").len() == 1);
    h.core.approve_join(&join_id).unwrap();
    let ai_dev::core::JoinOutcome::Token(token) = h.core.request_join("nb", "agent-a").unwrap() else {
        panic!("expected a token after approval");
    };

    assert_eq!(h.core.admit(&token).unwrap().agent, "agent-a");
    let token_id = h.core.notebook("nb").unwrap().tokens[0].id.clone();
    h.core.revoke_token("nb", &token_id).unwrap();

    assert!(h.core.admit(&token).is_none());
}

#[tokio::test]
async fn stopping_a_session_leaves_one_whose_name_extends_it() {
    let h = harness();
    bash_session(&h.core, "db").await;
    bash_session(&h.core, "db2").await;

    h.core.stop_session("db").await;

    assert!(!h.core.is_running("db").await);
    assert!(h.core.is_running("db2").await);
}

#[tokio::test]
async fn unedited_multiline_code_from_a_browser_form_is_not_an_edit() {
    let h = harness();
    h.core.create_notebook("nb").unwrap();
    bash_session(&h.core, "sh").await;

    let block = h.core.add_block("nb", "sh", "echo a\necho b", "agent-a").unwrap();
    h.core.approve_run(&block.id, Some("echo a\r\necho b".into()), None).unwrap();
    let done = wait_for(&h.core, &block.id, BlockState::Finished).await;

    assert!(done.proposed_code.is_none());
}

#[tokio::test]
async fn a_session_still_alive_after_a_restart_is_adopted_not_restarted() {
    let h = harness();
    h.core.create_notebook("nb").unwrap();
    bash_session(&h.core, "sh").await;
    let reopened = Core::open(h.dir.path(), "http://localhost:7471", &h.socket).unwrap();

    reopened.start_session("sh").await.unwrap();
    let block = reopened.add_block("nb", "sh", "echo adopted", HUMAN).unwrap();
    reopened.approve_run(&block.id, None, None).unwrap();
    let done = wait_for(&reopened, &block.id, BlockState::Finished).await;

    assert!(done.output.unwrap().contains("adopted"));
}

#[tokio::test]
async fn a_corrupt_state_file_is_reported_not_replaced_by_an_empty_one() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("state.json"), "{ truncated").unwrap();

    let error = Core::open(dir.path(), "http://localhost:7471", "aidev-test-corrupt").err().unwrap();

    assert!(error.contains("corrupt"));
}

#[tokio::test]
async fn failed_setup_command_stops_the_session() {
    let h = harness();
    let def = SessionDef::from_preset("sh", Preset::Bash, "bash --norc", &["echo {{nope}}".to_string()]);
    h.core.save_session(def).unwrap();

    assert!(h.core.start_session("sh").await.is_err());
    assert!(!h.core.is_running("sh").await);
}

#[tokio::test]
async fn a_bash_session_starts_in_a_login_shell() {
    let h = harness();
    h.core.create_notebook("nb").unwrap();
    h.core
        .save_session(SessionDef::from_preset("sh", Preset::Bash, "", &[]))
        .unwrap();
    h.core.start_session("sh").await.unwrap();

    let block = h.core.add_block("nb", "sh", "shopt login_shell", HUMAN).unwrap();
    h.core.approve_run(&block.id, None, None).unwrap();
    let done = wait_for(&h.core, &block.id, BlockState::Finished).await;

    let output = done.output.unwrap();
    assert!(output.contains("login_shell") && output.contains("on"), "{output:?}");
}

#[tokio::test]
async fn the_composer_keeps_the_session_i_last_ran_in_after_a_sessions_change() {
    let h = harness();
    h.core.create_notebook("nb").unwrap();
    bash_session(&h.core, "first").await;
    bash_session(&h.core, "second").await;
    h.core.add_block("nb", "first", "echo a", "agent-a").unwrap();
    h.core.add_block("nb", "second", "echo b", HUMAN).unwrap();

    h.core.stop_session("first").await;
    let composer = ai_dev::notebook_view::render(&h.core, "nb", ai_dev::notebook_view::Part::Composer).await;

    assert!(composer.contains("<option selected>second</option>"), "{composer}");
}

async fn stopped_bash_session(core: &Arc<Core>, name: &str) {
    bash_session(core, name).await;
    core.stop_session(name).await;
}

#[tokio::test]
async fn run_on_a_stopped_session_leaves_the_block_proposed() {
    let h = harness();
    h.core.create_notebook("nb").unwrap();
    stopped_bash_session(&h.core, "sh").await;
    let block = h.core.add_block("nb", "sh", "echo hi", "agent-a").unwrap();

    h.core.approve_run(&block.id, Some("echo edited".into()), Some("use this".into())).unwrap();
    let after = wait_for_stopped_line(&h.core, &block.id).await;

    assert_eq!(after.state, BlockState::Pending);
    assert_eq!(after.code, "echo edited");
    assert_eq!(after.note.as_deref(), Some("use this"));
    assert!(after.output.is_none());
}

async fn wait_for_stopped_line(core: &Core, id: &str) -> Block {
    for _ in 0..200 {
        let block = core.block(id).unwrap();
        if block.session_stopped {
            return block;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    panic!("block {id} never showed the stopped session: {:?}", core.block(id));
}

#[tokio::test]
async fn start_and_run_starts_the_session_and_finishes_the_block() {
    let h = harness();
    h.core.create_notebook("nb").unwrap();
    stopped_bash_session(&h.core, "sh").await;
    let block = h.core.add_block("nb", "sh", "echo started-for-you", HUMAN).unwrap();
    h.core.approve_run(&block.id, None, None).unwrap();
    wait_for_stopped_line(&h.core, &block.id).await;

    h.core.start_and_run(&block.id).await.unwrap();
    let done = wait_for(&h.core, &block.id, BlockState::Finished).await;

    assert!(done.output.unwrap().contains("started-for-you"));
    assert!(!done.session_stopped);
}

#[tokio::test]
async fn a_failed_block_run_again_finishes_with_new_output_in_the_same_block() {
    let h = harness();
    h.core.create_notebook("nb").unwrap();
    bash_session(&h.core, "sh").await;
    let block = h.core.add_block("nb", "sh", "echo {{later}}-second-time", HUMAN).unwrap();
    h.core.approve_run(&block.id, None, None).unwrap();
    wait_for(&h.core, &block.id, BlockState::Failed).await;
    let scope = ai_dev::core::VariableScope::Notebook("nb".into());
    h.core.set_variable(scope, "later", "now", false).unwrap();

    h.core.rerun_block(&block.id).unwrap();
    let again = wait_for(&h.core, &block.id, BlockState::Finished).await;

    assert!(again.output.unwrap().contains("now-second-time"));
    assert_eq!(h.core.blocks("nb").len(), 1);
}

#[tokio::test]
async fn run_again_on_an_agents_block_waits_for_read_approval_again() {
    let h = harness();
    h.core.create_notebook("nb").unwrap();
    bash_session(&h.core, "sh").await;
    let block = h.core.add_block("nb", "sh", "echo one", "agent-a").unwrap();
    h.core.approve_run(&block.id, None, None).unwrap();
    wait_for(&h.core, &block.id, BlockState::Finished).await;
    h.core.decide_read(&block.id, ReadDecision::Released).unwrap();

    h.core.rerun_block(&block.id).unwrap();
    let again = wait_for(&h.core, &block.id, BlockState::Finished).await;

    assert_eq!(again.read, ReadDecision::Undecided);
}

#[tokio::test]
async fn an_auto_run_proposal_on_a_stopped_session_stays_proposed() {
    let h = harness();
    h.core.create_notebook("nb").unwrap();
    let mut def = SessionDef::from_preset("sh", Preset::Bash, "bash --norc", &[]);
    def.run_policy = Policy::Auto;
    h.core.save_session(def).unwrap();

    let block = h.core.propose_block("nb", "sh", "echo nope", "agent-a", "why").unwrap();
    let waiting = wait_for(&h.core, &block.id, BlockState::Pending).await;

    assert!(waiting.session_stopped);
    assert!(waiting.output.is_none());
}

#[tokio::test]
async fn a_deleted_block_that_has_run_leaves_the_page_and_the_notebook_file() {
    let h = harness();
    h.core.create_notebook("nb").unwrap();
    bash_session(&h.core, "sh").await;
    let block = h.core.add_block("nb", "sh", "echo deleteme", HUMAN).unwrap();
    h.core.approve_run(&block.id, None, None).unwrap();
    wait_for(&h.core, &block.id, BlockState::Finished).await;

    h.core.delete_block(&block.id).unwrap();
    h.core.delete_block(&block.id).unwrap();

    let deleted = h.core.block(&block.id).unwrap();
    assert_eq!(deleted.state, BlockState::Deleted);
    assert!(deleted.output.is_none() && deleted.code.is_empty());
    let file = std::fs::read_to_string(h.dir.path().join("notebooks/nb.md")).unwrap();
    assert!(!file.contains("deleteme"));
    let view = ai_dev::notebook_view::render(&h.core, "nb", ai_dev::notebook_view::Part::Block(block.id.clone())).await;
    assert!(view.contains(" hidden></div>") && !view.contains("deleteme"), "{view}");
}

#[tokio::test]
async fn a_block_that_is_not_finished_or_failed_cannot_be_deleted() {
    let h = harness();
    h.core.create_notebook("nb").unwrap();
    bash_session(&h.core, "sh").await;
    let block = h.core.add_block("nb", "sh", "sleep 5", HUMAN).unwrap();
    assert!(h.core.delete_block(&block.id).is_err());

    h.core.approve_run(&block.id, None, None).unwrap();
    assert!(h.core.delete_block(&block.id).is_err());
    assert_ne!(h.core.block(&block.id).unwrap().state, BlockState::Deleted);
}
