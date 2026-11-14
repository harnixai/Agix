use super::*;
use crate::entry::{self, SessionKind};
use base::message::{ContentBlock, ToolResultContent};
use base::permission::PermissionMode;
use serde_json::json;
use tempfile::TempDir;
use time::OffsetDateTime;

#[tokio::test]
async fn append_creates_file_under_sanitized_dir() {
    let (store, _cwd, _proj) = make_store().await;
    let s = SessionId::new();
    store
        .append(
            s,
            LogEntry::User {
                content: vec![ContentBlock::Text {
                    text: "hi".into(),
                    cache_control: None,
                }],
            },
        )
        .await
        .unwrap();

    let path = store.session_file_path(&s);
    assert!(path.exists());
    let content = tokio::fs::read_to_string(&path).await.unwrap();
    assert!(content.ends_with('\n'));
    assert_eq!(content.lines().count(), 1);
}

#[tokio::test]
async fn append_then_load_roundtrip() {
    let (store, _cwd, _proj) = make_store().await;
    let s = SessionId::new();

    // meta + 3 messages
    store
        .append(
            s,
            LogEntry::Meta {
                cwd: "/tmp/test".into(),
                started_at: OffsetDateTime::now_utc(),
                model: "claude-sonnet-4-6".into(),
                permission_mode: format!("{:?}", PermissionMode::Default),
                engine_version: "0.0.1".into(),
                agix_version: "0.0.1".into(),
                parent_session_id: None,
                scene: Some("code".into()),
                project_root: None,
                session_kind: SessionKind::Primary,
                schema_version: entry::CURRENT_META_SCHEMA_VERSION,
            },
        )
        .await
        .unwrap();

    for txt in &["one", "two", "three"] {
        store
            .append(
                s,
                LogEntry::User {
                    content: vec![ContentBlock::Text {
                        text: (*txt).into(),
                        cache_control: None,
                    }],
                },
            )
            .await
            .unwrap();
    }

    let loaded = store.load(s).await.unwrap();
    assert_eq!(loaded.len(), 4);
    assert!(matches!(loaded[0].entry, LogEntry::Meta { .. }));
    for (env, expected) in loaded.iter().skip(1).zip(["one", "two", "three"].iter()) {
        match &env.entry {
            LogEntry::User { content } => match &content[0] {
                ContentBlock::Text { text, .. } => assert_eq!(text, expected),
                _ => panic!(),
            },
            _ => panic!(),
        }
    }
}

#[tokio::test]
async fn load_unknown_session_errors_with_session_not_found() {
    let (store, _cwd, _proj) = make_store().await;
    let s = SessionId::new();
    let err = store.load(s).await.unwrap_err();
    assert!(matches!(err, HistoryError::SessionNotFound(_)));
}

#[tokio::test]
async fn list_sessions_returns_existing_files() {
    let (store, _cwd, _proj) = make_store().await;
    let a = SessionId::new();
    let b = SessionId::new();
    for s in [a, b] {
        store
            .append(
                s,
                LogEntry::User {
                    content: vec![ContentBlock::Text {
                        text: "x".into(),
                        cache_control: None,
                    }],
                },
            )
            .await
            .unwrap();
    }
    let mut listed = store.list_sessions().await.unwrap();
    listed.sort_by_key(|s| s.to_string());
    let mut expected = vec![a, b];
    expected.sort_by_key(|s| s.to_string());
    assert_eq!(listed, expected);
}

#[tokio::test]
async fn recent_session_summaries_include_counts_and_preview() {
    let (store, _cwd, _proj) = make_store().await;
    let s = SessionId::new();
    store
        .append(
            s,
            LogEntry::User {
                content: vec![ContentBlock::Text {
                    text: "first request".into(),
                    cache_control: None,
                }],
            },
        )
        .await
        .unwrap();
    store
        .append(
            s,
            LogEntry::Assistant {
                content: vec![ContentBlock::Text {
                    text: "useful answer".into(),
                    cache_control: None,
                }],
                stop_reason: None,
                usage: None,
                model: None,
            },
        )
        .await
        .unwrap();

    let summaries = store.find_sessions(&SessionQuery::recent(5)).await.unwrap();
    assert_eq!(summaries.len(), 1);
    assert_eq!(summaries[0].session_id, s);
    assert_eq!(summaries[0].entry_count, 2);
    assert_eq!(summaries[0].message_count, 2);
    assert!(summaries[0].preview.contains("useful answer"));
}

