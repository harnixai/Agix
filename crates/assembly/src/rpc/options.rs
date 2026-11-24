//! 会话的 `options`：字段定义，以及逐字段读取并报告读不懂之处的读法；读不懂的一律拒绝。

use serde::Serialize;
use serde_json::Value;

use crate::session_pool::api::SessionOptions;

impl SessionOptions {
    /// Every field `options` may carry.
    pub const FIELDS: &'static [&'static str] = &[
        "recorder",
        "clock",
        "telemetry",
        "output_schema",
        "provider",
        "model",
        "prompt_types",
        "host_tools",
        "attended",
        "auto_name",
        "skill_dirs",
        "settings",
    ];

    /// The fields whose value is a list. Each entry is read on its own, so
    /// each bad entry is reported by its index.
    pub const LIST_FIELDS: &'static [&'static str] = &["prompt_types", "host_tools", "skill_dirs"];

    /// `params.options` as far as it can be read, and what in it could not
    /// be: a field this daemon does not know, one whose value does not
    /// parse, or one entry of a list field. Each field is read on its own, so
    /// one bad field leaves the rest in force.
    pub fn from_params(params: &Value) -> (Option<Self>, Vec<IgnoredOption>) {
        Self::from_options(params.get("options"))
    }

    /// [`Self::from_params`] on the `options` value itself.
    pub fn from_options(options: Option<&Value>) -> (Option<Self>, Vec<IgnoredOption>) {
        let fields = match options {
            None | Some(Value::Null) => return (None, Vec::new()),
            Some(Value::Object(fields)) => fields,
            Some(_) => {
                return (
                    None,
                    vec![IgnoredOption {
                        field: None,
                        error: "options must be an object".to_string(),
                    }],
                )
            }
        };
        let mut ignored = Vec::new();
        let mut readable = serde_json::Map::new();
        for (name, value) in fields {
            if !Self::FIELDS.contains(&name.as_str()) {
                let error = match RENAMED.iter().find(|(old, _)| old == name) {
                    Some((_, new)) => format!("unknown field; it is now `{new}`"),
                    None => "unknown field".to_string(),
                };
                ignored.push(IgnoredOption {
                    field: Some(name.clone()),
                    error,
                });
                continue;
            }
            let value = match value {
                Value::Array(entries) if Self::LIST_FIELDS.contains(&name.as_str()) => {
                    Value::Array(Self::readable_entries(name, entries, &mut ignored))
                }
                _ => value.clone(),
            };
            match Self::read_alone(name, &value) {
                Ok(()) if name == "recorder" && !recorder_reads(&value, &mut ignored) => {}
                Ok(()) if name == "settings" && !settings_read(&value, &mut ignored) => {}
                Ok(()) => {
                    readable.insert(name.clone(), value);
                }
                Err(error) => ignored.push(IgnoredOption {
                    field: Some(name.clone()),
                    error,
                }),
            }
        }
        if readable.is_empty() {
            return (None, ignored);
        }
        match serde_json::from_value(Value::Object(readable)) {
            Ok(options) => (Some(options), ignored),
            Err(e) => {
                ignored.push(IgnoredOption {
                    field: None,
                    error: e.to_string(),
                });
                (None, ignored)
            }
        }
    }

    fn read_alone(name: &str, value: &Value) -> Result<(), String> {
        let alone = Value::Object([(name.to_string(), value.clone())].into_iter().collect());
        serde_json::from_value::<Self>(alone)
            .map(drop)
            .map_err(|e| e.to_string())
    }

    fn readable_entries(
        name: &str,
        entries: &[Value],
        ignored: &mut Vec<IgnoredOption>,
    ) -> Vec<Value> {
        let mut kept = Vec::new();
        for (index, entry) in entries.iter().enumerate() {
            match Self::read_alone(name, &Value::Array(vec![entry.clone()])) {
                Ok(()) => kept.push(entry.clone()),
                Err(error) => ignored.push(IgnoredOption {
                    field: Some(format!("{name}[{index}]")),
                    error,
                }),
            }
        }
        if name == "host_tools" {
            host_tool_problems(entries, ignored);
        }
        kept
    }
}

/// Fields `options` no longer has, with what took their place.
const RENAMED: &[(&str, &str)] = &[
    ("effort", "settings.model.effort"),
    ("thinking", "settings.model.thinking"),
    ("thinking_mode", "settings.model.thinking"),
    (
        "prompt",
        "settings.prompt.append / settings.prompt.override",
    ),
    ("permission_mode", "settings.permissions.mode"),
    (
        "permission_rules",
        "settings.permissions.allow / allow_exact / ask / deny",
    ),
    ("turn", "settings.turn"),
];

