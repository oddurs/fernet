//! TOML, via `toml_edit`'s span-preserving parser (`docs/DECISIONS.md`, entry 12).

use std::fmt::Write as _;

use toml_edit::{ArrayOfTables, DocumentMut, InlineTable, Item, Table, Value};

use crate::{
    Builder, Caps, Driver, DriverError, Edit, MAX_DEPTH, NewValue, NodeId, NodeKind, ScalarType,
    Tree,
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

    fn splice(&self, src: &[u8], ptr: &[String], edit: Edit) -> Result<Vec<u8>, DriverError> {
        let tree = self.parse(src)?;
        let id = tree.get(ptr).ok_or(DriverError::NotFound)?;
        let out = match edit {
            Edit::SetScalar(text) => {
                let node = tree.node(id);
                let NodeKind::Scalar(ty) = node.kind else {
                    return Err(DriverError::WrongKind("not a value"));
                };
                let text = crate::strip_newline(&text);
                let (s, _) = crate::text(src)?;
                let literal = match ty {
                    ScalarType::String | ScalarType::Text => {
                        string_literal(&s[node.value_span.clone()], text)
                    }
                    _ if value_type(text) == Some(ty) => text.to_owned(),
                    _ => {
                        return Err(DriverError::InvalidValue(format!(
                            "{text:?} isn't a toml {}",
                            crate::type_name(ty)
                        )));
                    }
                };
                let out = crate::replace(src, node.value_span.clone(), &literal);
                crate::check_scalar(self, &out, ptr, ty, text)?;
                return Ok(out);
            }
            Edit::Insert { key, value } => edit_doc(src, ptr, |slot| insert(slot, &key, &value))?,
            Edit::Remove => {
                let (last, parent) = ptr
                    .split_last()
                    .ok_or(DriverError::Unsupported("can't remove the whole document"))?;
                edit_doc(src, parent, |slot| remove(slot, last))?
            }
        };
        self.parse(&out)
            .map_err(|_| DriverError::Unsupported("the edit would produce invalid toml"))?;
        Ok(out)
    }

    fn capabilities(&self) -> Caps {
        Caps::FULL
    }
}

/// The type `text` has as a TOML value, if it is one.
fn value_type(text: &str) -> Option<ScalarType> {
    let doc: DocumentMut = format!("v = {text}").parse().ok()?;
    let ty = match doc.get("v")?.as_value()? {
        Value::String(_) => ScalarType::String,
        Value::Integer(_) => ScalarType::Integer,
        Value::Float(_) => ScalarType::Float,
        Value::Boolean(_) => ScalarType::Bool,
        Value::Datetime(_) => ScalarType::Datetime,
        Value::Array(_) | Value::InlineTable(_) => return None,
    };
    Some(ty)
}

/// Escapes for TOML basic strings. Newlines stay raw when `multiline`.
fn escape_basic(text: &str, multiline: bool) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' if multiline => out.push('\n'),
            '\n' => out.push_str("\\n"),
            '\t' => out.push_str("\\t"),
            '\r' => out.push_str("\\r"),
            '\u{8}' => out.push_str("\\b"),
            '\u{c}' => out.push_str("\\f"),
            c if c.is_control() => drop(write!(out, "\\u{:04X}", c as u32)),
            c => out.push(c),
        }
    }
    out
}

/// A string literal for `text` in the style of `old` (the existing literal):
/// literal strings stay literal when they can, multi-line stays multi-line,
/// and everything else becomes a basic string.
fn string_literal(old: &str, text: &str) -> String {
    let plain_chars = !text
        .chars()
        .any(|c| c.is_control() && c != '\t' && c != '\n');
    if old.starts_with("'''") && plain_chars && !text.contains('\'') {
        let lead = if old.starts_with("'''\n") || text.starts_with('\n') {
            "\n"
        } else {
            ""
        };
        return format!("'''{lead}{text}'''");
    }
    if old.starts_with("\"\"\"") {
        let lead = if old.starts_with("\"\"\"\n") {
            "\n"
        } else {
            ""
        };
        // A leading newline right after the quotes is trimmed by TOML; escape it.
        let body = escape_basic(text, true);
        let body = if lead.is_empty() && body.starts_with('\n') {
            format!("\\n{}", &body[1..])
        } else {
            body
        };
        return format!("\"\"\"{lead}{body}\"\"\"");
    }
    if old.starts_with('\'') && plain_chars && !text.contains(['\'', '\n']) {
        return format!("'{text}'");
    }
    format!("\"{}\"", escape_basic(text, false))
}

