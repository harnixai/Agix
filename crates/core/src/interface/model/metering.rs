//! A call's stream with its usage counted once: into the node its
//! [`UsageTap`] is charged to, and to whatever else the layer that routed the
//! call keeps (a provider's quota). Counted at [`ModelEvent::EndTurn`], or,
//! for a call that broke off, failed or was dropped before it, when the
//! stream goes, with what the client reported so far. The two metering points
//! — the provider router and the wrapper over a model nothing routes — both
//! count through this, so a call is counted the same way wherever it went.

use futures::StreamExt;

use super::{ModelError, ModelEvent, ModelStream, Usage, UsageTap};

/// Told what a metered call used, once.
pub type OnSpent = Box<dyn FnOnce(Usage) + Send>;

impl UsageTap {
    /// `stream`, its usage counted once into the node this tap is charged to
    /// and to `also`.
    pub fn metered(self, stream: ModelStream, also: Option<OnSpent>) -> ModelStream {
        if also.is_none() && self.charge.is_none() {
            return stream;
        }
        Box::new(Metering {
            inner: stream,
            tap: self,
            also,
            counted: false,
        })
    }
}

struct Metering {
    inner: ModelStream,
    tap: UsageTap,
    also: Option<OnSpent>,
    counted: bool,
}

impl Metering {
    fn count_once(&mut self, usage: Usage) {
        if std::mem::replace(&mut self.counted, true) {
            return;
        }
        if let Some(also) = self.also.take() {
            also(usage);
        }
        if let Some(node) = self.tap.charge() {
            node.add(usage);
        }
    }
}

impl futures::Stream for Metering {
    type Item = Result<ModelEvent, ModelError>;

    fn poll_next(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Self::Item>> {
        let polled = self.inner.poll_next_unpin(cx);
        if let std::task::Poll::Ready(Some(Ok(ModelEvent::EndTurn { usage, .. }))) = &polled {
            let usage = *usage;
            self.count_once(usage);
        }
        polled
    }
}

impl Drop for Metering {
    fn drop(&mut self) {
        if let Some(usage) = self.tap.reported() {
            self.count_once(usage);
        }
    }
}
