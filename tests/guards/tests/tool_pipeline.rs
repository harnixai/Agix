//! One tool call, one pipeline.
//!
//! PIPE-1: a tool is run only by the runtime's dispatch, `call_tool`. syn
//! cannot tell `Tool::call` from any other `call`, but every `Tool::call`
//! takes a `ProgressSender`, so the places that make one are the places that
//! can run a tool.
//! PIPE-2: a call is put to a `PermissionPolicy` by the runtime's
//! `authorize`, or handed on by a delegate's relay; nothing else asks the
//! policy about a call it then runs itself. A `decide` is taken to be the
//! policy's when its one argument is a request: `&request`, or `req` passed on.
//! PIPE-3: a call's `ToolContext` is made in one place, `tool_context_for`,
//! which gives it every field; after that only `approval` is set, once the
//! call is let through. Every public field has a production reader outside
//! `base` and that one constructor, and every `Option` field is given a value
//! somewhere — so a field nothing reads, or one read but never given, cannot
//! sit on the context looking like something a host can inject.

use std::collections::BTreeSet;

use guards::allow::{self, Finding};
use guards::scan::{self, Callee};
use guards::{par_flat_map, ws, Hit, SourceFile, View};
use syn::visit::Visit;

/// `file::function` around `hit`, or `file` at its top level.
fn place(f: &SourceFile, hit: &Hit) -> String {
    let around = scan::fns(f, View::Prod)
        .into_iter()
        .filter(|func| func.hit.line <= hit.line && hit.line < func.hit.line + func.lines)
        .max_by_key(|func| func.hit.line);
    match around {
        Some(func) => format!("{}::{}", f.rel, func.qual),
        None => f.rel.clone(),
    }
}

const MAKES_A_PROGRESS_SENDER: &[(&str, &str)] = &[(
    "crates/runtime/src/agent/turn/tool_exec.rs::call_tool",
    "the dispatch every call takes",
)];

#[test]
fn only_the_dispatch_runs_a_tool() {
    let found = par_flat_map(ws().production(), |f| {
        scan::calls(f, View::Prod)
            .into_iter()
            .filter(|c| match &c.callee {
                Callee::Path(p) => {
                    let n = p.len();
                    n >= 2
                        && p[n - 2] == "ProgressSender"
                        && matches!(p[n - 1].as_str(), "noop" | "new" | "with_callback")
                }
                Callee::Method(_) => false,
            })
            .map(|c| Finding {
                key: place(f, &c.hit),
                hit: c.hit,
            })
            .collect()
    });
    allow::check(
        "tool_pipeline::MAKES_A_PROGRESS_SENDER",
        found,
        MAKES_A_PROGRESS_SENDER,
        "call the tool through the session's dispatch — `ToolContext::dispatch` from a tool, \
         `ToolContext::inline_commands` for a skill's command — so its hooks, permission, \
         middleware, transformers and telemetry see it",
    );
}

const PUTS_A_CALL_TO_THE_POLICY: &[(&str, &str)] = &[
    (
        "crates/runtime/src/agent/turn/tool_permission.rs::authorize",
        "the dispatch's one decision per call",
    ),
    (
        "crates/agents/src/agent_tool/permission.rs::DelegateRelay::decide",
        "a delegate's call, relayed to its parent's policy",
    ),
    (
        "crates/permissions/src/policy/host.rs::HostSays::of",
        "the engine's policy asking the host's, as one of its rules",
    ),
];

fn is_a_request(arg: &syn::Expr) -> bool {
    match arg {
        syn::Expr::Reference(_) => true,
        syn::Expr::Path(p) => p
            .path
            .get_ident()
            .is_some_and(|i| matches!(i.to_string().as_str(), "req" | "request" | "relayed")),
        _ => false,
    }
}

