//! `session.rerunCall`：把会话录像里的一次调用读回来，套上宿主的改动，经会话所在 `(项目, 场景)`
//! 对的 provider 路由与配额重发，答新应答与和录下那次的差异。只读录像：不写录像、不动会话、
//! 不记进会话的花费；会话不在内存时同样能跑，也不把它带回内存。

use super::recording_rpc::{expect_generation, not_recorded};
use crate::recording_view::{self, RerunOf};
use crate::session_pool::*;
use telemetry::recorder::rerun::{self, RerunError};
use tokio_util::sync::CancellationToken;

impl SessionPool {
    pub async fn rerun_call(
        &self,
        request: api::RerunRequest,
        cancel: CancellationToken,
    ) -> Result<api::RerunResult, PoolError> {
        let session_id = request.session_id.clone();
        let (index, expected, edits) = (request.index, request.generation, request.edits);
        let (mut loaded, generation) = self
            .read_recording(&session_id, move |opened, _| {
                expect_generation(opened, expected)?;
                let mut loaded = rerun::request_in(&opened.recording, &opened.blobs, index)
                    .map_err(unrunnable)?;
                recording_view::apply_edits(&mut loaded, &edits)
                    .map_err(PoolError::InvalidRequest)?;
                Ok((loaded, opened.generation()))
            })
            .await?
            .ok_or_else(|| not_recorded(&session_id))?;

        let binding = self
            .binding_of_session(&session_id)
            .await
            .ok_or_else(|| PoolError::session_not_found(&session_id))?;
        let provider = request
            .provider
            .unwrap_or_else(|| loaded.record.provider.clone());
        let model_name = request.model.unwrap_or_else(|| loaded.params.model.clone());
        let model = routed_model(&binding, &provider, &model_name)?;
        loaded.params.model = model_name;

        let recorded_usage = recording_view::end_usage(&loaded.recorded_response);
        let rerun = rerun::rerun_one(&model, loaded, cancel).await;
        Ok(recording_view::rerun_result(
            RerunOf {
                session_id: &session_id,
                index,
                generation,
                recorded_usage,
            },
            &rerun,
        ))
    }
}

/// The model a rerun goes through: the pair's own routing, starting at
/// `provider`, metered against the pair's quotas and nobody's budget.
fn routed_model(
    binding: &ProjectBinding,
    provider: &str,
    model: &str,
) -> Result<Arc<dyn base::interface::model::Model>, PoolError> {
    if let Some(refusal) = &binding.routing.refusal {
        return Err(PoolError::InvalidRequest(refusal.clone()));
    }
    binding
        .check_model_choice(Some(provider), Some(model))
        .map_err(PoolError::InvalidRequest)?;
    let router = binding
        .task_router
        .session(Some(provider))
        .map_err(PoolError::InvalidRequest)?;
    Ok(base::provider::TaskRouting::model_for(
        &router,
        base::provider::TaskTiers::MAIN,
    ))
}

fn unrunnable(e: RerunError) -> PoolError {
    match e {
        RerunError::NoSuchCall { index, len } => PoolError::InvalidRequest(format!(
            "no call at index {index}: the recording holds {len}"
        )),
        other => PoolError::InvalidRequest(format!("the call cannot be sent again: {other}")),
    }
}
