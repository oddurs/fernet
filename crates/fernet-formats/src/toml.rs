//! TOML, via `toml_edit`'s span-preserving parser (`docs/DECISIONS.md`, entry 12).

use toml_edit::{Item, Table, Value};

use crate::{
    Builder, Caps, Driver, DriverError, Edit, MAX_DEPTH, NodeId, NodeKind, ScalarType, Tree,
};

/// The TOML driver.
#[derive(Clone, Copy, Debug, Default)]
pub struct Toml;

impl Driver for Toml {
    fn name(&self) -> &'static str {
        "toml"
    }

    fn extensions(&self) -> &'static [&'static str] {
        &["toml"]
    }

    fn sniff(&self, head: &[u8]) -> bool {
        let s = String::from_utf8_lossy(head);
        let Some(line) = crate::first_line(&s, "#") else {
            return false;
        };
        let header = |l: &str| {
            let inner = l
                .strip_prefix("[[")
                .and_then(|l| l.strip_suffix("]]"))
                .or_else(|| l.strip_prefix('[').and_then(|l| l.strip_suffix(']')));
            inner.is_some_and(|i| {
                !i.is_empty()
                    && i.chars()
                        .all(|c| c.is_ascii_alphanumeric() || "_-. \"'".contains(c))
            })
        };
        let assignment = |l: &str| {
            l.split_once('=').is_some_and(|(k, _)| {
                let k = k.trim();
                !k.is_empty()
                    && k.chars()
                        .all(|c| c.is_ascii_alphanumeric() || "_-.".contains(c))
            })
        };
        header(line) || assignment(line)
    }

    fn parse(&self, src: &[u8]) -> Result<Tree, DriverError> {
        let (s, start) = crate::text(src)?;
        // toml_edit rejects a BOM; parse after it and shift spans back.
        let body = &s[start..];
        let doc = toml_edit::Document::parse(body).map_err(|e| {
            let at = e.span().map_or(0, |r| r.start);
            DriverError::syntax(s, start + at, first_line_lower(e.message()))
        })?;
        let mut w = Walker {
            b: Builder::default(),
            src: s,
            shift: start,
            depth: 0,
        };
        let root = w.table(doc.as_table(), 0..s.len())?;
        Ok(w.b.finish(root))
    }

    fn splice(&self, _: &[u8], _: &[String], _: Edit) -> Result<Vec<u8>, DriverError> {
        Err(DriverError::Unsupported("toml writes arrive in v0.4"))
    }

    fn capabilities(&self) -> Caps {
        Caps::READ_ONLY
    }
}

fn first_line_lower(m: &str) -> String {
    m.lines().next().unwrap_or("invalid toml").to_lowercase()
}

struct Walker<'s> {
    b: Builder,
    src: &'s str,
    shift: usize,
    depth: usize,
}

