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
    let core = Core::open(dir.path(), "http://localhost:7471", &socket).unwrap();
    Harness { dir, core, socket }
}

async fn bash_session(core: &Arc<Core>, name: &str) {
    bash_session_with_block_limit(core, name, Some(20)).await;
}

async fn bash_session_with_block_limit(
    core: &Arc<Core>,
    name: &str,
    block_timeout_secs: Option<u64>,
) {
    let mut def = SessionDef::from_preset(name, Preset::Bash, "bash --norc", &[]);
    def.block_timeout_secs = block_timeout_secs;
    core.save_session(def).unwrap();
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

    let block = h
        .core
        .add_block("nb", "sh", "echo hello\nseq 1 300", HUMAN)
        .unwrap();
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

    let block = h
        .core
        .add_block("nb", "sh", "echo semi \\;", HUMAN)
        .unwrap();
    h.core.approve_run(&block.id, None, None).unwrap();
    let done = wait_for(&h.core, &block.id, BlockState::Finished).await;

    assert!(done.output.unwrap().contains("\nsemi ;"));
}

#[tokio::test]
async fn agent_block_waits_for_the_run_click_and_output_for_the_release() {
    let h = harness();
    h.core.create_notebook("nb").unwrap();
    bash_session(&h.core, "sh").await;

    let block = h
        .core
        .add_block("nb", "sh", "echo proposed", "agent-a")
        .unwrap();
    tokio::time::sleep(Duration::from_millis(800)).await;
    let waiting = h.core.block(&block.id).unwrap();
    assert_eq!(waiting.state, BlockState::Pending);
    assert!(waiting.output.is_none());

    h.core.approve_run(&block.id, None, None).unwrap();
    let done = wait_for(&h.core, &block.id, BlockState::Finished).await;
    assert_eq!(done.read, ReadDecision::Undecided);

    h.core
        .decide_read(&block.id, ReadDecision::Released)
        .unwrap();
    assert_eq!(
        h.core.block(&block.id).unwrap().read,
        ReadDecision::Released
    );
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

    let block = h
        .core
        .add_block("nb", "sh", "echo automatic", "agent-a")
        .unwrap();
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

    let block = h
        .core
        .add_block("nb", "sh", "echo {{undefined}}", "agent-a")
        .unwrap();
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
    assert!(
        h.core
            .propose_session("nb", "agent-a", clash, "to compare")
            .is_err()
    );
}

#[tokio::test]
async fn edited_code_runs_and_keeps_the_proposal_and_note() {
    let h = harness();
    h.core.create_notebook("nb").unwrap();
    bash_session(&h.core, "sh").await;

    let block = h
        .core
        .add_block("nb", "sh", "echo original", "agent-a")
        .unwrap();
    h.core
        .approve_run(
            &block.id,
            Some("echo edited".into()),
            Some("use this".into()),
        )
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
        .set_variable(
            ai_dev::core::VariableScope::Profile("prod".into()),
            "pw",
            "s3cr3t-value",
            true,
        )
        .unwrap();
    h.core
        .set_notebook_profile("nb", Some("prod".into()))
        .unwrap();
    bash_session(&h.core, "sh").await;

    let block = h
        .core
        .add_block("nb", "sh", "echo {{pw}}", "agent-a")
        .unwrap();
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

    let block = h
        .core
        .add_block("nb", "sh", "echo {{missing}}", HUMAN)
        .unwrap();
    h.core.approve_run(&block.id, None, None).unwrap();
    let failed = wait_for(&h.core, &block.id, BlockState::Failed).await;

    assert!(failed.output.unwrap().contains("missing"));
}

