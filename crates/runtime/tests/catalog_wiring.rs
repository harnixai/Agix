//! The catalog's `Builder` wiring, checked against running sessions: for each
//! point in `PROBES` a session is built through the `Builder` method the
//! catalog names, runs its warmup and one turn, and the implementation it was
//! given is shown to have been used. Every point the catalog says a `Builder`
//! method wires is probed here, so a setter that stores a value nothing reads
//! cannot claim a point unnoticed.
//!
//! What a delegate inherits is the session's `HostContracts`; the last case
//! shows the injected contracts are what the modules are handed, and the
//! agents crate shows a delegate runs on what it is handed
//! (`a_delegate_runs_on_its_parents_host_contracts`).

#![allow(clippy::unwrap_used, clippy::expect_used)]

use base::context::Session;
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use base::engine_exec::ENGINE_GIT_OPTIONS;
use base::error::ToolError;
use base::event::AgentEvent;
use base::interface::budget_policy::{BudgetPolicy, Spend, Spending};
use base::interface::catalog::{self, Wiring};
use base::interface::elicitation::{ElicitOutcome, FixedElicitation};
use base::interface::environment::Environment;
use base::interface::event_sink::CollectingSink;
use base::interface::exec::in_process::{
    InMemoryFileSystem, NoSandbox, OfflineNetwork, ScriptedProcess, ScriptedRun,
};
use base::interface::exec::{
    Confined, DirEntry, ExecError, ExecProviders, FileSystem, HttpRequest, HttpResponse, Metadata,
    Network, Origin, ProcessSpec, Sandbox, SandboxPolicy,
};
use base::interface::health::{CheckResult, HealthCheck, HealthChecks, HealthStatus};
use base::interface::host_call::{HostCall, HostCallEnd, HostCalls, HostReply, HostToolDecl};
use base::interface::instruction_provider::RuleProvider;
use base::interface::instruction_provider::{InstructionDoc, InstructionProvider};
use base::interface::memory_contracts::{
    MemoryRetriever, MemoryStorage, RetrievalHook, RetrievalRequest,
};
use base::interface::model::{
    Model, ModelContentBlock, ModelError, ModelEvent, ModelMessage, ModelStream, StreamParams,
    ToolDef,
};
use base::interface::model_interceptor::{ModelInterceptor, ModelRequestView};
use base::interface::permission::{
    Attendance, Basis, ExecProfile, Grant, GrantError, GrantScope, Obligations, PermissionFacts,
    PermissionGrants, PermissionOutcome, PermissionPolicy, PermissionRequest, PermissionSet, Reach,
    Subject,
};
use base::interface::prompt_assembler::{AssemblyRequest, PromptAssembler};
use base::interface::prompt_assembly::{AssemblyHook, Authority, PromptAssembly};
use base::interface::prompt_middleware::{PromptCall, PromptDecision, PromptMiddleware};
use base::interface::prompt_registry::{InMemoryPromptRegistry, PromptRegistry, RegisteredBlock};
use base::interface::recovery_policy::{
    CallRecovery, ModelFailure, Recovery, RecoveryAttempt, RecoveryPolicy, StopRecovery,
};
use base::interface::scene::{FixedPrompt, ScenePromptContext};
use base::interface::token_counter::TokenCounter;
use base::interface::tool::{PathStyle, ProgressSender, ToolContext, ToolResult, ToolSpec};
use base::interface::tool_middleware::{
    NextDispatch, ToolCall, ToolExec, ToolMiddleware, ToolOutcome,
};
use base::interface::tool_result::{ToolResultDraft, ToolResultTransformer};
use base::interface::turn_policy::{TurnPolicy, TurnProgress, TurnStep};
use base::memory::DurableMemory;
use base::paths::PathSettings;
use base::prompt::BlockOrigin;
use base::prompt::PromptBlock;
use base::rules::RuleEntry;
use base::tool::{Disposer, InMemoryToolRegistry, Tool};
use compaction::compact::{CompactError, CompactResult, Compactor};
use compaction::trim::{StaleTrim, StaleTrimInput};
use history::store::{HistoryStore, InMemoryHistoryStore};
use hooks::runner::PromptHookExecutor;
use hooks::{HookEvent, HookInput, HookRunner, HostHook, HostHookSource};
use memory::MemoryStore;
use runtime::agent::{Builder, HostContracts, InputMessage};
use runtime::module::{Module, ModuleError, SessionContext};
use runtime::plugin::{Plugin, PluginSession, Registrar};
use runtime::prompt_assembly::DefaultAssembler;
use settings::{LoadedSettings, Settings};
use tokio_util::sync::CancellationToken;

const MODEL_ENDPOINT: &str = "http://model.invalid/";
const RULE_PATH: &str = "/srv/rules/probe.md";

#[derive(Clone, Copy)]
enum Plan {
    Answer,
    Tool(&'static str, &'static str),
    Recall,
    FailOnce,
}

impl Plan {
    fn memory(self) -> bool {
        matches!(self, Plan::Recall)
    }
}

const TODO_INPUT: &str =
    r#"{"todos":[{"content":"look","status":"pending","activeForm":"Looking"}]}"#;
const TODO: Plan = Plan::Tool("TodoWrite", TODO_INPUT);

struct Probe {
    id: &'static str,
    builder: &'static str,
    plan: Plan,
    install: fn(Builder, &Probes) -> Builder,
    used: fn(&Probes, &Ran) -> Result<(), String>,
}

