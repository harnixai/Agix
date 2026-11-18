//! `agix.yaml`: one YAML 1.2 document to a JSON value under the core schema,
//! and a JSON value back to block-style YAML. The parser is `saphyr-parser`'s
//! event stream; resolution, aliases and the limits on them are here.

use std::collections::{HashMap, HashSet};
use std::rc::Rc;

use saphyr_parser::{Event, Parser, ScalarStyle, ScanError, Span, Tag};

use crate::value::{Value, MAX_DEPTH};
use crate::Failure;

/// Nodes and scalar bytes an alias may bring in, summed over every alias in
/// the document, each counted as often as it is named.
const ALIAS_NODE_LIMIT: u64 = 100_000;
const ALIAS_BYTE_LIMIT: u64 = 8 * 1024 * 1024;

const MAX_SAFE: u128 = 1 << 53;

#[derive(Clone, Copy, Default)]
struct Size {
    nodes: u64,
    bytes: u64,
    height: usize,
}

impl Size {
    fn leaf(bytes: usize) -> Size {
        Size {
            nodes: 1,
            bytes: bytes as u64,
            height: 0,
        }
    }

    fn add_child(&mut self, child: Size) {
        self.nodes = self.nodes.saturating_add(child.nodes);
        self.bytes = self.bytes.saturating_add(child.bytes);
        self.height = self.height.max(child.height + 1);
    }
}

/// A node just finished. A scalar's value is still a `Result`: a key is read
/// as its text, so a number out of range is an error only where it is a value.
struct Item {
    value: Result<Value, Failure>,
    key: Option<String>,
    size: Size,
    at: Span,
}

struct Anchored {
    value: Result<Rc<Value>, Failure>,
    key: Option<String>,
    size: Size,
}

enum Open {
    Seq(Vec<Value>),
    Map {
        entries: Vec<(String, Value)>,
        seen: HashSet<String>,
        key: Option<String>,
    },
}

struct Frame {
    open: Open,
    anchor: usize,
    size: Size,
    at: Span,
}

fn failure(code: &'static str, message: impl Into<String>, at: &Span) -> Failure {
    Failure::at(code, message, at.start.line(), at.start.col() + 1)
}

fn scan_failure(e: &ScanError) -> Failure {
    let (line, column) = (e.marker().line(), e.marker().col() + 1);
    Failure::at(
        "yaml.syntax",
        format!("{} at line {line}, column {column}", e.info()),
        line,
        column,
    )
}

#[derive(Default)]
struct Loader {
    stack: Vec<Frame>,
    anchors: HashMap<usize, Anchored>,
    documents: usize,
    alias_nodes: u64,
    alias_bytes: u64,
    root: Option<Value>,
}

/// The longest text `parse` reads: parsing is native code, which no script
/// time limit interrupts, so its input is what bounds its time.
pub(crate) const MAX_INPUT_BYTES: usize = 8 * 1024 * 1024;

pub(crate) fn parse(text: &str) -> Result<Value, Failure> {
    if text.len() > MAX_INPUT_BYTES {
        return Err(Failure::new(
            "yaml.too_large",
            format!(
                "the text is {} bytes; at most {} MiB is parsed",
                text.len(),
                MAX_INPUT_BYTES / (1024 * 1024)
            ),
        ));
    }
    let mut loader = Loader::default();
    for next in Parser::new_from_str(text) {
        let (event, at) = next.map_err(|e| scan_failure(&e))?;
        if let Some(item) = loader.event(event, at)? {
            loader.place(item)?;
        }
    }
    Ok(loader.root.unwrap_or(Value::Null))
}

