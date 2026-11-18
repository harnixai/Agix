//! `agix` as a script sees it: installed into a fresh QuickJS context, called
//! from JavaScript, results and errors read back as JSON.

use serde_json::{json, Value};

fn run(body: &str) -> Value {
    let runtime = rquickjs::Runtime::new().unwrap();
    let context = rquickjs::Context::full(&runtime).unwrap();
    let source = format!(
        "(() => {{ try {{ return JSON.stringify({{ ok: (() => {{ {body}\n }})() }}); }} \
         catch (e) {{ return JSON.stringify({{ err: {{ name: e.name, code: e.code, \
         message: e.message, line: e.line, column: e.column, \
         isError: e instanceof Error }} }}); }} }})()"
    );
    context.with(|ctx| {
        js_lib::install(&ctx).unwrap();
        let out: String = ctx.eval(source).unwrap_or_else(|e| {
            panic!("{body}\n{e}: {:?}", ctx.catch());
        });
        serde_json::from_str(&out).unwrap()
    })
}

fn ok(body: &str) -> Value {
    let out = run(body);
    match out.get("ok") {
        Some(v) => v.clone(),
        None => panic!("{body}\nthrew {}", out["err"]),
    }
}

fn err(body: &str) -> Value {
    let out = run(body);
    match out.get("err") {
        Some(e) => e.clone(),
        None => panic!("{body}\nreturned {}", out["ok"]),
    }
}

fn code(body: &str) -> String {
    let e = err(body);
    assert_eq!(e["name"], "AgixError", "{e}");
    assert_eq!(e["isError"], true, "{e}");
    e["code"].as_str().unwrap_or_default().to_string()
}

#[test]
fn version_is_the_engines_x_y_z() {
    let v = ok("return agix.version;");
    let v = v.as_str().unwrap();
    assert_eq!(v, js_lib::version());
    assert_eq!(v.split('.').count(), 3, "{v}");
    assert!(env!("CARGO_PKG_VERSION").starts_with(v));
}