const PROBES: &[Probe] = &[
    Probe {
        id: "exec.process",
        builder: "exec",
        plan: Plan::Answer,
        install: |b, p| b.exec(p.exec.clone()),
        used: |p, _| {
            let asked = p.process.seen();
            asked
                .iter()
                .any(|argv| argv == &git("rev-parse --is-inside-work-tree"))
                .then_some(())
                .ok_or(format!("the engine's git never reached it: {asked:?}"))
        },
    },
    Probe {
        id: "exec.filesystem",
        builder: "exec",
        plan: Plan::Answer,
        install: |b, p| b.exec(p.exec.clone()),
        used: |p, ran| {
            let dot_git = ran.cwd.join(".git");
            let reads = p.seen.reads.lock().unwrap().clone();
            if !reads.contains(&dot_git) {
                return Err(format!(
                    "{} was never read through it: {reads:?}",
                    dot_git.display()
                ));
            }
            let bound = ran
                .bound
                .as_ref()
                .ok_or("the session state has no providers")?;
            Arc::ptr_eq(&bound.filesystem, &p.exec.filesystem)
                .then_some(())
                .ok_or("the session state holds another filesystem".to_string())
        },
    },
    Probe {
        id: "exec.sandbox",
        builder: "exec",
        plan: Plan::Answer,
        install: |b, p| b.exec(p.exec.clone()),
        used: |p, _| {
            let confined = p.seen.confined.lock().unwrap().clone();
            confined
                .iter()
                .any(|program| program == "git")
                .then_some(())
                .ok_or(format!(
                    "the engine's git was never confined by it: {confined:?}"
                ))
        },
    },
    Probe {
        id: "exec.network",
        builder: "exec",
        plan: Plan::Answer,
        install: |b, p| b.exec(p.exec.clone()),
        used: |p, _| {
            let requests = p.seen.requests.lock().unwrap().clone();
            requests
                .iter()
                .any(|(url, origin)| url == MODEL_ENDPOINT && *origin == Origin::Operator)
                .then_some(())
                .ok_or(format!("the warmup request never reached it: {requests:?}"))
        },
    },
    Probe {
        id: "token.count",
        builder: "token_counter",
        plan: Plan::Answer,
        install: |b, p| b.token_counter(p.counter.clone()),
        used: |p, _| {
            (p.seen.counted.load(Ordering::SeqCst) > 0)
                .then_some(())
                .ok_or("the turn never asked it to count a conversation".to_string())
        },
    },
    Probe {
        id: "rules.source",
        builder: "rule_provider",
        plan: Plan::Answer,
        install: |b, p| b.rule_provider(p.rules.clone()),
        used: |p, ran| {
            if p.seen.rules_asked.load(Ordering::SeqCst) == 0 {
                return Err("the prompt was assembled without asking it".to_string());
            }
            let shown = format!("`{RULE_PATH}`");
            ran.prompts
                .iter()
                .any(|block| block.contains(&shown))
                .then_some(())
                .ok_or(format!(
                    "the model was not shown its rule: {:?}",
                    ran.prompts
                ))
        },
    },
    Probe {
        id: "tool.registry",
        builder: "tools",
        plan: Plan::Tool("ProbeTool", "{}"),
        install: |b, p| {
            b.tools(p.tools.clone())
                .permission(PermissionSet::without_grants(Arc::new(Permits(
                    p.seen.clone(),
                ))))
        },
        used: |p, _| p.seen.was_hit("tool.registry"),
    },
    Probe {
        id: "tool.around",
        builder: "tool_middleware",
        plan: TODO,
        install: |b, p| b.tool_middleware(Arc::new(Around(p.seen.clone()))),
        used: |p, _| p.seen.was_hit("tool.around"),
    },
    Probe {
        id: "tool.result",
        builder: "tool_result_transformer",
        plan: Plan::Tool("ProbeTool", "{}"),
        install: |b, p| {
            b.tools(p.tools.clone())
                .permission(PermissionSet::without_grants(Arc::new(Permits(
                    p.seen.clone(),
                ))))
                .tool_result_transformer(Arc::new(Marks(p.seen.clone())))
        },
        used: |p, _| p.seen.was_shown(RESULT_MARK),
    },
    Probe {
        id: "prompt.block",
        builder: "prompt_registry",
        plan: Plan::Answer,
        install: |b, p| b.prompt_registry(p.blocks.clone()),
        used: |p, _| p.seen.was_shown(BLOCK_MARK),
    },
    Probe {
        id: "prompt.context",
        builder: "prompt_registry",
        plan: Plan::Answer,
        install: |b, p| b.prompt_registry(p.contexts.clone()),
        used: |p, _| p.seen.was_shown(CONTEXT_MARK),
    },
    Probe {
        id: "prompt.variable",
        builder: "prompt_registry",
        plan: Plan::Answer,
        install: |b, p| b.prompt_registry(p.variables.clone()),
        used: |p, _| p.seen.was_shown(&format!("Hello {VARIABLE_MARK}")),
    },
    Probe {
        id: "prompt.assembler",
        builder: "prompt_assembler",
        plan: Plan::Answer,
        install: |b, _| b.prompt_assembler(Arc::new(AppendsBlock)),
        used: |p, _| p.seen.was_shown(ASSEMBLER_MARK),
    },
    Probe {
        id: "prompt.assemble",
        builder: "prompt_registry",
        plan: Plan::Answer,
        install: |b, p| b.prompt_registry(p.hooks_registry.clone()),
        used: |p, _| p.seen.was_shown(ASSEMBLE_MARK),
    },
    Probe {
        id: "prompt.submit",
        builder: "prompt_middleware",
        plan: Plan::Answer,
        install: |b, p| b.prompt_middleware(Arc::new(Rewrites(p.seen.clone()))),
        used: |p, _| p.seen.was_shown(SUBMIT_MARK),
    },
    Probe {
        id: "event.sink",
        builder: "event_sink",
        plan: Plan::Answer,
        install: |b, p| b.event_sink(p.sink.clone()),
        used: |p, _| {
            p.sink
                .events()
                .iter()
                .any(|e| matches!(e, AgentEvent::TurnComplete { .. }))
                .then_some(())
                .ok_or("the sink saw no TurnComplete".to_string())
        },
    },
    Probe {
        id: "health.check",
        builder: "health_check",
        plan: Plan::Answer,
        install: |b, p| b.health_check(Arc::new(Healthy(p.seen.clone()))),
        used: |p, ran| {
            let report = ran.health.report();
            report
                .checks
                .iter()
                .any(|c| c.name == "probe")
                .then_some(())
                .ok_or(format!("the report lacks its check: {report:?}"))?;
            p.seen.was_hit("health.check")
        },
    },
    Probe {
        id: "elicitation.ask",
        builder: "elicitation",
        plan: Plan::Tool(
            "AskUserQuestion",
            r#"{"question":"which one?","options":[{"key":"a","label":"A"}]}"#,
        ),
        install: |b, p| b.tools(p.tools.clone()).elicitation(p.elicitation.clone()),
        used: |p, _| {
            let asked = p.elicitation.asked();
            asked
                .iter()
                .any(|r| r.message.contains("which one?"))
                .then_some(())
                .ok_or(format!("the question never reached it: {asked:?}"))
        },
    },
    Probe {
        id: "host_calls.call",
        builder: "host_calls",
        plan: Plan::Tool("HostProbe", "{}"),
        install: |b, p| {
            b.host_tools(vec![HostToolDecl::new(
                "HostProbe",
                "answered by the host",
                serde_json::json!({"type": "object"}),
            )])
            .host_calls(Arc::new(AnswersHostCalls(p.seen.clone())))
            .permission(PermissionSet::without_grants(Arc::new(Permits(
                p.seen.clone(),
            ))))
        },
        used: |p, _| p.seen.was_hit("host_calls.call"),
    },
    Probe {
        id: "permission.check",
        builder: "permission",
        plan: Plan::Tool("ProbeTool", "{}"),
        install: |b, p| {
            b.tools(p.tools.clone())
                .permission(PermissionSet::without_grants(Arc::new(Permits(
                    p.seen.clone(),
                ))))
        },
        used: |p, _| p.seen.was_hit("permission.check"),
    },
    Probe {
        id: "permission.grants",
        builder: "permission",
        plan: Plan::Answer,
        install: |b, p| {
            b.permission(PermissionSet {
                policy: Arc::new(Permits(p.seen.clone())),
                grants: Some(Arc::new(KeepsGrants(p.seen.clone()))),
                path_denials: None,
            })
        },
        used: |p, _| p.seen.was_hit("permission.grants"),
    },
    Probe {
        id: "scene",
        builder: "scene",
        plan: Plan::Answer,
        install: |b, _| {
            let mut spec = scene::code::spec(scene::code::Interaction::Interactive);
            spec.prompt = Arc::new(FixedPrompt(vec![PromptBlock::system(SCENE_MARK)]));
            b.scene(Arc::new(spec))
        },
        used: |p, _| p.seen.was_shown(SCENE_MARK),
    },
    Probe {
        id: "model",
        builder: "model",
        plan: Plan::Answer,
        install: |b, p| b.model(p.model.clone()),
        used: |p, _| {
            (p.seen.model_calls.load(Ordering::SeqCst) > 0)
                .then_some(())
                .ok_or("the model was never called".to_string())
        },
    },
    Probe {
        id: "model.request",
        builder: "model_interceptor",
        plan: Plan::Answer,
        install: |b, p| b.model_interceptor(Arc::new(Intercepts(p.seen.clone()))),
        used: |p, _| p.seen.was_shown(REQUEST_MARK),
    },
    Probe {
        id: "model.message",
        builder: "model_interceptor",
        plan: Plan::Answer,
        install: |b, p| b.model_interceptor(Arc::new(Intercepts(p.seen.clone()))),
        used: |p, _| p.seen.was_hit("model.message"),
    },
    Probe {
        id: "history.store",
        builder: "history_store",
        plan: Plan::Answer,
        install: |b, p| b.history_store(p.history.clone()),
        used: |p, _| {
            let sessions = futures::executor::block_on(p.history.list_sessions())
                .map_err(|e| e.to_string())?;
            (!sessions.is_empty())
                .then_some(())
                .ok_or("nothing was written to it".to_string())
        },
    },
    Probe {
        id: "memory.retriever",
        builder: "memory_retriever",
        plan: Plan::Recall,
        install: |b, p| {
            b.memory_store(p.memories.clone())
                .memory_retriever(Arc::new(Retrieves(p.seen.clone())))
        },
        used: |p, _| p.seen.was_hit("memory.retriever"),
    },
    Probe {
        id: "memory.retrieval_hook",
        builder: "retrieval_hook",
        plan: Plan::Recall,
        install: |b, p| {
            b.memory_store(p.memories.clone())
                .memory_retriever(Arc::new(Retrieves(p.seen.clone())))
                .retrieval_hook(Arc::new(Retrieves(p.seen.clone())))
        },
        used: |p, _| p.seen.was_hit("memory.retrieval_hook"),
    },
    Probe {
        id: "instruction.source",
        builder: "instruction_provider",
        plan: Plan::Answer,
        install: |b, _| b.instruction_provider(Arc::new(Instructs)),
        used: |p, _| p.seen.was_shown(INSTRUCTION_MARK),
    },
    Probe {
        id: "turn.policy",
        builder: "turn_policy",
        plan: Plan::Answer,
        install: |b, p| b.turn_policy(Arc::new(WatchesTurns(p.seen.clone()))),
        used: |p, _| p.seen.was_hit("turn.policy"),
    },
    Probe {
        id: "model.recovery",
        builder: "recovery_policy",
        plan: Plan::FailOnce,
        install: |b, p| b.recovery_policy(Arc::new(Retries(p.seen.clone()))),
        used: |p, _| p.seen.was_hit("model.recovery"),
    },
    Probe {
        id: "budget",
        builder: "budget_policy",
        plan: Plan::Answer,
        install: |b, p| b.budget_policy(Arc::new(WatchesSpend(p.seen.clone()))),
        used: |p, _| p.seen.was_hit("budget"),
    },
    Probe {
        id: "environment",
        builder: "environment",
        plan: Plan::Answer,
        install: |b, p| b.environment(Arc::new(WatchesClock(p.seen.clone()))),
        used: |p, _| p.seen.was_hit("environment"),
    },
    Probe {
        id: "compaction",
        builder: "compactor",
        plan: Plan::Answer,
        install: |b, p| b.compactor(Arc::new(WatchesCompaction(p.seen.clone()))),
        used: |p, _| p.seen.was_hit("compaction"),
    },
    Probe {
        id: "script.carrier",
        builder: "plugin",
        plan: Plan::Answer,
        install: |b, p| b.plugin(Arc::new(Registers(p.seen.clone()))),
        used: |p, _| p.seen.was_hit("script.carrier"),
    },
    Probe {
        id: "hooks",
        builder: "hooks",
        plan: Plan::Answer,
        install: |b, p| b.hooks(p.hook_runner.clone()),
        used: |p, _| p.seen.was_hit("hooks"),
    },
    Probe {
        id: "hooks.host",
        builder: "host_hooks",
        plan: Plan::Answer,
        install: |b, p| b.host_hooks(Arc::new(AsksHostHooks(p.seen.clone()))),
        used: |p, _| p.seen.was_hit("hooks.host"),
    },
];

