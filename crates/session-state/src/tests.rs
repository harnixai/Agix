//! Unit tests of `SessionState`: durable state, engine attachment, goal recording, task leases and task events.

use super::*;

use base::context::{stop_with_notice, TaskLease};
use base::context::{TaskEvent, TaskStatus};
use base::interface::exec::ExecProviders;
use base::permission::PermissionMode;
use std::path::PathBuf;

fn todo(content: &str) -> TodoItem {
    TodoItem {
        content: content.into(),
        status: TodoStatus::Pending,
        active_form: content.into(),
    }
}

#[test]
fn plan_mode_and_todos_come_back_from_what_the_log_recorded() {
    let before =
        SessionState::for_tests("/tmp".into()).with_permission_mode(PermissionMode::AcceptEdits);
    assert_eq!(before.unrecorded_durable_state(), None);
    before.enter_plan_mode();
    before.set_todos(vec![todo("step one")]);
    let recorded = before.unrecorded_durable_state().expect("a change");
    assert_eq!(before.unrecorded_durable_state(), None);
    let logged = serde_json::to_value(&recorded).unwrap();
    assert_eq!(
        logged,
        serde_json::json!({
            "plan": { "exit_to": "acceptEdits" },
            "todos": [{ "content": "step one", "status": "pending", "active_form": "step one" }]
        })
    );

    let after =
        SessionState::for_tests("/tmp".into()).with_permission_mode(PermissionMode::AcceptEdits);
    after.restore_durable_state(serde_json::from_value(logged).unwrap());
    assert_eq!(after.permission_mode(), PermissionMode::Plan);
    assert_eq!(after.todos(), vec![todo("step one")]);
    assert_eq!(after.unrecorded_durable_state(), None);
    assert_eq!(after.exit_plan_mode(), PermissionMode::AcceptEdits);
    assert!(after.unrecorded_durable_state().is_some());
}

#[test]
fn a_durable_state_the_log_did_not_take_is_handed_out_again() {
    let state = SessionState::for_tests("/tmp".into());
    state.enter_plan_mode();
    let first = state.unrecorded_durable_state().expect("a change");
    state.durable_state_not_recorded();
    assert_eq!(state.unrecorded_durable_state(), Some(first));
    assert_eq!(state.unrecorded_durable_state(), None);
}

#[tokio::test]
async fn goal_changes_the_log_did_not_take_go_back_ahead_of_later_ones() {
    let goal = |objective: &str| {
        Some(base::context::goal::Goal {
            id: "g".into(),
            revision: 1,
            objective: objective.into(),
            phase: base::context::goal::GoalPhase::Active,
            max_rounds: 1,
            rounds_started: 0,
            rounds_since_armed: 0,
            blocked_reason: None,
        })
    };
    let state = SessionState::for_tests("/tmp".into());
    lock(&state.unrecorded_goal_changes).extend([goal("a"), goal("b")]);
    let unrecorded = state.unrecorded_goal_changes().await;
    lock(&state.unrecorded_goal_changes).push(goal("c"));
    state.goal_changes_not_recorded(unrecorded, 1);
    let objectives: Vec<_> = state
        .unrecorded_goal_changes()
        .await
        .changes
        .into_iter()
        .map(|g| g.unwrap().objective)
        .collect();
    assert_eq!(objectives, ["b", "c"]);
}

#[test]
fn the_tools_tool_search_loaded_come_back_from_the_log() {
    let before = SessionState::for_tests("/tmp".into());
    before.activate_tools(vec!["WebFetch".to_string(), "NotebookEdit".to_string()]);
    let logged =
        serde_json::to_value(before.unrecorded_durable_state().expect("a change")).unwrap();
    assert_eq!(
        logged["activated_tools"],
        serde_json::json!(["NotebookEdit", "WebFetch"])
    );

    let after = SessionState::for_tests("/tmp".into());
    after.restore_durable_state(serde_json::from_value(logged).unwrap());
    assert!(after.activated_tools().contains("WebFetch"));
    assert_eq!(after.unrecorded_durable_state(), None);
}

#[test]
fn a_session_restored_out_of_plan_mode_keeps_the_mode_it_was_built_with() {
    let after =
        SessionState::for_tests("/tmp".into()).with_permission_mode(PermissionMode::Default);
    after.restore_durable_state(serde_json::from_value(serde_json::json!({})).unwrap());
    assert_eq!(after.permission_mode(), PermissionMode::Default);
    assert!(after.todos().is_empty());
}

fn attachment(cwd: &str, base_mode: PermissionMode, generation: u64) -> EngineAttachment {
    EngineAttachment::new(
        SessionEnv {
            base_mode,
            ..SessionEnv::new(cwd.into())
        },
        ExecProviders::in_process(),
        generation,
    )
}

