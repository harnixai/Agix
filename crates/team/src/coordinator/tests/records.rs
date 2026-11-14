use super::*;
use crate::tool::TeamAgentSpec;
use base::interface::delegate::DelegateError;

/// A worker's prompt tells it it is in a team, which stage, who else is
/// running, and where the scratchpad is — the plumbing does it, not the
/// lead repeating it in every prompt.
#[tokio::test]
async fn worker_prompt_carries_the_team_context_block() {
    let stages = vec![
        TeamStage {
            name: "research".into(),
            agents: vec![
                TeamAgentSpec {
                    label: "sources".into(),
                    prompt: "0".into(),
                    agent_type: Some("explore".into()),
                },
                TeamAgentSpec {
                    label: "prior-art".into(),
                    prompt: "0".into(),
                    agent_type: None,
                },
            ],
            aggregate: None,
        },
        TeamStage {
            name: "synthesis".into(),
            agents: vec![TeamAgentSpec {
                label: "writer".into(),
                prompt: "0".into(),
                agent_type: None,
            }],
            aggregate: None,
        },
    ];

    let (_dir, ctx) = make_ctx();
    let delegator = Arc::new(FakeDelegator::new());
    let coordinator = DefaultCoordinator::with_delegator(delegator.clone());
    coordinator
        .orchestrate(make_request(ctx, stages))
        .await
        .expect("orchestrate ok");

    let prompts = delegator.prompts.lock().unwrap().clone();
    assert_eq!(prompts.len(), 3);

    let sources = prompts
        .iter()
        .find(|p| p.contains("You are: sources"))
        .expect("no prompt for the `sources` worker");
    assert!(sources.starts_with("<team-context>"));
    assert!(sources.contains("Team: test-team"));
    assert!(sources.contains("Stage 1/2: research"));
    assert!(
        sources.contains("prior-art"),
        "a worker should know who is running alongside it: {sources}"
    );
    assert!(
        sources.contains("synthesis"),
        "a worker should know its output feeds a later stage: {sources}"
    );
    assert!(
        sources.contains("SCRATCHPAD.md"),
        "a worker should be told where the shared scratchpad is: {sources}"
    );
    assert!(
        sources.ends_with("0"),
        "the worker's own task text must still be the tail of the prompt"
    );

    // The last stage has no later stages and a single member.
    let writer = prompts
        .iter()
        .find(|p| p.contains("You are: writer"))
        .expect("no prompt for the `writer` worker");
    assert!(writer.contains("Stage 2/2: synthesis"));
    assert!(writer.contains("you are the only worker in this stage"));

    let types = delegator.agent_types.lock().unwrap().clone();
    assert!(
        types.contains(&Some("explore".to_string())),
        "a member's agent_type must reach the delegator, got {types:?}"
    );
}

/// `TeamCreate` progress must be observable while the run is still going,
/// and must arrive before the final tool result.
#[tokio::test]
async fn team_progress_is_emitted_per_stage_before_the_final_result() {
    use base::event::{AgentEvent, TeamStageStatus};

    let stages = vec![
        TeamStage {
            name: "first".into(),
            agents: vec![TeamAgentSpec {
                label: "a".into(),
                prompt: "0".into(),
                agent_type: None,
            }],
            aggregate: None,
        },
        TeamStage {
            name: "second".into(),
            agents: vec![TeamAgentSpec {
                label: "b".into(),
                prompt: "0".into(),
                agent_type: None,
            }],
            aggregate: None,
        },
    ];

    let (_dir, ctx) = make_ctx();
    let delegator = Arc::new(FakeDelegator::new());
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let coordinator = DefaultCoordinator::with_delegator(delegator).with_events(tx);

    let result = coordinator
        .orchestrate(make_request(ctx, stages))
        .await
        .expect("orchestrate ok");

    let mut seen: Vec<(String, TeamStageStatus)> = Vec::new();
    while let Ok(ev) = rx.try_recv() {
        match ev {
            AgentEvent::TeamProgress {
                team,
                stage,
                stage_index,
                stage_count,
                status,
                members,
                ..
            } => {
                assert_eq!(team, "test-team");
                assert_eq!(stage_count, 2);
                assert_eq!(members.len(), 1);
                assert_eq!(stage_index, if stage == "first" { 0 } else { 1 });
                seen.push((stage, status));
            }
            other => panic!("unexpected event {other:?}"),
        }
    }

    assert_eq!(
        seen,
        vec![
            ("first".to_string(), TeamStageStatus::Started),
            ("first".to_string(), TeamStageStatus::Completed),
            ("second".to_string(), TeamStageStatus::Started),
            ("second".to_string(), TeamStageStatus::Completed),
        ],
        "each stage must report start and completion, in order"
    );
    assert!(!result.is_error);
}