#[test]
fn only_the_dispatch_and_the_relay_ask_the_policy() {
    let mut scanned = 0;
    let found: Vec<Finding> = ws()
        .production()
        .flat_map(|f| {
            scanned += 1;
            scan::calls(f, View::Prod)
                .into_iter()
                .filter(|c| matches!(&c.callee, Callee::Method(m) if m == "decide"))
                .filter(|c| c.args.len() == 1 && is_a_request(&c.args[0]))
                .map(|c| Finding {
                    key: place(f, &c.hit),
                    hit: c.hit,
                })
                .collect::<Vec<_>>()
        })
        .collect();
    allow::non_vacuous("tool_pipeline::PUTS_A_CALL_TO_THE_POLICY", scanned, 100);
    allow::check(
        "tool_pipeline::PUTS_A_CALL_TO_THE_POLICY",
        found,
        PUTS_A_CALL_TO_THE_POLICY,
        "dispatch the call instead of judging it yourself; the dispatch decides it once, asks \
         whoever can answer, and records the decision",
    );
}

const TOOL_CONTEXT: &str = "crates/core/src/interface/tool.rs";
const CONSTRUCTOR: &str = "crates/runtime/src/agent/turn/tool_env.rs";

/// The public fields of `ToolContext`, with whether each is an `Option`.
fn tool_context_fields() -> Vec<(String, bool)> {
    let tool_context = scan::structs(ws().file(TOOL_CONTEXT), View::Prod)
        .into_iter()
        .find(|s| s.name == "ToolContext")
        .unwrap_or_else(|| {
            panic!("[tool_pipeline::PIPE-3] no `struct ToolContext` in {TOOL_CONTEXT}")
        });
    tool_context
        .fields
        .into_iter()
        .filter(|f| f.vis == "pub")
        .filter_map(|f| Some((f.name?, f.ty.starts_with("Option"))))
        .collect()
}

const MAKES_A_TOOL_CONTEXT: &[(&str, &str)] = &[
    (
        "crates/runtime/src/agent/turn/tool_env.rs::tool_context_for",
        "the one constructor every call's context comes from",
    ),
    (
        "crates/platform/src/providers.rs::tool_context_for_test",
        "the fixture other crates' tests build on: base's test context over this machine",
    ),
];

#[test]
fn a_calls_context_is_made_in_one_place() {
    const MAKERS: &[&str] = &["new", "for_test_with", "from_engine_ctx_with"];
    let found = par_flat_map(ws().production(), |f| {
        scan::calls(f, View::Prod)
            .into_iter()
            .filter(|c| match &c.callee {
                Callee::Path(p) => {
                    let n = p.len();
                    (n >= 2 && p[n - 2] == "ToolContext" && MAKERS.contains(&p[n - 1].as_str()))
                        || p.last().is_some_and(|s| s == "tool_context_for_test")
                }
                Callee::Method(_) => false,
            })
            .map(|c| Finding {
                key: place(f, &c.hit),
                hit: c.hit,
            })
            .collect()
    });
    allow::check(
        "tool_pipeline::MAKES_A_TOOL_CONTEXT",
        found,
        MAKES_A_TOOL_CONTEXT,
        "take the context the call runs with, or build one through `tool_context_for`; a field \
         added to `ToolContext` is then given in one place",
    );
}

const SETS_A_TOOL_CONTEXT_FIELD: &[(&str, &str)] = &[
    (
        "crates/runtime/src/agent/turn/tool_exec.rs::execute_tool_inner::approval",
        "what the policy let the call through on, known only once it has",
    ),
    (
        "crates/runtime/src/agent/turn/tool_exec.rs::execute_tool_with_telemetry::cancel",
        "a `ToolExecCtx` taking the signal the middleware settled on, before any context is made",
    ),
    (
        "crates/runtime/src/agent/turn/prompt.rs::build_prompt_context::cwd",
        "a prompt's context, not a call's",
    ),
];

