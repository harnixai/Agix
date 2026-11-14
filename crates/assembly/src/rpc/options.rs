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
        "permission_mode",
        "permission_rules",
        "output_schema",
        "provider",
        "model",
        "prompt",
        "prompt_types",
        "host_tools",
    ];

    /// The fields whose value is a list. Each entry is read on its own, so
    /// one bad entry is left out without the rest — a bad allow rule must not
    /// take the deny rules beside it along.
    pub const LIST_FIELDS: &'static [&'static str] =
        &["permission_rules", "prompt_types", "host_tools"];

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
                ignored.push(IgnoredOption {
                    field: Some(name.clone()),
                    error: "unknown field".to_string(),
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
            permission_mode: Some(base::permission::PermissionMode::Plan),
            permission_rules: vec![serde_json::from_value(serde_json::json!({
                "source": "session", "behavior": "allow", "tool_name": "Read"
            }))
            .unwrap()],
            output_schema: Some(serde_json::json!({})),
            provider: Some("p".into()),
            model: Some("m".into()),
            prompt: Some(Default::default()),
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
            }],
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
    fn a_bad_entry_of_a_list_field_is_left_out_alone() {
        let params = serde_json::json!({"options": {
            "permission_rules": [
                {"source": "session", "behavior": "allwo", "tool_name": "Read"},
                {"source": "session", "behavior": "deny", "tool_name": "Bash"},
            ],
            "prompt_types": [7, "question"],
        }});
        let (options, ignored) = SessionOptions::from_params(&params);
        let options = options.unwrap();
        assert_eq!(options.permission_rules.len(), 1);
        assert_eq!(options.permission_rules[0].tool_name, "Bash");
        assert!(options.answers_questions());
        let mut fields: Vec<_> = ignored.iter().map(|i| i.field.as_deref()).collect();
        fields.sort();
        assert_eq!(
            fields,
            [Some("permission_rules[0]"), Some("prompt_types[0]")]
        );

        let (_, ignored) =
            SessionOptions::from_params(&serde_json::json!({"options": {"permission_rules": {}}}));
        assert_eq!(ignored[0].field.as_deref(), Some("permission_rules"));
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
            "permission_mode": "plann",
            "model": "m",
            "permision_rules": [],
        }});
        let (options, mut ignored) = SessionOptions::from_params(&params);
        assert_eq!(options.unwrap().model.as_deref(), Some("m"));
        ignored.sort_by(|a, b| a.field.cmp(&b.field));
        let fields: Vec<_> = ignored.iter().map(|i| i.field.as_deref()).collect();
        assert_eq!(fields, [Some("permision_rules"), Some("permission_mode")]);
        assert_eq!(ignored[0].error, "unknown field");
        assert!(ignored[1].error.contains("plann"), "{:?}", ignored[1]);

        let (options, ignored) = SessionOptions::from_params(&serde_json::json!({}));
        assert!(options.is_none() && ignored.is_empty());
        let (options, ignored) = SessionOptions::from_params(&serde_json::json!({"options": 3}));
        assert!(options.is_none());
        assert_eq!(ignored[0].field, None);
    }
}
