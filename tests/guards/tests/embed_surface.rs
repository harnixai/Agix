//! The embed entry (`assembly::embed`, re-exported as `agix::embed` and
//! `daemon::embed`) is the one Rust surface 0.x promises to keep: its config
//! only gains fields, its error only gains variants, and its methods keep their
//! signatures, wherever in the crate an `impl Embedded` block is. The types keep
//! their derives and trait impls, and no variant takes on an attribute that
//! would change how it is matched. These lists are that promise written down; a
//! change that removes or alters an entry breaks embedders, and an addition
//! goes on the list together with a line in docs/agix-sdk.md's migration notes.
//!
//! It also takes its paths from its config alone: no process environment.

use guards::scan;
use guards::{ws, TargetKind, View};
use quote::ToTokens;

const EMBED: &str = "crates/assembly/src/embed.rs";

const CONFIG_FIELDS: &[(&str, &str)] = &[
    ("data_root", "PathBuf"),
    ("cwd", "Option<PathBuf>"),
    ("scene", "String"),
    ("scenes", "Vec<String>"),
    ("model", "String"),
    ("max_tokens", "u32"),
    ("session_cap", "usize"),
    ("session_idle_timeout", "Duration"),
    ("permission_prompt_timeout", "Option<Duration>"),
    ("instance", "Option<String>"),
    ("socket", "Option<PathBuf>"),
    ("mcp_policy", "Option<PathBuf>"),
    ("managed_settings", "Option<PathBuf>"),
    ("groups_file", "Option<PathBuf>"),
    ("project_dir", "Option<String>"),
    ("product", "String"),
    ("engine", "String"),
    ("plugins", "Vec<PathBuf>"),
    ("js_libs", "Vec<HostJsLib>"),
    ("history", "Option<HistoryLocation>"),
];

const ERROR_VARIANTS: &[(&str, &str)] = &[
    ("UnknownScene", "{scene:String,known:Vec<String>}"),
    ("AlreadyRunning", "{pid:u32,socket:PathBuf}"),
    ("Startup", "(String)"),
    ("InstanceInProcess", "{running:String}"),
    ("GroupsInProcess", "{running:PathBuf}"),
    ("GroupsHeld", "{owner:u32,file:PathBuf}"),
    ("InvalidProjectDir", "{name:String,reason:String}"),
    ("InvalidProductName", "{name:String,reason:String}"),
    ("InvalidEngineName", "{name:String,reason:String}"),
    ("InvalidHostLibrary", "{namespace:String,reason:String}"),
];

const DERIVES: &[(&str, &[&str])] = &[
    ("EmbedConfig", &["Clone", "Debug"]),
    ("EmbedError", &["Debug", "Error"]),
];

/// Hand-written trait impls of the three types, as `Trait for Type`.
const TRAIT_IMPLS: &[&str] = &[];

const TYPES: &[&str] = &["EmbedConfig", "EmbedError", "Embedded"];

const METHODS: &[(&str, &str)] = &[
    (
        "start",
        "pubasyncfnstart(config:EmbedConfig)->Result<Embedded,EmbedError>",
    ),
    (
        "serve_stream",
        "pubfnserve_stream<S>(&self,stream:S)->JoinHandle<()>whereS:AsyncRead+AsyncWrite+Send+'static,",
    ),
    (
        "serve_unix",
        "pubasyncfnserve_unix(&self)->std::io::Result<()>",
    ),
    ("closed", "pubasyncfnclosed(&self)"),
    ("shutdown", "pubasyncfnshutdown(&self)"),
    ("pool", "pubfnpool(&self)->&Arc<SessionPool>"),
];

fn squash(tokens: impl ToTokens) -> String {
    tokens
        .to_token_stream()
        .to_string()
        .chars()
        .filter(|c| !c.is_whitespace())
        .collect()
}

fn non_exhaustive(attrs: &[syn::Attribute]) -> bool {
    attrs.iter().any(|a| a.path().is_ident("non_exhaustive"))
}

fn tree() -> std::rc::Rc<syn::File> {
    ws().file(EMBED).tree(View::Prod)
}