impl Loader {
    fn event(&mut self, event: Event<'_>, at: Span) -> Result<Option<Item>, Failure> {
        Ok(match event {
            Event::StreamStart | Event::StreamEnd | Event::DocumentEnd | Event::Nothing => None,
            Event::DocumentStart(_) => {
                self.documents += 1;
                if self.documents > 1 {
                    return Err(failure(
                        "yaml.multiple_documents",
                        format!(
                            "a second document starts at line {}; agix.yaml.parse reads one",
                            at.start.line()
                        ),
                        &at,
                    ));
                }
                None
            }
            Event::Alias(id) => Some(self.alias(id, at)?),
            Event::Scalar(text, style, anchor, tag) => {
                let value = scalar(&text, style, tag.as_deref(), &at)?;
                let bytes = match &value {
                    Ok(Value::String(s)) => s.len(),
                    _ => 8,
                };
                let item = Item {
                    value,
                    key: Some(text.into_owned()),
                    size: Size::leaf(bytes),
                    at,
                };
                Some(self.remember(anchor, item))
            }
            Event::SequenceStart(anchor, tag) => {
                collection_tag(tag.as_deref(), "seq", &at)?;
                self.open(Open::Seq(Vec::new()), anchor, at)?;
                None
            }
            Event::MappingStart(anchor, tag) => {
                collection_tag(tag.as_deref(), "map", &at)?;
                let open = Open::Map {
                    entries: Vec::new(),
                    seen: HashSet::new(),
                    key: None,
                };
                self.open(open, anchor, at)?;
                None
            }
            Event::SequenceEnd | Event::MappingEnd => {
                let frame = self
                    .stack
                    .pop()
                    .ok_or_else(|| failure("yaml.syntax", "unbalanced collection", &at))?;
                let value = match frame.open {
                    Open::Seq(items) => Value::Array(items),
                    Open::Map { entries, .. } => Value::Object(entries),
                };
                let item = Item {
                    value: Ok(value),
                    key: None,
                    size: frame.size,
                    at: frame.at,
                };
                Some(self.remember(frame.anchor, item))
            }
        })
    }

    fn alias(&mut self, id: usize, at: Span) -> Result<Item, Failure> {
        let target = self
            .anchors
            .get(&id)
            .ok_or_else(|| failure("yaml.syntax", "alias to an unknown anchor", &at))?;
        self.alias_nodes = self.alias_nodes.saturating_add(target.size.nodes);
        self.alias_bytes = self.alias_bytes.saturating_add(target.size.bytes);
        if self.alias_nodes > ALIAS_NODE_LIMIT || self.alias_bytes > ALIAS_BYTE_LIMIT {
            return Err(failure(
                "yaml.alias_limit",
                format!(
                    "aliases expand to more than {ALIAS_NODE_LIMIT} nodes or {ALIAS_BYTE_LIMIT} bytes"
                ),
                &at,
            ));
        }
        Ok(Item {
            value: target.value.clone().map(Value::Shared),
            key: target.key.clone(),
            size: target.size,
            at,
        })
    }

    fn open(&mut self, open: Open, anchor: usize, at: Span) -> Result<(), Failure> {
        if self.stack.len() >= MAX_DEPTH {
            return Err(too_deep(&at));
        }
        self.stack.push(Frame {
            open,
            anchor,
            size: Size::leaf(0),
            at,
        });
        Ok(())
    }

    fn remember(&mut self, anchor: usize, item: Item) -> Item {
        if anchor == 0 {
            return item;
        }
        let shared = item.value.map(Rc::new);
        self.anchors.insert(
            anchor,
            Anchored {
                value: shared.clone(),
                key: item.key.clone(),
                size: item.size,
            },
        );
        Item {
            value: shared.map(Value::Shared),
            ..item
        }
    }

    fn place(&mut self, item: Item) -> Result<(), Failure> {
        if self.stack.len() + item.size.height > MAX_DEPTH {
            return Err(too_deep(&item.at));
        }
        let Some(frame) = self.stack.last_mut() else {
            self.root = Some(item.value?);
            return Ok(());
        };
        frame.size.add_child(item.size);
        match &mut frame.open {
            Open::Seq(items) => items.push(item.value?),
            Open::Map { entries, seen, key } => match key.take() {
                None => {
                    let text = item.key.ok_or_else(|| {
                        failure(
                            "yaml.key",
                            "a mapping key must be a scalar; JSON object keys are strings",
                            &item.at,
                        )
                    })?;
                    if !seen.insert(text.clone()) {
                        return Err(failure(
                            "yaml.duplicate_key",
                            format!("duplicate key {text:?}"),
                            &item.at,
                        ));
                    }
                    *key = Some(text);
                }
                Some(k) => entries.push((k, item.value?)),
            },
        }
        Ok(())
    }
}

fn too_deep(at: &Span) -> Failure {
    failure(
        "yaml.depth",
        format!("nested deeper than {MAX_DEPTH} levels"),
        at,
    )
}

fn is_core(tag: &Tag) -> bool {
    tag.is_yaml_core_schema()
}