fn git(args: &str) -> String {
    format!("git {} {args}", ENGINE_GIT_OPTIONS.join(" "))
}

#[derive(Default)]
struct Seen {
    reads: Mutex<Vec<PathBuf>>,
    requests: Mutex<Vec<(String, Origin)>>,
    confined: Mutex<Vec<String>>,
    counted: AtomicUsize,
    rules_asked: AtomicUsize,
    model_calls: AtomicUsize,
    hits: Mutex<Vec<&'static str>>,
    prompts: Mutex<Vec<String>>,
    conversation: Mutex<String>,
}

impl Seen {
    fn hit(&self, id: &'static str) {
        self.hits.lock().unwrap().push(id);
    }

    fn was_hit(&self, id: &str) -> Result<(), String> {
        self.hits
            .lock()
            .unwrap()
            .contains(&id)
            .then_some(())
            .ok_or(format!("`{id}` was never reached"))
    }

    /// The model was sent `text`, in the system prompt or the conversation.
    fn was_shown(&self, text: &str) -> Result<(), String> {
        let prompts = self.prompts.lock().unwrap();
        if prompts.iter().any(|b| b.contains(text))
            || self.conversation.lock().unwrap().contains(text)
        {
            return Ok(());
        }
        Err(format!(
            "the model was never shown `{text}` ({} system blocks)",
            prompts.len()
        ))
    }
}

