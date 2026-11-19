//! What `base` — the contracts, the shared data and the pure decisions, and the
//! small fs/clock/path/text tools every crate shares — may link, read from `cargo metadata`: its normal dependencies, target
//! sections included, are exactly `BASE_MAY_DEPEND_ON`. A crate added there
//! is linked into every host, so it needs a reason here first; one the code
//! stopped using has to come off. Dev dependencies are not limited. That base
//! names no other crate of this workspace is `deps.rs`'s rule.
//!
//! base's tokio features stay within `BASE_TOKIO_FEATURES`: no `process`,
//! `signal` or `net`, so `cargo check -p base` proves base starts no process
//! and opens no socket through tokio — the workspace's tokio leaves those
//! three to the crates that ask for them.
//!
//! base's production code only shrinks (a line ratchet), reaches the filesystem
//! only in the files listed in `BASE_FS`, and calls no model: I/O and model
//! calls belong to the crate that owns the decision.

use std::collections::{BTreeMap, BTreeSet};

use guards::allow::{self, Finding};
use guards::scan::{self, Callee};
use guards::{par_flat_map, ratchet, ws, Dep, Hit, Package, SourceFile, TargetKind, View};

const BASE_TOKIO_FEATURES: &[(&str, &str)] = &[
    ("fs", "the workspace default"),
    ("io-util", "the workspace default"),
    ("macros", "select! and the tests"),
    ("parking_lot", "the workspace default"),
    ("rt", "spawn, spawn_blocking and the runtime handle"),
    ("rt-multi-thread", "the workspace default"),
    ("sync", "channels and locks"),
    ("time", "timeouts"),
];

const BASE_MAY_DEPEND_ON: &[(&str, &str)] = &[
    ("tokio", "the dir lock, timers and the runtime handle"),
    ("tokio-util", "CancellationToken in the contracts"),
    ("futures", "the streaming contracts"),
    ("async-trait", "the contracts"),
    ("serde", "the shared data types"),
    ("serde_json", "the shared data types"),
    (
        "schemars",
        "JsonSchema on the shared types the settings schema names",
    ),
    ("thiserror", "the error types"),
    ("anyhow", "error codes"),
    ("tracing", "logging"),
    ("uuid", "ids"),
    ("time", "the environment's clock and dates"),
    ("globset", "URL rules"),
    ("bs58", "ids"),
    ("url", "URL rules and the public-suffix check"),
    ("libc", "dir_lock's flock, unix only"),
];

fn manifest_hit(p: &Package, dep: &Dep) -> Hit {
    let file = format!("{}/Cargo.toml", p.dir);
    let text = std::fs::read_to_string(ws().root().join(&file)).unwrap_or_default();
    let (line, snippet) = text
        .lines()
        .enumerate()
        .find(|(_, l)| {
            l.trim_start()
                .strip_prefix(dep.name.as_str())
                .is_some_and(|rest| rest.starts_with([' ', '=', '.']))
        })
        .map(|(i, l)| (i + 1, l.trim().to_string()))
        .unwrap_or((0, format!("{} = …", dep.name)));
    Hit {
        file,
        line,
        snippet,
    }
}

#[test]
fn base_links_only_the_listed_crates() {
    let base = ws().package("base");
    let found: Vec<Finding> = base
        .normal_deps()
        .map(|d| Finding {
            key: d.name.clone(),
            hit: manifest_hit(base, d),
        })
        .collect();
    allow::non_vacuous("base::BASE_MAY_DEPEND_ON", found.len(), 5);
    allow::check(
        "base::BASE_MAY_DEPEND_ON",
        found,
        BASE_MAY_DEPEND_ON,
        "base is linked into every host: keep a dependency out of it (put the code that needs \
         it in the crate that uses it), or list it with what base needs it for; a dependency \
         base no longer uses comes off both the manifest and the list",
    );
}

#[test]
fn base_builds_tokio_without_processes_signals_or_sockets() {
    let base = ws().package("base");
    let allowed: BTreeSet<&str> = BASE_TOKIO_FEATURES.iter().map(|(f, _)| *f).collect();
    let tokio: Vec<&Dep> = base.normal_deps().filter(|d| d.name == "tokio").collect();
    allow::non_vacuous("base::BASE_TOKIO_FEATURES", tokio.len(), 1);
    let extra: Vec<String> = tokio
        .iter()
        .flat_map(|d| {
            d.features
                .iter()
                .filter(|f| !allowed.contains(f.as_str()))
                .map(|f| format!("{}: `{f}`", manifest_hit(base, d)))
        })
        .collect();
    assert!(
        extra.is_empty(),
        "[base::BASE_TOKIO_FEATURES] base's tokio has features outside the list:\n  {}\n\n\
         How to fix: a process goes through `&dyn Process`, a request through `&dyn Network`; \
         the crate that implements them asks tokio for the feature. The workspace's tokio \
         (root Cargo.toml) counts too, since base inherits it.\n\
         The list is `BASE_TOKIO_FEATURES` in tests/guards/tests/base.rs.",
        extra.join("\n  ")
    );
}

