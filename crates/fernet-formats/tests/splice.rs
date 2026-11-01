//! Writes: type-preserving scalar splices (invariant 4), inserts that match
//! their siblings' style, and removes.

#![allow(
    clippy::unwrap_used,
    clippy::panic,
    clippy::many_single_char_names,
    clippy::format_push_string,
    clippy::cast_possible_wrap,
    clippy::cast_precision_loss,
    clippy::manual_is_multiple_of
)] // Generators and test helpers.

use std::collections::BTreeMap;

use fernet_fs_formats::{
    Driver, DriverError, Edit, Json, NewValue, NodeId, NodeKind, ScalarType, Toml, Tree, Yaml,
};
use proptest::prelude::*;

fn ptr(p: &str) -> Vec<String> {
    p.split('/')
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
        .collect()
}

fn set(d: &dyn Driver, src: &str, p: &str, text: &str) -> Result<String, DriverError> {
    d.splice(src.as_bytes(), &ptr(p), Edit::SetScalar(text.into()))
        .map(|b| String::from_utf8(b).unwrap())
}

fn ins(d: &dyn Driver, src: &str, p: &str, key: &str, v: NewValue) -> Result<String, DriverError> {
    d.splice(
        src.as_bytes(),
        &ptr(p),
        Edit::Insert {
            key: key.into(),
            value: v,
        },
    )
    .map(|b| String::from_utf8(b).unwrap())
}

fn rm(d: &dyn Driver, src: &str, p: &str) -> Result<String, DriverError> {
    d.splice(src.as_bytes(), &ptr(p), Edit::Remove)
        .map(|b| String::from_utf8(b).unwrap())
}

fn text(t: &str) -> NewValue {
    NewValue::Text(t.into())
}

// ---- scalar writes ------------------------------------------------------

