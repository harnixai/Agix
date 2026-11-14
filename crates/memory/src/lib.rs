//! Durable memory — cross-session persistent knowledge, file-based.
//!
//! - Directories: the global `<global_data_dir>/memory/` and the project's
//!   `<local_data_dir>/memory/`, both from [`ConfigPaths`] — see
//!   [`MemoryStore::for_paths`], the one place they are derived
//! - Each memory is a `.md` file with YAML frontmatter
//! - `MEMORY.md` is the index (one line per memory entry)
//! - Secret scan before a write, the memory system prompt, and the LLM
//!   selection call behind recall

mod prompt;
mod secrets;
mod select;

pub use prompt::{build_memory_prompt, index_for_prompt, memory_prompt, DefaultExtraction};
pub use secrets::{scan_for_secrets, SecretPattern};
pub use select::{select_memories_from, LlmRetriever};

use base::interface::memory_contracts::MemoryStorage;
use base::memory::{DurableMemory, MemoryError, MemoryType};
use base::paths::{ConfigPaths, PromptFiles};
use base::text::frontmatter::{scalar, split_frontmatter};
use std::path::{Path, PathBuf};

/// How many memories extraction fills the store to: the most the index
/// shows, and past it the recall selection call carries every header there
/// is. A store at the cap gets no new memories; nothing is deleted to make
/// room, that is the user's call.
pub const MAX_MEMORIES: usize = 200;

#[derive(Debug, Clone)]
pub struct ScopedMemory {
    pub memory: DurableMemory,
    pub scope: MemoryScope,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum MemoryScope {
    /// Global → write to user_data_dir
    User,
    /// Project-specific → write to local_data_dir
    Local,
}

/// Persistent memory store using file-per-memory + MEMORY.md index.
///
/// The store spans two directories — a global one (`user_dir`) and a
/// project-local one (`local_dir`) — and every operation must respect that
/// split. The *read* path (`load_all`) merges both directories into one flat
/// list, so the natural `load_all()` → mutate → `persist_batch()` round trip
/// has to write each memory back where it came from.
///
/// Writes follow *provenance* rather than an explicit scope argument: since
/// memory identity is the file name, the directory a memory currently lives in
/// is recoverable from disk. `persist_batch` resolves each entry's home
/// directory that way (see [`MemoryStore::home_dir_for`]), which keeps the
/// existing `Vec<DurableMemory>` signature — used by callers that have no scope
/// to give — round-trip safe. Callers that *do* know the scope (memory
/// extraction classifying a memory as `project`) use
/// [`MemoryStore::persist_batch_scoped`], which overrides the resolution.
///
/// Indices are never built from a caller-supplied list:
/// [`MemoryStore::rebuild_index_from_disk`] regenerates a directory's
/// `MEMORY.md` from the `.md` files actually in it, so an index can never
/// shrink to just the batch that was written.
pub struct MemoryStore {
    user_dir: PathBuf,
    local_dir: PathBuf,
    state_root: Option<PathBuf>,
}

/// File name for the index.
pub const INDEX_FILE: &str = "MEMORY.md";

impl MemoryStore {
    pub fn new(user_dir: PathBuf, local_dir: PathBuf) -> Self {
        Self {
            user_dir,
            local_dir,
            state_root: None,
        }
    }

    /// The store a session uses: the global memory directory and the
    /// project's, as `paths` names them.
    pub fn for_paths(paths: &ConfigPaths) -> Self {
        Self {
            state_root: Some(paths.global_data_dir.clone()),
            ..Self::new(paths.global_memory_dir(), paths.local_memory_dir())
        }
    }

    /// How a file in `dir` is read: memories are files the store writes
    /// there itself, so one that resolves anywhere else is not one.
    fn files_in(&self, dir: &Path) -> PromptFiles {
        base::paths::SensitivePaths::new(self.state_root.as_deref(), None).prompt_files_within(dir)
    }

    /// Where a memory with no other home is written — the directory the
    /// model is told to write to.
    pub fn user_dir(&self) -> &Path {
        &self.user_dir
    }

    /// The distinct directories this store owns, most specific first.
    ///
    /// `local_dir` comes first so it shadows `user_dir` on a name collision.
    /// Callers are allowed to point both scopes at the same directory (team
    /// memory is a single flat namespace and does exactly that) — collapsing
    /// the duplicate here is what stops such a store from seeing every file,
    /// and every index line, twice.
    fn dirs(&self) -> Vec<&Path> {
        if self.user_dir == self.local_dir {
            vec![self.local_dir.as_path()]
        } else {
            vec![self.local_dir.as_path(), self.user_dir.as_path()]
        }
    }

