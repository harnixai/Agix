//! `HostConnections`, the registry of what host connections provide: who keeps
//! a contested tool name, when a connection drops out, how many may hook.

use super::*;
use crate::session_pool::parts::hosts::{HookEntry, HostConnections, MAX_HOOK_HOSTS};
use base::interface::host_call::HostToolDecl;
use hooks::{HookEvent, HostHookSource, OnError};

fn tool(name: &str) -> HostToolDecl {
    HostToolDecl::new(name, "d", serde_json::json!({"type": "object"}))
}

fn hook(index: usize, events: &[HookEvent]) -> HookEntry {
    HookEntry {
        index,
        events: events.to_vec(),
        tools: Vec::new(),
        timeout: std::time::Duration::from_secs(10),
        on_error: OnError::Allow,
    }
}

#[test]
fn the_first_connection_keeps_a_tool_name_and_the_second_is_told_of_the_clash() {
    let hosts = HostConnections::default();
    assert!(hosts
        .replace_tools(Arc::new(NullWatcher(1)), vec![tool("Note")])
        .unwrap()
        .is_empty());

    let clashed = hosts
        .replace_tools(Arc::new(NullWatcher(2)), vec![tool("Note"), tool("Other")])
        .unwrap();

    assert_eq!(clashed, ["Note"]);
    let mut owners: Vec<(String, u64)> = hosts
        .tools()
        .into_iter()
        .map(|t| (t.name, t.host.unwrap().0))
        .collect();
    owners.sort();
    assert_eq!(owners, [("Note".into(), 1), ("Other".into(), 2)]);
}

#[test]
fn a_connection_may_take_its_own_names_again() {
    let hosts = HostConnections::default();
    let me = Arc::new(NullWatcher(1));
    hosts.replace_tools(me.clone(), vec![tool("Note")]).unwrap();
    assert!(hosts
        .replace_tools(me, vec![tool("Note")])
        .unwrap()
        .is_empty());
}

#[test]
fn replacing_with_nothing_removes_the_provider() {
    let hosts = HostConnections::default();
    let me = Arc::new(NullWatcher(1));
    hosts.replace_tools(me.clone(), vec![tool("Note")]).unwrap();
    assert!(hosts
        .provider(base::interface::host_call::HostId(1))
        .is_some());

    hosts.replace_tools(me, Vec::new()).unwrap();

    assert!(hosts
        .provider(base::interface::host_call::HostId(1))
        .is_none());
    assert!(hosts.tools().is_empty());
}

#[test]
fn a_ninth_connection_with_hooks_is_refused_and_an_existing_one_may_replace_its_own() {
    let hosts = HostConnections::default();
    for id in 0..MAX_HOOK_HOSTS as u64 {
        hosts
            .replace_hooks(Arc::new(NullWatcher(id)), vec![hook(0, &[HookEvent::Stop])])
            .unwrap();
    }

    let refused = hosts.replace_hooks(
        Arc::new(NullWatcher(100)),
        vec![hook(0, &[HookEvent::Stop])],
    );
    assert!(refused.is_err());

    assert!(hosts
        .replace_hooks(Arc::new(NullWatcher(0)), vec![hook(0, &[HookEvent::Stop])])
        .is_ok());
    assert!(hosts
        .replace_hooks(Arc::new(NullWatcher(100)), Vec::new())
        .is_ok());
}

#[test]
fn hooks_are_found_by_event() {
    let hosts = HostConnections::default();
    hosts
        .replace_hooks(
            Arc::new(NullWatcher(1)),
            vec![
                hook(0, &[HookEvent::PreToolUse, HookEvent::PostToolUse]),
                hook(1, &[HookEvent::Stop]),
            ],
        )
        .unwrap();

    assert!(hosts.has_hooks_for(HookEvent::PreToolUse));
    assert!(!hosts.has_hooks_for(HookEvent::UserPromptSubmit));
    let pre: Vec<usize> = hosts
        .hooks_for(HookEvent::PreToolUse)
        .iter()
        .map(|h| h.index)
        .collect();
    let stop: Vec<usize> = hosts
        .hooks_for(HookEvent::Stop)
        .iter()
        .map(|h| h.index)
        .collect();
    assert_eq!((pre, stop), (vec![0], vec![1]));
    assert!(hosts.hooks_for(HookEvent::UserPromptSubmit).is_empty());
}

/// A connection that has closed.
struct Closed(u64);

impl SessionWatcher for Closed {
    fn id(&self) -> WatcherId {
        WatcherId(self.0)
    }

    fn deliver(&self, _frame: &SessionFrame) -> bool {
        false
    }

    fn is_open(&self) -> bool {
        false
    }
}

#[test]
fn a_closed_connection_registers_neither_tools_nor_hooks() {
    let hosts = HostConnections::default();

    assert!(hosts
        .replace_tools(Arc::new(Closed(1)), vec![tool("Note")])
        .is_err());
    assert!(hosts
        .replace_hooks(Arc::new(Closed(1)), vec![hook(0, &[HookEvent::Stop])])
        .is_err());

    assert!(hosts.tools().is_empty());
    assert!(!hosts.has_hooks_for(HookEvent::Stop));
    assert!(hosts
        .replace_tools(Arc::new(NullWatcher(2)), vec![tool("Note")])
        .unwrap()
        .is_empty());
}
