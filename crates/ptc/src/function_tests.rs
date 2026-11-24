use super::*;
use async_trait::async_trait;
use base::command::PluginFunction;
use base::interface::permission::{Reach, Subject};
use base::interface::tool_dispatch::{CallableTool, CommandScope, SubCallAnswer, ToolDispatch};
use base::tool::{ProgressSender, Tool, ToolContext, ToolResult, ValidationResult};
use serde_json::{json, Value};
use std::path::Path;
use std::time::Duration;
use tokio_util::sync::CancellationToken;

#[derive(Default)]
struct Fake {
    scopes: Mutex<Vec<CommandScope>>,
}

struct Scoped;

#[async_trait]
impl ToolDispatch for Scoped {
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
        match name {
            "Read" => Ok(SubCallAnswer {
                text: "raw".into(),
                structured: Some(json!({"content": "hello"})),
                reported_failure: false,
            }),
            other => Err(format!("{other} is not here")),
        }
    }
}

#[async_trait]
impl ToolDispatch for Fake {
    fn callable(&self) -> Vec<CallableTool> {
        vec![]
    }
    fn concurrency_safe(&self, _: &str, _: &Value) -> bool {
        true
    }
    async fn call(&self, _: &str, _: Value, _: CancellationToken) -> Result<SubCallAnswer, String> {
        Err("a function's calls go through its own dispatch".into())
    }
    fn for_command(&self, scope: CommandScope) -> Option<Arc<dyn ToolDispatch>> {
        self.scopes.lock().unwrap().push(scope);
        Some(Arc::new(Scoped))
    }
}

fn function(root: &Path, source: &str, schema: Option<Value>) -> PluginFunction {
    std::fs::write(root.join("xflow.js"), source).unwrap();
    PluginFunction {
        name: "check".into(),
        plugin: "xflow".into(),
        plugin_version: "1.0.0".into(),
        source: Default::default(),
        description: "gate check".into(),
        root: root.to_path_buf(),
        file: root.join("xflow.js"),
        function: "check".into(),
        writes: vec!["reports/**".into()],
        input_schema: schema,
        uses: Vec::new(),
        bytecode_dir: Some(root.join(".bytecode")),
    }
}

fn context(fake: &Arc<Fake>) -> ToolContext {
    let mut ctx =
        session_state::testing::tool_context_for_test(platform::local_providers(), "/proj".into());
    ctx.tool_use_id = "host-1".into();
    ctx.dispatch = Some(fake.clone());
    ctx
}

async fn call(tool: &PluginCallTool, input: Value, ctx: ToolContext) -> ToolResult {
    tool.call(
        json!({"function": "xflow.check", "input": input}),
        ctx,
        ProgressSender::noop("t"),
    )
    .await
    .unwrap()
}

const ECHO: &str = r#"
async function check(input, ctx) {
  const read = await tools.Read({ file_path: "a.md" });
  log("checked");
  return { input, ctx, read, frozen: Object.isFrozen(ctx) };
}"#;

