//! The `agix` global every embedded JavaScript runtime gets (docs/agix_js.md):
//! pure functions for YAML, glob, paths, JSON Schema, SHA-256, UUIDs and
//! argument parsing, matching what the engine itself does with the same
//! formats. `install` puts it, deeply frozen, into one context; the script
//! carrier and `RunCode` / `RunExt` call it before any user code runs. A
//! product an embedder renames also finds it under the global
//! [`alias_for`] its name gives.

mod json_schema;
mod value;
mod yaml;

use std::sync::OnceLock;

use rquickjs::module::{Module, WriteOptions};
use rquickjs::{Ctx, Exception, Function, Object, TypedArray};
use sha2::{Digest, Sha256};

use crate::value::Value;

const SOURCE: &str = include_str!("agix.js");
const MODULE_NAME: &str = "js-lib";

/// `x.y.z` of the engine; the workspace version carries a release suffix.
pub fn version() -> &'static str {
    let full = env!("CARGO_PKG_VERSION");
    full.split_once('-').map_or(full, |(v, _)| v)
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Failure {
    pub code: &'static str,
    pub message: String,
    pub position: Option<(usize, usize)>,
}

impl Failure {
    pub(crate) fn new(code: &'static str, message: impl Into<String>) -> Failure {
        Failure {
            code,
            message: message.into(),
            position: None,
        }
    }

    pub(crate) fn at(
        code: &'static str,
        message: impl Into<String>,
        line: usize,
        column: usize,
    ) -> Failure {
        Failure {
            position: Some((line, column)),
            ..Failure::new(code, message)
        }
    }

    fn throw(self, ctx: &Ctx<'_>) -> rquickjs::Error {
        let thrown = (|| {
            let o = Object::new(ctx.clone())?;
            o.set("__agix", true)?;
            o.set("code", self.code)?;
            o.set("message", self.message.as_str())?;
            if let Some((line, column)) = self.position {
                o.set("line", line)?;
                o.set("column", column)?;
            }
            Ok::<_, rquickjs::Error>(o)
        })();
        match thrown {
            Ok(o) => ctx.throw(o.into_value()),
            Err(e) => e,
        }
    }
}

/// Bytecode of `agix.js`, compiled the first time any runtime asks.
/// `Module::load` reads it as ROM data, which must outlive every runtime that
/// loaded it: hence a process-wide static and not a per-runtime copy.
fn bytecode() -> Result<&'static [u8], String> {
    static COMPILED: OnceLock<Result<Vec<u8>, String>> = OnceLock::new();
    COMPILED
        .get_or_init(|| {
            let runtime = rquickjs::Runtime::new().map_err(|e| e.to_string())?;
            let context = rquickjs::Context::full(&runtime).map_err(|e| e.to_string())?;
            context.with(|ctx| {
                Module::declare(ctx.clone(), MODULE_NAME, SOURCE)
                    .and_then(|m| m.write(WriteOptions::default()))
                    .map_err(|e| match e {
                        rquickjs::Error::Exception => format!("{:?}", ctx.catch()),
                        other => other.to_string(),
                    })
            })
        })
        .as_ref()
        .map(Vec::as_slice)
        .map_err(Clone::clone)
}

/// The global every carrier installs.
pub const GLOBAL: &str = "agix";

const RESERVED_WORDS: &[&str] = &[
    "arguments",
    "async",
    "await",
    "break",
    "case",
    "catch",
    "class",
    "const",
    "continue",
    "debugger",
    "default",
    "delete",
    "do",
    "else",
    "enum",
    "eval",
    "export",
    "extends",
    "false",
    "finally",
    "for",
    "function",
    "if",
    "implements",
    "import",
    "in",
    "instanceof",
    "interface",
    "let",
    "new",
    "null",
    "package",
    "private",
    "protected",
    "public",
    "return",
    "static",
    "super",
    "switch",
    "this",
    "throw",
    "true",
    "try",
    "typeof",
    "undefined",
    "var",
    "void",
    "while",
    "with",
    "yield",
];

