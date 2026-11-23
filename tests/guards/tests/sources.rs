//! Rules over what the source says, found in the syntax tree rather than in
//! the text: only `base::paths::home_dir` reads the home variables, and only
//! the listed files ask it for a home (SRC-1), a plugin's disclosure
//! names no script engine (SRC-2), every `.rs` file in a package belongs to a
//! target (SRC-3), and only the crates in `MAY_SPAWN` start a process
//! themselves (SRC-4), this machine's `Process` is built only where a
//! session's default providers are assembled (SRC-5), and settings JSON is
//! merged only in the places `SETTINGS_OVERLAY_SITES` lists (SRC-6).
//!
//! Everything else reaches a process through the session's `&dyn Process`,
//! which is what puts it on the machine the host chose, inside the session's
//! sandbox and environment policy, in a session of its own with no
//! controlling terminal, and in the ledger that reaps it. A crate that names
//! `std::process::Command`, `tokio::process::Command` or `process_wrap` in
//! production code has stepped around all of that.
//!
//! Where an instance keeps its state is decided once, by the process entry
//! point, and passed down. A module that reads `$HOME` instead has quietly
//! opted out: it writes to the invoking user's home no matter what the
//! instance was configured with, which breaks redirected deployments and
//! leaves test runs writing to a real `~/.agix`. Two things earn a place in
//! `HOME_READERS` (the files that call `home_dir`): the entry point, and facts about the machine — the sandbox
//! has to know the real home to protect `~/.ssh`, and a system prompt
//! describing the environment describes the user's. A storage root never
//! qualifies; the answer there is to take the root as a parameter.

use std::collections::BTreeSet;

use guards::allow::{self, Finding};
use guards::scan::{self, Callee};
use guards::{fswalk, par_flat_map, ws, View};
use syn::{Expr, Lit};

const HOME_ENV_READERS: &[(&str, &str)] = &[(
    "crates/core/src/paths/mod.rs",
    "`home_dir()`, the one place this machine's home is looked up: HOME, \
     else USERPROFILE. Tests that need a home call `home_dir()` too.",
)];

const HOME_READERS: &[(&str, &str)] = &[
    (
        "daemon/tests/credentials_in_file_tools.rs",
        "names this machine's home stores in the default deny-read list, which \
         is what the case is about.",
    ),
    (
        "crates/assembly/src/config.rs",
        "the entry point: resolves AGIX_CONFIG_HOME (or $HOME/.agix) once, \
         then passes it down. Everything else receives what this decides.",
    ),
    (
        "crates/settings/src/source.rs",
        "judges whether a user-tier settings file that is a symlink resolves \
         inside the user's home — where a dotfile manager puts the real file, \
         a fact about the machine, not a place to store things.",
    ),
    (
        "crates/permissions/src/dangerous.rs",
        "judges whether a grant reaches the user's home or above — a fact \
         about the machine the rule will run on, not a place to store things.",
    ),
    (
        "crates/core/src/frozen/mod.rs",
        "collects the environment snapshot — home alongside os, shell and \
         git state. A fact about the machine, put into the system prompt, \
         and replaceable by tests because it lives in the snapshot.",
    ),
    (
        "crates/core/src/interface/exec/sandbox.rs",
        "the default deny-read list is ~/.ssh and friends, so building it \
         needs this machine's home. It is policy, which is why it sits with \
         the contract rather than with a backend.",
    ),
    (
        "crates/platform/src/sandbox/roots.rs",
        "a backend confines a process on this machine, and the git \
         configuration a project's `include.path` names may start with `~/`, \
         which only this machine's home resolves. The state root it protects \
         comes from `SandboxPolicy::state_root` alone.",
    ),
    (
        "crates/core/src/paths/sensitive.rs",
        "`known_state_roots`: where another instance on this machine keeps its \
         keys by default, `~/.agix`, guarded like `~/.ssh` — a fact about the \
         machine. The session's own state root still comes as a parameter.",
    ),
    (
        "crates/core/src/paths/sensitive/spellings.rs",
        "a root spelled from `$HOME` in a command (`~/.agix/memory`) is the same \
         root: the home directory is how the shell will read the text, a fact about \
         the machine. The state root itself still comes as a parameter.",
    ),
    (
        "crates/platform/src/sandbox/tests/macos.rs",
        "the cases build the policy a confined command gets by default, which \
         names this machine's home.",
    ),
    (
        "crates/platform/src/sandbox/tests/strict.rs",
        "the cases build the policy a confined command gets by default, which \
         names this machine's home.",
    ),
    (
        "crates/platform/src/sandbox/tests/wrap.rs",
        "the cases build the policy a confined command gets by default, which \
         names this machine's home.",
    ),
    (
        "crates/settings/src/trust.rs",
        "expands a leading `~/` in `app.trusted_projects` patterns, the same \
         `~` permission rules expand — the home the user means by \
         `~/work/**`. A fact about the machine, never a place Agix writes.",
    ),
    (
        "crates/permissions/src/path_rule.rs",
        "expands `~` in permission rules and in the paths tools are asked to \
         touch — the home a shell expands `~` to when the command runs. A fact \
         about the machine, never a place Agix writes.",
    ),
    (
        "crates/tools/src/bash/classify/mod.rs",
        "same deny-list, reused by the command classifier so `cat ~/.ssh/id_rsa` \
         is not waved through as a read-only command.",
    ),
    (
        "crates/tools/src/bash/sandbox.rs",
        "the `~/` of `sandbox.cache_dirs`, where pip and npm keep their \
         caches.",
    ),
    (
        "crates/tools/src/bash/tests.rs",
        "the tests check both of the above against this machine's home.",
    ),
];

