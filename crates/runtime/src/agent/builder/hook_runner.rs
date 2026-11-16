//! Hook runner construction from `Settings::hooks_config`.

use super::*;
use base::context::Session;

/// Build a `HookRunner` from `settings.hooks_config` (raw JSON from
/// settings.json's `hooks` section) when the caller didn't inject one
/// explicitly via `Builder::hooks(...)`.
///
/// A malformed value degrades to no hooks + a `tracing::warn!` rather than
/// failing the build — same soft-degrade philosophy as the rest of
/// settings.json. Also wires the `<project dir>/hooks/` three-tier search path
/// (project > scene > global) so `Command` hooks can reference a bare
/// script name — see `hooks::HookRunner::with_hooks_search_dirs` docs.
///
/// `model`/`delegator` back the `"type": "prompt"` / `"type": "agent"`
/// hook executors (`crate::hook_executors`).
pub(in crate::agent) fn build_hook_runner(
    settings: &LoadedSettings,
    model: Arc<dyn Model>,
    delegator: Arc<dyn base::interface::delegate::Delegator>,
    session_id: Option<String>,
    exec: hooks::HookExec,
) -> HookRunner {
    // Key by key, not whole-map. `HooksSettings` is keyed by `HookEvent`,
    // which has no serde fallback, so one unrecognized event name would
    // otherwise fail the entire map and silently turn off every hook.
    let parsed: hooks::HooksSettings = match &settings.values.hooks_config {
        Some(v) => {
            let (parsed, report) = hooks::parse_hooks_settings(v);
            for name in &report.unknown_events {
                tracing::warn!(
                    event = %name,
                    "settings.json `hooks` names an event this engine does not have; \
                     that entry is ignored, the rest are unaffected"
                );
            }
            for (event, err) in &report.invalid_configs {
                tracing::warn!(
                    event = %event,
                    error = %err,
                    "settings.json `hooks` entry failed to parse; that entry is ignored, \
                     the rest are unaffected"
                );
            }
            parsed
        }
        None => Default::default(),
    };
    // A hook configured for an event nothing fires is indistinguishable, from
    // the outside, from a hook that runs and does nothing — the author waits
    // for it forever. The engine knows which events those are, so it says so
    // here rather than leaving the answer in a doc comment on the enum that
    // whoever wrote the config is not reading.
    for (event, configs) in &parsed {
        if !event.is_wired() {
            tracing::warn!(
                event = ?event,
                hooks = configs.len(),
                "hook configured for an event this engine never fires — it will not run; \
                 see `hooks::UNWIRED_EVENTS` for why"
            );
        }
    }
    // An untrusted project's hooks config is already gone from `settings`;
    // its `hooks/` directory goes too, or a user hook naming a script would
    // run the project's file of that name.
    let project_untrusted = settings.trust().is_untrusted();
    let dirs = settings.paths.config_paths();
    let project_hooks = dirs.project_hooks_dir().filter(|_| !project_untrusted);
    let hooks_search_dirs: Vec<_> = project_hooks
        .into_iter()
        .chain([dirs.user_hooks_dir(), dirs.global_hooks_dir()])
        .collect();
    let prompt_executor = Arc::new(crate::hook_executors::ModelPromptHookExecutor::new(
        model,
        settings.values.model.model_name.clone(),
        settings.values.model.max_tokens,
        session_id,
    ));
    HookRunner::new(parsed, exec)
        .with_hooks_search_dirs(hooks_search_dirs)
        .with_project_untrusted(project_untrusted)
        .with_prompt_executor(prompt_executor)
        .with_agent_executor(agent_hook_executor(settings, delegator))
}

fn agent_hook_executor(
    settings: &LoadedSettings,
    delegator: Arc<dyn base::interface::delegate::Delegator>,
) -> Arc<crate::hook_executors::DelegatorHookExecutor> {
    let cwd = settings.paths.working_dir().to_path_buf();
    let project = settings.paths.project_root().map(Into::into);
    Arc::new(
        crate::hook_executors::DelegatorHookExecutor::new(delegator, cwd)
            .in_worktrees_of(project, settings.paths.layout.clone()),
    )
}

pub(in crate::agent) fn hook_run_recorder(
    telemetry: TelemetryHandle,
) -> Arc<dyn Fn(hooks::HookRunReport) + Send + Sync> {
    Arc::new(move |run| {
        let _ = telemetry.record(telemetry::TelemetryEvent::hook_run(
            &run.session_id,
            0,
            run.turn_id,
            telemetry::HookRunPayload {
                event: format!("{:?}", run.event),
                hook_count: run.hooks,
                duration_ms: run.duration.as_millis() as u64,
                blocked: run.blocked,
                errors: run.errors,
                tool_use_id: run.tool_use_id,
            },
        ));
    })
}

/// The session's id, cwd, depth and permission mode, read each time a
/// payload is built. Holds the state weakly: the closure lives in tasks and
/// watchers that must not keep the session alive.
pub(in crate::agent) fn hook_ambient(
    session_id: String,
    cwd: String,
    depth: u32,
    state: Arc<session_state::SessionState>,
) -> hooks::payload::Ambient {
    let state = Arc::downgrade(&state);
    Arc::new(move || {
        let mode = state
            .upgrade()
            .map(|s| crate::turn::hook_permission_mode(s.permission_mode()))
            .unwrap_or_else(|| "default".into());
        hooks::HookInput::lifecycle(String::new(), session_id.clone(), cwd.clone(), mode)
            .with_depth(depth)
    })
}
