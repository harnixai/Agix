use super::*;
use crate::transcript::render_search_text;
use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};

/// What an index keeps per session so a search never opens a transcript.
struct IndexRow {
    modified: Option<time::OffsetDateTime>,
    id: String,
    text_lower: String,
    summary: SessionSummary,
}

/// A backend that takes search over outright.
///
/// Everything a query needs is derived once, on append, and the search
/// path touches nothing else — which is what [`load_calls`] is counted to
/// prove.
struct IndexedHistoryStore {
    inner: Arc<InMemoryHistoryStore>,
    index: std::sync::Mutex<HashMap<SessionId, IndexRow>>,
    load_calls: AtomicUsize,
}

impl IndexedHistoryStore {
    fn new(inner: Arc<InMemoryHistoryStore>) -> Self {
        Self {
            inner,
            index: std::sync::Mutex::new(HashMap::new()),
            load_calls: AtomicUsize::new(0),
        }
    }

    fn load_calls(&self) -> usize {
        self.load_calls.load(Ordering::SeqCst)
    }
}

#[async_trait]
impl HistoryStore for IndexedHistoryStore {
    async fn append(&self, session: SessionId, entry: LogEntry) -> Result<(), HistoryError> {
        self.inner.append(session, entry).await?;
        let entries = self.inner.load(session).await?;
        let messages = project_messages_with(&entries, self.projection());
        let row = IndexRow {
            modified: entries.iter().map(|env| env.ts).max(),
            id: session.to_string(),
            text_lower: render_search_text(&messages).to_lowercase(),
            summary: SessionSummary {
                session_id: session,
                last_modified: entries
                    .iter()
                    .map(|env| env.ts)
                    .max()
                    .map(format_ts)
                    .unwrap_or_else(unknown_time),
                entry_count: entries.len(),
                message_count: messages.len(),
                preview: preview_messages(&messages, 140),
                canonical_cwd: None,
                title: None,
                total_input_tokens: None,
                total_output_tokens: None,
                compact_count: 0,
            },
        };
        self.index
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(session, row);
        Ok(())
    }

    async fn load(&self, session: SessionId) -> Result<Vec<EnvelopedEntry>, HistoryError> {
        self.load_calls.fetch_add(1, Ordering::SeqCst);
        self.inner.load(session).await
    }

    async fn list_sessions(&self) -> Result<Vec<SessionId>, HistoryError> {
        self.inner.list_sessions().await
    }

    async fn delete(&self, session: SessionId) -> Result<(), HistoryError> {
        self.index
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&session);
        self.inner.delete(session).await
    }

    async fn find_sessions(
        &self,
        query: &SessionQuery,
    ) -> Result<Vec<SessionSummary>, HistoryError> {
        if query.limit == 0 {
            return Ok(Vec::new());
        }
        let lowered = query.needle().map(str::to_lowercase);
        let index = self.index.lock().unwrap_or_else(|e| e.into_inner());
        let mut hits: Vec<&IndexRow> = index
            .values()
            .filter(|row| match (query.needle(), &lowered) {
                (Some(needle), Some(lowered)) => {
                    row.id.contains(needle) || row.text_lower.contains(lowered)
                }
                _ => true,
            })
            .collect();
        hits.sort_by(|a, b| b.modified.cmp(&a.modified).then_with(|| b.id.cmp(&a.id)));
        hits.truncate(query.limit);
        Ok(hits.into_iter().map(|row| row.summary.clone()).collect())
    }
}

fn user(text: &str) -> LogEntry {
    LogEntry::User {
        content: vec![base::message::ContentBlock::Text {
            text: text.to_string(),
            cache_control: None,
        }],
    }
}

const CORPUS: [(&str, &str); 4] = [
    ("first", "planning the Postgres migration"),
    ("second", "renaming a struct"),
    ("third", "postgres again, and a deadlock"),
    ("fourth", "nothing to do with databases"),
];

async fn fill(store: &dyn HistoryStore, ids: &[SessionId]) {
    for (id, (label, text)) in ids.iter().zip(CORPUS) {
        store.append(*id, user(label)).await.unwrap();
        store.append(*id, user(text)).await.unwrap();
    }
}

fn queries() -> Vec<SessionQuery> {
    vec![
        SessionQuery::recent(10),
        SessionQuery::recent(2),
        SessionQuery::matching("postgres", 10),
        SessionQuery::matching("  Postgres ", 10),
        SessionQuery::matching("deadlock", 10),
        SessionQuery::matching("nothing anyone wrote", 10),
        SessionQuery::matching("postgres", 1),
        SessionQuery::recent(0),
    ]
}

