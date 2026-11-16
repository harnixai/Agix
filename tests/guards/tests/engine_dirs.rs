//! Where an engine directory and its entries are named: only in
//! `crates/core/src/paths/layout.rs`. The project-level directory is the
//! embedder's to name, so production code that spells `.agix` — or the
//! entries inside it — reads and writes the wrong place for every embedder
//! that chose another name, and, worse, leaves the model free to write where
//! the engine reads its configuration (DIR-1, DIR-2). Prompts and tool
//! descriptions that name a directory tell the model to do the same (DIR-3).
//!
//! The way out is always a method: `ConfigPaths` for a session, `StateRoot`
//! for the pool's state root, `Layout` when only the name is at hand.

use std::collections::BTreeSet;
use std::path::Path;

use guards::allow::{self, Finding};
use guards::scan::{self, Callee};
use guards::{par_flat_map, ws, Hit, View};
use syn::{Expr, Lit};

const LAYOUT: &str = "crates/core/src/paths/layout.rs";

const NAMES_A_DIRECTORY: &[(&str, &str)] = &[(
    "daemon/src/main.rs",
    "agixd's `--help` text: agixd is Agix itself, its project directory is always `.agix`.",
)];

const NAMES_AN_ENTRY: &[(&str, &str)] = &[];

const FIX: &str =
    "ask `ConfigPaths` / `Layout` / `StateRoot` for the path; the names live only in \
                   crates/core/src/paths/layout.rs";

const ENGINE_FILES: &[&str] = &[
    "settings.json",
    "settings.local.json",
    "mcp.local.json",
    "scheduled_tasks.json",
    ".imported.json",
];

fn mentions_engine_dir(s: &str) -> bool {
    let bytes = s.as_bytes();
    for name in [".agix", ".claude"] {
        let mut from = 0;
        while let Some(i) = s[from..].find(name) {
            let start = from + i;
            let end = start + name.len();
            let before_ok = start == 0
                || matches!(bytes[start - 1], b'/' | b'~' | b'`' | b'\'' | b'"' | b'(')
                || bytes[start - 1].is_ascii_whitespace();
            let after_ok = end == s.len()
                || matches!(bytes[end], b'/' | b'`' | b'\'' | b'"' | b')')
                || bytes[end].is_ascii_whitespace();
            if before_ok && after_ok {
                return true;
            }
            from = end;
        }
    }
    false
}

fn names_an_engine_file(s: &str) -> bool {
    ENGINE_FILES
        .iter()
        .any(|f| s == *f || s.ends_with(&format!("/{f}")))
}

fn is_outside_scope(rel: &str) -> bool {
    rel == LAYOUT
}

fn literal_findings(rel_ok: fn(&str) -> bool) -> Vec<Finding> {
    par_flat_map(ws().production(), |f| {
        if is_outside_scope(&f.rel) || f.rel.starts_with("tests/") || !rel_ok(&f.rel) {
            return Vec::new();
        }
        scan::string_literals(f, View::Prod)
            .into_iter()
            .filter(|(s, _)| {
                mentions_engine_dir(s) || names_an_engine_file(s) || s.contains(".agents/skills")
            })
            .map(|(_, hit)| Finding {
                key: hit.file.clone(),
                hit,
            })
            .collect()
    })
}

#[test]
fn dir_1_production_code_names_no_engine_directory_or_file() {
    let found = literal_findings(|_| true);
    allow::non_vacuous("engine_dirs::scan", ws().production().count(), 300);
    allow::check(
        "engine_dirs::NAMES_A_DIRECTORY",
        found,
        NAMES_A_DIRECTORY,
        FIX,
    );
}

fn entry_names() -> BTreeSet<String> {
    scan::str_consts(ws().file(LAYOUT), View::Full)
        .into_iter()
        .map(|(_, v)| v)
        .filter(|v| {
            !v.contains('/') && !v.starts_with('.') && !v.ends_with(".json") && !v.ends_with(".md")
        })
        .collect()
}

#[test]
fn dir_2_an_entry_of_an_engine_directory_is_not_joined_by_its_name() {
    let names = entry_names();
    allow::non_vacuous("engine_dirs::entries", names.len(), 15);
    let found = par_flat_map(ws().production(), |f| {
        if is_outside_scope(&f.rel)
            || f.rel.starts_with("tests/")
            || f.rel.starts_with("crates/core/src/paths/")
        {
            return Vec::new();
        }
        scan::calls(f, View::Prod)
            .into_iter()
            .filter(|c| matches!(&c.callee, Callee::Method(m) if m == "join"))
            .filter(|c| {
                matches!(c.args.as_slice(), [Expr::Lit(l)]
                    if matches!(&l.lit, Lit::Str(s) if names.contains(&s.value())))
            })
            .map(|c| Finding {
                key: c.hit.file.clone(),
                hit: c.hit,
            })
            .collect()
    });
    allow::check("engine_dirs::NAMES_AN_ENTRY", found, NAMES_AN_ENTRY, FIX);
}

fn markdown_templates(dir: &Path, out: &mut Vec<std::path::PathBuf>) {
    let Ok(read) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in read.flatten() {
        let path = entry.path();
        if path.is_dir() {
            markdown_templates(&path, out);
        } else if path.extension().is_some_and(|e| e == "md") {
            out.push(path);
        }
    }
}

#[test]
fn dir_3_prompt_templates_name_no_engine_directory() {
    let mut files = Vec::new();
    for package in ws().packages() {
        markdown_templates(&ws().root().join(&package.dir).join("src"), &mut files);
    }
    allow::non_vacuous("engine_dirs::templates", files.len(), 10);
    let found: Vec<Finding> = files
        .iter()
        .flat_map(|path| {
            let rel = path
                .strip_prefix(ws().root())
                .unwrap_or(path)
                .to_string_lossy()
                .replace('\\', "/");
            let text = std::fs::read_to_string(path).unwrap_or_default();
            text.lines()
                .enumerate()
                .filter(|(_, l)| mentions_engine_dir(l) || l.contains(".agents/skills"))
                .map(|(i, l)| Finding {
                    key: rel.clone(),
                    hit: Hit {
                        file: rel.clone(),
                        line: i + 1,
                        snippet: l.trim().chars().take(100).collect(),
                    },
                })
                .collect::<Vec<_>>()
        })
        .collect();
    allow::check(
        "engine_dirs::TEMPLATES",
        found,
        &[],
        "write `{project_dir}` in the template and render it from the session's `Layout`",
    );
}

#[test]
fn the_name_scan_knows_what_a_directory_name_looks_like() {
    for yes in [
        ".agix",
        "~/.agix/skills",
        "see `.agix/rules/`",
        "(.claude/skills)",
        "x .agix y",
        "./.claude",
    ] {
        assert!(mentions_engine_dir(yes), "{yes}");
    }
    for no in [
        ".agix-installed",
        ".git.agix-quarantine-x",
        "/tmp/.agix-egress",
        "agix",
        "x.agixy",
    ] {
        assert!(!mentions_engine_dir(no), "{no}");
    }
    assert!(names_an_engine_file("settings.json"));
    assert!(names_an_engine_file("/x/mcp.local.json"));
    assert!(!names_an_engine_file("my-settings.json"));
}
