//! 冻结 `Builder::build` 装出来的会话：registry 的工具名（按注册顺序）、`EngineConfig`
//! 的路径风格与延迟工具、`SessionState` 的 env，以及一个探针模块在 `on_session` 时
//! 看到的东西；再加几条拒绝构建的错误消息。拆 `build` 之前的特征化测试，要改时用
//! `UPDATE_GOLDEN=1 cargo test -p runtime --lib agent::tests::build_snapshot` 重新生成。
//!
//! 工具表依赖 `web`、`structured-output`、`ptc` 三个默认 feature，精简构建下不跑。

#![cfg(all(feature = "web", feature = "structured-output", feature = "ptc"))]

use super::*;
use crate::module::{Module, ModuleError, SessionContext};
use base::context::Session;
use std::fmt::Write as _;
use std::path::Path;
use std::sync::Mutex;

const GOLDEN: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/tests/golden/build_snapshot.txt"
);

struct Named(&'static str);

#[async_trait::async_trait]
impl base::tool::Tool for Named {
    fn name(&self) -> &str {
        self.0
    }
    fn spec(&self) -> base::tool::ToolSpec {
        base::tool::ToolSpec::new()
    }
    fn description(&self) -> &str {
        "named (test tool)"
    }
    fn input_schema(&self, _: base::tool::PathStyle) -> serde_json::Value {
        serde_json::json!({"type": "object"})
    }
    async fn call(
        &self,
        _: serde_json::Value,
        _: base::tool::ToolContext,
        _: base::tool::ProgressSender,
    ) -> Result<base::tool::ToolResult, base::error::ToolError> {
        Ok(base::tool::ToolResult::text("done"))
    }
}

struct Probe {
    seen: Arc<Mutex<Vec<String>>>,
}

impl Module for Probe {
    fn id(&self) -> &'static str {
        "probe"
    }

    fn on_session(&self, cx: &mut SessionContext<'_>) -> Result<(), ModuleError> {
        let agent_types: Vec<&str> = cx
            .plugin_agent_types
            .iter()
            .map(|t| t.name.as_str())
            .collect();
        self.seen.lock().unwrap().push(format!(
            "tools={:?} agent_types={agent_types:?} depth={}",
            cx.tools.names(),
            cx.depth
        ));
        Ok(())
    }
}

struct NeedsMissing;

impl Module for NeedsMissing {
    fn id(&self) -> &'static str {
        "needs-missing"
    }

    fn requires(&self) -> &'static [&'static str] {
        &["never-assembled"]
    }

    fn on_session(&self, _: &mut SessionContext<'_>) -> Result<(), ModuleError> {
        Ok(())
    }
}

struct Contributes {
    tool: &'static str,
    block: bool,
}

impl crate::plugin::Plugin for Contributes {
    fn name(&self) -> &str {
        "snapshot-plugin"
    }

    fn register(&self, _: &crate::plugin::PluginSession<'_>, r: &mut crate::plugin::Registrar) {
        r.tool(Arc::new(Named(self.tool)));
        if self.block {
            r.prompt_block(base::interface::prompt_registry::RegisteredBlock::system(
                "snapshot_block",
                5,
                "from the plugin",
            ));
            r.agent_type(base::agent_type::AgentTypeDefinition {
                name: "snapshot-type".into(),
                ..Default::default()
            });
        }
    }
}

fn scene_deferring(id: &str, deferred: &[&str]) -> SceneSpec {
    let mut s = bare_scene(id);
    s.interactive = true;
    s.tools.deferred = deferred.iter().map(|n| n.to_string()).collect();
    s
}

fn template_of(tools: Vec<Arc<dyn base::tool::Tool>>) -> Arc<InMemoryToolRegistry> {
    let template = Arc::new(InMemoryToolRegistry::new());
    for t in tools {
        template.register(t);
    }
    template
}

fn builder_for(scene: SceneSpec, template: Arc<InMemoryToolRegistry>) -> Builder {
    Builder::new()
        .scene(Arc::new(scene))
        .model(Arc::new(DummyModel) as Arc<dyn Model>)
        .settings(Arc::new(test_settings()))
        .tools(template)
        .skip_warmup(true)
}

