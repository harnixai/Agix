//! DEC-1: a wrapper rewrites every default method of the trait it wraps.
//!
//! A type that holds an inner `dyn T` (or is generic over `C: T`) and
//! implements `T` itself stands in for the inner one. A method it leaves to
//! `T`'s default body answers for itself instead of asking the inner
//! implementation, so a customisation inside the wrapper is lost without any
//! error. The wrapper must name every defaulted method, even to forward it.

use std::collections::{BTreeMap, BTreeSet};

use guards::allow::{self, Finding};
use guards::scan;
use guards::{ws, View};

/// `<wrapper>:<trait>::<method>`, why the wrapper may leave it to the default.
const NOT_FORWARDED: &[(&str, &str)] = &[
    (
        "LayeredPromptRegistry:PromptRegistry::register_context",
        "sugar over `register_block`, which the layer routes to its top registry",
    ),
    (
        "LayeredToolRegistry:ToolRegistry::is_empty",
        "derived from `list`, which merges overlay and base; forwarding would skip the merge",
    ),
    (
        "LayeredToolRegistry:ToolRegistry::len",
        "derived from `list`, which merges overlay and base; forwarding would skip the merge",
    ),
    (
        "LayeredToolRegistry:ToolRegistry::names",
        "derived from `list`, which merges overlay and base; forwarding would skip the merge",
    ),
];

fn mentions_dyn(ty: &str, trait_name: &str) -> bool {
    let squeezed: String = ty.split_whitespace().collect::<Vec<_>>().join(" ");
    let needle = format!("dyn {trait_name}");
    squeezed.match_indices(&needle).any(|(at, _)| {
        !squeezed[at + needle.len()..]
            .chars()
            .next()
            .is_some_and(|c| c.is_alphanumeric() || c == '_')
    })
}

#[test]
fn wrappers_forward_every_default_method() {
    let mut defaulted: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    for f in ws().production() {
        for t in scan::traits(f, View::Prod) {
            if !t.defaulted.is_empty() {
                defaulted.entry(t.name).or_default().extend(t.defaulted);
            }
        }
    }

    let mut found = Vec::new();
    let mut wrappers = 0;
    for pkg in ws().packages() {
        let files: Vec<_> = ws()
            .production()
            .filter(|f| f.package == pkg.name)
            .collect();
        let structs: Vec<_> = files
            .iter()
            .flat_map(|f| scan::structs(f, View::Prod))
            .collect();
        for f in &files {
            for imp in scan::impls(f, View::Prod) {
                let Some(trait_name) = imp.trait_path.as_ref().and_then(|p| p.last()) else {
                    continue;
                };
                let Some(methods) = defaulted.get(trait_name) else {
                    continue;
                };
                let holds_inner = structs.iter().any(|s| {
                    s.name == imp.self_ty
                        && s.fields.iter().any(|fl| mentions_dyn(&fl.ty, trait_name))
                });
                let generic_over_it = imp.param_bounds.iter().any(|b| b == trait_name);
                if !holds_inner && !generic_over_it {
                    continue;
                }
                wrappers += 1;
                for m in methods.iter().filter(|m| !imp.items.contains(m)) {
                    found.push(Finding {
                        key: format!("{}:{trait_name}::{m}", imp.self_ty),
                        hit: imp.hit.clone(),
                    });
                }
            }
        }
    }
    allow::non_vacuous("decorators::wrappers", wrappers, 5);
    allow::check(
        "decorators::NOT_FORWARDED",
        found,
        NOT_FORWARDED,
        "forward the method to the inner implementation, or list it with the reason the \
         default is right for this wrapper",
    );
}