/// The lone `!`, which the scanner hands over as an empty handle.
fn is_non_specific(tag: &Tag) -> bool {
    (tag.handle.is_empty() && tag.suffix == "!") || (tag.handle == "!" && tag.suffix.is_empty())
}

fn unsupported_tag(tag: &Tag, at: &Span) -> Failure {
    failure(
        "yaml.tag",
        format!("tag {tag} has no JSON value; only the core schema's tags are read"),
        at,
    )
}

fn collection_tag(tag: Option<&Tag>, kind: &str, at: &Span) -> Result<(), Failure> {
    match tag {
        None => Ok(()),
        Some(t) if is_non_specific(t) || (is_core(t) && t.suffix == kind) => Ok(()),
        Some(t) => Err(unsupported_tag(t, at)),
    }
}

/// The outer error is the tag's and stands wherever the node is; the inner
/// one is the value's.
fn scalar(
    text: &str,
    style: ScalarStyle,
    tag: Option<&Tag>,
    at: &Span,
) -> Result<Result<Value, Failure>, Failure> {
    let Some(tag) = tag else {
        return Ok(if style == ScalarStyle::Plain {
            resolve_plain(text, at)
        } else {
            Ok(Value::String(text.to_string()))
        });
    };
    if is_non_specific(tag) {
        return Ok(Ok(Value::String(text.to_string())));
    }
    if !is_core(tag) {
        return Err(unsupported_tag(tag, at));
    }
    let mismatch = || failure("yaml.tag", format!("{text:?} is not a valid {tag}"), at);
    let fits = match tag.suffix.as_str() {
        "str" => return Ok(Ok(Value::String(text.to_string()))),
        "null" => is_null_word(text),
        "bool" => bool_word(text).is_some(),
        "int" => looks_like_int(text),
        "float" => looks_like_int(text) || looks_like_float(text) || special_float(text).is_some(),
        _ => return Err(unsupported_tag(tag, at)),
    };
    if fits {
        Ok(resolve_plain(text, at))
    } else {
        Err(mismatch())
    }
}

fn is_null_word(s: &str) -> bool {
    matches!(s, "" | "~" | "null" | "Null" | "NULL")
}

fn bool_word(s: &str) -> Option<bool> {
    match s {
        "true" | "True" | "TRUE" => Some(true),
        "false" | "False" | "FALSE" => Some(false),
        _ => None,
    }
}

fn digits(s: &str, radix: u32) -> bool {
    !s.is_empty() && s.chars().all(|c| c.is_digit(radix))
}

fn decimal_int(s: &str) -> bool {
    digits(s.strip_prefix(['-', '+']).unwrap_or(s), 10)
}

fn looks_like_int(s: &str) -> bool {
    decimal_int(s)
        || s.strip_prefix("0o").is_some_and(|d| digits(d, 8))
        || s.strip_prefix("0x").is_some_and(|d| digits(d, 16))
}

fn special_float(s: &str) -> Option<&'static str> {
    let unsigned = s.strip_prefix(['-', '+']).unwrap_or(s);
    if matches!(unsigned, ".inf" | ".Inf" | ".INF") {
        return Some("infinity");
    }
    matches!(s, ".nan" | ".NaN" | ".NAN").then_some("NaN")
}

/// `[-+]? ( \. [0-9]+ | [0-9]+ ( \. [0-9]* )? ) ( [eE] [-+]? [0-9]+ )?`
fn looks_like_float(s: &str) -> bool {
    let s = s.strip_prefix(['-', '+']).unwrap_or(s);
    let (mantissa, exponent) = match s.find(['e', 'E']) {
        Some(i) => (&s[..i], Some(&s[i + 1..])),
        None => (s, None),
    };
    let mantissa_ok = match mantissa.split_once('.') {
        Some(("", frac)) => digits(frac, 10),
        Some((int, frac)) => digits(int, 10) && (frac.is_empty() || digits(frac, 10)),
        None => digits(mantissa, 10),
    };
    mantissa_ok && exponent.is_none_or(|e| digits(e.strip_prefix(['-', '+']).unwrap_or(e), 10))
}

fn plain_is_not_a_string(s: &str) -> bool {
    is_null_word(s)
        || bool_word(s).is_some()
        || looks_like_int(s)
        || special_float(s).is_some()
        || looks_like_float(s)
}

