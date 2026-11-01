//! CSV: rows by index (`0`, `1`, …, after the header row), each a directory of
//! its fields keyed by the header. Values are strings.
//!
//! Field spans come from a small RFC 4180 scanner; the `csv` crate checks it.

use std::ops::Range;

use crate::{Builder, Caps, Driver, DriverError, Edit, NewValue, NodeKind, ScalarType, Tree};

/// The CSV driver.
#[derive(Clone, Copy, Debug, Default)]
pub struct Csv;

/// One field: its span (quotes included) and decoded value.
struct Field {
    span: Range<usize>,
    value: String,
}

/// One record: its span (without the line ending) and fields.
struct Row {
    span: Range<usize>,
    /// The span including the line ending.
    line: Range<usize>,
    fields: Vec<Field>,
}

/// Splits `s` (from `start`) into records and fields.
fn scan(s: &str, start: usize) -> Result<Vec<Row>, DriverError> {
    let b = s.as_bytes();
    let mut rows = Vec::new();
    let mut i = start;
    while i < b.len() {
        let row_start = i;
        let mut fields = Vec::new();
        loop {
            let f_start = i;
            let mut value = String::new();
            if b.get(i) == Some(&b'"') {
                i += 1;
                loop {
                    match b.get(i) {
                        None => {
                            return Err(DriverError::syntax(
                                s,
                                f_start,
                                "unterminated quoted field",
                            ));
                        }
                        Some(b'"') if b.get(i + 1) == Some(&b'"') => {
                            value.push('"');
                            i += 2;
                        }
                        Some(b'"') => {
                            i += 1;
                            break;
                        }
                        Some(_) => {
                            let next = s[i..].find('"').map_or(b.len(), |n| i + n);
                            value.push_str(&s[i..next]);
                            i = next;
                        }
                    }
                }
                if !matches!(b.get(i), None | Some(b',' | b'\n' | b'\r')) {
                    return Err(DriverError::syntax(
                        s,
                        i,
                        "unexpected character after a quoted field",
                    ));
                }
            } else {
                let end = s[i..].find([',', '\n', '\r']).map_or(b.len(), |n| i + n);
                if s[i..end].contains('"') {
                    return Err(DriverError::syntax(s, i, "quote inside an unquoted field"));
                }
                value.push_str(&s[i..end]);
                i = end;
            }
            fields.push(Field {
                span: f_start..i,
                value,
            });
            if b.get(i) == Some(&b',') {
                i += 1;
            } else {
                break;
            }
        }
        let span = row_start..i;
        if b.get(i) == Some(&b'\r') {
            i += 1;
        }
        if b.get(i) == Some(&b'\n') {
            i += 1;
        }
        rows.push(Row {
            span,
            line: row_start..i,
            fields,
        });
    }
    Ok(rows)
}

/// A field literal: quoted when the value needs it (or the old one was). An
/// empty sole field is quoted too, or its row would be a blank line.
fn literal(old: &str, value: &str, sole: bool) -> String {
    let needs = value.contains([',', '"', '\n', '\r'])
        || value.trim() != value
        || (sole && value.is_empty());
    if needs || old.starts_with('"') {
        format!("\"{}\"", value.replace('"', "\"\""))
    } else {
        value.to_owned()
    }
}