/// Every item of the assembly library, inline modules' included.
fn crate_items() -> Vec<syn::Item> {
    fn walk(items: &[syn::Item], out: &mut Vec<syn::Item>) {
        for item in items {
            if let syn::Item::Mod(m) = item {
                if let Some((_, inner)) = &m.content {
                    walk(inner, out);
                }
            }
            out.push(item.clone());
        }
    }
    let mut out = Vec::new();
    for f in ws()
        .production()
        .filter(|f| f.package == "assembly" && f.target == TargetKind::Lib)
    {
        walk(&f.tree(View::Prod).items, &mut out);
    }
    out
}

fn derives(attrs: &[syn::Attribute]) -> Vec<String> {
    attrs
        .iter()
        .filter(|a| a.path().is_ident("derive"))
        .flat_map(|a| {
            a.parse_args_with(
                syn::punctuated::Punctuated::<syn::Path, syn::Token![,]>::parse_terminated,
            )
            .expect("a derive lists paths")
        })
        .map(|p| p.segments.last().unwrap().ident.to_string())
        .collect()
}

/// `expected` must all be in `found`, unchanged; anything else in `found`
/// is an addition not yet written down.
fn compare(what: &str, expected: &[(&str, &str)], found: &[(String, String)]) -> Vec<String> {
    let mut problems = Vec::new();
    for (name, shape) in expected {
        match found.iter().find(|(n, _)| n == name) {
            None => problems.push(format!(
                "{what} `{name}` is gone: that breaks every embedder using it"
            )),
            Some((_, actual)) if actual != shape => problems.push(format!(
                "{what} `{name}` changed from `{shape}` to `{actual}`: that breaks embedders"
            )),
            Some(_) => {}
        }
    }
    for (name, shape) in found {
        if !expected.iter().any(|(n, _)| n == name) {
            problems.push(format!(
                "{what} `{name}` (`{shape}`) is new: add it to the list in \
                 tests/guards/tests/embed_surface.rs and to the migration notes in \
                 docs/agix-sdk.md"
            ));
        }
    }
    problems
}

#[test]
fn the_config_only_gains_fields_and_cannot_be_built_by_a_literal() {
    let tree = tree();
    let config = tree
        .items
        .iter()
        .find_map(|item| match item {
            syn::Item::Struct(s) if s.ident == "EmbedConfig" => Some(s),
            _ => None,
        })
        .expect("EmbedConfig in crates/assembly/src/embed.rs");
    assert!(
        non_exhaustive(&config.attrs),
        "EmbedConfig must stay #[non_exhaustive]: a field added to it would break every \
         struct literal"
    );
    let fields: Vec<(String, String)> = config
        .fields
        .iter()
        .filter(|f| matches!(f.vis, syn::Visibility::Public(_)))
        .map(|f| (f.ident.as_ref().unwrap().to_string(), squash(&f.ty)))
        .collect();
    let problems = compare("field", CONFIG_FIELDS, &fields);
    assert!(problems.is_empty(), "{}", problems.join("\n"));

    let constructor = tree.items.iter().any(|item| match item {
        syn::Item::Impl(i) if i.trait_.is_none() && squash(&i.self_ty) == "EmbedConfig" => {
            i.items.iter().any(|item| {
                matches!(item, syn::ImplItem::Fn(f)
                    if f.sig.ident == "new" && squash(&f.sig) == "fnnew(data_root:implInto<PathBuf>)->Self")
            })
        }
        _ => false,
    });
    assert!(
        constructor,
        "EmbedConfig::new(data_root: impl Into<PathBuf>) -> Self is how embedders build one"
    );
}

#[test]
fn the_error_only_gains_variants() {
    let tree = tree();
    let error = tree
        .items
        .iter()
        .find_map(|item| match item {
            syn::Item::Enum(e) if e.ident == "EmbedError" => Some(e),
            _ => None,
        })
        .expect("EmbedError in crates/assembly/src/embed.rs");
    assert!(
        non_exhaustive(&error.attrs),
        "EmbedError must stay #[non_exhaustive]"
    );
    let variants: Vec<(String, String)> = error
        .variants
        .iter()
        .map(|v| (v.ident.to_string(), squash(&v.fields)))
        .collect();
    let problems = compare("variant", ERROR_VARIANTS, &variants);
    assert!(problems.is_empty(), "{}", problems.join("\n"));
}

