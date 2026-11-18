use super::*;

fn json(text: &str) -> String {
    parse(text)
        .unwrap_or_else(|f| panic!("{text:?}: {f:?}"))
        .to_json()
}

fn code(text: &str) -> &'static str {
    match parse(text) {
        Ok(v) => panic!("{text:?} parsed to {}", v.to_json()),
        Err(f) => f.code,
    }
}

fn yaml(json: &str) -> String {
    stringify(&Value::from_json(json).unwrap()).unwrap()
}

fn round_trip(json_text: &str) {
    let written = yaml(json_text);
    let back = parse(&written).unwrap_or_else(|f| panic!("{written}\n{f:?}"));
    assert_eq!(
        back.to_json(),
        Value::from_json(json_text).unwrap().to_json(),
        "written as:\n{written}"
    );
}

#[test]
fn core_schema_scalars() {
    assert_eq!(
        json("a: null\nb: ~\nc:\nd: true\ne: False\nf: 12\ng: -0x1F\nh: 0o17\ni: 0x1F\nj: 1.5\nk: .5\nl: 1e3\nm: yes\nn: 2024-01-02\no: '12'\np: \"true\""),
        r#"{"a":null,"b":null,"c":null,"d":true,"e":false,"f":12,"g":"-0x1F","h":15,"i":31,"j":1.5,"k":0.5,"l":1000,"m":"yes","n":"2024-01-02","o":"12","p":"true"}"#
    );
}

#[test]
fn keys_keep_their_order_and_are_read_as_text() {
    assert_eq!(
        json("z: 1\n1: a\ntrue: b\nnull: c\n1.50: d\n"),
        r#"{"z":1,"1":"a","true":"b","null":"c","1.50":"d"}"#
    );
}

#[test]
fn an_empty_document_is_null() {
    assert_eq!(json(""), "null");
    assert_eq!(json("# only a comment\n"), "null");
    assert_eq!(json("---\n...\n"), "null");
}

