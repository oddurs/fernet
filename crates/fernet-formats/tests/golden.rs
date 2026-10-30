//! Golden tests: every value reachable in the corpus matches an independent
//! reference parser, and every span slices back to its value.
//!
//! References: `serde_json` (with `arbitrary_precision`), `toml`, and
//! `serde_norway` (a maintained `serde_yaml`-compatible fork).

// Test helpers outside #[test] fns: failing loudly is the point.
#![allow(clippy::unwrap_used, clippy::panic, clippy::many_single_char_names)]

use std::path::{Path, PathBuf};

use fernet_fs_formats::{Driver, Json, NodeId, NodeKind, Registry, ScalarType, Toml, Tree, Yaml};

/// A format-neutral value for comparing against reference parsers.
#[derive(Debug, Clone, PartialEq)]
enum V {
    Map(Vec<(String, V)>),
    Seq(Vec<V>),
    Str(String),
    /// Numbers, booleans, nulls and datetimes, in a canonical text form.
    Atom(String),
}

fn corpus() -> Vec<PathBuf> {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/golden");
    let mut files: Vec<_> = std::fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap().path())
        .filter(|p| p.extension().is_some_and(|e| e != "md"))
        .collect();
    files.sort();
    files
}

// ---- JSON -----------------------------------------------------------------

/// Removes comments and trailing commas so `serde_json` can read JSONC.
/// Independent of the driver: a small state machine over the text.
fn strip_jsonc(s: &str) -> String {
    let b = s.trim_start_matches('\u{feff}').as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'"' => {
                let start = i;
                i += 1;
                while b[i] != b'"' {
                    i += if b[i] == b'\\' { 2 } else { 1 };
                }
                i += 1;
                out.extend_from_slice(&b[start..i]);
            }
            b'/' if b.get(i + 1) == Some(&b'/') => {
                while i < b.len() && b[i] != b'\n' {
                    i += 1;
                }
            }
            b'/' if b.get(i + 1) == Some(&b'*') => {
                i += 2;
                while &b[i..i + 2] != b"*/" {
                    i += 1;
                }
                i += 2;
            }
            c => {
                out.push(c);
                i += 1;
            }
        }
    }
    // Trailing commas: a ',' followed only by whitespace and a closer.
    let mut res: Vec<u8> = Vec::with_capacity(out.len());
    let mut in_str = false;
    let mut k = 0;
    while k < out.len() {
        let c = out[k];
        if in_str {
            res.push(c);
            if c == b'\\' {
                res.push(out[k + 1]);
                k += 1;
            } else if c == b'"' {
                in_str = false;
            }
        } else if c == b'"' {
            in_str = true;
            res.push(c);
        } else if c == b',' {
            let next = out[k + 1..].iter().find(|c| !c.is_ascii_whitespace());
            if !matches!(next, Some(b']' | b'}')) {
                res.push(c);
            }
        } else {
            res.push(c);
        }
        k += 1;
    }
    String::from_utf8(res).unwrap()
}

fn from_json(v: &serde_json::Value) -> V {
    match v {
        serde_json::Value::Object(m) => {
            V::Map(m.iter().map(|(k, v)| (k.clone(), from_json(v))).collect())
        }
        serde_json::Value::Array(a) => V::Seq(a.iter().map(from_json).collect()),
        serde_json::Value::String(s) => V::Str(s.clone()),
        other => V::Atom(other.to_string()),
    }
}

fn json_atom(text: &str) -> V {
    from_json(&serde_json::from_str(text).unwrap())
}

// ---- TOML -----------------------------------------------------------------

fn from_toml(v: &toml::Value) -> V {
    match v {
        toml::Value::Table(t) => V::Map(t.iter().map(|(k, v)| (k.clone(), from_toml(v))).collect()),
        toml::Value::Array(a) => V::Seq(a.iter().map(from_toml).collect()),
        toml::Value::String(s) => V::Str(s.clone()),
        toml::Value::Float(f) if f.is_nan() => V::Atom("nan".into()),
        toml::Value::Float(f) => V::Atom(format!("{f:?}")),
        other => V::Atom(other.to_string()),
    }
}

fn toml_atom(text: &str) -> V {
    let t: toml::Table = toml::from_str(&format!("v = {text}")).unwrap();
    from_toml(&t["v"])
}

// ---- YAML -----------------------------------------------------------------

fn yaml_key(k: &serde_norway::Value) -> Option<String> {
    match k {
        serde_norway::Value::String(s) => Some(s.clone()),
        serde_norway::Value::Number(n) => Some(n.to_string()),
        serde_norway::Value::Bool(b) => Some(b.to_string()),
        _ => None, // complex keys are left out by the driver
    }
}

fn from_yaml(v: &serde_norway::Value) -> V {
    match v {
        serde_norway::Value::Mapping(m) => V::Map(
            m.iter()
                .filter_map(|(k, v)| Some((yaml_key(k)?, from_yaml(v))))
                .collect(),
        ),
        serde_norway::Value::Sequence(a) => V::Seq(a.iter().map(from_yaml).collect()),
        serde_norway::Value::String(s) => V::Str(s.clone()),
        serde_norway::Value::Number(n) if n.as_f64().is_some_and(f64::is_nan) => {
            V::Atom("nan".into())
        }
        serde_norway::Value::Number(n) if n.is_f64() => {
            V::Atom(format!("{:?}", n.as_f64().unwrap()))
        }
        serde_norway::Value::Number(n) => V::Atom(n.to_string()),
        serde_norway::Value::Bool(b) => V::Atom(b.to_string()),
        serde_norway::Value::Null => V::Atom("null".into()),
        serde_norway::Value::Tagged(t) => from_yaml(&t.value),
    }
}

