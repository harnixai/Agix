use super::*;
use crate::tool::TeamAgentSpec;
use base::interface::delegate::DelegateError;
use base::tool::ToolResultContent;
use std::sync::atomic::Ordering;

/// (a) The agents of a stage are all running at the same time: none of
/// them can finish until every one has started.
#[tokio::test]
async fn stage_agents_run_concurrently() {
    let n = 6usize;
    let agents: Vec<TeamAgentSpec> = (0..n)
        .map(|i| TeamAgentSpec {
            label: format!("agent{i}"),
            prompt: "work".into(),
            agent_type: None,
        })
        .collect();
    let stages = vec![TeamStage {
        name: "stage0".into(),
        agents,
        aggregate: None,
    }];

    let (_dir, ctx) = make_ctx();
    let delegator = Arc::new(GatheringDelegator {
        gathered: tokio::sync::Barrier::new(n),
    });
    let coordinator = DefaultCoordinator::with_delegator(delegator);
    let req = make_request(ctx, stages);

    // Run one after the other, the first agent would wait for the rest
    // forever; the timeout only turns that into a failure.
    let result = tokio::time::timeout(Duration::from_secs(120), coordinator.orchestrate(req))
        .await
        .expect("the stage's agents never all ran at once")
        .expect("orchestrate ok");
    assert!(!result.is_error, "no agent should have errored");
}

/// (b) The section order in the coordinator's output must match the
/// declaration order of `stage.agents`, even when later-declared agents
/// finish first (i.e. output order is NOT completion order).
#[tokio::test]
async fn stage_output_preserves_declaration_order_not_completion_order() {
    // Agent 0 sleeps longest, last agent sleeps shortest — so completion
    // order is the reverse of declaration order.
    let delays_ms: [u64; 4] = [150, 100, 50, 10];
    let agents: Vec<TeamAgentSpec> = delays_ms
        .iter()
        .enumerate()
        .map(|(i, d)| TeamAgentSpec {
            label: format!("agent{i}"),
            prompt: d.to_string(),
            agent_type: None,
        })
        .collect();
    let stages = vec![TeamStage {
        name: "stage0".into(),
        agents,
        aggregate: None,
    }];

    let (_dir, ctx) = make_ctx();
    let delegator = Arc::new(FakeDelegator::new());
    let coordinator = DefaultCoordinator::with_delegator(delegator);
    let req = make_request(ctx, stages);

    let result = coordinator.orchestrate(req).await.expect("orchestrate ok");
    let text = match &result.content {
        ToolResultContent::Text(t) => t.clone(),
        ToolResultContent::Blocks(_) => panic!("expected text content"),
    };

    // Assert each label's heading appears in the text in declaration
    // order (agent0 before agent1 before agent2 before agent3), which
    // is the reverse of completion order.
    let positions: Vec<usize> = (0..delays_ms.len())
        .map(|i| {
            text.find(&format!("### agent{i}"))
                .unwrap_or_else(|| panic!("missing section for agent{i} in:\n{text}"))
        })
        .collect();
    let mut sorted = positions.clone();
    sorted.sort();
    assert_eq!(
        positions, sorted,
        "expected output sections in declaration order agent0..agent3, got positions {positions:?}"
    );
}

#[tokio::test]
async fn the_result_is_the_members_output_without_orchestration_instructions() {
    let stages = vec![TeamStage {
        name: "stage0".into(),
        agents: vec![TeamAgentSpec {
            label: "only".into(),
            prompt: "10".into(),
            agent_type: None,
        }],
        aggregate: None,
    }];
    let (_dir, ctx) = make_ctx();
    let coordinator = DefaultCoordinator::with_delegator(Arc::new(FakeDelegator::new()));
    let result = coordinator
        .orchestrate(make_request(ctx, stages))
        .await
        .expect("orchestrate ok");
    let ToolResultContent::Text(text) = &result.content else {
        panic!("expected text content");
    };
    assert!(text.contains("### only"), "{text}");
    for leaked in ["## Your Role", "## Your Tools", "## Workers", "SendMessage"] {
        assert!(!text.contains(leaked), "{leaked} in:\n{text}");
    }
}

