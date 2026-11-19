use super::*;
use async_trait::async_trait;
use base::command::ExtCommand;
use base::interface::host_js::{HostCallContext, HostJsError, HostJsLib};
use base::interface::tool_dispatch::{CallableTool, CommandScope, SubCallAnswer, ToolDispatch};
use base::tool::{ProgressSender, Tool, ToolContext, ToolResult, ToolResultContent};
use serde_json::{json, Value};
use std::path::Path;
use std::time::Duration;
use tokio_util::sync::CancellationToken;

struct NoTools;

#[async_trait]
impl ToolDispatch for NoTools {
    fn callable(&self) -> Vec<CallableTool> {
        vec![]
    }
    fn concurrency_safe(&self, _: &str, _: &Value) -> bool {
        true
    }
    async fn call(
        &self,
        name: &str,
        _: Value,
        _: CancellationToken,
    ) -> Result<SubCallAnswer, String> {
        Err(format!("{name} is not here"))
    }
    fn for_command(&self, _: CommandScope) -> Option<Arc<dyn ToolDispatch>> {
        Some(Arc::new(NoTools))
    }
}

fn command(root: &Path, source: &str, uses: &[&str]) -> ExtCommand {
    std::fs::write(root.join("cmd.js"), source).unwrap();
    ExtCommand {
        name: "flows".into(),
        description: "flows".into(),
        plugin: "flow-plugin".into(),
        plugin_version: "2.0.0".into(),
        source: Default::default(),
        root: root.to_path_buf(),
        file: root.join("cmd.js"),
        function: "main".into(),
        read_only: vec!["".into()],
        writes: Vec::new(),
        uses: uses.iter().map(|u| u.to_string()).collect(),
        bytecode_dir: None,
    }
}

/// `codix.flow.start` answers with what it was called with and from where;
/// `codix.flow.fail` fails; `codix.flow.wait` waits for its cancellation
/// and says it came; `codix.flow.big` answers with `n` characters.
fn codix(cancelled: Arc<tokio::sync::Notify>) -> HostJsLib {
    HostJsLib::new("codix")
        .function("flow.start", |cx: HostCallContext, input| async move {
            Ok(json!({
                "input": input,
                "plugin": cx.plugin,
                "version": cx.plugin_version,
                "entry": cx.entry,
                "session": cx.session_id,
                "projectRoot": cx.project_root,
                "cwd": cx.cwd,
                "via": cx.via,
                "depth": cx.agent_depth,
            }))
        })
        .function("flow.fail", |_, _| async {
            Err(HostJsError::new("denied", "not in this project"))
        })
        .function("flow.wait", move |cx: HostCallContext, _| {
            let cancelled = cancelled.clone();
            async move {
                cx.cancel.cancelled().await;
                cancelled.notify_one();
                Ok(Value::Null)
            }
        })
        .function("flow.big", |_, n: Value| async move {
            Ok(Value::String("x".repeat(n.as_u64().unwrap_or(0) as usize)))
        })
        .function("flow.deep", |_, n: Value| async move {
            let n = n.as_u64().unwrap_or(0);
            Ok((0..n).fold(Value::Null, |inner, _| Value::Array(vec![inner])))
        })
}

fn tool(dir: &Path, source: &str, uses: &[&str], libs: Vec<HostJsLib>) -> RunExtTool {
    RunExtTool::new(vec![command(dir, source, uses)])
        .with_host_libraries(HostLibraries::new(libs))
        .with_limits(Limits {
            call_result_chars: 1_000,
            time: Duration::from_secs(20),
            ..Limits::for_commands()
        })
}

fn context() -> ToolContext {
    let mut ctx =
        session_state::testing::tool_context_for_test(platform::local_providers(), "/proj".into());
    ctx.tool_use_id = "toolu_ext".into();
    ctx.session_id = "sess-1".into();
    ctx.via = "user".into();
    ctx.dispatch = Some(Arc::new(NoTools));
    ctx
}

async fn run_in(tool: &RunExtTool, ctx: ToolContext) -> ToolResult {
    tool.call(json!({"command": "flows"}), ctx, ProgressSender::noop("t"))
        .await
        .unwrap()
}

fn text(r: &ToolResult) -> String {
    match &r.content {
        ToolResultContent::Text(s) => s.clone(),
        other => format!("{other:?}"),
    }
}

fn libs() -> Vec<HostJsLib> {
    vec![codix(Arc::default())]
}

