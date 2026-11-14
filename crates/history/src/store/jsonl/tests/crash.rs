use super::*;
use std::io::Write;
use tempfile::TempDir;

fn user(text: &str) -> LogEntry {
    LogEntry::User {
        content: vec![base::message::ContentBlock::Text {
            text: text.to_string(),
            cache_control: None,
        }],
    }
}

fn text_of(entry: &EnvelopedEntry) -> String {
    match &entry.entry {
        LogEntry::User { content } => match &content[0] {
            base::message::ContentBlock::Text { text, .. } => text.clone(),
            other => panic!("{other:?}"),
        },
        other => panic!("{other:?}"),
    }
}

async fn store_in(root: &TempDir) -> JsonlHistoryStore {
    JsonlHistoryStore::with_roots(root.path(), HistoryRoots::under(&root.path().join("state")))
        .await
        .unwrap()
}

fn append_raw(path: &Path, bytes: &[u8]) {
    let mut file = std::fs::OpenOptions::new().append(true).open(path).unwrap();
    file.write_all(bytes).unwrap();
}

#[tokio::test]
async fn a_last_line_cut_short_is_dropped_on_read_and_cut_off_before_the_next_append() {
    let cut_in_a_character = "{\"text\":\"中".as_bytes();
    for fragment in [
        &br#"{"uuid":"x","session_id":"#[..],
        &cut_in_a_character[..cut_in_a_character.len() - 1],
        b"\0\0\0\0",
    ] {
        let root = TempDir::new().unwrap();
        let store = store_in(&root).await;
        let s = SessionId::new();
        store.append(s, user("one")).await.unwrap();
        store.append(s, user("two")).await.unwrap();
        let path = store.session_path_of(s).await;
        append_raw(&path, fragment);

        let loaded = store
            .load(s)
            .await
            .expect("a cut last line is not a broken log");
        assert_eq!(
            loaded.iter().map(text_of).collect::<Vec<_>>(),
            ["one", "two"]
        );
        assert_eq!(
            store.session_facts(s).await.unwrap(),
            SessionFacts::default()
        );

        store.append(s, user("three")).await.unwrap();
        let loaded = store.load(s).await.unwrap();
        assert_eq!(
            loaded.iter().map(text_of).collect::<Vec<_>>(),
            ["one", "two", "three"]
        );
        assert!(std::fs::read_to_string(&path).unwrap().ends_with('\n'));
    }
}

#[tokio::test]
async fn a_whole_last_line_missing_only_its_newline_is_kept() {
    let root = TempDir::new().unwrap();
    let store = store_in(&root).await;
    let s = SessionId::new();
    store.append(s, user("one")).await.unwrap();
    let path = store.session_path_of(s).await;
    let whole = std::fs::read(&path).unwrap();
    std::fs::write(&path, &whole[..whole.len() - 1]).unwrap();

    assert_eq!(store.load(s).await.unwrap().len(), 1);
    store.append(s, user("two")).await.unwrap();
    let loaded = store.load(s).await.unwrap();
    assert_eq!(
        loaded.iter().map(text_of).collect::<Vec<_>>(),
        ["one", "two"]
    );
}

#[tokio::test]
async fn a_session_with_a_broken_log_does_not_hide_another_ones_children() {
    let root = TempDir::new().unwrap();
    let store = store_in(&root).await;
    let parent = SessionId::new();
    let broken = SessionId::new();
    store.append(parent, user("p")).await.unwrap();
    store.append(broken, user("b")).await.unwrap();
    let path = store.session_path_of(broken).await;
    std::fs::write(&path, "not json\n{}\n").unwrap();
    assert!(store
        .child_sessions(&parent.to_string())
        .await
        .unwrap()
        .is_empty());
}

