//! The models a provider's endpoint lists: Anthropic's `GET /v1/models`, page
//! by page, and the `GET …/models` of the OpenAI-shaped protocols. Sent the way
//! the model clients send their calls — the same `Network`, as the operator —
//! so the deployment's egress and proxy settings apply to it as to them.

use std::time::Duration;

use base::interface::credentials::Secret;
use base::interface::exec::{HttpRequest, HttpResponse, Network, Origin};
use base::interface::model::{ModelError, ModelErrorKind};
use base::provider::ListedModel;
use serde_json::Value;
use url::Url;

use crate::error::{classify, error_type, service_message, AnthropicError};

const LIST_TIMEOUT: Duration = Duration::from_secs(30);
const MAX_PAGES: usize = 50;
const MAX_PAGE_BYTES: u64 = 8 * 1024 * 1024;
const MESSAGE_CHARS: usize = 500;

/// What an endpoint listed. `truncated`: it had more pages than are read.
#[derive(Debug, Default)]
pub struct Listing {
    pub models: Vec<ListedModel>,
    pub truncated: bool,
}

#[derive(Debug)]
pub enum ListModelsError {
    /// The endpoint does not list its models, or not in a shape this reads.
    Unsupported(String),
    Failed(ModelError),
}

impl From<ModelError> for ListModelsError {
    fn from(e: ModelError) -> Self {
        Self::Failed(e)
    }
}

/// Every model the endpoint of a provider with this `api_type` and `base_url`
/// lists, in the endpoint's order, up to [`MAX_PAGES`] pages of at most
/// [`MAX_PAGE_BYTES`] each.
pub async fn list_models(
    api_type: &str,
    base_url: Option<&str>,
    api_key: &Secret,
    net: &dyn Network,
) -> Result<Listing, ListModelsError> {
    let base_url = base_url.map(str::trim).filter(|u| !u.is_empty());
    let listing = async {
        match api_type {
            "anthropic" => list_anthropic(base_url, api_key, net).await,
            "openai_compatible" => {
                let base = base_url.ok_or_else(|| {
                    ModelError::invalid_request("api_type 'openai_compatible' requires base_url")
                })?;
                list_openai(&openai_models_url(base, "/chat/completions")?, api_key, net).await
            }
            "openai_responses" => {
                let base = base_url.unwrap_or("https://api.openai.com");
                list_openai(&openai_models_url(base, "/responses")?, api_key, net).await
            }
            other => Err(ListModelsError::Unsupported(format!(
                "listing models is not implemented for api_type '{other}'"
            ))),
        }
    };
    tokio::time::timeout(LIST_TIMEOUT, listing)
        .await
        .unwrap_or_else(|_| {
            Err(ModelError::new(
                ModelErrorKind::Timeout,
                format!(
                    "the model list did not arrive within {}s",
                    LIST_TIMEOUT.as_secs()
                ),
            )
            .into())
        })
}

async fn list_anthropic(
    base_url: Option<&str>,
    api_key: &Secret,
    net: &dyn Network,
) -> Result<Listing, ListModelsError> {
    let url = anthropic_models_url(base_url.unwrap_or(crate::client::ANTHROPIC_API_BASE_URL))?;
    let mut models = Vec::new();
    let mut after: Option<String> = None;
    for _ in 0..MAX_PAGES {
        let mut page_url = url.clone();
        page_url.query_pairs_mut().append_pair("limit", "1000");
        if let Some(after) = &after {
            page_url.query_pairs_mut().append_pair("after_id", after);
        }
        let request = HttpRequest::get(page_url.as_str())
            .header("anthropic-version", "2023-06-01")
            .header("x-api-key", api_key.expose());
        let page = fetch_json(net, request).await?;
        let entries = data_of(&page)?;
        models.extend(entries.iter().filter_map(listed));
        let next = page
            .get("last_id")
            .and_then(Value::as_str)
            .or_else(|| entries.last()?.get("id")?.as_str())
            .map(str::to_string);
        let has_more = page.get("has_more").and_then(Value::as_bool) == Some(true);
        if !has_more || next.is_none() || next == after {
            return Ok(Listing {
                models,
                truncated: false,
            });
        }
        after = next;
    }
    Ok(Listing {
        models,
        truncated: true,
    })
}

async fn list_openai(
    url: &Url,
    api_key: &Secret,
    net: &dyn Network,
) -> Result<Listing, ListModelsError> {
    let request = HttpRequest::get(url.as_str())
        .header("authorization", format!("Bearer {}", api_key.expose()));
    let body = fetch_json(net, request).await?;
    Ok(Listing {
        models: data_of(&body)?.iter().filter_map(listed).collect(),
        truncated: false,
    })
}

async fn fetch_json(net: &dyn Network, request: HttpRequest) -> Result<Value, ListModelsError> {
    let request = request.max_bytes(MAX_PAGE_BYTES);
    let url = request.url.clone();
    let response = net
        .send(request, Origin::Operator)
        .await
        .map_err(|e| ModelError::from(AnthropicError::from(e)))?;
    refuse_failure(&url, &response)?;
    serde_json::from_slice(&response.body).map_err(|_| {
        ListModelsError::Unsupported(format!("{url} did not answer with a JSON model list"))
    })
}

fn refuse_failure(url: &str, response: &HttpResponse) -> Result<(), ListModelsError> {
    let status = response.status;
    if (200..300).contains(&status) {
        return Ok(());
    }
    let body = response.body_text();
    let said = truncated(&service_message(&body));
    if matches!(status, 404 | 405 | 501) {
        return Err(ListModelsError::Unsupported(format!(
            "{url} answered HTTP {status}: {said}"
        )));
    }
    let kind = classify(Some(status), error_type(&body).as_deref(), &body);
    Err(ModelError::new(kind, said).with_status(status).into())
}

