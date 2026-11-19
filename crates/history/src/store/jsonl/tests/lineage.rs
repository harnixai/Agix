use super::blobs::text_block;
use super::*;
use crate::entry::{self, SessionKind};
use time::OffsetDateTime;

fn said(text: &str) -> LogEntry {
    LogEntry::User {
        content: vec![text_block(text)],
    }
}

fn sidechain_of(parent: SessionId) -> LogEntry {
    LogEntry::Meta {
        cwd: "/tmp".into(),
        started_at: OffsetDateTime::now_utc(),
        model: "m".into(),
        permission_mode: "default".into(),
        engine_version: "0".into(),
        agix_version: "0".into(),
        parent_session_id: Some(parent.to_string()),
        scene: None,
        project_root: None,
        session_kind: SessionKind::Sidechain,
        schema_version: entry::CURRENT_META_SCHEMA_VERSION,
    }
}

#[tokio::test]
async fn a_sidechain_deleted_between_measuring_and_reading_is_left_out() {
    let (store, _cwd, _proj) = make_store().await;
    let root = SessionId::new();
    let kept = SessionId::new();
    let gone = SessionId::new();
    store.append(root, said("root")).await.unwrap();
    store.append(kept, sidechain_of(root)).await.unwrap();
    store.append(gone, sidechain_of(root)).await.unwrap();

    let dir = store.dir_of(root).await;
    let (logs, lengths) = store.measure(root).await;
    assert_eq!(lengths.len(), 3);
    std::fs::remove_file(session_file(&dir, &gone)).unwrap();

    let snapshot = store.read_lineage(root, &logs, lengths).await.unwrap();
    let ids: Vec<SessionId> = snapshot.sessions.iter().map(|(s, _)| *s).collect();
    assert_eq!(ids, [root, kept]);
}

#[tokio::test]
async fn a_snapshot_stops_at_the_last_whole_line_it_measured() {
    let (store, _cwd, _proj) = make_store().await;
    let root = SessionId::new();
    store.append(root, said("first")).await.unwrap();
    let dir = store.dir_of(root).await;
    use std::io::Write;
    std::fs::OpenOptions::new()
        .append(true)
        .open(session_file(&dir, &root))
        .unwrap()
        .write_all(b"{\"half\": ")
        .unwrap();
    let (logs, lengths) = store.measure(root).await;

    let snapshot = store.read_lineage(root, &logs, lengths).await.unwrap();
    assert_eq!(snapshot.sessions[0].1.len(), 1);
}