fn describe(agent: &Agent, seen: &Mutex<Vec<String>>) -> String {
    let mut out = String::new();
    writeln!(out, "tools: {:?}", agent.tooling.tools().names()).unwrap();
    writeln!(out, "path_style: {:?}", agent.config.path_style).unwrap();
    let mut deferred: Vec<&String> = agent.config.deferred_tools.iter().collect();
    deferred.sort();
    writeln!(out, "deferred_tools: {deferred:?}").unwrap();
    let env = agent.session_state.env();
    writeln!(out, "env.cwd: {}", env.cwd.display()).unwrap();
    writeln!(
        out,
        "env.additional_writable_dirs: {:?}",
        env.additional_writable_dirs
    )
    .unwrap();
    writeln!(out, "env.read_directories: {:?}", env.read_directories).unwrap();
    writeln!(
        out,
        "env.flags: read_anywhere={} unattended={} base_mode={:?}",
        env.read_anywhere, env.unattended, env.base_mode
    )
    .unwrap();
    let blocks = agent.prompt.registry().blocks();
    let names: Vec<&str> = blocks.iter().map(|b| b.name.as_str()).collect();
    writeln!(out, "prompt_registry_blocks: {names:?}").unwrap();
    for line in seen.lock().unwrap().iter() {
        writeln!(out, "probe: {line}").unwrap();
    }
    out
}

fn build_described(builder: Builder, seen: &Arc<Mutex<Vec<String>>>) -> String {
    let (agent, _rx, _tx) = builder
        .module(Arc::new(Probe { seen: seen.clone() }))
        .build()
        .expect("build should succeed");
    describe(&agent, seen)
}

fn refusal(builder: Builder) -> String {
    match builder.build() {
        Ok(_) => "built".into(),
        Err(e) => e.to_string(),
    }
}