/// Whether `options.recorder` names a usable place; what does not is added to
/// `ignored` by field.
fn recorder_reads(value: &Value, ignored: &mut Vec<IgnoredOption>) -> bool {
    use crate::session_pool::api::RecorderOptions;
    let Ok(recorder) = serde_json::from_value::<RecorderOptions>(value.clone()) else {
        return true;
    };
    let problems = recorder.problems();
    for (field, error) in &problems {
        ignored.push(IgnoredOption {
            field: Some(format!("recorder.{field}")),
            error: error.clone(),
        });
    }
    problems.is_empty()
}

/// Whether `options.settings` holds only what a session may be given; what
/// it does not is added to `ignored` by field.
fn settings_read(value: &Value, ignored: &mut Vec<IgnoredOption>) -> bool {
    let options = SessionOptions {
        settings: value.as_object().cloned(),
        ..Default::default()
    };
    let problems = options.settings_problems();
    for (field, error) in &problems {
        ignored.push(IgnoredOption {
            field: Some(field.clone()),
            error: error.clone(),
        });
    }
    problems.is_empty()
}

/// What the engine would refuse of `options.host_tools`, by the field of the
/// entry it is in. Whether a name is taken by one of the session's own tools
/// is known only once its engine is built.
fn host_tool_problems(entries: &[Value], ignored: &mut Vec<IgnoredOption>) {
    use crate::session_pool::api::HostToolOption;
    if entries.len() > runtime::host_tools::MAX_DECLARED {
        ignored.push(IgnoredOption {
            field: Some("host_tools".into()),
            error: format!("at most {} host tools", runtime::host_tools::MAX_DECLARED),
        });
    }
    let mut names = std::collections::HashSet::new();
    for (index, entry) in entries.iter().enumerate() {
        let Ok(tool) = serde_json::from_value::<HostToolOption>(entry.clone()) else {
            continue;
        };
        let mut problems = runtime::host_tools::declaration_problems(&tool.declaration());
        if !names.insert(tool.name.clone()) {
            problems.push(("name", "declared twice".into()));
        }
        for (field, error) in problems {
            ignored.push(IgnoredOption {
                field: Some(format!("host_tools[{index}].{field}")),
                error,
            });
        }
    }
}

/// A field of `options` that could not be read, as the `INVALID_PARAMS`
/// error's `invalid_options` lists it. `field` is `None` when `options` as a
/// whole could not be read.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, schemars::JsonSchema)]
pub struct IgnoredOption {
    pub field: Option<String>,
    pub error: String,
}

/// `options` as a method's params carry it: any JSON, read field by field by
/// [`read_options`] rather than refused whole.
#[derive(Debug, Clone, Default, PartialEq, serde::Deserialize)]
#[serde(transparent)]
pub struct RawOptions(pub Value);

impl schemars::JsonSchema for RawOptions {
    fn schema_name() -> std::borrow::Cow<'static, str> {
        "RawOptions".into()
    }

    fn inline_schema() -> bool {
        true
    }

    fn json_schema(generator: &mut schemars::SchemaGenerator) -> schemars::Schema {
        generator.subschema_for::<SessionOptions>()
    }
}