#[tokio::test]
async fn a_broken_line_mid_log_leaves_only_that_session_out_of_a_search() {
    let root = TempDir::new().unwrap();
    let store = store_in(&root).await;
    let good = SessionId::new();
    let broken = SessionId::new();
    store.append(good, user("needle one")).await.unwrap();
    store.append(broken, user("needle two")).await.unwrap();
    store.append(broken, user("needle three")).await.unwrap();
    let path = store.session_path_of(broken).await;
    let log = std::fs::read_to_string(&path).unwrap();
    std::fs::write(&path, log.replacen('\n', "\nnot json\n", 1)).unwrap();
    assert!(matches!(
        store.load(broken).await,
        Err(HistoryError::Parse { .. })
    ));

    let here = store
        .find_sessions(&SessionQuery::matching("needle", 10))
        .await
        .unwrap();
    let everywhere = store
        .find_sessions(&SessionQuery::matching("needle", 10).within(SessionScope::AllProjects))
        .await
        .unwrap();
    for found in [here, everywhere] {
        assert_eq!(
            found.iter().map(|s| s.session_id).collect::<Vec<_>>(),
            [good]
        );
    }
}

fn sidecar_of(root: &TempDir, session: SessionId) -> PathBuf {
    session_sidecar_dir(&root.path().join("state").join("sessions"), &session)
}

fn compacted() -> LogEntry {
    LogEntry::Compact {
        before_tokens: 10,
        after_tokens: 5,
        summary_block_id: None,
        replacement_history: None,
        summary: None,
        snip_removed_uuids: None,
    }
}

#[tokio::test]
async fn delete_takes_the_whole_sidecar_with_it() {
    let root = TempDir::new().unwrap();
    let store = store_in(&root).await;
    let s = SessionId::new();
    store.append(s, user("one")).await.unwrap();
    store.append(s, compacted()).await.unwrap();
    store
        .set_session_title(s, Some("named".into()))
        .await
        .unwrap();
    let sidecar = sidecar_of(&root, s);
    std::fs::write(sidecar.join("session_memory.md"), "what was said").unwrap();
    std::fs::create_dir_all(sidecar.join("nested")).unwrap();
    std::fs::write(sidecar.join("nested/x"), "x").unwrap();

    store.delete(s).await.unwrap();

    assert!(!store.session_path_of(s).await.exists());
    assert!(!sidecar.exists());
    store.delete(s).await.unwrap();
    assert!(matches!(
        store.append(s, user("again")).await,
        Err(crate::error::HistoryError::SessionDeleted(_))
    ));
    assert!(!sidecar.exists());
    assert_eq!(
        store.session_stats(s).await.unwrap(),
        SessionStats::default()
    );
}

