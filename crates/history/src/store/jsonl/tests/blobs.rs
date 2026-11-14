use super::*;
use base::message::{ContentBlock, StopReason, ToolResultContent};
use tempfile::TempDir;

use crate::blob::{BlobStore, InMemoryBlobStore};
use base::message::ImageSource;

async fn make_store_with_blobs() -> (JsonlHistoryStore, TempDir, TempDir, TempDir) {
    let cwd_tmp = TempDir::new().unwrap();
    let projects_tmp = TempDir::new().unwrap();
    let paste_base = TempDir::new().unwrap();
    let store =
        JsonlHistoryStore::with_roots(cwd_tmp.path(), HistoryRoots::under(projects_tmp.path()))
            .await
            .unwrap()
            .with_paste_store(PasteStore::new(paste_base.path()));
    (store, cwd_tmp, projects_tmp, paste_base)
}

pub(super) fn text_block(text: &str) -> ContentBlock {
    ContentBlock::Text {
        text: text.to_string(),
        cache_control: None,
    }
}

fn image_block() -> ContentBlock {
    ContentBlock::Image {
        source: ImageSource::Base64 {
            media_type: "image/png".into(),
            data: "iVBORw0KGgo=".into(),
        },
    }
}

#[tokio::test]
async fn small_content_stays_in_the_log() {
    let (store, _cwd, _proj, _base) = make_store_with_blobs().await;
    let s = SessionId::new();
    store
        .append(
            s,
            LogEntry::User {
                content: vec![text_block("short")],
            },
        )
        .await
        .unwrap();

    let loaded = store.load(s).await.unwrap();
    assert_eq!(loaded.len(), 1);
    assert!(
        matches!(loaded[0].entry, LogEntry::User { .. }),
        "expected User, got {:?}",
        loaded[0].entry
    );
}

#[tokio::test]
async fn large_content_leaves_the_log_and_comes_back_on_load() {
    let (store, _cwd, _proj, _base) = make_store_with_blobs().await;
    let s = SessionId::new();
    let big_text = "X".repeat(1500);
    store
        .append(
            s,
            LogEntry::User {
                content: vec![text_block(&big_text)],
            },
        )
        .await
        .unwrap();

    let loaded = store.load(s).await.unwrap();
    assert_eq!(loaded.len(), 1);
    match &loaded[0].entry {
        LogEntry::User { content } => match &content[0] {
            ContentBlock::Text { text, .. } => assert_eq!(text, &big_text),
            other => panic!("expected Text block, got {other:?}"),
        },
        other => panic!("expected User, got {other:?}"),
    }
}

#[tokio::test]
async fn the_jsonl_holds_a_reference_and_the_blob_holds_the_content() {
    let (store, _cwd, _proj, base) = make_store_with_blobs().await;
    let s = SessionId::new();
    let big_text = "Y".repeat(1500);
    store
        .append(
            s,
            LogEntry::User {
                content: vec![text_block(&big_text)],
            },
        )
        .await
        .unwrap();

    let raw = tokio::fs::read_to_string(store.session_file_path(&s))
        .await
        .unwrap();
    assert!(raw.contains("\"kind\":\"blob\""), "raw jsonl: {raw}");
    assert!(
        !raw.contains(&big_text),
        "the body must not also be inline: {raw}"
    );

    let entries: Vec<EnvelopedEntry> = raw
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    let LogEntry::Blob { blob } = &entries[0].entry else {
        panic!("expected a Blob reference, got {:?}", entries[0].entry);
    };
    assert_eq!(blob.store, "paste");
    assert_eq!(blob.describes, "user");
    assert!(blob.bytes > big_text.len() as u64);

    let stored = tokio::fs::read_to_string(base.path().join("pastes").join(&blob.id))
        .await
        .unwrap();
    assert!(stored.contains(&big_text));
}