#[test]
fn agix_is_frozen_all_the_way_down() {
    assert_eq!(
        ok(r#"
            "use strict";
            const tries = [
              () => { agix.yaml = null; },
              () => { agix.yaml.parse = null; },
              () => { agix.yaml.parse.extra = 1; },
              () => { agix.extra = 1; },
              () => { delete agix.glob; },
              () => { globalThis.agix = {}; },
              () => { Object.defineProperty(globalThis, "agix", { value: 1 }); },
            ];
            return tries.map((t) => { try { t(); return "changed"; } catch (e) { return e.name; } });
        "#),
        json!(vec!["TypeError"; 7])
    );
    assert_eq!(
        ok("return [Object.isFrozen(agix), Object.isFrozen(agix.cli), typeof agix.yaml.parse];"),
        json!([true, true, "function"])
    );
}

#[test]
fn errors_are_agix_errors_with_a_code() {
    let e = err("agix.yaml.parse('a: [1');");
    assert_eq!(e["name"], "AgixError");
    assert_eq!(e["code"], "yaml.syntax");
    assert_eq!(e["isError"], true);
    assert!(e["line"].as_u64().is_some(), "{e}");
    assert!(e["column"].as_u64().is_some(), "{e}");
    assert_eq!(
        ok("try { agix.glob.test('[', 'a'); } catch (e) { return [e instanceof Error, e.constructor.name, String(e).startsWith('AgixError: ')]; }"),
        json!([true, "AgixError", true])
    );
}

#[test]
fn wrong_argument_types_are_type_errors() {
    for call in [
        "agix.yaml.parse(1)",
        "agix.glob.test(1, 'a')",
        "agix.glob.filter('*', 'a')",
        "agix.path.join('a', 1)",
        "agix.hash.sha256(1)",
        "agix.cli.parseArgs('a', {})",
        "agix.json.validate({}, undefined)",
        "agix.yaml.stringifyFrontmatter({}, 1)",
    ] {
        assert_eq!(err(&format!("{call};"))["name"], "TypeError", "{call}");
    }
}

#[test]
fn yaml_parse_keeps_order_and_resolves_the_core_schema() {
    assert_eq!(
        ok("return Object.keys(agix.yaml.parse('z: 1\\na: 2\\nm: 3'));"),
        json!(["z", "a", "m"])
    );
    assert_eq!(
        ok("return agix.yaml.parse('a: 2024-01-01\\nb: yes\\nc: 0x10\\nd: ~\\ne: [1, \"2\"]');"),
        json!({"a": "2024-01-01", "b": "yes", "c": 16, "d": null, "e": [1, "2"]})
    );
}

#[test]
fn yaml_error_codes() {
    assert_eq!(
        code("agix.yaml.parse('a: 1\\n---\\nb: 2');"),
        "yaml.multiple_documents"
    );
    assert_eq!(
        code("agix.yaml.parse('n: 9007199254740993');"),
        "yaml.number_range"
    );
    assert_eq!(code("agix.yaml.parse('a: !thing x');"), "yaml.tag");
    assert_eq!(code("agix.yaml.parse('? [1]\\n: x');"), "yaml.key");
    assert_eq!(
        code("agix.yaml.parse('a: 1\\na: 2');"),
        "yaml.duplicate_key"
    );
    assert_eq!(
        code("agix.yaml.parse('['.repeat(200) + ']'.repeat(200));"),
        "yaml.depth"
    );
    assert_eq!(
        code(
            r#"
            let s = 'a: &a ["x","x","x","x","x","x","x","x","x","x"]\n';
            for (const [n, p] of [["b","a"],["c","b"],["d","c"],["e","d"],["f","e"],["g","f"],["h","g"],["i","h"]]) {
              s += `${n}: &${n} [${Array(10).fill("*" + p).join(",")}]\n`;
            }
            agix.yaml.parse(s);
        "#
        ),
        "yaml.alias_limit"
    );
    for bad in [
        "undefined",
        "() => 1",
        "NaN",
        "Infinity",
        "{ a: undefined }",
        "[1n]",
        "Symbol()",
    ] {
        assert_eq!(
            code(&format!("agix.yaml.stringify({bad});")),
            "yaml.unrepresentable",
            "{bad}"
        );
    }
    assert_eq!(
        code("const a = {}; a.self = a; agix.yaml.stringify(a);"),
        "yaml.unrepresentable"
    );
    assert_eq!(
        code("let v = 1; for (let i = 0; i < 200; i++) v = [v]; agix.yaml.stringify(v);"),
        "yaml.depth"
    );
}

#[test]
fn yaml_stringify_writes_block_style_in_insertion_order() {
    assert_eq!(
        ok("return agix.yaml.stringify({ name: 'forge', tags: ['a', 'b'], when: new Date(0), nested: { z: 1, a: '12' }, empty: [] });"),
        json!("name: forge\ntags:\n  - a\n  - b\nwhen: 1970-01-01T00:00:00.000Z\nnested:\n  z: 1\n  a: '12'\nempty: []\n")
    );
    assert_eq!(
        ok("const v = { s: 'a\\nb', n: -1.5, t: true, u: null, q: 'it: is', k: ['x', { y: [1] }] }; return JSON.stringify(agix.yaml.parse(agix.yaml.stringify(v))) === JSON.stringify(v);"),
        json!(true)
    );
}

#[test]
fn frontmatter_splits_and_joins_byte_for_byte() {
    assert_eq!(
        ok("return agix.yaml.parseFrontmatter('---\\ntitle: x\\ntags: [a]\\n---\\n# Body\\n\\ntext\\n');"),
        json!({"data": {"title": "x", "tags": ["a"]}, "body": "# Body\n\ntext\n"})
    );
    assert_eq!(
        ok("return agix.yaml.parseFrontmatter('\\uFEFF---\\r\\na: 1\\r\\n---\\r\\nbody\\r\\nmore');"),
        json!({"data": {"a": 1}, "body": "body\r\nmore"})
    );
    assert_eq!(
        ok("return agix.yaml.parseFrontmatter('no frontmatter\\n---\\nx\\n');"),
        json!({"data": null, "body": "no frontmatter\n---\nx\n"})
    );
    assert_eq!(
        ok("return agix.yaml.parseFrontmatter('---\\n---\\nbody');"),
        json!({"data": {}, "body": "body"})
    );
    assert_eq!(
        ok("return agix.yaml.parseFrontmatter('---\\na: 1\\n---');"),
        json!({"data": {"a": 1}, "body": ""})
    );
    assert_eq!(
        ok("return agix.yaml.stringifyFrontmatter({ title: 'x' }, 'body\\n');"),
        json!("---\ntitle: x\n---\nbody\n")
    );
    assert_eq!(
        ok("return agix.yaml.stringifyFrontmatter(null, 'just body');"),
        json!("just body")
    );
    assert_eq!(
        ok(r#"
            const bodies = ["", "x", "\n", "\r\n\r\n", "---", "a\n---\nb", "﻿bom", "  lead\n",
                            "trailing ws  \t", "tabs\tand seps", "end without newline", "🎉\r"];
            const datas = [{}, { a: 1 }, { s: "---" }, { s: "a\n---\nb" }, ["---"], "---", 12, { "---": "x" },
                           { long: "x".repeat(2000) }];
            const bad = [];
            for (const b of bodies) for (const d of datas) {
              const text = agix.yaml.stringifyFrontmatter(d, b);
              const back = agix.yaml.parseFrontmatter(text);
              if (back.body !== b || JSON.stringify(back.data) !== JSON.stringify(d)) bad.push([d, b, text]);
            }
            return bad;
        "#),
        json!([])
    );
}

#[test]
fn frontmatter_errors() {
    let e = err("agix.yaml.parseFrontmatter('---\\na: 1\\nb: [\\n---\\nbody');");
    assert_eq!(e["code"], "yaml.syntax");
    assert!(
        e["line"].as_u64().unwrap() >= 3,
        "lines count from the top of the text: {e}"
    );
    assert_eq!(
        code("agix.yaml.parseFrontmatter('---\\na: 1\\n');"),
        "yaml.syntax"
    );
    assert_eq!(
        code("agix.yaml.stringifyFrontmatter(null, '---\\na: 1\\n---\\nx');"),
        "yaml.unrepresentable"
    );
    assert_eq!(
        code("agix.yaml.stringifyFrontmatter(undefined, 'x');"),
        "yaml.unrepresentable"
    );
}

#[test]
fn glob_has_the_glob_tools_semantics() {
    let cases = [
        ("*.rs", "main.rs", true),
        ("*.rs", "src/main.rs", false),
        ("**/*.rs", "src/a/b.rs", true),
        ("**/*.rs", "main.rs", true),
        ("src/**", "src/a/b", true),
        ("*.{js,ts}", "a.ts", true),
        ("*.{js,ts}", "a.rs", false),
        ("[ab].txt", "b.txt", true),
        ("[!ab].txt", "b.txt", false),
        ("?.md", "a.md", true),
        ("?.md", "/.md", false),
        ("*", ".hidden", true),
        ("*.md", "README.MD", false),
        ("/abs/*.md", "/abs/x.md", true),
        ("abs/*.md", "/abs/x.md", false),
    ];
    for (pattern, path, want) in cases {
        assert_eq!(
            ok(&format!("return agix.glob.test({pattern:?}, {path:?});")),
            json!(want),
            "{pattern} vs {path}"
        );
    }
    assert_eq!(
        ok("return agix.glob.test(['*.md', '*.rs'], 'x.rs');"),
        json!(true)
    );
    assert_eq!(
        ok("return agix.glob.filter(['docs/**', '*.toml'], ['b.toml', 'src/x.rs', 'docs/a/b.md', 'a.toml']);"),
        json!(["b.toml", "docs/a/b.md", "a.toml"])
    );
    assert_eq!(code("agix.glob.test('a[', 'a');"), "glob.pattern");
    assert_eq!(
        code("agix.glob.filter(['ok', '{a'], ['a']);"),
        "glob.pattern"
    );
}

#[test]
fn path_matches_node_path_posix() {
    let cases: &[(&str, Value)] = &[
        ("join('a', 'b', '../c', './d')", json!("a/c/d")),
        ("join()", json!(".")),
        ("join('', '')", json!(".")),
        ("join('/a/', 'b/')", json!("/a/b/")),
        ("join('a', '/b')", json!("a/b")),
        ("normalize('')", json!(".")),
        ("normalize('/../a//b/./c/..')", json!("/a/b")),
        ("normalize('../a/../../b')", json!("../../b")),
        ("normalize('./')", json!("./")),
        ("normalize('a/..')", json!(".")),
        ("normalize('//')", json!("/")),
        ("relative('/a/b/c', '/a/d')", json!("../../d")),
        ("relative('/a', '/a')", json!("")),
        ("relative('/a/bc', '/a/b')", json!("../b")),
        ("relative('/', '/x/y')", json!("x/y")),
        ("relative('a/b', 'a/c/d')", json!("../c/d")),
        ("dirname('/a/b/')", json!("/a")),
        ("dirname('/a/b')", json!("/a")),
        ("dirname('a')", json!(".")),
        ("dirname('/')", json!("/")),
        ("dirname('//a')", json!("//")),
        ("dirname('/a//b')", json!("/a/")),
        ("dirname('')", json!(".")),
        ("basename('/a/b.txt')", json!("b.txt")),
        ("basename('/a/b.txt', '.txt')", json!("b")),
        ("basename('/a/b/')", json!("b")),
        ("basename('b.txt', 'b.txt')", json!("")),
        ("basename('/a/b.txt', 'xt')", json!("b.t")),
        ("basename('aaa', 'a')", json!("aa")),
        ("basename('')", json!("")),
        ("extname('a.tar.gz')", json!(".gz")),
        ("extname('.bashrc')", json!("")),
        ("extname('a.')", json!(".")),
        ("extname('a')", json!("")),
        ("extname('..')", json!("")),
        ("extname('a/.b.c')", json!(".c")),
        ("isAbsolute('/x')", json!(true)),
        ("isAbsolute('x/y')", json!(false)),
        ("isAbsolute('')", json!(false)),
    ];
    for (call, want) in cases {
        assert_eq!(&ok(&format!("return agix.path.{call};")), want, "{call}");
    }
    assert_eq!(code("agix.path.relative('/a', 'b');"), "path.relative");
    assert_eq!(code("agix.path.relative('../a', 'b');"), "path.relative");
    assert_eq!(err("agix.path.resolve('a');")["name"], "TypeError");
}

#[test]
fn json_validate_reports_errors_as_values() {
    assert_eq!(
        ok("return agix.json.validate({ type: 'object', required: ['a'] }, { a: 1 });"),
        json!({"valid": true, "errors": []})
    );
    let out = ok(r#"return agix.json.validate(
        { type: "object", properties: { a: { type: "integer" }, list: { type: "array", items: { type: "string" } } } },
        { a: "x", list: ["ok", 3] });"#);
    assert_eq!(out["valid"], false);
    let errors = out["errors"].as_array().unwrap();
    let at: Vec<&str> = errors
        .iter()
        .map(|e| e["instancePath"].as_str().unwrap())
        .collect();
    assert!(at.contains(&"/a") && at.contains(&"/list/1"), "{out}");
    for e in errors {
        assert!(e["schemaPath"].as_str().unwrap().starts_with('/'), "{e}");
        assert!(!e["message"].as_str().unwrap().is_empty(), "{e}");
    }
}

#[test]
fn json_validate_drafts_formats_refs_and_limits() {
    assert_eq!(
        ok("return agix.json.validate({ type: 'string', format: 'date-time' }, 'yesterday').valid;"),
        json!(false)
    );
    assert_eq!(
        ok("return ['email', 'uuid', 'date', 'uri'].map((f) => agix.json.validate({ format: f }, 'not valid !').valid);"),
        json!([false, false, false, false])
    );
    assert_eq!(
        ok("return agix.json.validate({ $defs: { n: { type: 'number' } }, items: { $ref: '#/$defs/n' } }, [1, 'x']).errors.map((e) => e.instancePath);"),
        json!(["/1"])
    );
    assert_eq!(
        ok("return agix.json.validate({ $schema: 'http://json-schema.org/draft-04/schema#', type: 'number', maximum: 5, exclusiveMaximum: true }, 5).valid;"),
        json!(false),
        "draft 4's boolean exclusiveMaximum"
    );
    assert_eq!(
        ok("return agix.json.validate({ type: 'array', items: { type: 'string' } }, Array(500).fill(1)).errors.length;"),
        json!(100)
    );
    assert_eq!(
        code("agix.json.validate({ $ref: 'https://example.com/s.json' }, 1);"),
        "json.external_ref"
    );
    assert_eq!(
        code("agix.json.validate({ $ref: 'other.json#/x' }, 1);"),
        "json.external_ref"
    );
    assert_eq!(code("agix.json.validate({ type: 12 }, 1);"), "json.schema");
    assert_eq!(
        code("agix.json.validate({ minimum: 'x' }, 1);"),
        "json.schema"
    );
    assert_eq!(
        code("let v = 1; for (let i = 0; i < 200; i++) v = [v]; agix.json.validate({}, v);"),
        "json.depth"
    );
}

#[test]
fn sha256_of_text_and_bytes() {
    assert_eq!(
        ok("return agix.hash.sha256('');"),
        json!("e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855")
    );
    assert_eq!(
        ok("return agix.hash.sha256('é');"),
        ok("return agix.hash.sha256(new Uint8Array([0xc3, 0xa9]));")
    );
    assert_eq!(
        ok("return agix.hash.sha256(new Uint8Array([1, 2, 3, 4]).subarray(1, 3));"),
        ok("return agix.hash.sha256(new Uint8Array([2, 3]));")
    );
}

#[test]
fn uuids_are_lowercase_and_hyphenated() {
    let out = ok("return [agix.uuid.v4(), agix.uuid.v4(), agix.uuid.v7(), agix.uuid.v7()];");
    let ids: Vec<&str> = out
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap())
        .collect();
    let shape = |s: &str| {
        s.len() == 36
            && s.chars().enumerate().all(|(i, c)| {
                if [8, 13, 18, 23].contains(&i) {
                    c == '-'
                } else {
                    c.is_ascii_hexdigit() && !c.is_ascii_uppercase()
                }
            })
    };
    assert!(ids.iter().all(|s| shape(s)), "{ids:?}");
    assert_ne!(ids[0], ids[1]);
    assert_eq!(&ids[0][14..15], "4");
    assert_eq!(&ids[2][14..15], "7");
    assert!(ids[2] <= ids[3], "v7 sorts by time: {ids:?}");
}

#[test]
fn parse_args_is_a_strict_subset_of_node_util_parse_args() {
    assert_eq!(
        ok(r#"return agix.cli.parseArgs(
            ["req", "--status", "draft", "-v", "--tag=a", "-t", "b", "-pfoo", "--", "--not-an-option"],
            { options: {
                status: { type: "string" },
                verbose: { type: "boolean", short: "v" },
                tag: { type: "string", short: "t", multiple: true },
                pri: { type: "string", short: "p" },
                limit: { type: "string", default: "10" },
            } });"#),
        json!({
            "values": {"status": "draft", "verbose": true, "tag": ["a", "b"], "pri": "foo", "limit": "10"},
            "positionals": ["req", "--not-an-option"]
        })
    );
    assert_eq!(
        ok(r#"return agix.cli.parseArgs(["-vx", "-"], { options: {
            v: { type: "boolean", short: "v" }, x: { type: "boolean", short: "x" } } });"#),
        json!({"values": {"v": true, "x": true}, "positionals": ["-"]})
    );
    assert_eq!(
        ok("return agix.cli.parseArgs(['--s='], { options: { s: { type: 'string' } } }).values;"),
        json!({"s": ""})
    );
}