/// (c) With more agents than the semaphore has permits, the observed
/// peak in-flight concurrency must never exceed the permit count (6).
#[tokio::test]
async fn stage_dispatch_is_bounded_by_semaphore() {
    let n = 10usize;
    let delay_ms: u64 = 40;

    let agents: Vec<TeamAgentSpec> = (0..n)
        .map(|i| TeamAgentSpec {
            label: format!("agent{i}"),
            prompt: delay_ms.to_string(),
            agent_type: None,
        })
        .collect();
    let stages = vec![TeamStage {
        name: "stage0".into(),
        agents,
        aggregate: None,
    }];

    let (_dir, ctx) = make_ctx();
    let delegator = Arc::new(FakeDelegator::new());
    let coordinator = DefaultCoordinator::with_delegator(delegator.clone());
    let req = make_request(ctx, stages);

    let result = coordinator.orchestrate(req).await.expect("orchestrate ok");
    assert!(!result.is_error, "no agent should have errored");

    let peak = delegator.peak_in_flight.load(Ordering::SeqCst);
    assert!(
        peak <= 6,
        "expected peak in-flight <= 6 permits, observed {peak}"
    );
    // Sanity: with 10 agents and a limit of 6 we expect the bound to
    // actually be exercised (otherwise the test proves nothing).
    assert!(
        peak >= 2,
        "test setup produced no real concurrency (peak={peak})"
    );
}

/// A fake `Delegator` that actually *uses* `extra_tools` (unlike
/// `FakeDelegator`, which ignores them) — it calls the `ListPeers` tool it was handed and returns its result as
/// the "agent's own output", proving the tool it received is real and
/// wired to the right mailbox, not just present-but-inert.
struct MailboxAwareDelegator;

#[async_trait]
impl Delegator for MailboxAwareDelegator {
    async fn stop_member(&self, _: &TeamSeat) {}

    fn live_members(&self) -> Vec<base::interface::delegate::LiveMember> {
        Vec::new()
    }

    async fn delegate(&self, request: DelegateRequest) -> Result<Delegated, DelegateError> {
        let ctx = platform::tool_context_for_test(cwd_of(&request));
        let extra_tools = stage_tools(request);
        let list_peers = extra_tools
            .iter()
            .find(|t| t.name() == "ListPeers")
            .expect("coordinator must hand each team member a ListPeers tool");
        let result = list_peers
            .call(
                serde_json::json!({}),
                ctx,
                base::tool::ProgressSender::noop("test"),
            )
            .await
            .map_err(|e| DelegateError::Invalid(e.to_string()))?;
        match result.content {
            ToolResultContent::Text(t) => finished(t),
            ToolResultContent::Blocks(_) => finished("(blocks)"),
        }
    }
}

/// Team members must receive *working*
/// `SendMessage`/`ReadMail`/`ListPeers` tools scoped to the team's own
/// mailbox — not just have the plumbing exist unused. Each of the two
/// members here calls `ListPeers` for real and returns the result as
/// its output; both should see both labels.
#[tokio::test]
async fn orchestrate_gives_team_members_a_working_listpeers_tool() {
    let agents = vec![
        TeamAgentSpec {
            label: "alice".into(),
            prompt: "irrelevant".into(),
            agent_type: None,
        },
        TeamAgentSpec {
            label: "bob".into(),
            prompt: "irrelevant".into(),
            agent_type: None,
        },
    ];
    let stages = vec![TeamStage {
        name: "stage0".into(),
        agents,
        aggregate: None,
    }];

    let (_dir, ctx) = make_ctx();
    let delegator = Arc::new(MailboxAwareDelegator);
    let coordinator = DefaultCoordinator::with_delegator(delegator);
    let req = make_request(ctx, stages);

    let result = coordinator.orchestrate(req).await.expect("orchestrate ok");
    assert!(!result.is_error, "no agent should have errored");
    let text = match &result.content {
        ToolResultContent::Text(t) => t.clone(),
        ToolResultContent::Blocks(_) => panic!("expected text content"),
    };
    assert!(
        text.contains("alice"),
        "expected 'alice' to appear via a real ListPeers call in:\n{text}"
    );
    assert!(
        text.contains("bob"),
        "expected 'bob' to appear via a real ListPeers call in:\n{text}"
    );
}

