//! Holds the `plugin.toml` example in `docs/plugin_packages.md` to the
//! manifest types: the example must parse as a manifest, and it must name
//! exactly the fields each manifest type deserializes — a field added to the
//! types and left out of the document, or the reverse, fails here.

use std::collections::BTreeSet;
use std::path::PathBuf;

const MANIFEST_SRC: &str = include_str!("../src/manifest.rs");

fn doc() -> String {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../docs/plugin_packages.md");
    std::fs::read_to_string(path).expect("the plugin package document exists")
}

fn example(doc: &str) -> String {
    let start = doc.find("```toml\n").expect("a toml example") + "```toml\n".len();
    let end = start + doc[start..].find("```").expect("the example ends");
    doc[start..end].to_string()
}

fn fields_of(type_name: &str) -> BTreeSet<String> {
    let head = format!("pub struct {type_name} {{\n");
    let start = MANIFEST_SRC
        .find(&head)
        .expect("the type is in manifest.rs")
        + head.len();
    let body = &MANIFEST_SRC[start..start + MANIFEST_SRC[start..].find("\n}").unwrap()];
    body.lines()
        .filter_map(|line| line.trim().strip_prefix("pub "))
        .filter_map(|rest| rest.split(':').next())
        .map(str::to_string)
        .collect()
}

fn keys(table: &toml::Table) -> BTreeSet<String> {
    table.keys().cloned().collect()
}

fn first_table<'a>(value: &'a toml::Value, what: &str) -> &'a toml::Table {
    match value {
        toml::Value::Array(items) => items[0].as_table(),
        other => other.as_table(),
    }
    .unwrap_or_else(|| panic!("`{what}` is a table"))
}

#[test]
fn the_documented_example_is_a_valid_manifest() {
    let example = example(&doc());
    toml::from_str::<packages::manifest::PluginManifest>(&example)
        .unwrap_or_else(|e| panic!("the example in docs/plugin_packages.md does not parse: {e}"));
}

#[test]
fn the_documented_example_names_every_manifest_field_and_no_other() {
    let example = example(&doc());
    let root: toml::Table = toml::from_str(&example).unwrap();

    let top: BTreeSet<String> = keys(&root)
        .into_iter()
        .filter(|k| !k.starts_with(packages::manifest::HOST_SECTION_PREFIX))
        .collect();
    assert_eq!(top, fields_of("PluginManifest"), "top-level sections");

    for (section, type_name) in [
        ("plugin", "PluginMeta"),
        ("script", "ScriptPayload"),
        ("mcp", "McpPayload"),
        ("scene", "SceneSection"),
        ("agent", "AgentDef"),
        ("skills", "SkillsSection"),
        ("command", "CommandDef"),
        ("function", "FunctionDef"),
    ] {
        let table = first_table(&root[section], section);
        assert_eq!(
            keys(table),
            fields_of(type_name),
            "[{section}] vs {type_name}"
        );
    }
}
