//! Every settings.json example in the README and `docs/` — fenced `json` /
//! `jsonc` blocks and inline `{…}` or `"key": …` spans whose top-level keys
//! are all settings keys — must pass the generated settings schema and load
//! without a value left out; the few the docs show on purpose as ignored must
//! be refused by both. An editor flagging a documented example, or the loader
//! quietly dropping one the schema accepts, is the same mismatch.
//!
//! The README's settings tables (a `Setting | Default` header) must name only
//! settings keys, and a default written as one literal must be the built-in one.
//! Every key the schema has is governed by a row of the key table
//! (`settings::keys`).
//!
//! `docs/settings.md` lists every settings key exactly once, and what its
//! scope, trust and restart cells say is what the key table says.

#![cfg(feature = "agents")]
#![allow(clippy::unwrap_used)]

use std::path::{Path, PathBuf};

use assembly::settings_sections;
use base::interface::config_source::{InMemoryLayers, Tier};
use serde_json::Value;
use settings::{settings_json_schema, unknown_settings_keys, LoadRequest, LoadedSettings};

struct Example {
    at: String,
    value: Value,
}

fn repo() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

fn documents() -> Vec<PathBuf> {
    let mut files = vec![repo().join("README.md")];
    let mut docs: Vec<PathBuf> = std::fs::read_dir(repo().join("docs"))
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .filter(|path| path.extension().is_some_and(|e| e == "md"))
        .collect();
    docs.sort();
    files.extend(docs);
    files
}

fn without_comments(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    let mut in_string = false;
    while let Some(c) = chars.next() {
        if in_string {
            out.push(c);
            match c {
                '\\' => out.extend(chars.next()),
                '"' => in_string = false,
                _ => {}
            }
            continue;
        }
        match (c, chars.peek()) {
            ('"', _) => {
                in_string = true;
                out.push(c);
            }
            ('/', Some('/')) => {
                for c in chars.by_ref() {
                    if c == '\n' {
                        out.push('\n');
                        break;
                    }
                }
            }
            _ => out.push(c),
        }
    }
    out
}

fn is_settings(value: &Value, keys: &[String]) -> bool {
    value.as_object().is_some_and(|object| {
        !object.is_empty()
            && object
                .keys()
                .all(|k| k == "$schema" || keys.iter().any(|known| known == k))
    })
}

fn examples(keys: &[String]) -> Vec<Example> {
    let mut found = Vec::new();
    for file in documents() {
        let name = file
            .strip_prefix(repo())
            .unwrap_or(&file)
            .display()
            .to_string();
        let text = std::fs::read_to_string(&file).unwrap();
        let mut lines = text.lines().enumerate();
        while let Some((number, line)) = lines.next() {
            let trimmed = line.trim_start();
            if let Some(lang) = trimmed.strip_prefix("```") {
                let body: Vec<&str> = lines
                    .by_ref()
                    .map(|(_, l)| l)
                    .take_while(|l| !l.trim_start().starts_with("```"))
                    .collect();
                if matches!(lang.trim(), "json" | "jsonc") {
                    if let Ok(value) =
                        serde_json::from_str::<Value>(&without_comments(&body.join("\n")))
                    {
                        if is_settings(&value, keys) {
                            found.push(Example {
                                at: format!("{name}:{}", number + 1),
                                value,
                            });
                        }
                    }
                }
                continue;
            }
            for span in line.split('`').skip(1).step_by(2) {
                let span = span.trim();
                let candidate = if span.starts_with('{') {
                    span.to_string()
                } else if span.starts_with('"') {
                    format!("{{{span}}}")
                } else {
                    continue;
                };
                if let Ok(value) = serde_json::from_str::<Value>(&candidate) {
                    if is_settings(&value, keys) {
                        found.push(Example {
                            at: format!("{name}:{}", number + 1),
                            value,
                        });
                    }
                }
            }
        }
    }
    found
}

