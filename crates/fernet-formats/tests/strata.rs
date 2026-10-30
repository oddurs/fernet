//! Milestone 5 drivers: golden reads, reference comparisons, and the splice
//! invariant for the writable ones.

#![allow(
    clippy::unwrap_used,
    clippy::panic,
    clippy::format_push_string,
    clippy::needless_raw_string_hashes,
    clippy::single_match_else
)] // Test helpers.

use std::collections::BTreeMap;
use std::path::Path;

use fernet_fs_formats::{
    Csv, Driver, Edit, Markdown, NodeId, NodeKind, Registry, ScalarType, Tree,
};
use proptest::prelude::*;

fn fixture(name: &str) -> Vec<u8> {
    std::fs::read(
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/strata")
            .join(name),
    )
    .unwrap()
}

/// The tree as indented lines: `key/` for directories, `key = text` for
/// values.
fn outline(t: &Tree, id: NodeId, depth: usize, out: &mut String) {
    for (k, c) in &t.node(id).children {
        let n = t.node(*c);
        let pad = "  ".repeat(depth);
        match n.kind {
            NodeKind::Scalar(_) => out.push_str(&format!("{pad}{k} = {:?}\n", n.text)),
            _ => {
                out.push_str(&format!("{pad}{k}/\n"));
                outline(t, *c, depth + 1, out);
            }
        }
    }
}

#[test]
fn markdown_golden() {
    let t = Markdown.parse(&fixture("README.md")).unwrap();
    let mut got = String::new();
    outline(&t, t.root, 0, &mut got);
    let want = r##"body = ""
fernet/
  body = "\nevery file has depth, every moment is kept.\n"
  Install/
    body = "\n```sh\ncargo install fernet-fs\n# not a heading\n```\n"
  Usage/
    body = "\nRead a value:\n\n    fernet cat config.json#/server/port\n"
    Time/
      body = "\nUse `@`.\n"
    Depth/
      body = "\nUse `#`.\n"
  Setext heading/
    body = "\nText under setext.\n"
  Usage (2)/
    body = "\nA second Usage section."
"##;
    assert_eq!(got, want);
}

#[test]
fn csv_matches_the_csv_crate() {
    for name in ["people.csv", "crlf.csv"] {
        let src = fixture(name);
        let t = Csv.parse(&src).unwrap();
        let mut rdr = csv::Reader::from_reader(&src[..]);
        let headers: Vec<String> = rdr.headers().unwrap().iter().map(str::to_owned).collect();
        let rows: Vec<csv::StringRecord> = rdr.records().map(Result::unwrap).collect();
        assert_eq!(t.node(t.root).children.len(), rows.len(), "{name}");
        for (i, row) in rows.iter().enumerate() {
            for (h, v) in headers.iter().zip(row.iter()) {
                let id = t.get(&[i.to_string(), h.clone()]).unwrap();
                assert_eq!(t.node(id).text, v, "{name} row {i} {h}");
            }
        }
    }
}

#[test]
fn registry_picks_the_new_drivers() {
    let r = Registry::default();
    let pick = |p: &str, head: &[u8]| r.detect(Path::new(p), head).map(Driver::name);
    assert_eq!(pick("README.md", b"# x"), Some("markdown"));
    assert_eq!(pick("data.csv", b"a,b"), Some("csv"));
    assert_eq!(pick("app.db", b""), Some("sqlite"));
    assert_eq!(pick("blob", b"SQLite format 3\0rest"), Some("sqlite"));
    assert_eq!(pick("bundle", b"PK\x03\x04\0\0"), Some("zip"));
    let mut tar = vec![0u8; 512];
    tar[257..262].copy_from_slice(b"ustar");
    assert_eq!(pick("backup", &tar), Some("tar"));
    assert_eq!(pick("notes.txt", b"# hi"), None);
}