#[tokio::test]
async fn notebook_file_holds_blocks_and_no_output_and_survives_a_restart() {
    let h = harness();
    h.core.create_notebook("nb").unwrap();
    bash_session(&h.core, "sh").await;
    let block = h
        .core
        .add_block("nb", "sh", "echo visible-only-in-memory", HUMAN)
        .unwrap();
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

    let first = h
        .core
        .add_block("one", "sh", "sleep 1; echo first-output", HUMAN)
        .unwrap();
    let second = h
        .core
        .add_block("two", "sh", "echo second-output", HUMAN)
        .unwrap();
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
    let ai_dev::core::JoinOutcome::Pending { .. } = h.core.request_join("nb", "agent-a").unwrap()
    else {
        panic!("expected a pending join");
    };
    let join_id = h.core.pending_joins("nb")[0].id.clone();
    assert!(h.core.pending_joins("nb").len() == 1);
    h.core.approve_join(&join_id).unwrap();
    let ai_dev::core::JoinOutcome::Token(token) = h.core.request_join("nb", "agent-a").unwrap()
    else {
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

    let block = h
        .core
        .add_block("nb", "sh", "echo a\necho b", "agent-a")
        .unwrap();
    h.core
        .approve_run(&block.id, Some("echo a\r\necho b".into()), None)
        .unwrap();
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
    let block = reopened
        .add_block("nb", "sh", "echo adopted", HUMAN)
        .unwrap();
    reopened.approve_run(&block.id, None, None).unwrap();
    let done = wait_for(&reopened, &block.id, BlockState::Finished).await;

    assert!(done.output.unwrap().contains("adopted"));
}

#[tokio::test]
async fn a_corrupt_state_file_is_reported_not_replaced_by_an_empty_one() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("state.json"), "{ truncated").unwrap();

    let error = Core::open(dir.path(), "http://localhost:7471", "aidev-test-corrupt")
        .err()
        .unwrap();

    assert!(error.contains("corrupt"));
}

#[tokio::test]
async fn failed_setup_command_stops_the_session() {
    let h = harness();
    let def = SessionDef::from_preset(
        "sh",
        Preset::Bash,
        "bash --norc",
        &["echo {{nope}}".to_string()],
    );
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

    let block = h
        .core
        .add_block("nb", "sh", "shopt login_shell", HUMAN)
        .unwrap();
    h.core.approve_run(&block.id, None, None).unwrap();
    let done = wait_for(&h.core, &block.id, BlockState::Finished).await;

    let output = done.output.unwrap();
    assert!(
        output.contains("login_shell") && output.contains("on"),
        "{output:?}"
    );
}

#[tokio::test]
async fn the_composer_keeps_the_session_i_last_ran_in_after_a_sessions_change() {
    let h = harness();
    h.core.create_notebook("nb").unwrap();
    bash_session(&h.core, "first").await;
    bash_session(&h.core, "second").await;
    h.core
        .add_block("nb", "first", "echo a", "agent-a")
        .unwrap();
    h.core.add_block("nb", "second", "echo b", HUMAN).unwrap();

    h.core.stop_session("first").await;
    let composer =
        ai_dev::notebook_view::render(&h.core, "nb", ai_dev::notebook_view::Part::Composer).await;

    assert!(
        composer.contains("<option selected>second</option>"),
        "{composer}"
    );
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

    h.core
        .approve_run(
            &block.id,
            Some("echo edited".into()),
            Some("use this".into()),
        )
        .unwrap();
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
    panic!(
        "block {id} never showed the stopped session: {:?}",
        core.block(id)
    );
}

#[tokio::test]
async fn start_and_run_starts_the_session_and_finishes_the_block() {
    let h = harness();
    h.core.create_notebook("nb").unwrap();
    stopped_bash_session(&h.core, "sh").await;
    let block = h
        .core
        .add_block("nb", "sh", "echo started-for-you", HUMAN)
        .unwrap();
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
    let block = h
        .core
        .add_block("nb", "sh", "echo {{later}}-second-time", HUMAN)
        .unwrap();
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
    h.core
        .decide_read(&block.id, ReadDecision::Released)
        .unwrap();

    h.core.rerun_block(&block.id).unwrap();
    let again = wait_for(&h.core, &block.id, BlockState::Finished).await;

    assert_eq!(again.read, ReadDecision::Undecided);
}