#[test]
fn json_scalars() {
    let src = "{\n  // keep me\n  \"port\": 8080, \"name\": \"api\", \"on\": true, \"x\": null, \"f\": 1.5\n}\n";
    assert_eq!(
        set(&Json, src, "port", "9090\n").unwrap(),
        src.replace("8080", "9090")
    );
    assert_eq!(
        set(&Json, src, "name", "a \"b\"\n\tc\\").unwrap(),
        src.replace("\"api\"", r#""a \"b\"\n\tc\\""#)
    );
    assert_eq!(
        set(&Json, src, "on", "false").unwrap(),
        src.replace("true", "false")
    );
    assert_eq!(
        set(&Json, src, "f", "2e3").unwrap(),
        src.replace("1.5", "2e3")
    );
    assert_eq!(set(&Json, src, "f", "2").unwrap(), src.replace("1.5", "2"));
    for (p, bad) in [
        ("port", "nope"),
        ("port", "08"),
        ("port", "1."),
        ("on", "yes"),
        ("x", "nil"),
        ("on", "True"),
    ] {
        assert!(
            matches!(set(&Json, src, p, bad), Err(DriverError::InvalidValue(_))),
            "{p}={bad}"
        );
    }
    assert!(matches!(
        set(&Json, src, "", "x"),
        Err(DriverError::WrongKind(_))
    ));
    assert!(matches!(
        set(&Json, src, "nope", "x"),
        Err(DriverError::NotFound)
    ));
}

#[test]
fn toml_scalars_keep_their_quoting() {
    let src = "a = \"basic\"\nb = 'literal'\nc = '''\nmulti\n'''\nn = 0x1F\nf = 1.0\nd = 2026-10-01\nt = true\n";
    assert_eq!(
        set(&Toml, src, "a", "new \"q\"").unwrap(),
        src.replace("\"basic\"", r#""new \"q\"""#)
    );
    assert_eq!(
        set(&Toml, src, "b", "C:\\path").unwrap(),
        src.replace("'literal'", "'C:\\path'")
    );
    // A literal string can't hold a quote: it becomes basic.
    assert_eq!(
        set(&Toml, src, "b", "it's").unwrap(),
        src.replace("'literal'", "\"it's\"")
    );
    assert_eq!(
        set(&Toml, src, "c", "two\nlines").unwrap(),
        src.replace("'''\nmulti\n'''", "'''\ntwo\nlines'''")
    );
    assert_eq!(
        set(&Toml, src, "n", "0o17").unwrap(),
        src.replace("0x1F", "0o17")
    );
    assert_eq!(
        set(&Toml, src, "f", "6.5e-1").unwrap(),
        src.replace("1.0", "6.5e-1")
    );
    assert_eq!(
        set(&Toml, src, "d", "2027-01-02T03:04:05Z").unwrap(),
        src.replace("2026-10-01", "2027-01-02T03:04:05Z")
    );
    for (p, bad) in [
        ("n", "1.5"),
        ("f", "2"),
        ("d", "tomorrow"),
        ("t", "yes"),
        ("n", "\"7\""),
    ] {
        assert!(
            matches!(set(&Toml, src, p, bad), Err(DriverError::InvalidValue(_))),
            "{p}={bad}"
        );
    }
}

#[test]
fn yaml_scalars() {
    let src = "a: plain\nb: 'single'\nc: \"double\"\nn: 8080\nlist: [x, y]\nblock: |\n  text\nanch: &x 1\nali: *x\nnested:\n  k: v # comment\n";
    assert_eq!(
        set(&Yaml, src, "a", "still plain").unwrap(),
        src.replace("a: plain", "a: still plain")
    );
    // Text the core schema would read as another type gets quoted.
    assert_eq!(
        set(&Yaml, src, "a", "true").unwrap(),
        src.replace("a: plain", "a: \"true\"")
    );
    assert_eq!(
        set(&Yaml, src, "a", "x: y").unwrap(),
        src.replace("a: plain", "a: \"x: y\"")
    );
    assert_eq!(
        set(&Yaml, src, "b", "it's").unwrap(),
        src.replace("'single'", "'it''s'")
    );
    assert_eq!(
        set(&Yaml, src, "c", "tab\there").unwrap(),
        src.replace("\"double\"", "\"tab\\there\"")
    );
    assert_eq!(
        set(&Yaml, src, "n", "0x1F").unwrap(),
        src.replace("8080", "0x1F")
    );
    assert_eq!(
        set(&Yaml, src, "list/1", "z, w").unwrap(),
        src.replace("[x, y]", "[x, \"z, w\"]")
    );
    assert_eq!(
        set(&Yaml, src, "nested/k", "w").unwrap(),
        src.replace("k: v #", "k: w #")
    );
    assert!(matches!(
        set(&Yaml, src, "n", "eighty"),
        Err(DriverError::InvalidValue(_))
    ));
    assert!(matches!(
        set(&Yaml, src, "block", "x"),
        Err(DriverError::Unsupported(_))
    ));
    assert!(matches!(
        set(&Yaml, src, "anch", "2"),
        Err(DriverError::Unsupported(_))
    ));
    assert!(matches!(
        set(&Yaml, src, "ali", "2"),
        Err(DriverError::Unsupported(_))
    ));
    assert!(matches!(
        ins(&Yaml, src, "", "new", text("1")),
        Err(DriverError::Unsupported(_))
    ));
    assert!(matches!(
        rm(&Yaml, src, "a"),
        Err(DriverError::Unsupported(_))
    ));
}

// ---- inserts and removes (golden) ---------------------------------------

#[test]
fn json_inserts_match_siblings() {
    let cases: &[(&str, &str, &str, NewValue, &str)] = &[
        // One per line, two-space indent.
        (
            "{\n  \"a\": 1,\n  \"b\": 2\n}\n",
            "",
            "c",
            text("3"),
            "{\n  \"a\": 1,\n  \"b\": 2,\n  \"c\": 3\n}\n",
        ),
        // Four-space indent, no space after the colon.
        (
            "{\n    \"a\":1\n}",
            "",
            "s",
            text("hi"),
            "{\n    \"a\":1,\n    \"s\":\"hi\"\n}",
        ),
        // Same line.
        (
            "{\"a\": 1, \"b\": 2}",
            "",
            "c",
            NewValue::EmptyObject,
            "{\"a\": 1, \"b\": 2, \"c\": {}}",
        ),
        // Trailing comma (JSONC) stays trailing.
        (
            "{\n  \"a\": 1,\n}",
            "",
            "b",
            text("true"),
            "{\n  \"a\": 1,\n  \"b\": true,\n}",
        ),
        // Nested, deeper indent.
        (
            "{\n  \"o\": {\n    \"x\": 1\n  }\n}",
            "o",
            "y",
            NewValue::EmptyArray,
            "{\n  \"o\": {\n    \"x\": 1,\n    \"y\": []\n  }\n}",
        ),
        // Arrays append.
        ("{\"l\": [1, 2]}", "l", "2", text("3"), "{\"l\": [1, 2, 3]}"),
        (
            "[\n  \"a\"\n]",
            "",
            "1",
            text("b"),
            "[\n  \"a\",\n  \"b\"\n]",
        ),
        // Empty containers.
        (
            "{\"o\": {}}",
            "o",
            "k",
            text("null"),
            "{\"o\": {\"k\": null}}",
        ),
        ("{\"l\": []}", "l", "0", text("x"), "{\"l\": [\"x\"]}"),
    ];
    for (src, at, key, v, want) in cases {
        assert_eq!(
            &ins(&Json, src, at, key, v.clone()).unwrap(),
            want,
            "{src:?} + {key}"
        );
    }
    assert!(matches!(
        ins(&Json, "{\"a\": 1}", "", "a", text("2")),
        Err(DriverError::Exists)
    ));
    assert!(matches!(
        ins(&Json, "[1]", "", "5", text("2")),
        Err(DriverError::InvalidValue(_))
    ));
}

#[test]
fn json_removes() {
    let cases = [
        (
            "{\n  \"a\": 1,\n  \"b\": 2,\n  \"c\": 3\n}",
            "b",
            "{\n  \"a\": 1,\n  \"c\": 3\n}",
        ),
        ("{\n  \"a\": 1,\n  \"b\": 2\n}", "b", "{\n  \"a\": 1\n}"),
        ("{\"a\": 1, \"b\": 2}", "a", "{\"b\": 2}"),
        ("{\"o\": {\"only\": 1}}", "o/only", "{\"o\": {}}"),
        ("[1, 2, 3]", "1", "[1, 3]"),
    ];
    for (src, p, want) in cases {
        assert_eq!(rm(&Json, src, p).unwrap(), want, "{src:?} - {p}");
    }
}

#[test]
fn toml_inserts_and_removes() {
    let src = "[server]\nport = 8080\nhost = \"a\"\n\n[db]\nurl = \"x\"\n";
    assert_eq!(
        ins(&Toml, src, "server", "tls", text("true")).unwrap(),
        "[server]\nport = 8080\nhost = \"a\"\ntls = true\n\n[db]\nurl = \"x\"\n"
    );
    assert_eq!(
        ins(&Toml, src, "db", "pool", text("small")).unwrap(),
        "[server]\nport = 8080\nhost = \"a\"\n\n[db]\nurl = \"x\"\npool = \"small\"\n"
    );
    assert_eq!(
        rm(&Toml, src, "server/host").unwrap(),
        "[server]\nport = 8080\n\n[db]\nurl = \"x\"\n"
    );
    assert_eq!(
        rm(&Toml, src, "db").unwrap(),
        "[server]\nport = 8080\nhost = \"a\"\n"
    );
    let arr = "l = [1, 2]\nt = { a = 1 }\n";
    assert_eq!(
        ins(&Toml, arr, "l", "2", text("3")).unwrap(),
        "l = [1, 2, 3]\nt = { a = 1 }\n"
    );
    assert_eq!(
        ins(&Toml, arr, "t", "b", text("2026-10-01")).unwrap(),
        "l = [1, 2]\nt = { a = 1, b = 2026-10-01 }\n"
    );
    assert_eq!(rm(&Toml, arr, "l/0").unwrap(), "l = [2]\nt = { a = 1 }\n");
    assert!(matches!(
        ins(&Toml, src, "server", "port", text("1")),
        Err(DriverError::Exists)
    ));
}

// ---- the splice invariant, property-tested ------------------------------

/// Every leaf as (pointer, type, text).
fn leaves(t: &Tree) -> BTreeMap<String, (NodeKind, String)> {
    fn walk(t: &Tree, id: NodeId, p: &str, out: &mut BTreeMap<String, (NodeKind, String)>) {
        let n = t.node(id);
        if let NodeKind::Scalar(_) = n.kind {
            out.insert(p.to_owned(), (n.kind, n.text.clone()));
        }
        for (k, c) in &n.children {
            walk(t, *c, &format!("{p}/{k}"), out);
        }
    }
    let mut out = BTreeMap::new();
    walk(t, t.root, "", &mut out);
    out
}

#[derive(Debug, Clone)]
enum Gen {
    Str(String),
    Int(i64),
    Float(f64),
    Bool(bool),
    Map(Vec<(String, Gen)>),
    List(Vec<Gen>),
}

fn gen_value() -> impl Strategy<Value = Gen> {
    let leaf = prop_oneof![
        "[a-zA-Z0-9 _.-]{0,10}".prop_map(Gen::Str),
        any::<i64>().prop_map(Gen::Int),
        (-1e9f64..1e9).prop_map(Gen::Float),
        any::<bool>().prop_map(Gen::Bool),
    ];
    leaf.prop_recursive(3, 24, 5, |inner| {
        prop_oneof![
            proptest::collection::vec(("[a-z][a-z0-9_]{0,6}", inner.clone()), 1..5).prop_map(
                |kv| {
                    let mut seen = std::collections::HashSet::new();
                    Gen::Map(
                        kv.into_iter()
                            .filter(|(k, _)| seen.insert(k.clone()))
                            .collect(),
                    )
                }
            ),
            proptest::collection::vec(inner, 1..4).prop_map(Gen::List),
        ]
    })
}

fn root_map() -> impl Strategy<Value = Vec<(String, Gen)>> {
    proptest::collection::vec(("[a-z][a-z0-9_]{0,6}", gen_value()), 1..6).prop_map(|kv| {
        let mut seen = std::collections::HashSet::new();
        kv.into_iter()
            .filter(|(k, _)| seen.insert(k.clone()))
            .collect()
    })
}

fn json_str(s: &str) -> String {
    serde_json::to_string(s).unwrap()
}

fn to_json(v: &Gen, indent: usize, depth: usize) -> String {
    let pad = |d: usize| {
        if indent == 0 {
            String::new()
        } else {
            format!("\n{}", " ".repeat(indent * d))
        }
    };
    match v {
        Gen::Str(s) => json_str(s),
        Gen::Int(i) => i.to_string(),
        Gen::Float(f) => format!("{f:?}"),
        Gen::Bool(b) => b.to_string(),
        Gen::Map(kv) => {
            let items: Vec<String> = kv
                .iter()
                .map(|(k, v)| {
                    format!(
                        "{}{}: {}",
                        pad(depth + 1),
                        json_str(k),
                        to_json(v, indent, depth + 1)
                    )
                })
                .collect();
            format!("{{{}{}}}", items.join(","), pad(depth))
        }
        Gen::List(l) => {
            let items: Vec<String> = l
                .iter()
                .map(|v| format!("{}{}", pad(depth + 1), to_json(v, indent, depth + 1)))
                .collect();
            format!("[{}{}]", items.join(","), pad(depth))
        }
    }
}

fn to_yaml(v: &Gen, depth: usize) -> String {
    let pad = "  ".repeat(depth);
    match v {
        Gen::Str(s) => format!(" '{}'\n", s.replace('\'', "''")),
        Gen::Int(i) => format!(" {i}\n"),
        Gen::Float(f) => format!(" {f:?}\n"),
        Gen::Bool(b) => format!(" {b}\n"),
        Gen::Map(kv) => {
            let mut s = String::from("\n");
            for (k, v) in kv {
                s.push_str(&format!("{pad}{k}:{}", to_yaml(v, depth + 1)));
            }
            s
        }
        Gen::List(l) => {
            let mut s = String::from("\n");
            for v in l {
                s.push_str(&format!("{pad}-{}", to_yaml(v, depth + 1)));
            }
            s
        }
    }
}

fn toml_inline(v: &Gen) -> String {
    match v {
        Gen::Str(s) => json_str(s),
        Gen::Int(i) => i.to_string(),
        Gen::Float(f) => format!("{f:?}"),
        Gen::Bool(b) => b.to_string(),
        Gen::Map(kv) => format!(
            "{{ {} }}",
            kv.iter()
                .map(|(k, v)| format!("{k} = {}", toml_inline(v)))
                .collect::<Vec<_>>()
                .join(", ")
        ),
        Gen::List(l) => format!(
            "[{}]",
            l.iter().map(toml_inline).collect::<Vec<_>>().join(", ")
        ),
    }
}

fn to_toml(root: &[(String, Gen)]) -> String {
    let mut top = String::new();
    let mut tables = String::new();
    for (k, v) in root {
        match v {
            Gen::Map(kv) => {
                tables.push_str(&format!("\n[{k}]\n"));
                for (k2, v2) in kv {
                    tables.push_str(&format!("{k2} = {}\n", toml_inline(v2)));
                }
            }
            _ => top.push_str(&format!("{k} = {}\n", toml_inline(v))),
        }
    }
    top + &tables
}

/// New text of a scalar's type.
fn new_text(ty: ScalarType, seed: u64, s: &str) -> String {
    match ty {
        ScalarType::String => s.to_owned(),
        ScalarType::Integer => (seed as i64).wrapping_sub(i64::MAX / 2).to_string(),
        ScalarType::Float => format!("{:?}", (seed as f64) / 7.0 - 1e6),
        ScalarType::Bool => (seed % 2 == 0).to_string(),
        _ => "null".into(),
    }
}

/// Splices a random leaf and checks invariant 4.
fn check_splice(
    d: &dyn Driver,
    src: &str,
    pick: usize,
    seed: u64,
    s: &str,
) -> Result<(), TestCaseError> {
    let tree = d
        .parse(src.as_bytes())
        .map_err(|e| TestCaseError::fail(format!("fixture doesn't parse: {e}\n{src}")))?;
    let before = leaves(&tree);
    let Some((p, (kind, _))) = before.iter().nth(pick % before.len().max(1)) else {
        return Ok(());
    };
    let NodeKind::Scalar(ty) = *kind else {
        return Ok(());
    };
    let id = tree.get(&ptr(p)).unwrap();
    let span = tree.node(id).value_span.clone();
    let input = new_text(ty, seed, s);
    let input = if seed % 3 == 0 {
        format!("{input}\n")
    } else {
        input
    };
    // Exactly one trailing newline is stripped from what's written.
    let text = input.strip_suffix('\n').unwrap_or(&input).to_owned();
    let out = d
        .splice(src.as_bytes(), &ptr(p), Edit::SetScalar(input))
        .map_err(|e| TestCaseError::fail(format!("{p} = {text:?}: {e}\n{src}")))?;
    // Bytes outside the value span are identical.
    let tail = src.len() - span.end;
    prop_assert_eq!(&out[..span.start], &src.as_bytes()[..span.start]);
    prop_assert_eq!(&out[out.len() - tail..], &src.as_bytes()[span.end..]);
    // It reparses; the edited value is the input; nothing else changed.
    let after_tree = d
        .parse(&out)
        .map_err(|e| TestCaseError::fail(format!("result doesn't parse: {e}")))?;
    let mut after = leaves(&after_tree);
    let (k, t) = after.remove(p).unwrap();
    prop_assert_eq!(t, text);
    let mut before = before.clone();
    let (k0, _) = before.remove(p).unwrap();
    // JSON numbers may switch between integer and float.
    if d.name() != "json" {
        prop_assert_eq!(k, k0);
    }
    prop_assert_eq!(after, before);
    Ok(())
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(512))]

    #[test]
    fn json_splice_invariant(root in root_map(), indent in 0usize..5, pick in any::<usize>(), seed in any::<u64>(), s in "\\PC{0,12}|[\"\\\\\n\t\u{0}-\u{1f}]{0,4}") {
        let src = to_json(&Gen::Map(root), indent, 0) + "\n";
        check_splice(&Json, &src, pick, seed, &s)?;
    }

    #[test]
    fn yaml_splice_invariant(root in root_map(), pick in any::<usize>(), seed in any::<u64>(), s in "\\PC{0,12}|[\"'\\\\\n\t:# \u{1}]{0,4}") {
        let src = to_yaml(&Gen::Map(root), 0).trim_start().to_owned();
        check_splice(&Yaml, &src, pick, seed, &s)?;
    }

    #[test]
    fn toml_splice_invariant(root in root_map(), pick in any::<usize>(), seed in any::<u64>(), s in "\\PC{0,12}|[\"'\\\\\n\t\u{1}]{0,4}") {
        let src = to_toml(&root);
        check_splice(&Toml, &src, pick, seed, &s)?;
    }

    /// A rejected write leaves nothing changed: splice returns an error and
    /// never partial output.
    #[test]
    fn rejected_writes_are_errors(root in root_map(), pick in any::<usize>(), junk in "[a-z ]{1,6}") {
        let src = to_json(&Gen::Map(root), 2, 0);
        let tree = Json.parse(src.as_bytes()).unwrap();
        let all = leaves(&tree);
        if let Some((p, (NodeKind::Scalar(ScalarType::Integer | ScalarType::Float | ScalarType::Bool), _))) = all.iter().nth(pick % all.len()) {
            let r = Json.splice(src.as_bytes(), &ptr(p), Edit::SetScalar(junk.clone()));
            prop_assert!(matches!(r, Err(DriverError::InvalidValue(_))), "{p} = {junk:?}: {r:?}");
        }
    }
}

