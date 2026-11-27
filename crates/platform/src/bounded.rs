//! Running a process and keeping at most so much of its output: for a
//! program whose output size nobody vouches for, started on any `Process`.

use base::interface::exec::{
    ExecError, ExitStatus, OutputChunk, OutputStream, Process, ProcessHandle, ProcessSpec,
};
use futures::StreamExt;
use tokio_util::sync::CancellationToken;

/// What [`capture_bounded`] kept.
#[derive(Debug)]
pub struct BoundedCapture {
    pub status: ExitStatus,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
    /// Either stream printed more than the limit; the rest was read and
    /// dropped.
    pub truncated: bool,
}

/// [`base::interface::exec::capture`], keeping at most `limit` bytes of each stream, and done once
/// the process has exited: see [`drain_bounded`].
pub async fn capture_bounded(
    process: &dyn Process,
    spec: ProcessSpec,
    cancel: CancellationToken,
    limit: usize,
) -> Result<BoundedCapture, ExecError> {
    let child = process.spawn(spec, cancel).await?;
    let (kept, _) = drain_bounded(child, limit, std::future::pending::<()>()).await?;
    Ok(kept)
}

/// Read a started process's output, at most `limit` bytes of each stream,
/// until it exits. When `stop` resolves first the process and its group are
/// stopped, and what `stop` gave is handed back beside the output. Output
/// still arriving after the exit, from something it left running with its
/// pipes, is read for [`OUTPUT_GRACE_AFTER_EXIT`](base::interface::exec::OUTPUT_GRACE_AFTER_EXIT)
/// and then left, and whatever of its group is still running is stopped.
pub async fn drain_bounded<T>(
    mut child: Box<dyn ProcessHandle>,
    limit: usize,
    stop: impl std::future::Future<Output = T>,
) -> Result<(BoundedCapture, Option<T>), ExecError> {
    let mut chunks = child
        .output()
        .unwrap_or_else(|| futures::stream::empty().boxed());
    let mut kept = BoundedCapture {
        status: ExitStatus {
            code: None,
            signal: None,
            success: false,
        },
        stdout: Vec::new(),
        stderr: Vec::new(),
        truncated: false,
    };
    let mut stop = std::pin::pin!(stop);
    let mut stopped = None;
    let mut open = true;
    // The output is read while waiting, not before: the pipes stay open as
    // long as anything the process started holds them, which is not the
    // process's own lifetime.
    let status = loop {
        let ended = {
            let wait = child.wait();
            tokio::pin!(wait);
            loop {
                tokio::select! {
                    status = &mut wait => break Ok(status?),
                    chunk = chunks.next(), if open => match chunk {
                        Some(chunk) => kept.keep(chunk?, limit),
                        None => open = false,
                    },
                    why = &mut stop, if stopped.is_none() => break Err(why),
                }
            }
        };
        match ended {
            Ok(status) => break status,
            Err(why) => {
                stopped = Some(why);
                child.kill().await;
            }
        }
    };
    let _ = tokio::time::timeout(base::interface::exec::OUTPUT_GRACE_AFTER_EXIT, async {
        while let Some(Ok(chunk)) = chunks.next().await {
            kept.keep(chunk, limit);
        }
    })
    .await;
    child.kill().await;
    kept.status = status;
    Ok((kept, stopped))
}

impl BoundedCapture {
    fn keep(&mut self, chunk: OutputChunk, limit: usize) {
        let into = match chunk.stream {
            OutputStream::Stdout => &mut self.stdout,
            OutputStream::Stderr => &mut self.stderr,
        };
        let room = limit.saturating_sub(into.len());
        if chunk.bytes.len() > room {
            self.truncated = true;
        }
        into.extend_from_slice(&chunk.bytes[..chunk.bytes.len().min(room)]);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use base::interface::exec::in_process::{ScriptedProcess, ScriptedRun};

    #[tokio::test]
    async fn a_bounded_capture_keeps_the_limit_of_each_stream_and_says_so() {
        let process = ScriptedProcess::new().with(
            "gen",
            ScriptedRun {
                stdout: "0123456789".into(),
                stderr: "ab".into(),
                code: 0,
            },
        );
        let out = capture_bounded(
            &process,
            ProcessSpec::new("gen", "/"),
            CancellationToken::new(),
            4,
        )
        .await
        .unwrap();
        assert_eq!(out.stdout, b"0123");
        assert_eq!(out.stderr, b"ab");
        assert!(out.truncated);
        assert!(out.status.success);

        let whole = capture_bounded(
            &process,
            ProcessSpec::new("gen", "/"),
            CancellationToken::new(),
            10,
        )
        .await
        .unwrap();
        assert_eq!(whole.stdout, b"0123456789");
        assert!(!whole.truncated);
    }
}