/// One recorder per contract, fresh for each session.
struct Probes {
    seen: Arc<Seen>,
    process: ScriptedProcess,
    exec: ExecProviders,
    counter: Arc<dyn TokenCounter>,
    rules: Arc<dyn RuleProvider>,
    model: Arc<ScriptedModel>,
    tools: Arc<InMemoryToolRegistry>,
    blocks: Arc<InMemoryPromptRegistry>,
    contexts: Arc<InMemoryPromptRegistry>,
    variables: Arc<InMemoryPromptRegistry>,
    hooks_registry: Arc<InMemoryPromptRegistry>,
    _registrations: Vec<Disposer>,
    sink: Arc<CollectingSink>,
    elicitation: Arc<FixedElicitation>,
    history: Arc<InMemoryHistoryStore>,
    memories: Arc<MemoryStore>,
    hook_runner: Arc<HookRunner>,
    _memory_dir: tempfile::TempDir,
}

impl Probes {
    fn new(plan: Plan) -> Self {
        let seen = Arc::new(Seen::default());
        let process = ScriptedProcess::new().with(
            git("rev-parse --is-inside-work-tree"),
            ScriptedRun::ok("true\n"),
        );
        let exec = ExecProviders {
            process: Arc::new(process.clone()),
            filesystem: Arc::new(RecordingFs(seen.clone(), InMemoryFileSystem::new())),
            network: Arc::new(RecordingNetwork(seen.clone(), OfflineNetwork::new())),
            sandbox: Arc::new(RecordingSandbox(seen.clone())),
        };

        let tools = Arc::new(InMemoryToolRegistry::new());
        tools::register_builtin_tools(&tools);
        let _ = tools.register(Arc::new(ProbeTool(seen.clone())));

        let mut registrations = Vec::new();
        let blocks = InMemoryPromptRegistry::new();
        registrations.push(blocks.register_block(RegisteredBlock::system(
            "probe.block",
            600,
            BLOCK_MARK,
        )));
        let contexts = InMemoryPromptRegistry::new();
        registrations.push(contexts.register_context(
            "probe.context",
            600,
            Arc::new(|_| Some(CONTEXT_MARK.to_string())),
        ));
        let variables = InMemoryPromptRegistry::new();
        registrations.push(variables.register_block(RegisteredBlock::system(
            "probe.variable",
            600,
            "Hello {{probe_variable}}",
        )));
        registrations.push(variables.register_variable(
            "probe_variable",
            Arc::new(|_| Some(VARIABLE_MARK.to_string())),
        ));
        let hooks_registry = InMemoryPromptRegistry::new();
        registrations.push(hooks_registry.register_assembly_hook(
            Arc::new(AppendsAtAssembly),
            Authority::local(BlockOrigin::Kernel),
        ));

        let memory_dir = tempfile::tempdir().unwrap();
        let memories = Arc::new(MemoryStore::new(
            memory_dir.path().join("user"),
            memory_dir.path().join("local"),
        ));
        memories
            .persist_batch(
                (0..8)
                    .map(|i| DurableMemory {
                        name: format!("mem-{i}"),
                        description: format!("probe memory {i}"),
                        memory_type: Default::default(),
                        content: "content".into(),
                        source_session_id: String::new(),
                        confidence: 0.8,
                        last_seen: String::new(),
                        recall_count: 0,
                    })
                    .collect(),
            )
            .unwrap();

        let (hook_settings, _) = hooks::config::parse_hooks_settings(&serde_json::json!({
            "UserPromptSubmit": [{ "type": "prompt", "prompt": "judge" }]
        }));
        let hook_runner = Arc::new(
            HookRunner::new(hook_settings, hooks::HookExec::nowhere())
                .with_prompt_executor(Arc::new(JudgesPrompts(seen.clone()))),
        );

        Probes {
            counter: Arc::new(CountingCounter(seen.clone())),
            rules: Arc::new(CountingRules(seen.clone())),
            model: Arc::new(ScriptedModel {
                seen: seen.clone(),
                plan,
            }),
            tools,
            blocks,
            contexts,
            variables,
            hooks_registry,
            _registrations: registrations,
            sink: CollectingSink::new(),
            elicitation: FixedElicitation::new(ElicitOutcome::declined(
                "the probe answers nothing",
            )),
            history: Arc::new(InMemoryHistoryStore::new()),
            memories,
            hook_runner,
            _memory_dir: memory_dir,
            seen,
            process,
            exec,
        }
    }
}

