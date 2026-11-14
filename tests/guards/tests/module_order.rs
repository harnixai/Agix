//! ORD-1: a module that reads what an earlier module provided says so.
//!
//! `SessionContext::mcp_for_delegates` and `delegator` return what modules
//! before the caller provided, and nothing otherwise. A crate that calls one
//! must declare, in `Module::after`, the module that provides it, so `build()`
//! refuses the order in which the call would quietly find nothing. Every id
//! an `after` names is a module that exists.

use std::collections::BTreeSet;

use guards::allow::{self, Finding};
use guards::scan::{self, Callee, Returned};
use guards::{ws, View};

/// (method on `SessionContext`, the module id that provides what it reads).
const READS: &[(&str, &str)] = &[("mcp_for_delegates", "mcp"), ("delegator", "agents")];

fn crate_modules(package: &str) -> (Vec<String>, Vec<String>) {
    let mut ids = Vec::new();
    let mut after = Vec::new();
    for f in ws().production().filter(|f| f.package == package) {
        let consts = scan::str_consts(f, View::Prod);
        for r in scan::trait_fn_returns(f, View::Prod, "Module", "id") {
            match r {
                Returned::Literal(s) => ids.push(s),
                Returned::Const(c) => ids.extend(
                    consts
                        .iter()
                        .filter(|(n, _)| *n == c)
                        .map(|(_, v)| v.clone()),
                ),
            }
        }
        for list in scan::trait_fn_strings(f, View::Prod, "Module", "after") {
            after.extend(list);
        }
    }
    (ids, after)
}

#[test]
fn a_module_that_reads_an_earlier_modules_output_declares_it_in_after() {
    let mut found = Vec::new();
    let mut known = BTreeSet::new();
    let mut declared = BTreeSet::new();
    let mut module_crates = 0;
    for pkg in ws().packages() {
        let (ids, after) = crate_modules(&pkg.name);
        known.extend(ids.iter().cloned());
        declared.extend(after.iter().cloned());
        if ids.is_empty() {
            continue;
        }
        module_crates += 1;
        for f in ws().production().filter(|f| f.package == pkg.name) {
            for call in scan::calls(f, View::Prod) {
                let Callee::Method(m) = &call.callee else {
                    continue;
                };
                for (method, provider) in READS {
                    if m == method && !after.iter().any(|a| a == provider) {
                        found.push(Finding {
                            key: format!("{}:{method}", pkg.name),
                            hit: call.hit.clone(),
                        });
                    }
                }
            }
        }
    }
    allow::non_vacuous("module_order::module_crates", module_crates, 3);
    allow::check(
        "module_order::READS",
        found,
        &[],
        "add the providing module's id to this crate's `Module::after`",
    );
    let stale: Vec<_> = declared.difference(&known).collect();
    assert!(
        stale.is_empty(),
        "`Module::after` names {stale:?}, which no `impl Module` has as its id"
    );
}
