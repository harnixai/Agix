//! Which crate may depend on which, read from `cargo metadata`.
//!
//! Every workspace member has one tier in `CRATES`, and a first-party normal or
//! build edge (any target, optional or not) points to a lower tier. The kernel
//! is three tiers so that its crates cannot lean on each other in a circle: K1
//! needs only the contracts and the machine, K2 builds on K1, K3 (`runtime`)
//! on both. Modules sit on the kernel and reach each other only through the
//! edges in `MODULE_EDGES`; only the assembly and the shell know which modules
//! exist. Test support may use anything and nothing else may use it; dev edges
//! are not limited.
//!
//! `platform` — this machine's execution providers — sits on `base` alone, and
//! only the crates in `MAY_USE_PLATFORM` reach the machine through it.
//!
//! Downstream projects (Harnix, IPDrun, …) are outside this workspace and
//! nothing here can hold their edges; what they may rely on is the facade
//! crate `agix` and `docs/agix-sdk.md` §9.

use std::collections::{BTreeMap, BTreeSet};

use guards::allow::{self, Finding};
use guards::{ws, Dep, Hit, Package};

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum Tier {
    Contract,
    Machine,
    K1,
    K2,
    K3,
    Module,
    Assembly,
    Shell,
    TestSupport,
}

use Tier::*;

pub(crate) const CRATES: &[(&str, Tier)] = &[
    ("base", Contract),
    ("platform", Machine),
    ("model", K1),
    ("memory", K1),
    ("prompt-files", K1),
    ("session-state", K1),
    ("history", K1),
    ("telemetry", K1),
    ("scene", K1),
    ("permissions", K1),
    ("ptc", K1),
    ("settings", K1),
    ("hooks", K2),
    ("tools", K2),
    ("session", K2),
    ("compaction", K2),
    ("runtime", K3),
    ("team", Module),
    ("packages", Module),
    ("script-host", Module),
    ("mcp", Module),
    ("skills", Module),
    ("agents", Module),
    ("demo-rust-plugin", Module),
    ("hostile-plugins", TestSupport),
    ("assembly", Assembly),
    ("agix", Shell),
    ("daemon", Shell),
    ("rpc-client", TestSupport),
    ("blackbox", TestSupport),
    ("test-runner", TestSupport),
    ("mcp-toy-server", TestSupport),
    ("mcp-oauth-mock", TestSupport),
    ("guards", TestSupport),
];

/// Module → module edges, each with why the one module needs the other.
const MODULE_EDGES: &[(&str, &str, &str)] = &[(
    "agents",
    "team",
    "the team tools live in agents, and team is the coordinator they drive",
)];

/// Edges between two crates of one tier below the modules, each with why.
const SAME_TIER: &[(&str, &str, &str)] = &[];

/// Edges that break the tiers today, each with what removes it.
const NOT_YET: &[(&str, &str, &str)] = &[];

/// The crates whose normal or build dependencies may name `platform`, each
/// with what it does on this machine. The rest only implement or consume
/// contracts, which is what lets a host that executes elsewhere use them
/// unchanged.
pub(crate) const MAY_USE_PLATFORM: &[(&str, &str, &str)] = &[
    (
        "tools",
        "crates/tools",
        "the sandbox status and probes `Bash` reports",
    ),
    (
        "runtime",
        "crates/runtime",
        "builds a session's providers when the host gives none",
    ),
    ("model", "crates/model", "the model endpoint's HTTP client"),
    (
        "mcp",
        "crates/mcp",
        "stdio servers are local processes; OAuth is a local HTTP client",
    ),
    (
        "packages",
        "crates/packages",
        "fetches packages over this machine's network",
    ),
    (
        "assembly",
        "crates/assembly",
        "fences the sockets it serves from the sandbox; the discovery lock's process identity",
    ),
    (
        "agix",
        "crates/agix",
        "re-exports the crash hook and orphan reaping an embedder's process installs",
    ),
    (
        "daemon",
        "daemon",
        "the crash hook and reaping a previous run's commands",
    ),
    (
        "blackbox",
        "tests/blackbox",
        "installs the TLS provider for its own clients",
    ),
];