struct RecordingFs(Arc<Seen>, InMemoryFileSystem);

#[async_trait]
impl FileSystem for RecordingFs {
    async fn read(&self, path: &Path) -> Result<Vec<u8>, ExecError> {
        self.0.reads.lock().unwrap().push(path.to_path_buf());
        self.1.read(path).await
    }
    async fn write(&self, path: &Path, bytes: &[u8]) -> Result<(), ExecError> {
        self.1.write(path, bytes).await
    }
    async fn create_dir_all(&self, path: &Path) -> Result<(), ExecError> {
        self.1.create_dir_all(path).await
    }
    async fn read_dir(&self, path: &Path) -> Result<Vec<DirEntry>, ExecError> {
        self.1.read_dir(path).await
    }
    async fn remove_dir_all(&self, path: &Path) -> Result<(), ExecError> {
        self.1.remove_dir_all(path).await
    }
    async fn metadata(&self, path: &Path) -> Result<Metadata, ExecError> {
        self.1.metadata(path).await
    }
    async fn canonicalize(&self, path: &Path) -> Result<PathBuf, ExecError> {
        self.1.canonicalize(path).await
    }
}

struct RecordingNetwork(Arc<Seen>, OfflineNetwork);

#[async_trait]
impl Network for RecordingNetwork {
    async fn send(&self, req: HttpRequest, origin: Origin) -> Result<HttpResponse, ExecError> {
        self.0
            .requests
            .lock()
            .unwrap()
            .push((req.url.clone(), origin));
        self.1.send(req, origin).await
    }
    fn permits(&self, host: &str, origin: Origin) -> bool {
        self.1.permits(host, origin)
    }
}

struct RecordingSandbox(Arc<Seen>);

impl Sandbox for RecordingSandbox {
    fn confine(&self, spec: ProcessSpec, policy: &SandboxPolicy) -> Confined {
        self.0.confined.lock().unwrap().push(spec.program.clone());
        NoSandbox.confine(spec, policy)
    }
}

struct CountingCounter(Arc<Seen>);

impl TokenCounter for CountingCounter {
    fn count_text(&self, text: &str) -> usize {
        text.len()
    }
    fn count_block(&self, _: &ModelContentBlock) -> usize {
        1
    }
    fn count_messages(&self, messages: &[ModelMessage]) -> usize {
        self.0.counted.fetch_add(1, Ordering::SeqCst);
        messages.len()
    }
}

struct CountingRules(Arc<Seen>);

impl RuleProvider for CountingRules {
    fn id(&self) -> &str {
        "probe"
    }
    fn rules(&self) -> Vec<RuleEntry> {
        self.0.rules_asked.fetch_add(1, Ordering::SeqCst);
        vec![RuleEntry {
            name: "probe".into(),
            description: "the probe's rule".into(),
            path: RULE_PATH.into(),
        }]
    }
}

const RESULT_MARK: &str = "RESULT-MARK-FROM-TRANSFORMER";
const BLOCK_MARK: &str = "BLOCK-MARK-FROM-REGISTRY";
const CONTEXT_MARK: &str = "CONTEXT-MARK-FROM-PROVIDER";
const VARIABLE_MARK: &str = "VARIABLE-MARK-FROM-PROVIDER";
const ASSEMBLER_MARK: &str = "ASSEMBLER-MARK-FROM-HOST";
const ASSEMBLE_MARK: &str = "ASSEMBLE-MARK-FROM-HOOK";
const SUBMIT_MARK: &str = "SUBMIT-MARK-FROM-MIDDLEWARE";
const SCENE_MARK: &str = "SCENE-MARK-FROM-HOST";
const REQUEST_MARK: &str = "REQUEST-MARK-FROM-INTERCEPTOR";
const INSTRUCTION_MARK: &str = "INSTRUCTION-MARK-FROM-PROVIDER";