#[test]
fn the_engine_s_methods_keep_their_signatures() {
    let items = crate_items();
    let methods: Vec<(String, String)> = items
        .iter()
        .filter_map(|item| match item {
            syn::Item::Impl(i) if i.trait_.is_none() && squash(&i.self_ty) == "Embedded" => Some(i),
            _ => None,
        })
        .flat_map(|i| i.items.iter())
        .filter_map(|item| match item {
            syn::ImplItem::Fn(f) if matches!(f.vis, syn::Visibility::Public(_)) => {
                Some((f.sig.ident.to_string(), format!("pub{}", squash(&f.sig))))
            }
            _ => None,
        })
        .collect();
    let problems = compare("method", METHODS, &methods);
    assert!(problems.is_empty(), "{}", problems.join("\n"));
}

#[test]
fn the_entry_reads_no_process_environment() {
    let f = ws().file(EMBED);
    let reads: Vec<String> = scan::paths(f, View::Prod)
        .into_iter()
        .filter(|(path, _)| {
            let path: Vec<&str> = path
                .iter()
                .map(String::as_str)
                .filter(|s| *s != "::")
                .collect();
            path.windows(2)
                .any(|w| w[0] == "env" && w[1].starts_with("var"))
                || path.last() == Some(&"home_dir")
        })
        .map(|(_, hit)| hit.to_string())
        .collect();
    assert!(
        reads.is_empty(),
        "the embed entry takes its paths from EmbedConfig, not from this process's \
         environment (agixd's main reads AGIX_CONFIG_HOME through \
         config::default_global_root):\n{}",
        reads.join("\n")
    );
}

#[test]
fn the_types_keep_their_derives() {
    let tree = tree();
    let mut problems = Vec::new();
    for (ty, wanted) in DERIVES {
        let attrs = tree
            .items
            .iter()
            .find_map(|item| match item {
                syn::Item::Struct(s) if s.ident == ty => Some(&s.attrs),
                syn::Item::Enum(e) if e.ident == ty => Some(&e.attrs),
                _ => None,
            })
            .unwrap_or_else(|| panic!("{ty} in {EMBED}"));
        let found = derives(attrs);
        for derive in *wanted {
            if !found.iter().any(|f| f == derive) {
                problems.push(format!(
                    "{ty} no longer derives {derive}: that breaks every embedder using it"
                ));
            }
        }
    }
    assert!(problems.is_empty(), "{}", problems.join("\n"));
}

#[test]
fn no_variant_takes_on_an_attribute_and_each_says_what_it_is() {
    let tree = tree();
    let error = tree
        .items
        .iter()
        .find_map(|item| match item {
            syn::Item::Enum(e) if e.ident == "EmbedError" => Some(e),
            _ => None,
        })
        .expect("EmbedError in crates/assembly/src/embed.rs");
    let mut problems = Vec::new();
    for v in &error.variants {
        if !v.attrs.iter().any(|a| a.path().is_ident("error")) {
            problems.push(format!("variant `{}` has no #[error(..)] message", v.ident));
        }
        for a in &v.attrs {
            if !a.path().is_ident("error") && !a.path().is_ident("doc") {
                problems.push(format!(
                    "variant `{}` carries `{}`: an attribute such as #[non_exhaustive] \
                     breaks the embedders that match on it",
                    v.ident,
                    squash(a)
                ));
            }
        }
    }
    assert!(problems.is_empty(), "{}", problems.join("\n"));
}

#[test]
fn the_types_keep_their_trait_impls() {
    let found: Vec<(String, String)> = crate_items()
        .iter()
        .filter_map(|item| match item {
            syn::Item::Impl(i) => {
                let (_, path, _) = i.trait_.as_ref()?;
                let ty = squash(&i.self_ty);
                TYPES
                    .contains(&ty.as_str())
                    .then(|| (format!("{} for {ty}", squash(path)), String::new()))
            }
            _ => None,
        })
        .collect();
    let expected: Vec<(&str, &str)> = TRAIT_IMPLS.iter().map(|t| (*t, "")).collect();
    let problems = compare("trait impl", &expected, &found);
    assert!(problems.is_empty(), "{}", problems.join("\n"));
}