/// The crates whose production code may start a process itself, each with
/// what it starts.
const MAY_SPAWN: &[(&str, &str)] = &[
    (
        "platform",
        "`LocalProcess`, the one `Process` on this machine, and the sandbox \
         backends' fixed-argv probes",
    ),
    (
        "mcp",
        "stdio MCP servers: processes the operator configured, started in a \
         session of their own through process-wrap",
    ),
];

const MAY_BUILD_LOCAL_PROCESS: &[(&str, &str)] = &[(
    "crates/runtime/src/exec.rs",
    "the session's default providers when the host gives none; everything else, hooks \
     included, runs on the providers that session ended up with",
)];

/// Crates that still start one themselves, each with what takes it away.
const NOT_YET: &[(&str, &str)] = &[];

const ENGINE_MODULES: &[&str] = &["rquickjs", "script_host"];
const ENGINE_TYPES: &[&str] = &["ScriptEngine"];

const DISCLOSURE: &str = "crates/packages/src/disclosure.rs";

const NOT_IN_A_TARGET: &[(&str, &str)] = &[];

const SELFTEST_FIXTURES: &str = "tests/guards/fixtures/";

const HOME_VARS: &[&str] = &["HOME", "USERPROFILE"];

fn is_home_var_call(segments: &[String], args: &[Expr]) -> bool {
    let reads_env =
        matches!(segments, [.., env, var] if env == "env" && (var == "var" || var == "var_os"));
    reads_env
        && matches!(
            args.first(),
            Some(Expr::Lit(l)) if matches!(&l.lit, Lit::Str(s) if HOME_VARS.contains(&s.value().as_str()))
        )
}

fn is_home_dir_call(segments: &[String]) -> bool {
    segments.last().is_some_and(|s| s == "home_dir")
}

fn calls_matching<'a>(
    files: impl IntoIterator<Item = &'a guards::SourceFile>,
    wanted: fn(&[String], &[Expr]) -> bool,
) -> Vec<Finding> {
    par_flat_map(files, |f| {
        scan::calls(f, View::Full)
            .into_iter()
            .filter(|c| match &c.callee {
                Callee::Path(p) => wanted(p, &c.args),
                Callee::Method(_) => false,
            })
            .map(|c| Finding {
                key: c.hit.file.clone(),
                hit: c.hit,
            })
            .collect()
    })
}

#[test]
fn only_home_dir_reads_the_home_variables() {
    allow::check(
        "sources::HOME_ENV_READERS",
        calls_matching(ws().files(), is_home_var_call),
        HOME_ENV_READERS,
        "ask `base::paths::home_dir()` instead of reading HOME or USERPROFILE; tests too",
    );
}

#[test]
fn only_the_entry_point_and_environment_facts_resolve_a_home_directory() {
    allow::check(
        "sources::HOME_READERS",
        calls_matching(ws().files(), |p, _| is_home_dir_call(p)),
        HOME_READERS,
        "take the state root as a parameter (see `base::paths::ConfigPaths`); if this really \
         is a fact about the machine rather than a place to store things, list the file with \
         the reason",
    );
}