#[tokio::test(flavor = "multi_thread")]
async fn the_function_gets_input_and_ctx_and_its_value_comes_back_structured() {
    let dir = tempfile::tempdir().unwrap();
    let tool = PluginCallTool::new(function(dir.path(), ECHO, None), false)
        .unwrap()
        .in_session("s-1");
    let fake = Arc::new(Fake::default());
    let r = call(&tool, json!({"change": "c1"}), context(&fake)).await;
    assert!(!r.is_error, "{r:?}");
    let out = r.structured_content.unwrap();
    assert_eq!(out["ok"], true, "{out}");
    assert_eq!(out["logs"], json!(["checked"]), "{out}");
    let value = &out["value"];
    assert_eq!(value["input"], json!({"change": "c1"}));
    assert_eq!(value["read"], json!({"content": "hello"}));
    assert_eq!(value["frozen"], true);
    assert_eq!(
        value["ctx"],
        json!({
            "via": "host", "projectRoot": "/proj", "cwd": "/proj",
            "plugin": {"name": "xflow", "version": "1.0.0"},
            "function": "xflow.check", "session": "s-1", "readOnly": true,
        })
    );
    assert_eq!(
        *fake.scopes.lock().unwrap(),
        vec![CommandScope {
            command: "xflow.check".into(),
            line: "xflow.check".into(),
            plugin: "xflow".into(),
            read_only: true,
            writes: Vec::new(),
        }],
        "a call the host does not let write is read-only"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_writing_call_is_scoped_to_the_functions_writes_and_asked_about_as_an_edit() {
    let dir = tempfile::tempdir().unwrap();
    let tool = PluginCallTool::new(function(dir.path(), ECHO, None), true).unwrap();
    let fake = Arc::new(Fake::default());
    let ctx = context(&fake);
    let facts = tool.permission_facts(&json!({}), &ctx).await;
    assert_eq!(facts.reach, Reach::EditsProject);
    assert_eq!(facts.subject, Subject::Opaque("xflow.check".into()));
    let r = call(&tool, Value::Null, ctx).await;
    assert_eq!(
        r.structured_content.unwrap()["value"]["ctx"]["session"],
        Value::Null
    );
    let scopes = fake.scopes.lock().unwrap();
    assert!(!scopes[0].read_only);
    assert_eq!(scopes[0].writes, vec!["reports/**".to_string()]);
}

#[tokio::test(flavor = "multi_thread")]
async fn input_outside_the_schema_is_refused_unrun() {
    let dir = tempfile::tempdir().unwrap();
    let schema = json!({"type": "object", "required": ["change"]});
    let tool = PluginCallTool::new(function(dir.path(), ECHO, Some(schema)), false).unwrap();
    let fake = Arc::new(Fake::default());
    let r = call(&tool, json!({"other": 1}), context(&fake)).await;
    assert!(r.is_error);
    let out = r.structured_content.unwrap();
    assert_eq!(out["error"]["code"], "invalid_input", "{out}");
    assert!(
        out["error"]["message"].as_str().unwrap().contains("change"),
        "{out}"
    );
    assert!(fake.scopes.lock().unwrap().is_empty(), "nothing ran");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_throw_comes_back_as_its_message_with_the_log() {
    let dir = tempfile::tempdir().unwrap();
    let source = "async function check() { log('looked'); throw new Error('gate failed'); }";
    let tool = PluginCallTool::new(function(dir.path(), source, None), false).unwrap();
    let r = call(&tool, Value::Null, context(&Arc::new(Fake::default()))).await;
    let out = r.structured_content.unwrap();
    assert_eq!(
        out,
        json!({"ok": false, "error": {"code": "threw", "message": "gate failed"}, "logs": ["looked"]})
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn the_time_given_is_the_limit_and_no_more_than_a_commands_can_be_given() {
    let dir = tempfile::tempdir().unwrap();
    let f = function(
        dir.path(),
        "async function check() { while (true) {} }",
        None,
    );
    assert!(PluginCallTool::new(f.clone(), false)
        .unwrap()
        .with_time(PluginCallTool::time_limit() + Duration::from_millis(1))
        .is_err());
    let tool = PluginCallTool::new(f, false)
        .unwrap()
        .with_time(Duration::from_millis(300))
        .unwrap();
    let r = call(&tool, Value::Null, context(&Arc::new(Fake::default()))).await;
    assert_eq!(r.structured_content.unwrap()["error"]["code"], "timeout");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_cancelled_call_is_stopped_and_says_so() {
    let dir = tempfile::tempdir().unwrap();
    let f = function(
        dir.path(),
        "async function check() { while (true) {} }",
        None,
    );
    let tool = PluginCallTool::new(f, false).unwrap();
    let ctx = context(&Arc::new(Fake::default()));
    let cancel = ctx.cancel.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(200)).await;
        cancel.cancel();
    });
    let r = call(&tool, Value::Null, ctx).await;
    assert_eq!(r.structured_content.unwrap()["error"]["code"], "cancelled");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_call_of_another_function_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let tool = PluginCallTool::new(function(dir.path(), ECHO, None), false).unwrap();
    let ctx = context(&Arc::new(Fake::default()));
    let checked = tool
        .validate_input(&json!({"function": "xflow.other"}), &ctx)
        .await;
    assert!(matches!(checked, ValidationResult::Err(..)));
}