#[test]
fn parse_args_errors_name_the_option() {
    let e = err("agix.cli.parseArgs(['--nope'], { options: {} });");
    assert_eq!(e["code"], "cli.unknown_option");
    assert!(e["message"].as_str().unwrap().contains("--nope"), "{e}");
    assert_eq!(
        code("agix.cli.parseArgs(['-z'], { options: { a: { type: 'boolean', short: 'a' } } });"),
        "cli.unknown_option"
    );
    let e = err("agix.cli.parseArgs(['--status'], { options: { status: { type: 'string' } } });");
    assert_eq!(e["code"], "cli.missing_value");
    assert!(e["message"].as_str().unwrap().contains("--status"), "{e}");
    assert_eq!(
        code("agix.cli.parseArgs(['--status', '--x'], { options: { status: { type: 'string' }, x: { type: 'boolean' } } });"),
        "cli.missing_value"
    );
    assert_eq!(
        code("agix.cli.parseArgs(['--flag=yes'], { options: { flag: { type: 'boolean' } } });"),
        "cli.unexpected_value"
    );
    assert_eq!(
        code("agix.cli.parseArgs(['stray'], { options: {}, allowPositionals: false });"),
        "cli.unexpected_positional"
    );
    assert_eq!(
        code("agix.cli.parseArgs([], { options: { a: { type: 'number' } } });"),
        "cli.config"
    );
}