/// Regressions from review.
#[test]
fn review_regressions() {
    use fernet_fs_formats::{Csv, Markdown};
    let s = |b: Vec<u8>| String::from_utf8(b).unwrap();
    // JSON: indentation is whitespace only, not the members before.
    assert_eq!(
        ins(&Json, "[\n  1, 2\n]", "", "2", text("3")).unwrap(),
        "[\n  1, 2,\n  3\n]"
    );
    assert_eq!(
        ins(&Json, "{\n  \"a\": [1, 2], \"b\": 3\n}", "", "d", text("4")).unwrap(),
        "{\n  \"a\": [1, 2], \"b\": 3,\n  \"d\": 4\n}"
    );
    // JSON: removing a duplicated key is refused, not half-done.
    assert!(rm(&Json, "{\"a\": 1, \"b\": 2, \"a\": 3}", "a").is_err());
    // TOML: CRLF stays CRLF; a reordering print is refused.
    assert_eq!(
        ins(&Toml, "a = 1\r\nb = 2\r\n", "", "c", text("3")).unwrap(),
        "a = 1\r\nb = 2\r\nc = 3\r\n"
    );
    assert!(ins(&Toml, "a.b = 1\nc = 2\na.d = 3\n", "", "e", text("4")).is_err());
    // YAML: a tag on the line above still makes the value read-only.
    assert!(set(&Yaml, "a: !!binary\n  aGVsbG8=\n", "a", "x").is_err());
    // CSV: a new row in a one-column file is a row.
    let out = s(Csv
        .splice(
            b"a\n1\n",
            &ptr(""),
            Edit::Insert {
                key: "1".into(),
                value: NewValue::EmptyObject,
            },
        )
        .unwrap());
    assert_eq!(
        Csv.parse(out.as_bytes())
            .unwrap()
            .node(Csv.parse(out.as_bytes()).unwrap().root)
            .children
            .len(),
        2
    );
    // Markdown: headings in list items keep their marker; read-back is idempotent.
    let md = "# A\nx\n\n- # B\nmore\n";
    let t = Markdown.parse(md.as_bytes()).unwrap();
    let body = &t.node(t.get(&ptr("A/body")).unwrap()).text;
    assert_eq!(
        set(&Markdown, md, "A/body", &format!("{body}\n")).unwrap(),
        md
    );
    assert_eq!(
        set(&Markdown, md, "A/body", "y").unwrap(),
        "# A\ny\n- # B\nmore\n"
    );
    // Markdown: removing a quoted section leaves the next heading alone.
    assert_eq!(
        rm(&Markdown, "> # A\n> text\n\n# B\n", "A").unwrap(),
        "# B\n"
    );
    // Markdown: mkdir goes one level below the parent heading.
    assert_eq!(
        ins(&Markdown, "## A\ntext\n", "A", "B", NewValue::EmptyObject).unwrap(),
        "## A\ntext\n\n### B\n"
    );
    assert!(ins(&Markdown, "###### A\n", "A", "B", NewValue::EmptyObject).is_err());
    // Markdown: keys or bodies that would make other sections are refused.
    assert!(ins(&Markdown, "# A\n", "A", "foo *bar*", NewValue::EmptyObject).is_err());
    assert!(ins(&Markdown, "# A\n", "A", "X", text("a\n# Y")).is_err());
    // Markdown: a body after a final heading with no newline.
    assert_eq!(
        set(&Markdown, "# A", "A/body", "hello").unwrap(),
        "# A\nhello"
    );
}