/// A listing asks which sessions there are. Building a preview for each
/// costs a full transcript read, and `session.list` throws every preview
/// away — so a few hundred sessions turned a directory read into a parse
/// of all of them.
#[tokio::test]
async fn an_ids_only_query_does_not_open_the_transcripts() {
    let (store, _cwd, _proj) = make_store().await;
    let s = SessionId::new();
    for text in ["a question", "a useful answer"] {
        store
            .append(
                s,
                LogEntry::User {
                    content: vec![ContentBlock::Text {
                        text: text.into(),
                        cache_control: None,
                    }],
                },
            )
            .await
            .unwrap();
    }

    let full = store
        .find_sessions(&crate::query::SessionQuery::recent(5))
        .await
        .unwrap();
    assert_eq!(full.len(), 1);
    assert_eq!(full[0].message_count, 2);
    assert!(full[0].preview.contains("useful answer"));

    let ids = store
        .find_sessions(&crate::query::SessionQuery::recent(5).ids_only())
        .await
        .unwrap();
    assert_eq!(ids.len(), 1, "the same sessions, in the same order");
    assert_eq!(ids[0].session_id, s);
    assert_eq!(
        (ids[0].message_count, ids[0].entry_count),
        (0, 0),
        "a count that was never read is reported as unread, not guessed"
    );
    assert!(
        ids[0].preview.is_empty(),
        "a preview here would mean the transcript was opened after all"
    );
}

#[tokio::test]
async fn a_transcript_that_will_not_parse_hides_only_itself() {
    let (store, _cwd, _proj) = make_store().await;
    let good = SessionId::new();
    store
        .append(
            good,
            LogEntry::User {
                content: vec![ContentBlock::Text {
                    text: "readable".into(),
                    cache_control: None,
                }],
            },
        )
        .await
        .unwrap();

    let corrupt = SessionId::new();
    tokio::fs::write(store.session_file_path(&corrupt), "{not json at all\n")
        .await
        .unwrap();

    let summaries = store
        .find_sessions(&SessionQuery::recent(10))
        .await
        .expect("one unreadable log must not fail the listing");
    assert_eq!(
        summaries.iter().map(|s| s.session_id).collect::<Vec<_>>(),
        vec![good]
    );
}

#[tokio::test]
async fn search_session_summaries_matches_transcript_text_and_id() {
    let (store, _cwd, _proj) = make_store().await;
    let matching = SessionId::new();
    let other = SessionId::new();
    store
        .append(
            matching,
            LogEntry::User {
                content: vec![ContentBlock::Text {
                    text: "needle topic".into(),
                    cache_control: None,
                }],
            },
        )
        .await
        .unwrap();
    store
        .append(
            other,
            LogEntry::User {
                content: vec![ContentBlock::Text {
                    text: "different topic".into(),
                    cache_control: None,
                }],
            },
        )
        .await
        .unwrap();

    let by_text = store
        .find_sessions(&SessionQuery::matching("needle", 10))
        .await
        .unwrap();
    assert_eq!(by_text.len(), 1);
    assert_eq!(by_text[0].session_id, matching);

    let id_prefix = &other.to_string()[..8];
    let by_id = store
        .find_sessions(&SessionQuery::matching(id_prefix, 10))
        .await
        .unwrap();
    assert_eq!(by_id.len(), 1);
    assert_eq!(by_id[0].session_id, other);
}

#[tokio::test]
async fn search_all_project_session_summaries_scans_other_project_dirs() {
    let cwd_a = TempDir::new().unwrap();
    let cwd_b = TempDir::new().unwrap();
    let projects = TempDir::new().unwrap();
    let store_a = JsonlHistoryStore::with_roots(cwd_a.path(), HistoryRoots::under(projects.path()))
        .await
        .unwrap();
    let store_b = JsonlHistoryStore::with_roots(cwd_b.path(), HistoryRoots::under(projects.path()))
        .await
        .unwrap();
    let session_b = SessionId::new();
    store_b
        .append(
            session_b,
            LogEntry::User {
                content: vec![ContentBlock::Text {
                    text: "cross project needle".into(),
                    cache_control: None,
                }],
            },
        )
        .await
        .unwrap();

    let found = store_a
        .find_sessions(&SessionQuery::matching("needle", 10).within(SessionScope::AllProjects))
        .await
        .unwrap();

    assert_eq!(found.len(), 1);
    assert_eq!(found[0].session_id, session_b);
}

#[tokio::test]
async fn list_sessions_empty_dir() {
    let (store, _cwd, _proj) = make_store().await;
    let listed = store.list_sessions().await.unwrap();
    assert!(listed.is_empty());
}