    /// Load all memories together with the directory each one came from.
    ///
    /// This is the provenance-preserving read that the scope-aware write paths
    /// build on. `load_all` is the same thing with the directory dropped.
    fn load_all_with_home(&self) -> Vec<(PathBuf, DurableMemory)> {
        let mut out: Vec<(PathBuf, DurableMemory)> = Vec::new();
        for dir in self.dirs() {
            for mem in Self::load_from_dir(dir, &self.files_in(dir)) {
                // First writer wins: `dirs()` yields local before user, so a
                // project-local memory shadows a global one with the same name.
                if out.iter().any(|(_, m)| m.name == mem.name) {
                    continue;
                }
                out.push((dir.to_path_buf(), mem));
            }
        }
        // Most recent first. `name` breaks ties so the order is stable —
        // `read_dir` order is not, and `last_seen` is frequently equal.
        out.sort_by(|(_, a), (_, b)| {
            b.last_seen
                .cmp(&a.last_seen)
                .then_with(|| a.name.cmp(&b.name))
        });
        out
    }

    /// Load the memories of both directories (local overrides user on the
    /// same name), most recent first and no more than [`MAX_MEMORIES`]: a
    /// file written into a directory without going through the store — the
    /// `Write` tool needs none — is not a way past the cap.
    pub fn load_all(&self) -> Vec<DurableMemory> {
        let mut all = self.load_all_with_home();
        if all.len() > MAX_MEMORIES {
            tracing::warn!(
                held = all.len(),
                cap = MAX_MEMORIES,
                "more memories on disk than the cap; the oldest are not read"
            );
            all.truncate(MAX_MEMORIES);
        }
        all.into_iter().map(|(_, m)| m).collect()
    }

    /// How many distinct memories are on disk, past the cap included.
    pub fn count(&self) -> usize {
        self.load_all_with_home().len()
    }

    /// Load MEMORY.md index content across both directories.
    ///
    /// Entries are deduplicated by the memory they point at: a name present in
    /// both directories is one memory (`load_all` shadows it), so listing it
    /// twice would tell the model it has two distinct memories.
    pub fn load_index(&self) -> String {
        let mut lines: Vec<String> = Vec::new();
        let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
        let (mut entries, mut dropped) = (0usize, 0usize);
        for dir in self.dirs() {
            let Ok(s) = self.files_in(dir).read_to_string(&dir.join(INDEX_FILE)) else {
                continue;
            };
            for line in s.lines() {
                // Dedup key: the entry's memory name when the line is a real
                // index entry (`- [name](name.md) — description`), otherwise
                // the line text itself (headings, blank lines, prose).
                let key = index_line_name(line).unwrap_or_else(|| line.trim().to_string());
                if key.is_empty() {
                    lines.push(line.to_string());
                    continue;
                }
                if seen.contains(&key) {
                    continue;
                }
                let entry = index_line_name(line).is_some();
                if entry && entries >= MAX_MEMORIES {
                    dropped += 1;
                    continue;
                }
                entries += usize::from(entry);
                seen.insert(key);
                lines.push(line.to_string());
            }
        }
        if dropped > 0 {
            tracing::warn!(
                dropped,
                cap = MAX_MEMORIES,
                "the memory index lists more entries than the cap; the rest are not shown"
            );
        }
        let mut content = lines.join("\n");
        if !content.is_empty() {
            content.push('\n');
        }
        content
    }

    /// Persist a batch of memories, each into the directory it already lives in.
    ///
    /// New names default to `user_dir`. Existing names stay put — this is the
    /// path a `load_all()` → mutate → persist round trip goes through, and it
    /// must not relocate project-local memories into the global directory.
    pub fn persist_batch(&self, memories: Vec<DurableMemory>) -> Result<usize, MemoryError> {
        let scoped = memories
            .into_iter()
            .map(|memory| {
                let scope = if self.home_dir_for(&memory.name) == self.local_dir {
                    MemoryScope::Local
                } else {
                    MemoryScope::User
                };
                ScopedMemory { memory, scope }
            })
            .collect();
        self.persist_batch_scoped(scoped)
    }

    /// Persist with explicit scope routing, overriding provenance.
    ///
    /// Use this when the caller knows where a memory belongs — e.g. turn-end
    /// extraction classifying a memory as `project` and routing it to the
    /// project-local directory.
    pub fn persist_batch_scoped(&self, memories: Vec<ScopedMemory>) -> Result<usize, MemoryError> {
        let mut saved = 0usize;
        for (dir, scope) in [
            (&self.user_dir, MemoryScope::User),
            (&self.local_dir, MemoryScope::Local),
        ] {
            let batch: Vec<&DurableMemory> = memories
                .iter()
                .filter(|sm| sm.scope == scope && sm.memory.confidence >= 0.3)
                .map(|sm| &sm.memory)
                .collect();
            if batch.is_empty() {
                continue;
            }
            let _writing = lock_dir(dir)?;
            for memory in &batch {
                Self::write_memory_file(dir, memory)?;
            }
            saved += batch.len();
            // Rebuilt from the directory's files rather than from the batch —
            // an index of the batch alone drops every memory it didn't mention.
            Self::rebuild_index_from_disk(dir, &self.files_in(dir))?;
        }
        Ok(saved)
    }