/// What the carriers put on `globalThis` themselves, `RunCode`'s and
/// `RunExt`'s stand-ins for missing Node APIs included.
const CARRIER_GLOBALS: &[&str] = &[
    "agent",
    "args",
    "callTool",
    "console",
    "exports",
    "fetch",
    "fs",
    "log",
    "messages",
    "module",
    "notify",
    "process",
    "require",
    "stopRequested",
    "tools",
];

/// Every name a fresh context already resolves on `globalThis`, its
/// prototype chain included.
fn standard_globals() -> &'static [String] {
    static NAMES: OnceLock<Vec<String>> = OnceLock::new();
    NAMES.get_or_init(|| {
        let listed = (|| {
            let runtime = rquickjs::Runtime::new().ok()?;
            let context = rquickjs::Context::full(&runtime).ok()?;
            context.with(|ctx| {
                ctx.eval::<Vec<String>, _>(
                    "const names = [];                      for (let o = globalThis; o; o = Object.getPrototypeOf(o))                        names.push(...Object.getOwnPropertyNames(o));                      names",
                )
                .ok()
            })
        })();
        listed.unwrap_or_default()
    })
}

/// Why `name` cannot be a second name of the library.
fn refuse_alias(name: &str) -> Option<&'static str> {
    if !name.starts_with(|c: char| c.is_ascii_lowercase()) {
        return Some("it must start with a lowercase ASCII letter");
    }
    if !name
        .chars()
        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit())
    {
        return Some("only lowercase ASCII letters and digits are allowed");
    }
    if name == GLOBAL {
        return Some("it is the library's own name");
    }
    if RESERVED_WORDS.contains(&name) {
        return Some("it is a JavaScript reserved word");
    }
    if CARRIER_GLOBALS.contains(&name) || standard_globals().iter().any(|g| g == name) {
        return Some("a script already has a global of that name");
    }
    None
}

/// The second global a product called `product` finds the library under:
/// its name's ASCII letters and digits, lowercase. `None` when that is
/// `agix` itself or not a name a script could be given.
pub fn alias_for(product: &str) -> Option<String> {
    let name: String = product
        .chars()
        .filter(char::is_ascii_alphanumeric)
        .map(|c| c.to_ascii_lowercase())
        .collect();
    refuse_alias(&name).is_none().then_some(name)
}

pub fn install(ctx: &Ctx<'_>) -> rquickjs::Result<()> {
    install_as(ctx, &[])
}

/// [`install`], with the same frozen object also bound to each of `aliases`.
pub fn install_as(ctx: &Ctx<'_>, aliases: &[&str]) -> rquickjs::Result<()> {
    if let Some((name, why)) = aliases
        .iter()
        .find_map(|name| refuse_alias(name).map(|why| (name, why)))
    {
        return Err(Exception::throw_type(
            ctx,
            &format!("`{name}` cannot name the script base library: {why}"),
        ));
    }
    let code = bytecode().map_err(|e| {
        Exception::throw_internal(
            ctx,
            &format!("the script base library could not be compiled: {e}"),
        )
    })?;
    // SAFETY: the bytes are what `Module::write` produced from `SOURCE` in
    // this process, with this QuickJS build, and they live for the process.
    let module = unsafe { Module::load(ctx.clone(), code)? };
    let (module, evaluated) = module.eval()?;
    match evaluated.result::<()>() {
        Some(r) => r?,
        None => {
            return Err(Exception::throw_internal(
                ctx,
                "the script base library did not finish loading",
            ))
        }
    }
    let build: Function = module.namespace()?.get("default")?;
    build.call::<_, ()>((natives(ctx)?, version(), aliases.to_vec()))
}

fn natives<'js>(ctx: &Ctx<'js>) -> rquickjs::Result<Object<'js>> {
    let n = Object::new(ctx.clone())?;
    n.set("yamlParse", Function::new(ctx.clone(), yaml_parse)?)?;
    n.set("yamlStringify", Function::new(ctx.clone(), yaml_stringify)?)?;
    n.set("globTest", Function::new(ctx.clone(), glob_test)?)?;
    n.set("globMatches", Function::new(ctx.clone(), glob_matches)?)?;
    n.set("jsonValidate", Function::new(ctx.clone(), json_validate)?)?;
    n.set("sha256Text", Function::new(ctx.clone(), sha256_text)?)?;
    n.set("sha256Bytes", Function::new(ctx.clone(), sha256_bytes)?)?;
    n.set(
        "uuidV4",
        Function::new(ctx.clone(), || uuid::Uuid::new_v4().to_string())?,
    )?;
    n.set(
        "uuidV7",
        Function::new(ctx.clone(), || uuid::Uuid::now_v7().to_string())?,
    )?;
    Ok(n)
}