#[test]
fn an_attached_engine_brings_its_env_and_mode_but_plan_mode_stays() {
    let state = SessionState::new();
    state.attach(attachment("/one", PermissionMode::AcceptEdits, 1));
    assert_eq!(state.env().cwd, PathBuf::from("/one"));
    assert_eq!(state.permission_mode(), PermissionMode::AcceptEdits);

    state.attach(attachment("/two", PermissionMode::Default, 2));
    assert_eq!(state.env().cwd, PathBuf::from("/two"));
    assert_eq!(state.permission_mode(), PermissionMode::Default);

    state.set_permission_mode(PermissionMode::AcceptEdits);
    state.enter_plan_mode();
    state.attach(attachment("/three", PermissionMode::BypassPermissions, 3));
    assert_eq!(state.permission_mode(), PermissionMode::Plan);
    assert_eq!(state.exit_plan_mode(), PermissionMode::AcceptEdits);
}

#[test]
fn a_host_switch_into_plan_enters_it_from_the_current_mode_and_out_of_it_forgets_that() {
    let state = SessionState::new();
    state.attach(attachment("/one", PermissionMode::Default, 1));
    let mut events = state.subscribe_permission_mode();

    assert!(state.switch_permission_mode(PermissionMode::AcceptEdits));
    assert_eq!(
        state.host_permission_mode(),
        Some(PermissionMode::AcceptEdits)
    );
    assert!(state.switch_permission_mode(PermissionMode::Plan));
    assert!(!state.switch_permission_mode(PermissionMode::Plan));
    assert_eq!(
        state.durable_state().plan,
        Some(PlanState {
            exit_to: Some(PermissionMode::AcceptEdits)
        })
    );
    assert_eq!(state.exit_plan_mode(), PermissionMode::AcceptEdits);

    state.enter_plan_mode();
    assert!(state.switch_permission_mode(PermissionMode::DontAsk));
    assert_eq!(state.durable_state().plan, None);
    assert_eq!(state.exit_plan_mode(), PermissionMode::Default);

    let seen: Vec<(PermissionMode, ModeChangeSource)> =
        std::iter::from_fn(|| events.try_recv().ok())
            .map(|change| (change.mode, change.source))
            .collect();
    assert_eq!(
        seen,
        [
            (PermissionMode::AcceptEdits, ModeChangeSource::Host),
            (PermissionMode::Plan, ModeChangeSource::Host),
            (PermissionMode::AcceptEdits, ModeChangeSource::Model),
            (PermissionMode::Plan, ModeChangeSource::Model),
            (PermissionMode::DontAsk, ModeChangeSource::Host),
            (PermissionMode::Default, ModeChangeSource::Model),
        ]
    );
}

/// A rewind is the host's: going back into plan, or nowhere new, is told as
/// a switch would be.
#[test]
fn a_rewind_back_into_plan_is_told_as_the_hosts_change() {
    let state = SessionState::new();
    state.attach(attachment("/one", PermissionMode::AcceptEdits, 1));
    let mut events = state.subscribe_permission_mode();
    let planning = DurableState {
        plan: Some(PlanState {
            exit_to: Some(PermissionMode::AcceptEdits),
        }),
        ..Default::default()
    };
    state.restore_rewound_state(Some(planning.clone()));
    state.restore_rewound_state(Some(planning));
    assert_eq!(state.permission_mode(), PermissionMode::Plan);
    let seen: Vec<(PermissionMode, ModeChangeSource)> =
        std::iter::from_fn(|| events.try_recv().ok())
            .map(|change| (change.mode, change.source))
            .collect();
    assert_eq!(seen, [(PermissionMode::Plan, ModeChangeSource::Host)]);
}

#[test]
fn an_attached_engine_takes_over_from_the_hosts_choice() {
    let state = SessionState::new();
    state.attach(attachment("/one", PermissionMode::Default, 1));
    state.switch_permission_mode(PermissionMode::AcceptEdits);
    let changes = state.permission_mode_changes();
    state.attach(attachment("/two", PermissionMode::AcceptEdits, 2));
    assert_eq!(state.host_permission_mode(), None);
    assert_eq!(state.permission_mode_changes(), changes);
    state.attach(attachment("/three", PermissionMode::Default, 3));
    assert_eq!(state.permission_mode(), PermissionMode::Default);
    assert_ne!(state.permission_mode_changes(), changes);
}

#[test]
fn an_engine_detaches_only_itself() {
    let state = SessionState::new();
    state.attach(attachment("/one", PermissionMode::Default, 1));
    state.attach(attachment("/two", PermissionMode::Default, 2));
    state.detach(1);
    assert!(state.exec().is_some(), "the engine attached since stays");
    state.detach(2);
    assert!(state.exec().is_none());
    assert_eq!(state.env().cwd, PathBuf::from("/two"));
}

fn leased(session: &std::sync::Arc<SessionState>, id: &str) -> TaskLease {
    TaskLease::new(
        session.clone(),
        id,
        TaskType::Agent,
        tokio_util::sync::CancellationToken::new(),
        serde_json::json!({}),
    )
    .notifying(|task| format!("{} stopped", task.task_id))
}