/// What the loader makes of one file on its own: the values it left out.
fn left_out_by_loader(example: &Example) -> Vec<String> {
    let source =
        InMemoryLayers::new().with(Tier::Global, false, &example.at, example.value.clone());
    let dir = PathBuf::from("/nonexistent");
    let settings = LoadedSettings::load_from(LoadRequest {
        source: &source,
        global_dir: dir.join("global"),
        scene_dir: dir.join("scene"),
        local_dir: dir.join("project/.agix"),
        scope: "code",
        default_model: "model",
        sections: &settings_sections::all(),
        layout: Default::default(),
    });
    let mut left_out: Vec<String> = settings
        .ignored
        .into_iter()
        .map(|ignored| format!("{}: {}", ignored.key, ignored.error))
        .collect();
    left_out.extend(
        unknown_settings_keys(&example.value, &settings_sections::all())
            .into_iter()
            .map(|key| format!("unknown key {key}")),
    );
    left_out
}

#[test]
fn every_documented_settings_example_is_valid_and_loads_whole() {
    let schema = settings_json_schema(&settings_sections::all());
    let keys: Vec<String> = schema["properties"]
        .as_object()
        .unwrap()
        .keys()
        .cloned()
        .collect();
    let validator = jsonschema::validator_for(&schema).unwrap();
    let examples = examples(&keys);
    for expected in ["README.md", "docs/hooks.md", "docs/settings.md"] {
        assert!(
            examples.iter().any(|e| e.at.starts_with(expected)),
            "no settings example found in {expected}; the extraction is broken"
        );
    }
    // Values the docs show on purpose as what gets ignored.
    let counterexamples = [
        serde_json::json!({"language": 5}),
        serde_json::json!({"permissions": {"mode": "plann"}}),
    ];
    // `plugin.list`'s result, which shares a settings key and is no settings file.
    let not_settings = [serde_json::json!({"plugins": []})];

    let mut wrong = Vec::new();
    for example in examples.iter().filter(|e| !not_settings.contains(&e.value)) {
        let schema_errors: Vec<String> = validator
            .iter_errors(&example.value)
            .map(|error| format!("{error} at {}", error.instance_path()))
            .collect();
        let left_out = left_out_by_loader(example);
        if counterexamples.contains(&example.value) {
            if schema_errors.is_empty() || left_out.is_empty() {
                wrong.push(format!(
                    "{}: a counterexample, but schema errors {schema_errors:?}, loader left out {left_out:?}",
                    example.at
                ));
            }
        } else if !schema_errors.is_empty() || !left_out.is_empty() {
            wrong.push(format!(
                "{}: {}: schema errors {schema_errors:?}, loader left out {left_out:?}",
                example.at, example.value
            ));
        }
    }
    assert!(wrong.is_empty(), "{wrong:#?}");
}

#[test]
fn the_schema_accepts_what_the_loader_starts_from() {
    let validator =
        jsonschema::validator_for(&settings_json_schema(&settings_sections::all())).unwrap();
    for minimal in [
        serde_json::json!({}),
        serde_json::json!({"model": {"order": ["anthropic"], "tasks": {"main": "fast"}}}),
        serde_json::json!({"sandbox": {"network": {"mode": "deny_all"}}}),
        serde_json::json!({"turn": {"parallel_tools": 4}}),
        serde_json::json!({"mcp": {"servers": {"gh": {"enabled": false}}}}),
        serde_json::json!({"app": {"history": "project", "system_proxy": false}}),
        serde_json::json!({"team": {"enabled": true, "stage_concurrency": 2}}),
    ] {
        let errors: Vec<String> = validator
            .iter_errors(&minimal)
            .map(|e| e.to_string())
            .collect();
        assert!(errors.is_empty(), "{minimal}: {errors:?}");
    }
    for refused in [
        serde_json::json!({"permissions": {"mode": "bubble"}}),
        serde_json::json!({"permissions": {"allow": [{"tool": "Bash"}]}}),
        serde_json::json!({"model": {"max_tokens": "many"}}),
    ] {
        assert!(!validator.is_valid(&refused), "{refused} passed the schema");
    }
}

fn committed_schema_path() -> PathBuf {
    repo().join("docs/schemas/settings.schema.json")
}

fn rendered_schema() -> String {
    serde_json::to_string_pretty(&settings_json_schema(&settings_sections::all())).unwrap() + "\n"
}

#[test]
fn settings_schema_matches_committed_file() {
    let existing = std::fs::read_to_string(committed_schema_path()).unwrap_or_default();
    assert!(
        existing == rendered_schema(),
        "docs/schemas/settings.schema.json is out of date; run `cargo test -p agix-assembly --test settings_schema write_settings_schema -- --ignored` and commit it"
    );
}

