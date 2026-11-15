//! The response side of recording one call: a stream that forwards the
//! model's events while writing them, and the `end` record that closes the
//! call however the stream stops.

use base::interface::model::{ModelError, ModelEvent, ModelStream, Usage};
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};

use super::format::{now_ms, ChunkRecord, EndRecord, Outcome, Record, RecordedError};
use super::pack::{self, RunPacker};
use super::writer::RecordingWriter;

pub(super) fn write_end(
    writer: &RecordingWriter,
    call_seq: u64,
    outcome: Outcome,
    stop_reason: Option<String>,
    usage: Option<Usage>,
    started: std::time::Instant,
) {
    let record = Record::End(EndRecord {
        seq: writer.next_seq(),
        ts: now_ms(),
        call: call_seq,
        outcome,
        stop_reason,
        usage,
        duration_ms: started.elapsed().as_millis() as u64,
    });
    if let Err(e) = writer.append(&record).and_then(|()| writer.flush()) {
        tracing::warn!(error = %e, "recorder: failed to write end record");
    }
}

/// Forwards the model's stream downstream while writing every event.
///
/// Recording happens on the way through rather than after collection: buffering
/// the whole response before returning it would make the recorder visible in
/// the agent's own latency, and would lose everything if the process died
/// mid-stream.
pub(super) struct RecordingStream {
    pub(super) inner: ModelStream,
    pub(super) writer: Arc<RecordingWriter>,
    pub(super) packer: RunPacker,
    pub(super) call_seq: u64,
    pub(super) started: std::time::Instant,
    pub(super) stop_reason: String,
    pub(super) usage: Usage,
    pub(super) finished: bool,
}

impl RecordingStream {
    fn write(&mut self, mut records: Vec<Record>) {
        for record in &mut records {
            let from = self.writer.next_seq_block(pack::seq_width(record));
            pack::stamp_seqs(record, from);
            if let Err(e) = self.writer.append(record) {
                tracing::warn!(error = %e, "recorder: failed to write chunk");
                return;
            }
        }
    }

    fn finish(&mut self, outcome: Outcome) {
        if self.finished {
            return;
        }
        self.finished = true;
        let tail = self.packer.flush();
        self.write(tail);
        let stop_reason = (!self.stop_reason.is_empty()).then(|| self.stop_reason.clone());
        write_end(
            &self.writer,
            self.call_seq,
            outcome,
            stop_reason,
            Some(self.usage),
            self.started,
        );
    }
}

impl futures::Stream for RecordingStream {
    type Item = Result<ModelEvent, ModelError>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = self.get_mut();
        match Pin::new(&mut this.inner).poll_next(cx) {
            Poll::Ready(Some(Ok(event))) => {
                if let ModelEvent::EndTurn { stop_reason, usage } = &event {
                    this.stop_reason = stop_reason.clone();
                    this.usage = *usage;
                }
                let records = this.packer.push(ChunkRecord {
                    // Numbered on the way out, not here — see `pack::stamp_seqs`.
                    seq: 0,
                    ts: now_ms(),
                    call: this.call_seq,
                    chunk: event.clone(),
                });
                this.write(records);
                Poll::Ready(Some(Ok(event)))
            }
            Poll::Ready(Some(Err(e))) => {
                this.finish(Outcome::Error {
                    error: RecordedError::from(&e),
                });
                Poll::Ready(Some(Err(e)))
            }
            Poll::Ready(None) => {
                this.finish(Outcome::Ok);
                Poll::Ready(None)
            }
            Poll::Pending => Poll::Pending,
        }
    }
}

/// A stream dropped before its final event was cancelled — the turn was
/// interrupted, or a caller stopped reading. One dropped after it had ended
/// the call: a caller that stops at `EndTurn` without polling for the end of
/// the stream got the whole answer. Closing the call here is what keeps every
/// `call` record paired with an `end`.
impl Drop for RecordingStream {
    fn drop(&mut self) {
        let answered = !self.stop_reason.is_empty();
        self.finish(if answered {
            Outcome::Ok
        } else {
            Outcome::Cancelled
        });
    }
}