fn resolve_plain(s: &str, at: &Span) -> Result<Value, Failure> {
    if is_null_word(s) {
        return Ok(Value::Null);
    }
    if let Some(b) = bool_word(s) {
        return Ok(Value::Bool(b));
    }
    let out_of_range = || {
        failure(
            "yaml.number_range",
            format!("{s} is outside the range a JavaScript number holds exactly"),
            at,
        )
    };
    if looks_like_int(s) {
        let (negative, magnitude) = if let Some(d) = s.strip_prefix("0x") {
            (false, u128::from_str_radix(d, 16).ok())
        } else if let Some(d) = s.strip_prefix("0o") {
            (false, u128::from_str_radix(d, 8).ok())
        } else {
            let negative = s.starts_with('-');
            let d = s.trim_start_matches(['-', '+']);
            (negative, d.parse::<u128>().ok())
        };
        let magnitude = magnitude
            .filter(|m| *m <= MAX_SAFE)
            .ok_or_else(out_of_range)?;
        let n = magnitude as f64;
        return Ok(Value::Number(if negative { -n } else { n }));
    }
    if let Some(what) = special_float(s) {
        return Err(failure(
            "yaml.number_range",
            format!("{s} is {what}, which JSON has no value for"),
            at,
        ));
    }
    if looks_like_float(s) {
        let n: f64 = s.parse().map_err(|_| out_of_range())?;
        return if n.is_finite() {
            Ok(Value::Number(n))
        } else {
            Err(out_of_range())
        };
    }
    Ok(Value::String(s.to_string()))
}

pub(crate) fn stringify(value: &Value) -> Result<String, Failure> {
    check_numbers(value)?;
    let mut out = String::new();
    match value.resolved() {
        Value::Array(items) if !items.is_empty() => sequence(&mut out, items, 0, false),
        Value::Object(entries) if !entries.is_empty() => mapping(&mut out, entries, 0, false),
        other => {
            out.push_str(&inline(other, 2));
            out.push('\n');
        }
    }
    Ok(out)
}

fn check_numbers(value: &Value) -> Result<(), Failure> {
    match value.resolved() {
        Value::Number(n) if !n.is_finite() => Err(Failure::new(
            "yaml.unrepresentable",
            "a non-finite number has no YAML value here",
        )),
        Value::Array(items) => items.iter().try_for_each(check_numbers),
        Value::Object(entries) => entries.iter().try_for_each(|(_, v)| check_numbers(v)),
        _ => Ok(()),
    }
}

fn indent(out: &mut String, n: usize) {
    out.extend(std::iter::repeat_n(' ', n));
}

fn mapping(out: &mut String, entries: &[(String, Value)], at: usize, first_inline: bool) {
    for (i, (key, value)) in entries.iter().enumerate() {
        if i > 0 || !first_inline {
            indent(out, at);
        }
        if key.chars().count() > 1000 {
            out.push_str("? ");
            out.push_str(&double_quoted(key));
            out.push('\n');
            indent(out, at);
            out.push(':');
        } else {
            out.push_str(&key_text(key));
            out.push(':');
        }
        after_indicator(out, value, at, true);
    }
}

fn sequence(out: &mut String, items: &[Value], at: usize, first_inline: bool) {
    for (i, item) in items.iter().enumerate() {
        if i > 0 || !first_inline {
            indent(out, at);
        }
        out.push('-');
        after_indicator(out, item, at, false);
    }
}

/// What follows `key:` or `-`; `at` is the column of that key or dash.
fn after_indicator(out: &mut String, value: &Value, at: usize, under_key: bool) {
    match value.resolved() {
        Value::Object(entries) if !entries.is_empty() => {
            if under_key {
                out.push('\n');
                mapping(out, entries, at + 2, false);
            } else {
                out.push(' ');
                mapping(out, entries, at + 2, true);
            }
        }
        Value::Array(items) if !items.is_empty() => {
            if under_key {
                out.push('\n');
                sequence(out, items, at + 2, false);
            } else {
                out.push(' ');
                sequence(out, items, at + 2, true);
            }
        }
        scalar => {
            out.push(' ');
            out.push_str(&inline(scalar, at + 2));
            out.push('\n');
        }
    }
}

fn inline(value: &Value, content: usize) -> String {
    match value.resolved() {
        Value::Null => "null".into(),
        Value::Bool(b) => b.to_string(),
        Value::Number(n) => number(*n),
        Value::String(s) => string_text(s, content),
        Value::Array(_) => "[]".into(),
        Value::Object(_) => "{}".into(),
        Value::Shared(v) => inline(v, content),
    }
}