#[tokio::test]
async fn run_and_release_lets_the_agent_read_without_a_release_click() {
    let h = harness();
    h.core.create_notebook("nb").unwrap();
    bash_session(&h.core, "sh").await;
    let block = h
        .core
        .add_block("nb", "sh", "echo one-click", "agent-a")
        .unwrap();

    h.core
        .approve_run_and_release(&block.id, None, None)
        .unwrap();
    let done = wait_for(&h.core, &block.id, BlockState::Finished).await;

    assert!(done.output.unwrap().contains("one-click"));
    assert_eq!(done.read, ReadDecision::Released);
}

#[tokio::test]
async fn holding_the_read_while_running_leaves_the_output_waiting_for_a_release() {
    let h = harness();
    h.core.create_notebook("nb").unwrap();
    bash_session(&h.core, "sh").await;
    let block = h
        .core
        .add_block("nb", "sh", "sleep 2; echo held", "agent-a")
        .unwrap();
    h.core
        .approve_run_and_release(&block.id, None, None)
        .unwrap();
    wait_for(&h.core, &block.id, BlockState::Running).await;

    h.core.hold_read(&block.id).unwrap();
    let done = wait_for(&h.core, &block.id, BlockState::Finished).await;

    assert!(done.output.unwrap().contains("held"));
    assert_eq!(done.read, ReadDecision::Undecided);
}

#[tokio::test]
async fn holding_the_read_of_a_finished_block_is_refused() {
    let h = harness();
    h.core.create_notebook("nb").unwrap();
    bash_session(&h.core, "sh").await;
    let block = h.core.add_block("nb", "sh", "echo one", "agent-a").unwrap();
    h.core.approve_run(&block.id, None, None).unwrap();
    wait_for(&h.core, &block.id, BlockState::Finished).await;

    assert!(h.core.hold_read(&block.id).is_err());
}

#[tokio::test]
async fn a_failing_block_run_and_released_is_released() {
    let h = harness();
    h.core.create_notebook("nb").unwrap();
    bash_session(&h.core, "sh").await;
    let block = h
        .core
        .add_block("nb", "sh", "echo {{undefined}}", "agent-a")
        .unwrap();

    h.core
        .approve_run_and_release(&block.id, None, None)
        .unwrap();
    let done = wait_for(&h.core, &block.id, BlockState::Failed).await;

    assert_eq!(done.read, ReadDecision::Released);
}

#[tokio::test]
async fn run_and_release_on_a_block_i_wrote_is_refused() {
    let h = harness();
    h.core.create_notebook("nb").unwrap();
    bash_session(&h.core, "sh").await;
    let block = h.core.add_block("nb", "sh", "echo mine", HUMAN).unwrap();

    let refused = h.core.approve_run_and_release(&block.id, None, None);

    assert!(refused.is_err());
    assert_eq!(h.core.block(&block.id).unwrap().state, BlockState::Pending);
}

#[tokio::test]
async fn start_and_run_keeps_the_release_of_the_first_click() {
    let h = harness();
    h.core.create_notebook("nb").unwrap();
    stopped_bash_session(&h.core, "sh").await;
    let block = h
        .core
        .add_block("nb", "sh", "echo after-start", "agent-a")
        .unwrap();
    h.core
        .approve_run_and_release(&block.id, None, None)
        .unwrap();
    wait_for_stopped_line(&h.core, &block.id).await;

    h.core.start_and_run(&block.id).await.unwrap();
    let done = wait_for(&h.core, &block.id, BlockState::Finished).await;

    assert!(done.output.unwrap().contains("after-start"));
    assert_eq!(done.read, ReadDecision::Released);
}