struct ProbeTool(Arc<Seen>);

#[async_trait]
impl Tool for ProbeTool {
    fn name(&self) -> &str {
        "ProbeTool"
    }
    fn description(&self) -> &str {
        "a tool the host registered"
    }
    fn spec(&self) -> ToolSpec {
        ToolSpec::new().read_only(true)
    }
    fn input_schema(&self, _: PathStyle) -> serde_json::Value {
        serde_json::json!({"type": "object"})
    }
    async fn permission_facts(&self, _: &serde_json::Value, _: &ToolContext) -> PermissionFacts {
        PermissionFacts::new(Subject::None, Reach::Unknown)
    }
    async fn call(
        &self,
        _: serde_json::Value,
        _: ToolContext,
        _: ProgressSender,
    ) -> Result<ToolResult, ToolError> {
        self.0.hit("tool.registry");
        Ok(ToolResult::text("probe ran"))
    }
}

struct Around(Arc<Seen>);

#[async_trait]
impl ToolMiddleware for Around {
    async fn around(
        &self,
        call: &ToolCall,
        exec: &mut ToolExec,
        next: NextDispatch<'_>,
    ) -> ToolOutcome {
        if call.name == "TodoWrite" {
            self.0.hit("tool.around");
        }
        next.run(call, exec).await
    }
}

struct Marks(Arc<Seen>);

impl ToolResultTransformer for Marks {
    fn transform(&self, _: &ToolCall, draft: &mut ToolResultDraft) {
        draft.text.push_str(RESULT_MARK);
        self.0.hit("tool.result");
    }
}

struct AppendsBlock;

impl PromptAssembler for AppendsBlock {
    fn assemble(&self, request: &AssemblyRequest<'_>) -> Vec<PromptBlock> {
        let mut blocks = DefaultAssembler.assemble(request);
        blocks.push(PromptBlock::system(ASSEMBLER_MARK));
        blocks
    }
}

struct AppendsAtAssembly;

impl AssemblyHook for AppendsAtAssembly {
    fn on_assemble(
        &self,
        assembly: &mut PromptAssembly,
        _: &ScenePromptContext<'_>,
    ) -> Result<(), String> {
        assembly.push(PromptBlock::system(ASSEMBLE_MARK));
        Ok(())
    }
}

struct Rewrites(Arc<Seen>);

#[async_trait]
impl PromptMiddleware for Rewrites {
    async fn inspect(&self, _: &PromptCall<'_>) -> PromptDecision {
        self.0.hit("prompt.submit");
        PromptDecision::Rewrite(SUBMIT_MARK.to_string())
    }
}

struct Healthy(Arc<Seen>);

impl HealthCheck for Healthy {
    fn name(&self) -> &str {
        "probe"
    }
    fn check(&self) -> CheckResult {
        self.0.hit("health.check");
        CheckResult {
            status: HealthStatus::Ok,
            summary: "fine".into(),
            details: serde_json::Value::Null,
        }
    }
}

struct AnswersHostCalls(Arc<Seen>);

#[async_trait]
impl HostCalls for AnswersHostCalls {
    async fn call(&self, _: HostCall, _: &CancellationToken) -> HostCallEnd {
        self.0.hit("host_calls.call");
        HostCallEnd::Replied(HostReply::Result(ToolResult::text("the host answered")))
    }
}

struct Permits(Arc<Seen>);

#[async_trait]
impl PermissionPolicy for Permits {
    async fn decide(&self, _: &PermissionRequest<'_>) -> PermissionOutcome {
        self.0.hit("permission.check");
        PermissionOutcome::Permit {
            basis: Basis::Bypass,
            profile: ExecProfile::for_permit(&Basis::Bypass, None, Attendance::Attended),
            obligations: Obligations::scanning_nested_repos(false),
        }
    }
}

struct KeepsGrants(Arc<Seen>);

#[async_trait]
impl PermissionGrants for KeepsGrants {
    async fn grant(&self, _: Grant) -> Result<(), GrantError> {
        self.0.hit("permission.grants");
        Ok(())
    }
    fn clear(&self, _: GrantScope) {
        self.0.hit("permission.grants");
    }
}

struct Intercepts(Arc<Seen>);

impl ModelInterceptor for Intercepts {
    fn on_request(&self, request: &mut ModelRequestView) {
        request
            .prompt_blocks
            .push(PromptBlock::system(REQUEST_MARK));
        self.0.hit("model.request");
    }
    fn on_message(&self, _: &mut ModelMessage) {
        self.0.hit("model.message");
    }
}

struct Retrieves(Arc<Seen>);

#[async_trait]
impl MemoryRetriever for Retrieves {
    async fn retrieve(
        &self,
        _: &dyn MemoryStorage,
        _: &dyn Model,
        _: &RetrievalRequest,
    ) -> Vec<String> {
        self.0.hit("memory.retriever");
        Vec::new()
    }
}

impl RetrievalHook for Retrieves {
    fn before_retrieve(&self, _: &mut RetrievalRequest) {
        self.0.hit("memory.retrieval_hook");
    }
}

struct Instructs;

impl InstructionProvider for Instructs {
    fn id(&self) -> &str {
        "probe"
    }
    fn documents(&self) -> Vec<InstructionDoc> {
        vec![InstructionDoc {
            path: "/srv/AGENTS.md".into(),
            content: INSTRUCTION_MARK.into(),
        }]
    }
}