const BASE_LINES: &str = "ratchet/base.txt";

/// Production files of base that reach the filesystem themselves, each with why.
const BASE_FS: &[(&str, &str)] = &[
    (
        "crates/core/src/fs.rs",
        "write_atomic: the one temp-and-rename every crate writes through",
    ),
    (
        "crates/core/src/dir_lock.rs",
        "flock on a settings or memory directory, shared by two K1 crates",
    ),
    (
        "crates/core/src/path.rs",
        "resolve_physically canonicalizes the prefix that exists",
    ),
    (
        "crates/core/src/paths/mod.rs",
        "ensure_local_data_gitignore beside a tier's local files",
    ),
    (
        "crates/core/src/paths/sensitive.rs",
        "PromptFiles: every file the engine reads for the model goes through the sensitive-path check; SensitivePaths resolves its roots and lists the scene directories once, when built",
    ),
    (
        "crates/core/src/paths/sensitive/spellings.rs",
        "SensitivePaths resolves each root it is built with once, through its symbolic links",
    ),
    (
        "crates/core/src/interface/exec/filesystem.rs",
        "the Filesystem contract's provided methods call its own canonicalize and metadata",
    ),
    (
        "crates/core/src/interface/exec/in_process/filesystem.rs",
        "InProcessFilesystem: the in-process implementation of that contract",
    ),
];

/// Production files of base that call a model, each with why.
const BASE_MODEL_CALLS: &[(&str, &str)] = &[(
    "crates/core/src/provider/routing.rs",
    "the task router is the `Model` every routed call goes through: it passes the call to a \
     provider's client and on to the next when one cannot take it; runtime and agents reach it \
     through `base`, so it cannot live in `model`",
)];

const FS_METHODS: &[&str] = &[
    "canonicalize",
    "metadata",
    "symlink_metadata",
    "read_dir",
    "read_link",
    "try_exists",
    "exists",
    "is_file",
    "is_dir",
];

fn base_files() -> impl Iterator<Item = &'static SourceFile> {
    ws().production()
        .filter(|f| f.package == "base" && f.target == TargetKind::Lib)
}

#[test]
fn base_production_code_only_shrinks() {
    let lines: usize = base_files().map(|f| f.production_lines()).sum();
    allow::non_vacuous("base::lines", lines, 10_000);
    ratchet::check(
        BASE_LINES,
        BTreeMap::from([("lines crates/core".to_string(), lines)]),
        0,
    );
}

#[test]
fn base_reaches_the_filesystem_only_where_listed() {
    let found = par_flat_map(base_files(), |f| {
        let by_path = scan::paths(f, View::Prod)
            .into_iter()
            .filter(|(p, _)| {
                let p: Vec<&str> = p
                    .iter()
                    .map(String::as_str)
                    .filter(|s| *s != "::")
                    .collect();
                matches!(p.as_slice(), ["std", "fs", ..] | ["tokio", "fs", ..])
            })
            .map(|(_, hit)| hit);
        let by_method = scan::calls(f, View::Prod)
            .into_iter()
            .filter(|c| matches!(&c.callee, Callee::Method(m) if FS_METHODS.contains(&m.as_str())))
            .map(|c| c.hit);
        by_path
            .chain(by_method)
            .map(|hit| Finding {
                key: f.rel.clone(),
                hit,
            })
            .collect()
    });
    allow::check(
        "base::BASE_FS",
        found,
        BASE_FS,
        "base holds contracts and data: do the I/O in the crate that owns it (settings, memory, \
         prompt-files, session-state, platform) or through `exec.filesystem`; or list the file with why",
    );
}

#[test]
fn base_calls_no_model() {
    let found = par_flat_map(base_files(), |f| {
        scan::calls(f, View::Prod)
            .into_iter()
            .filter(|c| matches!(&c.callee, Callee::Method(m) if m == "stream"))
            .map(|c| Finding {
                key: f.rel.clone(),
                hit: c.hit,
            })
            .collect()
    });
    allow::check(
        "base::BASE_MODEL_CALLS",
        found,
        BASE_MODEL_CALLS,
        "a model call is an implementation: put it in the crate that owns the decision (memory's \
         select_memories_from is the example), or list the file with why",
    );
}