#[tokio::test]
async fn the_team_records_its_stages_and_leaves_its_members_to_the_delegator() {
    let stages = vec![TeamStage {
        name: "only".into(),
        agents: vec![TeamAgentSpec {
            label: "a".into(),
            prompt: "0".into(),
            agent_type: Some("researcher".into()),
        }],
        aggregate: None,
    }];

    let (_dir, ctx) = make_ctx();
    let delegator = Arc::new(FakeDelegator::new());
    let (tx, mut rx) = tokio::sync::mpsc::channel(16);
    let coordinator = DefaultCoordinator::with_delegator(delegator)
        .with_telemetry(telemetry::TelemetryHandle::new(tx));

    let result = coordinator
        .orchestrate(make_request(ctx, stages))
        .await
        .expect("orchestrate ok");
    assert!(!result.is_error);

    let mut kinds = Vec::new();
    while let Ok(event) = rx.try_recv() {
        kinds.push(event.kind().to_string());
    }
    assert_eq!(kinds, ["team_stage_complete"]);
}

/// A failing member is named in the stage's completion event, so the
/// host can surface the problem without waiting for the whole run.
#[tokio::test]
async fn team_progress_reports_failed_members() {
    use base::event::{AgentEvent, TeamStageStatus};

    struct AlwaysFails;
    #[async_trait]
    impl Delegator for AlwaysFails {
        async fn stop_member(&self, _: &TeamSeat) {}

        fn live_members(&self) -> Vec<base::interface::delegate::LiveMember> {
            Vec::new()
        }

        async fn delegate(&self, _: DelegateRequest) -> Result<Delegated, DelegateError> {
            finished_as("", DelegateEnd::Failed("boom".into()))
        }
    }

    let stages = vec![TeamStage {
        name: "only".into(),
        agents: vec![TeamAgentSpec {
            label: "doomed".into(),
            prompt: "task".into(),
            agent_type: None,
        }],
        aggregate: None,
    }];
    let (_dir, ctx) = make_ctx();
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let coordinator = DefaultCoordinator::with_delegator(Arc::new(AlwaysFails)).with_events(tx);

    let result = coordinator
        .orchestrate(make_request(ctx, stages))
        .await
        .expect("orchestrate returns Ok with is_error set");
    assert!(result.is_error);

    let mut failed_seen = None;
    while let Ok(ev) = rx.try_recv() {
        if let AgentEvent::TeamProgress {
            status: TeamStageStatus::Completed,
            failed,
            ..
        } = ev
        {
            failed_seen = Some(failed);
        }
    }
    assert_eq!(failed_seen, Some(vec!["doomed".to_string()]));
}