/// Saves a team memory when its task says "remember", otherwise answers
/// with the prompt it was given.
struct MemoryAwareDelegator;

#[async_trait]
impl Delegator for MemoryAwareDelegator {
    async fn stop_member(&self, _: &TeamSeat) {}

    fn live_members(&self) -> Vec<base::interface::delegate::LiveMember> {
        Vec::new()
    }

    async fn delegate(&self, request: DelegateRequest) -> Result<Delegated, DelegateError> {
        if !request.prompt.contains("remember") {
            return finished(request.prompt);
        }
        let ctx = platform::tool_context_for_test(cwd_of(&request));
        let extra_tools = stage_tools(request);
        let save = extra_tools
            .iter()
            .find(|t| t.name() == "TeamMemorySave")
            .expect("coordinator must hand each team member TeamMemorySave");
        let input = serde_json::json!({
            "name": "api-style",
            "description": "Handlers return typed errors",
            "content": "Every handler returns a typed error, never a string."
        });
        let result = save
            .call(input, ctx, base::tool::ProgressSender::noop("test"))
            .await
            .map_err(|e| DelegateError::Invalid(e.to_string()))?;
        assert!(!result.is_error);
        finished("saved")
    }
}

#[tokio::test]
async fn a_later_stage_sees_what_an_earlier_stage_saved_to_team_memory() {
    let stage = |name: &str, label: &str, prompt: &str| TeamStage {
        name: name.into(),
        agents: vec![TeamAgentSpec {
            label: label.into(),
            prompt: prompt.into(),
            agent_type: None,
        }],
        aggregate: None,
    };
    let stages = vec![
        stage("design", "architect", "remember the API style"),
        stage("build", "builder", "implement it"),
    ];

    let (dir, ctx) = make_ctx();
    let registry = Arc::new(crate::registry::TeamRegistry::new());
    let coordinator = DefaultCoordinator::with_delegator(Arc::new(MemoryAwareDelegator))
        .with_registry(registry.clone());
    let mut req = make_request(ctx, stages);
    req.name = "mem".into();
    let result = coordinator.orchestrate(req).await.expect("orchestrate ok");
    let text = match &result.content {
        ToolResultContent::Text(t) => t.clone(),
        ToolResultContent::Blocks(_) => panic!("expected text content"),
    };
    assert!(
        text.contains("[api-style](api-style.md)"),
        "the build stage's prompt should list the saved entry:\n{text}"
    );

    let team_dir = registry.list()[0].team_dir.clone();
    assert!(team_dir.join("memory").join("api-style.md").is_file());
    assert!(team_dir.starts_with(dir.path()));
    coordinator.cleanup_team("mem").await.unwrap();
    assert!(!team_dir.join("memory").exists());
}

