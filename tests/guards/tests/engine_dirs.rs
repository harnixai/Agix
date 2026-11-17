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
//!
//! The product's name is the embedder's too (`EmbedConfig.product_name`):
//! a production string or a prompt template that spells `Agix` shows a
//! host's users and its model the wrong product (NAME-1, NAME-2). It comes
//! from `Layout::product_name` and what is derived from it. The protocol's
//! own names — `agix.*` methods and namespaces, `AGIX_*` variables, internal
//! `agix-` identifiers — are not the product's display name and are not
//! matched. The doc comments of a `JsonSchema` type are the descriptions of a
//! tool's input the model reads, and name neither (NAME-3).

use std::collections::BTreeSet;
use std::path::Path;

use guards::allow::{self, Finding};
use guards::scan::{self, Callee};
use guards::{par_flat_map, ws, Hit, View};
use syn::visit::{self, Visit};
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

const NAMES_THE_PRODUCT: &[(&str, &str)] = &[
    (
        "daemon/src/main.rs",
        "agixd's `--help` text: agixd is Agix itself, never renamed.",
    ),
    (
        "crates/assembly/src/server/schema.rs: Agix daemon JSON-RPC",
        "the title of the protocol's schema document, the interface's name.",
    ),
    (
        "crates/assembly/src/telemetry_otel/mod.rs: agix",
        "the OpenTelemetry instrumentation scope, which names the library the `agix.*` \
         metrics come from; `service.name` is the product's.",
    ),
    (
        "crates/assembly/src/config.rs: /tmp/agix",
        "agixd's state root when the process has no home: agixd's own, like its socket.",
    ),
];

const PRODUCT_FIX: &str = "take the name from `Layout::product_name` (or `product_id`, \
                           `user_agent`, `worktree_branch_prefix`, `quarantine_prefix`, \
                           `attribution`); it is spelled only in crates/core/src/paths/layout.rs";

/// `agix` as a word of its own, any case: not part of `.agix`, `agix-x`,
/// `agix_x`, `AGIX_X`, and not the `agix.` namespace (lowercase, then a dot
/// that ends the string or leads a name).
fn mentions_the_product(s: &str) -> bool {
    let lower = s.to_ascii_lowercase();
    let bytes = lower.as_bytes();
    let mut from = 0;
    while let Some(i) = lower[from..].find("agix") {
        let start = from + i;
        let end = start + 4;
        let joined = |b: u8| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-');
        let namespace = &s[start..end] == "agix"
            && bytes.get(end) == Some(&b'.')
            && bytes.get(end + 1).is_none_or(u8::is_ascii_alphabetic);
        let before_ok = start == 0 || !(joined(bytes[start - 1]) || bytes[start - 1] == b'.');
        let after_ok = !namespace && (end == bytes.len() || !joined(bytes[end]));
        if before_ok && after_ok {
            return true;
        }
        from = end;
    }
    false
}

#[test]
fn name_1_production_code_spells_no_product_name() {
    let found = par_flat_map(ws().production(), |f| {
        if is_outside_scope(&f.rel) || f.rel.starts_with("tests/") {
            return Vec::new();
        }
        scan::string_literals(f, View::Prod)
            .into_iter()
            .filter(|(s, _)| mentions_the_product(s))
            .map(|(s, hit)| Finding {
                key: if hit.file == "daemon/src/main.rs" {
                    hit.file.clone()
                } else {
                    format!("{}: {}", hit.file, s.trim())
                },
                hit,
            })
            .collect()
    });
    allow::non_vacuous("engine_dirs::product", ws().production().count(), 300);
    allow::check(
        "engine_dirs::NAMES_THE_PRODUCT",
        found,
        NAMES_THE_PRODUCT,
        PRODUCT_FIX,
    );
}

#[test]
fn name_2_prompt_templates_spell_no_product_name() {
    let mut files = Vec::new();
    for package in ws().packages() {
        markdown_templates(&ws().root().join(&package.dir).join("src"), &mut files);
    }
    allow::non_vacuous("engine_dirs::product_templates", files.len(), 10);
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
                .filter(|(_, l)| mentions_the_product(l))
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
        "engine_dirs::PRODUCT_TEMPLATES",
        found,
        &[],
        "write `{product_name}` or `{attribution}` in the template and render it from the \
         session's `Layout`",
    );
}