fn yaml_parse(ctx: Ctx<'_>, text: String) -> rquickjs::Result<String> {
    yaml::parse(&text)
        .map(|v| v.to_json())
        .map_err(|f| f.throw(&ctx))
}

fn yaml_stringify(ctx: Ctx<'_>, json: String) -> rquickjs::Result<String> {
    let value = Value::from_json(&json).map_err(|e| {
        let failure = if e.to_string().contains("recursion limit") {
            Failure::new(
                "yaml.depth",
                format!("nested deeper than {} levels", value::MAX_DEPTH),
            )
        } else {
            Failure::new("yaml.unrepresentable", e.to_string())
        };
        failure.throw(&ctx)
    })?;
    yaml::stringify(&value).map_err(|f| f.throw(&ctx))
}

/// The longest glob pattern, and the most pattern text one call compiles.
const MAX_PATTERN_CHARS: usize = 4_096;
const MAX_PATTERNS_CHARS: usize = 16_384;

fn globs(patterns: &[String]) -> Result<Vec<globset::GlobMatcher>, Failure> {
    let lengths = patterns.iter().map(|p| p.chars().count());
    if lengths.clone().any(|n| n > MAX_PATTERN_CHARS) {
        return Err(Failure::new(
            "glob.pattern",
            format!("a pattern is longer than {MAX_PATTERN_CHARS} characters"),
        ));
    }
    if lengths.sum::<usize>() > MAX_PATTERNS_CHARS {
        return Err(Failure::new(
            "glob.pattern",
            format!("the patterns are longer than {MAX_PATTERNS_CHARS} characters together"),
        ));
    }
    patterns
        .iter()
        .map(|p| {
            globset::GlobBuilder::new(p)
                .literal_separator(true)
                .build()
                .map(|g| g.compile_matcher())
                .map_err(|e| Failure::new("glob.pattern", format!("invalid glob pattern: {e}")))
        })
        .collect()
}

fn glob_test(ctx: Ctx<'_>, patterns: Vec<String>, path: String) -> rquickjs::Result<bool> {
    let matchers = globs(&patterns).map_err(|f| f.throw(&ctx))?;
    Ok(matchers.iter().any(|m| m.is_match(&path)))
}

fn glob_matches(
    ctx: Ctx<'_>,
    patterns: Vec<String>,
    paths: Vec<String>,
) -> rquickjs::Result<Vec<bool>> {
    let matchers = globs(&patterns).map_err(|f| f.throw(&ctx))?;
    Ok(paths
        .iter()
        .map(|p| matchers.iter().any(|m| m.is_match(p)))
        .collect())
}

fn json_validate(ctx: Ctx<'_>, schema: String, value: String) -> rquickjs::Result<String> {
    let read = |what: &str, text: &str| {
        serde_json::from_str::<serde_json::Value>(text).map_err(|e| {
            let failure = if e.to_string().contains("recursion limit") {
                Failure::new(
                    "json.depth",
                    format!(
                        "the {what} is nested deeper than {} levels",
                        value::MAX_DEPTH
                    ),
                )
            } else {
                Failure::new("json.schema", format!("the {what} is not JSON: {e}"))
            };
            failure.throw(&ctx)
        })
    };
    let schema = read("schema", &schema)?;
    let value = read("value", &value)?;
    json_schema::validate(&schema, &value)
        .map(|v| v.to_string())
        .map_err(|f| f.throw(&ctx))
}

fn sha256_text(text: String) -> String {
    hex::encode(Sha256::digest(text.as_bytes()))
}

fn sha256_bytes(ctx: Ctx<'_>, bytes: TypedArray<'_, u8>) -> rquickjs::Result<String> {
    let bytes = bytes
        .as_bytes()
        .ok_or_else(|| Exception::throw_type(&ctx, "the Uint8Array's buffer is detached"))?;
    Ok(hex::encode(Sha256::digest(bytes)))
}