/// `orchestrate()` records the team in the shared registry (so
/// `TeamList` can see it), and `cleanup_team()` actually removes both the
/// registry entry and the on-disk `.agix/teams/{id}/` directory. A
/// second `cleanup_team()` call for the same
/// name must error instead of silently "succeeding" again.
#[tokio::test]
async fn orchestrate_records_team_and_cleanup_team_removes_it() {
    let agents = vec![TeamAgentSpec {
        label: "worker".into(),
        prompt: "0".into(),
        agent_type: None,
    }];
    let stages = vec![TeamStage {
        name: "stage0".into(),
        agents,
        aggregate: None,
    }];

    let (_dir, ctx) = make_ctx();
    let delegator = Arc::new(FakeDelegator::new());
    let registry = Arc::new(crate::registry::TeamRegistry::new());
    let coordinator = DefaultCoordinator::with_delegator(delegator).with_registry(registry.clone());
    let req = make_request(ctx, stages);

    coordinator.orchestrate(req).await.expect("orchestrate ok");

    let teams = registry.list();
    assert_eq!(teams.len(), 1, "expected exactly one team recorded");
    let team = teams[0].clone();
    assert_eq!(team.name, "test-team");
    assert_eq!(team.members.len(), 1);
    assert_eq!(team.members[0].label, "worker");
    assert_eq!(team.members[0].lifecycle, TeammateLifecycle::Completed);
    assert!(
        team.team_dir.exists(),
        "team_dir should exist on disk after orchestrate(): {:?}",
        team.team_dir
    );

    coordinator
        .cleanup_team("test-team")
        .await
        .expect("cleanup_team should succeed for a team that exists");
    assert!(
        registry.list().is_empty(),
        "team should be gone from the registry after cleanup_team"
    );
    assert!(
        !team.team_dir.exists(),
        "team_dir should be deleted after cleanup_team: {:?}",
        team.team_dir
    );

    let err = coordinator
        .cleanup_team("test-team")
        .await
        .expect_err("cleanup_team on an already-removed team must error, not fake-succeed");
    assert!(
        err.contains("no such team"),
        "expected a clear 'no such team' error, got: {err}"
    );
}

/// `orchestrate()` must land `team.json`/`state.json`/
/// `events.jsonl` in the shapes `crate::persist` defines, not just the
/// in-memory `TeamRegistry` entry — a future project-level `TeamStore`
/// reads these files, not the per-session registry.
#[tokio::test]
async fn orchestrate_writes_team_json_state_json_and_events_jsonl() {
    let agents = vec![TeamAgentSpec {
        label: "worker".into(),
        prompt: "0".into(),
        agent_type: None,
    }];
    let stages = vec![TeamStage {
        name: "stage0".into(),
        agents,
        aggregate: None,
    }];

    let (_dir, ctx) = make_ctx();
    let delegator = Arc::new(FakeDelegator::new());
    let registry = Arc::new(crate::registry::TeamRegistry::new());
    let coordinator = DefaultCoordinator::with_delegator(delegator)
        .with_registry(registry.clone())
        .with_scene_id("code".to_string());
    let req = make_request(ctx, stages);

    coordinator.orchestrate(req).await.expect("orchestrate ok");
    let team_dir = registry.list()[0].team_dir.clone();

    let decl = crate::persist::read_team_declaration(&team_dir)
        .unwrap()
        .expect("team.json should exist");
    assert_eq!(decl.name, "test-team");
    assert_eq!(decl.scene, "code");
    assert_eq!(decl.mode, crate::persist::TeamMode::Batch);
    assert_eq!(decl.stages.len(), 1);
    assert_eq!(decl.stages[0].members[0].label, "worker");
    assert!(
        !team_dir.join("config.json").exists(),
        "config.json should be superseded by team.json, not written alongside it"
    );

    let state = crate::persist::read_team_state(&team_dir)
        .unwrap()
        .expect("state.json should exist");
    assert_eq!(state.status, crate::persist::TeamStatus::Completed);
    assert_eq!(state.members[0].lifecycle, TeammateLifecycle::Completed);

    let events = crate::persist::read_events(&team_dir).unwrap();
    let kinds: Vec<&str> = events
        .iter()
        .map(|e| match &e.event {
            crate::persist::TeamEvent::TeamCreated { .. } => "team_created",
            crate::persist::TeamEvent::StageStarted { .. } => "stage_started",
            crate::persist::TeamEvent::MemberSpawned { .. } => "member_spawned",
            crate::persist::TeamEvent::MemberCompleted { .. } => "member_completed",
            crate::persist::TeamEvent::StageCompleted { .. } => "stage_completed",
            crate::persist::TeamEvent::TeamCompleted { .. } => "team_completed",
            crate::persist::TeamEvent::Truncated { .. } => "truncated",
        })
        .collect();
    assert_eq!(
        kinds,
        vec![
            "team_created",
            "stage_started",
            "member_spawned",
            "member_completed",
            "stage_completed",
            "team_completed",
        ]
    );
}
