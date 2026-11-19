//! Picking the memories relevant to a query: one model call, with a
//! substring scan as the fallback.

use async_trait::async_trait;
use base::interface::memory_contracts::{MemoryRetriever, MemoryStorage, RetrievalRequest};
use base::interface::model::Model;
use base::memory::DurableMemory;

/// The substring fallback the LLM path drops to.
///
/// Identical to what `MemoryStore::search` does, applied to memories already
/// read rather than re-reading them — the caller is holding the same list the
/// store would have handed back.
fn substring_matches(memories: &[DurableMemory], query: &str) -> Vec<DurableMemory> {
    memories
        .iter()
        .filter(|m| {
            m.name.contains(query) || m.description.contains(query) || m.content.contains(query)
        })
        .cloned()
        .collect()
}

/// The names of the memories relevant to `query`, picked by a model call
/// unless there are no more candidates than `max_results`.
///
/// Takes memories already read rather than a [`MemoryStore`], so a
/// [`MemoryStorage`](base::interface::memory_contracts::MemoryStorage)
/// backend that is not this store gets the same selection.
#[allow(clippy::too_many_arguments)]
pub async fn select_memories_from(
    memories: &[DurableMemory],
    query: &str,
    model: &dyn base::interface::model::Model,
    max_results: usize,
    already_surfaced: &std::collections::HashSet<String>,
    recent_tools: &[String],
    model_name: &str,
    session_id: Option<&str>,
) -> Vec<String> {
    let start = std::time::Instant::now();
    let num_available = memories.len();
    if memories.is_empty() {
        return Vec::new();
    }
    let log = |selected: usize, note: &str| {
        tracing::info!(
            target: "memory_recall_shape",
            num_memories_selected = selected,
            num_memories_available = num_available,
            model_used = model_name,
            latency_ms = start.elapsed().as_millis() as u64,
            "{note}",
        );
    };

    let mut headers = memories.to_vec();
    headers.retain(|h| !already_surfaced.contains(&h.name));
    // A memory named like a recently used tool is ambiguous with it.
    headers.retain(|h| !recent_tools.iter().any(|t| h.name.eq_ignore_ascii_case(t)));

    if headers.is_empty() {
        log(0, "memory recall — no memories after dedup");
        return Vec::new();
    }
    if headers.len() <= max_results {
        let names: Vec<String> = headers.into_iter().map(|h| h.name).collect();
        log(names.len(), "memory recall completed");
        return names;
    }

    let (request_messages, params) =
        selection_request(query, &headers, max_results, model_name, session_id);
    let stream_result = model
        .stream(
            vec![],
            vec![],
            request_messages,
            params,
            tokio_util::sync::CancellationToken::new(),
        )
        .await;

    let Ok(mut stream) = stream_result else {
        let names = substring_fallback(memories, &headers, query, max_results);
        log(names.len(), "memory recall — fallback to substring search");
        return names;
    };

    use futures::StreamExt;
    let mut full_text = String::new();
    while let Some(Ok(event)) = stream.next().await {
        if let base::interface::model::ModelEvent::TextDelta { text } = event {
            full_text.push_str(&text);
        }
    }

    if let Some(names) = parse_selection(&full_text, max_results) {
        log(names.len(), "memory recall completed");
        return names;
    }
    let names = substring_fallback(memories, &headers, query, max_results);
    log(
        names.len(),
        "memory recall — final fallback to substring search",
    );
    names
}

fn substring_fallback(
    memories: &[DurableMemory],
    headers: &[DurableMemory],
    query: &str,
    max_results: usize,
) -> Vec<String> {
    let allowed: std::collections::HashSet<&str> =
        headers.iter().map(|h| h.name.as_str()).collect();
    substring_matches(memories, query)
        .into_iter()
        .filter(|m| allowed.contains(m.name.as_str()))
        .map(|m| m.name)
        .take(max_results)
        .collect()
}

fn parse_selection(text: &str, max_results: usize) -> Option<Vec<String>> {
    let parsed = serde_json::from_str::<serde_json::Value>(text).ok()?;
    let arr = parsed["selected_memories"].as_array()?;
    Some(
        arr.iter()
            .filter_map(|v| v.as_str().map(|s| s.to_string()))
            .take(max_results)
            .collect(),
    )
}

fn selection_request(
    query: &str,
    headers: &[DurableMemory],
    max_results: usize,
    model_name: &str,
    session_id: Option<&str>,
) -> (
    Vec<base::interface::model::ModelMessage>,
    base::interface::model::StreamParams,
) {
    use base::interface::model::ThinkingMode;
    use base::interface::model::{MessageRole, ModelContentBlock, ModelMessage};

    let manifest: String = headers
        .iter()
        .map(|h| format!("- **{}**: {}", h.name, h.description))
        .collect::<Vec<_>>()
        .join("\n");

    let system_prompt = "\
You are selecting memories that will be useful to an AI agent as it processes a user's query. \
You will be given the user's query and a list of available memory files with their names and descriptions.

Return a JSON array of memory names for the memories that will clearly be useful (up to the requested maximum). \
Only include memories that you are certain will be helpful based on their name and description.
- If you are unsure if a memory will be useful, do not include it. Be selective and discerning.
- If there are no memories that would clearly be useful, return an empty array.

Respond with ONLY valid JSON matching this schema (no markdown, no extra text, no code fences):

```json
{\"selected_memories\": [\"name1\", \"name2\"]}
```";

    let user_message = format!(
        "User query: {query}\n\nAvailable memories:\n{manifest}\n\n\
         Select up to {max_results} most relevant memory names.",
    );

    let request_messages = vec![
        ModelMessage {
            role: MessageRole::User,
            content: vec![ModelContentBlock::Text {
                text: system_prompt.into(),
            }],
        },
        ModelMessage {
            role: MessageRole::User,
            content: vec![ModelContentBlock::Text { text: user_message }],
        },
    ];

    let params = base::interface::model::StreamParams {
        model: model_name.into(),
        max_tokens: 1000,
        thinking_mode: ThinkingMode::Off,
        effort: None,
        cache_edits: vec![],
        origin: session_id.map(|id| {
            base::interface::model::CallOrigin::auxiliary(
                id,
                base::interface::model::call_purpose::MEMORY,
            )
        }),
        input_map: None,
        provider_search: None,
        usage_tap: Default::default(),
    };
    (request_messages, params)
}

/// The engine's own retriever: ask the model which memories are relevant.
///
/// The default because relevance here is a judgement about meaning, which is
/// the thing a model is for. It costs a model call, which is why the
/// `MemoryRetriever` seam exists.
pub struct LlmRetriever;

#[async_trait]
impl MemoryRetriever for LlmRetriever {
    async fn retrieve(
        &self,
        storage: &dyn MemoryStorage,
        model: &dyn Model,
        request: &RetrievalRequest,
    ) -> Vec<String> {
        select_memories_from(
            &storage.load_all(),
            &request.query,
            model,
            request.limit,
            &request.already_surfaced,
            &request.recent_tools,
            &request.model_name,
            request.session_id.as_deref(),
        )
        .await
    }
}
