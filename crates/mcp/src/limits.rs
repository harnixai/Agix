//! What a server's tool list is cut to before the model sees it: how many
//! tools, how long a description and how large an input schema. A server
//! decides what it lists; the product decides how much of that reaches every
//! request, so one server cannot fill the context or the request body.

use crate::client::McpToolMeta;

/// `tools` kept to `(max_tools, max_description_chars, max_schema_bytes)`,
/// and a sentence saying what was cut, `None` when nothing was. A schema over
/// its limit cannot be cut and still mean something, so it becomes an open
/// object.
pub fn within(
    mut tools: Vec<McpToolMeta>,
    (max_tools, max_description, max_schema): &(usize, usize, usize),
) -> (Vec<McpToolMeta>, Option<String>) {
    let total = tools.len();
    tools.truncate(*max_tools);
    let (mut descriptions, mut schemas) = (0, 0);
    for tool in &mut tools {
        if let Some(text) = &tool.description {
            if text.chars().count() > *max_description {
                tool.description = Some(base::text::truncate_chars(text, *max_description, "…"));
                descriptions += 1;
            }
        }
        if tool.input_schema.to_string().len() > *max_schema {
            tool.input_schema = serde_json::json!({ "type": "object" });
            schemas += 1;
        }
    }
    let mut cut = Vec::new();
    if tools.len() < total {
        cut.push(format!("kept {} of {total} tools", tools.len()));
    }
    if descriptions > 0 {
        cut.push(format!(
            "{descriptions} description(s) cut to {max_description} characters"
        ));
    }
    if schemas > 0 {
        cut.push(format!(
            "{schemas} input schema(s) over {max_schema} bytes replaced by an open object"
        ));
    }
    let note = (!cut.is_empty()).then(|| cut.join("; "));
    (tools, note)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::client::McpToolHints;

    fn tool(name: &str, description: &str, schema: serde_json::Value) -> McpToolMeta {
        McpToolMeta {
            name: name.into(),
            description: Some(description.into()),
            input_schema: schema,
            hints: McpToolHints::default(),
        }
    }

    #[test]
    fn what_is_within_the_limits_is_left_alone() {
        let tools = vec![tool("a", "short", serde_json::json!({"type": "object"}))];
        let (kept, note) = within(tools, &(2, 10, 100));
        assert_eq!(kept.len(), 1);
        assert_eq!(note, None);
    }

    #[test]
    fn tools_descriptions_and_schemas_past_the_limits_are_cut_and_reported() {
        let big_schema = serde_json::json!({"type": "object", "x": "s".repeat(200)});
        let tools = vec![
            tool("a", &"d".repeat(50), big_schema),
            tool("b", "ok", serde_json::json!({"type": "object"})),
            tool("c", "dropped", serde_json::json!({"type": "object"})),
        ];
        let (kept, note) = within(tools, &(2, 10, 100));
        assert_eq!(kept.len(), 2);
        assert_eq!(kept[0].description.as_deref().unwrap().chars().count(), 11);
        assert_eq!(kept[0].input_schema, serde_json::json!({"type": "object"}));
        let note = note.unwrap();
        assert!(
            note.contains("kept 2 of 3 tools")
                && note.contains("1 description(s)")
                && note.contains("1 input schema(s)"),
            "{note}"
        );
    }
}
