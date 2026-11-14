//! A tool result too long for the conversation is saved to a file in the
//! session's scratchpad, and the model is given its head, its tail and the
//! path. Runs after the result transformers, so what is saved is what the
//! model would have seen; the file is the engine's own, written with the
//! transcript's trust.

use super::tool_exec::{Caller, ToolExecCtx};
use base::interface::tool_middleware::ToolOutcome;
use std::path::{Path, PathBuf};

const SPILL_HEAD_CHARS: usize = 2_000;
const SPILL_TAIL_CHARS: usize = 1_000;

pub(super) async fn spill_oversized(
    ctx: &ToolExecCtx,
    name: &str,
    pages_itself: bool,
    mut outcome: ToolOutcome,
) -> ToolOutcome {
    let limits = &ctx.config.tool_results;
    let Some(dir) = &limits.dir else {
        return outcome;
    };
    if limits.inline_chars == 0 || pages_itself || ctx.caller != Caller::Model {
        return outcome;
    }
    let text = match &mut outcome {
        Ok(answer) => &mut answer.text,
        Err(message) => message,
    };
    if text.chars().nth(limits.inline_chars).is_none() {
        return outcome;
    }
    let saved = {
        let (dir, id, body) = (dir.clone(), ctx.tool_use_id.clone(), text.clone());
        tokio::task::spawn_blocking(move || write_spill(&dir, &id, &body)).await
    };
    let path = match saved {
        Ok(Ok(path)) => path,
        failure => {
            tracing::warn!(
                tool = name,
                error = ?failure,
                "could not save a long tool result to a file; it stays in the conversation"
            );
            return outcome;
        }
    };
    *text = preview(text, &path, ctx.config.file_limits.max_line_chars);
    if let Ok(answer) = &mut outcome {
        answer.structured = None;
    }
    outcome
}

fn preview(text: &str, path: &Path, max_line_chars: usize) -> String {
    let mut marker = compaction::budget::spill_marker(
        text.chars().count(),
        text.lines().count(),
        &path.display().to_string(),
    );
    if text
        .lines()
        .any(|l| l.chars().nth(max_line_chars).is_some())
    {
        marker.push_str(&format!(
            " Some lines are longer than Read shows ({max_line_chars} characters); \
             use Bash (cut -c, jq) for those."
        ));
    }
    let cut = compaction::budget::cut_keeping_fence(text, SPILL_HEAD_CHARS, SPILL_TAIL_CHARS);
    format!("{marker}\n{cut}")
}

fn file_name_of(tool_use_id: &str) -> String {
    let clean: String = tool_use_id
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_'))
        .collect();
    format!("{}.txt", if clean.is_empty() { "result" } else { &clean })
}

fn write_spill(dir: &Path, tool_use_id: &str, text: &str) -> std::io::Result<PathBuf> {
    create_private_dir(dir)?;
    let ignore = dir.join(".ignore");
    if !ignore.exists() {
        base::fs::write_atomic(&ignore, b"*\n", base::fs::WriteOpts::PRIVATE)?;
    }
    let path = dir.join(file_name_of(tool_use_id));
    base::fs::write_atomic(&path, text.as_bytes(), base::fs::WriteOpts::PRIVATE)?;
    Ok(path)
}

fn create_private_dir(dir: &Path) -> std::io::Result<()> {
    let mut builder = std::fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    std::os::unix::fs::DirBuilderExt::mode(&mut builder, 0o700);
    builder.create(dir)
}

/// Whether this `Read` or `Grep` is aimed at something saved here: what it
/// finds is a server's or a web page's words, however the path looks.
pub(super) fn reads_a_saved_result(
    ctx: &ToolExecCtx,
    name: &str,
    input: &serde_json::Value,
) -> bool {
    let Some(root) = ctx
        .config
        .tool_results
        .dir
        .as_deref()
        .and_then(Path::parent)
    else {
        return false;
    };
    let key = match name {
        "Read" => "file_path",
        "Grep" => "path",
        _ => return false,
    };
    let Some(target) = input.get(key).and_then(|v| v.as_str()) else {
        return false;
    };
    let target = ctx.cwd.join(target);
    let real = |p: &Path| std::fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf());
    real(&target).starts_with(real(root))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn preview_keeps_the_fence_around_head_and_tail() {
        let body = format!("{}{}", "h".repeat(5_000), "t".repeat(5_000));
        let fenced = format!(
            "<external-content source=\"s\" from=\"f\">\n{body}\n</external-content>\nNote.\n"
        );
        let out = preview(&fenced, Path::new("/s/t.txt"), 2_000);
        let (marker, rest) = out.split_once('\n').unwrap();
        assert_eq!(compaction::budget::spill_path_of(marker), Some("/s/t.txt"));
        assert!(rest.starts_with("<external-content source=\"s\" from=\"f\">\nhhh"));
        assert!(
            rest.ends_with("ttt\n</external-content>\nNote.\n"),
            "{rest}"
        );
        assert!(out.chars().count() < 4_000);
    }

    #[test]
    fn a_tool_use_id_with_path_characters_is_cleaned() {
        assert_eq!(file_name_of("toolu_01-A"), "toolu_01-A.txt");
        assert_eq!(file_name_of("../../etc/x"), "etcx.txt");
        assert_eq!(file_name_of("//"), "result.txt");
    }

    #[cfg(unix)]
    #[test]
    fn a_spill_directory_replaced_by_a_symlink_is_not_written_through() {
        let tmp = tempfile::tempdir().unwrap();
        let outside = tmp.path().join("outside");
        std::fs::create_dir(&outside).unwrap();
        let dir = tmp.path().join("tool-results").join("s");
        std::fs::create_dir_all(dir.parent().unwrap()).unwrap();
        std::os::unix::fs::symlink(&outside, &dir).unwrap();
        assert!(write_spill(&dir, "toolu_1", "planted").is_err());
        assert_eq!(std::fs::read_dir(&outside).unwrap().count(), 0);
    }

    #[test]
    fn a_line_longer_than_read_shows_adds_the_hint() {
        let out = preview(&"x".repeat(5_000), Path::new("/s/t.txt"), 2_000);
        assert!(out
            .lines()
            .next()
            .unwrap()
            .contains("longer than Read shows"));
    }
}