impl Walker<'_> {
    fn shifted(&self, r: Option<std::ops::Range<usize>>) -> Option<std::ops::Range<usize>> {
        r.map(|r| r.start + self.shift..r.end + self.shift)
    }

    /// A value's source text (`0x1F`, `1_000`, `1979-05-27 07:32:00Z` stay as
    /// written).
    fn raw(&self, span: &std::ops::Range<usize>) -> String {
        self.src.get(span.clone()).unwrap_or_default().to_owned()
    }

    fn enter(&mut self) -> Result<(), DriverError> {
        self.depth += 1;
        if self.depth > MAX_DEPTH {
            Err(DriverError::TooDeep)
        } else {
            Ok(())
        }
    }

    /// A table. `fallback` is used as its span when it has none (implicit
    /// tables like `a` in `[a.b]`).
    fn table(
        &mut self,
        t: &Table,
        fallback: std::ops::Range<usize>,
    ) -> Result<NodeId, DriverError> {
        self.enter()?;
        let span = self.shifted(t.span()).unwrap_or(fallback);
        let id = self.b.push(NodeKind::Object, span, String::new());
        for (k, item) in t {
            let key_span = t.key(k).and_then(|key| self.shifted(key.span()));
            let fallback = key_span.clone().unwrap_or(0..0);
            let child = match item {
                Item::None => continue,
                Item::Value(v) => self.value(v)?,
                Item::Table(t) => self.table(t, fallback)?,
                Item::ArrayOfTables(a) => {
                    self.enter()?;
                    let arr = self
                        .b
                        .push(NodeKind::Array, fallback.clone(), String::new());
                    for t in a {
                        let child = self.table(t, fallback.clone())?;
                        self.b.push_item(arr, child);
                    }
                    self.depth -= 1;
                    arr
                }
            };
            self.keyed(child, key_span);
            self.b.add_child(id, k.to_owned(), child);
        }
        self.depth -= 1;
        Ok(id)
    }

    fn keyed(&mut self, child: NodeId, key_span: Option<std::ops::Range<usize>>) {
        let node = self.b.node_mut(child);
        if let Some(ks) = &key_span {
            // Values span key..value; table headers already cover their key.
            if ks.start < node.value_span.start {
                node.span = ks.start..node.value_span.end;
            }
        }
        node.key_span = key_span;
    }

    fn value(&mut self, v: &Value) -> Result<NodeId, DriverError> {
        let span = self.shifted(v.span()).unwrap_or(0..0);
        let scalar = |ty, text: String| (NodeKind::Scalar(ty), text);
        let (kind, text) = match v {
            Value::String(s) => scalar(ScalarType::String, s.value().clone()),
            Value::Integer(_) => scalar(ScalarType::Integer, self.raw(&span)),
            Value::Float(_) => scalar(ScalarType::Float, self.raw(&span)),
            Value::Boolean(_) => scalar(ScalarType::Bool, self.raw(&span)),
            Value::Datetime(_) => scalar(ScalarType::Datetime, self.raw(&span)),
            Value::Array(a) => {
                self.enter()?;
                let id = self.b.push(NodeKind::Array, span, String::new());
                for item in a {
                    let child = self.value(item)?;
                    self.b.push_item(id, child);
                }
                self.depth -= 1;
                return Ok(id);
            }
            Value::InlineTable(t) => {
                self.enter()?;
                let id = self.b.push(NodeKind::Object, span, String::new());
                for (k, item) in t {
                    let key_span = t.key(k).and_then(|key| self.shifted(key.span()));
                    let child = self.value(item)?;
                    self.keyed(child, key_span);
                    self.b.add_child(id, k.to_owned(), child);
                }
                self.depth -= 1;
                return Ok(id);
            }
        };
        Ok(self.b.push(kind, span, text))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn get<'t>(t: &'t Tree, path: &[&str]) -> &'t crate::Node {
        let p: Vec<String> = path.iter().map(|s| (*s).to_owned()).collect();
        t.node(t.get(&p).unwrap_or_else(|| panic!("{path:?}")))
    }

    #[test]
    fn structure_and_spans() {
        let src = "# c\n[a]\nx = \"h\\tq\" # c\ny.z = 0x1F\n[c]\nk = { i = 1, j = [1, 2.5] }\n[a.b]\nw = 2026-10-01\n[[arr]]\nn = true\n[[arr]]\nn = false\n";
        let t = Toml.parse(src.as_bytes()).unwrap();
        let x = get(&t, &["a", "x"]);
        assert_eq!(x.text, "h\tq");
        assert_eq!(&src[x.value_span.clone()], "\"h\\tq\"");
        assert_eq!(&src[x.span.clone()], "x = \"h\\tq\"");
        let z = get(&t, &["a", "y", "z"]);
        assert_eq!(
            (z.kind, z.text.as_str()),
            (NodeKind::Scalar(ScalarType::Integer), "0x1F")
        );
        assert_eq!(
            get(&t, &["c", "k", "j", "1"]).kind,
            NodeKind::Scalar(ScalarType::Float)
        );
        assert_eq!(
            get(&t, &["a", "b", "w"]).kind,
            NodeKind::Scalar(ScalarType::Datetime)
        );
        assert_eq!(get(&t, &["arr", "1", "n"]).text, "false");
        assert_eq!(&src[get(&t, &["c"]).span.clone()], "[c]");
        let top: Vec<_> = t
            .node(t.root)
            .children
            .iter()
            .map(|(k, _)| k.as_str())
            .collect();
        assert_eq!(top, ["a", "c", "arr"]);
    }

    #[test]
    fn bom_and_unicode() {
        let src = "\u{feff}\"é\" = \"ü\"\n\"k é\" = 'lit\\n'\n";
        let t = Toml.parse(src.as_bytes()).unwrap();
        let n = get(&t, &["é"]);
        assert_eq!(&src[n.value_span.clone()], "\"ü\"");
        assert_eq!(&src[n.key_span.clone().unwrap()], "\"é\"");
        assert_eq!(get(&t, &["k é"]).text, "lit\\n");
    }

    #[test]
    fn errors() {
        let Err(DriverError::Syntax { line, .. }) = Toml.parse(b"a = 1\nb = \n") else {
            panic!()
        };
        assert_eq!(line, 2);
        assert!(Toml.parse(b"a = 1\na = 2\n").is_err());
    }
}
