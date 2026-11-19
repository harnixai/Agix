//! The facade crate `agix` (docs/agix-sdk.md) holds no code of its own. The
//! shape of the interface is decided by the crates it re-exports from; what is
//! guarded here is that the facade neither grows logic nor turns into a
//! forwarder for whole crates, and that its size changes only on purpose.

use std::collections::BTreeMap;

use guards::allow;
use guards::{ws, Hit, SourceFile, View};
use syn::{Attribute, Item, UseTree, Visibility};

const FACADE_EXPORTS: usize = 177;
const PRELUDE_ITEMS: usize = 11;

fn facade_files() -> Vec<&'static SourceFile> {
    ws().files().filter(|f| f.package == "agix").collect()
}

fn is_prelude(f: &SourceFile) -> bool {
    f.rel.ends_with("/prelude.rs")
}

fn attribute_problem(attrs: &[Attribute]) -> Option<String> {
    attrs
        .iter()
        .find(|a| !a.path().is_ident("doc") && !a.path().is_ident("cfg"))
        .map(|a| {
            format!(
                "attribute `{}`",
                a.path()
                    .get_ident()
                    .map(|i| i.to_string())
                    .unwrap_or_default()
            )
        })
}

fn item_attrs(item: &Item) -> &[Attribute] {
    match item {
        Item::Use(i) => &i.attrs,
        Item::Mod(i) => &i.attrs,
        _ => &[],
    }
}

fn walk_items(f: &SourceFile, items: &[Item], count: &mut usize, bad: &mut Vec<Hit>) {
    use syn::spanned::Spanned;
    for item in items {
        *count += 1;
        match item {
            Item::Use(u) if matches!(u.vis, Visibility::Public(_)) => {}
            Item::Mod(m) if matches!(m.vis, Visibility::Public(_)) => {
                if let Some((_, inner)) = &m.content {
                    walk_items(f, inner, count, bad);
                }
                if let Some(p) = attribute_problem(&m.attrs) {
                    bad.push(Hit {
                        snippet: format!("{} ({p})", f.hit(m.span()).snippet),
                        ..f.hit(m.span())
                    });
                }
                continue;
            }
            other => {
                bad.push(f.hit(other.span()));
                continue;
            }
        }
        if attribute_problem(item_attrs(item)).is_some() {
            bad.push(f.hit(item.span()));
        }
    }
}

#[test]
fn the_facade_holds_no_logic() {
    let mut count = 0;
    let mut bad = Vec::new();
    for f in facade_files() {
        let tree = f.tree(View::Full);
        if let Some(p) = attribute_problem(&tree.attrs) {
            bad.push(Hit {
                snippet: p,
                ..f.hit(proc_macro2::Span::call_site())
            });
        }
        walk_items(f, &tree.items, &mut count, &mut bad);
    }
    allow::non_vacuous("facade::items", count, 100);
    assert!(
        bad.is_empty(),
        "the facade may hold only `pub use`, `pub mod` and docs (cfg is the only attribute); \
         logic belongs in the crate the item comes from:\n{}",
        bad.iter()
            .map(|h| h.to_string())
            .collect::<Vec<_>>()
            .join("\n")
    );
}

struct Leaf {
    path: String,
    segments: usize,
    hit: Hit,
}

fn leaves(f: &SourceFile, tree: &UseTree, prefix: &mut Vec<String>, out: &mut Vec<Leaf>) {
    use syn::spanned::Spanned;
    let hit = || f.hit(tree.span());
    match tree {
        UseTree::Path(p) => {
            prefix.push(p.ident.to_string());
            leaves(f, &p.tree, prefix, out);
            prefix.pop();
        }
        UseTree::Name(n) => {
            let mut path = prefix.clone();
            path.push(n.ident.to_string());
            out.push(Leaf {
                segments: path.len(),
                path: path.join("::"),
                hit: hit(),
            });
        }
        UseTree::Rename(r) => {
            let mut path = prefix.clone();
            path.push(r.ident.to_string());
            out.push(Leaf {
                segments: path.len(),
                path: path.join("::"),
                hit: hit(),
            });
        }
        UseTree::Glob(_) => out.push(Leaf {
            path: format!("{}::*", prefix.join("::")),
            segments: 0,
            hit: hit(),
        }),
        UseTree::Group(g) => {
            for t in &g.items {
                leaves(f, t, prefix, out);
            }
        }
    }
}