impl Driver for Csv {
    fn name(&self) -> &'static str {
        "csv"
    }

    fn extensions(&self) -> &'static [&'static str] {
        &["csv"]
    }

    fn sniff(&self, _head: &[u8]) -> bool {
        false
    }

    fn parse(&self, src: &[u8]) -> Result<Tree, DriverError> {
        let (s, start) = crate::text(src)?;
        let rows = scan(s, start)?;
        let mut b = Builder::default();
        let root = b.push(NodeKind::Array, start..s.len(), String::new());
        let Some((header, data)) = rows.split_first() else {
            return Ok(b.finish(root));
        };
        let names: Vec<String> = header.fields.iter().map(|f| f.value.clone()).collect();
        for row in data {
            // A blank line is no row.
            if row.span.is_empty() {
                continue;
            }
            if row.fields.len() != names.len() {
                return Err(DriverError::syntax(
                    s,
                    row.span.start,
                    format!(
                        "row has {} fields, the header has {}",
                        row.fields.len(),
                        names.len()
                    ),
                ));
            }
            let r = b.push(NodeKind::Object, row.span.clone(), String::new());
            b.node_mut(r).span = row.line.clone();
            for (name, f) in names.iter().zip(&row.fields) {
                let id = b.push(
                    NodeKind::Scalar(ScalarType::String),
                    f.span.clone(),
                    f.value.clone(),
                );
                b.add_child(r, name.clone(), id);
            }
            b.push_item(root, r);
        }
        Ok(b.finish(root))
    }

    fn splice(&self, src: &[u8], ptr: &[String], edit: Edit) -> Result<Vec<u8>, DriverError> {
        let tree = self.parse(src)?;
        let (s, _) = crate::text(src)?;
        let id = tree.get(ptr).ok_or(DriverError::NotFound)?;
        let node = tree.node(id);
        let out = match edit {
            Edit::SetScalar(text) => {
                let NodeKind::Scalar(_) = node.kind else {
                    return Err(DriverError::WrongKind("not a field"));
                };
                let text = crate::strip_newline(&text);
                let sole = tree
                    .get(&ptr[..ptr.len().saturating_sub(1)])
                    .is_some_and(|row| tree.node(row).children.len() == 1);
                let out = crate::replace(
                    src,
                    node.value_span.clone(),
                    &literal(&s[node.value_span.clone()], text, sole),
                );
                crate::check_scalar(self, &out, ptr, ScalarType::String, text)?;
                return Ok(out);
            }
            Edit::Insert { key, value } => {
                if !ptr.is_empty() {
                    return Err(DriverError::Unsupported(
                        "csv rows have fixed columns; add a row instead",
                    ));
                }
                if key != node.children.len().to_string() {
                    return Err(DriverError::InvalidValue(format!(
                        "rows are added at the end: the next is {}",
                        node.children.len()
                    )));
                }
                if value != NewValue::EmptyObject {
                    return Err(DriverError::InvalidValue(
                        "a new row is a directory: mkdir it".into(),
                    ));
                }
                let header = scan(s, crate::text(src)?.1)?
                    .into_iter()
                    .next()
                    .map_or(0, |h| h.fields.len());
                // With no header there are no columns, and a new line would
                // become the header, not a row.
                if header == 0 {
                    return Err(DriverError::Unsupported(
                        "this csv file has no header row; write one first",
                    ));
                }
                let newline = if s.contains("\r\n") { "\r\n" } else { "\n" };
                let lead = if s.is_empty() || s.ends_with('\n') {
                    ""
                } else {
                    newline
                };
                // One column: an empty row would be a blank line, which is no row.
                let row = if header == 1 {
                    "\"\"".to_owned()
                } else {
                    ",".repeat(header.saturating_sub(1))
                };
                let out =
                    crate::replace(src, src.len()..src.len(), &format!("{lead}{row}{newline}"));
                // The new row must be there (blank lines are no rows).
                let after = self.parse(&out)?;
                if after.node(after.root).children.len() != node.children.len() + 1 {
                    return Err(DriverError::Unsupported("can't add a row to this csv file"));
                }
                out
            }
            Edit::Remove => {
                if ptr.len() != 1 {
                    return Err(DriverError::Unsupported(
                        "csv rows have fixed columns; remove the row instead",
                    ));
                }
                crate::replace(src, node.span.clone(), "")
            }
        };
        self.parse(&out)?;
        Ok(out)
    }

    fn capabilities(&self) -> Caps {
        Caps {
            read: true,
            write_scalar: true,
            insert: true,
            remove: true,
            mkdir: true,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SRC: &str = "id,name,note\n1,ana,\"likes, commas\"\n2,bo,\"says \"\"hi\"\"\"\n";

    fn get<'t>(t: &'t Tree, p: &[&str]) -> &'t crate::Node {
        let p: Vec<String> = p.iter().map(|s| (*s).to_owned()).collect();
        t.node(t.get(&p).unwrap())
    }

    #[test]
    fn rows_and_fields() {
        let t = Csv.parse(SRC.as_bytes()).unwrap();
        assert_eq!(t.node(t.root).children.len(), 2);
        assert_eq!(get(&t, &["0", "name"]).text, "ana");
        assert_eq!(get(&t, &["0", "note"]).text, "likes, commas");
        assert_eq!(get(&t, &["1", "note"]).text, "says \"hi\"");
        assert_eq!(
            &SRC[get(&t, &["1", "note"]).value_span.clone()],
            "\"says \"\"hi\"\"\""
        );
        assert!(Csv.parse(b"a,b\n1\n").is_err());
        assert!(Csv.parse(b"a\n\"open\n").is_err());
        // CRLF and a missing final newline.
        let t = Csv.parse(b"a,b\r\n1,2").unwrap();
        assert_eq!(get(&t, &["0", "b"]).text, "2");
    }

    #[test]
    fn writes() {
        let p = |v: &[&str]| v.iter().map(|s| (*s).to_owned()).collect::<Vec<_>>();
        let out = Csv
            .splice(
                SRC.as_bytes(),
                &p(&["0", "name"]),
                Edit::SetScalar("ana, jr".into()),
            )
            .unwrap();
        assert_eq!(
            String::from_utf8(out).unwrap(),
            SRC.replace("1,ana,", "1,\"ana, jr\",")
        );
        let out = Csv
            .splice(
                SRC.as_bytes(),
                &p(&[]),
                Edit::Insert {
                    key: "2".into(),
                    value: NewValue::EmptyObject,
                },
            )
            .unwrap();
        assert_eq!(String::from_utf8(out).unwrap(), format!("{SRC},,\n"));
        let out = Csv
            .splice(SRC.as_bytes(), &p(&["0"]), Edit::Remove)
            .unwrap();
        assert_eq!(
            String::from_utf8(out).unwrap(),
            "id,name,note\n2,bo,\"says \"\"hi\"\"\"\n"
        );
    }
}