const WASM_RUNTIMES: &[&str] = &["wasmtime", "wasmer", "wasmi"];

fn tier(name: &str) -> Tier {
    CRATES
        .iter()
        .find(|(n, _)| *n == name)
        .map(|(_, t)| *t)
        .unwrap_or_else(|| panic!("`{name}` has no tier in CRATES (tests/guards/tests/deps.rs)"))
}

/// Normal and build edges into the workspace.
fn first_party_edges(p: &Package) -> impl Iterator<Item = &Dep> {
    p.normal_deps().filter(|d| d.first_party)
}

fn manifest_hit(p: &Package, dep: &str) -> Hit {
    let file = if p.dir.is_empty() {
        "Cargo.toml".to_string()
    } else {
        format!("{}/Cargo.toml", p.dir)
    };
    let text = std::fs::read_to_string(ws().root().join(&file)).unwrap_or_default();
    let declares = |line: &str| {
        let line = line.trim_start();
        line.strip_prefix(dep)
            .is_some_and(|rest| rest.starts_with([' ', '=', '.']))
            || (line.starts_with('[') && line.contains(&format!("dependencies.{dep}]")))
    };
    let (line, snippet) = text
        .lines()
        .enumerate()
        .find(|(_, l)| declares(l))
        .map(|(i, l)| (i + 1, l.trim().to_string()))
        .unwrap_or((0, format!("{dep} = …")));
    Hit {
        file,
        line,
        snippet,
    }
}

fn edge(p: &Package, dep: &str) -> Finding {
    Finding {
        key: format!("{} → {dep}", p.name),
        hit: manifest_hit(p, dep),
    }
}

