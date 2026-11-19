//! SessionMemory — auto-maintenance of the `session_memory.md` sidecar file.
//!
//! Tracks extraction timestamps and staleness to prompt the model to update
//! its cross-session notes every N turns.
//!
//! The sidecar file is `session_memory.md` in the session's sidecar directory
//! (`HistoryStore::sidecar_locator`) and carries a YAML frontmatter block with
//! metadata:
//!
//! ```markdown
//! ---
//! extraction_started: <utc-rfc3339>
//! extraction_completed: <utc-rfc3339>
//! last_update_turn: <u32>
//! ---
//! # Session Memory
//! ```

use history::placement::SidecarLocator;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;
use tokio::sync::Mutex;

const SESSION_MEMORY_HEADER: &str =
    "# Session Memory\n\nTrack persistent facts about the user, project, and workflow.\n";

/// Manages the session_memory.md sidecar file with timestamp and staleness tracking.
///
/// Wrapped in `Arc<Mutex<>>` when shared so that mark-extraction and staleness
/// checks are safe across concurrent access.
#[derive(Clone)]
pub struct SessionMemory {
    inner: Arc<SessionMemoryInner>,
}

struct SessionMemoryInner {
    file: MemoryFile,
    /// The turn number when session memory was last updated.
    last_update_turn: AtomicU32,
    /// Internal mutability for file I/O.
    io_lock: Mutex<()>,
}

enum MemoryFile {
    Fixed(PathBuf),
    /// In the session's sidecar directory, wherever that is when asked: the
    /// session's `Meta`, written after this handle is made, decides it.
    Sidecar(SidecarLocator),
}

impl SessionMemory {
    /// Create a new SessionMemory handle pointing at `path`.
    ///
    /// The file is **not** created here — call [`init_session_memory`] to
    /// write the initial header.
    pub fn new(path: PathBuf) -> Self {
        Self::of(MemoryFile::Fixed(path))
    }

    /// `session_memory.md` in the sidecar directory `sidecar` finds.
    pub fn in_sidecar(sidecar: SidecarLocator) -> Self {
        Self::of(MemoryFile::Sidecar(sidecar))
    }

    fn of(file: MemoryFile) -> Self {
        Self {
            inner: Arc::new(SessionMemoryInner {
                file,
                last_update_turn: AtomicU32::new(0),
                io_lock: Mutex::new(()),
            }),
        }
    }

    /// Create the `session_memory.md` file with a default header if it does
    /// not already exist. Safe to call multiple times.
    pub async fn init_session_memory(&self) -> std::io::Result<()> {
        let _guard = self.inner.io_lock.lock().await;
        let path = &self.path()?;
        if path.symlink_metadata().is_ok() {
            return Ok(());
        }
        if let Some(parent) = path.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        let header = format!(
            "---\nextraction_started: ~\nextraction_completed: ~\nlast_update_turn: 0\n---\n\n{}",
            SESSION_MEMORY_HEADER
        );
        write_replacing(path, header).await
    }

    /// Record that extraction completed. Updates the completion timestamp and
    /// the `last_update_turn` counter so staleness can be computed.
    pub async fn mark_extraction_completed(&self, turn_count: u32) -> std::io::Result<()> {
        let _guard = self.inner.io_lock.lock().await;
        let now = iso_now();
        self.update_frontmatter_field("extraction_completed", &now)
            .await?;
        self.inner
            .last_update_turn
            .store(turn_count, Ordering::Release);
        // Also persist turn_count into the frontmatter
        self.update_frontmatter_field("last_update_turn", &turn_count.to_string())
            .await?;
        Ok(())
    }

    /// Returns `true` if `current_turn - last_update_turn > 10`, meaning the
    /// session notes have not been refreshed recently enough.
    pub fn is_stale(&self, current_turn: u32) -> bool {
        let last = self.inner.last_update_turn.load(Ordering::Acquire);
        let threshold: u32 = 10;
        current_turn.saturating_sub(last) > threshold
    }

    /// The turn number at which session memory was last updated.
    pub fn last_update_turn(&self) -> u32 {
        self.inner.last_update_turn.load(Ordering::Acquire)
    }

    /// Path to the session_memory.md sidecar file.
    pub fn path(&self) -> std::io::Result<PathBuf> {
        match &self.inner.file {
            MemoryFile::Fixed(path) => Ok(path.clone()),
            MemoryFile::Sidecar(sidecar) => sidecar.file(history::path::MEMORY_FILE).map_err(|e| {
                std::io::Error::new(std::io::ErrorKind::PermissionDenied, e.to_string())
            }),
        }
    }

    /// Read the full content of the session_memory.md file, if it exists.
    pub async fn load_content(&self) -> std::io::Result<Option<String>> {
        match read_own(&self.path()?).await {
            Ok(content) => Ok(Some(content)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e),
        }
    }