#[tokio::test]
async fn run_again_after_a_one_click_run_waits_for_read_approval() {
    let h = harness();
    h.core.create_notebook("nb").unwrap();
    bash_session(&h.core, "sh").await;
    let block = h.core.add_block("nb", "sh", "echo one", "agent-a").unwrap();
    h.core
        .approve_run_and_release(&block.id, None, None)
        .unwrap();
    wait_for(&h.core, &block.id, BlockState::Finished).await;

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

    let block = h
        .core
        .propose_block("nb", "sh", "echo nope", "agent-a", "why")
        .unwrap();
    let waiting = wait_for(&h.core, &block.id, BlockState::Pending).await;

    assert!(waiting.session_stopped);
    assert!(waiting.output.is_none());
}

#[tokio::test]
async fn a_deleted_block_that_has_run_leaves_the_page_and_the_notebook_file() {
    let h = harness();
    h.core.create_notebook("nb").unwrap();
    bash_session(&h.core, "sh").await;
    let block = h
        .core
        .add_block("nb", "sh", "echo deleteme", HUMAN)
        .unwrap();
    h.core.approve_run(&block.id, None, None).unwrap();
    wait_for(&h.core, &block.id, BlockState::Finished).await;

    h.core.delete_block(&block.id).unwrap();
    h.core.delete_block(&block.id).unwrap();

    let deleted = h.core.block(&block.id).unwrap();
    assert_eq!(deleted.state, BlockState::Deleted);
    assert!(deleted.output.is_none() && deleted.code.is_empty());
    let file = std::fs::read_to_string(h.dir.path().join("notebooks/nb.md")).unwrap();
    assert!(!file.contains("deleteme"));
    let view = ai_dev::notebook_view::render(
        &h.core,
        "nb",
        ai_dev::notebook_view::Part::Block(block.id.clone()),
    )
    .await;
    assert!(
        view.contains(" hidden></div>") && !view.contains("deleteme"),
        "{view}"
    );
}

fn notebook_file(h: &Harness) -> std::path::PathBuf {
    h.dir.path().join("notebooks/nb.md")
}

#[tokio::test]
async fn creating_a_notebook_over_an_existing_file_keeps_its_blocks() {
    let h = harness();
    let text = "# nb\n\n```sh session=sh author=human id=abc\necho from-git\n```\n";
    std::fs::create_dir_all(h.dir.path().join("notebooks")).unwrap();
    std::fs::write(notebook_file(&h), text).unwrap();

    h.core.create_notebook("nb").unwrap();

    let blocks = h.core.blocks("nb");
    assert_eq!(blocks.len(), 1);
    assert_eq!(
        (blocks[0].id.as_str(), blocks[0].code.as_str()),
        ("abc", "echo from-git")
    );
    assert_eq!(blocks[0].state, BlockState::Saved);
    assert_eq!(std::fs::read_to_string(notebook_file(&h)).unwrap(), text);
}

#[tokio::test]
async fn a_press_after_the_file_changed_loads_it_and_runs_nothing() {
    let h = harness();
    h.core.create_notebook("nb").unwrap();
    bash_session(&h.core, "sh").await;
    let block = h
        .core
        .add_block("nb", "sh", "echo before", "agent-a")
        .unwrap();
    let edited = format!(
        "# nb\n\n```sh session=sh author=agent-a id={}\necho edited-on-disk\n```\n",
        block.id
    );
    std::fs::write(notebook_file(&h), &edited).unwrap();

    let error = h.core.approve_run(&block.id, None, None).err().unwrap();

    assert_eq!(
        error,
        "nb.md changed on disk. It was loaded again and nothing ran."
    );
    assert_eq!(std::fs::read_to_string(notebook_file(&h)).unwrap(), edited);
    let loaded = h.core.block(&block.id).unwrap();
    assert_eq!(
        (loaded.state, loaded.code.as_str()),
        (BlockState::Saved, "echo edited-on-disk")
    );
}