/// `options` as [`SessionOptions::from_options`] reads it; anything in it
/// that does not read is an `INVALID_PARAMS` error naming each field.
pub fn read_options(
    options: Option<&RawOptions>,
) -> Result<Option<SessionOptions>, super::RpcError> {
    let (read, ignored) = SessionOptions::from_options(options.map(|raw| &raw.0));
    if ignored.is_empty() {
        return Ok(read);
    }
    let fields: Vec<&str> = ignored
        .iter()
        .map(|i| i.field.as_deref().unwrap_or("options"))
        .collect();
    Err(super::RpcError {
        code: super::codes::INVALID_PARAMS,
        message: format!("options that do not read: {}", fields.join(", ")),
        data: Some(serde_json::json!({ "invalid_options": ignored })),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session_pool::api::{AskKind, ClockOptions, TelemetryOptions};

    #[test]
    fn the_listed_option_fields_are_exactly_the_struct_s() {
        let every_field = SessionOptions {
            recorder: Some(serde_json::from_value(serde_json::json!({"mode": "record"})).unwrap()),
            clock: Some(ClockOptions {
                start: "2026-10-28T09:00:00Z".into(),
            }),
            telemetry: Some(TelemetryOptions {
                output: "/tmp/t.jsonl".into(),
            }),
            output_schema: Some(serde_json::json!({})),
            provider: Some("p".into()),
            model: Some("m".into()),
            prompt_types: vec![AskKind::Question.as_str().into()],
            host_tools: vec![crate::session_pool::api::HostToolOption {
                name: "HostEcho".into(),
                description: "d".into(),
                input_schema: serde_json::json!({"type": "object"}),
                read_only: false,
                destructive: false,
                concurrency_safe: None,
                timeout_ms: 120_000,
                deferred: false,
                subagents: false,
                permit: crate::session_pool::api::HostToolPermit::Allow,
            }],
            attended: Some(true),
            auto_name: Some(false),
            skill_dirs: vec![crate::session_pool::api::SkillDirOption {
                path: "/skills".into(),
            }],
            settings: serde_json::json!({"turn": {"token_budget": 1}})
                .as_object()
                .cloned(),
        };
        let serialized = serde_json::to_value(&every_field).unwrap();
        let mut names: Vec<&str> = serialized
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        let mut listed = SessionOptions::FIELDS.to_vec();
        names.sort_unstable();
        listed.sort_unstable();
        assert_eq!(names, listed);

        let mut lists: Vec<&str> = serialized
            .as_object()
            .unwrap()
            .iter()
            .filter(|(_, v)| v.is_array())
            .map(|(k, _)| k.as_str())
            .collect();
        let mut listed = SessionOptions::LIST_FIELDS.to_vec();
        lists.sort_unstable();
        listed.sort_unstable();
        assert_eq!(lists, listed, "a list field is not read entry by entry");
    }

    #[test]
    fn a_bad_entry_of_a_list_field_is_reported_by_its_index() {
        let params = serde_json::json!({"options": {"prompt_types": [7, "question"]}});
        let (options, ignored) = SessionOptions::from_params(&params);
        assert!(options.unwrap().answers_questions());
        let fields: Vec<_> = ignored.iter().map(|i| i.field.as_deref()).collect();
        assert_eq!(fields, [Some("prompt_types[0]")]);

        let (_, ignored) =
            SessionOptions::from_params(&serde_json::json!({"options": {"prompt_types": {}}}));
        assert_eq!(ignored[0].field.as_deref(), Some("prompt_types"));
    }

    #[test]
    fn settings_a_session_may_not_be_given_are_reported_by_their_key() {
        let (_, mut ignored) = SessionOptions::from_params(&serde_json::json!({"options": {
            "settings": {
                "hooks": {"Stop": []},
                "mcp": {"servers": {"gh": {"command": "gh"}}},
                "model": {"order": ["a"], "effort": "low"},
                "permissions": {"deny": ["Bash(rm"], "mode": "plan"},
                "turn": {"token_budget": 10},
            },
        }}));
        ignored.sort_by(|a, b| a.field.cmp(&b.field));
        let fields: Vec<_> = ignored.iter().map(|i| i.field.as_deref()).collect();
        assert_eq!(
            fields,
            [
                Some("settings.hooks"),
                Some("settings.mcp"),
                Some("settings.model.order"),
                Some("settings.permissions.deny[0]"),
            ]
        );

        let (options, ignored) = SessionOptions::from_params(&serde_json::json!({"options": {
            "settings": {"permissions": {"mode": "plan", "deny": ["Write"]}},
        }}));
        assert!(ignored.is_empty(), "{ignored:?}");
        assert_eq!(
            options.unwrap().requested_permission_mode(),
            Some(base::permission::PermissionMode::Plan)
        );
        let (_, ignored) =
            SessionOptions::from_params(&serde_json::json!({"options": {"settings": []}}));
        assert_eq!(ignored[0].field.as_deref(), Some("settings"));
    }

    #[test]
    fn a_field_that_moved_says_where_to() {
        let (_, mut ignored) = SessionOptions::from_params(&serde_json::json!({"options": {
            "thinking_mode": "off", "turn": {}, "permission_mode": "plan", "permission_rules": [],
            "effort": "high", "thinking": "off", "prompt": {"append": "x"},
        }}));
        ignored.sort_by(|a, b| a.field.cmp(&b.field));
        let moved: Vec<_> = ignored
            .iter()
            .map(|i| (i.field.as_deref().unwrap_or_default(), i.error.as_str()))
            .collect();
        assert_eq!(
            moved,
            [
                ("effort", "unknown field; it is now `settings.model.effort`"),
                ("permission_mode", "unknown field; it is now `settings.permissions.mode`"),
                (
                    "permission_rules",
                    "unknown field; it is now `settings.permissions.allow / allow_exact / ask / deny`"
                ),
                (
                    "prompt",
                    "unknown field; it is now `settings.prompt.append / settings.prompt.override`"
                ),
                ("thinking", "unknown field; it is now `settings.model.thinking`"),
                ("thinking_mode", "unknown field; it is now `settings.model.thinking`"),
                ("turn", "unknown field; it is now `settings.turn`"),
            ]
        );
    }

    #[test]
    fn a_thinking_budget_no_call_can_carry_is_refused_in_the_session_layer() {
        let (_, ignored) = SessionOptions::from_params(&serde_json::json!({"options": {
            "settings": {"model": {"thinking": {"on_budget": 0}}},
        }}));
        let fields: Vec<_> = ignored.iter().map(|i| i.field.as_deref()).collect();
        assert_eq!(fields, [Some("settings.model.thinking")]);
        let (options, ignored) = SessionOptions::from_params(&serde_json::json!({"options": {
            "settings": {"model": {"thinking": {"on_budget": 1024}, "effort": "high"}},
        }}));
        assert!(ignored.is_empty(), "{ignored:?}");
        assert!(options.unwrap().settings.is_some());
    }

    #[test]
    fn attended_and_auto_name_take_only_a_bool() {
        let (options, ignored) = SessionOptions::from_params(&serde_json::json!({"options": {
            "attended": true, "auto_name": false,
        }}));
        let options = options.unwrap();
        assert!(ignored.is_empty(), "{ignored:?}");
        assert_eq!(options.attended, Some(true));
        assert!(!options.names_itself());

        let (_, mut ignored) = SessionOptions::from_params(&serde_json::json!({"options": {
            "attended": "yes", "auto_name": 0,
        }}));
        ignored.sort_by(|a, b| a.field.cmp(&b.field));
        let fields: Vec<_> = ignored.iter().map(|i| i.field.as_deref()).collect();
        assert_eq!(fields, [Some("attended"), Some("auto_name")]);
    }

    #[test]
    fn a_recorder_must_name_an_absolute_root_and_one_directory_under_it() {
        for (recorder, field) in [
            (
                serde_json::json!({"mode": "record", "dir": "rel/dir"}),
                "recorder.dir",
            ),
            (
                serde_json::json!({"mode": "record", "name": "../up"}),
                "recorder.name",
            ),
            (
                serde_json::json!({"mode": "replay", "name": "a/b"}),
                "recorder.name",
            ),
            (
                serde_json::json!({"mode": "record", "name": ""}),
                "recorder.name",
            ),
        ] {
            let (options, ignored) = SessionOptions::from_params(
                &serde_json::json!({"options": {"recorder": recorder}}),
            );
            assert!(options.is_none(), "{recorder}");
            assert_eq!(ignored.len(), 1, "{recorder}");
            assert_eq!(ignored[0].field.as_deref(), Some(field), "{recorder}");
        }
        let (options, ignored) = SessionOptions::from_params(&serde_json::json!({"options": {
            "recorder": {"mode": "record", "dir": "/abs/root", "name": "run-1"}
        }}));
        assert!(ignored.is_empty());
        assert_eq!(
            options.unwrap().recorder.unwrap().name.as_deref(),
            Some("run-1")
        );
    }

    #[test]
    fn strict_is_an_unknown_field_like_any_other() {
        let (_, ignored) =
            SessionOptions::from_params(&serde_json::json!({"options": {"strict": true}}));
        assert_eq!(ignored[0].field.as_deref(), Some("strict"));
        assert_eq!(ignored[0].error, "unknown field");
    }

    #[test]
    fn a_bad_option_field_is_reported_and_the_rest_still_apply() {
        let params = serde_json::json!({"options": {
            "attended": "hihg",
            "model": "m",
            "promt": {},
        }});
        let (options, mut ignored) = SessionOptions::from_params(&params);
        assert_eq!(options.unwrap().model.as_deref(), Some("m"));
        ignored.sort_by(|a, b| a.field.cmp(&b.field));
        let fields: Vec<_> = ignored.iter().map(|i| i.field.as_deref()).collect();
        assert_eq!(fields, [Some("attended"), Some("promt")]);
        assert!(ignored[0].error.contains("hihg"), "{:?}", ignored[0]);
        assert_eq!(ignored[1].error, "unknown field");

        let (options, ignored) = SessionOptions::from_params(&serde_json::json!({}));
        assert!(options.is_none() && ignored.is_empty());
        let (options, ignored) = SessionOptions::from_params(&serde_json::json!({"options": 3}));
        assert!(options.is_none());
        assert_eq!(ignored[0].field, None);
    }
}
