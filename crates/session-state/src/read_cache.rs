//! 读缓存：模型读过哪些文件、读时的 mtime 与范围，供 staleness 和重复读取检测用。

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use base::context::{HeldRead, ReadRecord};

/// 一次文件读取记录，包含读取时的 mtime。
/// `FileEdit` / `FileWrite` 的 `validate_input` 用它做 staleness 检测。
#[derive(Debug, Clone)]
pub(crate) struct FileReadEntry {
    /// 读取时的文件 mtime。
    pub mtime: SystemTime,
    /// 读取时的 offset（FileRead 的 offset 参数）。
    /// `None` 表示整文件读取（offset 未指定）。
    pub offset: Option<usize>,
    /// 读取时的 limit（FileRead 的 limit 参数）。
    /// `None` 表示读至文件末尾。
    pub limit: Option<usize>,
    /// The `Read` call whose result holds the text in the model's context.
    /// `None` once nothing there does: `Edit` / `Write` changed the file and
    /// the model saw a diff, or that result was cleared or compacted away.
    /// Only a held read answers a repeat from the cache.
    pub held_by: Option<String>,
    pub cut_short: bool,
}

#[derive(Debug, Default)]
pub(crate) struct ReadCache {
    entries: std::sync::Mutex<HashMap<PathBuf, FileReadEntry>>,
    last_path: std::sync::Mutex<Option<PathBuf>>,
}

impl ReadCache {
    pub(crate) fn record_read(&self, read: ReadRecord<'_>) {
        let held_by = Some(read.tool_use_id.to_string());
        if self.remember_file(read.path, read.offset, read.limit, held_by, read.cut_short) {
            *self.last_path.lock().unwrap_or_else(|e| e.into_inner()) =
                Some(read.path.to_path_buf());
        }
    }

    pub(crate) fn record_write(&self, path: &Path) {
        self.remember_file(path, None, None, None, false);
    }

    fn remember_file(
        &self,
        path: &Path,
        offset: Option<usize>,
        limit: Option<usize>,
        held_by: Option<String>,
        cut_short: bool,
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
                    held_by,
                    cut_short,
                },
            );
        true
    }

    pub(crate) fn check_staleness(&self, path: &Path) -> Option<&'static str> {
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

    pub(crate) fn check_dedup(
        &self,
        path: &Path,
        offset: Option<usize>,
        limit: Option<usize>,
    ) -> Option<HeldRead> {
        let current_mtime = std::fs::metadata(path).and_then(|m| m.modified()).ok()?;
        let cache = self.entries.lock().unwrap_or_else(|e| e.into_inner());
        let entry = cache.get(path)?;
        let same = entry.held_by.is_some()
            && entry.mtime == current_mtime
            && entry.offset == offset
            && entry.limit == limit;
        same.then_some(match entry.cut_short {
            true => HeldRead::CutShort,
            false => HeldRead::Whole,
        })
    }

    pub(crate) fn release(&self, tool_use_ids: &[String]) {
        let mut entries = self.entries.lock().unwrap_or_else(|e| e.into_inner());
        for entry in entries.values_mut() {
            if entry
                .held_by
                .as_ref()
                .is_some_and(|by| tool_use_ids.contains(by))
            {
                entry.held_by = None;
            }
        }
    }

    pub(crate) fn forget(&self, paths: Option<&[PathBuf]>) {
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

    pub(crate) fn is_consecutive(&self, path: &Path) -> bool {
        let last = self.last_path.lock().unwrap_or_else(|e| e.into_inner());
        last.as_ref().is_some_and(|p| p == path)
    }
}