/// The value at `p` after parsing `src` with `d`.
fn value_at(d: &dyn Driver, src: &str, p: &str) -> Option<(NodeKind, String)> {
    let t = d.parse(src.as_bytes()).unwrap();
    t.get(&ptr(p)).map(|id| {
        let n = t.node(id);
        (n.kind, n.text.clone())
    })
}

/// Edge cases in the TOML driver's inserts and removes.
#[test]
fn toml_edge_case_regressions() {
    // Mixed line endings: untouched lines keep theirs, even inside a
    // multi-line string, and the string's value doesn't change.
    let src = "a = 1\r\ns = \"\"\"x\ny\"\"\"\n";
    let out = ins(&Toml, src, "", "n", text("2")).unwrap();
    assert_eq!(out, "a = 1\r\ns = \"\"\"x\ny\"\"\"\nn = 2\n");
    assert_eq!(value_at(&Toml, &out, "s").unwrap().1, "x\ny");
    let src = "a = 1\r\nb = 2\r\ns = \"\"\"x\ny\"\"\"\r\n";
    let out = ins(&Toml, src, "", "n", text("2")).unwrap();
    assert_eq!(out, format!("{src}n = 2\r\n"));
    assert_eq!(
        rm(&Toml, src, "b").unwrap(),
        "a = 1\r\ns = \"\"\"x\ny\"\"\"\r\n"
    );
    // New text is a TOML value only if it's exactly one; else a string.
    let src = "a = 1\n";
    for (t, kind, want) in [
        ("1\nc = 2", ScalarType::String, "1\nc = 2"),
        ("5 # five", ScalarType::String, "5 # five"),
        ("5", ScalarType::Integer, "5"),
        ("5\n", ScalarType::Integer, "5"),
        (" 5", ScalarType::String, " 5"),
        ("1979-05-27", ScalarType::Datetime, "1979-05-27"),
    ] {
        let out = ins(&Toml, src, "", "b", text(t)).unwrap();
        assert_eq!(
            value_at(&Toml, &out, "b"),
            Some((NodeKind::Scalar(kind), want.to_owned())),
            "{t:?} -> {out:?}"
        );
        assert!(value_at(&Toml, &out, "c").is_none(), "{t:?}");
        assert!(out.starts_with(src));
    }
    // Duplicate lines aren't a reorder.
    let src = "[server]\nenabled = true\n[client]\nport = 1\n";
    assert_eq!(
        ins(&Toml, src, "client", "enabled", text("true")).unwrap(),
        format!("{src}enabled = true\n")
    );
    let src = "[[p]]\nn = 1\n[[p]]\nn = 2\n";
    assert_eq!(
        ins(&Toml, src, "p", "2", NewValue::EmptyObject).unwrap(),
        format!("{src}\n[[p]]\n")
    );
    assert_eq!(rm(&Toml, src, "p/0").unwrap(), "[[p]]\nn = 2\n");
    assert_eq!(rm(&Toml, src, "p/1").unwrap(), "[[p]]\nn = 1\n");
    // ... but real reorders are still refused.
    assert!(ins(&Toml, "a.b = 1\nc = 2\na.d = 3\n", "", "e", text("4")).is_err());
    // Arrays with per-element comments can't keep them on the right element.
    let src = "arr = [\n  1, # one\n  2, # two\n  3, # three\n]\n";
    assert!(matches!(
        rm(&Toml, src, "arr/1"),
        Err(DriverError::Unsupported(_))
    ));
    assert!(matches!(
        ins(&Toml, src, "arr", "3", text("4")),
        Err(DriverError::Unsupported(_))
    ));
    assert!(matches!(
        rm(&Toml, src, "arr/9"),
        Err(DriverError::NotFound)
    ));
    // Without comments they still work.
    assert_eq!(
        rm(&Toml, "arr = [\n  1,\n  2,\n  3,\n]\n", "arr/1").unwrap(),
        "arr = [\n  1,\n  3,\n]\n"
    );
    // The only table of an array of tables: refused, not the key deleted.
    assert!(matches!(
        rm(&Toml, "x = 1\n[[p]]\nn = 1\n", "p/0"),
        Err(DriverError::Unsupported(_))
    ));
    assert_eq!(rm(&Toml, "x = 1\n[[p]]\nn = 1\n", "p").unwrap(), "x = 1\n");
}