#[tokio::test]
async fn a_block_whose_text_did_not_change_keeps_its_output_through_a_load() {
    let h = harness();
    h.core.create_notebook("nb").unwrap();
    bash_session(&h.core, "sh").await;
    let block = h
        .core
        .add_block("nb", "sh", "echo kept-output", HUMAN)
        .unwrap();
    h.core.approve_run(&block.id, None, None).unwrap();
    let finished = wait_for(&h.core, &block.id, BlockState::Finished).await;
    let written = std::fs::read_to_string(notebook_file(&h)).unwrap();
    let added = "\n```sh session=sh author=human id=added\necho new\n```\n";
    std::fs::write(notebook_file(&h), format!("{written}{added}")).unwrap();

    h.core.load_notebook_file_if_changed("nb");

    let blocks = h.core.blocks("nb");
    assert_eq!(blocks.len(), 2);
    assert_eq!(
        (blocks[0].state, &blocks[0].output),
        (BlockState::Finished, &finished.output)
    );
    assert_eq!(
        (blocks[1].id.as_str(), blocks[1].state),
        ("added", BlockState::Saved)
    );
}

#[tokio::test]
async fn a_file_with_a_repeated_id_is_refused_and_not_written() {
    let h = harness();
    h.core.create_notebook("nb").unwrap();
    bash_session(&h.core, "sh").await;
    let kept = h
        .core
        .add_block("nb", "sh", "echo kept", "agent-a")
        .unwrap();
    let repeated = "# nb\n\n```sh session=sh author=human id=abc\nls\n```\n\n```sh session=sh author=human id=abc\npwd\n```\n";
    std::fs::write(notebook_file(&h), repeated).unwrap();

    let error = h
        .core
        .add_block("nb", "sh", "echo more", HUMAN)
        .err()
        .unwrap();

    assert_eq!(error, "nb.md: block id abc is used twice");
    assert_eq!(
        std::fs::read_to_string(notebook_file(&h)).unwrap(),
        repeated
    );
    let blocks = h.core.blocks("nb");
    assert_eq!((blocks.len(), blocks[0].id.as_str()), (1, kept.id.as_str()));
}

#[tokio::test]
async fn an_agents_block_with_a_why_on_two_lines_keeps_its_output_through_a_load() {
    let h = harness();
    h.core.create_notebook("nb").unwrap();
    bash_session(&h.core, "sh").await;
    let block = h
        .core
        .propose_block("nb", "sh", "echo kept", "agent-a", "check the\nlogs")
        .unwrap();
    h.core.approve_run(&block.id, None, None).unwrap();
    wait_for(&h.core, &block.id, BlockState::Finished).await;
    let written = std::fs::read_to_string(notebook_file(&h)).unwrap();
    std::fs::write(
        notebook_file(&h),
        format!("{written}\n```sh session=sh author=human id=added\nls\n```\n"),
    )
    .unwrap();

    h.core.load_notebook_file_if_changed("nb");

    assert_eq!(h.core.block(&block.id).unwrap().state, BlockState::Finished);
}

#[tokio::test]
async fn a_notebook_over_a_copy_of_another_notebooks_file_is_not_loaded() {
    let h = harness();
    h.core.create_notebook("nb").unwrap();
    h.core
        .save_session(SessionDef::from_preset(
            "sh",
            Preset::Bash,
            "bash --norc",
            &[],
        ))
        .unwrap();
    let block = h
        .core
        .add_block("nb", "sh", "echo original", "agent-a")
        .unwrap();
    std::fs::copy(notebook_file(&h), h.dir.path().join("notebooks/copy.md")).unwrap();

    h.core.create_notebook("copy").unwrap();

    let expected = format!("copy.md: block id {} is used in notebook nb", block.id);
    assert_eq!(h.core.notebook_file_error("copy"), Some(expected));
    assert!(h.core.blocks("copy").is_empty());
}

