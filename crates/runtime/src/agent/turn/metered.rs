//! A session's model when nothing routes it, metered as a routed call is:
//! what its calls spend is charged where their tap says. Without it, a
//! session built on a bare client would count nothing.

use std::sync::Arc;

use base::interface::model::{Model, ModelError, ModelMessage, ModelStream, StreamParams, ToolDef};
use base::prompt::PromptBlock;
use base::provider::ApiType;
use tokio_util::sync::CancellationToken;

pub(crate) struct Metered(pub(crate) Arc<dyn Model>);

#[async_trait::async_trait]
impl Model for Metered {
    fn api_type(&self) -> ApiType {
        self.0.api_type()
    }

    fn provenance(&self) -> Option<String> {
        self.0.provenance()
    }

    async fn stream(
        &self,
        prompt_blocks: Vec<PromptBlock>,
        tools: Vec<ToolDef>,
        messages: Vec<ModelMessage>,
        mut params: StreamParams,
        cancel: CancellationToken,
    ) -> Result<ModelStream, ModelError> {
        let tap = params.usage_tap.for_attempt();
        params.usage_tap = tap.clone();
        let stream = self
            .0
            .stream(prompt_blocks, tools, messages, params, cancel)
            .await?;
        Ok(tap.metered(stream, None))
    }
}
