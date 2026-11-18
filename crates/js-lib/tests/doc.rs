//! The members installed on `agix` are exactly the ones `docs/agix_js.md` §3
//! documents: a `### `agix.<name>`` heading per namespace, and the functions
//! written as `` `name(…)` `` in its section (or none, for a plain value).

use std::collections::BTreeSet;

fn documented() -> BTreeSet<String> {
    let path = concat!(env!("CARGO_MANIFEST_DIR"), "/../../docs/agix_js.md");
    let doc = std::fs::read_to_string(path).unwrap_or_else(|e| panic!("{path}: {e}"));
    let section = doc
        .split("\n## 3.")
        .nth(1)
        .and_then(|rest| rest.split("\n## 4.").next())
        .expect("docs/agix_js.md has a §3 followed by a §4");

    let mut members = BTreeSet::new();
    let mut fenced = false;
    let mut current: Option<(String, BTreeSet<String>)> = None;
    let mut close = |current: &mut Option<(String, BTreeSet<String>)>| {
        if let Some((ns, functions)) = current.take() {
            if functions.is_empty() {
                members.insert(ns);
            } else {
                members.extend(functions.into_iter().map(|f| format!("{ns}.{f}")));
            }
        }
    };
    for line in section.lines() {
        if line.trim_start().starts_with("```") {
            fenced = !fenced;
            continue;
        }
        if fenced {
            continue;
        }
        if let Some(heading) = line.strip_prefix("### ") {
            close(&mut current);
            let name = heading.trim().trim_matches('`');
            assert!(
                name.starts_with("agix."),
                "a §3 heading that is not a member: {line}"
            );
            current = Some((name.to_string(), BTreeSet::new()));
            continue;
        }
        let Some((_, functions)) = current.as_mut() else {
            continue;
        };
        for span in line.split('`').skip(1).step_by(2) {
            let name: String = span
                .chars()
                .take_while(|c| c.is_ascii_alphanumeric())
                .collect();
            let is_call = span[name.len()..].starts_with('(');
            if is_call && name.starts_with(|c: char| c.is_ascii_lowercase()) {
                functions.insert(name);
            }
        }
    }
    close(&mut current);
    members
}

fn installed() -> BTreeSet<String> {
    let runtime = rquickjs::Runtime::new().unwrap();
    let context = rquickjs::Context::full(&runtime).unwrap();
    context.with(|ctx| {
        js_lib::install(&ctx).unwrap();
        let listed: Vec<String> = ctx
            .eval(
                "Object.keys(agix).flatMap((ns) => typeof agix[ns] === 'object' \
                 ? Object.keys(agix[ns]).map((f) => `agix.${ns}.${f}`) : [`agix.${ns}`])",
            )
            .unwrap();
        listed.into_iter().collect()
    })
}

#[test]
fn the_installed_members_are_the_documented_ones() {
    let doc = documented();
    assert!(doc.len() >= 15, "read too little from the doc: {doc:?}");
    let code = installed();
    let only_doc: Vec<_> = doc.difference(&code).collect();
    let only_code: Vec<_> = code.difference(&doc).collect();
    assert!(
        only_doc.is_empty() && only_code.is_empty(),
        "docs/agix_js.md §3 and the installed `agix` disagree\n  documented only: {only_doc:?}\n  installed only: {only_code:?}"
    );
}