/// The acceptance: an index answers exactly what the scan would have, down
/// to the order and the preview text, and answers it without reading a
/// single transcript.
#[tokio::test]
async fn an_index_and_a_scan_are_indistinguishable_to_a_caller() {
    let shared = Arc::new(InMemoryHistoryStore::new());
    let indexed = IndexedHistoryStore::new(shared.clone());
    let ids: Vec<SessionId> = (0..CORPUS.len()).map(|_| SessionId::new()).collect();
    // Written once, through the indexing backend, so both sides see the
    // same envelopes — two appends of the same entry get two timestamps.
    fill(&indexed, &ids).await;

    let before = indexed.load_calls();
    for query in queries() {
        let scanned = shared.find_sessions(&query).await.unwrap();
        let from_index = indexed.find_sessions(&query).await.unwrap();
        assert_eq!(
            from_index, scanned,
            "the two backends disagree on {query:?}"
        );
    }
    assert_eq!(
        indexed.load_calls(),
        before,
        "an index that reads transcripts to answer a query is not an index"
    );
}

/// A ceiling the caller sets is a ceiling both backends keep.
#[tokio::test]
async fn a_limit_is_a_ceiling_and_zero_means_nothing() {
    let shared = Arc::new(InMemoryHistoryStore::new());
    let indexed = IndexedHistoryStore::new(shared.clone());
    let ids: Vec<SessionId> = (0..CORPUS.len()).map(|_| SessionId::new()).collect();
    fill(&indexed, &ids).await;

    for store in [&*shared as &dyn HistoryStore, &indexed as &dyn HistoryStore] {
        assert_eq!(
            store.find_sessions(&SessionQuery::recent(0)).await.unwrap(),
            vec![]
        );
        assert_eq!(
            store
                .find_sessions(&SessionQuery::recent(2))
                .await
                .unwrap()
                .len(),
            2
        );
        assert_eq!(
            store
                .find_sessions(&SessionQuery::recent(99))
                .await
                .unwrap()
                .len(),
            CORPUS.len()
        );
    }
}

/// Give each session file a distinct mtime, in the order they were
/// written.
///
/// The two backends order by different clocks — a file's mtime and an
/// entry's timestamp — and only one of them has a resolution this test
/// controls. On a filesystem whose mtime granularity is coarser than the
/// loop that wrote these files (ext4 with small inodes rounds to the
/// second), all four land on the same mtime, the file-backed store falls
/// through to its id tie-break, and a *limited* query then answers with a
/// different set rather than a different order. That is the tie the test
/// below says it tolerates in ordering — but a set difference is not an
/// ordering difference, and the assertion is about the set.
fn stamp_distinct_mtimes(store: &JsonlHistoryStore, ids: &[SessionId]) {
    let base = std::time::SystemTime::now() - std::time::Duration::from_secs(3600);
    for (i, id) in ids.iter().enumerate() {
        let path = store.session_file_path(id);
        let when = base + std::time::Duration::from_secs(10 * (i as u64 + 1));
        let file = std::fs::File::options()
            .write(true)
            .open(&path)
            .expect("the session file this test just wrote");
        file.set_times(std::fs::FileTimes::new().set_modified(when))
            .expect("the temp filesystem allows setting mtimes");
    }
}

/// The two shipped backends have different notions of "when was this last
/// touched" — a file's mtime and an entry's timestamp — so their orders
/// can differ by a tie. Which sessions match cannot.
#[tokio::test]
async fn the_shipped_backends_agree_on_which_sessions_match() {
    let cwd = tempfile::TempDir::new().unwrap();
    let projects = tempfile::TempDir::new().unwrap();
    let jsonl = JsonlHistoryStore::with_roots(cwd.path(), HistoryRoots::under(projects.path()))
        .await
        .unwrap();
    let memory = InMemoryHistoryStore::new();
    let ids: Vec<SessionId> = (0..CORPUS.len()).map(|_| SessionId::new()).collect();
    fill(&jsonl, &ids).await;
    fill(&memory, &ids).await;
    stamp_distinct_mtimes(&jsonl, &ids);

    for query in queries() {
        let matched = |summaries: Vec<SessionSummary>| {
            let mut ids: Vec<String> = summaries
                .into_iter()
                .map(|s| s.session_id.to_string())
                .collect();
            ids.sort();
            ids
        };
        let from_files = matched(jsonl.find_sessions(&query).await.unwrap());
        let from_memory = matched(memory.find_sessions(&query).await.unwrap());
        assert_eq!(
            from_files, from_memory,
            "the shipped backends disagree on {query:?}"
        );
    }
}
