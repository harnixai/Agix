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

use guards::allow::{self, Finding};
use guards::scan::{self, Callee};
use guards::{par_flat_map, ws, Hit, SourceFile, View};

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