#[test]
#[ignore]
fn write_settings_schema() {
    let path = committed_schema_path();
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, rendered_schema()).unwrap();
}

#[test]
fn every_settings_key_has_a_row() {
    let schema = settings_json_schema(&settings_sections::all());
    assert_eq!(
        settings::keys::key_gaps(&schema, &settings_sections::all()),
        Vec::<String>::new()
    );
}

#[test]
fn an_untrusted_project_may_narrow_what_modules_own_but_not_widen_it() {
    use base::interface::config_source::Tier;
    let layers = |global: Value, project: Value| {
        InMemoryLayers::new()
            .with(Tier::Global, false, "global", global)
            .with(Tier::Project, false, "project", project)
    };
    let load = |source: &InMemoryLayers| {
        let dir = std::env::temp_dir().join("agix-settings-schema-test");
        LoadedSettings::load_from(LoadRequest {
            source,
            global_dir: dir.join("global"),
            scene_dir: dir.join("scene"),
            local_dir: dir.join("work/.agix"),
            scope: "code",
            default_model: "m",
            sections: &settings_sections::all(),
            layout: Default::default(),
        })
    };
    let untrusted = serde_json::json!({"app": {"trust_all_projects": false}});
    let mut global = untrusted.clone();
    global["plugins"] = serde_json::json!({"enabled": false, "allow": ["a", "b"]});
    global["team"] = serde_json::json!({"stage_concurrency": 3});
    let source = layers(
        global,
        serde_json::json!({
            "plugins": {"enabled": true, "allow": ["a"]},
            "team": {"enabled": true, "stage_concurrency": 30, "member_idle_secs": 10},
            "scripts": [{"path": "x.js", "point": "prompt.block", "entry": "f"}],
        }),
    );
    let settings = load(&source);
    let plugins: assembly::plugins::PluginsConfig = settings.values.section();
    assert!(!plugins.enabled);
    assert_eq!(plugins.allow, ["a"]);
    let team: agents::TeamSettings = settings.values.section();
    assert!(!team.enabled);
    assert_eq!(team.stage_concurrency, 3);
    assert_eq!(team.member_idle_secs, 10);
    assert!(settings_sections::ScriptBindings::of(&settings.values).is_empty());
    assert_eq!(
        settings.requires_trust(),
        [
            "plugins.enabled",
            "scripts",
            "team.enabled",
            "team.stage_concurrency"
        ]
    );
}

struct TableRow {
    at: String,
    key: String,
    default: Option<String>,
}

/// The rows of each table in `text` whose header starts `| <setting> | <default> |`.
fn settings_table_rows(name: &str, text: &str, headers: &[(&str, &str)]) -> Vec<TableRow> {
    let cells = |line: &str| -> Vec<String> {
        line.trim()
            .trim_matches('|')
            .split('|')
            .map(|cell| cell.trim().to_string())
            .collect()
    };
    let backticked = |cell: &str| {
        cell.strip_prefix('`')
            .and_then(|c| c.strip_suffix('`'))
            .filter(|c| !c.contains('`'))
            .map(String::from)
    };
    let mut rows = Vec::new();
    let mut in_table = false;
    for (number, line) in text.lines().enumerate() {
        if !line.starts_with('|') {
            in_table = false;
            continue;
        }
        let cells = cells(line);
        if cells.len() >= 2 && headers.contains(&(cells[0].as_str(), cells[1].as_str())) {
            in_table = true;
            continue;
        }
        if !in_table || cells[0].starts_with("---") {
            continue;
        }
        if let Some(key) = backticked(&cells[0]) {
            rows.push(TableRow {
                at: format!("{name}:{}", number + 1),
                key,
                default: cells.get(1).and_then(|cell| backticked(cell)),
            });
        }
    }
    rows
}