fn number(n: f64) -> String {
    if n.fract() == 0.0 {
        return if n.abs() <= MAX_SAFE as f64 {
            format!("{}", n as i64)
        } else {
            format!("{n:e}")
        };
    }
    let plain = format!("{n}");
    let exponent = format!("{n:e}");
    if exponent.len() < plain.len() {
        exponent
    } else {
        plain
    }
}

fn printable(c: char) -> bool {
    matches!(c, '\t' | '\n' | ' '..='~' | '\u{85}' | '\u{A0}'..='\u{D7FF}' | '\u{E000}'..='\u{FFFD}' | '\u{10000}'..)
        && c != '\u{FEFF}'
}

fn plain_ok(s: &str) -> bool {
    let Some(first) = s.chars().next() else {
        return false;
    };
    if plain_is_not_a_string(s) {
        return false;
    }
    if "?:,[]{}#&*!|>'\"%@`".contains(first) {
        return false;
    }
    if first == '-' && (s.len() == 1 || s.starts_with("- ") || s.starts_with("---")) {
        return false;
    }
    if s.starts_with("...") {
        return false;
    }
    if s.starts_with([' ', '\t']) || s.ends_with([' ', '\t', ':']) {
        return false;
    }
    if s.contains(": ") || s.contains(":\t") || s.contains(" #") || s.contains("\t#") {
        return false;
    }
    s.chars()
        .all(|c| printable(c) && !matches!(c, '\t' | '\n' | '\u{85}' | '\u{2028}' | '\u{2029}'))
}

fn single_quoted_ok(s: &str) -> bool {
    s.chars()
        .all(|c| printable(c) && !matches!(c, '\t' | '\n' | '\u{85}' | '\u{2028}' | '\u{2029}'))
}

fn key_text(s: &str) -> String {
    if plain_ok(s) {
        s.to_string()
    } else if single_quoted_ok(s) {
        single_quoted(s)
    } else {
        double_quoted(s)
    }
}

fn string_text(s: &str, content: usize) -> String {
    if plain_ok(s) {
        s.to_string()
    } else if single_quoted_ok(s) {
        single_quoted(s)
    } else if literal_ok(s) {
        literal(s, content)
    } else {
        double_quoted(s)
    }
}

fn single_quoted(s: &str) -> String {
    format!("'{}'", s.replace('\'', "''"))
}

fn double_quoted(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\t' => out.push_str("\\t"),
            '\r' => out.push_str("\\r"),
            '\0' => out.push_str("\\0"),
            c if printable(c) && !matches!(c, '\u{85}' | '\u{2028}' | '\u{2029}') => out.push(c),
            c if (c as u32) <= 0xFF => out.push_str(&format!("\\x{:02X}", c as u32)),
            c if (c as u32) <= 0xFFFF => out.push_str(&format!("\\u{:04X}", c as u32)),
            c => out.push_str(&format!("\\U{:08X}", c as u32)),
        }
    }
    out.push('"');
    out
}

/// A literal block keeps every line as written; it is used only where its
/// reading is unambiguous: printable text with a line break, no carriage
/// return, and no line made only of blanks.
fn literal_ok(s: &str) -> bool {
    s.contains('\n')
        && s.chars().any(|c| !c.is_whitespace())
        && s.chars()
            .all(|c| printable(c) && !matches!(c, '\u{85}' | '\u{2028}' | '\u{2029}'))
        && s.split('\n')
            .all(|line| line.is_empty() || line.chars().any(|c| c != ' ' && c != '\t'))
        && !s.split('\n').any(|line| line.starts_with('\t'))
}

fn literal(s: &str, content: usize) -> String {
    let body = s.trim_end_matches('\n');
    let trailing = s.len() - body.len();
    let chomp = match trailing {
        0 => "-",
        1 => "",
        _ => "+",
    };
    let first = body.split('\n').find(|l| !l.is_empty()).unwrap_or("");
    let hint = if first.starts_with(' ') { "2" } else { "" };
    let mut out = format!("|{hint}{chomp}\n");
    for line in body.split('\n') {
        if !line.is_empty() {
            indent(&mut out, content);
            out.push_str(line);
        }
        out.push('\n');
    }
    for _ in 1..trailing {
        out.push('\n');
    }
    out.pop();
    out
}

#[cfg(test)]
mod tests;