fn keyed(list: &[(&'static str, &'static str, &'static str)]) -> Vec<(String, &'static str)> {
    list.iter()
        .map(|(a, b, why)| (format!("{a} → {b}"), *why))
        .collect()
}

fn check_keyed(
    guard: &str,
    found: Vec<Finding>,
    list: &[(&'static str, &'static str, &'static str)],
    fix: &str,
) {
    let keyed = keyed(list);
    let allowed: Vec<(&str, &str)> = keyed.iter().map(|(k, why)| (k.as_str(), *why)).collect();
    allow::check(guard, found, &allowed, fix);
}

#[test]
fn every_member_has_a_tier_and_every_tier_is_a_member() {
    let members: BTreeSet<&str> = ws().packages().iter().map(|p| p.name.as_str()).collect();
    let listed: BTreeMap<&str, usize> = CRATES.iter().fold(BTreeMap::new(), |mut m, (n, _)| {
        *m.entry(*n).or_insert(0) += 1;
        m
    });
    let unlisted: Vec<&&str> = members
        .iter()
        .filter(|m| !listed.contains_key(**m))
        .collect();
    let gone: Vec<&&str> = listed.keys().filter(|n| !members.contains(**n)).collect();
    let twice: Vec<&&str> = listed
        .iter()
        .filter(|(_, c)| **c > 1)
        .map(|(n, _)| n)
        .collect();
    assert!(
        unlisted.is_empty() && gone.is_empty() && twice.is_empty(),
        "CRATES in tests/guards/tests/deps.rs is out of step with the workspace:\n  \
         members with no tier: {unlisted:?} (give each one a tier)\n  \
         entries that are no member: {gone:?} (renamed or removed: take them off)\n  \
         entries listed twice: {twice:?}"
    );
}

#[derive(Default)]
struct Edges {
    upward: Vec<Finding>,
    between_modules: Vec<Finding>,
    same_tier: Vec<Finding>,
}

/// Every first-party edge that does not simply point down, by what may
/// excuse it.
fn edges() -> Edges {
    let mut out = Edges::default();
    let mut count = 0;
    for p in ws().packages() {
        let from = tier(&p.name);
        let mut seen = BTreeSet::new();
        for d in first_party_edges(p) {
            if !seen.insert(d.name.as_str()) {
                continue;
            }
            count += 1;
            let to = tier(&d.name);
            if from == TestSupport {
                continue;
            }
            if to == TestSupport || to > from {
                out.upward.push(edge(p, &d.name));
            } else if to == from && from == Module {
                out.between_modules.push(edge(p, &d.name));
            } else if to == from {
                out.same_tier.push(edge(p, &d.name));
            }
        }
    }
    allow::non_vacuous("deps::first-party edges", count, 80);
    out
}

#[test]
fn no_first_party_edge_points_up() {
    check_keyed(
        "deps::NOT_YET",
        edges().upward,
        NOT_YET,
        "depend on a lower tier — reach the higher crate through a kernel trait or a \
         `runtime::module::Module` — or, if the edge must stay for now, list it with what removes it",
    );
}

#[test]
fn a_module_reaches_another_module_only_through_a_listed_edge() {
    check_keyed(
        "deps::MODULE_EDGES",
        edges().between_modules,
        MODULE_EDGES,
        "list the edge with why the one module needs the other, or go through a kernel trait",
    );
}

#[test]
fn crates_of_one_tier_below_the_modules_do_not_depend_on_each_other() {
    check_keyed(
        "deps::SAME_TIER",
        edges().same_tier,
        SAME_TIER,
        "move one of the crates to another tier in CRATES, or list the edge with its reason",
    );
}

#[test]
fn only_the_listed_crates_reach_the_machine_through_platform() {
    let mut found = Vec::new();
    for p in ws().packages() {
        if first_party_edges(p).any(|d| d.name == "platform") {
            found.push(Finding {
                key: p.name.clone(),
                hit: manifest_hit(p, "platform"),
            });
        }
    }
    let allowed: Vec<(&str, &str)> = MAY_USE_PLATFORM
        .iter()
        .map(|(n, _, why)| (*n, *why))
        .collect();
    allow::check(
        "deps::MAY_USE_PLATFORM",
        found,
        &allowed,
        "take the providers from the `ToolContext` or the session, or list the crate with what \
         it does on this machine",
    );
    for (name, dir, _) in MAY_USE_PLATFORM {
        assert_eq!(
            &ws().package(name).dir,
            dir,
            "MAY_USE_PLATFORM lists {name} at {dir}, which is not where it lives"
        );
    }
}

/// Daemon features the facade does not forward, each with why.
const FACADE_OMITS: &[(&str, &str)] = &[
    (
        "ws",
        "the WebSocket listener is agixd's; the facade does not export it",
    ),
    ("full", "coding plus ws"),
];

fn without_deps(values: &[String]) -> BTreeSet<String> {
    values
        .iter()
        .filter(|v| !v.starts_with("dep:"))
        .cloned()
        .collect()
}

#[test]
fn the_facade_forwards_the_daemon_switches() {
    let daemon = &ws().package("daemon").features;
    let facade = &ws().package("agix").features;
    let omitted: BTreeSet<&str> = FACADE_OMITS.iter().map(|(f, _)| *f).collect();
    let mut problems = Vec::new();
    for (name, values) in daemon.iter().filter(|(n, _)| !omitted.contains(n.as_str())) {
        let Some(mine) = facade.get(name) else {
            problems.push(format!("agix has no feature `{name}` (daemon has it)"));
            continue;
        };
        let mut theirs = without_deps(values);
        if name == "default" && theirs.remove("full") {
            theirs.insert("coding".to_string());
        }
        if without_deps(mine) != theirs {
            problems.push(format!(
                "feature `{name}`: agix has {:?}, daemon has {theirs:?}",
                without_deps(mine)
            ));
        }
    }
    for name in facade.keys().filter(|n| !daemon.contains_key(*n)) {
        problems.push(format!("agix has `{name}`, daemon does not"));
    }
    assert!(
        problems.is_empty(),
        "the facade's features drifted from daemon's (FACADE_OMITS lists the ones left out):\n  {}",
        problems.join("\n  ")
    );
}

/// Who may hold the session's state as its concrete type, with `attach`,
/// `restore_from_log` and the goal rounds; everyone else sees `dyn Session`.
const MAY_USE_SESSION_STATE: &[(&str, &str)] = &[
    (
        "runtime",
        "builds the engine around the session's state: attaches it, restores it \
         from the log, claims goal rounds",
    ),
    (
        "assembly",
        "the session pool injects the state into the engine and serves the goal \
         methods to the host",
    ),
];

#[test]
fn only_the_listed_crates_hold_the_session_state_itself() {
    let mut found = Vec::new();
    for p in ws().packages() {
        if first_party_edges(p).any(|d| d.name == "session-state") {
            found.push(Finding {
                key: p.name.clone(),
                hit: manifest_hit(p, "session-state"),
            });
        }
    }
    allow::check(
        "deps::MAY_USE_SESSION_STATE",
        found,
        MAY_USE_SESSION_STATE,
        "take `base::context::Session` (`ToolContext.session`) instead, or list the crate with \
         why it needs the concrete type; tests reach it through a dev-dependency",
    );
}

#[test]
fn the_contracts_and_the_machine_sit_at_the_bottom() {
    let base: Vec<&str> = first_party_edges(ws().package("base"))
        .map(|d| d.name.as_str())
        .collect();
    assert!(
        base.is_empty(),
        "base depends on {base:?}; the contracts name no other crate of this workspace"
    );
    let platform: Vec<&str> = first_party_edges(ws().package("platform"))
        .map(|d| d.name.as_str())
        .filter(|d| *d != "base")
        .collect();
    assert!(
        platform.is_empty(),
        "platform depends on {platform:?}; it implements base's contracts and nothing above them"
    );
}

#[test]
fn no_crate_links_a_webassembly_runtime() {
    let mut found = Vec::new();
    for p in ws().packages() {
        for d in &p.deps {
            let is_wasm = WASM_RUNTIMES
                .iter()
                .any(|r| d.name == *r || d.name.starts_with(&format!("{r}-")));
            if is_wasm {
                found.push(manifest_hit(p, &d.name).to_string());
            }
        }
    }
    assert!(
        found.is_empty(),
        "a WebAssembly runtime is linked; extensions are Rust plugins or QuickJS scripts:\n  {}",
        found.join("\n  ")
    );
}

/// A build without `scripts` must link no JavaScript engine for extensions.
#[test]
fn the_script_carrier_is_optional_at_compile_time() {
    for name in ["assembly", "daemon"] {
        let p = ws().package(name);
        let declared: Vec<&Dep> = p
            .normal_deps()
            .filter(|d| d.name == "script-host")
            .collect();
        assert!(!declared.is_empty(), "{name} must declare `script-host`");
        for d in declared {
            assert!(
                d.optional,
                "{name}'s `script-host` must be optional: {}",
                manifest_hit(p, "script-host")
            );
        }
        let scripts = p
            .features
            .get("scripts")
            .unwrap_or_else(|| panic!("{name} has no `scripts` feature"));
        assert!(
            scripts.iter().any(|f| f == "dep:script-host"),
            "{name}'s `scripts` feature must turn on `dep:script-host`: {scripts:?}"
        );
    }
}

/// Module switches are the assembly's: a kernel crate that declared or
/// forwarded one would put a module's `#[cfg]` below the assembly.
#[test]
fn no_kernel_crate_declares_or_forwards_a_module_feature() {
    let modules: BTreeSet<&str> = CRATES
        .iter()
        .filter(|(_, t)| *t == Module)
        .map(|(n, _)| *n)
        .collect();
    let switches: BTreeSet<&str> = ws()
        .package("assembly")
        .features
        .iter()
        .filter(|(_, enables)| {
            enables
                .iter()
                .any(|e| e.strip_prefix("dep:").is_some_and(|d| modules.contains(d)))
        })
        .map(|(name, _)| name.as_str())
        .collect();
    allow::non_vacuous("deps::module switches", switches.len(), 4);

    let mut found = Vec::new();
    for p in ws().packages().iter().filter(|p| tier(&p.name) <= K3) {
        for (feature, enables) in &p.features {
            if modules.contains(feature.as_str()) || switches.contains(feature.as_str()) {
                found.push(format!("{}: declares `{feature}`", p.name));
            }
            for e in enables {
                let (krate, sub) = match e.strip_prefix("dep:") {
                    Some(d) => (d, None),
                    None => match e.split_once('/') {
                        Some((k, f)) => (k.trim_end_matches('?'), Some(f)),
                        None => (e.as_str(), None),
                    },
                };
                let names_a_module =
                    modules.contains(krate) || krate == "assembly" || krate == "daemon";
                let forwards_a_switch = sub.is_some_and(|f| switches.contains(f));
                if names_a_module || forwards_a_switch {
                    found.push(format!("{}: `{feature}` enables `{e}`", p.name));
                }
            }
        }
    }
    assert!(
        found.is_empty(),
        "kernel crates carry module features; a module's `#[cfg]` belongs in assembly and daemon \
         only:\n  {}",
        found.join("\n  ")
    );
}

/// The first-party crates a build of `root` links for its normal and build
/// edges, with `features` on at the root: optional deps only when a feature
/// turns them on, features forwarded the way cargo unifies them for this one
/// package alone.
fn linked_crates(root: &str, defaults: bool, features: &[&str]) -> BTreeSet<String> {
    let mut on: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    let mut queue: Vec<(String, Vec<String>, bool)> = vec![(
        root.to_string(),
        features.iter().map(|f| f.to_string()).collect(),
        defaults,
    )];
    while let Some((name, requested, with_defaults)) = queue.pop() {
        let p = ws().package(&name);
        let known = on.entry(name.clone()).or_default();
        let mut todo: Vec<String> = requested;
        if with_defaults && p.features.contains_key("default") {
            todo.push("default".into());
        }
        while let Some(f) = todo.pop() {
            if !known.insert(f.clone()) {
                continue;
            }
            for e in p.features.get(&f).into_iter().flatten() {
                match e.split_once('/') {
                    Some((dep, sub)) => {
                        let dep = dep.trim_end_matches('?');
                        let weak = e.contains("?/");
                        if !weak {
                            known.insert(format!("dep:{dep}"));
                        }
                        if !weak || known.contains(&format!("dep:{dep}")) {
                            queue.push((dep.to_string(), vec![sub.to_string()], false));
                        }
                    }
                    None => {
                        if let Some(dep) = e.strip_prefix("dep:") {
                            known.insert(format!("dep:{dep}"));
                        } else {
                            todo.push(e.clone());
                        }
                    }
                }
            }
        }
        let enabled_deps: Vec<String> = known
            .iter()
            .filter_map(|f| f.strip_prefix("dep:").map(str::to_string))
            .collect();
        let implicit: BTreeSet<&str> = p
            .features
            .values()
            .flatten()
            .filter_map(|e| e.strip_prefix("dep:"))
            .collect();
        for d in p.normal_deps().filter(|d| d.first_party) {
            let linked = !d.optional
                || enabled_deps.contains(&d.ident)
                || enabled_deps.contains(&d.name)
                || (!implicit.contains(d.name.as_str()) && known.contains(&d.name));
            if !linked {
                continue;
            }
            queue.push((d.name.clone(), d.features.clone(), d.default_features));
        }
    }
    on.into_keys().collect()
}

/// The locked build states that no extension execution path is in the binary,
/// which is only true if the crates are absent from the graph.
#[test]
fn the_full_tier_links_neither_the_package_layer_nor_the_script_carrier() {
    let full = linked_crates("daemon", false, &["full"]);
    allow::non_vacuous("deps::full tier closure", full.len(), 15);
    let leaked: Vec<&str> = ["packages", "script-host"]
        .into_iter()
        .filter(|c| full.contains(*c))
        .collect();
    assert!(
        leaked.is_empty(),
        "`daemon --no-default-features --features full` links {leaked:?}; those crates enter \
         only through the `scripts` and `plugin-packages` features"
    );
    let default = linked_crates("daemon", true, &[]);
    assert!(
        default.contains("packages") && default.contains("script-host"),
        "the default daemon build must link `packages` and `script-host`, or this guard cannot \
         tell a locked build from an ordinary one: {default:?}"
    );
}
