//! 宿主连接级的工具与 hook：登记（逐项收下或拒绝）、回答只认提供它的连接、
//! 宿主工具调用的进度。连接断开时的清扫在 `drop_watcher`。

use crate::session_pool::api::{
    HookAnswer, HooksSet, HostHookOnError, HostHookOption, HostHookSet, HostToolOption,
    HostToolsSet, RefusedEntry,
};
use crate::session_pool::parts::hosts::{HookEntry, MAX_HOOKS_PER_HOST, MAX_TOOLS_PER_HOST};
use crate::session_pool::*;
use base::interface::host_call::{HostCallEnd, HostId, HostReply};
use serde_json::Value;

fn refused(
    index: usize,
    name: Option<String>,
    field: Option<&str>,
    error: impl Into<String>,
) -> RefusedEntry {
    RefusedEntry {
        index,
        name,
        field: field.map(str::to_string),
        error: error.into(),
    }
}

fn name_of(entry: &Value) -> Option<String> {
    entry
        .get("name")
        .and_then(Value::as_str)
        .map(str::to_string)
}

impl SessionPool {
    /// `daemon.setHostTools` — what `watcher`'s connection provides every
    /// session whose engine is built from now on, in place of what it gave
    /// before. Each entry is taken or refused on its own.
    pub fn set_host_tools(
        &self,
        watcher: Arc<dyn SessionWatcher>,
        entries: Vec<Value>,
    ) -> HostToolsSet {
        let (mut refusals, taken) = self.admissible_tools(&entries);
        let indices: HashMap<String, usize> =
            taken.iter().map(|(i, t)| (t.name.clone(), *i)).collect();
        let clashes = match self
            .factory
            .hosts()
            .replace_tools(watcher, taken.iter().map(|(_, t)| t.clone()).collect())
        {
            Ok(clashes) => clashes,
            Err(closed) => {
                refusals.extend(
                    taken
                        .into_iter()
                        .map(|(i, t)| refused(i, Some(t.name), None, closed)),
                );
                refusals.sort_by_key(|r| r.index);
                return HostToolsSet {
                    host_tools: Vec::new(),
                    refused: refusals,
                };
            }
        };
        for name in &clashes {
            refusals.push(refused(
                indices[name],
                Some(name.clone()),
                Some("name"),
                "another connection provides a tool with this name",
            ));
        }
        refusals.sort_by_key(|r| r.index);
        let mut host_tools: Vec<(usize, String)> = indices
            .into_iter()
            .filter(|(name, _)| !clashes.contains(name))
            .map(|(name, i)| (i, name))
            .collect();
        host_tools.sort();
        HostToolsSet {
            host_tools: host_tools.into_iter().map(|(_, name)| name).collect(),
            refused: refusals,
        }
    }

    /// The entries of `daemon.setHostTools` that may be provided, by index,
    /// and why the others may not; whose names other connections hold is
    /// the registry's to say.
    fn admissible_tools(
        &self,
        entries: &[Value],
    ) -> (
        Vec<RefusedEntry>,
        Vec<(usize, base::interface::host_call::HostToolDecl)>,
    ) {
        let mut refusals = Vec::new();
        let mut taken: Vec<(usize, base::interface::host_call::HostToolDecl)> = Vec::new();
        for (index, entry) in entries.iter().enumerate() {
            let tool = match serde_json::from_value::<HostToolOption>(entry.clone()) {
                Ok(tool) => tool,
                Err(e) => {
                    refusals.push(refused(index, name_of(entry), None, e.to_string()));
                    continue;
                }
            };
            let decl = tool.declaration();
            let name = Some(tool.name.clone());
            if let Some((field, error)) = runtime::host_tools::declaration_problems(&decl)
                .into_iter()
                .next()
            {
                refusals.push(refused(index, name, Some(field), error));
            } else if self.factory.reserves_tool_name(&tool.name) {
                refusals.push(refused(
                    index,
                    name,
                    Some("name"),
                    "a built-in tool has this name",
                ));
            } else if taken.iter().any(|(_, t)| t.name == tool.name) {
                refusals.push(refused(index, name, Some("name"), "declared twice"));
            } else if taken.len() == MAX_TOOLS_PER_HOST {
                refusals.push(refused(
                    index,
                    name,
                    None,
                    format!("at most {MAX_TOOLS_PER_HOST} tools per connection"),
                ));
            } else {
                taken.push((index, decl));
            }
        }
        (refusals, taken)
    }