#[test]
fn the_home_scans_recognise_every_way_of_asking() {
    let src = r#"
        use std::env;
        fn a() {
            let _ = std::env::var("HOME");
            let _ = env::var_os("USERPROFILE");
            let _ = dirs::home_dir();
            let _ = env::var("PATH");
            // std::env::var("HOME")
        }
    "#;
    let f = guards::source::standalone("fixture.rs", src.to_string()).expect("parses");
    let lines = |wanted: fn(&[String], &[Expr]) -> bool| -> Vec<usize> {
        scan::calls(&f, View::Full)
            .into_iter()
            .filter(|c| match &c.callee {
                Callee::Path(p) => wanted(p, &c.args),
                Callee::Method(_) => false,
            })
            .map(|c| c.hit.line)
            .collect()
    };
    assert_eq!(lines(is_home_var_call), vec![4, 5]);
    assert_eq!(lines(|p, _| is_home_dir_call(p)), vec![6]);
    allow::non_vacuous("sources::home_lookups", ws().files().count(), 400);
}

#[test]
fn plugin_disclosure_names_no_script_engine() {
    let file = ws().file(DISCLOSURE);
    let paths = scan::paths(file, View::Prod);
    allow::non_vacuous("sources::disclosure", paths.len(), 10);
    let offenders: Vec<String> = paths
        .iter()
        .filter(|(segments, _)| {
            segments
                .first()
                .is_some_and(|s| ENGINE_MODULES.contains(&s.as_str()))
                || segments.iter().any(|s| ENGINE_TYPES.contains(&s.as_str()))
        })
        .map(|(_, hit)| hit.to_string())
        .collect();
    assert!(
        offenders.is_empty(),
        "{DISCLOSURE} names a script engine:\n  {}\n\nDisclosure is about what a package \
         says; it must not depend on how its scripts run. Move the engine-specific part to \
         the script host.",
        offenders.join("\n  ")
    );
}

#[test]
fn every_rust_file_in_a_package_belongs_to_a_target() {
    let known: BTreeSet<&str> = ws().files().map(|f| f.rel.as_str()).collect();
    let mut found = Vec::new();
    let mut seen = 0;
    for package in ws().packages() {
        for path in fswalk::rust_files(&ws().root().join(&package.dir)) {
            seen += 1;
            let rel = path
                .strip_prefix(ws().root())
                .unwrap_or(&path)
                .to_string_lossy()
                .replace('\\', "/");
            if !rel.starts_with(SELFTEST_FIXTURES) && !known.contains(rel.as_str()) {
                found.push(Finding {
                    hit: guards::Hit {
                        file: rel.clone(),
                        line: 1,
                        snippet: "no target's module tree reaches this file".to_string(),
                    },
                    key: rel,
                });
            }
        }
    }
    allow::non_vacuous("sources::orphans", seen, 400);
    allow::check(
        "sources::NOT_IN_A_TARGET",
        found,
        NOT_IN_A_TARGET,
        "declare the file with `mod`, or delete it: the compiler never reads it, so nothing \
         it says is checked",
    );
}

/// The names `process-wrap` goes by in the workspace's code.
fn process_wrap_idents() -> BTreeSet<String> {
    ws().packages()
        .iter()
        .flat_map(|p| &p.deps)
        .filter(|d| d.name == "process-wrap")
        .map(|d| d.ident.clone())
        .chain(["process_wrap".to_string()])
        .collect()
}

/// `std::process::Command`, `tokio::process::Command`, a glob import of
/// either module, or anything under `process-wrap`; `use` aliases are undone
/// before this sees the path.
fn names_a_spawner(segments: &[String], wrappers: &BTreeSet<String>) -> bool {
    let path = match segments {
        [colons, rest @ ..] if colons == "::" => rest,
        all => all,
    };
    match path {
        [root, process, item, ..]
            if (root == "std" || root == "tokio")
                && process == "process"
                && (item == "Command" || item == "*") =>
        {
            true
        }
        [first, ..] => wrappers.contains(first),
        [] => false,
    }
}

fn spawners(files: Vec<&guards::SourceFile>, view: View) -> Vec<Finding> {
    let wrappers = process_wrap_idents();
    par_flat_map(files, |f| {
        scan::paths(f, view)
            .into_iter()
            .filter(|(segments, _)| names_a_spawner(segments, &wrappers))
            .map(|(_, hit)| Finding {
                key: f.package.clone(),
                hit,
            })
            .collect()
    })
}

