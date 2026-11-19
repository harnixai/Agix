//! The model calls a turn makes on its side — compaction, memory recall and
//! extraction, web search and page extraction — spent into that turn's
//! `TurnSpend` as its own calls are, so the turn's budget and its reported
//! usage see them.

use base::event::UsageBreakdown;
use base::interface::budget_policy::{Spend, TurnSpend};
use base::interface::model::{
    Model, ModelError, ModelEvent, ModelMessage, ModelStream, StreamParams, ToolDef, Usage,
};
use base::prompt::PromptBlock;
use base::provider::ApiType;
use futures::StreamExt as _;
use std::sync::{Arc, Mutex};
use tokio_util::sync::CancellationToken;

/// The spend of the turn the session is in; empty between turns.
#[derive(Clone, Default)]
pub(crate) struct OpenTurn(Arc<Mutex<Option<Arc<TurnSpend>>>>);

impl OpenTurn {
    pub(crate) fn open(&self, spend: Arc<TurnSpend>) {
        *self.0.lock().unwrap_or_else(|e| e.into_inner()) = Some(spend);
    }

    pub(crate) fn close(&self) {
        *self.0.lock().unwrap_or_else(|e| e.into_inner()) = None;
    }

    pub(crate) fn spend(&self) -> Option<Arc<TurnSpend>> {
        self.0.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }
}

enum SpentInto {
    Turn(Arc<TurnSpend>),
    Open(OpenTurn),
}

/// `inner`, each call it makes spent into a turn.
pub(crate) struct SpentModel {
    inner: Arc<dyn Model>,
    into: SpentInto,
}

impl SpentModel {
    /// Calls spent into `turn`, whenever they are made.
    pub(crate) fn into_turn(inner: Arc<dyn Model>, turn: Arc<TurnSpend>) -> Arc<dyn Model> {
        Arc::new(Self {
            inner,
            into: SpentInto::Turn(turn),
        })
    }

    /// Calls spent into the turn open when each is made; none between turns.
    pub(crate) fn into_open(inner: Arc<dyn Model>, open: OpenTurn) -> Arc<dyn Model> {
        Arc::new(Self {
            inner,
            into: SpentInto::Open(open),
        })
    }
}

#[async_trait::async_trait]
impl Model for SpentModel {
    fn api_type(&self) -> ApiType {
        self.inner.api_type()
    }

    fn provenance(&self) -> Option<String> {
        self.inner.provenance()
    }

    async fn stream(
        &self,
        prompt_blocks: Vec<PromptBlock>,
        tools: Vec<ToolDef>,
        messages: Vec<ModelMessage>,
        params: StreamParams,
        cancel: CancellationToken,
    ) -> Result<ModelStream, ModelError> {
        let spend = match &self.into {
            SpentInto::Turn(turn) => Some(turn.clone()),
            SpentInto::Open(open) => open.spend(),
        };
        let stream = self
            .inner
            .stream(prompt_blocks, tools, messages, params, cancel)
            .await?;
        let Some(spend) = spend else {
            return Ok(stream);
        };
        Ok(Box::new(stream.map(move |item| {
            if let Ok(ModelEvent::EndTurn { usage, .. }) = &item {
                spend.add(*usage);
            }
            item
        })))
    }
}

/// What a turn spent as its `turn_complete` reports it, `own` being its
/// agent's own calls.
pub(crate) fn turn_usage(spend: &TurnSpend, own: Usage) -> (Usage, UsageBreakdown) {
    let (all, delegates) = (usage_of(spend.read()), usage_of(spend.delegated()));
    let less = |a: u32, b: u32, c: u32| a.saturating_sub(b).saturating_sub(c);
    let side = Usage {
        input_tokens: less(all.input_tokens, delegates.input_tokens, own.input_tokens),
        output_tokens: less(
            all.output_tokens,
            delegates.output_tokens,
            own.output_tokens,
        ),
        cache_creation_input_tokens: less(
            all.cache_creation_input_tokens,
            delegates.cache_creation_input_tokens,
            own.cache_creation_input_tokens,
        ),
        cache_read_input_tokens: less(
            all.cache_read_input_tokens,
            delegates.cache_read_input_tokens,
            own.cache_read_input_tokens,
        ),
    };
    let breakdown = UsageBreakdown {
        own,
        side,
        delegates,
    };
    (all, breakdown)
}

fn usage_of(spend: Spend) -> Usage {
    let narrow = |n: u64| u32::try_from(n).unwrap_or(u32::MAX);
    Usage {
        input_tokens: narrow(spend.input_tokens),
        output_tokens: narrow(spend.output_tokens),
        cache_creation_input_tokens: narrow(spend.cache_creation_tokens),
        cache_read_input_tokens: narrow(spend.cache_read_tokens),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct OneCall;

    #[async_trait::async_trait]
    impl Model for OneCall {
        fn api_type(&self) -> ApiType {
            ApiType::Anthropic
        }

        async fn stream(
            &self,
            _: Vec<PromptBlock>,
            _: Vec<ToolDef>,
            _: Vec<ModelMessage>,
            _: StreamParams,
            _: CancellationToken,
        ) -> Result<ModelStream, ModelError> {
            Ok(Box::new(futures::stream::iter([Ok(ModelEvent::EndTurn {
                stop_reason: "end_turn".into(),
                usage: base::interface::model::Usage {
                    input_tokens: 30,
                    output_tokens: 4,
                    ..Default::default()
                },
            })])))
        }
    }

    fn params() -> StreamParams {
        StreamParams {
            model: "m".into(),
            max_tokens: 100,
            thinking_mode: base::interface::model::ThinkingMode::Off,
            effort: None,
            cache_edits: Vec::new(),
            origin: None,
            input_map: None,
            provider_search: None,
            usage_tap: Default::default(),
        }
    }

    async fn call(model: &dyn Model) {
        let mut stream = model
            .stream(vec![], vec![], vec![], params(), CancellationToken::new())
            .await
            .unwrap();
        while stream.next().await.is_some() {}
    }

    #[tokio::test]
    async fn a_side_call_is_spent_into_the_turn_open_when_it_is_made() {
        let open = OpenTurn::default();
        let model = SpentModel::into_open(Arc::new(OneCall), open.clone());
        call(model.as_ref()).await;

        let turn = Arc::new(TurnSpend::default());
        open.open(turn.clone());
        call(model.as_ref()).await;
        open.close();
        call(model.as_ref()).await;
        assert_eq!(turn.read().total_tokens(), 34);
    }

    #[test]
    fn what_is_neither_its_own_nor_its_delegates_was_spent_on_its_side() {
        let root = Arc::new(TurnSpend::default());
        let child = TurnSpend::under(root.clone());
        let call = |n: u32| Usage {
            input_tokens: n,
            ..Default::default()
        };
        root.add(call(5));
        root.add(call(40));
        child.add(call(300));
        let (all, split) = turn_usage(&root, call(5));
        assert_eq!(all.input_tokens, 345);
        assert_eq!(
            (
                split.own.input_tokens,
                split.side.input_tokens,
                split.delegates.input_tokens
            ),
            (5, 40, 300)
        );
    }
}