/// Every leaf as (pointer, text).
fn leaves(t: &Tree) -> BTreeMap<String, String> {
    fn walk(t: &Tree, id: NodeId, p: &str, out: &mut BTreeMap<String, String>) {
        let n = t.node(id);
        if let NodeKind::Scalar(_) = n.kind {
            out.insert(p.to_owned(), n.text.clone());
        }
        for (k, c) in &n.children {
            walk(t, *c, &format!("{p}/{k}"), out);
        }
    }
    let mut out = BTreeMap::new();
    walk(t, t.root, "", &mut out);
    out
}

fn ptr(p: &str) -> Vec<String> {
    p.split('/')
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
        .collect()
}

/// The splice invariant for one edit, as in `splice.rs`.
fn check(d: &dyn Driver, src: &str, pick: usize, text: &str) -> Result<(), TestCaseError> {
    let tree = d.parse(src.as_bytes()).unwrap();
    let before = leaves(&tree);
    let Some(p) = before.keys().nth(pick % before.len().max(1)).cloned() else {
        return Ok(());
    };
    let id = tree.get(&ptr(&p)).unwrap();
    let span = tree.node(id).value_span.clone();
    let Ok(out) = d.splice(src.as_bytes(), &ptr(&p), Edit::SetScalar(text.to_owned())) else {
        // Markdown refuses bodies that would change the section structure.
        prop_assert_eq!(d.name(), "markdown");
        return Ok(());
    };
    let tail = src.len() - span.end;
    prop_assert_eq!(&out[..span.start], &src.as_bytes()[..span.start]);
    prop_assert_eq!(&out[out.len() - tail..], &src.as_bytes()[span.end..]);
    let mut after = leaves(&d.parse(&out).unwrap());
    let edited = after.remove(&p).unwrap();
    prop_assert_eq!(edited, text.strip_suffix('\n').unwrap_or(text));
    let mut before = before;
    before.remove(&p);
    prop_assert_eq!(after, before);
    Ok(())
}

fn markdown_doc() -> impl Strategy<Value = String> {
    proptest::collection::vec(
        (1usize..4, "[A-Za-z][A-Za-z ]{0,8}", "[a-z .,`*]{0,20}"),
        0..6,
    )
    .prop_map(|secs| {
        let mut s = String::from("Intro.\n");
        for (level, title, body) in secs {
            s.push_str(&format!(
                "\n{} {}\n\n{body}\n",
                "#".repeat(level),
                title.trim_end()
            ));
        }
        s
    })
}

fn csv_doc() -> impl Strategy<Value = String> {
    (
        1usize..4,
        proptest::collection::vec(proptest::collection::vec("[a-z0-9 ,\"]{0,6}", 3), 0..5),
    )
        .prop_map(|(cols, rows)| {
            let q = |v: &str| {
                if v.contains([',', '"']) || v.trim() != v {
                    format!("\"{}\"", v.replace('"', "\"\""))
                } else {
                    v.to_owned()
                }
            };
            let mut s = (0..cols)
                .map(|i| format!("c{i}"))
                .collect::<Vec<_>>()
                .join(",");
            s.push('\n');
            for r in rows {
                s.push_str(&r[..cols].iter().map(|v| q(v)).collect::<Vec<_>>().join(","));
                s.push('\n');
            }
            s
        })
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(512))]

    #[test]
    fn markdown_splice_invariant(doc in markdown_doc(), pick in any::<usize>(), text in "[a-z .,\n#=-]{0,30}") {
        check(&Markdown, &doc, pick, &text)?;
    }

    #[test]
    fn csv_splice_invariant(doc in csv_doc(), pick in any::<usize>(), text in "\\PC{0,8}|[,\"\n\r ]{0,4}") {
        check(&Csv, &doc, pick, &text)?;
    }
}

#[test]
fn markdown_bodies_are_text() {
    let t = Markdown.parse(b"# a\n\nx\n").unwrap();
    let id = t.get(&["a".into(), "body".into()]).unwrap();
    assert_eq!(t.node(id).kind, NodeKind::Scalar(ScalarType::Text));
}