struct WatchesTurns(Arc<Seen>);

impl TurnPolicy for WatchesTurns {
    fn before_model_call(&self, _: &TurnProgress<'_>) -> TurnStep {
        self.0.hit("turn.policy");
        TurnStep::Continue
    }
}

struct Retries(Arc<Seen>);

impl RecoveryPolicy for Retries {
    fn on_failure(&self, _: &ModelFailure<'_>, _: &CallRecovery) -> Recovery {
        self.0.hit("model.recovery");
        Recovery::RetrySame {
            after: std::time::Duration::ZERO,
        }
    }
    fn on_early_stop(&self, _: &str, _: &RecoveryAttempt) -> StopRecovery {
        StopRecovery::Accept
    }
}

struct WatchesSpend(Arc<Seen>);

impl BudgetPolicy for WatchesSpend {
    fn on_usage(&self, _: &Spend) -> Spending {
        self.0.hit("budget");
        Spending::WithinBudget
    }
}

struct WatchesClock(Arc<Seen>);

impl Environment for WatchesClock {
    fn now(&self) -> time::OffsetDateTime {
        self.0.hit("environment");
        time::OffsetDateTime::UNIX_EPOCH
    }
    fn new_id(&self) -> base::id::Id {
        self.0.hit("environment");
        base::id::Id::new()
    }
}

struct WatchesCompaction(Arc<Seen>);

#[async_trait]
impl Compactor for WatchesCompaction {
    async fn compact(
        &self,
        _: Vec<ModelMessage>,
        _: usize,
        _: usize,
        _: &dyn TokenCounter,
    ) -> Result<(Vec<ModelMessage>, CompactResult), CompactError> {
        self.0.hit("compaction");
        Err(CompactError::NotApplicable("probe".into()))
    }
    fn trim_stale(&self, _: &mut [ModelMessage], _: &StaleTrimInput<'_>) -> StaleTrim {
        self.0.hit("compaction");
        StaleTrim::default()
    }
}

struct Registers(Arc<Seen>);

impl Plugin for Registers {
    fn name(&self) -> &str {
        "probe-plugin"
    }
    fn register(&self, _: &PluginSession<'_>, _: &mut Registrar) {
        self.0.hit("script.carrier");
    }
}

struct AsksHostHooks(Arc<Seen>);

impl HostHookSource for AsksHostHooks {
    fn has_hooks_for(&self, event: HookEvent) -> bool {
        if event == HookEvent::UserPromptSubmit {
            self.0.hit("hooks.host");
        }
        false
    }
    fn hooks_for(&self, _: HookEvent) -> Vec<HostHook> {
        Vec::new()
    }
}

struct JudgesPrompts(Arc<Seen>);

#[async_trait]
impl PromptHookExecutor for JudgesPrompts {
    async fn execute(
        &self,
        _: &str,
        _: Option<&str>,
        _: &HookInput,
        _: &CancellationToken,
    ) -> Result<String, String> {
        self.0.hit("hooks");
        Ok("{}".to_string())
    }
}

/// Answers per its plan, and keeps everything it was sent.
struct ScriptedModel {
    seen: Arc<Seen>,
    plan: Plan,
}

fn stream_of(events: Vec<ModelEvent>) -> ModelStream {
    let events: Vec<Result<ModelEvent, ModelError>> = events.into_iter().map(Ok).collect();
    Box::new(futures::stream::iter(events))
}

fn answer() -> Vec<ModelEvent> {
    vec![
        ModelEvent::TextDelta { text: "ok".into() },
        ModelEvent::EndTurn {
            stop_reason: "end_turn".into(),
            usage: Default::default(),
        },
    ]
}

#[async_trait]
impl Model for ScriptedModel {
    fn api_type(&self) -> base::provider::ApiType {
        base::provider::ApiType::Anthropic
    }
    async fn stream(
        &self,
        prompt_blocks: Vec<PromptBlock>,
        _tools: Vec<ToolDef>,
        messages: Vec<ModelMessage>,
        _params: StreamParams,
        _cancel: CancellationToken,
    ) -> Result<ModelStream, ModelError> {
        let call = self.seen.model_calls.fetch_add(1, Ordering::SeqCst);
        self.seen
            .prompts
            .lock()
            .unwrap()
            .extend(prompt_blocks.into_iter().map(|b| b.content));
        self.seen
            .conversation
            .lock()
            .unwrap()
            .push_str(&format!("{messages:?}"));
        match (self.plan, call) {
            (Plan::FailOnce, 0) => Err(ModelError::overloaded()),
            (Plan::Tool(name, input), 0) => Ok(stream_of(vec![
                ModelEvent::ToolUse {
                    id: "probe-call".into(),
                    name: name.into(),
                    input: serde_json::from_str(input).unwrap(),
                },
                ModelEvent::EndTurn {
                    stop_reason: "tool_use".into(),
                    usage: Default::default(),
                },
            ])),
            (Plan::Recall, 0) => Ok(stream_of(vec![
                ModelEvent::ToolUse {
                    id: "probe-call".into(),
                    name: "TodoWrite".into(),
                    input: serde_json::from_str(TODO_INPUT).unwrap(),
                },
                ModelEvent::EndTurn {
                    stop_reason: "tool_use".into(),
                    usage: Default::default(),
                },
            ])),
            _ => Ok(stream_of(answer())),
        }
    }
}