/// The doc comments of every `#[derive(JsonSchema)]` type in `tree` — the
/// type's, its fields', its variants' — with where each is.
fn schema_docs(tree: &syn::File) -> Vec<(String, proc_macro2::Span)> {
    #[derive(Default)]
    struct Docs(Vec<(String, proc_macro2::Span)>);
    impl Docs {
        fn take(&mut self, attrs: &[syn::Attribute]) {
            for attr in attrs.iter().filter(|a| a.path().is_ident("doc")) {
                if let syn::Meta::NameValue(nv) = &attr.meta {
                    if let Expr::Lit(syn::ExprLit {
                        lit: Lit::Str(s), ..
                    }) = &nv.value
                    {
                        self.0.push((s.value(), s.span()));
                    }
                }
            }
        }
        fn fields(&mut self, fields: &syn::Fields) {
            for field in fields {
                self.take(&field.attrs);
            }
        }
    }
    fn derives_schema(attrs: &[syn::Attribute]) -> bool {
        attrs.iter().any(|a| {
            a.path().is_ident("derive")
                && quote::ToTokens::to_token_stream(a)
                    .to_string()
                    .contains("JsonSchema")
        })
    }
    impl<'ast> Visit<'ast> for Docs {
        fn visit_item_struct(&mut self, item: &'ast syn::ItemStruct) {
            if derives_schema(&item.attrs) {
                self.take(&item.attrs);
                self.fields(&item.fields);
            }
            visit::visit_item_struct(self, item);
        }
        fn visit_item_enum(&mut self, item: &'ast syn::ItemEnum) {
            if derives_schema(&item.attrs) {
                self.take(&item.attrs);
                for variant in &item.variants {
                    self.take(&variant.attrs);
                    self.fields(&variant.fields);
                }
            }
            visit::visit_item_enum(self, item);
        }
    }
    let mut docs = Docs::default();
    docs.visit_file(tree);
    docs.0
}

/// Whether a file implements a tool, whose input types are then what the
/// model reads. Other `JsonSchema` types describe settings and the protocol
/// to people, who are told the default names.
fn defines_a_tool(text: &str) -> bool {
    text.contains("Tool for ")
        && (text.contains("schema_for!(") || text.contains("input_schema::<"))
}

#[test]
fn name_3_tool_input_descriptions_name_no_engine_directory_or_product() {
    let tool_files: Vec<_> = ws()
        .production()
        .filter(|f| !f.rel.starts_with("tests/") && defines_a_tool(&f.text))
        .collect();
    allow::non_vacuous("engine_dirs::schema_docs", tool_files.len(), 30);
    let found = par_flat_map(tool_files.iter().copied(), |f| {
        schema_docs(&f.tree(View::Prod))
            .into_iter()
            .filter(|(doc, _)| {
                mentions_engine_dir(doc) || mentions_the_product(doc) || doc.contains("/worktree-")
            })
            .map(|(_, span)| {
                let hit = f.hit(span);
                Finding {
                    key: hit.file.clone(),
                    hit,
                }
            })
            .collect()
    });
    allow::check(
        "engine_dirs::SCHEMA_DOCS",
        found,
        &[],
        "describe the input without the instance's names; the tool's guide renders them from \
         the session's `Layout`",
    );
}

#[test]
fn the_schema_doc_scan_reads_the_docs_of_schema_types_only() {
    let tree: syn::File = syn::parse_str(
        r#"
        /// The input in `.agix/x`.
        #[derive(Deserialize, JsonSchema)]
        struct Input {
            /// Branch `agix/worktree-a`.
            a: String,
        }
        /// Not a schema: `.agix`.
        struct Other;
        "#,
    )
    .unwrap();
    let docs: Vec<String> = schema_docs(&tree).into_iter().map(|(d, _)| d).collect();
    assert_eq!(docs.len(), 2, "{docs:?}");
    assert!(docs.iter().all(|d| !d.contains("Not a schema")));
}

#[test]
fn the_product_scan_knows_the_product_name_from_the_protocols_names() {
    for yes in [
        "Agix",
        "You are Agix, an agent",
        "agix/1.0",
        "/etc/agix/x.json",
        "a newer Agix;",
        "by Agix.",
    ] {
        assert!(mentions_the_product(yes), "{yes}");
    }
    for no in [
        ".agix",
        "agix.session-export",
        "agix.",
        "AGIX_CONFIG_HOME",
        "agix-egress-relay",
        ".agix-installed",
        "agixd",
        "Agixy",
    ] {
        assert!(!mentions_the_product(no), "{no}");
    }
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