/// Edge cases in the Markdown driver.
#[test]
fn markdown_edge_case_regressions() {
    use fernet_fs_formats::Markdown;
    let keys = |src: &str| {
        let t = Markdown.parse(src.as_bytes()).unwrap();
        t.node(t.root)
            .children
            .iter()
            .map(|(k, _)| k.clone())
            .collect::<Vec<_>>()
    };
    // Emptying a body that holds a link definition a heading uses is refused.
    let src = "# A\n[foo]: /url\n\n# [foo]\ntext\n";
    assert_eq!(keys(src), ["body", "A", "foo"]);
    assert!(matches!(
        rm(&Markdown, src, "A/body"),
        Err(DriverError::InvalidValue(_))
    ));
    // Emptying an ordinary body still works.
    assert_eq!(
        rm(&Markdown, "# A\nx\n\n# B\n", "A/body").unwrap(),
        "# A\n\n# B\n"
    );
    // An empty body written back as read is byte-identical.
    for (src, p) in [
        ("# A\n# B\n", "A/body"),
        ("# A\n# B\n", "body"),
        ("# A", "A/body"),
        ("# A\n", "A/body"),
        ("# A\n\n# B\n", "A/body"),
    ] {
        let (_, read) = value_at(&Markdown, src, p).unwrap();
        assert_eq!(
            set(&Markdown, src, p, &format!("{read}\n")).unwrap(),
            src,
            "{src:?} {p}"
        );
        assert_eq!(set(&Markdown, src, p, &read).unwrap(), src, "{src:?} {p}");
    }
    assert_eq!(
        set(&Markdown, "# A\n# B\n", "A/body", "x").unwrap(),
        "# A\nx\n# B\n"
    );
    // YAML front matter is part of the root body, not a heading.
    let src = "---\ntitle: Hello\n---\n# Intro\nhi\n";
    assert_eq!(keys(src), ["body", "Intro"]);
    assert_eq!(
        value_at(&Markdown, src, "body").unwrap().1,
        "---\ntitle: Hello\n---"
    );
    assert_eq!(
        set(&Markdown, src, "body", "---\ntitle: Bye\n---").unwrap(),
        "---\ntitle: Bye\n---\n# Intro\nhi\n"
    );
    // A multi-line setext heading keeps its line break as a space.
    assert_eq!(keys("Foo\nbar\n===\n"), ["body", "Foo bar"]);
}