/// The packages under `tests/` drive the product from outside — cargo, git,
/// a built `agixd` — and are never linked into it.
fn is_test_support(package: &str) -> bool {
    ws().package(package).dir.starts_with("tests/")
}

#[test]
fn only_the_listed_crates_start_a_process_themselves() {
    let production: Vec<_> = ws()
        .production()
        .filter(|f| !is_test_support(&f.package))
        .collect();
    allow::non_vacuous("sources::MAY_SPAWN", production.len(), 300);
    let listed: Vec<(&str, &str)> = MAY_SPAWN.iter().chain(NOT_YET).copied().collect();
    allow::check(
        "sources::MAY_SPAWN",
        spawners(production, View::Prod),
        &listed,
        "build a `ProcessSpec` and run it through the session's `&dyn Process` (`ToolContext.exec`, \
         `Agent.exec`, `base::engine_exec::EngineExec`), or a `platform::LocalProcess` for an \
         operator's process; a crate that has to start one itself goes on `MAY_SPAWN` with \
         what it starts, one on its way off goes on `NOT_YET`",
    );
}

#[test]
fn the_spawn_scan_sees_through_aliases() {
    let src = r#"
        use tokio::process as p;
        use std::process::Command as C;
        use std::process::*;
        fn a() {
            p::Command::new("x");
            C::new("y");
            ::std::process::Command::new("z");
            process_wrap::tokio::CommandWrap::with_new("w", |_| {});
            let _ = std::process::id();
            let _ = std::process::Stdio::null();
        }
    "#;
    let f = guards::source::standalone("fixture.rs", src.to_string()).expect("parses");
    let lines: BTreeSet<usize> = spawners(vec![&f], View::Full)
        .into_iter()
        .map(|finding| finding.hit.line)
        .collect();
    assert_eq!(lines, BTreeSet::from([3, 4, 6, 7, 8, 9]));
}

fn names_the_local_process(segments: &[String]) -> bool {
    let path = match segments {
        [colons, rest @ ..] if colons == "::" => rest,
        all => all,
    };
    match path {
        [first, rest @ ..] if first == "platform" => rest
            .iter()
            .any(|s| s == "LocalProcess" || s.starts_with("local_providers")),
        _ => false,
    }
}

fn local_process_builders(files: Vec<&guards::SourceFile>, view: View) -> Vec<Finding> {
    par_flat_map(files, |f| {
        scan::paths(f, view)
            .into_iter()
            .filter(|(segments, _)| names_the_local_process(segments))
            .map(|(_, hit)| Finding {
                key: f.rel.clone(),
                hit,
            })
            .collect()
    })
}

#[test]
fn only_the_default_assembly_builds_this_machines_process() {
    let production: Vec<_> = ws()
        .production()
        .filter(|f| f.package != "platform" && !is_test_support(&f.package))
        .collect();
    allow::non_vacuous("sources::local_process", production.len(), 300);
    allow::check(
        "sources::MAY_BUILD_LOCAL_PROCESS",
        local_process_builders(production, View::Prod),
        MAY_BUILD_LOCAL_PROCESS,
        "take the session's `ExecProviders` (`Agent.exec`, `ToolContext.exec`, `HookExec`) \
         instead of building this machine's process; a host that runs elsewhere swaps those \
         and anything built here would still run locally",
    );
}

#[test]
fn the_local_process_scan_sees_the_ways_of_naming_it() {
    let src = r#"
        use platform::LocalProcess as L;
        fn a() {
            let _ = platform::LocalProcess::default();
            let _ = platform::local_providers_with(env);
            let _ = L::default();
            let _ = platform::LocalNetwork::default();
        }
    "#;
    let f = guards::source::standalone("fixture.rs", src.to_string()).expect("parses");
    let lines: BTreeSet<usize> = local_process_builders(vec![&f], View::Full)
        .into_iter()
        .map(|finding| finding.hit.line)
        .collect();
    assert_eq!(lines, BTreeSet::from([2, 4, 5, 6]));
}

