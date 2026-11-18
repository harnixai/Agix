//! 工具结果进入对话：内容渲染成文本、带外图片的收集与追加。

use base::tool::ToolResultContent;

/// A base64 image a tool returned, waiting to be attached to this round's
/// tool-result message. See [`ToolExecCtx::images`].
#[derive(Debug, Clone)]
pub(crate) struct PendingToolImage {
    pub media_type: String,
    pub data: String,
}

/// Render a `ToolResultContent` into the text the model sees, lifting out any
/// images so the caller can emit them as real `ModelContentBlock::Image`s.
///
/// `Blocks` is what `mcp::adapter::into_tool_result` produces for any structured
/// MCP result, so this is the normal path for a large class of tools.
pub(super) fn render_tool_result_content(
    content: base::tool::ToolResultContent,
) -> (String, Vec<PendingToolImage>) {
    match content {
        ToolResultContent::Text(t) => (t, Vec::new()),
        ToolResultContent::Blocks(blocks) => {
            let mut parts: Vec<String> = Vec::new();
            let mut images: Vec<PendingToolImage> = Vec::new();
            for block in &blocks {
                if let Some((media_type, data)) = block.as_image() {
                    images.push(PendingToolImage {
                        media_type: media_type.to_string(),
                        data: data.to_string(),
                    });
                    continue;
                }
                // Any non-image block contributes whatever text it carries.
                // An unknown `block_type` with text is still worth showing;
                // one with neither text nor a usable image source has nothing
                // to contribute and is dropped rather than Debug-printed.
                if let Some(text) = block.text.as_ref() {
                    if !text.is_empty() {
                        parts.push(text.clone());
                    }
                }
            }
            let mut text = parts.join("\n");
            if text.is_empty() && !images.is_empty() {
                // A tool_result whose content is the empty string is not useful
                // to the model (and some providers reject it) — name what the
                // accompanying Image blocks are instead.
                text = if images.len() == 1 {
                    "[image]".to_string()
                } else {
                    format!("[{} images]", images.len())
                };
            }
            (text, images)
        }
    }
}

/// Fence a result that came from outside the user's reach — a server's, a
/// host's, a file's outside the project, what an operator's endpoint relays —
/// so the model reads it as data. A server's error is its own words and is
/// fenced too; any other failure is the engine's. A program reading a file
/// outside the project gets its lines as they are. A reach or relay this
/// does not know is fenced.
pub(super) fn fence_external(
    name: &str,
    facts: &base::interface::permission::ToolFacts,
    from_code: bool,
    outcome: &mut base::interface::tool_middleware::ToolOutcome,
) {
    use base::interface::permission::{Reach, Relays};
    let source = || facts.subject.content().unwrap_or(name).to_string();
    let (source, from) = match (&facts.relays, &facts.reach) {
        (Some(Relays::Web), _) => (source(), "the web".to_string()),
        (Some(Relays::McpServer(server)), _) => (
            name.to_string(),
            match server {
                Some(server) => format!("the MCP server `{server}`"),
                None => "an MCP server".to_string(),
            },
        ),
        (None, Reach::Server) => (
            name.to_string(),
            match facts.origin.mcp_server() {
                Some(server) => format!("the MCP server `{server}`"),
                None => "a server".to_string(),
            },
        ),
        (None, Reach::Host { .. }) => (source(), "the web".to_string()),
        (None, Reach::Outside) if !from_code => {
            (source(), "a file outside the project".to_string())
        }
        (
            None,
            Reach::Outside
            | Reach::SessionOnly
            | Reach::Delegates
            | Reach::Operator
            | Reach::Inside
            | Reach::EditsProject
            | Reach::Persists(_)
            | Reach::Refused(_)
            | Reach::Unknown,
        ) => return,
        _ => (source(), "outside the user's reach".to_string()),
    };
    let text = match outcome {
        Ok(answer) => &mut answer.text,
        Err(message) if facts.reach == Reach::Server => message,
        Err(_) => return,
    };
    *text = tools::external_content::fence(&source, &from, text);
}

/// What `Read` or `Grep` found in a saved tool result: the original may have
/// been a server's or a page's, and reading it back must not wash it into a
/// project file.
pub(super) fn fence_saved_result(
    name: &str,
    outcome: &mut base::interface::tool_middleware::ToolOutcome,
) {
    if let Ok(answer) = outcome {
        answer.text = tools::external_content::fence(
            name,
            "an earlier tool result saved to file",
            &answer.text,
        );
    }
}