    /// Count one more recall of each of `names`, in the directory each lives in.
    ///
    /// Each file is re-read under its directory's lock, so two sessions
    /// recalling the same memory at once both count.
    pub fn record_recalled(&self, names: &[String]) -> Result<(), MemoryError> {
        for (dir, memory) in self.load_all_with_home() {
            if !names.contains(&memory.name) {
                continue;
            }
            let _writing = lock_dir(&dir)?;
            let path = dir.join(format!("{}.md", sanitize_filename(&memory.name)));
            let Some(mut current) = self
                .files_in(&dir)
                .read_to_string(&path)
                .ok()
                .and_then(|raw| Self::parse_memory_file(&raw))
            else {
                continue;
            };
            current.recall_count += 1;
            Self::write_memory_file(&dir, &current)?;
        }
        Ok(())
    }

    /// The directory a memory with this name currently lives in, or `user_dir`
    /// if it is new. Provenance is recovered from disk because `DurableMemory`
    /// itself carries no scope (it is serialized into the `.md` files and
    /// constructed by callers all over the workspace).
    fn home_dir_for(&self, name: &str) -> PathBuf {
        let file = format!("{}.md", sanitize_filename(name));
        for dir in self.dirs() {
            if dir.join(&file).exists() {
                return dir.to_path_buf();
            }
        }
        self.user_dir.clone()
    }

    /// Search memories by query (simple substring match on name + description + content).
    pub fn search(&self, query: &str) -> Vec<DurableMemory> {
        self.load_all()
            .into_iter()
            .filter(|m| {
                m.name.contains(query) || m.description.contains(query) || m.content.contains(query)
            })
            .collect()
    }

    /// Remove a memory by name from every directory that holds it.
    pub fn remove(&self, name: &str) -> Result<bool, MemoryError> {
        let file = format!("{}.md", sanitize_filename(name));
        let mut removed = false;
        for dir in self.dirs() {
            let path = dir.join(&file);
            if !path.exists() {
                continue;
            }
            let _writing = lock_dir(dir)?;
            if path.exists() {
                std::fs::remove_file(&path).map_err(|e| MemoryError::Io(e.to_string()))?;
                Self::rebuild_index_from_disk(dir, &self.files_in(dir))?;
                removed = true;
            }
        }
        Ok(removed)
    }

    fn memory_dir(dir: &Path) -> PathBuf {
        dir.to_path_buf()
    }

    fn load_from_dir(dir: &Path, files: &PromptFiles) -> Vec<DurableMemory> {
        let md = Self::memory_dir(dir);
        let mut memories = Vec::new();
        if let Ok(entries) = std::fs::read_dir(&md) {
            for entry in entries.flatten() {
                let p = entry.path();
                if p.extension().is_some_and(|e| e == "md")
                    && p.file_name() != Some(std::ffi::OsStr::new(INDEX_FILE))
                {
                    if let Ok(content) = files.read_to_string(&p) {
                        if let Some(mem) = Self::parse_memory_file(&content) {
                            memories.push(mem);
                        }
                    }
                }
            }
        }
        memories
    }

    fn parse_memory_file(raw: &str) -> Option<DurableMemory> {
        let (frontmatter, body) = split_frontmatter(raw.trim());
        let frontmatter = frontmatter?;

        let name = scalar(frontmatter, "name").filter(|n| !n.is_empty())?;
        let description = scalar(frontmatter, "description").unwrap_or_default();
        let memory_type = scalar(frontmatter, "type")
            .map(|t| match t.as_str() {
                "user" => MemoryType::User,
                "feedback" => MemoryType::Feedback,
                "project" => MemoryType::Project,
                "reference" => MemoryType::Reference,
                _ => MemoryType::User,
            })
            .unwrap_or_default();

        let content = body.trim().to_string();

        // Metadata fields. Each is optional and falls back to a default, so a
        // file without them still parses.
        let meta_str = |key: &str| scalar(frontmatter, key);

        Some(DurableMemory {
            name,
            description,
            memory_type,
            content,
            source_session_id: meta_str("source_session_id").unwrap_or_default(),
            confidence: meta_str("confidence")
                .and_then(|s| s.parse::<f64>().ok())
                .unwrap_or(0.8),
            last_seen: meta_str("last_seen").unwrap_or_default(),
            recall_count: meta_str("recall_count")
                .and_then(|s| s.parse::<u32>().ok())
                .unwrap_or(0),
        })
    }