fn collect(f: &SourceFile, items: &[Item], out: &mut Vec<Leaf>) {
    for item in items {
        match item {
            Item::Use(u) => leaves(f, &u.tree, &mut Vec::new(), out),
            Item::Mod(m) => {
                if let Some((_, inner)) = &m.content {
                    collect(f, inner, out);
                }
            }
            _ => {}
        }
    }
}

fn facade_leaves(prelude: bool) -> Vec<Leaf> {
    let mut out = Vec::new();
    for f in facade_files()
        .into_iter()
        .filter(|f| is_prelude(f) == prelude)
    {
        collect(f, &f.tree(View::Full).items, &mut out);
    }
    out
}

#[test]
fn the_facade_re_exports_items_not_crates() {
    let bad: Vec<String> = facade_leaves(false)
        .into_iter()
        .chain(facade_leaves(true))
        .filter(|l| l.segments < 2 || l.path.ends_with("::self") || l.path.contains('*'))
        .map(|l| format!("{}\n      `{}`", l.hit, l.path))
        .collect();
    assert!(
        bad.is_empty(),
        "re-export items by their full path: no glob, no `self`, no whole crate \
         (`pub use ::base as agent` passes every reshuffle of the crate on to the embedder):\n{}",
        bad.join("\n")
    );
}

#[test]
fn every_item_is_exported_once() {
    let mut seen: BTreeMap<String, Vec<Hit>> = BTreeMap::new();
    for l in facade_leaves(false) {
        seen.entry(l.path).or_default().push(l.hit);
    }
    let twice: Vec<String> = seen
        .iter()
        .filter(|(_, hits)| hits.len() > 1)
        .map(|(p, hits)| {
            format!(
                "`{p}` at {}",
                hits.iter()
                    .map(|h| h.to_string())
                    .collect::<Vec<_>>()
                    .join(", ")
            )
        })
        .collect();
    assert!(twice.is_empty(), "exported twice: {}", twice.join("\n"));
}

#[test]
fn the_prelude_only_names_the_theme_modules() {
    let leaves = facade_leaves(true);
    let outside: Vec<String> = leaves
        .iter()
        .filter(|l| !l.path.starts_with("crate::"))
        .map(|l| format!("{}\n      `{}`", l.hit, l.path))
        .collect();
    assert!(
        outside.is_empty(),
        "the prelude re-exports from the theme modules (`crate::…`), not from other crates:\n{}",
        outside.join("\n")
    );
    assert_eq!(
        leaves.len(),
        PRELUDE_ITEMS,
        "the prelude is the minimum for a pool, a session and a turn; change PRELUDE_ITEMS with it \
         and say in the commit which snippet of docs/agix-sdk.md needs the addition"
    );
}

#[test]
fn a_history_store_wrapper_needs_nothing_outside_the_facade() {
    let exported: Vec<String> = facade_leaves(false).into_iter().map(|l| l.path).collect();
    let missing: Vec<&str> = [
        "history::error::HistoryError",
        "history::placement::ProjectHistory",
        "history::placement::ProjectHistoryDir",
        "history::placement::SessionIndex",
        "history::placement::SidecarLocator",
    ]
    .into_iter()
    .filter(|path| !exported.iter().any(|e| e == path))
    .collect();
    assert!(
        missing.is_empty(),
        "`HistoryStore` methods a wrapper forwards, and `with_project_history`, name these: {missing:?}"
    );
}

#[test]
fn the_facade_is_the_size_it_says() {
    let n = facade_leaves(false).len();
    assert_eq!(
        n, FACADE_EXPORTS,
        "the facade exports {n} items; change FACADE_EXPORTS with the export, and say in the commit \
         which downstream project needs it (an issue) or which section of docs/agix-sdk.md names it"
    );
}
