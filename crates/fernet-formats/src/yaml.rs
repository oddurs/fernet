//! YAML, built from `saphyr-parser` events (`docs/DECISIONS.md`, entry 13).
//!
//! Read semantics for v0.1: only the first document is read; aliases resolve
//! to their anchored node; `<<` merge keys are ordinary keys; mapping and
//! sequence keys are left out; scalar types follow the YAML 1.2 core schema.

use std::fmt::Write as _;

use std::collections::HashMap;
use std::ops::Range;

use saphyr_parser::{Event, Parser, ScalarStyle, Span, Tag};

use crate::{
    Builder, Caps, Driver, DriverError, Edit, MAX_DEPTH, NodeId, NodeKind, ScalarType, Tree,
};

/// The YAML driver.
#[derive(Clone, Copy, Debug, Default)]
pub struct Yaml;

impl Driver for Yaml {
    fn name(&self) -> &'static str {
        "yaml"
    }

    fn extensions(&self) -> &'static [&'static str] {
        &["yaml", "yml"]
    }

    fn sniff(&self, head: &[u8]) -> bool {
        let s = String::from_utf8_lossy(head);
        crate::first_line(&s, "#")
            .is_some_and(|l| l == "---" || l.starts_with("--- ") || l.starts_with("%YAML"))
    }

    fn parse(&self, src: &[u8]) -> Result<Tree, DriverError> {
        let (s, _) = crate::text(src)?;
        Build::new(s).run()
    }

    fn splice(&self, src: &[u8], ptr: &[String], edit: Edit) -> Result<Vec<u8>, DriverError> {
        let Edit::SetScalar(text) = edit else {
            return Err(DriverError::Unsupported(
                "yaml supports writing values only, not adding or removing keys",
            ));
        };
        let tree = self.parse(src)?;
        let (s, _) = crate::text(src)?;
        // Nodes reached through an alias, or carrying an anchor, are shared:
        // writing one would change every alias.
        let mut refs = vec![0u32; tree.nodes.len()];
        for n in &tree.nodes {
            for (_, c) in &n.children {
                refs[c.0] += 1;
            }
        }
        let mut at = tree.root;
        for key in ptr {
            at = tree.child(at, key).ok_or(DriverError::NotFound)?;
            if refs[at.0] > 1 {
                return Err(DriverError::Unsupported(
                    "values behind yaml aliases are read-only",
                ));
            }
        }
        let node = tree.node(at);
        let NodeKind::Scalar(ty) = node.kind else {
            return Err(DriverError::WrongKind("not a value"));
        };
        let old = &s[node.value_span.clone()];
        if old.starts_with(['|', '>']) {
            return Err(DriverError::Unsupported("yaml block scalars are read-only"));
        }
        if has_properties(s, node.value_span.start) {
            return Err(DriverError::Unsupported(
                "yaml values with anchors or tags are read-only",
            ));
        }
        let text = crate::strip_newline(&text);
        let literal = match ty {
            ScalarType::String | ScalarType::Text => string_literal(old, text),
            _ if core_schema(text) == ty => text.to_owned(),
            _ => {
                return Err(DriverError::InvalidValue(format!(
                    "{text:?} isn't a yaml {}",
                    crate::type_name(ty)
                )));
            }
        };
        let out = crate::replace(src, node.value_span.clone(), &literal);
        crate::check_scalar(self, &out, ptr, ty, text)?;
        Ok(out)
    }

    fn capabilities(&self) -> Caps {
        Caps::SCALARS
    }
}

/// Char index → byte offset. saphyr's markers count characters.
struct Offsets {
    /// Empty when the source is ASCII (identity mapping).
    table: Vec<usize>,
    len: usize,
}

impl Offsets {
    fn new(s: &str) -> Self {
        let table = if s.is_ascii() {
            Vec::new()
        } else {
            s.char_indices()
                .map(|(i, _)| i)
                .chain(std::iter::once(s.len()))
                .collect()
        };
        Self {
            table,
            len: s.len(),
        }
    }

    fn byte(&self, char_index: usize) -> usize {
        if self.table.is_empty() {
            char_index.min(self.len)
        } else {
            self.table.get(char_index).copied().unwrap_or(self.len)
        }
    }
}

struct Frame {
    id: NodeId,
    /// Anchor id to record when the collection ends; 0 for none.
    anchor: usize,
    /// For mappings, the key state; `None` for sequences.
    key: Option<Key>,
}