#[tokio::test]
async fn build_matches_the_golden_snapshot() {
    let mut out = String::new();
    let mut section = |name: &str, body: String| {
        writeln!(out, "## {name}\n{body}").unwrap();
    };
    let fresh = || Arc::new(Mutex::new(Vec::new()));
    let structured = || -> Arc<dyn base::tool::Tool> {
        Arc::new(tools::structured_output::StructuredOutputTool::default())
    };

    {
        let seen = fresh();
        let mut settings = test_settings();
        settings.values.files.extra_dirs = vec!["extra".into()];
        settings.values.files.read_dirs = vec!["docs".into()];
        let builder = builder_for(
            scene_deferring("minimal", &["StructuredOutput", "Later"]),
            template_of(vec![Arc::new(Named("Early")), structured()]),
        )
        .settings(Arc::new(settings));
        section("minimal", build_described(builder, &seen));
    }

    {
        let seen = fresh();
        let builder = builder_for(
            scene_deferring("plugin", &[]),
            template_of(vec![Arc::new(Named("Early"))]),
        )
        .plugin(Arc::new(Contributes {
            tool: "PluginTool",
            block: true,
        }));
        section(
            "rust plugin with prompt block",
            build_described(builder, &seen),
        );
    }

    {
        let seen = fresh();
        let builder = builder_for(
            scene_deferring("schema", &["StructuredOutput", "Later"]),
            template_of(vec![Arc::new(Named("Early")), structured()]),
        )
        .output_schema(serde_json::json!({"type": "object"}));
        section("output schema", build_described(builder, &seen));
    }

    {
        let seen = fresh();
        let mut scene = bare_scene("unattended");
        scene.interactive = false;
        let mut template: Vec<Arc<dyn base::tool::Tool>> = vec![Arc::new(Named("Early"))];
        template.extend(
            base::interface::scene::NEEDS_A_USER
                .iter()
                .map(|n| Arc::new(Named(n)) as Arc<dyn base::tool::Tool>),
        );
        let builder = builder_for(scene, template_of(template)).question_prompts(true);
        section("unattended scene", build_described(builder, &seen));
    }

    {
        let seen = fresh();
        let mut scene = bare_scene("unattended");
        scene.interactive = false;
        let mut template: Vec<Arc<dyn base::tool::Tool>> = vec![Arc::new(Named("Early"))];
        template.extend(
            base::interface::scene::NEEDS_A_USER
                .iter()
                .map(|n| Arc::new(Named(n)) as Arc<dyn base::tool::Tool>),
        );
        let builder = builder_for(scene, template_of(template))
            .question_prompts(true)
            .attended(Some(true));
        section(
            "unattended scene, attended session",
            build_described(builder, &seen),
        );
    }

    {
        let seen = fresh();
        let mut scene = bare_scene("whitelist");
        scene.tools = base::interface::scene::ToolSurface::allowing(&["Early"]);
        let builder = builder_for(
            scene,
            template_of(vec![Arc::new(Named("Early")), structured()]),
        )
        .output_schema(serde_json::json!({"type": "object"}));
        section(
            "output schema through a whitelist",
            build_described(builder, &seen),
        );
    }

    {
        let mut scene = bare_scene("refused");
        scene.tools = base::interface::scene::ToolSurface::denying(&["StructuredOutput"]);
        let builder = builder_for(scene, template_of(vec![structured()]))
            .output_schema(serde_json::json!({"type": "object"}));
        section("refused: output schema denied", refusal(builder));
    }

    {
        let seen = fresh();
        let runner = Arc::new(local_hook_runner(Default::default()));
        let builder = builder_for(
            scene_deferring("delegate", &[]),
            template_of(vec![Arc::new(Named("Early"))]),
        )
        .agent_depth(1)
        .hooks(runner.clone());
        let (agent, _rx, _tx) = builder
            .module(Arc::new(Probe { seen: seen.clone() }))
            .build()
            .expect("build should succeed");
        let mut body = describe(&agent, &seen);
        writeln!(
            body,
            "hooks_are_the_callers: {}",
            std::ptr::eq(agent.hooks(), runner.as_ref())
        )
        .unwrap();
        section("depth 1 with hooks from outside", body);
    }

    {
        let seen = fresh();
        let template = Arc::new(InMemoryToolRegistry::new());
        template.register(Arc::new(Named("Alpha")));
        template.register(Arc::new(tools::tool_search::ToolSearchTool::new(
            template.clone(),
        )));
        template.register(Arc::new(Named("Beta")));
        let builder = builder_for(scene_deferring("search", &["Beta", "ToolSearch"]), template);
        section("template with ToolSearch", build_described(builder, &seen));
    }

    {
        let providers_and_ledger = builder_for(bare_scene("refused"), template_of(vec![]))
            .exec(base::interface::exec::ExecProviders::in_process())
            .process_groups(platform::GroupLedger::new());
        section(
            "refused: exec and process_groups",
            refusal(providers_and_ledger),
        );
    }

    {
        let dir = tempfile::tempdir().unwrap();
        let store = history::store::JsonlHistoryStore::with_roots(
            dir.path(),
            history::path::HistoryRoots::under(dir.path()),
        )
        .await
        .expect("store should build");
        let bad_id = builder_for(bare_scene("refused"), template_of(vec![]))
            .history_store(Arc::new(store))
            .session_id("not-a-session-id");
        section("refused: unparseable session_id", refusal(bad_id));
    }

    {
        let mut scene = bare_scene("refused");
        scene.extra_tools = vec![Arc::new(Named("Taken"))];
        let builder = builder_for(scene, template_of(vec![Arc::new(Named("Taken"))]));
        section("refused: scene tool name taken", refusal(builder));
    }

    {
        let builder = builder_for(
            bare_scene("refused"),
            template_of(vec![Arc::new(Named("Taken"))]),
        )
        .plugin(Arc::new(Contributes {
            tool: "Taken",
            block: false,
        }));
        section("refused: plugin tool name taken", refusal(builder));
    }

    {
        let builder =
            builder_for(bare_scene("refused"), template_of(vec![])).module(Arc::new(NeedsMissing));
        section("refused: module order", refusal(builder));
    }

    if std::env::var_os("UPDATE_GOLDEN").is_some() {
        std::fs::create_dir_all(Path::new(GOLDEN).parent().unwrap()).unwrap();
        std::fs::write(GOLDEN, &out).unwrap();
        return;
    }
    let expected = std::fs::read_to_string(GOLDEN)
        .unwrap_or_else(|e| panic!("{GOLDEN}: {e}; generate it with UPDATE_GOLDEN=1"));
    if out != expected {
        let first = out
            .lines()
            .zip(expected.lines())
            .position(|(a, e)| a != e)
            .unwrap_or_else(|| out.lines().count().min(expected.lines().count()));
        panic!(
            "build snapshot drifted from {GOLDEN} at line {}: {:?} != {:?}",
            first + 1,
            out.lines().nth(first),
            expected.lines().nth(first),
        );
    }
}

