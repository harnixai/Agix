//! `docs/ARCHITECTURE.md` §1 against the code (G9): the crate table, the
//! crates said to reach the machine through `platform`, and the module row of
//! the layer diagram are read as text and compared with `CRATES`,
//! `MAY_USE_PLATFORM` and the crates that implement `Module`.

use std::collections::BTreeSet;

use guards::{scan, ws, View};

use super::deps::Tier::{self, *};
use super::deps::{CRATES, MAY_USE_PLATFORM};

const DOC: &str = "docs/ARCHITECTURE.md";

fn doc() -> String {
    std::fs::read_to_string(ws().root().join(DOC)).unwrap_or_else(|e| panic!("{DOC}: {e}"))
}

fn ticked(text: &str) -> Vec<String> {
    text.split('`')
        .skip(1)
        .step_by(2)
        .map(str::to_string)
        .collect()
}

fn set(names: impl IntoIterator<Item = String>) -> BTreeSet<String> {
    names.into_iter().collect()
}

fn is_library(name: &str, tier: Tier) -> bool {
    tier != TestSupport && !ws().package(name).dir.starts_with("examples/")
}

fn library_crates() -> BTreeSet<String> {
    set(CRATES
        .iter()
        .filter(|(n, t)| is_library(n, *t))
        .map(|(n, _)| n.to_string()))
}

fn same(what: &str, in_doc: &BTreeSet<String>, in_code: &BTreeSet<String>, code_is: &str) {
    let only_doc: Vec<&String> = in_doc.difference(in_code).collect();
    let only_code: Vec<&String> = in_code.difference(in_doc).collect();
    assert!(
        only_doc.is_empty() && only_code.is_empty(),
        "{DOC}: {what} disagrees with {code_is}\n  in the doc only: {only_doc:?}\n  in the code only: {only_code:?}"
    );
}

#[test]
fn the_crate_table_lists_exactly_the_library_crates() {
    let text = doc();
    let mut rows = text
        .lines()
        .skip_while(|l| !l.starts_with("| crate | 层 | 职责 |"))
        .skip(2)
        .take_while(|l| l.starts_with('|'));
    let mut names = BTreeSet::new();
    let mut count = 0;
    for row in rows.by_ref() {
        count += 1;
        let cell = row.split('|').nth(1).unwrap_or_default();
        match cell.split_once("crate 名") {
            Some((_, after)) => names.extend(ticked(after)),
            None => names.extend(ticked(cell)),
        }
    }
    guards::allow::non_vacuous("architecture_doc::crate table rows", count, 10);
    same(
        "the §1 crate table",
        &names,
        &library_crates(),
        "`CRATES` in tests/guards/tests/deps.rs (library crates, no test support or examples)",
    );
}

fn between<'a>(text: &'a str, from: &str, to: &str) -> &'a str {
    let start = text
        .find(from)
        .unwrap_or_else(|| panic!("{DOC}: cannot find `{from}`"))
        + from.len();
    let len = text[start..]
        .find(to)
        .unwrap_or_else(|| panic!("{DOC}: cannot find `{to}` after `{from}`"));
    &text[start..start + len]
}

#[test]
fn the_platform_paragraph_names_the_crates_that_may_use_platform() {
    let text = doc();
    let users = set(ticked(between(
        &text,
        "用它的是真要碰本机的 crate——",
        "清单与",
    )));
    let in_code = set(MAY_USE_PLATFORM
        .iter()
        .filter(|(n, _, _)| tier_of(n) != TestSupport)
        .map(|(n, _, _)| n.to_string()));
    same(
        "the crates said to use `platform`",
        &users,
        &in_code,
        "`MAY_USE_PLATFORM` in tests/guards/tests/deps.rs",
    );

    let contract_only = set(ticked(between(&text, "`MAY_USE_PLATFORM`;", "只经契约")));
    let expected: BTreeSet<String> = library_crates()
        .into_iter()
        .filter(|n| n != "base" && n != "platform" && !in_code.contains(n))
        .collect();
    same(
        "the crates said to reach the machine only through contracts",
        &contract_only,
        &expected,
        "the library crates outside `MAY_USE_PLATFORM`",
    );
}

fn tier_of(name: &str) -> Tier {
    CRATES
        .iter()
        .find(|(n, _)| *n == name)
        .map(|(_, t)| *t)
        .unwrap_or_else(|| panic!("`{name}` has no tier in CRATES"))
}

#[test]
fn the_module_row_of_the_layer_diagram_is_the_module_tier() {
    let text = doc();
    let row = text
        .lines()
        .find(|l| l.starts_with(" 模块 "))
        .unwrap_or_else(|| panic!("{DOC}: the layer diagram has no ` 模块 ` row"));
    let named = set(row.trim_start_matches(" 模块").split('·').flat_map(|part| {
        let part = part.trim();
        match part.split_once("(+") {
            Some((a, b)) => vec![
                a.trim().to_string(),
                b.trim_end_matches(')').trim().to_string(),
            ],
            None => vec![part.to_string()],
        }
    }));
    let tier = set(CRATES
        .iter()
        .filter(|(n, t)| *t == Module && is_library(n, *t))
        .map(|(n, _)| n.to_string()));
    same(
        "the diagram's module row",
        &named,
        &tier,
        "the `Module` tier of `CRATES`",
    );
}

#[test]
fn every_crate_implementing_module_is_in_the_module_tier() {
    let mut implementors = BTreeSet::new();
    for f in ws().production() {
        for i in scan::impls(f, View::Prod) {
            if i.trait_path
                .as_ref()
                .and_then(|p| p.last())
                .map(String::as_str)
                == Some("Module")
            {
                implementors.insert(f.package.clone());
            }
        }
    }
    guards::allow::non_vacuous(
        "architecture_doc::Module implementors",
        implementors.len(),
        3,
    );
    let outside: Vec<&String> = implementors
        .iter()
        .filter(|p| tier_of(p) != Module)
        .collect();
    assert!(
        outside.is_empty(),
        "{outside:?} implement `runtime::module::Module` but are not in the `Module` tier of `CRATES`; \
         modules are the crates the assembly lists in modules.rs"
    );
}