fn settings(dir: &Path, memory: bool) -> LoadedSettings {
    let mut values = Settings::defaults_for("test-model");
    values.model.base_url = MODEL_ENDPOINT.into();
    values.memory_enabled = memory;
    values.telemetry_enabled = false;
    let mut paths = PathSettings::default();
    paths.user_data_dir = dir.join("user");
    paths.global_data_dir = dir.join("global");
    paths.workspace = base::paths::Workspace::project_of(dir);
    paths.local_data_dir = dir.to_path_buf();
    LoadedSettings::new(values, paths)
}

fn builder(dir: &Path, probes: &Probes, plan: Plan) -> Builder {
    Builder::new()
        .scene(Arc::new(scene::code::spec(
            scene::code::Interaction::Interactive,
        )))
        .model(probes.model.clone())
        .settings(Arc::new(settings(dir, plan.memory())))
}

/// What a session left behind for the probes to look at.
struct Ran {
    cwd: PathBuf,
    bound: Option<ExecProviders>,
    prompts: Vec<String>,
    health: Arc<HealthChecks>,
}

/// Warmup and one turn, through `Agent::run` as a host drives it.
async fn run_session(probe: &Probe, probes: &Probes) -> Ran {
    let dir = tempfile::tempdir().unwrap();
    let (mut agent, mut events, input) =
        (probe.install)(builder(dir.path(), probes, probe.plan), probes)
            .skip_warmup(!probe.id.starts_with("exec."))
            .build()
            .expect("the session builds");
    let health = agent.health();
    let cwd = agent.settings().paths.working_dir().to_path_buf();
    let bound = agent.session_state().exec();
    let session = CancellationToken::new();
    let engine = tokio::spawn({
        let session = session.clone();
        async move { agent.run(session).await }
    });
    input
        .send(InputMessage::User {
            content: "go".into(),
            attachments: vec![],
            turn_id: "t1".into(),
        })
        .unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(20), async {
        while let Some(event) = events.recv().await {
            if matches!(event, AgentEvent::TurnComplete { .. }) {
                return;
            }
        }
        panic!("the session ended before its turn completed");
    })
    .await
    .expect("the turn completes");
    session.cancel();
    drop(input);
    engine.await.unwrap();
    let prompts = probes.seen.prompts.lock().unwrap().clone();
    Ran {
        cwd,
        bound,
        prompts,
        health,
    }
}

fn builder_wired() -> BTreeSet<&'static str> {
    catalog::all()
        .iter()
        .filter(|p| p.wired_by.iter().any(|w| matches!(w, Wiring::Builder(_))))
        .map(|p| p.id)
        .collect()
}

#[test]
fn every_point_a_builder_method_wires_is_probed() {
    let probed: BTreeSet<&str> = PROBES.iter().map(|p| p.id).collect();
    assert_eq!(probed.len(), PROBES.len(), "a point is probed twice");
    assert_eq!(
        probed,
        builder_wired(),
        "the catalog's `Wiring::Builder` points are not the probed ones"
    );
}

#[test]
fn each_probe_calls_the_method_the_catalog_names() {
    for probe in PROBES {
        let point = catalog::find(probe.id)
            .unwrap_or_else(|| panic!("`{}` is not in the catalog", probe.id));
        assert!(
            point.wired_by.contains(&Wiring::Builder(probe.builder)),
            "the catalog does not say `Builder::{}` wires `{}`: {:?}",
            probe.builder,
            probe.id,
            point.wired_by
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn each_builder_point_is_used_by_the_session_it_was_given_to() {
    let mut unused = Vec::new();
    for probe in PROBES {
        let probes = Probes::new(probe.plan);
        let ran = run_session(probe, &probes).await;
        if let Err(why) = (probe.used)(&probes, &ran) {
            let talked = probes.seen.conversation.lock().unwrap().clone();
            let tail: String = talked
                .chars()
                .rev()
                .take(700)
                .collect::<Vec<_>>()
                .into_iter()
                .rev()
                .collect();
            unused.push(format!(
                "{} (`Builder::{}`): {why}\n    conversation ends: {tail}",
                probe.id, probe.builder
            ));
        }
    }
    assert!(
        unused.is_empty(),
        "given through the Builder and never used:\n  {}",
        unused.join("\n  ")
    );
}

/// Captures what the session hands its modules, which is what a delegate is
/// built on.
struct HandedToModules(Mutex<Option<HostContracts>>);

impl Module for HandedToModules {
    fn id(&self) -> &'static str {
        "handed-to-modules"
    }
    fn on_session(&self, cx: &mut SessionContext<'_>) -> Result<(), ModuleError> {
        *self.0.lock().unwrap() = Some(cx.host.clone());
        Ok(())
    }
}

#[test]
fn a_delegate_is_built_on_the_contracts_the_builder_was_given() {
    let dir = tempfile::tempdir().unwrap();
    let probes = Probes::new(Plan::Answer);
    let module = Arc::new(HandedToModules(Mutex::new(None)));
    let _session = builder(dir.path(), &probes, Plan::Answer)
        .exec(probes.exec.clone())
        .token_counter(probes.counter.clone())
        .rule_provider(probes.rules.clone())
        .module(module.clone())
        .skip_warmup(true)
        .build()
        .expect("the session builds");
    let host = module.0.lock().unwrap().take().expect("the module was run");
    assert!(Arc::ptr_eq(&host.exec.process, &probes.exec.process));
    assert!(Arc::ptr_eq(&host.exec.filesystem, &probes.exec.filesystem));
    assert!(Arc::ptr_eq(&host.exec.network, &probes.exec.network));
    assert!(Arc::ptr_eq(&host.exec.sandbox, &probes.exec.sandbox));
    assert!(Arc::ptr_eq(&host.token_counter, &probes.counter));
    assert!(Arc::ptr_eq(&host.rules, &probes.rules));
}