fn local_hook_runner(settings: hooks::HooksSettings) -> hooks::HookRunner {
    let providers = platform::local_providers();
    hooks::HookRunner::new(
        settings,
        hooks::HookExec {
            process: providers.process,
            network: providers.network,
        },
    )
}

#[tokio::test]
async fn a_session_in_a_worktree_works_there_and_writes_what_a_commit_needs_of_the_git_directory() {
    let dir = tempfile::tempdir().unwrap();
    let repo = dir.path().canonicalize().unwrap();
    let out = std::process::Command::new("sh")
        .args([
            "-c",
            "git init -q . && git -c user.name=t -c user.email=t@t commit -q --allow-empty -m i \
             && git worktree add -q -b wt .wkts/wt",
        ])
        .current_dir(&repo)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .output()
        .unwrap();
    assert!(out.status.success(), "{out:?}");
    let worktree = repo.join(".wkts/wt");
    let elsewhere = tempfile::tempdir().unwrap();
    let elsewhere = elsewhere.path().canonicalize().unwrap();
    // What a sandboxed command could have done since the worktree was made:
    // the links name another directory, which must not become writable.
    std::fs::write(
        repo.join(".git/worktrees/wt/commondir"),
        format!("{}\n", elsewhere.display()),
    )
    .unwrap();
    let env_for = |git: base::paths::WorktreeGit| {
        let mut settings = test_settings();
        settings.paths.workspace = base::paths::Workspace::Project(repo.clone());
        let config = settings::SessionConfig::of(Arc::new(settings)).with_worktree(Some(
            base::paths::LinkedWorktree {
                dir: worktree.clone(),
                git: Some(git),
            },
        ));
        let builder =
            builder_for(scene_deferring("wt", &[]), template_of(Vec::new())).session_config(config);
        let (agent, _rx, _tx) = builder.build().expect("build should succeed");
        let env = agent.session_state.env();
        let sandbox = agent.config.sandbox.worktree_git.clone();
        (env, sandbox)
    };
    let git = repo.join(".git");
    let own = base::paths::WorktreeGit {
        dir: git.clone(),
        admin: git.join("worktrees/wt"),
        branch: "wt".into(),
    };

    let (env, sandbox) = env_for(own.clone());
    assert_eq!(env.cwd, worktree);
    for dir in own.writable() {
        assert!(
            env.additional_writable_dirs.contains(&dir),
            "{} missing from {:?}",
            dir.display(),
            env.additional_writable_dirs
        );
    }
    assert!(
        !env.additional_writable_dirs.contains(&git),
        "the whole git directory was made writable: {:?}",
        env.additional_writable_dirs
    );
    assert!(
        env.read_directories.contains(&git),
        "{:?}",
        env.read_directories
    );
    assert_eq!(sandbox, Some(own.clone()));
    assert!(!env
        .additional_writable_dirs
        .iter()
        .any(|d| d.starts_with(&elsewhere)));

    for (what, git) in [
        (
            "another git directory",
            base::paths::WorktreeGit {
                dir: elsewhere.clone(),
                ..own.clone()
            },
        ),
        (
            "an entry outside worktrees/",
            base::paths::WorktreeGit {
                admin: git.join("refs"),
                ..own.clone()
            },
        ),
        (
            "a branch climbing out of refs/heads/",
            base::paths::WorktreeGit {
                branch: "../../config".into(),
                ..own.clone()
            },
        ),
    ] {
        let (env, sandbox) = env_for(git);
        assert!(
            !env.additional_writable_dirs
                .iter()
                .any(|d| d.starts_with(&elsewhere) || d.starts_with(repo.join(".git"))),
            "{what} made something writable: {:?}",
            env.additional_writable_dirs
        );
        assert_eq!(sandbox, None, "{what}");
    }
}
