//! `session.search`：按内容找会话。范围缺省是 `session.list` 的（服务过的项目、
//! 不含侧链、最近的在前），给了 `session_ids` 就只在那些里按给的顺序找。只读，
//! 不把会话带回内存。

use crate::session_pool::*;

impl SessionPool {
    pub async fn search_sessions(
        &self,
        request: api::SearchRequest,
    ) -> Result<api::SearchResult, PoolError> {
        let deadline = std::time::Instant::now() + request.deadline;
        let store = self.directory.require_store()?.clone();
        if request.query.trim().is_empty() {
            return Err(PoolError::InvalidRequest("`query` is empty".into()));
        }
        let (mut sessions, include_sidechains) = match &request.session_ids {
            Some(ids) => (named_sessions(ids)?, true),
            None => (self.sessions_in_range(store.as_ref()).await, false),
        };
        let over = sessions.len() > request.max_sessions;
        sessions.truncate(request.max_sessions);
        let found = store
            .search_text(&history::search::TextQuery {
                text: request.query,
                sessions,
                include_sidechains,
                limit: request.limit,
                deadline: Some(deadline),
                snippet_chars: request.snippet_chars,
            })
            .await
            .map_err(|e| PoolError::Internal(format!("searching the history failed: {e}")))?;
        let mut hits = Vec::with_capacity(found.hits.len());
        for hit in found.hits {
            let session_id = hit.session_id.to_string();
            hits.push(api::SearchHit {
                name: self.directory.stats_of(&session_id).await.title,
                session_id,
                seq: hit.seq,
                origin: hit.source.into(),
                ts: hit
                    .ts
                    .format(&time::format_description::well_known::Rfc3339)
                    .unwrap_or_default(),
                snippet: hit.snippet,
            });
        }
        Ok(api::SearchResult {
            hits,
            scanned: found.scanned,
            truncated: found.truncated || over,
        })
    }

    /// The sessions of every project this daemon serves, most recently
    /// written first.
    async fn sessions_in_range(
        &self,
        store: &dyn history::store::HistoryStore,
    ) -> Vec<base::session::SessionId> {
        let mut found = Vec::new();
        for home in self.directory.served_projects(self.cwd()).await {
            let query = history::query::SessionQuery::recent(usize::MAX)
                .ids_only()
                .within(history::query::SessionScope::Project(home));
            if let Ok(summaries) = store.find_sessions(&query).await {
                found.extend(summaries);
            }
        }
        found.sort_by(|a, b| {
            b.last_modified
                .cmp(&a.last_modified)
                .then_with(|| b.session_id.to_string().cmp(&a.session_id.to_string()))
        });
        let mut seen = std::collections::HashSet::new();
        found
            .into_iter()
            .map(|s| s.session_id)
            .filter(|sid| seen.insert(*sid))
            .collect()
    }
}

fn named_sessions(ids: &[String]) -> Result<Vec<base::session::SessionId>, PoolError> {
    let mut seen = std::collections::HashSet::new();
    let mut out = Vec::with_capacity(ids.len());
    for id in ids {
        let sid = base::session::SessionId::parse(id)
            .map_err(|e| PoolError::InvalidRequest(format!("invalid session_id `{id}`: {e}")))?;
        if seen.insert(sid) {
            out.push(sid);
        }
    }
    Ok(out)
}