#[tokio::test(flavor = "multi_thread")]
async fn a_command_calls_the_host_and_the_host_sees_who_called() {
    let dir = tempfile::tempdir().unwrap();
    let tool = tool(
        dir.path(),
        "async function main() { return await codix.flow.start({ change: 'c1' }); }",
        &["codix"],
        libs(),
    );
    let ctx = context();
    let session = ctx.session.clone();
    let r = run_in(&tool, ctx).await;
    assert!(!r.is_error, "{}", text(&r));
    assert_eq!(
        r.structured_content,
        Some(json!({
            "input": {"change": "c1"},
            "plugin": "flow-plugin",
            "version": "2.0.0",
            "entry": "flows",
            "session": "sess-1",
            "projectRoot": "/proj",
            "cwd": "/proj",
            "via": "user",
            "depth": 0,
        }))
    );
    let output = session
        .find_running_task("toolu_ext")
        .unwrap()
        .output_text();
    assert!(output.contains("codix.flow.start ok "), "{output}");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_host_error_rejects_with_its_code_and_message() {
    let dir = tempfile::tempdir().unwrap();
    let caught = tool(
        dir.path(),
        "async function main() { try { await codix.flow.fail(); } \
         catch (e) { return [e instanceof Error, e.name, e.code, e.message]; } }",
        &["codix"],
        libs(),
    );
    let ctx = context();
    let session = ctx.session.clone();
    let r = run_in(&caught, ctx).await;
    assert_eq!(
        r.structured_content,
        Some(json!([true, "Error", "denied", "not in this project"])),
        "{}",
        text(&r)
    );
    let output = session
        .find_running_task("toolu_ext")
        .unwrap()
        .output_text();
    assert!(output.contains("codix.flow.fail error denied "), "{output}");

    let uncaught = tool(
        dir.path(),
        "async function main() { return await codix.flow.fail(); }",
        &["codix"],
        libs(),
    );
    let r = run_in(&uncaught, context()).await;
    assert!(r.is_error);
    assert!(text(&r).starts_with("not in this project"), "{}", text(&r));
}

#[tokio::test(flavor = "multi_thread")]
async fn a_stopped_script_cancels_the_host_call() {
    let dir = tempfile::tempdir().unwrap();
    let cancelled = Arc::new(tokio::sync::Notify::new());
    let tool = RunExtTool::new(vec![command(
        dir.path(),
        "async function main() { await codix.flow.wait(); return 'done'; }",
        &["codix"],
    )])
    .with_host_libraries(HostLibraries::new(vec![codix(cancelled.clone())]))
    .with_limits(Limits {
        time: Duration::from_millis(300),
        ..Limits::for_commands()
    });
    let r = run_in(&tool, context()).await;
    assert!(r.is_error);
    assert!(text(&r).contains("ran out of time"), "{}", text(&r));
    tokio::time::timeout(Duration::from_secs(5), cancelled.notified())
        .await
        .expect("the host's cancellation fired");

    let ctx = context();
    let turn = ctx.cancel.clone();
    let tool = tool.with_limits(Limits::for_commands());
    let running = tokio::spawn(async move { run_in(&tool, ctx).await });
    tokio::time::sleep(Duration::from_millis(200)).await;
    turn.cancel();
    let r = tokio::time::timeout(Duration::from_secs(5), running)
        .await
        .expect("the run ended")
        .unwrap();
    assert!(
        text(&r).contains("the turn was interrupted"),
        "{}",
        text(&r)
    );
    tokio::time::timeout(Duration::from_secs(5), cancelled.notified())
        .await
        .expect("the host's cancellation fired");
}

#[tokio::test(flavor = "multi_thread")]
async fn input_and_result_are_held_to_the_call_limits() {
    let dir = tempfile::tempdir().unwrap();
    let tool = tool(
        dir.path(),
        "async function main() {
           const code = async (f) => { try { await f(); return 'ok'; } catch (e) { return e.code + ': ' + e.message; } };
           let deep = 0; for (let i = 0; i < 200; i++) deep = [deep];
           return [
             await code(() => codix.flow.start('y'.repeat(2000))),
             await code(() => codix.flow.big(2000)),
             await code(() => codix.flow.big(10)),
             await code(() => codix.flow.deep(129)),
             await code(() => codix.flow.deep(128)),
             await code(() => codix.flow.start(deep)),
           ];
         }",
        &["codix"],
        libs(),
    );
    let r = run_in(&tool, context()).await;
    let got: Vec<String> = serde_json::from_value(r.structured_content.clone().unwrap()).unwrap();
    assert!(
        got[0].starts_with("limit: the input is 2002 characters"),
        "{got:?}"
    );
    assert!(
        got[1].starts_with("limit: the result is 2002 characters"),
        "{got:?}"
    );
    assert_eq!(got[2], "ok");
    assert_eq!(got[3], "limit: the result nests deeper than 128 levels");
    assert_eq!(got[4], "ok");
    assert!(
        got[5].starts_with("limit: the input cannot be handed over"),
        "{got:?}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn the_prelude_runs_after_the_stubs_and_before_the_command_then_all_is_frozen() {
    let dir = tempfile::tempdir().unwrap();
    let lib = codix(Arc::default()).prelude(
        "codix.flow.startTwice = async (x) => [await codix.flow.start(x), await codix.flow.start(x)];",
    );
    let tool = tool(
        dir.path(),
        "async function main() {
           const twice = await codix.flow.startTwice(1);
           codix.flow.start = 1; codix.added = 2;
           const replaced = (() => { try { globalThis.codix = 1; } catch (e) {} return typeof codix; })();
           return [twice.map((r) => r.input), typeof codix.flow.start, codix.added === undefined,
                   Object.isFrozen(codix), Object.isFrozen(codix.flow), delete globalThis.codix,
                   replaced, typeof __hostLibraries, Object.keys(globalThis).includes('codix')];
         }",
        &["codix"],
        vec![lib],
    );
    let r = run_in(&tool, context()).await;
    assert_eq!(
        r.structured_content,
        Some(json!([
            [1, 1],
            "function",
            true,
            true,
            true,
            false,
            "object",
            "undefined",
            false
        ])),
        "{}",
        text(&r)
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_failing_prelude_fails_the_run_and_names_the_library() {
    let dir = tempfile::tempdir().unwrap();
    let lib = codix(Arc::default()).prelude("throw new Error('prelude broke');");
    let tool = tool(
        dir.path(),
        "function main() { return 1; }",
        &["codix"],
        vec![lib],
    );
    let r = run_in(&tool, context()).await;
    assert!(r.is_error);
    assert!(
        text(&r).contains("the prelude of host library `codix` failed: prelude broke"),
        "{}",
        text(&r)
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn only_a_library_the_package_uses_is_there() {
    let dir = tempfile::tempdir().unwrap();
    let other = HostJsLib::new("other").function("ping", |_, _| async { Ok(json!("pong")) });
    let probe = "async function main() { return [typeof codix, typeof other, \
                 typeof other === 'object' ? await other.ping() : null]; }";
    let none = tool(
        dir.path(),
        probe,
        &[],
        vec![codix(Arc::default()), other.clone()],
    );
    let r = run_in(&none, context()).await;
    assert_eq!(
        r.structured_content,
        Some(json!(["undefined", "undefined", null])),
        "{}",
        text(&r)
    );
    let one = tool(
        dir.path(),
        probe,
        &["other"],
        vec![codix(Arc::default()), other],
    );
    let r = run_in(&one, context()).await;
    assert_eq!(
        r.structured_content,
        Some(json!(["undefined", "object", "pong"])),
        "{}",
        text(&r)
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_program_has_no_host_library() {
    let mut ctx = context();
    ctx.tool_use_id = "toolu_code".into();
    let tool = RunCodeTool::new(&[], Limits::default(), Library::default());
    let r = tool
        .call(
            json!({"code": "return [typeof codix, typeof __hostLibraries];", "description": "probe"}),
            ctx,
            ProgressSender::noop("t"),
        )
        .await
        .unwrap();
    assert_eq!(
        r.structured_content,
        Some(json!(["undefined", "undefined"])),
        "{}",
        text(&r)
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_defect_under_a_renamed_engine_names_no_internal_global() {
    let dir = tempfile::tempdir().unwrap();
    for source in [
        "await null; null.x;\nfunction main() {}",
        "async function main() { await codix.flow.start(1); return undefinedThing; }",
        "async function main() { return codix.flow.start(() => 1n); }",
    ] {
        let tool =
            tool(dir.path(), source, &["codix"], libs()).with_library(Library::for_engine("foo"));
        let r = run_in(&tool, context()).await;
        assert!(r.is_error, "{source}");
        assert!(!text(&r).contains("agix"), "{source}: {}", text(&r));
    }
}
