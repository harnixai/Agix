//! The LIVE cases in `tests/blackbox/tests/live_*.rs` run with the real
//! model under the permission mode a user would use; none of them takes the
//! gate away just to get past it (G12). `bypassPermissions`, its aliases
//! `yolo` and `auto`, as a permission mode in a case is a failure unless a
//! `// bypass:` comment in the same function says why the case is about
//! bypass itself.

use guards::allow::{self, Finding};
use guards::scan;
use guards::{par_flat_map, ws, View};

const BYPASS_NAMES: &[&str] = &["bypassPermissions", "yolo", "auto"];

fn is_live_case_file(rel: &str) -> bool {
    rel.strip_prefix("tests/blackbox/tests/")
        .is_some_and(|name| name.starts_with("live_") && name.ends_with(".rs"))
}

#[test]
fn live_cases_do_not_bypass_permissions_without_saying_why() {
    let files: Vec<_> = ws().tests().filter(|f| is_live_case_file(&f.rel)).collect();
    allow::non_vacuous("blackbox_discipline", files.len(), 15);

    let found = par_flat_map(files.iter().copied(), |f| {
        let fns = scan::fns(f, View::Full);
        scan::string_literals(f, View::Full)
            .into_iter()
            .filter(|(value, hit)| {
                BYPASS_NAMES.contains(&value.as_str())
                    && (value != "auto" || hit.snippet.to_lowercase().contains("mode"))
            })
            .filter(|(_, hit)| {
                let Some(case) = fns
                    .iter()
                    .find(|c| (c.hit.line..c.hit.line + c.lines).contains(&hit.line))
                else {
                    return true;
                };
                !(case.hit.line..case.hit.line + case.lines)
                    .any(|n| f.line(n).trim_start().starts_with("// bypass:"))
            })
            .map(|(value, hit)| Finding {
                key: format!("{value} in {}", hit.file),
                hit,
            })
            .collect()
    });
    allow::check(
        "blackbox_discipline::BYPASS_NAMES",
        found,
        &[],
        "grant what the case needs with a permission rule, answer the prompts with a Watcher, or, \
         when the case is about bypass itself, put a `// bypass: <why>` line inside the function",
    );
}