#[test]
fn every_runtime_gets_its_own_agix_from_one_compiled_copy() {
    for _ in 0..3 {
        assert_eq!(ok("return agix.path.join('a', 'b');"), json!("a/b"));
    }
}

#[test]
fn installing_twice_in_one_context_is_refused() {
    let runtime = rquickjs::Runtime::new().unwrap();
    let context = rquickjs::Context::full(&runtime).unwrap();
    context.with(|ctx| {
        js_lib::install(&ctx).unwrap();
        assert!(js_lib::install(&ctx).is_err());
        let v: String = ctx.eval("agix.path.basename('/a/b')").unwrap();
        assert_eq!(v, "b");
    });
}

#[tokio::test]
async fn the_async_runtime_installs_the_same_library() {
    let runtime = rquickjs::AsyncRuntime::new().unwrap();
    let context = rquickjs::AsyncContext::full(&runtime).await.unwrap();
    let out = context
        .async_with(async |ctx| {
            js_lib::install(&ctx).unwrap();
            let promise: rquickjs::Promise = ctx
                .eval("(async () => JSON.stringify(agix.yaml.parse('a: [1, 2]')))()")
                .unwrap();
            promise.into_future::<String>().await.unwrap()
        })
        .await;
    assert_eq!(out, r#"{"a":[1,2]}"#);
}
