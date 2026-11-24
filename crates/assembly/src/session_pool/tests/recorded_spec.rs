//! `agix.session/spec` 条目的载荷格式：0.3 与 0.4.0 写出的日志读回成现在的形状，现在写出的
//! 原样读回得到同一份 JSON，读不懂的条目不让读取失败。

use crate::session_pool::parts::specs::{recorded_in, RecordedSpec};
use serde_json::json;

const SPEC_0_4_0: &str = include_str!("fixtures/spec_0.4.0.json");

fn entry(payload: serde_json::Value) -> history::entry::EnvelopedEntry {
    history::entry::EnvelopedEntry::new(
        base::session::SessionId::new(),
        history::entry::LogEntry::Extension {
            ns: "agix.session".into(),
            event: "spec".into(),
            payload,
        },
    )
}

/// 0.4.0 kept the effort, thinking and prompt a session was created and
/// changed with beside its settings; they come back as keys of its own
/// settings, so a resumed session keeps them.
#[test]
fn a_spec_entry_written_by_0_4_0_reads_back_in_this_shape() {
    let written: serde_json::Value = serde_json::from_str(SPEC_0_4_0).unwrap();
    let spec = recorded_in(&[entry(written)]);

    let options = spec.options.as_ref().expect("options");
    assert_eq!(options.model.as_deref(), Some("m-options"));
    assert_eq!(
        options.clock.as_ref().unwrap().start,
        "2026-10-28T09:00:00Z"
    );
    assert_eq!(options.recorder.as_ref().unwrap().mode, "record");
    assert_eq!(options.telemetry.as_ref().unwrap().output, "/tmp/t.jsonl");
    assert_eq!(options.prompt_types, ["question"]);
    assert_eq!(
        serde_json::Value::Object(options.settings.clone().unwrap()),
        json!({
            "permissions": {"mode": "plan", "deny": ["Write"]},
            "turn": {"token_budget": 5000},
            "model": {"effort": "max", "thinking": "off"},
            "prompt": {"append": "be brief"},
        })
    );
    assert_eq!(
        serde_json::Value::Object(spec.settings.clone().unwrap()),
        json!({
            "permissions": {"mode": "plan", "deny": ["Write"]},
            "turn": {"token_budget": 5000},
            "model": {"effort": "xhigh"},
            "prompt": {"append": "spec prompt"},
        }),
        "what was set since is over what the session was created with; a `null` is taken out"
    );
    assert_eq!(
        spec.requested_permission_mode(),
        Some(base::permission::PermissionMode::Plan)
    );
    assert_eq!(spec.provider.as_deref(), Some("p-spec"));
    assert_eq!(spec.model.as_deref(), Some("m-spec"));
}

#[test]
fn a_spec_entry_written_now_reads_back_and_writes_the_same_json() {
    let written = json!({
        "options": {
            "model": "m",
            "output_schema": {"type": "object"},
            "settings": {"model": {"effort": "high"}},
        },
        "settings": {"model": {"effort": "low"}, "prompt": {"override": "only this"}},
        "provider": "p",
        "permission_mode": "acceptEdits",
    });
    let spec: RecordedSpec = serde_json::from_value(written.clone()).unwrap();
    assert_eq!(serde_json::to_value(&spec).unwrap(), written);
    let read = recorded_in(&[entry(written)]);
    assert_eq!(
        read.session_setting("prompt.override"),
        Some(&json!("only this"))
    );
}

#[test]
fn an_empty_spec_entry_reads_as_no_spec() {
    let spec: RecordedSpec = serde_json::from_value(json!({})).unwrap();
    assert!(spec.options.is_none() && spec.settings.is_none());
    assert_eq!(serde_json::to_value(&spec).unwrap(), json!({}));
}

/// A 0.3 session's options, recorded in the shape of the time, come back in
/// this one: what it was held to then it is held to now.
#[test]
fn a_spec_entry_written_by_0_3_reads_back_in_this_shape() {
    let spec = recorded_in(&[entry(json!({"options": {
        "thinking_mode": "off",
        "permission_mode": "plan",
        "permission_rules": [
            {"source": "session", "behavior": "deny", "tool_name": "Write"},
            {"source": "session", "behavior": "allow", "tool_name": "Bash", "rule_content": "ls:*"},
        ],
        "turn": {"token_budget": 5000},
        "model": "m",
    }}))]);
    let options = spec.options.expect("options");
    assert_eq!(options.model.as_deref(), Some("m"));
    assert_eq!(
        serde_json::Value::Object(options.settings.unwrap()),
        json!({
            "model": {"thinking": "off"},
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
        json!({"settings": "x", "turn": {"token_budget": 1}}),
        json!({"settings": {"permissions": 3}, "permission_mode": "plan"}),
        json!({"settings": [], "permission_rules": [
            {"behavior": "deny", "tool_name": "Write"},
        ]}),
        json!({"settings": 7, "effort": "high", "prompt": {"append": "x"}}),
        json!({"settings": {"model": 3}, "effort": "high", "thinking": {"on_budget": "lots"}}),
        json!({"settings": {"prompt": "x"}, "prompt": [1, 2]}),
    ] {
        let spec = recorded_in(&[entry(json!({ "options": options }))]);
        let settings = spec.options.and_then(|o| o.settings).unwrap_or_default();
        assert!(!settings.contains_key("turn"), "{options}");
        assert!(
            settings.get("permissions").is_none_or(|p| !p.is_object()),
            "{options}"
        );
    }
    for payload in [
        json!({"effort": 3, "thinking_mode": [], "prompt": "x"}),
        json!({"options": {"settings": "x"}, "effort": "high", "prompt": {"append": 5}}),
        json!({"settings": {"model": "x"}, "effort": "high"}),
    ] {
        let spec = recorded_in(&[entry(payload)]);
        let _ = spec.session_setting("model.effort");
        let _ = spec.requested_permission_mode();
    }
    let kept = recorded_in(&[entry(json!({"settings": "x", "model": "m"}))]);
    assert_eq!(
        kept.model.as_deref(),
        Some("m"),
        "a session layer that does not read leaves the rest of the spec"
    );
}

/// The plugins a session draws on only narrow: a last spec that does not
/// read leaves its `plugins` settings in force over the spec read before it,
/// and leaves the session none when they cannot be found in it either.
#[test]
fn a_last_spec_that_does_not_read_keeps_its_plugins_narrowed() {
    let earlier = entry(json!({"options": {"settings": {"model": {"effort": "low"}}}}));
    let unread = entry(json!({
        "options": {"prompt_types": 3, "settings": {"plugins": {"allow": ["a"]}}},
    }));
    let spec = recorded_in(&[earlier.clone(), unread]);
    assert_eq!(
        spec.settings_layer()
            .map(|l| serde_json::Value::Object(l.clone())),
        Some(json!({"model": {"effort": "low"}, "plugins": {"allow": ["a"]}}))
    );

    for lost in [
        json!("x"),
        json!({"options": 3}),
        json!({"options": {"settings": "x"}}),
    ] {
        let spec = recorded_in(&[earlier.clone(), entry(lost.clone())]);
        assert_eq!(
            spec.session_setting("plugins.enabled"),
            Some(&json!(false)),
            "{lost}"
        );
    }

    let read = entry(json!({"options": {"settings": {"plugins": {"allow": ["a"]}}}}));
    assert_eq!(
        recorded_in(&[read]).settings,
        None,
        "a spec that reads is taken as it is"
    );
    let untouched = entry(json!({"options": {"prompt_types": 3}}));
    assert_eq!(
        recorded_in(&[earlier, untouched]).session_setting("plugins"),
        None
    );
}
