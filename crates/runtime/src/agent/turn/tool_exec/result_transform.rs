//! 工具结果变换器的应用：在所有钩子之后、结果交给调用方之前，对文本与图片做最后一道改写。

use super::super::tool_result::PendingToolImage;
use super::ToolExecCtx;

/// Run the registered transformers over this call's outcome and the images
/// it returned.
///
/// Deliberately the last thing that touches them — after every hook, after
/// the lifecycle events, immediately before the caller gets them. That
/// ordering is what makes a redacting transformer a guarantee: nothing
/// downstream can put back what it removed.
pub(in crate::agent::turn) fn apply_result_transformers(
    ctx: &ToolExecCtx,
    name: &str,
    input: &serde_json::Value,
    outcome: base::interface::tool_middleware::ToolOutcome,
    images: Vec<PendingToolImage>,
) -> (
    base::interface::tool_middleware::ToolOutcome,
    Vec<PendingToolImage>,
) {
    if ctx.result_transformers.is_empty() {
        return (outcome, images);
    }
    let call = base::interface::tool_middleware::ToolCall {
        name: name.to_string(),
        input: input.clone(),
    };
    // A transformer that turns a plain answer into a failure gets the
    // engine-level treatment that has always meant: the call is an `Err`,
    // which cancels its concurrent siblings. One handed a result the tool
    // itself already called a failure hands back the same shape, because
    // nothing about it changed.
    let was_reported_failure = matches!(&outcome, Ok(a) if a.reported_failure);
    let (text, is_error, carried) = match outcome {
        Ok(a) => (
            a.text,
            a.reported_failure,
            (a.new_messages, a.structured, a.file_changes),
        ),
        Err(e) => (e, true, (None, None, Vec::new())),
    };
    let (new_messages, structured, file_changes) = carried;
    let original = (text.clone(), is_error);
    let mut draft = base::interface::tool_result::ToolResultDraft {
        text,
        images: images
            .into_iter()
            .map(|i| base::interface::tool_result::ResultImage {
                media_type: i.media_type,
                data: i.data,
            })
            .collect(),
        is_error,
    };
    base::interface::tool_result::apply(&ctx.result_transformers, &call, &mut draft);
    // The structured form is the same answer as the text; once a transformer
    // has rewritten the text (a redaction, say), handing a program the
    // untouched structure would undo it.
    let untouched = (&draft.text, draft.is_error) == (&original.0, original.1);
    let structured = structured.filter(|_| untouched);
    let file_changes = match untouched {
        true => file_changes,
        false => file_changes
            .into_iter()
            .map(|c| c.without_content())
            .collect(),
    };

    let images = draft
        .images
        .into_iter()
        .map(|i| PendingToolImage {
            media_type: i.media_type,
            data: i.data,
        })
        .collect();

    let outcome = if draft.is_error && !was_reported_failure {
        Err(draft.text)
    } else {
        Ok(base::interface::tool_middleware::ToolAnswer {
            text: draft.text,
            new_messages,
            reported_failure: draft.is_error,
            structured,
            file_changes,
        })
    };
    (outcome, images)
}
