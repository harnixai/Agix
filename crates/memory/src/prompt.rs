//! The memory system prompt: how to write and use memories, plus the index.

use crate::{MemoryStore, INDEX_FILE};
use base::interface::memory_contracts::ExtractionPolicy;
use base::paths::ConfigPaths;
use std::path::Path;

/// Build the memory system prompt: how to write and use memories, followed by
/// the `MEMORY.md` index read from disk (truncated to 200 lines / 25KB).
pub fn build_memory_prompt(memory_dir: &Path) -> String {
    memory_prompt_with_index(
        memory_dir,
        base::paths::SensitivePaths::default()
            .prompt_files_within(memory_dir)
            .read_to_string(&memory_dir.join(INDEX_FILE))
            .ok(),
    )
}

/// The memory system prompt for a session's store: the model writes to its
/// global directory, and the index shown is both directories' together, so a
/// memory extraction filed under the project is listed too.
pub fn memory_prompt(store: &MemoryStore) -> String {
    memory_prompt_with_index(store.user_dir(), Some(store.load_index()))
}

fn memory_prompt_with_index(memory_dir: &Path, index: Option<String>) -> String {
    let dir_str = memory_dir.display().to_string();
    let mut sections = vec![
        MEMORY_HEADER.replace("{dir}", &dir_str),
        TYPES_SECTION_INDIVIDUAL.to_string(),
        WHAT_NOT_TO_SAVE_SECTION.to_string(),
        WHEN_TO_ACCESS_SECTION.to_string(),
        TRUSTING_RECALL_SECTION.to_string(),
    ];

    if let Some(raw) = index {
        let truncated = truncate_entrypoint_content(&raw);
        if !truncated.content.is_empty() {
            sections.push(format!(
                "\n## Current memory index ({} — {} entries, {} chars)\n\n{}",
                INDEX_FILE,
                truncated.line_count,
                truncated.content.len(),
                truncated.content
            ));
        }
    }

    sections.join("\n")
}

/// Truncated memory entrypoint content.
pub(crate) struct TruncatedEntry {
    pub(crate) content: String,
    line_count: usize,
}

/// Truncate MEMORY.md content to line and byte caps.
/// - Line cap: 200 lines
/// - Byte cap: 25,000 bytes
pub(crate) fn truncate_entrypoint_content(raw: &str) -> TruncatedEntry {
    const MAX_LINES: usize = 200;
    const MAX_BYTES: usize = 25_000;

    let trimmed = raw.trim();
    let lines: Vec<&str> = trimmed.lines().collect();
    let line_count = lines.len();
    let byte_count = trimmed.len();

    let was_line_truncated = line_count > MAX_LINES;
    let was_byte_truncated = byte_count > MAX_BYTES;

    if !was_line_truncated && !was_byte_truncated {
        return TruncatedEntry {
            content: trimmed.to_string(),
            line_count,
        };
    }

    let mut truncated: String = if was_line_truncated {
        lines[..MAX_LINES].join("\n")
    } else {
        trimmed.to_string()
    };

    if truncated.len() > MAX_BYTES {
        let head = base::text::truncate_at_char_boundary(&truncated, MAX_BYTES);
        let cut_at = head.rfind('\n').unwrap_or(head.len());
        truncated.truncate(cut_at);
    }

    let reason = if was_byte_truncated && !was_line_truncated {
        format!(
            "{} bytes (limit: {} bytes) — index entries are too long",
            byte_count, MAX_BYTES
        )
    } else if was_line_truncated && !was_byte_truncated {
        format!("{} lines (limit: {} lines)", line_count, MAX_LINES)
    } else {
        format!("{} lines and {} bytes", line_count, byte_count)
    };

    truncated.push_str(&format!(
        "\n\n> WARNING: {} is {}. Only part of it was loaded. Keep index entries to one line under ~200 chars; move detail into topic files.",
        INDEX_FILE, reason
    ));

    TruncatedEntry {
        content: truncated,
        line_count,
    }
}

const MEMORY_HEADER: &str = "\
# Memory

You have a persistent file-based memory at `{dir}`. The directory exists — write to it \
directly (no mkdir, no existence check). Each memory is one file holding one fact, with \
frontmatter:

```
---
name: <short-kebab-case-slug>
description: <one-line summary — used to decide relevance during recall>
type: user | feedback | project | reference
---

<the fact; for feedback/project, follow with **Why:** and **How to apply:** lines. \
Link related memories with `[[their-name]]`.>
```

`[[name]]` refers to another memory's `name:` slug. Link liberally; a link to a memory \
that doesn't exist yet marks something worth writing later, not an error.

After writing the file, add a one-line pointer to `MEMORY.md` (`- [Title](file.md) — \
hook`). `MEMORY.md` is the index loaded into context each session: one line per memory, \
no frontmatter, never memory content.