#[tokio::test]
async fn parse_error_carries_line_number() {
    let (store, _cwd, proj) = make_store().await;
    let s = SessionId::new();
    let dir = store.project_dir_path();
    tokio::fs::create_dir_all(&dir).await.unwrap();
    let path = store.session_file_path(&s);

    // 第 1 行合法；第 2 行坏；第 3 行合法
    let id1 = base::id::Id::new();
    let id3 = base::id::Id::new();
    let line1 = serde_json::to_string(&json!({
        "v": 1, "id": id1, "ts": "2026-05-04T00:00:00Z",
        "session_id": s, "kind": "user", "content": []
    }))
    .unwrap();
    let line2 = "{ this is broken json";
    let line3 = serde_json::to_string(&json!({
        "v": 1, "id": id3, "ts": "2026-05-04T00:00:01Z",
        "session_id": s, "kind": "user", "content": []
    }))
    .unwrap();

    let blob = format!("{line1}\n{line2}\n{line3}\n");
    tokio::fs::write(&path, blob).await.unwrap();

    let err = store.load(s).await.unwrap_err();
    match err {
        HistoryError::Parse { line, .. } => assert_eq!(line, 2),
        other => panic!("expected Parse, got {other:?}"),
    }
    // store 仍可被 list（项目目录还在）
    let listed = store.list_sessions().await.unwrap();
    assert_eq!(listed, vec![s]);
    drop(proj);
}

#[tokio::test]
async fn concurrent_appends_do_not_interleave_lines() {
    let (store, _cwd, _proj) = make_store().await;
    let s = SessionId::new();
    let store = Arc::new(store);

    let mut tasks = Vec::new();
    for i in 0..30u32 {
        let st = store.clone();
        tasks.push(tokio::spawn(async move {
            st.append(
                s,
                LogEntry::User {
                    content: vec![ContentBlock::Text {
                        text: format!("msg-{i}"),
                        cache_control: None,
                    }],
                },
            )
            .await
            .unwrap();
        }));
    }
    for t in tasks {
        t.await.unwrap();
    }

    let loaded = store.load(s).await.unwrap();
    assert_eq!(loaded.len(), 30);
    // 每行都能解析；顺序不保证（concurrent）但不应该出现 partial line 错
}

#[tokio::test]
async fn delete_removes_session_file() {
    let (store, _cwd, _proj) = make_store().await;
    let s = SessionId::new();
    store
        .append(
            s,
            LogEntry::User {
                content: vec![ContentBlock::Text {
                    text: "will be deleted".into(),
                    cache_control: None,
                }],
            },
        )
        .await
        .unwrap();
    let path = store.session_file_path(&s);
    assert!(path.exists());
    store.delete(s).await.unwrap();
    assert!(!path.exists());
    // 再次删除不报错
    store.delete(s).await.unwrap();
}

#[tokio::test]
async fn tool_result_roundtrip() {
    let (store, _cwd, _proj) = make_store().await;
    let s = SessionId::new();
    store
        .append(
            s,
            LogEntry::ToolResult {
                tool_use_id: "toolu_01".into(),
                content: ToolResultContent::Text("stdout".into()),
                is_error: false,
            },
        )
        .await
        .unwrap();
    let loaded = store.load(s).await.unwrap();
    assert_eq!(loaded.len(), 1);
    match &loaded[0].entry {
        LogEntry::ToolResult {
            tool_use_id,
            content,
            is_error,
        } => {
            assert_eq!(tool_use_id, "toolu_01");
            assert_eq!(content, &ToolResultContent::Text("stdout".into()));
            assert!(!is_error);
        }
        _ => panic!(),
    }
}

#[tokio::test]
async fn search_all_projects_finds_match() {
    let cwd_a = TempDir::new().unwrap();
    let cwd_b = TempDir::new().unwrap();
    let projects = TempDir::new().unwrap();
    let store_a = JsonlHistoryStore::with_roots(cwd_a.path(), HistoryRoots::under(projects.path()))
        .await
        .unwrap();
    let store_b = JsonlHistoryStore::with_roots(cwd_b.path(), HistoryRoots::under(projects.path()))
        .await
        .unwrap();

    let session_a = SessionId::new();
    store_a
        .append(
            session_a,
            LogEntry::User {
                content: vec![ContentBlock::Text {
                    text: "project alpha unrelated".into(),
                    cache_control: None,
                }],
            },
        )
        .await
        .unwrap();

    let session_b = SessionId::new();
    store_b
        .append(
            session_b,
            LogEntry::User {
                content: vec![ContentBlock::Text {
                    text: "project beta with needle".into(),
                    cache_control: None,
                }],
            },
        )
        .await
        .unwrap();

    let found = store_a
        .find_sessions(&SessionQuery::matching("needle", 10).within(SessionScope::AllProjects))
        .await
        .unwrap();

    assert_eq!(found.len(), 1);
    assert_eq!(found[0].session_id, session_b);
}

