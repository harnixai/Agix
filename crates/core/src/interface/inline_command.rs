//! A skill's `` !`cmd` ``, run as a `Bash` call of the session — through the
//! same dispatch a model's call takes — with nobody to ask about it.

use tokio_util::sync::CancellationToken;

use crate::interface::tool_dispatch::SubCallAnswer;

#[async_trait::async_trait]
pub trait InlineCommands: Send + Sync {
    /// `Err` is the call not producing an answer: unknown tool, refused,
    /// invalid, or failed; the text says which.
    async fn run_bash(
        &self,
        command: &str,
        cancel: CancellationToken,
    ) -> Result<SubCallAnswer, String>;
}