/// Edge cases in the YAML driver.
#[test]
fn yaml_edge_case_regressions() {
    // An empty value can be written, after its colon; a null takes any type.
    let src = "name:\nport: 80\n";
    assert_eq!(set(&Yaml, src, "name", "5").unwrap(), "name: 5\nport: 80\n");
    assert_eq!(
        set(&Yaml, src, "name", "api").unwrap(),
        "name: api\nport: 80\n"
    );
    assert_eq!(
        set(&Yaml, src, "name", "true").unwrap(),
        "name: true\nport: 80\n"
    );
    assert_eq!(
        set(&Yaml, src, "name", "a: b").unwrap(),
        "name: \"a: b\"\nport: 80\n"
    );
    assert_eq!(set(&Yaml, src, "name", "").unwrap(), src);
    assert_eq!(set(&Yaml, "a: \nb: 1\n", "a", "x").unwrap(), "a: x\nb: 1\n");
    assert_eq!(
        set(&Yaml, "a: # note\nb: 1\n", "a", "x").unwrap(),
        "a: x # note\nb: 1\n"
    );
    assert_eq!(
        set(&Yaml, "\"q\":\nb: 1\n", "q", "1").unwrap(),
        "\"q\": 1\nb: 1\n"
    );
    assert_eq!(
        set(&Yaml, "{a: , b: 1}\n", "a", "2").unwrap(),
        "{a: 2, b: 1}\n"
    );
    assert_eq!(set(&Yaml, "a: ~\n", "a", "7").unwrap(), "a: 7\n");
    // ... but other types still keep theirs.
    assert!(matches!(
        set(&Yaml, src, "port", "x"),
        Err(DriverError::InvalidValue(_))
    ));
    // `!` or `&` in a comment isn't a tag or anchor.
    assert_eq!(
        set(&Yaml, "a: # see !this & that\n  1\n", "a", "2").unwrap(),
        "a: # see !this & that\n  2\n"
    );
    assert_eq!(
        set(&Yaml, "a: 1 # !x\nb: 2\n", "b", "3").unwrap(),
        "a: 1 # !x\nb: 3\n"
    );
    // Real properties are still read-only, comments or not.
    assert!(set(&Yaml, "a: !!str # c\n  x\n", "a", "y").is_err());
    assert!(set(&Yaml, "a: &x # c\n  1\n", "a", "2").is_err());
}