fn data_of(body: &Value) -> Result<&Vec<Value>, ListModelsError> {
    body.get("data").and_then(Value::as_array).ok_or_else(|| {
        ListModelsError::Unsupported("the endpoint's answer has no `data` list of models".into())
    })
}

/// One entry of a listing, with the fields the endpoint actually gave. The
/// alternatives are the names the same fact goes by on different endpoints:
/// Anthropic's own, OpenRouter's, vLLM's.
fn listed(entry: &Value) -> Option<ListedModel> {
    let id = entry.get("id")?.as_str()?.to_string();
    let first_u64 = |paths: &[&[&str]]| paths.iter().find_map(|p| at(entry, p)?.as_u64());
    let reasoning = entry.get("reasoning").and_then(Value::as_bool).or_else(|| {
        let params = entry.get("supported_parameters")?.as_array()?;
        Some(params.iter().any(|p| p.as_str() == Some("reasoning")))
    });
    Some(ListedModel {
        id,
        display_name: entry
            .get("display_name")
            .and_then(Value::as_str)
            .map(str::to_string),
        context_window: first_u64(&[
            &["context_window"],
            &["max_input_tokens"],
            &["context_length"],
            &["max_model_len"],
        ]),
        max_output: first_u64(&[
            &["max_output"],
            &["max_tokens"],
            &["max_output_tokens"],
            &["top_provider", "max_completion_tokens"],
        ])
        .and_then(|n| u32::try_from(n).ok()),
        input_types: [
            &["input_types"][..],
            &["input_modalities"],
            &["architecture", "input_modalities"],
        ]
        .iter()
        .find_map(|p| strings(at(entry, p)?)),
        reasoning,
    })
}

fn at<'a>(value: &'a Value, path: &[&str]) -> Option<&'a Value> {
    path.iter().try_fold(value, |v, key| v.get(key))
}

fn strings(value: &Value) -> Option<Vec<String>> {
    value
        .as_array()?
        .iter()
        .map(|v| v.as_str().map(str::to_string))
        .collect()
}

fn truncated(message: &str) -> String {
    match message.char_indices().nth(MESSAGE_CHARS) {
        Some((at, _)) => format!("{}…", &message[..at]),
        None => message.to_string(),
    }
}

/// `…/v1/models` beside the Messages endpoint `base` resolves to.
fn anthropic_models_url(base: &str) -> Result<Url, ListModelsError> {
    let base = Url::parse(base)
        .map_err(|e| ModelError::invalid_request(format!("invalid base_url '{base}': {e}")))?;
    let messages = crate::client::messages_url(&base).map_err(ModelError::from)?;
    let mut url = messages.clone();
    let path = messages.path().trim_end_matches("/messages").to_string();
    url.set_path(&format!("{path}/models"));
    Ok(url)
}

/// `…/models` beside the endpoint an OpenAI-shaped client posts to, which a
/// `base_url` names the way [`crate::openai`] reads it: a host, a `/v1`, or
/// the full endpoint ending in `endpoint`.
fn openai_models_url(base: &str, endpoint: &str) -> Result<Url, ModelError> {
    let trimmed = base.trim().trim_end_matches('/');
    let root = trimmed.strip_suffix(endpoint).unwrap_or(trimmed);
    let full = if root.ends_with("/v1") {
        format!("{root}/models")
    } else {
        format!("{root}/v1/models")
    };
    Url::parse(&full)
        .map_err(|e| ModelError::invalid_request(format!("invalid base_url '{base}': {e}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_models_endpoint_sits_beside_the_one_the_client_posts_to() {
        for base in [
            "https://h",
            "https://h/",
            "https://h/v1",
            "https://h/v1/messages",
        ] {
            assert_eq!(
                anthropic_models_url(base).unwrap().as_str(),
                "https://h/v1/models",
                "{base}"
            );
        }
        assert_eq!(
            anthropic_models_url("https://gw/anthropic")
                .unwrap()
                .as_str(),
            "https://gw/anthropic/v1/models"
        );
        for base in [
            "https://h",
            "https://h/v1/",
            "https://h/v1/chat/completions",
        ] {
            assert_eq!(
                openai_models_url(base, "/chat/completions")
                    .unwrap()
                    .as_str(),
                "https://h/v1/models",
                "{base}"
            );
        }
        assert_eq!(
            openai_models_url("https://h/v1/responses", "/responses")
                .unwrap()
                .as_str(),
            "https://h/v1/models"
        );
    }

    #[test]
    fn a_listing_keeps_only_what_the_endpoint_gave() {
        let bare = listed(&serde_json::json!({"id": "gpt-x", "object": "model"})).unwrap();
        assert_eq!(
            bare,
            ListedModel {
                id: "gpt-x".into(),
                ..Default::default()
            }
        );
        let rich = listed(&serde_json::json!({
            "id": "vendor/m",
            "context_length": 131072,
            "top_provider": {"max_completion_tokens": 8192},
            "architecture": {"input_modalities": ["text", "image"]},
            "supported_parameters": ["tools", "reasoning"],
        }))
        .unwrap();
        assert_eq!(rich.context_window, Some(131_072));
        assert_eq!(rich.max_output, Some(8192));
        assert_eq!(
            rich.input_types,
            Some(vec!["text".to_string(), "image".to_string()])
        );
        assert_eq!(rich.reasoning, Some(true));
        assert!(listed(&serde_json::json!({"object": "model"})).is_none());
    }
}
