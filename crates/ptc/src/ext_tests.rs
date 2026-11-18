use super::*;
use async_trait::async_trait;
use base::command::ExtCommand;
use base::interface::tool_dispatch::{CallableTool, CommandScope, SubCallAnswer, ToolDispatch};
use base::tool::{ProgressSender, Tool, ToolResult, ToolResultContent};
use serde_json::{json, Value};
use std::path::Path;
use tokio_util::sync::CancellationToken;

/// Answers `Read` with a file's text and `Write` with "ok"; records the scope
/// a command asked for and every call made under it.
#[derive(Default)]
struct Fake {
    scopes: Mutex<Vec<CommandScope>>,
    calls: Arc<Mutex<Vec<(String, Value)>>>,
}

struct Scoped(Arc<Mutex<Vec<(String, Value)>>>);

#[async_trait]
impl ToolDispatch for Scoped {
    fn callable(&self) -> Vec<CallableTool> {
        vec![]
    }
    fn concurrency_safe(&self, name: &str, _: &Value) -> bool {
        name == "Read"
    }
    async fn call(
        &self,
        name: &str,
        input: Value,
        _: CancellationToken,
    ) -> Result<SubCallAnswer, String> {
        self.0
            .lock()
            .unwrap()
            .push((name.to_string(), input.clone()));
        match name {
            "Read" => Ok(SubCallAnswer {
                text: "raw".into(),
                structured: Some(json!({"content": "hello"})),
                reported_failure: false,
            }),
            "Write" => Ok(SubCallAnswer {
                text: "ok".into(),
                structured: None,
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
        Err("a command's calls go through its own dispatch".into())
    }
    fn for_command(&self, scope: CommandScope) -> Option<Arc<dyn ToolDispatch>> {
        self.scopes.lock().unwrap().push(scope);
        Some(Arc::new(Scoped(self.calls.clone())))
    }
}

fn command(root: &Path, source: &str) -> ExtCommand {
    std::fs::write(root.join("cmd.js"), source).unwrap();
    ExtCommand {
        name: "forge".into(),
        description: "requirements and specs".into(),
        plugin: "forge-plugin".into(),
        plugin_version: "1.2.3".into(),
        root: root.to_path_buf(),
        file: root.join("cmd.js"),
        function: "main".into(),
        read_only: vec!["help".into(), "spec list".into()],
        writes: vec!["forge/**".into()],
        bytecode_dir: Some(root.join(".bytecode")),
    }
}

async fn run(tool: &RunExtTool, input: Value) -> (ToolResult, Arc<Fake>) {
    let fake = Arc::new(Fake::default());
    let mut ctx =
        session_state::testing::tool_context_for_test(platform::local_providers(), "/proj".into());
    ctx.tool_use_id = "toolu_ext".into();
    ctx.dispatch = Some(fake.clone());
    let result = tool
        .call(input, ctx, ProgressSender::noop("t"))
        .await
        .unwrap();
    (result, fake)
}

fn text(r: &ToolResult) -> String {
    match &r.content {
        ToolResultContent::Text(s) => s.clone(),
        other => format!("{other:?}"),
    }
}

const ECHO: &str = r#"// @ts-check
async function main(argv, input, ctx) {
  const read = await tools.Read({ file_path: "forge/a.md" });
  log("read it");
  return { argv, input, ctx, read };
}"#;

#[tokio::test(flavor = "multi_thread")]
async fn main_gets_argv_input_and_ctx_and_its_value_comes_back_as_json() {
    let dir = tempfile::tempdir().unwrap();
    let tool = RunExtTool::new(vec![command(dir.path(), ECHO)]);
    let (r, fake) = run(
        &tool,
        json!({"command": "forge spec list --status 'in review'", "input": {"n": 1}}),
    )
    .await;
    assert!(!r.is_error, "{}", text(&r));
    let value = r.structured_content.clone().unwrap();
    assert_eq!(
        value["argv"],
        json!(["spec", "list", "--status", "in review"])
    );
    assert_eq!(value["input"], json!({"n": 1}));
    assert_eq!(value["ctx"]["command"], "forge");
    assert_eq!(value["ctx"]["projectRoot"], "/proj");
    assert_eq!(
        value["ctx"]["plugin"],
        json!({"name": "forge-plugin", "version": "1.2.3"})
    );
    assert_eq!(value["ctx"]["readOnly"], true);
    assert_eq!(value["read"], json!({"content": "hello"}));
    assert!(text(&r).starts_with('{'), "{}", text(&r));
    assert!(text(&r).contains("Log:\nread it"), "{}", text(&r));
    assert_eq!(
        fake.scopes.lock().unwrap().as_slice(),
        [CommandScope {
            command: "forge".into(),
            plugin: "forge-plugin".into(),
            read_only: true,
            writes: vec![],
        }]
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_string_comes_back_as_it_is_and_a_write_call_carries_its_scope() {
    let dir = tempfile::tempdir().unwrap();
    let tool = RunExtTool::new(vec![command(
        dir.path(),
        "const main = async (argv) => `did ${argv.join(' ')}`;",
    )]);
    let (r, fake) = run(&tool, json!({"command": "forge flow archive C-1"})).await;
    assert_eq!(text(&r), "did flow archive C-1");
    assert_eq!(r.structured_content, Some(json!("did flow archive C-1")));
    let scopes = fake.scopes.lock().unwrap();
    assert!(!scopes[0].read_only);
    assert_eq!(scopes[0].writes, vec!["forge/**".to_string()]);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_thrown_error_is_its_message_with_what_was_already_done() {
    let dir = tempfile::tempdir().unwrap();
    let tool = RunExtTool::new(vec![command(
        dir.path(),
        r#"async function main() {
             await tools.Write({ file_path: "forge/a.md", content: "x" });
             throw new Error("usage: forge flow archive <id>");
           }"#,
    )]);
    let (r, _) = run(&tool, json!({"command": "forge flow archive"})).await;
    assert!(r.is_error);
    let t = text(&r);
    assert!(t.starts_with("usage: forge flow archive <id>"), "{t}");
    assert!(t.contains("Already done before it failed"), "{t}");
    assert!(t.contains("Write(file_path: forge/a.md"), "{t}");
    assert!(
        !t.contains("at main"),
        "a usage error carries no stack: {t}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_defect_in_the_command_is_reported_with_its_kind_and_stack() {
    let dir = tempfile::tempdir().unwrap();
    let tool = RunExtTool::new(vec![command(
        dir.path(),
        "async function main() { return undefinedThing.x; }",
    )]);
    let (r, _) = run(&tool, json!({"command": "forge help"})).await;
    assert!(r.is_error);
    assert!(text(&r).starts_with("ReferenceError:"), "{}", text(&r));
    assert!(text(&r).contains("cmd.js"), "{}", text(&r));
}

#[tokio::test(flavor = "multi_thread")]
async fn a_command_has_no_agent_and_cannot_start_programs_or_commands() {
    let dir = tempfile::tempdir().unwrap();
    let tool = RunExtTool::new(vec![command(
        dir.path(),
        r#"async function main() {
             const out = { agent: typeof agent, notify: typeof notify, messages: typeof messages,
                           args: typeof args };
             for (const name of ["Agent", "RunCode", "RunExt"]) {
               try { await callTool(name, {}); out[name] = "ran"; }
               catch (e) { out[name] = e.message; }
             }
             return out;
           }"#,
    )]);
    let (r, fake) = run(&tool, json!({"command": "forge help"})).await;
    let v = r.structured_content.clone().unwrap();
    for name in ["agent", "notify", "messages", "args"] {
        assert_eq!(v[name], "undefined", "{v}");
    }
    for name in ["Agent", "RunCode", "RunExt"] {
        assert!(
            v[name]
                .as_str()
                .unwrap()
                .contains("cannot be called from a plugin command"),
            "{v}"
        );
    }
    assert!(fake.calls.lock().unwrap().is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn top_level_await_and_a_function_hung_on_global_this_both_work() {
    let dir = tempfile::tempdir().unwrap();
    let tool = RunExtTool::new(vec![command(
        dir.path(),
        "const greeting = await Promise.resolve('hi');\n\
         (function () { globalThis.main = () => greeting; })();",
    )]);
    let (r, _) = run(&tool, json!({"command": "forge help"})).await;
    assert_eq!(text(&r), "hi");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_missing_function_says_so() {
    let dir = tempfile::tempdir().unwrap();
    let tool = RunExtTool::new(vec![command(dir.path(), "function other() {}")]);
    let (r, _) = run(&tool, json!({"command": "forge help"})).await;
    assert!(r.is_error);
    assert_eq!(text(&r), "cmd.js defines no function `main`");
}

#[tokio::test(flavor = "multi_thread")]
async fn the_entry_is_kept_compiled_and_a_changed_entry_is_compiled_again() {
    let dir = tempfile::tempdir().unwrap();
    let tool = RunExtTool::new(vec![command(dir.path(), "const main = () => 'one';")]);
    let (r, _) = run(&tool, json!({"command": "forge help"})).await;
    assert_eq!(text(&r), "one");
    let kept: Vec<_> = std::fs::read_dir(dir.path().join(".bytecode"))
        .unwrap()
        .flatten()
        .collect();
    assert_eq!(kept.len(), 1, "{kept:?}");
    let (r, _) = run(&tool, json!({"command": "forge help"})).await;
    assert_eq!(text(&r), "one", "loaded from what was kept");

    std::fs::write(dir.path().join("cmd.js"), "const main = () => 'two';").unwrap();
    let (r, _) = run(&tool, json!({"command": "forge help"})).await;
    assert_eq!(text(&r), "two");

    for entry in std::fs::read_dir(dir.path().join(".bytecode"))
        .unwrap()
        .flatten()
    {
        std::fs::write(entry.path(), b"\x00not bytecode").unwrap();
    }
    let (r, _) = run(&tool, json!({"command": "forge help"})).await;
    assert_eq!(text(&r), "two", "an unreadable kept file is compiled again");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_long_running_command_is_stopped_at_its_limit() {
    let dir = tempfile::tempdir().unwrap();
    let tool = RunExtTool::new(vec![command(dir.path(), "function main() { for (;;) {} }")])
        .with_limits(Limits {
            time: std::time::Duration::from_millis(300),
            ..Limits::for_commands()
        });
    let (r, _) = run(&tool, json!({"command": "forge help"})).await;
    assert!(r.is_error);
    assert!(text(&r).contains("ran out of time"), "{}", text(&r));
}

#[tokio::test]
async fn reads_are_read_only_calls_and_the_rest_are_not() {
    let dir = tempfile::tempdir().unwrap();
    let tool = RunExtTool::new(vec![command(dir.path(), ECHO)]);
    let traits = |line: &str| tool.call_traits(&json!({ "command": line }));
    assert!(traits("forge spec list").read_only);
    assert!(traits("forge spec list --all").concurrency_safe);
    assert!(!traits("forge spec add").read_only);
    assert!(!traits("forge help | cat").read_only);
    assert!(!traits("nope help").read_only);
}

#[tokio::test]
async fn validation_refuses_shell_syntax_and_names_the_installed_commands() {
    let dir = tempfile::tempdir().unwrap();
    let tool = RunExtTool::new(vec![command(dir.path(), ECHO)]);
    let ctx =
        session_state::testing::tool_context_for_test(platform::local_providers(), "/proj".into());
    let refused = |line: &str| {
        let tool = &tool;
        let ctx = ctx.clone();
        let line = line.to_string();
        async move {
            match tool.validate_input(&json!({ "command": line }), &ctx).await {
                base::tool::ValidationResult::Err(m, _) => m,
                base::tool::ValidationResult::Ok => String::new(),
            }
        }
    };
    assert!(refused("forge help > out.txt")
        .await
        .contains("not a shell"));
    let unknown = refused("make all").await;
    assert!(unknown.contains("no command `make`"), "{unknown}");
    assert!(
        unknown.contains("`forge` (requirements and specs)"),
        "{unknown}"
    );
    assert_eq!(refused("forge spec list").await, "");
}

#[test]
fn the_command_line_is_split_by_shell_quoting_without_a_shell() {
    let ok = |line: &str| split_command_line(line).unwrap();
    assert_eq!(ok("forge spec list"), ["forge", "spec", "list"]);
    assert_eq!(
        ok("  forge   'a b'  \"c d\" e\\ f"),
        ["forge", "a b", "c d", "e f"]
    );
    assert_eq!(
        ok(r#"forge "say \"hi\"" 'it''s'"#),
        ["forge", r#"say "hi""#, "its"]
    );
    assert_eq!(
        ok("forge '$HOME | * ? [x]' costs 5 $ a=b \"a ? [b] $\""),
        [
            "forge",
            "$HOME | * ? [x]",
            "costs",
            "5",
            "$",
            "a=b",
            "a ? [b] $"
        ]
    );
    assert_eq!(ok("forge ''"), ["forge", ""]);
    for line in [
        "forge a | b",
        "forge a > f",
        "forge a < f",
        "forge a; rm x",
        "forge a && b",
        "forge a || b",
        "forge a &",
        "forge $(id)",
        "forge `id`",
        "forge $HOME",
        "forge ${HOME}",
        "forge *.md",
        "forge \"$(id)\"",
        "forge \"`id`\"",
        "forge \"$HOME\"",
        "forge a\nrm x",
        "forge costs $5",
        "forge $0",
        "forge $$",
        "forge $!",
        "forge $-",
        "forge x$'a'",
        "forge \"$5\"",
        "forge \"pid $$\"",
        "forge a?b",
        "forge [ab].md",
    ] {
        let e = split_command_line(line).unwrap_err();
        assert!(e.contains("not a shell"), "{line}: {e}");
    }
    assert!(split_command_line("forge 'open").is_err());
    assert!(split_command_line("forge \"open").is_err());
}

#[tokio::test]
async fn a_write_call_is_put_to_the_mode_with_its_scope_in_the_question() {
    let dir = tempfile::tempdir().unwrap();
    let tool = RunExtTool::new(vec![command(dir.path(), ECHO)]);
    let ctx =
        session_state::testing::tool_context_for_test(platform::local_providers(), "/proj".into());
    let facts = tool
        .permission_facts(&json!({"command": "forge  flow archive 'C 1'"}), &ctx)
        .await;
    assert_eq!(
        facts.subject,
        base::interface::permission::Subject::Opaque("forge flow archive 'C 1'".into())
    );
    assert_eq!(
        facts.reach,
        base::interface::permission::Reach::EditsProject
    );
    assert!(facts.question.unwrap().contains("may write forge/**"));
    let read = tool
        .permission_facts(&json!({"command": "forge spec list"}), &ctx)
        .await;
    assert_eq!(read.reach, base::interface::permission::Reach::Delegates);
}

#[test]
fn the_description_lists_each_installed_command_on_its_own_line() {
    let dir = tempfile::tempdir().unwrap();
    let tool = RunExtTool::new(vec![command(dir.path(), ECHO)]);
    assert!(tool.description().contains("This is not a shell"));
    assert!(tool
        .description()
        .ends_with("Installed:\n  forge — requirements and specs"));
    assert!(tool.spec().callable_from_code && tool.spec().needs_dispatch);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_command_has_the_agix_library() {
    let dir = tempfile::tempdir().unwrap();
    let tool = RunExtTool::new(vec![command(
        dir.path(),
        "function main() { return agix.yaml.parse('status: draft'); }",
    )]);
    let (r, _) = run(&tool, json!({"command": "forge help"})).await;
    assert!(!r.is_error, "{}", text(&r));
    assert_eq!(r.structured_content, Some(json!({"status": "draft"})));
}
