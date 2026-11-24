//! NativeSearchProvider —— 让模型提供方自己搜索。
//!
//! `WebSearch` 默认的后端：发一次带 `StreamParams::provider_search` 的调用，
//! 收集提供方找到的页面（`ModelEvent::SearchResults`）与它的说明文字。走的是
//! 会话自己的 `Model`，所以跟着会话选的提供方走，也照常进录像与用量统计；
//! 没有自带搜索的提供方会拒绝这次调用，错误原样交给模型。

use crate::web_search::{
    SearchError, SearchOutput, SearchOutputItem, SearchProvider, SearchResult,
};
use async_trait::async_trait;
use base::interface::model::ThinkingMode;
use base::interface::model::{
    call_purpose, CallOrigin, MessageRole, Model, ModelContentBlock, ModelEvent, ModelMessage,
    ProviderSearch, StreamParams,
};
use base::prompt::PromptBlock;
use futures::stream::StreamExt;
use std::sync::Arc;
use tokio_util::sync::CancellationToken;

/// The most searches one `WebSearch` call lets the provider make.
const MAX_SEARCHES: usize = 8;

/// The model name a search asks for, read at each search: the session's own
/// follows the model the session was switched to since.
pub type ModelName = Arc<dyn Fn() -> String + Send + Sync>;

pub struct NativeSearchProvider {
    model: Arc<dyn Model>,
    model_name: ModelName,
}

impl std::fmt::Debug for NativeSearchProvider {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("NativeSearchProvider")
            .field("model_name", &(self.model_name)())
            .finish_non_exhaustive()
    }
}

impl NativeSearchProvider {
    pub fn new(model: Arc<dyn Model>, model_name: impl Into<String>) -> Self {
        let name = model_name.into();
        Self::following(model, Arc::new(move || name.clone()))
    }

    pub fn following(model: Arc<dyn Model>, model_name: ModelName) -> Self {
        Self { model, model_name }
    }
}

#[async_trait]
impl SearchProvider for NativeSearchProvider {
    async fn search(
        &self,
        query: &str,
        max_results: usize,
        session_id: &str,
    ) -> Result<SearchOutput, SearchError> {
        let params = StreamParams {
            model: (self.model_name)(),
            max_tokens: 8192,
            thinking_mode: ThinkingMode::Off,
            effort: None,
            cache_edits: Vec::new(),
            origin: Some(CallOrigin::auxiliary(session_id, call_purpose::WEB_SEARCH)),
            input_map: None,
            provider_search: Some(ProviderSearch {
                max_uses: max_results.clamp(1, MAX_SEARCHES) as u32,
            }),
            usage_tap: Default::default(),
        };
        let mut stream = self
            .model
            .stream(
                vec![PromptBlock::system(
                    "You are an assistant for performing a web search tool use.",
                )],
                Vec::new(),
                vec![ModelMessage {
                    role: MessageRole::User,
                    content: vec![ModelContentBlock::Text {
                        text: format!("Perform a web search for the query: {query}"),
                    }],
                }],
                params,
                CancellationToken::new(),
            )
            .await
            .map_err(|e| SearchError::Other(anyhow::Error::new(e)))?;

        let mut items: Vec<SearchOutputItem> = Vec::new();
        while let Some(event) = stream.next().await {
            match event.map_err(|e| SearchError::Other(anyhow::Error::new(e)))? {
                ModelEvent::SearchResults { hits } => {
                    items.push(SearchOutputItem::Links(
                        hits.into_iter()
                            .map(|hit| SearchResult {
                                title: hit.title,
                                url: hit.url,
                                snippet: String::new(),
                            })
                            .collect(),
                    ));
                }
                ModelEvent::TextDelta { text }
                | ModelEvent::ContentBlockStart {
                    block: ModelContentBlock::Text { text },
                    ..
                } => match items.last_mut() {
                    Some(SearchOutputItem::Text(last)) => last.push_str(&text),
                    _ if text.is_empty() => {}
                    _ => items.push(SearchOutputItem::Text(text)),
                },
                ModelEvent::EndTurn { .. } => break,
                _ => {}
            }
        }
        if !items
            .iter()
            .any(|item| matches!(item, SearchOutputItem::Links(_)))
        {
            return Err(not_searched(&items));
        }
        Ok(SearchOutput { items })
    }
}

/// What a search call that came back without searching is reported as: its
/// text is the model's own knowledge, not something found on the web.
fn not_searched(items: &[SearchOutputItem]) -> SearchError {
    let said: String = items
        .iter()
        .filter_map(|item| match item {
            SearchOutputItem::Text(text) => Some(text.trim()),
            SearchOutputItem::Links(_) => None,
        })
        .collect::<Vec<_>>()
        .join(" ");
    let mut message =
        "the provider answered without searching the web, so there are no results".to_string();
    if !said.is_empty() {
        message.push_str(&format!("; it said: {said}"));
    }
    SearchError::Other(anyhow::anyhow!(message))
}