    /// Write one memory as `<dir>/<name>.md`.
    ///
    /// All of `DurableMemory`'s fields are serialized, so `confidence`,
    /// `last_seen` and the cross-session `recall_count` survive a write/read
    /// round trip. Every metadata field is optional on the read side.
    fn write_memory_file(dir: &Path, mem: &DurableMemory) -> Result<(), MemoryError> {
        let md = Self::memory_dir(dir);
        std::fs::create_dir_all(&md).map_err(|e| MemoryError::Io(e.to_string()))?;
        // Sanitize filename: strip path separators and parent-dir traversal
        let safe_name = sanitize_filename(&mem.name);
        let path = md.join(format!("{}.md", safe_name));
        // Flat frontmatter format matching parse_memory_file's expectations —
        // name/description/type at top level, not nested under
        // `metadata:`. Description is quoted because it is free-form prose that
        // routinely contains `:` (which would otherwise make the line parse as
        // a nested mapping).
        let frontmatter_fields = format!(
            "name: {}\ndescription: {}\ntype: {}\nconfidence: {}\nlast_seen: {}\n\
             source_session_id: {}\nrecall_count: {}",
            mem.name,
            yaml_quote(&mem.description),
            mem.memory_type.as_str(),
            mem.confidence,
            yaml_quote(&mem.last_seen),
            yaml_quote(&mem.source_session_id),
            mem.recall_count,
        );
        // The closing `---` must start its own line: glued onto the last field
        // (`type: user---`), `parse_memory_file` would not find it and every
        // body would read back empty while the frontmatter still parsed.
        let frontmatter = format!("---\n{}\n---\n\n{}", frontmatter_fields, mem.content,);
        write_replacing(&path, &frontmatter)
    }

    fn rebuild_index(dir: &Path, memories: &[DurableMemory]) -> Result<(), MemoryError> {
        let md = Self::memory_dir(dir);
        std::fs::create_dir_all(&md).map_err(|e| MemoryError::Io(e.to_string()))?;
        let mut index = String::new();
        for mem in memories {
            index.push_str(&format!(
                "- [{}]({}.md) — {}\n",
                mem.name, mem.name, mem.description
            ));
        }
        write_replacing(&md.join(INDEX_FILE), &index)
    }

    /// Regenerate a directory's `MEMORY.md` from the `.md` files in it.
    ///
    /// The only supported way to write an index. Deriving it from disk rather
    /// than from whatever list a caller passed in is what keeps the index and
    /// the files it points at from diverging — the previous `persist_batch`
    /// wrote an index containing only the batch it was handed, silently
    /// dropping every other memory from the model's view of its own memory.
    fn rebuild_index_from_disk(dir: &Path, files: &PromptFiles) -> Result<(), MemoryError> {
        let mut memories = Self::load_from_dir(dir, files);
        // `read_dir` order is unspecified; sort so the index is reproducible.
        memories.sort_by(|a, b| a.name.cmp(&b.name));
        Self::rebuild_index(dir, &memories)
    }
}

/// Holds `dir` against every other writer: the other stores of this process
/// over the same directory — each session builds its own — and, on unix,
/// other processes.
fn lock_dir(dir: &Path) -> Result<base::dir_lock::DirLock, MemoryError> {
    base::dir_lock::DirLock::acquire(dir)
        .map_err(|e| MemoryError::Io(format!("{}: {e}", dir.display())))
}

/// Write `contents` to `path` so a reader sees the old file or the new one,
/// never a truncated one. The temporary name does not end in `.md`, so a
/// directory scan never mistakes it for a memory.
fn write_replacing(path: &Path, contents: &str) -> Result<(), MemoryError> {
    let io = |e: std::io::Error| MemoryError::Io(e.to_string());
    let mut tmp = path.as_os_str().to_owned();
    tmp.push(format!(".tmp-{}", std::process::id()));
    let tmp = PathBuf::from(tmp);
    std::fs::write(&tmp, contents).map_err(io)?;
    std::fs::rename(&tmp, path).map_err(|e| {
        let _ = std::fs::remove_file(&tmp);
        io(e)
    })
}

impl MemoryStorage for MemoryStore {
    fn load_all(&self) -> Vec<DurableMemory> {
        MemoryStore::load_all(self)
    }

    fn search(&self, query: &str) -> Vec<DurableMemory> {
        MemoryStore::search(self, query)
    }

    fn persist_batch(&self, memories: Vec<DurableMemory>) -> Result<usize, MemoryError> {
        MemoryStore::persist_batch(self, memories)
    }