#[test]
fn integers_past_two_to_the_53_are_refused() {
    assert_eq!(json("9007199254740992"), "9007199254740992");
    assert_eq!(json("-9007199254740992"), "-9007199254740992");
    assert_eq!(code("9007199254740993"), "yaml.number_range");
    assert_eq!(code("a: 0x20000000000001"), "yaml.number_range");
    assert_eq!(
        code("123456789012345678901234567890123456789012"),
        "yaml.number_range"
    );
    assert_eq!(code("x: .inf"), "yaml.number_range");
    assert_eq!(code("x: -.Inf"), "yaml.number_range");
    assert_eq!(code("x: .NaN"), "yaml.number_range");
    assert_eq!(code("x: 1e400"), "yaml.number_range");
    assert_eq!(json("'9007199254740993': 1"), r#"{"9007199254740993":1}"#);
    assert_eq!(json("9007199254740993: 1"), r#"{"9007199254740993":1}"#);
}

#[test]
fn tags() {
    assert_eq!(
        json("a: !!str 12\nb: !!int '7'\nc: ! 12\nd: !!float 1\ne: !!null ''\nf: !!seq [1]\ng: !!map {x: 1}"),
        r#"{"a":"12","b":7,"c":"12","d":1,"e":null,"f":[1],"g":{"x":1}}"#
    );
    assert_eq!(code("a: !!timestamp 2001-12-14"), "yaml.tag");
    assert_eq!(code("a: !!binary aGk="), "yaml.tag");
    assert_eq!(code("a: !custom x"), "yaml.tag");
    assert_eq!(code("a: !!set {x: null}"), "yaml.tag");
    assert_eq!(code("a: !!int abc"), "yaml.tag");
    assert_eq!(code("a: !!map [1]"), "yaml.tag");
    assert_eq!(code("!thing k: v"), "yaml.tag");
}

#[test]
fn aliases_expand() {
    assert_eq!(
        json("base: &b {x: 1, y: [1, 2]}\ncopy: *b\nname: &n hi\nagain: *n\n*n : key\n"),
        r#"{"base":{"x":1,"y":[1,2]},"copy":{"x":1,"y":[1,2]},"name":"hi","again":"hi","hi":"key"}"#
    );
}

#[test]
fn a_billion_laughs_stops_at_the_alias_limit() {
    let bomb = r#"
a: &a ["lol","lol","lol","lol","lol","lol","lol","lol","lol"]
b: &b [*a,*a,*a,*a,*a,*a,*a,*a,*a]
c: &c [*b,*b,*b,*b,*b,*b,*b,*b,*b]
d: &d [*c,*c,*c,*c,*c,*c,*c,*c,*c]
e: &e [*d,*d,*d,*d,*d,*d,*d,*d,*d]
f: &f [*e,*e,*e,*e,*e,*e,*e,*e,*e]
g: &g [*f,*f,*f,*f,*f,*f,*f,*f,*f]
h: &h [*g,*g,*g,*g,*g,*g,*g,*g,*g]
i: &i [*h,*h,*h,*h,*h,*h,*h,*h,*h]
"#;
    let started = std::time::Instant::now();
    assert_eq!(code(bomb), "yaml.alias_limit");
    assert!(started.elapsed() < std::time::Duration::from_secs(5));
}

#[test]
fn a_long_string_named_many_times_stops_at_the_alias_limit() {
    let mut text = format!("s: &s {}\nl:\n", "x".repeat(1024 * 1024));
    for _ in 0..16 {
        text.push_str("  - *s\n");
    }
    assert_eq!(code(&text), "yaml.alias_limit");
}

#[test]
fn a_second_document_is_refused() {
    assert_eq!(code("a: 1\n---\nb: 2\n"), "yaml.multiple_documents");
    assert_eq!(code("--- 1\n--- 2\n"), "yaml.multiple_documents");
}

#[test]
fn syntax_errors_say_where() {
    let f = parse("a: 1\nb: [1, 2\nc: 3\n").unwrap_err();
    assert_eq!(f.code, "yaml.syntax");
    let (line, column) = f.position.unwrap();
    assert!(line >= 2, "{f:?}");
    assert!(column >= 1, "{f:?}");
    assert!(f.message.contains("line"), "{f:?}");
}

#[test]
fn keys_must_be_scalars_and_unique() {
    assert_eq!(code("? [1, 2]\n: x\n"), "yaml.key");
    assert_eq!(code("? {a: 1}\n: x\n"), "yaml.key");
    assert_eq!(code("a: 1\na: 2\n"), "yaml.duplicate_key");
    assert_eq!(code("1: a\n'1': b\n"), "yaml.duplicate_key");
}

#[test]
fn nesting_has_a_limit() {
    let deep = "[".repeat(MAX_DEPTH + 1) + &"]".repeat(MAX_DEPTH + 1);
    assert_eq!(code(&deep), "yaml.depth");
    let ok = "[".repeat(MAX_DEPTH) + &"]".repeat(MAX_DEPTH);
    assert!(parse(&ok).is_ok());
    let via_alias = format!(
        "a: &a {}{}\nb: [[[*a]]]\n",
        "[".repeat(MAX_DEPTH - 2),
        "]".repeat(MAX_DEPTH - 2)
    );
    assert_eq!(code(&via_alias), "yaml.depth");
}

#[test]
fn stringify_writes_block_style() {
    assert_eq!(
        yaml(r#"{"name":"forge","n":3,"ok":true,"none":null,"list":[1,"two",{"a":1,"b":[]}],"map":{"x":{"y":"z"}},"empty":{}}"#),
        "name: forge\nn: 3\nok: true\nnone: null\nlist:\n  - 1\n  - two\n  - a: 1\n    b: []\nmap:\n  x:\n    y: z\nempty: {}\n"
    );
    assert_eq!(yaml("[[1,2],[3]]"), "- - 1\n  - 2\n- - 3\n");
    assert_eq!(yaml(r#""hi""#), "hi\n");
    assert_eq!(yaml("[]"), "[]\n");
    assert_eq!(yaml("null"), "null\n");
}

#[test]
fn stringify_quotes_only_when_needed() {
    assert_eq!(
        yaml(
            r##"["plain text","12","true","null","","~","1e3"," lead","trail ","a: b","a #b","#x","-x","- x","---","it's","x:","@a","0x1F","yes","2024-01-02","a,b"]"##
        ),
        concat!(
            "- plain text\n- '12'\n- 'true'\n- 'null'\n- ''\n- '~'\n- '1e3'\n- ' lead'\n",
            "- 'trail '\n- 'a: b'\n- 'a #b'\n- '#x'\n- -x\n- '- x'\n- '---'\n- it's\n- 'x:'\n",
            "- '@a'\n- '0x1F'\n- yes\n- 2024-01-02\n- a,b\n"
        )
    );
    assert_eq!(yaml(r#"{"a b":1,"1":2,"":3}"#), "a b: 1\n'1': 2\n'': 3\n");
}

#[test]
fn stringify_does_not_fold_long_lines() {
    let long = "word ".repeat(100).trim_end().to_string();
    let text = yaml(&serde_json::Value::String(long.clone()).to_string());
    assert_eq!(text, format!("{long}\n"));
}

#[test]
fn multi_line_strings_are_literal_blocks() {
    assert_eq!(yaml(r#"{"s":"a\nb\n"}"#), "s: |\n  a\n  b\n");
    assert_eq!(yaml(r#"{"s":"a\nb"}"#), "s: |-\n  a\n  b\n");
    assert_eq!(yaml(r#"{"s":"a\n\n"}"#), "s: |+\n  a\n\n");
    assert_eq!(yaml(r#"["  a\nb"]"#), "- |2-\n    a\n  b\n");
}

#[test]
fn numbers_read_back_as_the_same_number() {
    assert_eq!(
        yaml("[1e21,0.1,-2.5,1e-7,9007199254740994]"),
        "- 1e21\n- 0.1\n- -2.5\n- 1e-7\n- 9.007199254740994e15\n"
    );
    round_trip("[9007199254740994,1e21,0.1,-2.5,1e-7,123456.789,1.7976931348623157e308,5e-324]");
}

#[test]
fn strings_read_back_unchanged() {
    for s in [
        "a\nb",
        "a\n",
        "a\n\n\n",
        "\nlead",
        "  indented\nnext",
        "tab\there",
        "\ttab first\nx",
        "cr\r\nlf",
        "quote ' and \"",
        "back\\slash",
        "\u{0}nul",
        "\u{7f}del",
        "bom\u{feff}",
        "line\u{2028}sep",
        "nel\u{85}",
        "emoji 🎉",
        "  ",
        " ",
        "\n",
        "\n\n",
        "a\n \nb",
        "---",
        "...",
        "--- x",
        "a\n---\nb",
        "- a",
        "? x",
        ": x",
        "[a]",
        "{a}",
        "*a",
        "&a",
        "!a",
        "|",
        ">",
        "%x",
        "'",
        "\"",
        "a: ",
        "#",
        "x #",
        "=",
        "<<",
        ".",
        ".inf",
        "-.inf",
        ".nan",
        "+1",
        "0o8",
        "0x",
        "1_000",
        "1.",
        ".1e5",
        "1e",
        "e1",
        "Null",
        "NULL",
        "nULL",
        "TRUE",
        "tRUE",
        "ÿ\u{a0}nbsp",
        "trailing \nspace",
    ] {
        let value = serde_json::json!({ "k": s, "list": [s], s: 1 });
        round_trip(&value.to_string());
        round_trip(&serde_json::Value::String(s.to_string()).to_string());
    }
}

#[test]
fn a_very_long_key_is_written_as_an_explicit_key() {
    let key = "k".repeat(1500);
    let value = serde_json::json!({ key.clone(): { "a": [1] }, "b": 2 });
    let text = yaml(&value.to_string());
    assert!(text.contains("\n? \""), "{text}");
    round_trip(&value.to_string());
}

#[test]
fn stringify_round_trips_nested_shapes() {
    round_trip(
        r#"{"a":[[1,[2,{"x":[]}]],{"b":{"c":[{"d":"e\nf"},{}]}}],"g":[{"h":"i","j":[{"k":"l\n"}]}],"m":{}}"#,
    );
}