/// Without a wired channel nothing is emitted and orchestration is
/// unaffected: the case for embedders that give no `with_events`.
#[tokio::test]
async fn team_progress_is_a_no_op_without_a_wired_channel() {
    let stages = vec![TeamStage {
        name: "only".into(),
        agents: vec![TeamAgentSpec {
            label: "a".into(),
            prompt: "0".into(),
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
    assert!(!result.is_error);
}

/// Cancels the lead's turn from inside the first stage's member and
/// hands back partial text, ending as `end` says.
struct CancelsTheLead {
    lead: tokio_util::sync::CancellationToken,
    end: DelegateEnd,
    spawned: std::sync::Mutex<Vec<String>>,
}

#[async_trait]
impl Delegator for CancelsTheLead {
    async fn stop_member(&self, _: &TeamSeat) {}

    fn live_members(&self) -> Vec<base::interface::delegate::LiveMember> {
        Vec::new()
    }

    async fn delegate(&self, request: DelegateRequest) -> Result<Delegated, DelegateError> {
        let task = task_text(&request.prompt);
        self.spawned
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(task.clone());
        if task == "first" {
            self.lead.cancel();
        }
        finished_as(format!("partial-{task}"), self.end.clone())
    }
}

/// Whether the member says it was cancelled or not, the lead's call
/// being cancelled ends the team there. The first stage aggregates on a
/// model that panics if asked: a cancelled stage is not judged.
#[tokio::test]
async fn an_interrupted_team_starts_no_further_stage_and_ends_cancelled() {
    for end in [DelegateEnd::Completed, DelegateEnd::Cancelled] {
        let stage = |name: &str, prompt: &str| TeamStage {
            name: name.into(),
            agents: vec![TeamAgentSpec {
                label: name.into(),
                prompt: prompt.into(),
                agent_type: None,
            }],
            aggregate: Some(AggregateMode::Best),
        };
        let (dir, ctx) = make_ctx();
        let delegator = Arc::new(CancelsTheLead {
            lead: ctx.cancel.clone(),
            end: end.clone(),
            spawned: Default::default(),
        });
        let registry = Arc::new(crate::registry::TeamRegistry::new());
        let coordinator =
            DefaultCoordinator::with_delegator(delegator.clone()).with_registry(registry.clone());
        let result = coordinator
            .orchestrate(make_request(
                ctx,
                vec![stage("one", "first"), stage("two", "second")],
            ))
            .await
            .expect("orchestrate returns a result");

        let spawned = delegator
            .spawned
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        assert_eq!(spawned, ["first"], "{end:?}: a later stage was started");
        assert!(
            result.is_error,
            "{end:?}: an interrupted team is not a success"
        );
        let team_dir = registry.list()[0].team_dir.clone();
        assert!(team_dir.starts_with(dir.path()));
        let state: serde_json::Value = serde_json::from_str(
            &std::fs::read_to_string(team_dir.join("state.json")).expect("state.json"),
        )
        .expect("state.json is json");
        assert_eq!(state["status"], "cancelled", "{end:?}: {state}");
        let log = std::fs::read_to_string(team_dir.join("events.jsonl")).unwrap();
        let last: serde_json::Value = serde_json::from_str(log.lines().last().unwrap()).unwrap();
        assert_eq!(last["kind"], "team_completed", "{end:?}: {log}");
        assert_eq!(last["status"], "cancelled", "{end:?}: {log}");
    }
}

/// `events.jsonl` tells a member that was cancelled from one that failed
/// or finished.
#[tokio::test]
async fn a_cancelled_member_is_recorded_as_cancelled() {
    let (_dir, ctx) = make_ctx();
    let delegator = Arc::new(CancelsTheLead {
        lead: ctx.cancel.clone(),
        end: DelegateEnd::Cancelled,
        spawned: Default::default(),
    });
    let stage = TeamStage {
        name: "one".into(),
        agents: vec![TeamAgentSpec {
            label: "first".into(),
            prompt: "first".into(),
            agent_type: None,
        }],
        aggregate: None,
    };
    let registry = Arc::new(crate::registry::TeamRegistry::new());
    DefaultCoordinator::with_delegator(delegator)
        .with_registry(registry.clone())
        .orchestrate(make_request(ctx, vec![stage]))
        .await
        .expect("orchestrate returns a result");

    let team_dir = registry.list()[0].team_dir.clone();
    let log = std::fs::read_to_string(team_dir.join("events.jsonl")).unwrap();
    let completed: serde_json::Value = log
        .lines()
        .map(|l| serde_json::from_str::<serde_json::Value>(l).unwrap())
        .find(|e| e["kind"] == "member_completed")
        .expect("member_completed is recorded");
    assert_eq!(completed["cancelled"], true, "{completed}");
    assert_eq!(completed["is_error"], true, "{completed}");
    let state: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(team_dir.join("state.json")).expect("state.json"),
    )
    .expect("state.json is json");
    assert_eq!(state["members"][0]["lifecycle"], "cancelled", "{state}");
    assert_eq!(
        registry.list()[0].members[0].lifecycle,
        TeammateLifecycle::Cancelled
    );
}

/// A stage's telemetry belongs to the lead's session, not to the team.
#[tokio::test]
async fn team_stage_telemetry_is_the_leads_sessions() {
    let stage = TeamStage {
        name: "only".into(),
        agents: vec![TeamAgentSpec {
            label: "a".into(),
            prompt: "0".into(),
            agent_type: None,
        }],
        aggregate: None,
    };
    let (_dir, ctx) = make_ctx();
    let lead_session = ctx.session_id.clone();
    let (tx, mut rx) = tokio::sync::mpsc::channel(16);
    DefaultCoordinator::with_delegator(Arc::new(FakeDelegator::new()))
        .with_telemetry(telemetry::TelemetryHandle::new(tx))
        .orchestrate(make_request(ctx, vec![stage]))
        .await
        .expect("orchestrate ok");

    let event = rx.try_recv().expect("team_stage_complete is recorded");
    assert_eq!(event.kind(), "team_stage_complete");
    assert_eq!(event.session_id, lead_session);
}

/// A member still waiting for the stage's permit when the lead's call is
/// cancelled is not started once the permit comes.
#[tokio::test]
async fn a_member_waiting_for_a_permit_is_not_started_after_a_cancel() {
    let (_dir, ctx) = make_ctx();
    let delegator = Arc::new(CancelsTheLead {
        lead: ctx.cancel.clone(),
        end: DelegateEnd::Cancelled,
        spawned: Default::default(),
    });
    let member = |label: &str| TeamAgentSpec {
        label: label.into(),
        prompt: label.into(),
        agent_type: None,
    };
    let stage = TeamStage {
        name: "one".into(),
        agents: vec![member("first"), member("second")],
        aggregate: None,
    };
    let request = make_request(ctx, vec![stage]);
    let result = DefaultCoordinator::with_delegator(delegator.clone())
        .with_stage_concurrency(1)
        .orchestrate(request)
        .await
        .expect("orchestrate returns a result");

    let spawned = delegator
        .spawned
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clone();
    assert_eq!(spawned, ["first"]);
    assert!(result.is_error);
}

/// Hangs until `cancel` fires, as a judge cut off mid-answer would.
struct HangsUntilCancelled;

#[async_trait]
impl Model for HangsUntilCancelled {
    fn api_type(&self) -> ApiType {
        ApiType::Anthropic
    }
    async fn stream(
        &self,
        _prompt_blocks: Vec<PromptBlock>,
        _tools: Vec<base::interface::model::ToolDef>,
        _messages: Vec<base::interface::model::ModelMessage>,
        _params: StreamParams,
        _cancel: tokio_util::sync::CancellationToken,
    ) -> Result<ModelStream, ModelError> {
        Ok(Box::new(futures::stream::pending()))
    }
}

/// Cancels the lead's call when asked to judge, then never answers.
struct CancelsTheLeadWhenJudging {
    lead: tokio_util::sync::CancellationToken,
}

#[async_trait]
impl Model for CancelsTheLeadWhenJudging {
    fn api_type(&self) -> ApiType {
        ApiType::Anthropic
    }
    async fn stream(
        &self,
        _prompt_blocks: Vec<PromptBlock>,
        _tools: Vec<base::interface::model::ToolDef>,
        _messages: Vec<base::interface::model::ModelMessage>,
        _params: StreamParams,
        _cancel: tokio_util::sync::CancellationToken,
    ) -> Result<ModelStream, ModelError> {
        self.lead.cancel();
        Ok(Box::new(futures::stream::pending()))
    }
}

/// The last stage's members all finished but its judging was cut off by
/// the lead's cancel: the team did not complete, and says so.
#[tokio::test]
async fn a_team_interrupted_while_its_last_stage_is_judged_ends_cancelled() {
    let (_dir, ctx) = make_ctx();
    let lead = ctx.cancel.clone();
    let stage = TeamStage {
        name: "only".into(),
        agents: ["a", "b"]
            .into_iter()
            .map(|label| TeamAgentSpec {
                label: label.into(),
                prompt: "0".into(),
                agent_type: None,
            })
            .collect(),
        aggregate: Some(AggregateMode::Aggregate),
    };
    let registry = Arc::new(crate::registry::TeamRegistry::new());
    let request = OrchestrateRequest {
        model: Arc::new(CancelsTheLeadWhenJudging { lead }),
        ..make_request(ctx, vec![stage])
    };
    let result = DefaultCoordinator::with_delegator(Arc::new(FakeDelegator::new()))
        .with_registry(registry.clone())
        .orchestrate(request)
        .await
        .expect("orchestrate returns a result");

    assert!(result.is_error, "an interrupted team is not a success");
    let team_dir = registry.list()[0].team_dir.clone();
    let state: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(team_dir.join("state.json")).expect("state.json"),
    )
    .expect("state.json is json");
    assert_eq!(state["status"], "cancelled", "{state}");
}

/// A judge that would never answer stops with the lead's call, even when
/// the model itself ignores the token.
#[tokio::test]
async fn aggregation_stops_with_the_leads_call() {
    let cancel = tokio_util::sync::CancellationToken::new();
    cancel.cancel();
    let sections = vec![
        ("a".to_string(), "one".to_string(), false),
        ("b".to_string(), "two".to_string(), false),
    ];
    let kept = aggregate(
        &HangsUntilCancelled,
        "m",
        AggregateMode::Best,
        "stage",
        &sections,
        None,
        &cancel,
    )
    .await;
    assert_eq!(kept, sections);
}