/// A TOML value for new text: the value it is, if it's exactly one TOML
/// value (no comment, nothing after it), else a string, as JSON does.
fn new_value(text: &str) -> Value {
    let text = crate::strip_newline(text);
    match text.parse::<Value>() {
        Ok(mut v) if !matches!(v, Value::String(_)) => {
            v.decor_mut().clear();
            v
        }
        _ => Value::from(text),
    }
}

/// True if an array's layout holds comments. `toml_edit` keeps a comment in
/// the whitespace before the next element, so adding or removing an element
/// would move it onto the wrong one.
fn has_comments(a: &toml_edit::Array) -> bool {
    let raw = |r: Option<&toml_edit::RawString>| {
        r.and_then(|r| r.as_str()).is_some_and(|s| s.contains('#'))
    };
    raw(Some(a.trailing()))
        || a.iter()
            .any(|v| raw(v.decor().prefix()) || raw(v.decor().suffix()))
}

const COMMENTED_ARRAY: DriverError =
    DriverError::Unsupported("this array has comments between its elements; edit it by hand");

/// A mutable position in a `toml_edit` document.
enum Slot<'a> {
    Table(&'a mut Table),
    Inline(&'a mut InlineTable),
    Array(&'a mut toml_edit::Array),
    Tables(&'a mut ArrayOfTables),
    Scalar,
}

fn slot_of_item(item: &mut Item) -> Slot<'_> {
    match item {
        Item::Table(t) => Slot::Table(t),
        Item::ArrayOfTables(a) => Slot::Tables(a),
        Item::Value(v) => slot_of_value(v),
        Item::None => Slot::Scalar,
    }
}

fn slot_of_value(value: &mut Value) -> Slot<'_> {
    match value {
        Value::InlineTable(t) => Slot::Inline(t),
        Value::Array(a) => Slot::Array(a),
        _ => Slot::Scalar,
    }
}

fn step<'a>(slot: Slot<'a>, key: &str) -> Option<Slot<'a>> {
    match slot {
        Slot::Table(t) => t.get_mut(key).map(slot_of_item),
        Slot::Inline(t) => t.get_mut(key).map(slot_of_value),
        Slot::Array(a) => a.get_mut(key.parse::<usize>().ok()?).map(slot_of_value),
        Slot::Tables(a) => a.get_mut(key.parse::<usize>().ok()?).map(Slot::Table),
        Slot::Scalar => None,
    }
}

