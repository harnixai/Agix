//! How the streaming executor stops waiting for a call: a sibling's error
//! abandons it, or its turn is cancelled and the grace given to it passes.

use std::future::Future;
use tokio_util::sync::CancellationToken;

/// The error a concurrent call stands on when a sibling's error abandons it.
pub(crate) const ABANDONED_BY_SIBLING: &str = "cancelled by sibling error";

/// The error a call stands on when its turn was cancelled before it finished.
pub(crate) const CANCELLED_WITH_THE_TURN: &str =
    "cancelled: the turn was stopped before the call finished";

/// The turn's token, and how long a call still running when it fires is
/// given to end on its own before it is dropped.
#[derive(Clone)]
pub(super) struct Stop {
    pub(super) token: CancellationToken,
    pub(super) grace: std::time::Duration,
}

impl Stop {
    async fn past_grace(&self) {
        self.token.cancelled().await;
        tokio::time::sleep(self.grace).await;
    }

    /// `call`'s outcome, or a cancellation once the grace has passed: a tool
    /// that ignores its token cannot hold a cancelled turn open.
    pub(super) async fn bound<Fut>(
        &self,
        call: Fut,
    ) -> base::interface::tool_middleware::ToolOutcome
    where
        Fut: Future<Output = base::interface::tool_middleware::ToolOutcome>,
    {
        tokio::select! {
            biased;
            outcome = call => outcome,
            _ = self.past_grace() => Err(CANCELLED_WITH_THE_TURN.to_string()),
        }
    }

    pub(super) fn abandoned(&self) -> String {
        if self.token.is_cancelled() {
            CANCELLED_WITH_THE_TURN
        } else {
            ABANDONED_BY_SIBLING
        }
        .to_string()
    }
}
