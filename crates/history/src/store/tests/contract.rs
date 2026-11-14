//! The `HistoryStore` contract run once per backend.

use super::super::*;
use crate::entry::{EnvelopedEntry, LogEntry};
use crate::error::HistoryError;
use crate::path::HistoryRoots;
use async_trait::async_trait;
use base::session::SessionId;

use crate::entry::SessionKind;
use base::permission::PermissionMode;

async fn jsonl() -> (
    Box<dyn HistoryStore>,
    Option<tempfile::TempDir>,
    tempfile::TempDir,
) {
    let cwd = tempfile::TempDir::new().unwrap();
    let projects = tempfile::TempDir::new().unwrap();
    let store = JsonlHistoryStore::with_roots(cwd.path(), HistoryRoots::under(projects.path()))
        .await
        .unwrap();
    (Box::new(store), Some(cwd), projects)
}

fn in_memory() -> Box<dyn HistoryStore> {
    Box::new(InMemoryHistoryStore::new())
}

fn user(text: &str) -> LogEntry {
    LogEntry::User {
        content: vec![base::message::ContentBlock::Text {
            text: text.to_string(),
            cache_control: None,
        }],
    }
}

fn meta(parent: Option<&str>) -> LogEntry {
    LogEntry::Meta {
        cwd: "/tmp".into(),
        started_at: time::OffsetDateTime::now_utc(),
        model: "test-model".into(),
        permission_mode: format!("{:?}", PermissionMode::Default),
        engine_version: "0.0.1".into(),
        agix_version: "0.0.1".into(),
        parent_session_id: parent.map(str::to_string),
        scene: None,
        project_root: None,
        session_kind: SessionKind::Primary,
        schema_version: crate::entry::CURRENT_META_SCHEMA_VERSION,
    }
}

fn meta_in(scene: &str, project_root: &str) -> LogEntry {
    match meta(None) {
        LogEntry::Meta {
            cwd,
            started_at,
            model,
            permission_mode,
            engine_version,
            agix_version,
            parent_session_id,
            session_kind,
            schema_version,
            ..
        } => LogEntry::Meta {
            cwd,
            started_at,
            model,
            permission_mode,
            engine_version,
            agix_version,
            parent_session_id,
            scene: Some(scene.to_string()),
            project_root: Some(project_root.to_string()),
            session_kind,
            schema_version,
        },
        other => other,
    }
}

/// Runs one property against both shipped backends, so a divergence is a
/// failing test rather than a surprise at the call site.
macro_rules! for_each_store {
    ($name:ident, |$store:ident| $body:block) => {
        #[tokio::test]
        async fn $name() {
            {
                let (boxed, _cwd, _projects) = jsonl().await;
                let $store: &dyn HistoryStore = boxed.as_ref();
                $body
            }
            {
                let boxed = in_memory();
                let $store: &dyn HistoryStore = boxed.as_ref();
                $body
            }
        }
    };
}

for_each_store!(entries_come_back_in_the_order_they_went_in, |store| {
    let s = SessionId::new();
    for i in 0..5 {
        store.append(s, user(&format!("m{i}"))).await.unwrap();
    }
    let loaded = store.load(s).await.unwrap();
    let texts: Vec<String> = loaded
        .iter()
        .filter_map(|e| match &e.entry {
            LogEntry::User { content } => content.iter().find_map(|b| match b {
                base::message::ContentBlock::Text { text, .. } => Some(text.clone()),
                _ => None,
            }),
            _ => None,
        })
        .collect();
    assert_eq!(texts, ["m0", "m1", "m2", "m3", "m4"]);
});

for_each_store!(
    a_session_that_was_never_written_is_not_an_empty_one,
    |store| {
        let err = store.load(SessionId::new()).await.unwrap_err();
        assert!(
            matches!(err, HistoryError::SessionNotFound(_)),
            "absent must be distinguishable from empty: {err:?}"
        );
    }
);

for_each_store!(listing_finds_every_written_session, |store| {
    let a = SessionId::new();
    let b = SessionId::new();
    store.append(a, user("hi")).await.unwrap();
    store.append(b, user("hi")).await.unwrap();
    let mut listed: Vec<String> = store
        .list_sessions()
        .await
        .unwrap()
        .iter()
        .map(SessionId::to_string)
        .collect();
    listed.sort();
    let mut want = vec![a.to_string(), b.to_string()];
    want.sort();
    assert_eq!(listed, want);
});

for_each_store!(deleting_forgets_the_session_and_is_idempotent, |store| {
    let s = SessionId::new();
    store.append(s, user("hi")).await.unwrap();
    store.delete(s).await.unwrap();
    assert!(matches!(
        store.load(s).await.unwrap_err(),
        HistoryError::SessionNotFound(_)
    ));
    store
        .delete(s)
        .await
        .expect("deleting what is not there is not an error");
});