    /// Update a single frontmatter field in-place, preserving the rest of the
    /// file content. The frontmatter is a YAML block delimited by `---`.
    async fn update_frontmatter_field(&self, key: &str, value: &str) -> std::io::Result<()> {
        let path = &self.path()?;
        let content = match read_own(path).await {
            Ok(content) => content,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
            Err(e) => return Err(e),
        };

        let (frontmatter, body) = match base::text::frontmatter::split_frontmatter(&content) {
            (Some(fm), body) => (fm.trim().to_string(), body.trim_start().to_string()),
            (None, _) => (String::new(), content.clone()),
        };

        // Rebuild frontmatter: update matching line or append.
        let mut found = false;
        let mut new_fm = String::new();
        for line in frontmatter.lines() {
            if let Some(prefix) = line.split(':').next() {
                if prefix.trim() == key {
                    new_fm.push_str(&format!("{}: {}\n", key, value));
                    found = true;
                } else {
                    new_fm.push_str(line);
                    new_fm.push('\n');
                }
            } else {
                new_fm.push_str(line);
                new_fm.push('\n');
            }
        }
        if !found {
            new_fm.push_str(&format!("{}: {}\n", key, value));
        }

        write_replacing(path, format!("---\n{}---\n\n{}", new_fm, body)).await
    }
}

async fn read_own(path: &std::path::Path) -> std::io::Result<String> {
    let path = path.to_path_buf();
    tokio::task::spawn_blocking(move || base::fs::read_to_string_no_follow(&path))
        .await
        .map_err(std::io::Error::other)?
}

async fn write_replacing(path: &std::path::Path, content: String) -> std::io::Result<()> {
    let path = path.to_path_buf();
    tokio::task::spawn_blocking(move || {
        base::fs::write_atomic(&path, content.as_bytes(), base::fs::WriteOpts::FAST)
    })
    .await
    .map_err(std::io::Error::other)?
}

fn iso_now() -> String {
    let now = time::OffsetDateTime::now_utc();
    now.format(&time::format_description::well_known::Rfc3339)
        .unwrap_or_else(|_| "unknown".into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn sm(path: PathBuf) -> SessionMemory {
        SessionMemory::new(path)
    }

    #[tokio::test]
    async fn init_creates_file_with_header() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("session_memory.md");
        let sm = sm(path.clone());
        sm.init_session_memory().await.unwrap();

        assert!(path.exists());
        let content = tokio::fs::read_to_string(&path).await.unwrap();
        assert!(content.contains("Session Memory"));
        assert!(content.contains("extraction_started"));
    }

    #[tokio::test]
    async fn init_is_idempotent() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("session_memory.md");
        let sm = sm(path.clone());
        sm.init_session_memory().await.unwrap();
        // Second call should not error
        sm.init_session_memory().await.unwrap();
        assert!(path.exists());
    }

    #[tokio::test]
    async fn mark_extraction_updates_frontmatter() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("session_memory.md");
        let sm = sm(path.clone());
        sm.init_session_memory().await.unwrap();

        sm.mark_extraction_completed(5).await.unwrap();
        let content = tokio::fs::read_to_string(&path).await.unwrap();
        assert!(!content.contains("extraction_completed: ~"));
        assert!(content.contains("extraction_completed: "));
        assert!(content.contains("last_update_turn: 5"));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn a_memory_file_that_is_a_link_is_neither_read_nor_written_through() {
        let dir = TempDir::new().unwrap();
        let outside = dir.path().join("outside.md");
        std::fs::write(&outside, "---\nlast_update_turn: 0\n---\n\noutside").unwrap();
        let path = dir.path().join("session_memory.md");
        std::os::unix::fs::symlink(&outside, &path).unwrap();
        let sm = sm(path);
        sm.init_session_memory().await.unwrap();
        assert!(sm.load_content().await.is_err());
        let _ = sm.mark_extraction_completed(5).await;
        assert_eq!(
            std::fs::read_to_string(&outside).unwrap(),
            "---\nlast_update_turn: 0\n---\n\noutside"
        );
    }

    #[test]
    fn staleness_tracks_turn_delta() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("session_memory.md");
        let sm = sm(path);
        assert!(sm.last_update_turn() == 0);
        // At turn 0 with last_update=0 → 0-0=0 ≤ 10 → not stale
        assert!(!sm.is_stale(0));
        // At turn 20 with last_update=0 → 20 > 10 → stale
        assert!(sm.is_stale(20));
    }

    #[tokio::test]
    async fn mark_completed_resets_staleness() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("session_memory.md");
        let sm = sm(path);
        sm.init_session_memory().await.unwrap();

        sm.mark_extraction_completed(5).await.unwrap();
        assert!(!sm.is_stale(10)); // delta ≤ 10
        assert!(!sm.is_stale(15)); // delta = 10, not > 10
        assert!(sm.is_stale(16)); // delta = 11, > 10
    }

    #[tokio::test]
    async fn load_content_returns_none_for_missing_file() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("session_memory.md");
        let sm = sm(path);
        let content = sm.load_content().await.unwrap();
        assert!(content.is_none());
    }

    #[tokio::test]
    async fn load_content_returns_file_content() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("session_memory.md");
        let sm = sm(path.clone());
        sm.init_session_memory().await.unwrap();

        let content = sm.load_content().await.unwrap();
        assert!(content.is_some());
        assert!(content.unwrap().contains("Session Memory"));
    }
}