enum Key {
    /// Waiting for a key.
    Waiting,
    /// A scalar key, waiting for its value.
    Named(String, Range<usize>),
    /// A key we can't name (a mapping, sequence or alias); its value is
    /// parsed and dropped.
    Skip,
}

enum Anchor {
    Open,
    Done(NodeId),
}

struct Build<'s> {
    src: &'s str,
    offsets: Offsets,
    b: Builder,
    stack: Vec<Frame>,
    anchors: HashMap<usize, Anchor>,
    root: Option<NodeId>,
}

impl<'s> Build<'s> {
    fn new(src: &'s str) -> Self {
        Self {
            src,
            offsets: Offsets::new(src),
            b: Builder::default(),
            stack: Vec::new(),
            anchors: HashMap::new(),
            root: None,
        }
    }

    fn range(&self, span: &Span) -> Range<usize> {
        self.offsets.byte(span.start.index())..self.offsets.byte(span.end.index())
    }

    fn error(&self, span: &Span, message: &str) -> DriverError {
        DriverError::syntax(self.src, self.range(span).start, message)
    }

    /// True if the next node event is a mapping key.
    fn wants_key(&self) -> bool {
        matches!(
            self.stack.last(),
            Some(Frame {
                key: Some(Key::Waiting),
                ..
            })
        )
    }

    fn set_key(&mut self, key: Key) {
        if let Some(frame) = self.stack.last_mut() {
            frame.key = Some(key);
        }
    }

    /// Hangs a finished value node on its parent. `alias` nodes already have
    /// their own spans from the anchor site, which are kept.
    fn attach(&mut self, id: NodeId, alias: bool) {
        let Some(frame) = self.stack.last_mut() else {
            if self.root.is_none() {
                self.root = Some(id);
            }
            return;
        };
        let parent = frame.id;
        match frame.key.replace(Key::Waiting) {
            None => {
                frame.key = None;
                self.b.push_item(parent, id);
            }
            Some(Key::Named(key, key_span)) => {
                if !alias {
                    let node = self.b.node_mut(id);
                    node.span = key_span.start..node.value_span.end.max(key_span.end);
                    node.key_span = Some(key_span);
                }
                self.b.add_child(parent, key, id);
            }
            Some(Key::Skip | Key::Waiting) => {}
        }
    }

    /// Skips leading `&anchor` and `!tag` properties in a scalar's span.
    fn strip_props(&self, r: Range<usize>) -> Range<usize> {
        let mut start = r.start;
        let bytes = self.src.as_bytes();
        while start < r.end && matches!(bytes[start], b'&' | b'!') {
            while start < r.end && !bytes[start].is_ascii_whitespace() {
                start += 1;
            }
            while start < r.end && bytes[start].is_ascii_whitespace() {
                start += 1;
            }
        }
        start..r.end
    }

    /// For block scalars saphyr's span starts at the content; widen it to
    /// start at the `|` or `>` header so the span is a self-contained value.
    fn block_header(&self, r: Range<usize>) -> Range<usize> {
        let before = &self.src[..r.start];
        let trimmed = before.trim_end_matches([' ', '\t', '\r', '\n']);
        let line_start = trimmed.rfind('\n').map_or(0, |i| i + 1);
        let line = &trimmed[line_start..];
        let header = line.char_indices().find(|&(i, c)| {
            (c == '|' || c == '>') && {
                let rest = line[i + 1..]
                    .trim_start_matches(|c: char| c.is_ascii_digit() || c == '+' || c == '-');
                let rest = rest.trim_start_matches([' ', '\t']);
                rest.is_empty() || rest.starts_with('#')
            }
        });
        header.map_or(r.clone(), |(i, _)| line_start + i..r.end)
    }

    fn scalar(
        &mut self,
        value: &str,
        style: ScalarStyle,
        tag: Option<&Tag>,
        span: &Span,
    ) -> NodeId {
        let r = self.strip_props(self.range(span));
        let r = match style {
            ScalarStyle::Literal | ScalarStyle::Folded => self.block_header(r),
            _ => r,
        };
        let ty = match tag {
            Some(t) if t.is_yaml_core_schema() => match t.suffix.as_str() {
                "int" => ScalarType::Integer,
                "float" => ScalarType::Float,
                "bool" => ScalarType::Bool,
                "null" => ScalarType::Null,
                _ => ScalarType::String,
            },
            None if style == ScalarStyle::Plain => core_schema(value),
            // Quoted and block scalars, and custom tags (`!Ref x`), are text.
            _ => ScalarType::String,
        };
        self.b.push(NodeKind::Scalar(ty), r, value.to_owned())
    }