#[tokio::test]
async fn search_empty_query_returns_all() {
    let (store, _cwd, _proj) = make_store().await;
    let a = SessionId::new();
    let b = SessionId::new();
    for (s, txt) in [(&a, "first session"), (&b, "second session")] {
        store
            .append(
                *s,
                LogEntry::User {
                    content: vec![ContentBlock::Text {
                        text: (*txt).into(),
                        cache_control: None,
                    }],
                },
            )
            .await
            .unwrap();
    }

    let results = store
        .find_sessions(&SessionQuery::matching("", 10))
        .await
        .unwrap();

    assert_eq!(results.len(), 2);
    let mut ids: Vec<_> = results.iter().map(|s| s.session_id).collect();
    ids.sort_by_key(|id| id.to_string());
    let mut expected = vec![a, b];
    expected.sort_by_key(|id| id.to_string());
    assert_eq!(ids, expected);
}

#[tokio::test]
async fn search_same_repo_filters_correctly() {
    let repo_dir = TempDir::new().unwrap();
    let outside_dir = TempDir::new().unwrap();
    let shared_projects = TempDir::new().unwrap();
    let config_home = TempDir::new().unwrap();

    // Init git in repo_dir
    let init_output = tokio::process::Command::new("git")
        .arg("init")
        .current_dir(repo_dir.path())
        .output()
        .await
        .unwrap();
    assert!(init_output.status.success());

    // Both stores share one global root, so a session written by either is
    // visible to the other — which is the thing this test filters on.
    let roots = HistoryRoots::under(config_home.path());
    let repo_store = JsonlHistoryStore::with_roots(repo_dir.path(), roots.clone())
        .await
        .unwrap();
    let outside_store = JsonlHistoryStore::with_roots(outside_dir.path(), roots.clone())
        .await
        .unwrap();

    // Create session in repo
    let session_in_repo = SessionId::new();
    repo_store
        .append(
            session_in_repo,
            LogEntry::User {
                content: vec![ContentBlock::Text {
                    text: "inside repo work".into(),
                    cache_control: None,
                }],
            },
        )
        .await
        .unwrap();

    // Create session outside repo
    let session_outside = SessionId::new();
    outside_store
        .append(
            session_outside,
            LogEntry::User {
                content: vec![ContentBlock::Text {
                    text: "outside repo work".into(),
                    cache_control: None,
                }],
            },
        )
        .await
        .unwrap();

    // Create metadata for in-repo session with canonicalized cwd
    let canonical_repo = tokio::fs::canonicalize(repo_dir.path()).await.unwrap();
    let sessions_root_dir = roots.sessions.clone();
    let in_repo_meta_dir = sessions_root_dir.join(session_in_repo.to_string());
    tokio::fs::create_dir_all(&in_repo_meta_dir).await.unwrap();
    let in_repo_meta = SessionMetadata::new(
        &canonical_repo,
        &shared_projects.path().join("in-repo"),
        session_in_repo,
    );
    tokio::fs::write(
        in_repo_meta_dir.join("metadata.json"),
        serde_json::to_string_pretty(&in_repo_meta).unwrap(),
    )
    .await
    .unwrap();

    // Create metadata for outside session with canonicalized cwd
    let canonical_outside = tokio::fs::canonicalize(outside_dir.path()).await.unwrap();
    let outside_meta_dir = sessions_root_dir.join(session_outside.to_string());
    tokio::fs::create_dir_all(&outside_meta_dir).await.unwrap();
    let outside_meta = SessionMetadata::new(
        &canonical_outside,
        &shared_projects.path().join("outside"),
        session_outside,
    );
    tokio::fs::write(
        outside_meta_dir.join("metadata.json"),
        serde_json::to_string_pretty(&outside_meta).unwrap(),
    )
    .await
    .unwrap();

    // Search from repo store with empty query to find all matching filter
    let found = repo_store
        .find_sessions(
            &SessionQuery::matching("", 10).within(SessionScope::Under(canonical_repo.clone())),
        )
        .await
        .unwrap();

    // Only the in-repo session should be found
    assert_eq!(found.len(), 1);
    assert_eq!(found[0].session_id, session_in_repo);
}