for_each_store!(
    the_parent_link_is_readable_and_drives_the_child_walk,
    |store| {
        let parent = SessionId::new();
        let child = SessionId::new();
        let orphan = SessionId::new();
        store.append(parent, meta(None)).await.unwrap();
        store
            .append(child, meta(Some(&parent.to_string())))
            .await
            .unwrap();
        store.append(orphan, meta(None)).await.unwrap();

        assert_eq!(store.session_parent(parent).await.unwrap(), None);
        assert_eq!(
            store.session_parent(child).await.unwrap(),
            Some(parent.to_string())
        );
        assert_eq!(
            store.child_sessions(&parent.to_string()).await.unwrap(),
            vec![child]
        );
    }
);

for_each_store!(
    the_scene_and_project_a_session_was_created_under_are_readable,
    |store| {
        let s = SessionId::new();
        store.append(s, meta_in("harnix", "/repo/a")).await.unwrap();
        store.append(s, user("hi")).await.unwrap();

        let facts = store.session_facts(s).await.unwrap();
        assert_eq!(facts.scene.as_deref(), Some("harnix"));
        assert_eq!(facts.project_root.as_deref(), Some("/repo/a"));
        assert_eq!(facts.session_kind, SessionKind::Primary);
    }
);

// A pre-v2 log has no `scene`/`project_root` on its `Meta` line, and a
// session that has not been written to has no `Meta` line at all. Both
// answer `None` rather than an inferred value — inferring here is what
// docs/daemon_rpc_protocol.md §3.4's scene check exists to refuse.
for_each_store!(
    facts_a_log_does_not_record_are_absent_not_guessed,
    |store| {
        let written = SessionId::new();
        store.append(written, meta(None)).await.unwrap();
        let facts = store.session_facts(written).await.unwrap();
        assert_eq!(facts.scene, None);
        assert_eq!(facts.project_root, None);

        let never_written = SessionId::new();
        assert_eq!(
            store.session_facts(never_written).await.unwrap(),
            SessionFacts::default(),
            "a session with no log is not an error here, it is no facts"
        );
    }
);

for_each_store!(a_session_keeps_the_name_a_person_gave_it, |store| {
    let s = SessionId::new();
    store.append(s, meta(None)).await.unwrap();
    assert_eq!(store.session_stats(s).await.unwrap().title, None);

    store
        .set_session_title(s, Some("重构 daemon 装配".into()))
        .await
        .unwrap();
    assert_eq!(
        store.session_stats(s).await.unwrap().title.as_deref(),
        Some("重构 daemon 装配")
    );

    // Renaming twice is renaming, not accumulating.
    store
        .set_session_title(s, Some("second".into()))
        .await
        .unwrap();
    assert_eq!(
        store.session_stats(s).await.unwrap().title.as_deref(),
        Some("second")
    );

    store.set_session_title(s, None).await.unwrap();
    assert_eq!(
        store.session_stats(s).await.unwrap().title,
        None,
        "clearing the name puts the session back to whatever names it automatically"
    );
});

// A name is not an entry in the log — it is chosen after the fact and
// changed after the fact, so naming a session must not disturb what was
// said in it.
for_each_store!(naming_a_session_does_not_touch_its_transcript, |store| {
    let s = SessionId::new();
    store.append(s, meta(None)).await.unwrap();
    store.append(s, user("hello")).await.unwrap();
    let before = store.load(s).await.unwrap().len();

    store
        .set_session_title(s, Some("named".into()))
        .await
        .unwrap();

    assert_eq!(store.load(s).await.unwrap().len(), before);
});

for_each_store!(an_extension_entry_comes_back_exactly_as_written, |store| {
    let s = SessionId::new();
    store.append(s, meta(None)).await.unwrap();
    store
        .append(
            s,
            LogEntry::Extension {
                ns: "com.example.gone".into(),
                event: "checkpoint".into(),
                payload: serde_json::json!({"step": 3, "nested": {"a": [1, 2]}}),
            },
        )
        .await
        .unwrap();
    store.append(s, user("after")).await.unwrap();

    let loaded = store.load(s).await.unwrap();
    assert_eq!(loaded.len(), 3, "an unreadable entry is still an entry");
    let LogEntry::Extension { ns, event, payload } = &loaded[1].entry else {
        panic!(
            "expected the extension entry in position, got {:?}",
            loaded[1].entry
        );
    };
    assert_eq!(ns, "com.example.gone");
    assert_eq!(event, "checkpoint");
    assert_eq!(
        payload,
        &serde_json::json!({"step": 3, "nested": {"a": [1, 2]}})
    );

    assert_eq!(
        store.load_messages(s).await.unwrap().len(),
        1,
        "it must not have become something the model sees"
    );
});

for_each_store!(messages_are_projected_from_the_log, |store| {
    let s = SessionId::new();
    store.append(s, meta(None)).await.unwrap();
    store.append(s, user("hello")).await.unwrap();
    let messages = store.load_messages(s).await.unwrap();
    assert_eq!(messages.len(), 1, "Meta must not reach the model");
});