#[cfg(test)]
mod tests {
    use super::*;
    use base::interface::model::{ModelError, ModelStream, SearchHit, ToolDef};

    /// Answers with `events`, keeping the params it was called with.
    struct SearchingModel {
        events: Vec<ModelEvent>,
        refuse: bool,
        asked: std::sync::Mutex<Vec<StreamParams>>,
    }

    impl SearchingModel {
        fn answering(events: Vec<ModelEvent>) -> Arc<Self> {
            Arc::new(Self {
                events,
                refuse: false,
                asked: Default::default(),
            })
        }
    }

    #[async_trait]
    impl Model for SearchingModel {
        fn api_type(&self) -> base::provider::ApiType {
            base::provider::ApiType::Anthropic
        }
        async fn stream(
            &self,
            _prompt_blocks: Vec<PromptBlock>,
            tools: Vec<ToolDef>,
            _messages: Vec<ModelMessage>,
            params: StreamParams,
            _cancel: CancellationToken,
        ) -> Result<ModelStream, ModelError> {
            assert!(tools.is_empty(), "a search call offers no tools of its own");
            self.asked.lock().unwrap().push(params);
            if self.refuse {
                return Err(ModelError::unsupported("no search here"));
            }
            Ok(Box::new(futures::stream::iter(
                self.events.clone().into_iter().map(Ok),
            )))
        }
    }

    fn hits(pages: &[(&str, &str)]) -> ModelEvent {
        ModelEvent::SearchResults {
            hits: pages
                .iter()
                .map(|(title, url)| SearchHit {
                    title: title.to_string(),
                    url: url.to_string(),
                })
                .collect(),
        }
    }

    fn end() -> ModelEvent {
        ModelEvent::EndTurn {
            stop_reason: "end_turn".into(),
            usage: Default::default(),
        }
    }

    #[tokio::test]
    async fn the_pages_and_what_the_provider_said_come_back_in_order() {
        let model = SearchingModel::answering(vec![
            ModelEvent::TextDelta {
                text: "Searching. ".into(),
            },
            hits(&[
                ("Rust Lang", "https://rust-lang.org"),
                ("Learn Rust", "https://learn-rust.org"),
            ]),
            ModelEvent::TextDelta {
                text: "Rust is a systems ".into(),
            },
            ModelEvent::TextDelta {
                text: "programming language.".into(),
            },
            end(),
        ]);
        let output = NativeSearchProvider::new(model, "m")
            .search("rust", 10, "s-1")
            .await
            .unwrap();
        assert!(matches!(&output.items[0], SearchOutputItem::Text(t) if t == "Searching. "));
        let SearchOutputItem::Links(links) = &output.items[1] else {
            panic!("{:?}", output.items);
        };
        assert_eq!(links[1].url, "https://learn-rust.org");
        assert!(matches!(
            &output.items[2],
            SearchOutputItem::Text(t) if t == "Rust is a systems programming language."
        ));
    }

    #[tokio::test]
    async fn the_call_asks_the_provider_to_search_on_the_sessions_behalf() {
        let model = SearchingModel::answering(vec![hits(&[]), end()]);
        NativeSearchProvider::new(model.clone(), "the-session-model")
            .search("anything", 25, "s-7")
            .await
            .unwrap();
        let asked = model.asked.lock().unwrap();
        let params = &asked[0];
        assert_eq!(params.model, "the-session-model");
        assert_eq!(
            params.provider_search,
            Some(ProviderSearch {
                max_uses: MAX_SEARCHES as u32
            })
        );
        assert_eq!(params.thinking_mode, ThinkingMode::Off);
        let origin = params.origin.as_ref().expect("attributed");
        assert_eq!(origin.session_id, "s-7");
        assert_eq!(origin.purpose(), Some(call_purpose::WEB_SEARCH));
    }

    #[tokio::test]
    async fn an_answer_without_a_search_is_reported_as_no_search() {
        let model = SearchingModel::answering(vec![
            ModelEvent::TextDelta {
                text: "Rust 1.80 came out in July.".into(),
            },
            end(),
        ]);
        let err = NativeSearchProvider::new(model, "m")
            .search("rust", 5, "s")
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("without searching the web"), "{err}");
        assert!(err.contains("Rust 1.80 came out in July."), "{err}");
    }

    #[tokio::test]
    async fn a_provider_without_search_says_so() {
        let model = Arc::new(SearchingModel {
            events: Vec::new(),
            refuse: true,
            asked: Default::default(),
        });
        let err = NativeSearchProvider::new(model, "m")
            .search("anything", 5, "s")
            .await
            .unwrap_err();
        assert!(err.to_string().contains("no search here"), "{err}");
    }
}