    fn run(mut self) -> Result<Tree, DriverError> {
        let mut parser = Parser::new_from_str(self.src);
        let mut seen_document = false;
        for next in &mut parser {
            let (event, span) = next.map_err(|e| {
                let at = self.offsets.byte(e.marker().index());
                DriverError::syntax(self.src, at, e.info().to_lowercase())
            })?;
            match event {
                // Only the first document is read.
                Event::DocumentStart(_) if seen_document => break,
                Event::DocumentStart(_) => seen_document = true,
                Event::DocumentEnd | Event::StreamEnd if self.stack.is_empty() => break,
                Event::Scalar(value, style, anchor, tag) => {
                    if self.wants_key() {
                        let r = self.strip_props(self.range(&span));
                        self.set_key(Key::Named(value.into_owned(), r));
                        continue;
                    }
                    let id = self.scalar(&value, style, tag.as_deref(), &span);
                    if anchor != 0 {
                        self.anchors.insert(anchor, Anchor::Done(id));
                    }
                    self.attach(id, false);
                }
                Event::MappingStart(anchor, _) | Event::SequenceStart(anchor, _) => {
                    if self.stack.len() >= MAX_DEPTH {
                        return Err(DriverError::TooDeep);
                    }
                    let is_map = matches!(event, Event::MappingStart(..));
                    let kind = if is_map {
                        NodeKind::Object
                    } else {
                        NodeKind::Array
                    };
                    let r = self.strip_props(self.range(&span));
                    let id = self.b.push(kind, r, String::new());
                    if anchor != 0 {
                        self.anchors.insert(anchor, Anchor::Open);
                    }
                    if self.wants_key() {
                        self.set_key(Key::Skip);
                    } else {
                        self.attach(id, false);
                    }
                    self.stack.push(Frame {
                        id,
                        anchor,
                        key: is_map.then_some(Key::Waiting),
                    });
                }
                Event::MappingEnd | Event::SequenceEnd => {
                    let Some(frame) = self.stack.pop() else {
                        continue;
                    };
                    let at = self.range(&span).start;
                    // A flow collection ends after its bracket; a block one
                    // where its last child ends, not where the next token starts.
                    let last = self.b.node_mut(frame.id).children.last().map(|&(_, c)| c);
                    let end = match (self.src.as_bytes().get(at), last) {
                        (Some(b']' | b'}'), _) => at + 1,
                        (_, Some(c)) => self.b.node_mut(c).value_span.end,
                        (_, None) => at,
                    };
                    let node = self.b.node_mut(frame.id);
                    let end = end.max(node.value_span.start);
                    node.value_span.end = end;
                    node.span.end = node.span.end.max(end);
                    if frame.anchor != 0 {
                        self.anchors.insert(frame.anchor, Anchor::Done(frame.id));
                    }
                }
                Event::Alias(anchor) => {
                    if self.wants_key() {
                        self.set_key(Key::Skip);
                        continue;
                    }
                    let id = match self.anchors.get(&anchor) {
                        Some(Anchor::Done(id)) => *id,
                        Some(Anchor::Open) => return Err(self.error(&span, "recursive alias")),
                        None => return Err(self.error(&span, "unknown anchor")),
                    };
                    self.attach(id, true);
                }
                Event::DocumentEnd | Event::StreamEnd | Event::Nothing | Event::StreamStart => {}
            }
        }
        let root = match self.root {
            Some(root) => root,
            // An empty stream is a null document.
            None => self
                .b
                .push(NodeKind::Scalar(ScalarType::Null), 0..0, String::new()),
        };
        Ok(self.b.finish(root))
    }
}

/// True if the token just before `at` (on the same line) is an `&anchor` or
/// `!tag` property.
fn has_properties(s: &str, at: usize) -> bool {
    // Properties may sit on the line above (`a: !!binary` then the value).
    let before = s[..at].trim_end_matches([' ', '\t', '\n', '\r']);
    let token_start = before
        .rfind([' ', '\t', '\n', ':', '-', '[', '{', ','])
        .map_or(0, |i| i + 1);
    before[token_start..].starts_with(['&', '!'])
}