Before saving, check for an existing file that covers it and update that instead of \
duplicating. Delete memories that turn out to be wrong.";

const TYPES_SECTION_INDIVIDUAL: &str = r#"## Types of memory

- **user** — the user's role, goals, responsibilities, knowledge and preferences, so you can tailor your work to them (a senior engineer is helped differently from a first-time coder). Save when you learn such details. Nothing judgemental, nothing irrelevant to the work.
- **feedback** — how the user wants work approached: what to avoid *and* what to keep doing. Save when they correct you ("no not that", "stop doing X") and when they confirm a non-obvious approach worked ("yes exactly", accepting an unusual choice without pushback) — recording only corrections drifts you away from validated approaches and makes you overly cautious. Save what applies to future conversations, especially if surprising. Lead with the rule, then **Why:** (the reason given) and **How to apply:** (when it kicks in), so you can judge edge cases. These exist so the user never has to give the same guidance twice.
- **project** — ongoing work, goals, initiatives, bugs or incidents not derivable from the code or git history: who is doing what, why, by when. Convert relative dates to absolute ("Thursday" → "2026-03-05"). Lead with the fact or decision, then **Why:** and **How to apply:**; these change fast, so keep them current.
- **reference** — where information lives in external systems and what it is for (a Linear project, a Slack channel, a dashboard). Use when the user refers to such a system.

Examples:
- "I've been writing Go for ten years but this is my first time touching the React side" → user: deep Go expertise, new to React and this frontend — explain frontend in backend terms.
- "don't mock the database in these tests — we got burned when mocked tests passed but the prod migration failed" → feedback: integration tests hit a real database. **Why:** a mock/prod divergence once hid a broken migration. **How to apply:** any test touching persistence.
- "yeah the single bundled PR was the right call here" → feedback: for refactors in this area the user prefers one bundled PR — a confirmed judgement call, not a correction.
- "we're freezing non-critical merges after Thursday for the mobile release" → project: merge freeze begins 2026-03-05; flag non-critical PR work after that date.
- "pipeline bugs are tracked in the Linear project INGEST" → reference: pipeline bugs live in Linear project "INGEST"."#;

const WHAT_NOT_TO_SAVE_SECTION: &str = r#"## What NOT to save

- Code patterns, conventions, architecture, file paths, project structure — read the project instead.
- Git history, recent changes, who changed what — `git log` / `git blame` are authoritative.
- Debugging solutions or fix recipes — the fix is in the code, the context in the commit message.
- Anything already in CLAUDE.md files.
- Ephemeral task details: in-progress work, temporary state, the current conversation.

These hold even when the user asks you to save. Asked to save a PR list or activity summary, ask what was *surprising* or *non-obvious* about it — that is the part worth keeping."#;

const WHEN_TO_ACCESS_SECTION: &str = r#"## When to access memories
- When memories seem relevant, or the user refers to earlier conversations. You MUST access memory when the user asks you to check, recall or remember.
- If the user says to ignore memory, proceed as if MEMORY.md were empty: don't apply, cite, compare against or mention it.
- Memories can go stale. Before answering or acting on one alone, verify it against the current files or resources; if it conflicts with what you observe, trust the observation and update or remove the memory."#;

const TRUSTING_RECALL_SECTION: &str = r#"## Before recommending from memory

A memory naming a file, function or flag says it existed *when written*; it may have been renamed, removed or never merged. Check a path exists or grep for the name before recommending it, and always verify first when the user is about to act on it. A memory summarizing repo state is a snapshot: for *recent* or *current* state, prefer `git log` or the code."#;

/// The `MEMORY.md` index of the session's two directories as the prompt shows
/// it: `None` when there is none, truncated to the entrypoint caps.
pub async fn index_for_prompt(paths: &ConfigPaths) -> Option<String> {
    let paths = paths.clone();
    tokio::task::spawn_blocking(move || MemoryStore::for_paths(&paths).load_index())
        .await
        .ok()
        .filter(|index| !index.trim().is_empty())
        .map(|index| truncate_entrypoint_content(&index).content)
}

/// The engine's own extraction instructions: the memory system prompt.
pub struct DefaultExtraction;

impl ExtractionPolicy for DefaultExtraction {
    fn instructions(&self, memory_dir: &Path) -> String {
        build_memory_prompt(memory_dir)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use base::interface::memory_contracts::NoExtraction;

    #[test]
    fn a_deployment_can_stop_the_model_being_told_to_write_memories() {
        let dir = Path::new("/tmp/memories");
        assert!(!DefaultExtraction.instructions(dir).is_empty());
        assert!(NoExtraction.instructions(dir).is_empty());
    }
}
