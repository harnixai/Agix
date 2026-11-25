//! 会话日志：`session.history` / `appendEntry` / `entries` / `export` / `import`；
//! 录像：`session.calls` / `call` / `callBlob` / `rerunCall`；内容搜索：`session.search`。

use super::*;
use crate::rpc::methods::history::{
    AppendEntryParams, CallBlobParams, CallParams, CallsParams, EntriesParams, ExportParams,
    HistoryParams, ImportParams, RerunCallParams, SearchParams, SystemEditParam,
};
use crate::session_pool::api::{
    BlockSelector, CallBlob, CallDetail, CallList, EntryList, EntryView, ExportSummary,
    HistoryPage, ImportSummary, RerunEdits, RerunRequest, RerunResult, SearchRequest, SearchResult,
    SystemEdit,
};
use tokio_util::sync::CancellationToken;

rpc_methods! {
    "session.history" => history(HistoryParams) -> HistoryPage;
    "session.appendEntry" => append_entry(AppendEntryParams) -> EntryView;
    "session.entries" => entries(EntriesParams) -> EntryList;
    "session.export" => export(ExportParams) -> ExportSummary;
    "session.import" => import(ImportParams) -> ImportSummary;
    "session.calls" => calls(CallsParams) -> CallList;
    "session.call" => call(CallParams) -> CallDetail;
    "session.callBlob" => call_blob(CallBlobParams) -> CallBlob;
    "session.rerunCall" => rerun_call(RerunCallParams) -> RerunResult;
    "session.search" => search(SearchParams) -> SearchResult;
}

/// Page size when the caller does not say: big enough for a typical
/// conversation in one round trip, small enough that a forgotten `limit`
/// cannot turn one RPC into a multi-megabyte line.
const HISTORY_DEFAULT_LIMIT: usize = 100;

/// A request for more is clamped, not refused: the page echoes the `limit`
/// applied plus `total` and `has_more`, so a caller can tell and page on.
const HISTORY_MAX_LIMIT: usize = 500;

async fn history(
    s: &DaemonServer,
    _: &Call,
    params: HistoryParams,
) -> Result<HistoryPage, PoolError> {
    let offset = params.offset.min(usize::MAX as u64) as usize;
    let limit = params
        .limit
        .map(|n| n.min(HISTORY_MAX_LIMIT as u64) as usize)
        .unwrap_or(HISTORY_DEFAULT_LIMIT);
    s.pool
        .session_history(&params.session_id, offset, limit)
        .await
}

async fn append_entry(
    s: &DaemonServer,
    _: &Call,
    params: AppendEntryParams,
) -> Result<EntryView, PoolError> {
    s.pool
        .append_entry(
            &params.session_id,
            &params.ns,
            &params.event,
            params.payload,
        )
        .await
}

async fn entries(
    s: &DaemonServer,
    _: &Call,
    params: EntriesParams,
) -> Result<EntryList, PoolError> {
    s.pool
        .list_entries(&params.session_id, params.namespaces.as_deref())
        .await
}

async fn export(
    s: &DaemonServer,
    _: &Call,
    params: ExportParams,
) -> Result<ExportSummary, PoolError> {
    let include = match params.include {
        None => crate::export::Include::default(),
        Some(names) => crate::export::Include::parse(&names).map_err(PoolError::InvalidRequest)?,
    };
    s.pool
        .export_session(crate::export::ExportRequest {
            session_id: params.session_id,
            path: params.path,
            format: params.format,
            include,
        })
        .await
}

async fn import(
    s: &DaemonServer,
    _: &Call,
    params: ImportParams,
) -> Result<ImportSummary, PoolError> {
    s.pool
        .import_session(crate::import::ImportRequest {
            path: params.path,
            project_root: params.project_root,
            keep_ids: params.keep_ids,
        })
        .await
}

const CALLS_DEFAULT_LIMIT: u64 = 200;
const CALLS_MAX_LIMIT: u64 = 500;

async fn calls(s: &DaemonServer, _: &Call, params: CallsParams) -> Result<CallList, PoolError> {
    let limit = params
        .limit
        .unwrap_or(CALLS_DEFAULT_LIMIT)
        .min(CALLS_MAX_LIMIT);
    s.pool
        .session_calls(
            &params.session_id,
            clamp(params.from, u64::MAX),
            clamp(limit, CALLS_MAX_LIMIT),
            params.generation,
        )
        .await
}

async fn call(s: &DaemonServer, _: &Call, params: CallParams) -> Result<CallDetail, PoolError> {
    s.pool
        .session_call(
            &params.session_id,
            clamp(params.index, u64::MAX),
            params.generation,
        )
        .await
}

async fn call_blob(
    s: &DaemonServer,
    _: &Call,
    params: CallBlobParams,
) -> Result<CallBlob, PoolError> {
    s.pool
        .session_call_blob(&params.session_id, &params.blob)
        .await
}

async fn rerun_call(
    s: &DaemonServer,
    cx: &Call,
    params: RerunCallParams,
) -> Result<RerunResult, PoolError> {
    let request = rerun_request(params)?;
    let cancel = CancellationToken::new();
    let running = s.pool.rerun_call(request, cancel.clone());
    tokio::pin!(running);
    tokio::select! {
        result = &mut running => result,
        _ = cx.client.closed() => {
            cancel.cancel();
            running.await
        }
    }
}

fn rerun_request(params: RerunCallParams) -> Result<RerunRequest, PoolError> {
    let edits = params.edits.unwrap_or_default();
    let system = edits
        .system
        .into_iter()
        .enumerate()
        .map(|(at, edit)| system_edit(at, edit))
        .collect::<Result<_, _>>()?;
    Ok(RerunRequest {
        session_id: params.session_id,
        index: clamp(params.index, u64::MAX),
        generation: params.generation,
        edits: RerunEdits {
            system,
            drop_messages: edits
                .drop_messages
                .into_iter()
                .map(|n| clamp(n, u64::MAX))
                .collect(),
        },
        provider: params.provider,
        model: params.model,
    })
}

fn system_edit(at: usize, edit: SystemEditParam) -> Result<SystemEdit, PoolError> {
    let refuse = |why: &str| PoolError::InvalidRequest(format!("edits.system[{at}]: {why}"));
    let block = match (edit.index, edit.source) {
        (Some(index), None) => BlockSelector::Index(clamp(index, u64::MAX)),
        (None, Some(source)) => BlockSelector::Source(source),
        (Some(_), Some(_)) => return Err(refuse("give `index` or `source`, not both")),
        (None, None) => return Err(refuse("give `index` or `source`")),
    };
    let text = edit.text.ok_or_else(|| {
        refuse("`text` is required: a string replaces the block, null removes it")
    })?;
    Ok(SystemEdit { block, text })
}

async fn search(
    s: &DaemonServer,
    _: &Call,
    params: SearchParams,
) -> Result<SearchResult, PoolError> {
    let bound = |given: Option<u64>, default: u64, max: u64| clamp(given.unwrap_or(default), max);
    s.pool
        .search_sessions(SearchRequest {
            query: params.query,
            session_ids: params.session_ids,
            limit: bound(params.limit, 20, 100).max(1),
            max_sessions: bound(params.max_sessions, 500, 5000),
            deadline: std::time::Duration::from_millis(
                bound(params.deadline_ms, 2000, 10_000) as u64
            ),
            snippet_chars: bound(params.snippet_chars, 120, 500),
        })
        .await
}

fn clamp(n: u64, max: u64) -> usize {
    usize::try_from(n.min(max)).unwrap_or(usize::MAX)
}
