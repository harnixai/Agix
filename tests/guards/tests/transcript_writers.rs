//! Only a few places may rewrite the conversation held by `SessionManager`
//! (G11). `messages` is private to the session crate, so the rewrites are the
//! methods named in `REWRITERS`, and each of them is called from the files
//! listed here and nowhere else: the compaction step in the runtime (the trim
//! passes, the checkpoint restore and `run_compaction`), `/clear`, the host's
//! context edits, and the images a tool round attaches. Appending
//! (`push_message*`) is not a rewrite.

use guards::allow::{self, Finding};
use guards::scan::{self, Callee};
use guards::{par_flat_map, ws, View};

const REWRITERS: &[&str] = &[
    "replace_messages_after_compact",
    "trim_in_place",
    "restore_messages",
    "drop_message",
    "edit_message",
    "attach_to_tool_results",
];

/// The session pool has methods of these names that forward a host request to
/// the engine; they reach the conversation through the runtime's own call.
const POOL_NAMESAKES: &[&str] = &["drop_message", "edit_message"];

const COMPACT: &str = "crates/runtime/src/agent/turn/compact/mod.rs";

const ALLOWED: &[(&str, &str)] = &[
    (
        "replace_messages_after_compact @ crates/runtime/src/agent/turn/compact/mod.rs",
        "`run_compaction`, the one path that drops history and records a compact marker",
    ),
    (
        "replace_messages_after_compact @ crates/session/src/session.rs",
        "`clear`, which is a compaction to nothing",
    ),
    (
        "trim_in_place @ crates/runtime/src/agent/turn/compact/mod.rs",
        "`trim_stale`, `trim_tool_results` and `trim_early`: blank old tool results, drop nothing",
    ),
    (
        "restore_messages @ crates/runtime/src/agent/turn/compact/mod.rs",
        "`restore_transcript`, putting back what an abandoned compaction step changed",
    ),
    (
        "drop_message @ crates/runtime/src/agent/turn/prompt.rs",
        "the host's context edit",
    ),
    (
        "edit_message @ crates/runtime/src/agent/turn/prompt.rs",
        "the host's context edit",
    ),
    (
        "attach_to_tool_results @ crates/runtime/src/agent/turn/user_turn/step.rs",
        "the images a tool round returned, appended to the message that round pushed",
    ),
];

fn rewrite_calls() -> Vec<Finding> {
    par_flat_map(ws().production(), |f| {
        scan::calls(f, View::Prod)
            .into_iter()
            .filter_map(|c| {
                let name = match &c.callee {
                    Callee::Method(m) => m.as_str(),
                    Callee::Path(p) => p.last()?.as_str(),
                };
                let pool_forward = POOL_NAMESAKES.contains(&name) && f.package == "assembly";
                (REWRITERS.contains(&name) && !pool_forward).then(|| Finding {
                    key: format!("{name} @ {}", c.hit.file),
                    hit: c.hit,
                })
            })
            .collect()
    })
}

#[test]
fn the_conversation_is_rewritten_only_where_the_list_says() {
    allow::non_vacuous("transcript_writers", ws().production().count(), 100);
    assert!(
        ws().production().any(|f| f.rel == COMPACT),
        "{COMPACT} moved: update this guard"
    );
    allow::check(
        "transcript_writers::ALLOWED",
        rewrite_calls(),
        ALLOWED,
        "route the rewrite through the compaction step (`run_compaction` for anything that \
         drops history, a trim pass for blanking tool results); if this really is another \
         legitimate writer, list the file with the reason",
    );
}
