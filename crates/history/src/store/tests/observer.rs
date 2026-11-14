use super::super::*;
use crate::entry::{EnvelopedEntry, LogEntry};
use crate::error::HistoryError;
use crate::path::HistoryRoots;
use async_trait::async_trait;
use base::session::SessionId;
use std::sync::Arc;

use base::message::ContentBlock;

#[derive(Default)]
struct Recording {
    seen: std::sync::Mutex<Vec<String>>,
}

impl AppendObserver for Recording {
    fn observed(&self, _session: SessionId, entry: &LogEntry) {
        let label = match entry {
            LogEntry::User { content } => content
                .iter()
                .find_map(|b| match b {
                    ContentBlock::Text { text, .. } => Some(text.clone()),
                    _ => None,
                })
                .unwrap_or_default(),
            other => format!("{other:?}"),
        };
        self.seen.lock().unwrap().push(label);
    }
}

fn user(text: &str) -> LogEntry {
    LogEntry::User {
        content: vec![ContentBlock::Text {
            text: text.to_string(),
            cache_control: None,
        }],
    }
}

/// The observer sees every append, in order — and the log is exactly what
/// it would have been without one. Wrapping a store must be free of
/// consequence to what it stores; that is the whole point of the seam
/// being read-only.
#[tokio::test]
async fn observing_a_log_does_not_change_it() {
    let plain: Arc<dyn HistoryStore> = Arc::new(InMemoryHistoryStore::new());
    let watched_inner: Arc<dyn HistoryStore> = Arc::new(InMemoryHistoryStore::new());
    let observer = Arc::new(Recording::default());
    let watched = ObservedHistoryStore::new(watched_inner.clone(), vec![observer.clone()]);

    let a = SessionId::new();
    for text in ["one", "two", "three"] {
        plain.append(a, user(text)).await.unwrap();
        watched.append(a, user(text)).await.unwrap();
    }

    let plain_entries = plain.load(a).await.unwrap();
    let watched_entries = watched.load(a).await.unwrap();
    assert_eq!(plain_entries.len(), watched_entries.len());
    for (p, w) in plain_entries.iter().zip(&watched_entries) {
        assert_eq!(
            serde_json::to_value(&p.entry).unwrap(),
            serde_json::to_value(&w.entry).unwrap(),
            "an observed append must store the same bytes as an unobserved one"
        );
    }

    assert_eq!(
        observer.seen.lock().unwrap().as_slice(),
        ["one", "two", "three"],
        "the observer must see every append, in order"
    );
}

/// A store mounted with a different projection hands
/// back a different conversation, and every reader of that store — resume,
/// fork, search — sees the same one, because they all come through here.
#[tokio::test]
async fn a_store_projection_decides_what_the_model_reads_back() {
    let cwd = tempfile::tempdir().unwrap();
    let projects = tempfile::tempdir().unwrap();
    let s = SessionId::new();

    let entries = [
        LogEntry::User {
            content: vec![base::message::ContentBlock::Text {
                text: "ship it".into(),
                cache_control: None,
            }],
        },
        LogEntry::Extension {
            ns: "com.acme.deploy".into(),
            event: "finished".into(),
            payload: serde_json::json!({"version": "1.4.0"}),
        },
    ];

    let engine_rules =
        JsonlHistoryStore::with_roots(cwd.path(), HistoryRoots::under(projects.path()))
            .await
            .unwrap();
    for e in entries.iter().cloned() {
        engine_rules.append(s, e).await.unwrap();
    }
    assert_eq!(
        engine_rules.load_messages(s).await.unwrap().len(),
        1,
        "the engine's rules keep an extension's state out of the conversation"
    );

    let host_rules =
        JsonlHistoryStore::with_roots(cwd.path(), HistoryRoots::under(projects.path()))
            .await
            .unwrap()
            .with_projection(Arc::new(crate::transcript::ExtensionsAreVisible {
                namespaces: vec!["com.acme.deploy".into()],
            }));
    let messages = host_rules.load_messages(s).await.unwrap();
    assert_eq!(messages.len(), 2);
    assert!(
        crate::transcript::render_search_text(&messages).contains("1.4.0"),
        "and the same log now carries the deploy into the conversation"
    );
}

/// A failed append is not an event. An observer that was told about one
/// would be recording something that did not happen.
#[tokio::test]
async fn an_append_that_fails_is_not_observed() {
    struct AlwaysFails;

    #[async_trait]
    impl HistoryStore for AlwaysFails {
        async fn append(&self, _s: SessionId, _e: LogEntry) -> Result<(), HistoryError> {
            Err(HistoryError::Io(std::io::Error::other("disk is gone")))
        }
        async fn load(&self, s: SessionId) -> Result<Vec<EnvelopedEntry>, HistoryError> {
            Err(HistoryError::SessionNotFound(s.to_string()))
        }
        async fn list_sessions(&self) -> Result<Vec<SessionId>, HistoryError> {
            Ok(Vec::new())
        }
        async fn delete(&self, _s: SessionId) -> Result<(), HistoryError> {
            Ok(())
        }
    }

    let observer = Arc::new(Recording::default());
    let watched = ObservedHistoryStore::new(Arc::new(AlwaysFails), vec![observer.clone()]);
    assert!(watched
        .append(SessionId::new(), user("never"))
        .await
        .is_err());
    assert!(
        observer.seen.lock().unwrap().is_empty(),
        "an observer must not be told about a write that did not happen"
    );
}

/// Everything else passes through untouched — the decorator is about
/// appends and must not become a place where reads acquire behavior.
#[tokio::test]
async fn every_other_operation_is_the_inner_store() {
    let inner: Arc<dyn HistoryStore> = Arc::new(InMemoryHistoryStore::new());
    let watched = ObservedHistoryStore::new(inner.clone(), vec![]);
    let s = SessionId::new();
    watched.append(s, user("hi")).await.unwrap();

    assert_eq!(watched.list_sessions().await.unwrap(), vec![s]);
    assert_eq!(watched.load_messages(s).await.unwrap().len(), 1);
    assert_eq!(watched.session_parent(s).await.unwrap(), None);
    watched.delete(s).await.unwrap();
    assert!(matches!(
        inner.load(s).await.unwrap_err(),
        HistoryError::SessionNotFound(_)
    ));
}

#[tokio::test]
async fn append_enveloped_reaches_the_inner_store() {
    let inner: Arc<dyn HistoryStore> = Arc::new(InMemoryHistoryStore::new());
    let observer = Arc::new(Recording::default());
    let watched = ObservedHistoryStore::new(inner.clone(), vec![observer.clone()]);
    let session = SessionId::new();
    let line = EnvelopedEntry::new(session, user("imported"));
    let id = line.id.clone();
    watched.append_enveloped(session, vec![line]).await.unwrap();
    let stored = inner.load(session).await.unwrap();
    assert_eq!(stored.len(), 1);
    assert_eq!(stored[0].id, id);
    assert_eq!(*observer.seen.lock().unwrap(), vec!["imported".to_string()]);
}