#[cfg(unix)]
#[tokio::test]
async fn delete_under_a_live_claim_keeps_only_the_lock_file() {
    let root = TempDir::new().unwrap();
    let store = store_in(&root).await;
    let s = SessionId::new();
    store.append(s, user("one")).await.unwrap();
    let claim = store.claim(s).await.unwrap();
    let sidecar = sidecar_of(&root, s);
    std::fs::write(sidecar.join("session_memory.md"), "what was said").unwrap();

    store.delete(s).await.unwrap();

    let left: Vec<_> = std::fs::read_dir(&sidecar)
        .unwrap()
        .map(|e| e.unwrap().file_name())
        .collect();
    assert_eq!(left, [std::ffi::OsString::from("writer.lock")]);
    assert!(
        lock_elsewhere(&sidecar.join("writer.lock")).is_none(),
        "the claim still keeps other processes out"
    );
    drop(claim);
    assert!(lock_elsewhere(&sidecar.join("writer.lock")).is_some());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn renaming_while_compacting_loses_neither_the_name_nor_a_count() {
    let root = TempDir::new().unwrap();
    let store = Arc::new(store_in(&root).await);
    let s = SessionId::new();
    store.append(s, user("one")).await.unwrap();
    let mut tasks = tokio::task::JoinSet::new();
    for _ in 0..100 {
        let renaming = store.clone();
        tasks.spawn(async move { renaming.set_session_title(s, Some("named".into())).await });
        let compacting = store.clone();
        tasks.spawn(async move { compacting.append(s, compacted()).await });
    }
    while let Some(done) = tasks.join_next().await {
        done.unwrap().unwrap();
    }

    let stats = store.session_stats(s).await.unwrap();
    assert_eq!(stats.title.as_deref(), Some("named"));
    assert_eq!(stats.compact_count, 100);
    assert_eq!(store.load(s).await.unwrap().len(), 101);
}

/// A blob store standing in for a disk that has stopped answering for
/// one session's writes.
struct StuckDisk {
    entered: std::sync::mpsc::SyncSender<()>,
    release: std::sync::Mutex<std::sync::mpsc::Receiver<()>>,
}

impl BlobStore for StuckDisk {
    fn name(&self) -> &str {
        "stuck"
    }

    fn put(&self, _bytes: &[u8]) -> Result<String, std::io::Error> {
        self.entered.send(()).unwrap();
        self.release.lock().unwrap().recv().unwrap();
        Ok("0123456789abcdef".into())
    }

    fn get(&self, _id: &str) -> Result<Option<Vec<u8>>, std::io::Error> {
        Ok(None)
    }
}

#[tokio::test]
async fn a_session_whose_write_is_stuck_does_not_hold_up_another() {
    let root = TempDir::new().unwrap();
    let (entered_tx, entered_rx) = std::sync::mpsc::sync_channel(1);
    let (release_tx, release_rx) = std::sync::mpsc::channel();
    let store = Arc::new(store_in(&root).await.with_blob_store(Arc::new(StuckDisk {
        entered: entered_tx,
        release: std::sync::Mutex::new(release_rx),
    })));
    let stuck = SessionId::new();
    let free = SessionId::new();
    store.append(stuck, user("before")).await.unwrap();

    let writing = store.clone();
    let stuck_append = tokio::spawn(async move {
        writing
            .append(
                stuck,
                LogEntry::User {
                    content: vec![base::message::ContentBlock::Image {
                        source: base::message::ImageSource::Base64 {
                            media_type: "image/png".into(),
                            data: "iVBORw0KGgo=".into(),
                        },
                    }],
                },
            )
            .await
    });
    tokio::task::spawn_blocking(move || entered_rx.recv())
        .await
        .unwrap()
        .unwrap();

    tokio::time::timeout(std::time::Duration::from_secs(10), async {
        store.append(free, user("not waiting")).await.unwrap();
        store.load(free).await.unwrap();
        store.snapshot_lineage(free).await.unwrap();
    })
    .await
    .expect("another session's append went through while this one is stuck");

    release_tx.send(()).unwrap();
    stuck_append.await.unwrap().unwrap();
    assert_eq!(store.load(stuck).await.unwrap().len(), 2);
}

#[tokio::test]
async fn sync_flushes_an_existing_log_and_ignores_a_missing_one() {
    let root = TempDir::new().unwrap();
    let store = store_in(&root).await;
    let s = SessionId::new();
    store.sync(s).await.unwrap();
    store.append(s, user("one")).await.unwrap();
    store.sync(s).await.unwrap();
    store.sync(s).await.unwrap();
}

#[cfg(unix)]
fn lock_elsewhere(path: &Path) -> Option<std::fs::File> {
    use std::os::fd::AsRawFd;
    let file = std::fs::OpenOptions::new().write(true).open(path).unwrap();
    // SAFETY: `flock` only reads the descriptor, which `file` keeps open.
    let rc = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
    (rc == 0).then_some(file)
}

#[cfg(unix)]
#[tokio::test]
async fn a_session_another_process_is_writing_is_refused() {
    let root = TempDir::new().unwrap();
    let store = store_in(&root).await;
    let s = SessionId::new();
    store.append(s, user("one")).await.unwrap();

    let lock_path =
        session_sidecar_dir(&root.path().join("state").join("sessions"), &s).join("writer.lock");
    let other = lock_elsewhere(&lock_path).expect("nobody holds it between appends");

    assert!(matches!(
        store.append(s, user("two")).await,
        Err(HistoryError::Locked(_))
    ));
    assert!(matches!(store.claim(s).await, Err(HistoryError::Locked(_))));
    assert!(matches!(
        store.delete(s).await,
        Err(HistoryError::Locked(_))
    ));
    assert_eq!(
        store.load(s).await.unwrap().len(),
        1,
        "reading is not writing"
    );

    drop(other);
    let claim = store.claim(s).await.unwrap();
    store.append(s, user("two")).await.unwrap();
    assert!(
        lock_elsewhere(&lock_path).is_none(),
        "a claim keeps other processes out between appends"
    );
    drop(claim);
    assert!(lock_elsewhere(&lock_path).is_some());
}

/// Stops one log's next line halfway onto the disk until the test lets
/// it finish.
pub(in crate::store) mod mid_write {
    use std::collections::HashMap;
    use std::path::{Path, PathBuf};
    use std::sync::{mpsc, LazyLock, Mutex};

    pub(crate) struct Pause {
        pub(crate) entered: mpsc::SyncSender<()>,
        pub(crate) release: mpsc::Receiver<()>,
        pub(crate) finished: mpsc::SyncSender<()>,
    }

    static ARMED: LazyLock<Mutex<HashMap<PathBuf, Pause>>> = LazyLock::new(Default::default);

    pub(crate) fn arm(path: &Path, pause: Pause) {
        ARMED.lock().unwrap().insert(path.to_path_buf(), pause);
    }

    pub(crate) fn write_all(
        file: &mut std::fs::File,
        path: &Path,
        line: &[u8],
    ) -> std::io::Result<()> {
        use std::io::Write;
        let Some(pause) = ARMED.lock().unwrap().remove(path) else {
            return file.write_all(line);
        };
        let (head, rest) = line.split_at(line.len() / 2);
        file.write_all(head)?;
        file.flush()?;
        pause.entered.send(()).unwrap();
        pause.release.recv().unwrap();
        file.write_all(rest)?;
        let _ = pause.finished.send(());
        Ok(())
    }
}

#[tokio::test]
async fn an_append_given_up_mid_write_finishes_its_line_before_the_next_append() {
    use std::sync::mpsc;
    let root = TempDir::new().unwrap();
    let store = Arc::new(store_in(&root).await);
    let s = SessionId::new();
    store.append(s, user("one")).await.unwrap();
    let path = store.session_path_of(s).await;

    let (entered_tx, entered_rx) = mpsc::sync_channel(1);
    let (release_tx, release_rx) = mpsc::channel();
    let (finished_tx, finished_rx) = mpsc::sync_channel(1);
    mid_write::arm(
        &path,
        mid_write::Pause {
            entered: entered_tx,
            release: release_rx,
            finished: finished_tx,
        },
    );
    let giving_up = store.clone();
    let two = tokio::spawn(async move { giving_up.append(s, user("two")).await });
    tokio::task::spawn_blocking(move || entered_rx.recv())
        .await
        .unwrap()
        .unwrap();
    two.abort();
    assert!(two.await.unwrap_err().is_cancelled());

    // A lock let go mid-write lets the next append run to its end while
    // the half-written line waits; a lock still held makes it wait.
    if store.log_of(s).try_lock().is_ok() {
        store.append(s, user("three")).await.unwrap();
        release_tx.send(()).unwrap();
    } else {
        release_tx.send(()).unwrap();
        store.append(s, user("three")).await.unwrap();
    }
    tokio::task::spawn_blocking(move || finished_rx.recv())
        .await
        .unwrap()
        .unwrap();

    let raw = std::fs::read_to_string(&path).unwrap();
    let texts: Vec<String> = raw
        .lines()
        .map(|line| {
            let env = serde_json::from_str::<EnvelopedEntry>(line)
                .unwrap_or_else(|e| panic!("a broken line in the log: {line:?}: {e}"));
            text_of(&env)
        })
        .collect();
    assert_eq!(texts, ["one", "two", "three"]);
}