/// Assignments `<receiver>.<field> = <value>` in production code: the field,
/// the receiver when it is a plain name, whether the value is literally
/// `None`, and the line.
#[derive(Default)]
struct Assignments {
    found: Vec<Assignment>,
}

struct Assignment {
    field: String,
    receiver: Option<String>,
    none: bool,
    line: usize,
}

impl<'ast> Visit<'ast> for Assignments {
    fn visit_expr_assign(&mut self, e: &'ast syn::ExprAssign) {
        if let syn::Expr::Field(field) = &*e.left {
            if let syn::Member::Named(name) = &field.member {
                let receiver = match &*field.base {
                    syn::Expr::Path(p) => p.path.get_ident().map(ToString::to_string),
                    _ => None,
                };
                self.found.push(Assignment {
                    field: name.to_string(),
                    receiver,
                    none: matches!(&*e.right, syn::Expr::Path(p) if p.path.is_ident("None")),
                    line: name.span().start().line,
                });
            }
        }
        syn::visit::visit_expr_assign(self, e);
    }
}

fn assignments(f: &SourceFile) -> Vec<Assignment> {
    let mut v = Assignments::default();
    v.visit_file(&f.tree(View::Prod));
    v.found
}

/// A context is named `ctx` or `tool_ctx` wherever one is written to; the
/// scan reads those names, since syn does not know a receiver's type.
#[test]
fn a_calls_context_is_given_whole_and_only_its_approval_comes_later() {
    let fields: BTreeSet<String> = tool_context_fields().into_iter().map(|(n, _)| n).collect();
    let found = par_flat_map(ws().production(), |f| {
        if f.rel == CONSTRUCTOR || f.rel.starts_with("crates/core/") {
            return Vec::new();
        }
        assignments(f)
            .into_iter()
            .filter(|a| fields.contains(&a.field))
            .filter(|a| matches!(a.receiver.as_deref(), Some("ctx" | "tool_ctx")))
            .map(|a| {
                let hit = Hit {
                    file: f.rel.clone(),
                    line: a.line,
                    snippet: f.line(a.line).trim().to_string(),
                };
                Finding {
                    key: format!("{}::{}", place(f, &hit), a.field),
                    hit,
                }
            })
            .collect()
    });
    allow::check(
        "tool_pipeline::SETS_A_TOOL_CONTEXT_FIELD",
        found,
        SETS_A_TOOL_CONTEXT_FIELD,
        "give the field in `tool_context_for`, where every call's context is made",
    );
}

#[test]
fn every_field_of_a_calls_context_is_read_and_given() {
    let fields = tool_context_fields();
    allow::non_vacuous("tool_pipeline::PIPE-3", fields.len(), 10);
    let read: BTreeSet<String> = par_flat_map(ws().production(), |f| {
        if f.rel == CONSTRUCTOR || f.rel.starts_with("crates/core/") {
            return Vec::new();
        }
        scan::field_accesses(f, View::Prod)
            .into_iter()
            .filter(|a| !a.write)
            .map(|a| a.name)
            .collect()
    })
    .into_iter()
    .collect();
    let given: BTreeSet<String> = par_flat_map(ws().production(), |f| {
        assignments(f)
            .into_iter()
            .filter(|a| !a.none)
            .map(|a| a.field)
            .collect()
    })
    .into_iter()
    .collect();
    let unread: Vec<&str> = fields
        .iter()
        .filter(|(name, _)| !read.contains(name))
        .map(|(name, _)| name.as_str())
        .collect();
    let never_given: Vec<&str> = fields
        .iter()
        .filter(|(name, optional)| *optional && !given.contains(name))
        .map(|(name, _)| name.as_str())
        .collect();
    assert!(
        unread.is_empty() && never_given.is_empty(),
        "[tool_pipeline::PIPE-3] `ToolContext` fields no production code reads: {unread:?}; \
         optional ones nothing ever gives a value: {never_given:?}. Wire them up or delete them."
    );
}
