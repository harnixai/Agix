//! `resolve_session_permission_tests` 原样搬来的用例。

use super::*;
use base::recorder::Divergence;

fn defaults() -> settings::LoadedSettings {
    settings::LoadedSettings::new(
        Settings::defaults_for("claude-sonnet-4-6"),
        base::paths::PathSettings::default(),
    )
}

#[tokio::test]
async fn input_validation_still_refuses_under_bypass() {
    let ctx = session_state::testing::tool_context_for_test(
        base::interface::exec::ExecProviders::in_process(),
        std::env::temp_dir(),
    );
    let tools = Arc::new(base::tool::InMemoryToolRegistry::new());
    tools::register_builtin_tools(&tools);
    // Input validation is the runtime's, once, before the policy is asked;
    // under bypass it still refuses what the tool rejects.
    assert!(tools
        .get("Bash")
        .unwrap()
        .validate_input(&serde_json::json!({"command": "  "}), &ctx)
        .await
        .is_err());
}

#[test]
fn a_session_clock_is_the_hosts_start_else_the_recordings_else_the_machines() {
    let date = |env: Option<Arc<dyn base::interface::environment::Environment>>| {
        env.map(|e| e.now().date().to_string())
    };
    let asked = SessionOptions {
        clock: Some(crate::session_pool::api::ClockOptions {
            start: "2026-10-28T09:00:00Z".into(),
        }),
        ..Default::default()
    };
    assert_eq!(
        date(session_clock(Some(&asked), None, "s")),
        Some("2026-10-28".into())
    );

    let root = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(root.path().join("rec")).unwrap();
    std::fs::write(
        root.path().join("rec/calls.jsonl"),
        "{\"type\":\"recording\",\"version\":1,\"name\":\"rec\",\"session_id\":\"s0\",\
             \"created_at\":1761955200000,\"engine_version\":\"x\"}\n",
    )
    .unwrap();
    let recorder = |mode, name: Option<&str>| RecorderConfig {
        mode,
        name: name.map(str::to_string),
        root: root.path().to_path_buf(),
        on_divergence: Divergence::Warn,
    };
    let replay = recorder(RecorderMode::Replay, Some("rec"));
    assert_eq!(
        date(session_clock(None, Some(&replay), "s")),
        Some("2025-11-01".into())
    );
    assert_eq!(
        date(session_clock(Some(&asked), Some(&replay), "s")),
        Some("2026-10-28".into()),
        "what the host asked for wins"
    );
    assert!(session_clock(
        None,
        Some(&recorder(RecorderMode::Record, Some("rec"))),
        "s"
    )
    .is_none());
    assert!(
        session_clock(None, Some(&recorder(RecorderMode::Replay, None)), "no-such").is_none(),
        "no recording, no shift"
    );
    let garbled = SessionOptions {
        clock: Some(crate::session_pool::api::ClockOptions {
            start: "yesterday".into(),
        }),
        ..Default::default()
    };
    assert!(session_clock(Some(&garbled), None, "s").is_none());
}

/// A client asking for a *more permissive* mode than the daemon is
/// configured with gets clamped, not obeyed.
#[test]
fn session_options_cannot_loosen_an_asking_settings_default() {
    let opts = SessionOptions {
        permission_mode: Some(base::permission::PermissionMode::BypassPermissions),
        ..Default::default()
    };
    assert_eq!(
        effective_permission_mode(&defaults(), Some(&opts)),
        base::permission::PermissionMode::Default,
        "the request should clamp down to the configured mode"
    );
}

/// ...unless the operator explicitly says clients are as trusted as the
/// daemon itself.
#[test]
fn allow_client_permission_override_restores_client_wins() {
    let mut settings = defaults();
    settings.values.allow_client_permission_override = true;
    let opts = SessionOptions {
        permission_mode: Some(base::permission::PermissionMode::BypassPermissions),
        ..Default::default()
    };
    assert_eq!(
        effective_permission_mode(&settings, Some(&opts)),
        base::permission::PermissionMode::BypassPermissions
    );
}

/// Tightening is always allowed, in both directions of the comparison:
/// a `bypassPermissions` daemon still honors a client that wants to be
/// asked.
#[test]
fn a_client_may_always_tighten() {
    let mut settings = defaults();
    settings.values.permission_mode = base::permission::PermissionMode::BypassPermissions;
    let opts = SessionOptions {
        permission_mode: Some(base::permission::PermissionMode::Plan),
        ..Default::default()
    };
    assert_eq!(
        effective_permission_mode(&settings, Some(&opts)),
        base::permission::PermissionMode::Plan
    );
}

#[test]
fn auto_and_yolo_in_session_options_clamp_and_resolve_as_bypass() {
    use base::permission::PermissionMode;
    for spelling in ["auto", "yolo"] {
        let opts: SessionOptions =
            serde_json::from_value(serde_json::json!({ "permission_mode": spelling })).unwrap();
        assert_eq!(
            opts.permission_mode,
            Some(PermissionMode::BypassPermissions)
        );

        assert_eq!(
            effective_permission_mode(&defaults(), Some(&opts)),
            PermissionMode::Default,
            "{spelling} must clamp like bypassPermissions"
        );
        let mut accepts_edits = defaults();
        accepts_edits.values.permission_mode = PermissionMode::AcceptEdits;
        assert_eq!(
            effective_permission_mode(&accepts_edits, Some(&opts)),
            PermissionMode::AcceptEdits
        );

        let mut bypass = defaults();
        bypass.values.permission_mode = PermissionMode::BypassPermissions;
        assert_eq!(
            effective_permission_mode(&bypass, Some(&opts)),
            PermissionMode::BypassPermissions,
            "{spelling}"
        );

        let mut overridable = defaults();
        overridable.values.allow_client_permission_override = true;
        assert_eq!(
            effective_permission_mode(&overridable, Some(&opts)),
            PermissionMode::BypassPermissions,
            "{spelling}"
        );
    }
}

#[test]
fn settings_ask_default_is_overridable_per_session_without_becoming_allow_all() {
    let opts = SessionOptions {
        permission_mode: Some(base::permission::PermissionMode::DontAsk),
        ..Default::default()
    };
    assert_eq!(
        effective_permission_mode(&defaults(), Some(&opts)),
        base::permission::PermissionMode::DontAsk
    );
}

#[test]
fn effective_mode_prefers_session_options_then_settings_then_ask() {
    use base::permission::PermissionMode;
    let mut settings = defaults();
    assert_eq!(
        effective_permission_mode(&settings, None),
        PermissionMode::Default,
        "an unconfigured daemon asks"
    );

    settings.values.permission_mode = PermissionMode::BypassPermissions;
    assert_eq!(
        effective_permission_mode(&settings, None),
        PermissionMode::BypassPermissions
    );

    let opts = SessionOptions {
        permission_mode: Some(PermissionMode::Plan),
        ..Default::default()
    };
    assert_eq!(
        effective_permission_mode(&settings, Some(&opts)),
        PermissionMode::Plan,
        "a per-session mode wins over the settings default"
    );
}