#[tokio::test]
async fn a_missing_file_keeps_the_blocks_and_is_not_written() {
    let h = harness();
    h.core.create_notebook("nb").unwrap();
    h.core
        .save_session(SessionDef::from_preset(
            "sh",
            Preset::Bash,
            "bash --norc",
            &[],
        ))
        .unwrap();
    let block = h
        .core
        .add_block("nb", "sh", "echo kept", "agent-a")
        .unwrap();
    std::fs::remove_file(notebook_file(&h)).unwrap();

    let error = h
        .core
        .add_block("nb", "sh", "echo more", HUMAN)
        .err()
        .unwrap();

    assert_eq!(error, "nb.md is missing");
    assert!(!notebook_file(&h).exists());
    assert_eq!(h.core.blocks("nb").len(), 1);
    assert_eq!(h.core.block(&block.id).unwrap().state, BlockState::Pending);
}

#[tokio::test]
async fn a_rejected_block_and_its_note_survive_a_load() {
    let h = harness();
    h.core.create_notebook("nb").unwrap();
    h.core
        .save_session(SessionDef::from_preset(
            "sh",
            Preset::Bash,
            "bash --norc",
            &[],
        ))
        .unwrap();
    let block = h.core.add_block("nb", "sh", "echo no", "agent-a").unwrap();
    h.core
        .reject_block(&block.id, Some("use the replica".into()))
        .unwrap();
    std::fs::write(
        notebook_file(&h),
        "# nb\n\n```sh session=sh author=human id=added\nls\n```\n",
    )
    .unwrap();

    h.core.load_notebook_file_if_changed("nb");

    let rejected = h.core.block(&block.id).unwrap();
    assert_eq!(
        (rejected.state, rejected.note.as_deref()),
        (BlockState::Rejected, Some("use the replica"))
    );
}

#[tokio::test]
async fn a_block_limit_too_large_for_the_clock_means_no_limit() {
    let h = harness();
    h.core.create_notebook("nb").unwrap();
    bash_session_with_block_limit(&h.core, "sh", Some(u64::MAX)).await;

    let block = h.core.add_block("nb", "sh", "echo fine", HUMAN).unwrap();
    h.core.approve_run(&block.id, None, None).unwrap();

    wait_for(&h.core, &block.id, BlockState::Finished).await;
}

#[tokio::test]
async fn a_block_in_a_session_with_no_limit_runs_past_one_second() {
    let h = harness();
    h.core.create_notebook("nb").unwrap();
    bash_session_with_block_limit(&h.core, "sh", None).await;

    let block = h
        .core
        .add_block("nb", "sh", "sleep 2; echo late", HUMAN)
        .unwrap();
    h.core.approve_run(&block.id, None, None).unwrap();
    let done = wait_for(&h.core, &block.id, BlockState::Finished).await;

    assert!(done.output.unwrap().trim_end().ends_with("\nlate"));
}

#[tokio::test]
async fn a_block_in_a_session_with_a_one_second_limit_fails_without_its_end_marker() {
    let h = harness();
    h.core.create_notebook("nb").unwrap();
    bash_session_with_block_limit(&h.core, "sh", Some(1)).await;

    let block = h.core.add_block("nb", "sh", "sleep 3", HUMAN).unwrap();
    h.core.approve_run(&block.id, None, None).unwrap();
    let failed = wait_for(&h.core, &block.id, BlockState::Failed).await;

    let output = failed.output.unwrap();
    assert_eq!(
        output.lines().next(),
        Some("no end marker after 1s, the session may still be busy")
    );
}

#[tokio::test]
#[ignore = "runs for 130 s"]
async fn a_block_in_a_session_with_no_limit_runs_sleep_130_to_its_end() {
    let h = harness();
    h.core.create_notebook("nb").unwrap();
    bash_session_with_block_limit(&h.core, "sh", None).await;

    let block = h
        .core
        .add_block("nb", "sh", "sleep 130; echo slept", HUMAN)
        .unwrap();
    h.core.approve_run(&block.id, None, None).unwrap();
    for _ in 0..1400 {
        if h.core.block(&block.id).unwrap().state == BlockState::Finished {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }

    let done = h.core.block(&block.id).unwrap();
    assert_eq!(done.state, BlockState::Finished, "{:?}", done.output);
    assert!(done.output.unwrap().trim_end().ends_with("\nslept"));
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