/// Parses `src` with `toml_edit`, applies `f` at `ptr`, and prints it back.
/// `toml_edit` keeps every untouched item's formatting; [`merge_lines`] then
/// keeps every untouched line's bytes, line ending included.
fn edit_doc(
    src: &[u8],
    ptr: &[String],
    f: impl FnOnce(Slot<'_>) -> Result<(), DriverError>,
) -> Result<Vec<u8>, DriverError> {
    let (s, start) = crate::text(src)?;
    let mut doc: DocumentMut = s[start..]
        .parse()
        .map_err(|_| DriverError::Unsupported("invalid toml"))?;
    let mut slot = Slot::Table(doc.as_table_mut());
    for key in ptr {
        slot = step(slot, key).ok_or(DriverError::NotFound)?;
    }
    f(slot)?;
    let printed = doc.to_string();
    let merged = merge_lines(&s[start..], &printed)?;
    // The merged text must mean exactly what toml_edit printed.
    let same = match (
        Toml.parse(merged.as_bytes()),
        Toml.parse(printed.as_bytes()),
    ) {
        (Ok(a), Ok(b)) => crate::same_content(&a, &b),
        _ => false,
    };
    if !same {
        return Err(DriverError::Unsupported(
            "this edit can't keep the file's layout; edit it by hand",
        ));
    }
    let mut out = s.as_bytes()[..start].to_vec();
    out.extend_from_slice(merged.as_bytes());
    Ok(out)
}

/// One line: its text without the line ending, and the ending (`\n`,
/// `\r\n` or nothing for a last line without one).
fn split_lines(s: &str) -> Vec<(&str, &str)> {
    s.split_inclusive('\n')
        .map(|l| {
            let body = l
                .strip_suffix('\n')
                .map_or(l, |b| b.strip_suffix('\r').unwrap_or(b));
            (body, &l[body.len()..])
        })
        .collect()
}

/// The most cells [`lcs_pairs`] may use: the changed middle of a file is
/// small for one insert or remove.
const MAX_LCS_CELLS: usize = 1 << 22;

/// Index pairs of a longest common subsequence of `old` and `new`.
fn lcs_pairs(old: &[&str], new: &[&str]) -> Result<Vec<(usize, usize)>, DriverError> {
    let width = new.len() + 1;
    if (old.len() + 1).saturating_mul(width) > MAX_LCS_CELLS {
        return Err(DriverError::Unsupported(
            "this edit changes too much of the file at once; edit it by hand",
        ));
    }
    // table[o * width + n]: LCS length of old[o..] and new[n..].
    let mut table = vec![0u32; (old.len() + 1) * width];
    for o in (0..old.len()).rev() {
        for n in (0..new.len()).rev() {
            table[o * width + n] = if old[o] == new[n] {
                table[(o + 1) * width + n + 1] + 1
            } else {
                table[(o + 1) * width + n].max(table[o * width + n + 1])
            };
        }
    }
    let mut pairs = Vec::new();
    let (mut o, mut n) = (0, 0);
    while o < old.len() && n < new.len() {
        if old[o] == new[n] {
            pairs.push((o, n));
            o += 1;
            n += 1;
        } else if table[(o + 1) * width + n] >= table[o * width + n + 1] {
            o += 1;
        } else {
            n += 1;
        }
    }
    Ok(pairs)
}

/// How many lines `a` and `b` share, counting duplicates (multiset
/// intersection).
fn shared_count(a: &[&str], b: &[&str]) -> usize {
    let mut counts: std::collections::HashMap<&str, isize> = std::collections::HashMap::new();
    for l in a {
        *counts.entry(l).or_default() += 1;
    }
    let mut shared = 0;
    for l in b {
        if let Some(c) = counts.get_mut(l)
            && *c > 0
        {
            *c -= 1;
            shared += 1;
        }
    }
    shared
}

/// Rebuilds `printed` (`toml_edit`'s output for the edited document) from
/// `old`'s lines wherever they line up: every line the edit didn't touch keeps
/// its exact bytes and line ending (`toml_edit` prints `\n`, and a file may
/// mix endings, even inside multi-line strings). New lines get the file's
/// usual ending.
///
/// Refuses if `toml_edit` moved untouched lines (it regroups dotted keys):
/// every non-blank line both versions share must stay in order. Lines are
/// compared as a multiset, so duplicate lines (`enabled = true` in two tables,
/// `[[p]]` headers) count once per occurrence.
fn merge_lines(old: &str, printed: &str) -> Result<String, DriverError> {
    let old_lines = split_lines(old);
    let new_lines = split_lines(printed);
    // Trim the common head and tail; one edit changes a small middle.
    let head = old_lines
        .iter()
        .zip(&new_lines)
        .take_while(|(a, b)| a.0 == b.0)
        .count();
    let tail = old_lines[head..]
        .iter()
        .rev()
        .zip(new_lines[head..].iter().rev())
        .take_while(|(a, b)| a.0 == b.0)
        .count();
    let old_mid: Vec<&str> = old_lines[head..old_lines.len() - tail]
        .iter()
        .map(|l| l.0)
        .collect();
    let new_mid: Vec<&str> = new_lines[head..new_lines.len() - tail]
        .iter()
        .map(|l| l.0)
        .collect();
    let nonblank = |l: &&str| !l.trim().is_empty();
    let old_set: Vec<&str> = old_mid.iter().copied().filter(nonblank).collect();
    let new_set: Vec<&str> = new_mid.iter().copied().filter(nonblank).collect();
    if lcs_pairs(&old_set, &new_set)?.len() != shared_count(&old_set, &new_set) {
        return Err(DriverError::Unsupported(
            "this edit would reorder the file's keys; edit it by hand",
        ));
    }
    let crlf = old.matches("\r\n").count();
    let usual = if crlf * 2 > old.matches('\n').count() {
        "\r\n"
    } else {
        "\n"
    };
    // Old line index for each new line that lines up with one.
    let mut from_old: Vec<Option<usize>> = vec![None; new_lines.len()];
    for (i, slot) in from_old.iter_mut().enumerate().take(head) {
        *slot = Some(i);
    }
    for (i, j) in lcs_pairs(&old_mid, &new_mid)? {
        from_old[head + j] = Some(head + i);
    }
    for k in 0..tail {
        from_old[new_lines.len() - 1 - k] = Some(old_lines.len() - 1 - k);
    }
    let mut out = String::with_capacity(printed.len() + crlf);
    for ((text, ending), old_index) in new_lines.iter().zip(from_old) {
        out.push_str(text);
        if ending.is_empty() {
            continue;
        }
        let kept = old_index.map_or("", |i| old_lines[i].1);
        out.push_str(match kept {
            "" if *ending == "\r\n" => "\r\n",
            "" => usual,
            kept => kept,
        });
    }
    Ok(out)
}

fn next_index(key: &str, len: usize) -> Result<(), DriverError> {
    if key == len.to_string() {
        Ok(())
    } else {
        Err(DriverError::InvalidValue(format!(
            "arrays grow at the end: the next index is {len}"
        )))
    }
}

fn insert(slot: Slot<'_>, key: &str, value: &NewValue) -> Result<(), DriverError> {
    let as_value = || match value {
        NewValue::EmptyObject => Value::InlineTable(InlineTable::new()),
        NewValue::EmptyArray => Value::Array(toml_edit::Array::new()),
        NewValue::Text(t) => new_value(t),
    };
    match slot {
        Slot::Table(t) => {
            if t.contains_key(key) {
                return Err(DriverError::Exists);
            }
            let item = match value {
                NewValue::EmptyObject => Item::Table(Table::new()),
                _ => Item::Value(as_value()),
            };
            t.insert(key, item);
        }
        Slot::Inline(t) => {
            if t.contains_key(key) {
                return Err(DriverError::Exists);
            }
            // The space before `}` belongs to the last value: move it to the new one.
            let mut suffix = String::new();
            if let Some((_, last)) = t.iter_mut().last() {
                last.decor()
                    .suffix()
                    .and_then(|s| s.as_str())
                    .unwrap_or("")
                    .clone_into(&mut suffix);
                last.decor_mut().set_suffix("");
            }
            let mut v = as_value();
            v.decor_mut().set_prefix(" ");
            v.decor_mut().set_suffix(suffix);
            t.insert(key, v);
        }
        Slot::Array(a) => {
            next_index(key, a.len())?;
            if has_comments(a) {
                return Err(COMMENTED_ARRAY);
            }
            a.push(as_value());
        }
        Slot::Tables(a) => {
            next_index(key, a.len())?;
            if *value != NewValue::EmptyObject {
                return Err(DriverError::InvalidValue(
                    "an array of tables only holds tables".into(),
                ));
            }
            a.push(Table::new());
        }
        Slot::Scalar => return Err(DriverError::WrongKind("not a table or array")),
    }
    Ok(())
}

fn remove(slot: Slot<'_>, key: &str) -> Result<(), DriverError> {
    let gone = match slot {
        Slot::Table(t) => t.remove(key).is_some(),
        Slot::Inline(t) => t.remove(key).is_some(),
        Slot::Array(a) => match key.parse::<usize>() {
            Ok(i) if i < a.len() && has_comments(a) => return Err(COMMENTED_ARRAY),
            Ok(i) if i < a.len() => {
                // Keep the layout: the new first element takes the old first's prefix.
                let prefix = a
                    .get(i)
                    .and_then(|v| v.decor().prefix())
                    .and_then(|p| p.as_str())
                    .map(str::to_owned);
                a.remove(i);
                if i == 0
                    && let (Some(first), Some(prefix)) = (a.get_mut(0), prefix)
                {
                    first.decor_mut().set_prefix(prefix);
                }
                true
            }
            _ => false,
        },
        Slot::Tables(a) => match key.parse::<usize>() {
            // An empty array of tables prints as nothing: the key would go.
            Ok(0) if a.len() == 1 => {
                return Err(DriverError::Unsupported(
                    "that's the array's only table: remove the whole array instead",
                ));
            }
            Ok(i) if i < a.len() => {
                a.remove(i);
                true
            }
            _ => false,
        },
        Slot::Scalar => false,
    };
    if gone {
        Ok(())
    } else {
        Err(DriverError::NotFound)
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