#[tokio::test]
async fn a_large_assistant_entry_keeps_every_field_across_the_round_trip() {
    let (store, _cwd, _proj, _base) = make_store_with_blobs().await;
    let s = SessionId::new();
    let big_text = "Z".repeat(1500);
    store
        .append(
            s,
            LogEntry::Assistant {
                content: vec![text_block(&big_text)],
                stop_reason: Some(StopReason::EndTurn),
                usage: None,
                model: Some("claude-sonnet-4-6".into()),
            },
        )
        .await
        .unwrap();

    let loaded = store.load(s).await.unwrap();
    match &loaded[0].entry {
        LogEntry::Assistant {
            content,
            stop_reason,
            model,
            ..
        } => {
            match &content[0] {
                ContentBlock::Text { text, .. } => assert_eq!(text, &big_text),
                other => panic!("expected Text block, got {other:?}"),
            }
            assert_eq!(*stop_reason, Some(StopReason::EndTurn));
            assert_eq!(model.as_deref(), Some("claude-sonnet-4-6"));
        }
        other => panic!("expected Assistant, got {other:?}"),
    }
}

#[tokio::test]
async fn a_big_tool_result_leaves_the_log_too() {
    let (store, _cwd, _proj, _base) = make_store_with_blobs().await;
    let s = SessionId::new();
    let output = "line\n".repeat(400);
    store
        .append(
            s,
            LogEntry::ToolResult {
                tool_use_id: "toolu_01".into(),
                content: ToolResultContent::Text(output.clone()),
                is_error: false,
            },
        )
        .await
        .unwrap();

    let raw = tokio::fs::read_to_string(store.session_file_path(&s))
        .await
        .unwrap();
    assert!(
        raw.contains("\"describes\":\"tool_result\""),
        "raw jsonl: {raw}"
    );
    assert!(!raw.contains(&output));

    match &store.load(s).await.unwrap()[0].entry {
        LogEntry::ToolResult { content, .. } => {
            assert_eq!(content, &ToolResultContent::Text(output));
        }
        other => panic!("expected ToolResult, got {other:?}"),
    }
}

/// An image is bytes nobody reads as text and no search matches, so it
/// leaves the log regardless of how small it is.
#[tokio::test]
async fn an_image_leaves_the_log_at_any_size() {
    let (store, _cwd, _proj, _base) = make_store_with_blobs().await;
    let s = SessionId::new();
    store
        .append(
            s,
            LogEntry::User {
                content: vec![text_block("look"), image_block()],
            },
        )
        .await
        .unwrap();
    store
        .append(
            s,
            LogEntry::ToolResult {
                tool_use_id: "toolu_02".into(),
                content: ToolResultContent::Blocks(vec![image_block()]),
                is_error: false,
            },
        )
        .await
        .unwrap();

    let raw = tokio::fs::read_to_string(store.session_file_path(&s))
        .await
        .unwrap();
    assert_eq!(
        raw.lines()
            .filter(|l| l.contains("\"kind\":\"blob\""))
            .count(),
        2,
        "both the message and the tool result carry an image: {raw}"
    );
    assert!(!raw.contains("iVBORw0KGgo="));

    let loaded = store.load(s).await.unwrap();
    assert!(matches!(loaded[0].entry, LogEntry::User { .. }));
    assert!(matches!(loaded[1].entry, LogEntry::ToolResult { .. }));
}

#[tokio::test]
async fn a_snapshot_puts_an_externalized_image_back() {
    let (store, _cwd, _proj, _base) = make_store_with_blobs().await;
    let s = SessionId::new();
    store
        .append(
            s,
            LogEntry::User {
                content: vec![text_block("look"), image_block()],
            },
        )
        .await
        .unwrap();

    let snapshot = store.snapshot_lineage(s).await.unwrap();
    let LogEntry::User { content } = &snapshot.sessions[0].1[0].entry else {
        panic!(
            "the reference was not hydrated: {:?}",
            snapshot.sessions[0].1
        );
    };
    assert!(content.iter().any(|b| matches!(
        b,
        ContentBlock::Image {
            source: ImageSource::Base64 { data, .. }
        } if data == "iVBORw0KGgo="
    )));
}