/// The schema of the dotted `key`, where `name[]` is one item of the list `name`.
fn schema_at<'a>(root: &'a Value, key: &str) -> Option<&'a Value> {
    fn resolve<'a>(root: &'a Value, mut node: &'a Value) -> &'a Value {
        while let Some(target) = node.get("$ref").and_then(Value::as_str) {
            match root.pointer(target.trim_start_matches('#')) {
                Some(next) => node = next,
                None => break,
            }
        }
        node
    }
    fn alternatives<'a>(root: &'a Value, node: &'a Value) -> Vec<&'a Value> {
        let node = resolve(root, node);
        let mut all = vec![node];
        for branch in ["anyOf", "oneOf", "allOf"] {
            for inner in node
                .get(branch)
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
            {
                all.push(resolve(root, inner));
            }
        }
        all
    }
    let mut node = root;
    for segment in key.split('.') {
        let (name, item) = match segment.strip_suffix("[]") {
            Some(name) => (name, true),
            None => (segment, false),
        };
        node = alternatives(root, node)
            .into_iter()
            .find_map(|n| n.get("properties").and_then(|p| p.get(name)))?;
        if item {
            node = alternatives(root, node)
                .into_iter()
                .find_map(|n| n.get("items"))?;
        }
    }
    Some(node)
}

fn built_in_defaults() -> Value {
    let source = InMemoryLayers::new();
    let dir = PathBuf::from("/nonexistent");
    serde_json::to_value(
        LoadedSettings::load_from(LoadRequest {
            source: &source,
            global_dir: dir.join("global"),
            scene_dir: dir.join("scene"),
            local_dir: dir.join("project/.agix"),
            scope: "code",
            default_model: "model",
            sections: &settings_sections::all(),
            layout: Default::default(),
        })
        .values,
    )
    .unwrap()
}

#[test]
fn the_readme_settings_tables_name_real_keys_and_their_defaults() {
    let schema = settings_json_schema(&settings_sections::all());
    let defaults = built_in_defaults();
    let headers = [("Setting", "Default"), ("设置", "缺省")];
    let mut rows = Vec::new();
    for file in ["README.md", "docs/README.zh-CN.md"] {
        let text = std::fs::read_to_string(repo().join(file)).unwrap();
        let found = settings_table_rows(file, &text, &headers);
        assert!(
            found.len() > 10,
            "{file}: no settings table found; the extraction is broken"
        );
        rows.extend(found);
    }
    let mut wrong = Vec::new();
    for row in &rows {
        if schema_at(&schema, &row.key).is_none() {
            wrong.push(format!("{}: `{}` is no settings key", row.at, row.key));
            continue;
        }
        let (Some(literal), false) = (&row.default, row.key.contains("[]")) else {
            continue;
        };
        let stated = serde_json::from_str::<Value>(literal)
            .unwrap_or_else(|_| Value::String(literal.clone()));
        // An unset setting (`app.worktree_dir`) has its default where it is
        // read; the table states that one, and there is nothing here to hold
        // it to.
        let Some(built_in) = row
            .key
            .split('.')
            .try_fold(&defaults, |node, segment| node.get(segment))
            .filter(|value| !value.is_null())
        else {
            continue;
        };
        if *built_in != stated {
            wrong.push(format!(
                "{}: `{}` defaults to {built_in}, the table says {stated}",
                row.at, row.key
            ));
        }
    }
    assert!(wrong.is_empty(), "{wrong:#?}");
}

fn scope_cell(scope: settings::keys::Scope) -> &'static str {
    use settings::keys::Scope;
    match scope {
        Scope::Process => "进程",
        Scope::Pair => "项目与场景",
        Scope::Session => "会话",
    }
}

fn trust_cell(key: &[&str], sections: &settings::section::Sections) -> String {
    use settings::keys::{Order, Trust};
    if settings::keys::global_only_at(key, sections).is_some() {
        return "只认全局文件层、cli 与 managed".into();
    }
    match settings::keys::trust_of(key, sections).expect("every key has a trust") {
        Trust::Free => "照用".into(),
        Trust::Withheld => "信任之后才生效".into(),
        Trust::Tighten(order) => {
            let tighter = match order {
                Order::TrueTighter => "只能设为 true",
                Order::FalseTighter => "只能设为 false",
                Order::Mode => "不得比下层的模式更宽",
                Order::NetworkMode => {
                    "deny_all 最严,其次 allowlist 与 proxy(两者不能互换),unrestricted 最宽"
                }
                Order::EnvMode => "allowlist 比 inherit 严",
                Order::Subset => "只能是下层的子集",
                Order::SubsetEmptyIsAll => "只能是下层的子集(空表示全部)",
                Order::Superset => "只能是下层的超集",
                Order::Lower => "只能更小(null 表示不限,最宽)",
                Order::Higher => "只能更大",
                Order::LowerZeroIsWidest => "只能更小(0 表示不限,最宽)",
                Order::ProviderDefinedBelow => "只能列下层已定义的 provider",
            };
            format!("只能收紧({tighter})")
        }
    }
}