    fn remove(&self, name: &str) -> Result<bool, MemoryError> {
        MemoryStore::remove(self, name)
    }
}

/// Extract the memory name from an index line (`- [name](name.md) — desc`).
/// Returns `None` for lines that are not index entries.
fn index_line_name(line: &str) -> Option<String> {
    let rest = line.trim_start().strip_prefix("- [")?;
    let (name, _) = rest.split_once("](")?;
    let name = name.trim();
    if name.is_empty() {
        None
    } else {
        Some(name.to_string())
    }
}

/// Quote a YAML scalar so free-form prose can't change the document's shape.
/// Memory descriptions routinely contain `:` (which would make the line parse
/// as a nested mapping) and `#` (a comment), and `last_seen` is an ISO-8601
/// timestamp — `2026-06-14T00:00:00Z` contains colons too.
fn yaml_quote(s: &str) -> String {
    format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\""))
}

/// Sanitize a memory name for use as a filename.
/// Prevents path traversal and filesystem issues.
fn sanitize_filename(name: &str) -> String {
    name.chars()
        .map(|c| {
            if c.is_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '-'
            }
        })
        .collect::<String>()
        .trim_matches('-')
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::prompt::truncate_entrypoint_content;
    use tempfile::TempDir;

    #[test]
    fn empty_store_returns_empty() {
        let tmp = TempDir::new().unwrap();
        let store = MemoryStore::new(
            tmp.path().join("user").join("memory"),
            tmp.path().join("local").join("memory"),
        );
        assert!(store.load_all().is_empty());
    }

    #[test]
    fn parse_memory_with_frontmatter() {
        let raw = r#"---
name: test-memory
description: A test memory
type: project
---

This is the content of the memory.
It can have multiple lines."#;
        let mem = MemoryStore::parse_memory_file(raw).unwrap();
        assert_eq!(mem.name, "test-memory");
        assert_eq!(mem.description, "A test memory");
        assert_eq!(mem.memory_type, MemoryType::Project);
        assert_eq!(
            mem.content,
            "This is the content of the memory.\nIt can have multiple lines."
        );
    }

    #[test]
    fn write_and_read_memory_file() {
        let tmp = TempDir::new().unwrap();
        let mem = DurableMemory {
            name: "test".into(),
            description: "test desc".into(),
            memory_type: MemoryType::Feedback,
            content: "Test content".into(),
            source_session_id: "s1".into(),
            confidence: 0.9,
            last_seen: "2026-06-10T00:00:00Z".into(),
            recall_count: 0,
        };
        MemoryStore::write_memory_file(tmp.path(), &mem).unwrap();
        // Read back
        let path = tmp.path().join("test.md");
        let raw = std::fs::read_to_string(&path).unwrap();
        let parsed = MemoryStore::parse_memory_file(&raw).unwrap();
        assert_eq!(parsed.name, "test");
        assert_eq!(parsed.description, "test desc");
        assert_eq!(parsed.memory_type, MemoryType::Feedback);
        // The body must survive the round trip. It did not: the closing `---`
        // was written onto the end of the last frontmatter field, so the
        // delimiter scan never found it and the content came back empty.
        assert_eq!(parsed.content, "Test content");
    }

    #[test]
    fn persist_and_load() {
        let tmp = TempDir::new().unwrap();
        let store = MemoryStore::new(
            tmp.path().join("user").join("memory"),
            tmp.path().join("local").join("memory"),
        );
        let mem = DurableMemory {
            name: "test".into(),
            description: "test summary".into(),
            memory_type: MemoryType::User,
            content: "The content".into(),
            source_session_id: "s1".into(),
            confidence: 0.8,
            last_seen: "2026-06-10T00:00:00Z".into(),
            recall_count: 0,
        };
        store.persist_batch(vec![mem]).unwrap();
        let loaded = store.load_all();
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].name, "test");
    }

    #[test]
    fn low_confidence_is_skipped() {
        let tmp = TempDir::new().unwrap();
        let store = MemoryStore::new(
            tmp.path().join("user").join("memory"),
            tmp.path().join("local").join("memory"),
        );
        let mem = DurableMemory {
            name: "low-conf".into(),
            description: "desc".into(),
            memory_type: MemoryType::Project,
            content: "content".into(),
            source_session_id: "s1".into(),
            confidence: 0.1, // below 0.3 threshold
            last_seen: "2026-06-10T00:00:00Z".into(),
            recall_count: 0,
        };
        let saved = store.persist_batch(vec![mem]).unwrap();
        assert_eq!(saved, 0);
    }

    #[test]
    fn sanitize_filename_prevents_traversal() {
        let result = sanitize_filename("../../etc/passwd");
        assert!(!result.contains('/'));
        assert!(!result.contains(".."));
    }

    // The read path merges `user_dir` and `local_dir`; every write path has
    // to put each memory back in its own directory.

    #[test]
    fn all_metadata_survives_a_write_read_round_trip() {
        let tmp = TempDir::new().unwrap();
        let (store, user_dir, _) = split_store(&tmp);
        let mut m = mem("meta");
        m.confidence = 0.42;
        m.last_seen = "2026-06-14T00:00:00Z".into();
        m.source_session_id = "sess-abc".into();
        m.recall_count = 7;
        store.persist_batch(vec![m]).unwrap();

        let back = MemoryStore::load_from_dir(&user_dir, &PromptFiles::unscoped());
        assert_eq!(back.len(), 1);
        let b = &back[0];
        assert_eq!(b.confidence, 0.42);
        assert_eq!(b.last_seen, "2026-06-14T00:00:00Z");
        assert_eq!(b.source_session_id, "sess-abc");
        assert_eq!(b.recall_count, 7);
        assert_eq!(b.content, "body of meta");
    }

    /// Free-form prose and ISO timestamps both contain `:`; unquoted they
    /// would parse as nested mappings and corrupt the frontmatter.
    #[test]
    fn colons_and_quotes_in_metadata_do_not_corrupt_frontmatter() {
        let tmp = TempDir::new().unwrap();
        let (store, user_dir, _) = split_store(&tmp);
        let mut m = mem("tricky");
        m.description = r#"uses "quotes": and a colon # plus a hash"#.into();
        store.persist_batch(vec![m]).unwrap();

        let back = MemoryStore::load_from_dir(&user_dir, &PromptFiles::unscoped());
        assert_eq!(back.len(), 1, "file failed to parse back");
        assert_eq!(
            back[0].description,
            r#"uses "quotes": and a colon # plus a hash"#
        );
    }

    /// A file without the metadata fields must still load, with defaults.
    #[test]
    fn old_format_files_without_metadata_still_parse() {
        let tmp = TempDir::new().unwrap();
        let (store, user_dir, _) = split_store(&tmp);
        std::fs::create_dir_all(&user_dir).unwrap();
        std::fs::write(
            user_dir.join("legacy.md"),
            "---\nname: legacy\ndescription: an old file\ntype: project\n---\n\nlegacy body\n",
        )
        .unwrap();

        let back = MemoryStore::load_from_dir(&user_dir, &PromptFiles::unscoped());
        assert_eq!(back.len(), 1);
        let b = &back[0];
        assert_eq!(b.name, "legacy");
        assert_eq!(b.memory_type, MemoryType::Project);
        assert_eq!(b.content.trim(), "legacy body");
        assert_eq!(b.confidence, 0.8, "default preserved");
        assert_eq!(b.recall_count, 0);
        assert!(b.last_seen.is_empty());
        let _ = &store;
    }

    fn mem(name: &str) -> DurableMemory {
        DurableMemory {
            name: name.into(),
            description: format!("desc for {name}"),
            memory_type: MemoryType::User,
            content: format!("body of {name}"),
            source_session_id: "s1".into(),
            confidence: 0.9,
            last_seen: "2026-08-01T00:00:00Z".into(),
            recall_count: 0,
        }
    }

    fn split_store(tmp: &TempDir) -> (MemoryStore, PathBuf, PathBuf) {
        let user = tmp.path().join("user").join("memory");
        let local = tmp.path().join("local").join("memory");
        (MemoryStore::new(user.clone(), local.clone()), user, local)
    }

    fn index_of(dir: &Path) -> String {
        std::fs::read_to_string(dir.join(INDEX_FILE)).unwrap_or_default()
    }

    #[test]
    fn second_batch_does_not_drop_first_batch_from_index() {
        // The MEMORY.md wipe: `persist_batch` rebuilt the index from only the
        // memories it was handed, so every turn-end extraction flattened the
        // index to that turn's findings.
        let tmp = TempDir::new().unwrap();
        let (store, user, _local) = split_store(&tmp);

        store.persist_batch(vec![mem("first-fact")]).unwrap();
        store.persist_batch(vec![mem("second-fact")]).unwrap();

        let index = index_of(&user);
        assert!(
            index.contains("first-fact"),
            "index lost the earlier batch: {index:?}"
        );
        assert!(
            index.contains("second-fact"),
            "index lost the newer batch: {index:?}"
        );
        assert_eq!(store.load_all().len(), 2);
    }

    #[test]
    fn index_matches_files_on_disk_after_every_write() {
        let tmp = TempDir::new().unwrap();
        let (store, user, _local) = split_store(&tmp);
        for name in ["a-fact", "b-fact", "c-fact"] {
            store.persist_batch(vec![mem(name)]).unwrap();
        }
        let index = index_of(&user);
        let listed = index.lines().filter(|l| l.starts_with("- [")).count();
        assert_eq!(listed, 3, "index out of sync with files: {index:?}");
    }

    #[test]
    fn local_memory_survives_load_all_persist_batch_round_trip() {
        // The recall write-back does exactly this to bump recall_count. It used
        // to copy every project-local memory into the global directory.
        let tmp = TempDir::new().unwrap();
        let (store, user, local) = split_store(&tmp);

        store
            .persist_batch_scoped(vec![ScopedMemory {
                memory: mem("project-fact"),
                scope: MemoryScope::Local,
            }])
            .unwrap();
        store.persist_batch(vec![mem("global-fact")]).unwrap();

        let all = store.load_all();
        assert_eq!(all.len(), 2);
        store.persist_batch(all).unwrap();

        assert!(
            local.join("project-fact.md").exists(),
            "local memory vanished"
        );
        assert!(
            !user.join("project-fact.md").exists(),
            "project-local memory was copied into the global dir"
        );
        assert!(user.join("global-fact.md").exists());
        assert!(!local.join("global-fact.md").exists());
    }

    #[test]
    fn local_index_is_rebuilt_when_local_contents_change() {
        let tmp = TempDir::new().unwrap();
        let (store, user, local) = split_store(&tmp);

        store
            .persist_batch_scoped(vec![ScopedMemory {
                memory: mem("project-fact"),
                scope: MemoryScope::Local,
            }])
            .unwrap();
        assert!(index_of(&local).contains("project-fact"));

        // A round trip through the unscoped write path must keep it there.
        store.persist_batch(store.load_all()).unwrap();
        assert!(
            index_of(&local).contains("project-fact"),
            "local index drifted out of sync with its files"
        );
        assert!(
            !index_of(&user).contains("project-fact"),
            "local memory leaked into the global index"
        );

        // And removal must rebuild the local index, not the global one.
        assert!(store.remove("project-fact").unwrap());
        assert!(!index_of(&local).contains("project-fact"));
        assert!(!local.join("project-fact.md").exists());
    }

    #[test]
    fn load_index_does_not_duplicate_an_entry_present_in_both_dirs() {
        let tmp = TempDir::new().unwrap();
        let (store, _user, _local) = split_store(&tmp);

        store
            .persist_batch_scoped(vec![
                ScopedMemory {
                    memory: mem("shared-name"),
                    scope: MemoryScope::User,
                },
                ScopedMemory {
                    memory: mem("shared-name"),
                    scope: MemoryScope::Local,
                },
            ])
            .unwrap();

        let index = store.load_index();
        assert_eq!(
            index.matches("shared-name](").count(),
            1,
            "same memory listed twice: {index:?}"
        );
        assert_eq!(store.load_all().len(), 1);
    }

    #[test]
    fn single_directory_store_does_not_see_everything_twice() {
        // Team memory points both scopes at one directory.
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path().join("team").join("memory");
        let store = MemoryStore::new(dir.clone(), dir.clone());
        store.persist_batch(vec![mem("team-fact")]).unwrap();

        assert_eq!(store.load_all().len(), 1);
        assert_eq!(store.load_index().matches("team-fact](").count(), 1);
    }

    #[test]
    fn an_index_cut_inside_a_multibyte_character_is_cut_before_it() {
        let line = format!("- [记忆](m.md) — {}\n", "部署流程与回滚步骤".repeat(8));
        let mut raw = String::new();
        while raw.len() < 30_000 {
            raw.push_str(&line);
        }
        let one_line = raw.replace('\n', " ");
        for index in [raw, one_line] {
            let mut index = index;
            while index.trim().is_char_boundary(25_000) {
                index.insert(0, 'x');
            }
            let cut = truncate_entrypoint_content(&index);
            let kept = cut.content.split("\n\n> WARNING").next().unwrap();
            assert!(kept.len() <= 25_000);
            assert!(index.trim().starts_with(kept));
            assert!(cut.content.contains("Only part of it was loaded"));
        }
    }

    #[test]
    fn a_session_store_lives_where_config_paths_say() {
        let tmp = TempDir::new().unwrap();
        let paths = ConfigPaths::new(tmp.path().join("g"), tmp.path().join("p/.agix"), "code");
        let store = MemoryStore::for_paths(&paths);
        assert_eq!(store.user_dir(), paths.global_memory_dir());
        store
            .persist_batch_scoped(vec![
                ScopedMemory {
                    memory: mem("global-fact"),
                    scope: MemoryScope::User,
                },
                ScopedMemory {
                    memory: mem("project-fact"),
                    scope: MemoryScope::Local,
                },
            ])
            .unwrap();
        assert!(paths.global_memory_dir().join("global-fact.md").exists());
        assert!(paths.local_memory_dir().join("project-fact.md").exists());

        let prompt = memory_prompt(&store);
        assert!(prompt.contains(&paths.global_memory_dir().display().to_string()));
        assert!(prompt.contains("global-fact") && prompt.contains("project-fact"));
    }

    #[test]
    fn concurrent_writers_from_separate_stores_lose_nothing() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path().join("memory");
        let writers: Vec<_> = (0..8)
            .map(|w| {
                let dir = dir.clone();
                std::thread::spawn(move || {
                    let store = MemoryStore::new(dir.clone(), dir);
                    for i in 0..10 {
                        store
                            .persist_batch(vec![mem(&format!("w{w}-m{i}"))])
                            .unwrap();
                    }
                })
            })
            .collect();
        for w in writers {
            w.join().unwrap();
        }
        let store = MemoryStore::new(dir.clone(), dir.clone());
        assert_eq!(store.load_all().len(), 80);
        let index = index_of(&dir);
        assert_eq!(index.lines().count(), 80, "{index}");
        let stray: Vec<_> = std::fs::read_dir(&dir)
            .unwrap()
            .flatten()
            .filter(|e| e.file_name().to_string_lossy().contains(".tmp-"))
            .collect();
        assert!(stray.is_empty(), "{stray:?}");
    }

    #[test]
    fn concurrent_recalls_of_one_memory_all_count() {
        let tmp = TempDir::new().unwrap();
        let (store, _, _) = split_store(&tmp);
        store.persist_batch(vec![mem("hot")]).unwrap();
        let store = std::sync::Arc::new(store);
        let recallers: Vec<_> = (0..8)
            .map(|_| {
                let store = store.clone();
                std::thread::spawn(move || {
                    for _ in 0..5 {
                        store.record_recalled(&["hot".to_string()]).unwrap();
                    }
                })
            })
            .collect();
        for r in recallers {
            r.join().unwrap();
        }
        assert_eq!(store.load_all()[0].recall_count, 40);
    }

    /// Memories written straight into the directory — the `Write` tool needs
    /// no store — are read no further than the cap: the most recent ones, and
    /// as many index lines.
    #[test]
    fn a_store_reads_no_more_than_its_cap() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path().join("memory");
        let store = MemoryStore::new(dir.clone(), dir.clone());
        let over = MAX_MEMORIES + 5;
        let mut index = String::new();
        for i in 0..over {
            let mut m = mem(&format!("m{i:03}"));
            m.last_seen = format!("2026-08-01T00:{:02}:{:02}Z", i / 60, i % 60);
            MemoryStore::write_memory_file(&dir, &m).unwrap();
            index.push_str(&format!("- [{0}]({0}.md) — d\n", m.name));
        }
        std::fs::write(dir.join(INDEX_FILE), index).unwrap();

        let held = store.load_all();
        assert_eq!(held.len(), MAX_MEMORIES);
        assert_eq!(
            held[0].name,
            format!("m{:03}", over - 1),
            "most recent first"
        );
        assert_eq!(
            store
                .load_index()
                .lines()
                .filter(|l| index_line_name(l).is_some())
                .count(),
            MAX_MEMORIES
        );
        assert_eq!(store.count(), over, "the count is not capped");
    }

    /// Another process writing the directory holds it through `flock`, and a
    /// store waits for it.
    #[cfg(unix)]
    #[test]
    fn a_write_waits_for_another_process_holding_the_directory() {
        use std::os::fd::AsRawFd;
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path().join("memory");
        std::fs::create_dir_all(&dir).unwrap();
        let other = std::fs::File::open(&dir).unwrap();
        assert_eq!(unsafe { libc::flock(other.as_raw_fd(), libc::LOCK_EX) }, 0);

        let (done_tx, done_rx) = std::sync::mpsc::channel();
        let writer = {
            let dir = dir.clone();
            std::thread::spawn(move || {
                MemoryStore::new(dir.clone(), dir)
                    .persist_batch(vec![mem("late")])
                    .unwrap();
                done_tx.send(()).unwrap();
            })
        };
        let early = done_rx.recv_timeout(std::time::Duration::from_millis(300));
        assert!(early.is_err(), "the write went ahead under another holder");
        drop(other);
        writer.join().unwrap();
        assert!(dir.join("late.md").exists());
    }

    #[test]
    fn a_key_in_a_memory_is_found() {
        let mut leaky = mem("leaky");
        leaky.content = "the deploy token is sk-abcdefghijklmnopqrstuvwx".into();
        assert_eq!(scan_for_secrets(&leaky)[0].kind, "API key (sk-...)");
        assert!(scan_for_secrets(&mem("clean")).is_empty());
    }
}
