//! The model calls a turn makes on its side — compaction, memory recall and
//! extraction, web search and page extraction — charged to that turn's
//! `SpendNode` and judged by what is left up its chain as its own calls are,
//! so the turn's budget and its reported usage see them. What a call spent
//! is counted where it is routed; this only says where it goes.

use base::event::UsageBreakdown;
use base::interface::budget_policy::SpendNode;
use base::interface::model::{
    Model, ModelError, ModelErrorKind, ModelMessage, ModelStream, StreamParams, ToolDef, Usage,
    UsageTap,
};
use base::prompt::PromptBlock;
use base::provider::ApiType;
use std::sync::{Arc, Mutex};
use tokio_util::sync::CancellationToken;

/// The spend of the turn the session is in; empty between turns. A side call
/// made between turns (`session.compact`, say) is charged to the session's
/// node instead, so `session.token_budget` sees it.
#[derive(Clone, Default)]
pub(crate) struct OpenTurn {
    turn: Arc<Mutex<Option<Arc<SpendNode>>>>,
    session: Option<Arc<SpendNode>>,
}

impl OpenTurn {
    pub(crate) fn within(session: Arc<SpendNode>) -> Self {
        Self {
            turn: Arc::default(),
            session: Some(session),
        }
    }

    pub(crate) fn open(&self, spend: Arc<SpendNode>) {
        *self.turn.lock().unwrap_or_else(|e| e.into_inner()) = Some(spend);
    }

    pub(crate) fn close(&self) {
        *self.turn.lock().unwrap_or_else(|e| e.into_inner()) = None;
    }

    pub(crate) fn spend(&self) -> Option<Arc<SpendNode>> {
        self.turn.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    /// Where a side call made now is charged: the open turn, else the session.
    pub(crate) fn side_call_node(&self) -> Option<Arc<SpendNode>> {
        self.spend().or_else(|| self.session.clone())
    }
}

enum SpentInto {
    Turn(Arc<SpendNode>),
    Open(OpenTurn),
}

/// `inner`, each call it makes charged to a turn unless its caller already
/// charged it somewhere — and then asking for no more output than the turn's
/// chain has left once the call's input is paid, and not sent when that is
/// too little. None of a side call's prompt is assumed cached.
pub(crate) struct SpentModel {
    inner: Arc<dyn Model>,
    into: SpentInto,
}

impl SpentModel {
    /// Calls spent into `turn`, whenever they are made.
    pub(crate) fn into_turn(inner: Arc<dyn Model>, turn: Arc<SpendNode>) -> Arc<dyn Model> {
        Arc::new(Self {
            inner,
            into: SpentInto::Turn(turn),
        })
    }

    /// Calls spent into the turn open when each is made; into the session's
    /// node between turns.
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
        mut params: StreamParams,
        cancel: CancellationToken,
    ) -> Result<ModelStream, ModelError> {
        let spend = match &self.into {
            SpentInto::Turn(turn) => Some(turn.clone()),
            SpentInto::Open(open) => open.side_call_node(),
        };
        if let (Some(spend), None) = (spend, params.usage_tap.charge()) {
            let input = input_tokens(&prompt_blocks, &tools, &messages);
            match super::call_input::headroom(spend.remaining(), input) {
                Ok(Some(left)) => params.max_tokens = params.max_tokens.min(left),
                Ok(None) => {}
                Err(refused) => {
                    return Err(ModelError::new(
                        ModelErrorKind::InvalidRequest,
                        format!("not sent: {}", refused.message()),
                    ))
                }
            }
            params.usage_tap = UsageTap::charging(spend);
        }
        self.inner
            .stream(prompt_blocks, tools, messages, params, cancel)
            .await
    }
}

fn input_tokens(
    prompt_blocks: &[PromptBlock],
    tools: &[ToolDef],
    messages: &[ModelMessage],
) -> u64 {
    let counter = base::interface::token_counter::default_counter();
    let prompt: usize = prompt_blocks
        .iter()
        .map(|block| counter.count_text(&block.content))
        .sum();
    let tools: usize = tools
        .iter()
        .map(|tool| {
            counter.count_text(&tool.description)
                + counter.count_text(&tool.input_schema.to_string())
        })
        .sum();
    (prompt + tools + counter.count_messages(messages)) as u64
}

/// What a turn spent as its `turn_complete` reports it, `own` being its
/// agent's own calls.
pub(crate) fn turn_usage(spend: &SpendNode, own: Usage) -> (Usage, UsageBreakdown) {
    let (all, delegates) = (spend.read().as_usage(), spend.delegated().as_usage());
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

#[cfg(test)]
mod tests {
    use super::*;
    use base::interface::model::ModelEvent;
    use futures::StreamExt as _;

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

    async fn call(model: &Arc<dyn Model>) {
        let mut stream = model
            .stream(vec![], vec![], vec![], params(), CancellationToken::new())
            .await
            .unwrap();
        while stream.next().await.is_some() {}
    }

    #[tokio::test]
    async fn a_side_call_is_spent_into_the_turn_open_when_it_is_made() {
        let open = OpenTurn::default();
        let metered = Arc::new(crate::agent::turn::Metered(Arc::new(OneCall)));
        let model = SpentModel::into_open(metered, open.clone());
        call(&model).await;

        let turn = Arc::new(SpendNode::default());
        open.open(turn.clone());
        call(&model).await;
        open.close();
        call(&model).await;
        assert_eq!(turn.read().counted_tokens(), 34);
    }

    #[test]
    fn what_is_neither_its_own_nor_its_delegates_was_spent_on_its_side() {
        let root = Arc::new(SpendNode::default());
        let child = SpendNode::turn(None, Some(root.clone()));
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
