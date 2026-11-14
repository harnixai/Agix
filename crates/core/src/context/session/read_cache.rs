//! 读缓存：模型读过哪些文件、读时的 mtime 与范围，供 staleness 和重复读取检测用。

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

/// 一次文件读取记录，包含读取时的 mtime。
/// `FileEdit` / `FileWrite` 的 `validate_input` 用它做 staleness 检测。
#[derive(Debug, Clone)]
pub struct FileReadEntry {
    /// 读取时的文件 mtime。
    pub mtime: SystemTime,
    /// 读取时的 offset（FileRead 的 offset 参数）。
    /// `None` 表示整文件读取（offset 未指定）。
    pub offset: Option<usize>,
    /// 读取时的 limit（FileRead 的 limit 参数）。
    /// `None` 表示读至文件末尾。
    pub limit: Option<usize>,
    /// Recorded by `Edit` / `Write` after they changed the file: the model
    /// knows what is in it, but saw a diff rather than the text, so a later
    /// `Read` still returns the content.
    pub written: bool,
}

#[derive(Debug, Default)]
pub(super) struct ReadCache {
    entries: std::sync::Mutex<HashMap<PathBuf, FileReadEntry>>,
    last_path: std::sync::Mutex<Option<PathBuf>>,
}

impl ReadCache {
    pub(super) fn record_read(&self, path: &Path, offset: Option<usize>, limit: Option<usize>) {
        if self.remember_file(path, offset, limit, false) {
            *self.last_path.lock().unwrap_or_else(|e| e.into_inner()) = Some(path.to_path_buf());
        }
    }

    pub(super) fn record_write(&self, path: &Path) {
        self.remember_file(path, None, None, true);
    }

    fn remember_file(
        &self,
        path: &Path,
        offset: Option<usize>,
        limit: Option<usize>,
        written: bool,
    ) -> bool {
        let Some(mtime) = std::fs::metadata(path).and_then(|m| m.modified()).ok() else {
            return false;
        };
        self.entries
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(
                path.to_path_buf(),
                FileReadEntry {
                    mtime,
                    offset,
                    limit,
                    written,
                },
            );
        true
    }

    pub(super) fn check_staleness(&self, path: &Path) -> Option<&'static str> {
        let meta = match std::fs::metadata(path) {
            Ok(m) => m,
            Err(_) => return None, // file doesn't exist — skip staleness check
        };
        let current_mtime = match meta.modified() {
            Ok(t) => t,
            Err(_) => return None, // can't determine mtime — skip check
        };
        let cache = self.entries.lock().unwrap_or_else(|e| e.into_inner());
        match cache.get(path) {
            None => Some("File must be read before editing. Use the Read tool first."),
            Some(entry) if entry.mtime != current_mtime => {
                Some("File has been modified since it was read. Re-read it first.")
            }
            _ => None, // fresh
        }
    }

    pub(super) fn check_dedup(
        &self,
        path: &Path,
        offset: Option<usize>,
        limit: Option<usize>,
    ) -> bool {
        let meta = match std::fs::metadata(path) {
            Ok(m) => m,
            Err(_) => return false,
        };
        let current_mtime = match meta.modified() {
            Ok(t) => t,
            Err(_) => return false,
        };
        let cache = self.entries.lock().unwrap_or_else(|e| e.into_inner());
        cache.get(path).is_some_and(|entry| {
            !entry.written
                && entry.mtime == current_mtime
                && entry.offset == offset
                && entry.limit == limit
        })
    }

    pub(super) fn forget(&self, paths: Option<&[PathBuf]>) {
        let mut entries = self.entries.lock().unwrap_or_else(|e| e.into_inner());
        let mut last = self.last_path.lock().unwrap_or_else(|e| e.into_inner());
        match paths {
            None => {
                entries.clear();
                *last = None;
            }
            Some(paths) => {
                for path in paths {
                    entries.remove(path);
                    if last.as_deref() == Some(path.as_path()) {
                        *last = None;
                    }
                }
            }
        }
    }

    pub(super) fn is_consecutive(&self, path: &Path) -> bool {
        let last = self.last_path.lock().unwrap_or_else(|e| e.into_inner());
        last.as_ref().is_some_and(|p| p == path)
    }
}