#[test]
fn the_settings_manual_lists_every_key_once_as_the_key_table_says() {
    let sections = settings_sections::all();
    let schema = settings_json_schema(&sections);
    let leaves = settings::SchemaKeys::of(&sections).leaves();
    let defaults = built_in_defaults();
    let text = std::fs::read_to_string(repo().join("docs/settings.md")).unwrap();
    let mut wrong = Vec::new();
    let mut listed: Vec<String> = Vec::new();
    let mut in_table = false;
    let mut rows = 0;
    for (number, line) in text.lines().enumerate() {
        let at = format!("docs/settings.md:{}", number + 1);
        if !line.starts_with('|') {
            in_table = false;
            continue;
        }
        let cells: Vec<String> = line
            .trim()
            .trim_matches('|')
            .split('|')
            .map(|cell| cell.trim().to_string())
            .collect();
        if cells[0] == "键" {
            assert_eq!(
                cells,
                ["键", "缺省", "作用域", "未受信项目", "改后", "说明"],
                "{at}: the header"
            );
            in_table = true;
            continue;
        }
        if !in_table || cells[0].starts_with("---") {
            continue;
        }
        rows += 1;
        if cells.len() != 6 {
            wrong.push(format!("{at}: {} cells, not 6", cells.len()));
            continue;
        }
        let Some(shown) = cells[0].strip_prefix('`').and_then(|c| c.strip_suffix('`')) else {
            wrong.push(format!("{at}: the key {} is not in backticks", cells[0]));
            continue;
        };
        if shown.contains("[]") {
            if schema_at(&schema, shown).is_none() {
                wrong.push(format!("{at}: `{shown}` is no settings key"));
            }
            continue;
        }
        let key: Vec<&str> = shown
            .split('.')
            .map(|segment| {
                if segment.starts_with('<') && segment.ends_with('>') {
                    "*"
                } else {
                    segment
                }
            })
            .collect();
        let dotted = key.join(".");
        if !leaves.contains(&dotted) {
            wrong.push(format!("{at}: `{shown}` is no settings key"));
            continue;
        }
        listed.push(dotted);

        let scope = scope_cell(settings::keys::scope_of(&key, &sections).unwrap());
        if cells[2] != scope {
            wrong.push(format!(
                "{at}: `{shown}` scope is {scope}, the table says {}",
                cells[2]
            ));
        }
        let trust = trust_cell(&key, &sections);
        if cells[3] != trust {
            wrong.push(format!(
                "{at}: `{shown}` trust is {trust}, the table says {}",
                cells[3]
            ));
        }
        let after = if settings::keys::is_read_at_start(&key, &sections) {
            "要重启"
        } else {
            "立即"
        };
        if cells[4] != after {
            wrong.push(format!(
                "{at}: `{shown}` applies {after}, the table says {}",
                cells[4]
            ));
        }

        let literal = cells[1]
            .strip_prefix('`')
            .and_then(|c| c.strip_suffix('`'))
            .filter(|c| !c.contains('`'));
        let built_in = key
            .iter()
            .try_fold(&defaults, |node, segment| node.get(segment))
            .filter(|value| !value.is_null());
        if let (Some(literal), Some(built_in)) = (literal, built_in) {
            let stated = serde_json::from_str::<Value>(literal)
                .unwrap_or_else(|_| Value::String(literal.to_string()));
            if *built_in != stated {
                wrong.push(format!(
                    "{at}: `{shown}` defaults to {built_in}, the table says {stated}"
                ));
            }
        }
    }
    assert!(
        rows > 80,
        "docs/settings.md: {rows} rows; the extraction is broken"
    );
    for leaf in &leaves {
        match listed.iter().filter(|key| *key == leaf).count() {
            1 => {}
            0 => wrong.push(format!("`{leaf}` is in no table of docs/settings.md")),
            n => wrong.push(format!("`{leaf}` is in {n} rows of docs/settings.md")),
        }
    }
    assert!(wrong.is_empty(), "{wrong:#?}");
}
