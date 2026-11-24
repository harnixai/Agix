//! LLM backend adapters — one [`base::interface::model::Model`] implementation
//! per wire protocol.
//!
//! - **Anthropic Messages API** ([`adapter::AnthropicModel`] over
//!   [`client::AnthropicClient`]) — the primary path.
//! - **OpenAI Chat Completions** ([`openai::OpenAICompatibleModel`]) — for
//!   OpenAI itself and the many gateways that only expose its shape (vLLM,
//!   Ollama, most third-party relays).
//! - **OpenAI Responses** ([`openai::responses::OpenAIResponsesModel`]) — the
//!   items-and-named-events protocol OpenAI's newer models are served on.
//!
//! All three translate the protocol-agnostic `PromptBlock` / `ToolDef` /
//! `ModelMessage` inputs into their own request format and normalize the
//! response back into a single `ModelEvent` stream, so nothing above this
//! crate needs to know which protocol is in use.
//!
//! Over those clients, [`routing`] sends each call to the first provider of
//! `model.order` that can take it and meters it there, once; runtime reaches
//! it only as [`base::provider::TaskRouting`].

pub mod adapter;
mod cancel;
pub mod catalog;
pub mod client;
pub mod error;
pub mod factory;
#[cfg(any(test, feature = "testing"))]
pub mod mock;
pub mod openai;
pub mod parser;
mod retry_after;
pub mod routing;
pub mod stream;
mod thinking;
pub mod tokens;
pub mod types;

pub use openai::responses::OpenAIResponsesModel;
pub use openai::OpenAICompatibleModel;

/// The call a tool's streamed arguments make: no arguments at all are an
/// empty object, anything else has to parse.
pub(crate) fn tool_use_from_arguments(
    id: String,
    name: String,
    arguments: String,
) -> base::interface::model::ModelEvent {
    use base::interface::model::ModelEvent;
    if arguments.trim().is_empty() {
        return ModelEvent::ToolUse {
            id,
            name,
            input: serde_json::Value::Object(Default::default()),
        };
    }
    match serde_json::from_str(&arguments) {
        Ok(input) => ModelEvent::ToolUse { id, name, input },
        Err(e) => {
            tracing::warn!(error = %e, tool = %name, raw = %arguments, "tool call arguments are not JSON");
            ModelEvent::MalformedToolUse {
                id,
                name,
                raw_input: arguments,
            }
        }
    }
}