    /// `daemon.setHooks` — the hooks `watcher`'s connection answers, in
    /// place of what it registered before, for every session including
    /// those already built, from the moment this returns. Each entry is
    /// taken or refused on its own; the call as a whole is refused when the
    /// daemon has as many hook connections as it takes.
    pub fn set_hooks(
        &self,
        watcher: Arc<dyn SessionWatcher>,
        entries: Vec<Value>,
    ) -> Result<HooksSet, PoolError> {
        let mut refusals = Vec::new();
        let mut taken = Vec::new();
        for (index, entry) in entries.iter().enumerate() {
            let hook = match serde_json::from_value::<HostHookOption>(entry.clone()) {
                Ok(hook) => hook,
                Err(e) => {
                    refusals.push(refused(index, None, None, e.to_string()));
                    continue;
                }
            };
            match hook_entry(index, &hook) {
                Err((field, error)) => refusals.push(refused(index, None, Some(field), error)),
                Ok(_) if taken.len() == MAX_HOOKS_PER_HOST => refusals.push(refused(
                    index,
                    None,
                    None,
                    format!("at most {MAX_HOOKS_PER_HOST} hooks per connection"),
                )),
                Ok(entry) => taken.push(entry),
            }
        }
        let accepted = taken
            .iter()
            .map(|h: &HookEntry| HostHookSet {
                index: h.index,
                events: h.events.iter().map(|e| e.name()).collect(),
            })
            .collect();
        self.factory
            .hosts()
            .replace_hooks(watcher, taken)
            .map_err(PoolError::InvalidRequest)?;
        Ok(HooksSet {
            hooks: accepted,
            refused: refusals,
        })
    }

    /// `session.respondHook` — `from`'s answer to the hook call `call_id`.
    /// Only the connection the call went to may answer it; a `call_id`
    /// nobody waits on is a silent no-op.
    pub async fn respond_hook(
        &self,
        from: Option<WatcherId>,
        session_id: &str,
        call_id: String,
        answer: HookAnswer,
    ) -> Result<(), PoolError> {
        let input_tx = self
            .answerable(session_id, &call_id, AskKind::HookCall, from)
            .await?;
        let _ = input_tx.send(InputMessage::HostToolResponse {
            call_id,
            end: HostCallEnd::Replied(HostReply::Hook(answer.response())),
        });
        Ok(())
    }

    /// `session.reportToolProgress` — progress on the open host tool call
    /// `call_id`, reported as the call's `tool_progress`. One that has ended
    /// or was never made is a silent no-op.
    pub async fn report_tool_progress(
        &self,
        from: Option<WatcherId>,
        session_id: &str,
        call_id: String,
        update: base::tool::ProgressUpdate,
    ) -> Result<(), PoolError> {
        let input_tx = self
            .answerable(session_id, &call_id, AskKind::HostToolCall, from)
            .await?;
        let _ = input_tx.send(InputMessage::HostToolProgress { call_id, update });
        Ok(())
    }

    /// The input of live `session_id`, when `from` may answer `call_id` as
    /// an ask of `kind`: one of another kind is refused, and so is one a
    /// host connection provides when `from` is not that connection.
    pub(in crate::session_pool) async fn answerable(
        &self,
        session_id: &str,
        call_id: &str,
        kind: AskKind,
        from: Option<WatcherId>,
    ) -> Result<InputSender, PoolError> {
        let (input_tx, hub) = self
            .live
            .with_live(session_id, |live| {
                (live.engine.input_tx.clone(), live.core.hub().clone())
            })
            .await
            .ok_or_else(|| PoolError::session_not_found(session_id))?;
        match hub.ask_host(call_id) {
            Some((asked, _)) if asked != kind => Err(PoolError::InvalidRequest(format!(
                "`{call_id}` is a {} ask, not a {}",
                asked.as_str(),
                kind.as_str()
            ))),
            Some((_, Some(host))) if from.map(|w| HostId(w.0)) != Some(host) => {
                Err(PoolError::InvalidRequest(format!(
                    "`{call_id}` is answered by the connection that provides it"
                )))
            }
            _ => Ok(input_tx),
        }
    }
}

fn hook_entry(index: usize, hook: &HostHookOption) -> Result<HookEntry, (&'static str, String)> {
    if hook.events.is_empty() {
        return Err(("events", "names no event".into()));
    }
    let mut events = Vec::new();
    for name in &hook.events {
        let event = hooks::HOSTED_EVENTS
            .iter()
            .find(|e| e.name() == *name)
            .ok_or_else(|| {
                (
                    "events",
                    format!(
                        "`{name}` is not an event a host connection may hook; those are {}",
                        hooks::HOSTED_EVENTS
                            .iter()
                            .map(|e| e.name())
                            .collect::<Vec<_>>()
                            .join(", ")
                    ),
                )
            })?;
        if !events.contains(event) {
            events.push(*event);
        }
    }
    let tools = hook.tools.clone().unwrap_or_default();
    if tools.len() > HostHookOption::MAX_TOOLS {
        return Err((
            "tools",
            format!("at most {} patterns", HostHookOption::MAX_TOOLS),
        ));
    }
    if tools.iter().any(|p| p.trim().is_empty()) {
        return Err(("tools", "a pattern is empty".into()));
    }
    if !(HostHookOption::MIN_TIMEOUT_MS..=HostHookOption::MAX_TIMEOUT_MS).contains(&hook.timeout_ms)
    {
        return Err((
            "timeout_ms",
            format!(
                "must be {}..={}",
                HostHookOption::MIN_TIMEOUT_MS,
                HostHookOption::MAX_TIMEOUT_MS
            ),
        ));
    }
    Ok(HookEntry {
        index,
        events,
        tools: if tools.iter().any(|p| p == "*") {
            Vec::new()
        } else {
            tools
        },
        timeout: std::time::Duration::from_millis(hook.timeout_ms),
        on_error: match hook.on_error {
            HostHookOnError::Allow => hooks::OnError::Allow,
            HostHookOnError::Block => hooks::OnError::Block,
        },
    })
}