/// True if `text` can be written as a plain scalar and read back as the same
/// string: one line, no surrounding spaces, no indicators, and not a word the
/// core schema would read as another type. Flow characters are refused too, so
/// it's safe inside `[…]` and `{…}`.
fn plain_ok(text: &str) -> bool {
    !text.is_empty()
        && core_schema(text) == ScalarType::String
        && text.trim() == text
        && !text.chars().any(|c| {
            c.is_control()
                || !yaml_printable(c)
                || matches!(c, ',' | '[' | ']' | '{' | '}' | '#' | '"' | '\'' | '\\')
        })
        && !text.starts_with(['-', '?', ':', '&', '*', '!', '|', '>', '%', '@', '`'])
        && !text.contains(": ")
        && !text.ends_with(':')
}

fn double_quoted(text: &str) -> String {
    let mut out = String::with_capacity(text.len() + 2);
    out.push('"');
    for c in text.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\t' => out.push_str("\\t"),
            '\r' => out.push_str("\\r"),
            '\0' => out.push_str("\\0"),
            c if !yaml_printable(c) => match u32::from(c) {
                n @ 0..=0xffff => drop(write!(out, "\\u{n:04x}")),
                n => drop(write!(out, "\\U{n:08x}")),
            },
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// YAML's printable set (spec 5.1).
fn yaml_printable(c: char) -> bool {
    matches!(u32::from(c), 0x9 | 0xA | 0xD | 0x20..=0x7E | 0x85 | 0xA0..=0xD7FF | 0xE000..=0xFFFD | 0x10000..=0x10_FFFF)
}

/// A scalar literal for `text` in the style of `old`: plain stays plain and
/// single-quoted stays single-quoted when the text allows; otherwise
/// double-quoted, which can carry anything.
fn string_literal(old: &str, text: &str) -> String {
    let simple = text.chars().all(|c| yaml_printable(c) && !c.is_control());
    if old.starts_with('\'') && simple {
        return format!("'{}'", text.replace('\'', "''"));
    }
    if !old.starts_with(['"', '\'']) && plain_ok(text) {
        return text.to_owned();
    }
    double_quoted(text)
}

/// YAML 1.2 core-schema resolution for untagged plain scalars.
fn core_schema(v: &str) -> ScalarType {
    match v {
        "" | "~" | "null" | "Null" | "NULL" => return ScalarType::Null,
        "true" | "True" | "TRUE" | "false" | "False" | "FALSE" => return ScalarType::Bool,
        ".inf" | ".Inf" | ".INF" | "+.inf" | "+.Inf" | "+.INF" | "-.inf" | "-.Inf" | "-.INF"
        | ".nan" | ".NaN" | ".NAN" => return ScalarType::Float,
        _ => {}
    }
    let digits = |s: &str, radix: u32| !s.is_empty() && s.chars().all(|c| c.is_digit(radix));
    if let Some(h) = v.strip_prefix("0x") {
        return if digits(h, 16) {
            ScalarType::Integer
        } else {
            ScalarType::String
        };
    }
    if let Some(o) = v.strip_prefix("0o") {
        return if digits(o, 8) {
            ScalarType::Integer
        } else {
            ScalarType::String
        };
    }
    let unsigned = v.strip_prefix(['-', '+']).unwrap_or(v);
    if digits(unsigned, 10) {
        return ScalarType::Integer;
    }
    // [-+]? ( \. [0-9]+ | [0-9]+ ( \. [0-9]* )? ) ( [eE] [-+]? [0-9]+ )?
    let (mantissa, exponent) = match unsigned.find(['e', 'E']) {
        Some(i) => (&unsigned[..i], Some(&unsigned[i + 1..])),
        None => (unsigned, None),
    };
    let mantissa_ok = match mantissa.split_once('.') {
        Some(("", frac)) => digits(frac, 10),
        Some((int, frac)) => digits(int, 10) && (frac.is_empty() || digits(frac, 10)),
        None => digits(mantissa, 10),
    };
    let exponent_ok = exponent.is_none_or(|e| digits(e.strip_prefix(['-', '+']).unwrap_or(e), 10));
    if mantissa_ok && exponent_ok {
        ScalarType::Float
    } else {
        ScalarType::String
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(s: &str) -> Tree {
        Yaml.parse(s.as_bytes())
            .unwrap_or_else(|e| panic!("{s:?}: {e}"))
    }

    fn get<'t>(t: &'t Tree, path: &[&str]) -> &'t crate::Node {
        let p: Vec<String> = path.iter().map(|s| (*s).to_owned()).collect();
        t.node(t.get(&p).unwrap_or_else(|| panic!("{path:?}")))
    }

    #[test]
    fn structure_types_and_spans() {
        let src = "a: &x 1\nb:\n  - \"q\\u00e9\"\n  - plain multi\n    line\nc: |\n  block\n  text\nd: *x\né: ü\nf: [1, 2.5, ~, true, 0x1F, .inf, '7']\ng:\nh: !!str 12\n";
        let t = parse(src);
        let a = get(&t, &["a"]);
        assert_eq!(
            (a.kind, &src[a.value_span.clone()]),
            (NodeKind::Scalar(ScalarType::Integer), "1")
        );
        assert_eq!(&src[a.span.clone()], "a: &x 1");
        assert_eq!(get(&t, &["b", "0"]).text, "qé");
        assert_eq!(get(&t, &["b", "1"]).text, "plain multi line");
        let c = get(&t, &["c"]);
        assert_eq!(c.text, "block\ntext\n");
        assert_eq!(&src[c.value_span.clone()], "|\n  block\n  text\n");
        assert_eq!(get(&t, &["d"]).text, "1");
        let e = get(&t, &["é"]);
        assert_eq!(&src[e.value_span.clone()], "ü");
        assert_eq!(&src[e.key_span.clone().unwrap()], "é");
        let types: Vec<_> = get(&t, &["f"])
            .children
            .iter()
            .map(|&(_, c)| t.node(c).kind)
            .collect();
        let expected = [
            ScalarType::Integer,
            ScalarType::Float,
            ScalarType::Null,
            ScalarType::Bool,
            ScalarType::Integer,
            ScalarType::Float,
            ScalarType::String,
        ];
        assert_eq!(types, expected.map(NodeKind::Scalar));
        assert_eq!(
            &src[get(&t, &["f"]).value_span.clone()],
            "[1, 2.5, ~, true, 0x1F, .inf, '7']"
        );
        assert_eq!(get(&t, &["g"]).kind, NodeKind::Scalar(ScalarType::Null));
        assert_eq!(get(&t, &["h"]).kind, NodeKind::Scalar(ScalarType::String));
        assert_eq!(
            &src[get(&t, &["b"]).value_span.clone()],
            "- \"q\\u00e9\"\n  - plain multi\n    line"
        );
    }

    #[test]
    fn first_document_only() {
        let t = parse("---\na: 1\n---\nb: 2\n");
        assert_eq!(get(&t, &["a"]).text, "1");
        assert!(t.child(t.root, "b").is_none());
    }

    #[test]
    fn merge_keys_are_literal_and_complex_keys_skipped() {
        let t = parse("base: &b {x: 1}\nuse:\n  <<: *b\n  y: 2\n? [k]\n: v\nz: 3\n");
        let keys: Vec<_> = get(&t, &["use"])
            .children
            .iter()
            .map(|(k, _)| k.as_str())
            .collect();
        assert_eq!(keys, ["<<", "y"]);
        assert_eq!(get(&t, &["use", "<<", "x"]).text, "1");
        let top: Vec<_> = t
            .node(t.root)
            .children
            .iter()
            .map(|(k, _)| k.as_str())
            .collect();
        assert_eq!(top, ["base", "use", "z"]);
    }

    #[test]
    fn block_headers() {
        let src = "a: >- # c|\n  folded\n  text\nb: |2+\n    x\n";
        let t = parse(src);
        assert_eq!(&src[get(&t, &["a"]).value_span.clone()][..4], ">- #");
        assert!(src[get(&t, &["b"]).value_span.clone()].starts_with("|2+"));
    }

    #[test]
    fn errors_and_empty() {
        assert!(Yaml.parse(b"a: [1\n").is_err());
        assert!(Yaml.parse(b"a: *nope\n").is_err());
        assert!(Yaml.parse(b"a: &x [*x]\n").is_err());
        assert_eq!(
            parse("").node(NodeId(0)).kind,
            NodeKind::Scalar(ScalarType::Null)
        );
        assert_eq!(
            parse("# only\n").node(NodeId(0)).kind,
            NodeKind::Scalar(ScalarType::Null)
        );
    }

    #[test]
    fn core_schema_cases() {
        use ScalarType::{Bool, Float, Integer, Null, String};
        for (v, ty) in [
            ("1", Integer),
            ("-1", Integer),
            ("+1", Integer),
            ("0o17", Integer),
            ("0x1f", Integer),
            ("1.", Float),
            (".5", Float),
            ("1e3", Float),
            ("-1.5E-3", Float),
            (".nan", Float),
            ("yes", String),
            ("1.2.3", String),
            ("0x", String),
            ("e3", String),
            ("1e", String),
            ("on", String),
            ("", Null),
            ("TRUE", Bool),
        ] {
            assert_eq!(core_schema(v), ty, "{v:?}");
        }
    }
}
