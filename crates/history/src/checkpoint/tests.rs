use super::*;
use crate::store::{HistoryStore, InMemoryHistoryStore, JsonlHistoryStore, ObservedHistoryStore};
use base::session::SessionId;
use std::sync::Arc;

#[test]
fn an_object_is_named_by_its_whole_sha256_and_stored_once() {
    let dir = tempfile::TempDir::new().unwrap();
    let objects = CheckpointObjects::new(dir.path().join("objects"));
    let id = objects.put(b"fn main() {}\n").unwrap();
    assert_eq!(id.len(), 64);
    assert_eq!(id, sha256_hex(b"fn main() {}\n"));
    assert_eq!(objects.put(b"fn main() {}\n").unwrap(), id);
    assert_eq!(
        std::fs::read_dir(dir.path().join("objects"))
            .unwrap()
            .count(),
        1
    );
    assert_eq!(
        objects.get(&id).unwrap().as_deref(),
        Some(&b"fn main() {}\n"[..])
    );
}

#[test]
fn an_object_whose_bytes_no_longer_match_its_name_is_missing() {
    let dir = tempfile::TempDir::new().unwrap();
    let objects = CheckpointObjects::new(dir.path().join("objects"));
    let id = objects.put(b"original").unwrap();
    std::fs::write(dir.path().join("objects").join(&id), b"tampered").unwrap();
    assert_eq!(objects.get(&id).unwrap(), None);
    assert_eq!(objects.get("../../etc/passwd").unwrap(), None);
}

#[cfg(unix)]
#[test]
fn objects_are_private_to_their_owner() {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::TempDir::new().unwrap();
    let objects = CheckpointObjects::new(dir.path().join("checkpoints/objects"));
    let id = objects.put(b"secret").unwrap();
    let mode = |p: &std::path::Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode(&dir.path().join("checkpoints/objects")), 0o700);
    assert_eq!(
        mode(&dir.path().join("checkpoints/objects").join(id)),
        0o600
    );
}

#[test]
fn a_recorded_path_inside_the_project_is_relative() {
    let root = std::path::Path::new("/repo");
    let rel = RecordedPath::of(std::path::Path::new("/repo/src/a.rs"), Some(root)).unwrap();
    assert_eq!(rel, RecordedPath::Rel("src/a.rs".into()));
    assert_eq!(
        serde_json::to_value(&rel).unwrap(),
        serde_json::json!({ "rel": "src/a.rs" })
    );
    assert_eq!(
        rel.resolve(Some(std::path::Path::new("/elsewhere"))),
        Some(std::path::PathBuf::from("/elsewhere/src/a.rs"))
    );
    let abs = RecordedPath::of(std::path::Path::new("/tmp/x"), Some(root)).unwrap();
    assert!(abs.outside_project());
}

#[tokio::test]
async fn every_shipped_store_and_its_observed_wrapper_keep_objects() {
    let dir = tempfile::TempDir::new().unwrap();
    let roots = crate::path::HistoryRoots::under(dir.path());
    let jsonl: Arc<dyn HistoryStore> = Arc::new(
        JsonlHistoryStore::with_roots(dir.path(), roots)
            .await
            .unwrap(),
    );
    let memory: Arc<dyn HistoryStore> = Arc::new(InMemoryHistoryStore::new());
    for store in [jsonl, memory] {
        let observed = ObservedHistoryStore::new(store.clone(), Vec::new());
        let root = SessionId::new();
        let id = observed
            .checkpoint_objects(root)
            .expect("forwarded")
            .put(b"before")
            .unwrap();
        let again = store.checkpoint_objects(root).unwrap().get(&id).unwrap();
        assert_eq!(again.as_deref(), Some(&b"before"[..]));
    }
}

#[tokio::test]
async fn deleting_the_session_takes_its_objects() {
    let dir = tempfile::TempDir::new().unwrap();
    let roots = crate::path::HistoryRoots::under(dir.path());
    let sessions = roots.sessions.clone();
    let store = JsonlHistoryStore::with_roots(dir.path(), roots)
        .await
        .unwrap();
    let root = SessionId::new();
    store
        .append(
            root,
            crate::entry::LogEntry::User {
                content: Vec::new(),
            },
        )
        .await
        .unwrap();
    store
        .checkpoint_objects(root)
        .unwrap()
        .put(b"before")
        .unwrap();
    let objects = crate::path::session_checkpoint_dir(&sessions, &root);
    assert!(objects.join("objects").is_dir());
    store.delete(root).await.unwrap();
    assert!(!objects.exists());
}