fn meta_of(kind: SessionKind, parent: SessionId) -> LogEntry {
    match meta(Some(&parent.to_string())) {
        LogEntry::Meta {
            cwd,
            started_at,
            model,
            permission_mode,
            engine_version,
            agix_version,
            parent_session_id,
            scene,
            project_root,
            schema_version,
            ..
        } => LogEntry::Meta {
            cwd,
            started_at,
            model,
            permission_mode,
            engine_version,
            agix_version,
            parent_session_id,
            scene,
            project_root,
            session_kind: kind,
            schema_version,
        },
        other => other,
    }
}

fn ids(snapshot: &LineageSnapshot) -> Vec<SessionId> {
    snapshot.sessions.iter().map(|(s, _)| *s).collect()
}

async fn lineage_contract(store: &dyn HistoryStore) {
    let root = SessionId::new();
    let child = SessionId::new();
    let grandchild = SessionId::new();
    let fork = SessionId::new();
    let under_fork = SessionId::new();
    let stranger = SessionId::new();
    store.append(root, meta(None)).await.unwrap();
    store
        .append(child, meta_of(SessionKind::Sidechain, root))
        .await
        .unwrap();
    store
        .append(grandchild, meta_of(SessionKind::Sidechain, child))
        .await
        .unwrap();
    store
        .append(fork, meta_of(SessionKind::Primary, root))
        .await
        .unwrap();
    store
        .append(under_fork, meta_of(SessionKind::Sidechain, fork))
        .await
        .unwrap();
    store.append(stranger, meta(None)).await.unwrap();

    let snapshot = store.snapshot_lineage(root).await.unwrap();
    assert_eq!(ids(&snapshot), [root, child, grandchild]);
    assert_eq!(snapshot.forks, [fork]);

    let err = store.snapshot_lineage(SessionId::new()).await.unwrap_err();
    assert!(matches!(err, HistoryError::SessionNotFound(_)), "{err:?}");
}

for_each_store!(
    a_lineage_reaches_every_sidechain_and_names_forks_without_entering_them,
    |store| {
        lineage_contract(store).await;
    }
);

for_each_store!(a_snapshot_holds_nothing_appended_after_it, |store| {
    let root = SessionId::new();
    let child = SessionId::new();
    store.append(root, meta(None)).await.unwrap();
    store.append(root, user("before")).await.unwrap();
    store
        .append(child, meta_of(SessionKind::Sidechain, root))
        .await
        .unwrap();

    let snapshot = store.snapshot_lineage(root).await.unwrap();

    store.append(root, user("after")).await.unwrap();
    store.append(child, user("after")).await.unwrap();
    let late = SessionId::new();
    store
        .append(late, meta_of(SessionKind::Sidechain, root))
        .await
        .unwrap();

    assert_eq!(ids(&snapshot), [root, child]);
    assert_eq!(snapshot.sessions[0].1.len(), 2);
    assert_eq!(snapshot.sessions[1].1.len(), 1);
    assert_eq!(
        store.snapshot_lineage(root).await.unwrap().sessions.len(),
        3
    );
});

for_each_store!(
    a_snapshot_taken_while_appends_run_is_a_prefix_of_the_log,
    |store| {
        let root = SessionId::new();
        store.append(root, meta(None)).await.unwrap();
        let writing = async {
            for i in 0..40 {
                store.append(root, user(&format!("m{i}"))).await.unwrap();
                tokio::task::yield_now().await;
            }
        };
        let reading = async {
            let mut seen = Vec::new();
            for _ in 0..20 {
                let snapshot = store.snapshot_lineage(root).await.unwrap();
                seen.push(snapshot.sessions[0].1.clone());
                tokio::task::yield_now().await;
            }
            seen
        };
        let ((), snapshots) = tokio::join!(writing, reading);
        let full = store.load(root).await.unwrap();
        for snapshot in snapshots {
            assert!(snapshot.len() <= full.len());
            for (taken, logged) in snapshot.iter().zip(&full) {
                assert_eq!(taken.id, logged.id);
            }
        }
    }
);

/// A backend that implements only what it must, so the defaults are what
/// answers.
struct OnlyTheRequired(InMemoryHistoryStore);

#[async_trait]
impl HistoryStore for OnlyTheRequired {
    async fn append(&self, session: SessionId, entry: LogEntry) -> Result<(), HistoryError> {
        self.0.append(session, entry).await
    }
    async fn load(&self, session: SessionId) -> Result<Vec<EnvelopedEntry>, HistoryError> {
        self.0.load(session).await
    }
    async fn list_sessions(&self) -> Result<Vec<SessionId>, HistoryError> {
        self.0.list_sessions().await
    }
    async fn delete(&self, session: SessionId) -> Result<(), HistoryError> {
        self.0.delete(session).await
    }
}

#[tokio::test]
async fn the_default_lineage_walk_agrees_with_the_shipped_backends() {
    lineage_contract(&OnlyTheRequired(InMemoryHistoryStore::new())).await;
}