fn yaml_value(text: &str) -> V {
    let docs: Vec<serde_norway::Value> = serde_norway::Deserializer::from_str(text)
        .map(|d| serde::Deserialize::deserialize(d).unwrap())
        .collect();
    docs.first().map_or(V::Atom("null".into()), from_yaml)
}

// ---- comparison -----------------------------------------------------------

struct Format {
    driver: &'static dyn Driver,
    reference: fn(&str) -> V,
    /// Interprets a scalar's rendered text (non-strings) or a value span.
    atom: fn(&str) -> V,
    /// Decodes a key span.
    key: fn(&str) -> String,
}

fn atom_key(v: V) -> String {
    match v {
        V::Str(s) | V::Atom(s) => s,
        other => panic!("key {other:?}"),
    }
}

fn format_for(path: &Path) -> Format {
    match path.extension().unwrap().to_str().unwrap() {
        "json" => Format {
            driver: &Json,
            reference: |s| json_atom(&strip_jsonc(s)),
            atom: |s| json_atom(&strip_jsonc(s)),
            key: |s| atom_key(json_atom(s)),
        },
        "toml" => Format {
            driver: &Toml,
            reference: |s| from_toml(&toml::Value::Table(toml::from_str(s).unwrap())),
            atom: toml_atom,
            key: |s| {
                let t: toml::Table = toml::from_str(&format!("{s} = 1")).unwrap();
                t.keys().next().unwrap().clone()
            },
        },
        _ => Format {
            driver: &Yaml,
            reference: yaml_value,
            atom: yaml_value,
            key: |s| atom_key(yaml_value(s)),
        },
    }
}

/// Our tree as a [`V`], interpreting non-string scalars with `atom`.
fn ours(t: &Tree, id: NodeId, f: &Format, leaves: &mut usize) -> V {
    let n = t.node(id);
    match n.kind {
        NodeKind::Object => V::Map(
            n.children
                .iter()
                .map(|(k, c)| (k.clone(), ours(t, *c, f, leaves)))
                .collect(),
        ),
        NodeKind::Array => V::Seq(
            n.children
                .iter()
                .map(|(_, c)| ours(t, *c, f, leaves))
                .collect(),
        ),
        NodeKind::Scalar(ty) => {
            *leaves += 1;
            match ty {
                ScalarType::String | ScalarType::Text => V::Str(n.text.clone()),
                // An empty YAML null reads as an empty file.
                _ if n.text.is_empty() => V::Atom("null".into()),
                _ => (f.atom)(&n.text),
            }
        }
    }
}

/// YAML keys and maps compare as sets: duplicates and ordering already
/// collapse in the reference.
fn normalize(v: V) -> V {
    match v {
        V::Map(mut m) => {
            m.sort_by(|a, b| a.0.cmp(&b.0));
            V::Map(m.into_iter().map(|(k, v)| (k, normalize(v))).collect())
        }
        V::Seq(s) => V::Seq(s.into_iter().map(normalize).collect()),
        other => other,
    }
}

#[test]
fn leaves_match_reference_parsers() {
    let mut total = 0;
    for path in corpus() {
        let src = std::fs::read(&path).unwrap();
        let text = std::str::from_utf8(&src).unwrap();
        let f = format_for(&path);
        let tree = f
            .driver
            .parse(&src)
            .unwrap_or_else(|e| panic!("{}: {e}", path.display()));
        let mut leaves = 0;
        let got = normalize(ours(&tree, tree.root, &f, &mut leaves));
        let want = normalize((f.reference)(text));
        assert_eq!(got, want, "{}", path.display());
        assert!(leaves > 0);
        total += leaves;
    }
    assert!(total > 200, "corpus too small: {total} leaves");
}

#[test]
fn value_spans_slice_back_to_values() {
    for path in corpus() {
        let src = std::fs::read(&path).unwrap();
        let text = std::str::from_utf8(&src).unwrap();
        let f = format_for(&path);
        let tree = f.driver.parse(&src).unwrap();
        for (i, n) in tree.nodes.iter().enumerate() {
            let ctx = || {
                format!(
                    "{} node {i} {:?}",
                    path.display(),
                    &text[n.value_span.clone()]
                )
            };
            assert!(
                n.span.start <= n.value_span.start && n.value_span.end <= n.span.end
                    || matches!(n.kind, NodeKind::Object),
                "{}",
                ctx()
            );
            let NodeKind::Scalar(ty) = n.kind else {
                continue;
            };
            let slice = &text[n.value_span.clone()];
            let mut leaves = 0;
            let want = match ty {
                ScalarType::String => V::Str(n.text.clone()),
                _ if n.text.is_empty() => V::Atom("null".into()),
                _ => ours(&tree, NodeId(i), &f, &mut leaves),
            };
            let got = match ((f.atom)(slice), ty) {
                // A plain YAML `!!str 12` slice reads as a number without its tag.
                (V::Atom(a), ScalarType::String) => V::Str(a),
                (v, _) => v,
            };
            assert_eq!(got, want, "{}", ctx());
            if let Some(ks) = &n.key_span {
                let key_slice = &text[ks.clone()];
                let key = (f.key)(key_slice);
                assert!(
                    tree.nodes
                        .iter()
                        .any(|p| p.children.iter().any(|(k, c)| *k == key && c.0 == i)),
                    "{}: key {key:?}",
                    ctx()
                );
            }
        }
    }
}

#[test]
fn registry_detects_corpus() {
    let r = Registry::default();
    for path in corpus() {
        let head = std::fs::read(&path).unwrap();
        let d = r.detect(&path, &head[..head.len().min(4096)]).unwrap();
        assert_eq!(d.name(), format_for(&path).driver.name());
    }
}
