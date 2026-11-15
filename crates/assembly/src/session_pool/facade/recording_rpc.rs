//! 录像读取 RPC：`session.calls` / `session.call` / `session.callBlob`。只读：会话
//! 不在内存时规格从日志读，不建引擎、不把会话带回内存。

use crate::recording_view::{self, BlobLookup, Opened};
use crate::session_pool::*;

impl SessionPool {
    /// `session.calls`: up to `limit` calls from `from`. `generation`, when
    /// given, must be the recording's current one.
    pub async fn session_calls(
        &self,
        session_id: &str,
        from: usize,
        limit: usize,
        generation: Option<u64>,
    ) -> Result<api::CallList, PoolError> {
        let page = self
            .read_recording(session_id, move |opened, writing| {
                expect_generation(opened, generation)?;
                let (calls, next) = opened.summaries(from, limit, writing);
                Ok((
                    opened.info(),
                    calls,
                    next,
                    opened.recording.calls.len(),
                    opened.recording.damaged,
                ))
            })
            .await?;
        Ok(match page {
            Some((info, calls, next, total, damaged)) => api::CallList {
                session_id: session_id.to_string(),
                recording: Some(info),
                calls,
                next,
                total,
                damaged,
            },
            None => api::CallList {
                session_id: session_id.to_string(),
                recording: None,
                calls: Vec::new(),
                next: 0,
                total: 0,
                damaged: 0,
            },
        })
    }

    /// `session.call`: the call at `index` in full.
    pub async fn session_call(
        &self,
        session_id: &str,
        index: usize,
        generation: Option<u64>,
    ) -> Result<api::CallDetail, PoolError> {
        let sid = session_id.to_string();
        self.read_recording(session_id, move |opened, writing| {
            expect_generation(opened, generation)?;
            opened.detail(&sid, index, writing).ok_or_else(|| {
                PoolError::InvalidRequest(format!(
                    "no call at index {index}: the recording holds {}",
                    opened.recording.calls.len()
                ))
            })
        })
        .await?
        .ok_or_else(|| not_recorded(session_id))
    }

    /// `session.callBlob`: a part `session.call` gave by reference.
    pub async fn session_call_blob(
        &self,
        session_id: &str,
        blob: &str,
    ) -> Result<api::CallBlob, PoolError> {
        let handle = blob.to_string();
        self.read_recording(session_id, move |opened, _| {
            opened.blob(&handle).map_err(|e| match e {
                BlobLookup::Malformed => PoolError::InvalidRequest(format!(
                    "`{handle}` is not a blob reference session.call gave"
                )),
                BlobLookup::Absent => {
                    PoolError::InvalidRequest(format!("the recording holds no blob `{handle}`"))
                }
                BlobLookup::TooLarge(bytes) => PoolError::InvalidRequest(format!(
                    "blob `{handle}` is {bytes} bytes, more than one frame carries"
                )),
            })
        })
        .await?
        .ok_or_else(|| not_recorded(session_id))
    }

    /// Run `read` over the session's recording on a blocking thread; `None`
    /// when the session records nothing or has written nothing yet. `read`
    /// is told whether the session is in memory, where a call without an end
    /// is still streaming rather than cut off.
    async fn read_recording<T: Send + 'static>(
        &self,
        session_id: &str,
        read: impl FnOnce(&Opened, bool) -> Result<T, PoolError> + Send + 'static,
    ) -> Result<Option<T>, PoolError> {
        let Some(dir) = self.recording_dir(session_id).await? else {
            return Ok(None);
        };
        let writing = self.live.contains(session_id).await;
        tokio::task::spawn_blocking(move || {
            match recording_view::open(&dir).map_err(PoolError::Internal)? {
                Some(opened) => read(&opened, writing).map(Some),
                None => Ok(None),
            }
        })
        .await
        .map_err(|e| PoolError::Internal(format!("reading the recording failed: {e}")))?
    }

    /// The directory `session_id` records into, by the spec this process
    /// holds or else the one its log recorded; `None` when it does not record.
    async fn recording_dir(&self, session_id: &str) -> Result<Option<PathBuf>, PoolError> {
        let spec = match self.specs.get(session_id) {
            Some(spec) => spec,
            None => crate::session_pool::parts::specs::recorded_in(
                &self.directory.load_entries(session_id).await?,
            ),
        };
        let settings = self.bindings.pool_settings().await;
        Ok(recording_location(
            spec.options.as_ref().and_then(|o| o.recorder.as_ref()),
            &settings,
            session_id,
        )
        .map(|(root, name)| root.join(name)))
    }
}

fn expect_generation(opened: &Opened, expected: Option<u64>) -> Result<(), PoolError> {
    match expected {
        Some(expected) if expected != opened.generation() => Err(PoolError::RecordingRestarted {
            generation: opened.generation(),
        }),
        _ => Ok(()),
    }
}

fn not_recorded(session_id: &str) -> PoolError {
    PoolError::InvalidRequest(format!("session `{session_id}` has no recording"))
}