#[tokio::test]
async fn load_messages_sees_hydrated_content() {
    let (store, _cwd, _proj, _base) = make_store_with_blobs().await;
    let s = SessionId::new();
    let big_text = "A".repeat(1500);
    store
        .append(
            s,
            LogEntry::User {
                content: vec![text_block(&big_text)],
            },
        )
        .await
        .unwrap();

    let messages = store.load_messages(s).await.unwrap();
    assert_eq!(messages.len(), 1);
    match &messages[0] {
        base::message::Message::User { content } => match &content[0] {
            ContentBlock::Text { text, .. } => assert_eq!(text, &big_text),
            other => panic!("expected Text block, got {other:?}"),
        },
        other => panic!("expected User message, got {other:?}"),
    }
}

/// The acceptance: uninstall the blob backend and the old log still opens.
#[tokio::test]
async fn a_log_written_with_a_blob_backend_loads_without_one() {
    let cwd = TempDir::new().unwrap();
    let projects = TempDir::new().unwrap();
    let paste_base = TempDir::new().unwrap();
    let s = SessionId::new();

    let roots = || HistoryRoots::under(projects.path());
    let with_blobs = JsonlHistoryStore::with_roots(cwd.path(), roots())
        .await
        .unwrap()
        .with_paste_store(PasteStore::new(paste_base.path()));
    with_blobs
        .append(
            s,
            LogEntry::System {
                subkind: crate::entry::SystemSubkind::Notice,
                text: "before".into(),
            },
        )
        .await
        .unwrap();
    with_blobs
        .append(
            s,
            LogEntry::User {
                content: vec![text_block(&"B".repeat(1500))],
            },
        )
        .await
        .unwrap();
    with_blobs
        .append(
            s,
            LogEntry::User {
                content: vec![text_block("after")],
            },
        )
        .await
        .unwrap();

    for (label, store) in [
        (
            "nothing mounted",
            JsonlHistoryStore::with_roots(cwd.path(), roots())
                .await
                .unwrap(),
        ),
        (
            "a different store mounted",
            JsonlHistoryStore::with_roots(cwd.path(), roots())
                .await
                .unwrap()
                .with_blob_store(Arc::new(InMemoryBlobStore::new())),
        ),
        (
            "the right store, emptied",
            JsonlHistoryStore::with_roots(cwd.path(), roots())
                .await
                .unwrap()
                .with_paste_store(PasteStore::new(TempDir::new().unwrap().path())),
        ),
    ] {
        let loaded = store
            .load(s)
            .await
            .unwrap_or_else(|e| panic!("{label}: a session must still load: {e:?}"));
        assert_eq!(
            loaded.len(),
            3,
            "{label}: an unreachable entry is still an entry"
        );
        assert!(
            matches!(loaded[1].entry, LogEntry::Blob { .. }),
            "{label}: expected the reference left in place, got {:?}",
            loaded[1].entry
        );
        assert_eq!(
            store.load_messages(s).await.unwrap().len(),
            1,
            "{label}: an unresolved reference is not something the model sees"
        );
    }
}

/// A reference names its store, and a store that is not the one named
/// does not get to answer for it — even when it happens to hold that id,
/// which content addressing makes likely rather than exotic. Resolving a
/// foreign id is how a backend with its own id space hands back the wrong
/// content.
#[tokio::test]
async fn a_reference_is_only_resolved_by_the_store_it_names() {
    let cwd = TempDir::new().unwrap();
    let projects = TempDir::new().unwrap();
    let paste_base = TempDir::new().unwrap();
    let s = SessionId::new();
    let entry = LogEntry::User {
        content: vec![text_block(&"C".repeat(1500))],
    };

    let written = JsonlHistoryStore::with_roots(cwd.path(), HistoryRoots::under(projects.path()))
        .await
        .unwrap()
        .with_paste_store(PasteStore::new(paste_base.path()));
    written.append(s, entry.clone()).await.unwrap();

    let impostor = InMemoryBlobStore::new();
    impostor
        .put(serde_json::to_string(&entry).unwrap().as_bytes())
        .unwrap();
    let reader = JsonlHistoryStore::with_roots(cwd.path(), HistoryRoots::under(projects.path()))
        .await
        .unwrap()
        .with_blob_store(Arc::new(impostor));

    assert!(
        matches!(
            reader.load(s).await.unwrap()[0].entry,
            LogEntry::Blob { .. }
        ),
        "a store that was not named must leave the reference alone"
    );
}