#[test]
fn a_concluded_lease_ends_its_task_as_it_says() {
    let session = std::sync::Arc::new(SessionState::for_tests("/tmp".into()));
    leased(&session, "t1").conclude(
        RunningStatus::Failed("boom".into()),
        Some("t1 failed".into()),
    );
    let task = session.find_running_task("t1").expect("kept after it ends");
    assert_eq!(
        *task.status.lock().unwrap(),
        RunningStatus::Failed("boom".into())
    );
    assert_eq!(session.take_notices(), ["t1 failed"]);
}

#[test]
fn a_lease_dropped_unconcluded_ends_its_task_stopped_and_says_so() {
    let session = std::sync::Arc::new(SessionState::for_tests("/tmp".into()));
    drop(leased(&session, "t2"));
    let task = session.find_running_task("t2").expect("kept after it ends");
    assert!(task.has_ended());
    assert_eq!(*task.status.lock().unwrap(), RunningStatus::Cancelled);
    assert_eq!(session.take_notices(), ["t2 stopped"]);
}

#[test]
fn a_task_stopped_while_its_lease_concludes_is_noticed_once() {
    let session = std::sync::Arc::new(SessionState::for_tests("/tmp".into()));
    let lease = leased(&session, "t4");
    let task = lease.task().clone();
    let (entered, in_window) = std::sync::mpsc::channel();
    let (release, released) = std::sync::mpsc::channel::<()>();
    let stopper = {
        let session = session.clone();
        std::thread::spawn(move || {
            stop_with_notice(session.as_ref(), &task, |task| {
                entered.send(()).unwrap();
                released.recv().unwrap();
                format!("{} stopped", task.task_id)
            });
        })
    };
    in_window.recv().unwrap();
    lease.conclude(RunningStatus::Completed, Some("t4 done".into()));
    release.send(()).unwrap();
    stopper.join().unwrap();
    let task = session.find_running_task("t4").expect("kept after it ends");
    assert_eq!(session.take_notices(), ["t4 stopped"]);
    assert_eq!(*task.status.lock().unwrap(), RunningStatus::Cancelled);
}

#[test]
fn a_task_stopped_from_outside_gets_no_notice_from_its_lease() {
    let session = std::sync::Arc::new(SessionState::for_tests("/tmp".into()));
    let lease = leased(&session, "t3");
    session.finish_task("t3", RunningStatus::Cancelled);
    lease.conclude(RunningStatus::Completed, Some("t3 done".into()));
    assert!(session.take_notices().is_empty());
}

fn task_events(rx: &mut tokio::sync::broadcast::Receiver<TaskEvent>) -> Vec<serde_json::Value> {
    std::iter::from_fn(|| rx.try_recv().ok())
        .map(|e| serde_json::to_value(e).unwrap())
        .collect()
}

#[tokio::test]
async fn a_stopped_task_finishes_once_its_work_winds_down() {
    let s = SessionState::for_tests("/tmp".into());
    let mut rx = s.subscribe_tasks();
    let task = s.register_task(
        "cd-1".into(),
        TaskType::Code,
        tokio_util::sync::CancellationToken::new(),
        serde_json::Value::Null,
    );
    task.append_output("round 1\n");
    assert!(s.cancel_running_task("cd-1"));
    assert!(task.cancel.is_cancelled());
    assert_eq!(task.info().status, TaskStatus::Stopped);
    assert!(!s.cancel_running_task("cd-1"), "already stopped");
    let kinds: Vec<_> = task_events(&mut rx)
        .iter()
        .map(|e| e["kind"].clone())
        .collect();
    assert_eq!(kinds, ["task_started"], "not finished while it winds down");

    task.append_output("wrapped up\n");
    s.finish_task("cd-1", RunningStatus::Completed);
    task.append_output("too late\n");
    let events = task_events(&mut rx);
    assert_eq!(events.len(), 2, "{events:?}");
    assert_eq!(events[0]["kind"], "task_progress");
    assert_eq!(events[0]["appended"], "round 1\nwrapped up\n");
    assert_eq!(events[1]["kind"], "task_finished");
    assert_eq!(events[1]["task"]["status"], "stopped");
    let read = task.output_from(0).unwrap();
    assert_eq!(read.next_byte, events[0]["next_byte"].as_u64().unwrap());
    assert!(read.closed);
}

#[test]
fn a_task_stopped_outside_a_runtime_finishes_at_once() {
    let s = SessionState::for_tests("/tmp".into());
    let mut rx = s.subscribe_tasks();
    let task = s.register_task(
        "ag-1".into(),
        TaskType::Agent,
        tokio_util::sync::CancellationToken::new(),
        serde_json::Value::Null,
    );
    assert!(s.cancel_running_task("ag-1"));
    assert!(task.output_closed());
    let kinds: Vec<_> = task_events(&mut rx)
        .iter()
        .map(|e| e["kind"].clone())
        .collect();
    assert_eq!(kinds, ["task_started", "task_finished"]);
}
