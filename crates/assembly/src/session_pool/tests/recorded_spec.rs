//! `agix.session/spec` 条目的载荷格式：0.4.0 写出的日志要能读回来，且原样写回得到同一份 JSON。

use crate::session_pool::parts::specs::RecordedSpec;

const SPEC_0_4_0: &str = include_str!("fixtures/spec_0.4.0.json");

#[test]
fn a_spec_entry_written_by_0_4_0_reads_back_and_writes_the_same_json() {
    let written: serde_json::Value = serde_json::from_str(SPEC_0_4_0).unwrap();
    let spec: RecordedSpec = serde_json::from_value(written.clone()).unwrap();

    let options = spec.options.as_ref().expect("options");
    assert_eq!(options.model.as_deref(), Some("m-options"));
    assert_eq!(
        options.clock.as_ref().unwrap().start,
        "2026-10-28T09:00:00Z"
    );
    assert_eq!(options.recorder.as_ref().unwrap().mode, "record");
    assert_eq!(options.telemetry.as_ref().unwrap().output, "/tmp/t.jsonl");
    assert_eq!(
        options.requested_permission_mode(),
        Some(base::permission::PermissionMode::Plan)
    );
    assert_eq!(
        options.thinking,
        Some(base::interface::model::ThinkingMode::Off)
    );
    assert_eq!(options.prompt_types, ["question"]);
    assert_eq!(spec.provider.as_deref(), Some("p-spec"));

    assert_eq!(serde_json::to_value(&spec).unwrap(), written);
}

#[test]
fn an_empty_spec_entry_reads_as_no_spec() {
    let spec: RecordedSpec = serde_json::from_value(serde_json::json!({})).unwrap();
    assert!(spec.options.is_none() && spec.prompt.is_none());
    assert_eq!(serde_json::to_value(&spec).unwrap(), serde_json::json!({}));
}

/// A 0.3 session's options, recorded in the shape of the time, come back in
/// this one: what it was held to then it is held to now.
#[test]
fn a_spec_entry_written_by_0_3_reads_back_in_this_shape() {
    let entry = history::entry::EnvelopedEntry::new(
        base::session::SessionId::new(),
        history::entry::LogEntry::Extension {
            ns: "agix.session".into(),
            event: "spec".into(),
            payload: serde_json::json!({"options": {
                "thinking_mode": "off",
                "permission_mode": "plan",
                "permission_rules": [
                    {"source": "session", "behavior": "deny", "tool_name": "Write"},
                    {"source": "session", "behavior": "allow", "tool_name": "Bash", "rule_content": "ls:*"},
                ],
                "turn": {"token_budget": 5000},
                "model": "m",
            }}),
        },
    );
    let spec = crate::session_pool::parts::specs::recorded_in(&[entry]);
    let options = spec.options.expect("options");
    assert_eq!(
        options.thinking,
        Some(base::interface::model::ThinkingMode::Off)
    );
    assert_eq!(options.model.as_deref(), Some("m"));
    assert_eq!(
        serde_json::Value::Object(options.settings.unwrap()),
        serde_json::json!({
            "permissions": {"mode": "plan", "deny": ["Write"], "allow": ["Bash(ls:*)"]},
            "turn": {"token_budget": 5000},
        })
    );
}

/// A log is read back from the project's `state/`, which a checked-out
/// repository can ship: a spec in a shape no engine wrote is read without
/// panicking, and what does not read is not taken.
#[test]
fn a_spec_entry_no_engine_wrote_does_not_stop_the_read() {
    for options in [
        serde_json::json!({"settings": "x", "turn": {"token_budget": 1}}),
        serde_json::json!({"settings": {"permissions": 3}, "permission_mode": "plan"}),
        serde_json::json!({"settings": [], "permission_rules": [
            {"behavior": "deny", "tool_name": "Write"},
        ]}),
    ] {
        let entry = history::entry::EnvelopedEntry::new(
            base::session::SessionId::new(),
            history::entry::LogEntry::Extension {
                ns: "agix.session".into(),
                event: "spec".into(),
                payload: serde_json::json!({ "options": options }),
            },
        );
        let spec = crate::session_pool::parts::specs::recorded_in(&[entry]);
        let settings = spec.options.and_then(|o| o.settings).unwrap_or_default();
        assert!(!settings.contains_key("turn"), "{options}");
        assert!(
            settings.get("permissions").is_none_or(|p| !p.is_object()),
            "{options}"
        );
    }
}