/// Every place production code merges settings JSON, as `<file>::<fn>`,
/// outside the primitives' own file. A layer laid over loaded settings
/// anywhere else would skip what these do after the merge — the rule lists
/// taken apart, the providers settled, the managed layer put back on top — so
/// a new one goes through `LoadedSettings::overlay` or comes here with why it
/// cannot.
const SETTINGS_OVERLAY_SITES: &[(&str, &str)] = &[
    (
        "crates/settings/src/loaded.rs::LayerFold::apply",
        "the loader: each layer of the source folded over the ones below it, the `cli` \
         layers joined for `config.get`, a project's own MCP server declarations gathered",
    ),
    (
        "crates/settings/src/loaded/managed_layer.rs::LoadedSettings::lay_managed_values",
        "`LoadedSettings::with_managed`: the managed layer put over everything, after the \
         loader and after every overlay",
    ),
    (
        "crates/settings/src/overlay.rs::LoadedSettings::overlay",
        "the one in-memory overlay: a host's changes and a session's own settings",
    ),
    (
        "crates/settings/src/overlay.rs::session_layer_problems",
        "checks a session's layer against the defaults; the merged copy is thrown away",
    ),
    (
        "crates/settings/src/managed.rs::followed_by",
        "two managed layers of one source joined before either is laid over anything",
    ),
    (
        "crates/settings/src/managed.rs::unfitting_value",
        "checks a managed file against the defaults; the merged copy is thrown away",
    ),
    (
        "crates/settings/src/edit.rs::merge",
        "`config.set` writing one tier's files: the file's own JSON, not loaded settings",
    ),
    (
        "crates/settings/src/edit.rs::merge_entry",
        "`config.set` writing one entry of a tier's file",
    ),
    (
        "crates/settings/src/edit.rs::extract_all",
        "`config.set` gathering the credentials it moves into `settings.local.json`",
    ),
    (
        "daemon/src/main.rs::settings_layer",
        "builds the `cli` layer from `--set` assignments; the loader merges it",
    ),
];

const MERGE_PRIMITIVES: &[&str] = &["merge_json_values", "merge_fitting"];
const MERGE_PRIMITIVES_FILE: &str = "crates/settings/src/merge.rs";

fn settings_merges(files: Vec<&guards::SourceFile>, view: View) -> Vec<Finding> {
    par_flat_map(files, |f| {
        let fns = scan::fns(f, view);
        scan::calls(f, view)
            .into_iter()
            .filter(|call| {
                matches!(&call.callee, Callee::Path(p)
                    if p.last().is_some_and(|name| MERGE_PRIMITIVES.contains(&name.as_str())))
            })
            .map(|call| {
                let within = fns
                    .iter()
                    .filter(|s| (s.hit.line..s.hit.line + s.lines).contains(&call.hit.line))
                    .min_by_key(|s| s.lines)
                    .map_or("", |s| s.qual.as_str());
                Finding {
                    key: format!("{}::{within}", f.rel),
                    hit: call.hit,
                }
            })
            .collect()
    })
}

#[test]
fn settings_are_merged_only_where_it_is_listed() {
    let production: Vec<_> = ws()
        .production()
        .filter(|f| !is_test_support(&f.package) && f.rel != MERGE_PRIMITIVES_FILE)
        .collect();
    allow::non_vacuous("sources::SETTINGS_OVERLAY_SITES", production.len(), 300);
    allow::check(
        "sources::SETTINGS_OVERLAY_SITES",
        settings_merges(production, View::Prod),
        SETTINGS_OVERLAY_SITES,
        "lay the settings over with `LoadedSettings::overlay` (a host's in-memory changes, a \
         session's own) or `LoadedSettings::with_managed`, which do what has to follow a \
         merge; a merge that is not an overlay of loaded settings goes on the list with why",
    );
}

#[test]
fn the_settings_merge_scan_sees_each_way_of_calling_it() {
    let src = r#"
        use settings::merge_json_values as merge;
        fn a() {
            settings::merge_json_values(&mut x, y);
            crate::merge::merge_fitting(&mut x, y, s);
            merge_fitting(&mut x, y, s);
            other::merge(&mut x, y);
        }
    "#;
    let f = guards::source::standalone("fixture.rs", src.to_string()).expect("parses");
    let found = settings_merges(vec![&f], View::Full);
    let lines: BTreeSet<usize> = found.iter().map(|finding| finding.hit.line).collect();
    assert_eq!(lines, BTreeSet::from([4, 5, 6]));
    assert!(
        found.iter().all(|finding| finding.key == "fixture.rs::a"),
        "{found:?}"
    );
}
