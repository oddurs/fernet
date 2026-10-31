//! JSON, with JSONC comments and trailing commas: a small span-tracking
//! recursive-descent parser.

use std::fmt::Write as _;

use crate::{
    Builder, Caps, Driver, DriverError, Edit, MAX_DEPTH, NewValue, Node, NodeId, NodeKind,
    ScalarType, Tree,
};

/// The JSON driver. Accepts `//` and `/* */` comments and trailing commas
/// everywhere (tsconfig and VS Code settings need both).
#[derive(Clone, Copy, Debug, Default)]
pub struct Json;

impl Driver for Json {
    fn name(&self) -> &'static str {
        "json"
    }

    fn extensions(&self) -> &'static [&'static str] {
        &["json", "jsonc", "code-workspace"]
    }

    fn sniff(&self, head: &[u8]) -> bool {
        let Ok((s, start)) = crate::text(head).or_else(|e| match e {
            // A 4 KiB head may cut a character in half.
            DriverError::NotUtf8 { offset } if offset + 4 >= head.len() => {
                std::str::from_utf8(&head[..offset])
                    .map(|s| (s, 0))
                    .map_err(|_| e)
            }
            e => Err(e),
        }) else {
            return false;
        };
        let mut p = Parser {
            src: s,
            pos: start,
            depth: 0,
            b: Builder::default(),
        };
        if p.skip_trivia().is_err() {
            return false;
        }
        matches!(p.peek(), Some(b'{' | b'['))
            && crate::first_line(s, "//").is_some_and(|l| !l.starts_with("[["))
    }

    fn parse(&self, src: &[u8]) -> Result<Tree, DriverError> {
        let (s, start) = crate::text(src)?;
        let mut p = Parser {
            src: s,
            pos: start,
            depth: 0,
            b: Builder::default(),
        };
        p.skip_trivia()?;
        let root = p.value()?;
        p.skip_trivia()?;
        if p.pos < s.len() {
            return Err(p.err("unexpected characters after the document"));
        }
        Ok(p.b.finish(root))
    }

    fn splice(&self, src: &[u8], ptr: &[String], edit: Edit) -> Result<Vec<u8>, DriverError> {
        let tree = self.parse(src)?;
        let id = tree.get(ptr).ok_or(DriverError::NotFound)?;
        match edit {
            Edit::SetScalar(text) => {
                let node = tree.node(id);
                let NodeKind::Scalar(ty) = node.kind else {
                    return Err(DriverError::WrongKind("not a value"));
                };
                let text = crate::strip_newline(&text);
                let literal = match ty {
                    ScalarType::String | ScalarType::Text => quote(text),
                    // JSON has one number type: integers and floats interchange.
                    ScalarType::Integer | ScalarType::Float if is_number(text) => text.to_owned(),
                    ScalarType::Bool if text == "true" || text == "false" => text.to_owned(),
                    ScalarType::Null if text == "null" => text.to_owned(),
                    _ => {
                        return Err(DriverError::InvalidValue(format!(
                            "{text:?} isn't a json {}",
                            crate::type_name(ty)
                        )));
                    }
                };
                let out = crate::replace(src, node.value_span.clone(), &literal);
                let want = match ty {
                    ScalarType::Integer | ScalarType::Float => number_type(text),
                    _ => ty,
                };
                crate::check_scalar(self, &out, ptr, want, text)?;
                Ok(out)
            }
            Edit::Insert { key, value } => {
                let out = insert(src, &tree, id, &key, &value)?;
                self.parse(&out)
                    .map_err(|_| DriverError::Unsupported("the edit would produce invalid json"))?;
                Ok(out)
            }
            Edit::Remove => {
                let out = remove(src, &tree, ptr)?;
                let after = self
                    .parse(&out)
                    .map_err(|_| DriverError::Unsupported("the edit would produce invalid json"))?;
                // A duplicated key: removing one occurrence would bring back
                // another value. Refuse rather than report a removal that
                // didn't happen.
                let in_object = tree
                    .get(&ptr[..ptr.len().saturating_sub(1)])
                    .is_some_and(|p| tree.node(p).kind == NodeKind::Object);
                if in_object && after.get(ptr).is_some() {
                    return Err(DriverError::Unsupported(
                        "the key appears more than once; remove the duplicates by hand",
                    ));
                }
                Ok(out)
            }
        }
    }

    fn capabilities(&self) -> Caps {
        Caps::FULL
    }
}

/// A JSON string literal for `text`: escapes quotes, backslashes and control
/// characters, and nothing else.
fn quote(text: &str) -> String {
    let mut out = String::with_capacity(text.len() + 2);
    out.push('"');
    for c in text.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\u{8}' => out.push_str("\\b"),
            '\u{c}' => out.push_str("\\f"),
            c if c < ' ' => drop(write!(out, "\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// True if `text` is exactly a JSON number.
fn is_number(text: &str) -> bool {
    let mut p = Parser {
        src: text,
        pos: 0,
        depth: 0,
        b: Builder::default(),
    };
    matches!(text.as_bytes().first(), Some(b'-' | b'0'..=b'9'))
        && p.number().is_ok()
        && p.pos == text.len()
}

fn number_type(text: &str) -> ScalarType {
    if text.contains(['.', 'e', 'E']) {
        ScalarType::Float
    } else {
        ScalarType::Integer
    }
}

/// The literal for a new value: numbers, booleans and null as written,
/// anything else as a string.
fn new_literal(value: &NewValue) -> String {
    match value {
        NewValue::EmptyObject => "{}".into(),
        NewValue::EmptyArray => "[]".into(),
        NewValue::Text(t) => {
            let t = crate::strip_newline(t);
            if is_number(t) || matches!(t, "true" | "false" | "null") {
                t.to_owned()
            } else {
                quote(t)
            }
        }
    }
}

/// Inserts `key` into the container `id`, matching the last sibling's layout:
/// same line or one per line, its indentation, its `:` spacing, and a trailing
/// comma if it has one.
fn insert(
    src: &[u8],
    tree: &Tree,
    id: NodeId,
    key: &str,
    value: &NewValue,
) -> Result<Vec<u8>, DriverError> {
    let (s, _) = crate::text(src)?;
    let node = tree.node(id);
    let object = match node.kind {
        NodeKind::Object => true,
        NodeKind::Array => false,
        NodeKind::Scalar(_) => return Err(DriverError::WrongKind("not an object or array")),
    };
    if object && tree.child(id, key).is_some() {
        return Err(DriverError::Exists);
    }
    if !object && key != node.children.len().to_string() {
        return Err(DriverError::InvalidValue(format!(
            "arrays grow at the end: the next index is {}",
            node.children.len()
        )));
    }
    let literal = new_literal(value);
    let open = node.value_span.start;
    let close = node.value_span.end - 1;
    let Some(last) = node
        .children
        .iter()
        .map(|&(_, c)| tree.node(c))
        .max_by_key(|n| n.span.end)
    else {
        let member = if object {
            format!("{}: {literal}", quote(key))
        } else {
            literal
        };
        return Ok(crate::replace(src, open + 1..close, &member));
    };
    let first = node
        .children
        .iter()
        .map(|&(_, c)| tree.node(c))
        .min_by_key(|n| n.span.start)
        .unwrap_or(last);
    let multiline = s[open..first.span.start].contains('\n');
    let indent = if multiline {
        // Only the leading whitespace: the line may hold earlier members.
        let line_start = s[..last.span.start].rfind('\n').map_or(0, |i| i + 1);
        let line = &s[line_start..last.span.start];
        line[..line.len() - line.trim_start().len()].to_owned()
    } else {
        String::new()
    };
    let sep = match &last.key_span {
        Some(k) if object => s[k.end..last.value_span.start].to_owned(),
        _ => ": ".to_owned(),
    };
    let member = if object {
        format!("{}{sep}{literal}", quote(key))
    } else {
        literal
    };
    let gap = if multiline {
        format!("\n{indent}")
    } else {
        " ".to_owned()
    };
    // A trailing comma after the last member (JSONC) stays trailing.
    let after = &s[last.span.end..close];
    let trimmed = after.trim_start();
    if trimmed.starts_with(',') {
        let comma = last.span.end + (after.len() - trimmed.len()) + 1;
        return Ok(crate::replace(
            src,
            comma..comma,
            &format!("{gap}{member},"),
        ));
    }
    Ok(crate::replace(
        src,
        last.span.end..last.span.end,
        &format!(",{gap}{member}"),
    ))
}

/// Removes the node at `ptr` with one adjoining comma.
fn remove(src: &[u8], tree: &Tree, ptr: &[String]) -> Result<Vec<u8>, DriverError> {
    let (_, parent_ptr) = ptr
        .split_last()
        .ok_or(DriverError::Unsupported("can't remove the whole document"))?;
    let parent = tree.get(parent_ptr).ok_or(DriverError::NotFound)?;
    let target = tree.get(ptr).ok_or(DriverError::NotFound)?;
    let mut siblings: Vec<&Node> = tree
        .node(parent)
        .children
        .iter()
        .map(|&(_, c)| tree.node(c))
        .collect();
    siblings.sort_by_key(|n| n.span.start);
    let t = tree.node(target);
    let i = siblings
        .iter()
        .position(|n| std::ptr::eq(*n, t))
        .ok_or(DriverError::NotFound)?;
    let range = match (siblings.get(i.wrapping_sub(1)), siblings.get(i + 1)) {
        (_, Some(next)) => t.span.start..next.span.start,
        (Some(prev), None) => prev.span.end..t.span.end,
        (None, None) => {
            let p = tree.node(parent);
            p.value_span.start + 1..p.value_span.end - 1
        }
    };
    Ok(crate::replace(src, range, ""))
}

struct Parser<'a> {
    src: &'a str,
    pos: usize,
    depth: usize,
    b: Builder,
}

impl Parser<'_> {
    fn err(&self, message: &str) -> DriverError {
        DriverError::syntax(self.src, self.pos, message)
    }

    fn peek(&self) -> Option<u8> {
        self.src.as_bytes().get(self.pos).copied()
    }

    fn eat(&mut self, c: u8) -> bool {
        let hit = self.peek() == Some(c);
        if hit {
            self.pos += 1;
        }
        hit
    }

    fn expect(&mut self, c: u8, what: &str) -> Result<(), DriverError> {
        if self.eat(c) {
            Ok(())
        } else {
            Err(self.err(&format!("expected {what}")))
        }
    }

    /// Skips whitespace and comments.
    fn skip_trivia(&mut self) -> Result<(), DriverError> {
        let bytes = self.src.as_bytes();
        loop {
            match bytes.get(self.pos..self.pos + 2) {
                Some(b"//") => {
                    self.pos = self.src[self.pos..]
                        .find('\n')
                        .map_or(self.src.len(), |i| self.pos + i);
                }
                Some(b"/*") => {
                    let Some(i) = self.src[self.pos + 2..].find("*/") else {
                        return Err(self.err("unterminated comment"));
                    };
                    self.pos += i + 4;
                }
                _ => match self.peek() {
                    Some(b' ' | b'\t' | b'\n' | b'\r') => self.pos += 1,
                    _ => return Ok(()),
                },
            }
        }
    }

    fn value(&mut self) -> Result<NodeId, DriverError> {
        match self.peek() {
            Some(b'{') => self.container(true),
            Some(b'[') => self.container(false),
            Some(b'"') => {
                let start = self.pos;
                let text = self.string()?;
                Ok(self
                    .b
                    .push(NodeKind::Scalar(ScalarType::String), start..self.pos, text))
            }
            Some(b't') => self.literal("true", ScalarType::Bool),
            Some(b'f') => self.literal("false", ScalarType::Bool),
            Some(b'n') => self.literal("null", ScalarType::Null),
            Some(b'-' | b'0'..=b'9') => self.number(),
            Some(_) => Err(self.err("expected a value")),
            None => Err(self.err("unexpected end of input")),
        }
    }

    fn literal(&mut self, word: &str, ty: ScalarType) -> Result<NodeId, DriverError> {
        let start = self.pos;
        if !self.src[start..].starts_with(word) {
            return Err(self.err("expected a value"));
        }
        self.pos += word.len();
        Ok(self
            .b
            .push(NodeKind::Scalar(ty), start..self.pos, word.to_owned()))
    }

    fn digits(&mut self) -> usize {
        let start = self.pos;
        while matches!(self.peek(), Some(b'0'..=b'9')) {
            self.pos += 1;
        }
        self.pos - start
    }

    fn number(&mut self) -> Result<NodeId, DriverError> {
        let start = self.pos;
        self.eat(b'-');
        let int_start = self.pos;
        match self.digits() {
            0 => return Err(self.err("expected a digit")),
            n if n > 1 && self.src.as_bytes()[int_start] == b'0' => {
                self.pos = int_start + 1;
                return Err(self.err("leading zeros aren't allowed"));
            }
            _ => {}
        }
        let mut float = false;
        if self.eat(b'.') {
            float = true;
            if self.digits() == 0 {
                return Err(self.err("expected a digit after '.'"));
            }
        }
        if matches!(self.peek(), Some(b'e' | b'E')) {
            float = true;
            self.pos += 1;
            if !self.eat(b'+') {
                self.eat(b'-');
            }
            if self.digits() == 0 {
                return Err(self.err("expected a digit in the exponent"));
            }
        }
        let ty = if float {
            ScalarType::Float
        } else {
            ScalarType::Integer
        };
        Ok(self.b.push(
            NodeKind::Scalar(ty),
            start..self.pos,
            self.src[start..self.pos].to_owned(),
        ))
    }

    fn hex4(&mut self) -> Result<u32, DriverError> {
        let h = self
            .src
            .get(self.pos..self.pos + 4)
            .filter(|h| h.bytes().all(|b| b.is_ascii_hexdigit()))
            .and_then(|h| u32::from_str_radix(h, 16).ok())
            .ok_or_else(|| self.err("expected four hex digits"))?;
        self.pos += 4;
        Ok(h)
    }

    /// Parses a string literal at `pos` and returns its decoded value.
    fn string(&mut self) -> Result<String, DriverError> {
        self.expect(b'"', "'\"'")?;
        let mut out = String::new();
        loop {
            let rest = &self.src[self.pos..];
            let Some(i) = rest.find(|c: char| c == '"' || c == '\\' || c < '\u{20}') else {
                return Err(DriverError::syntax(
                    self.src,
                    self.src.len(),
                    "unterminated string",
                ));
            };
            out.push_str(&rest[..i]);
            self.pos += i;
            match self.src.as_bytes()[self.pos] {
                b'"' => {
                    self.pos += 1;
                    return Ok(out);
                }
                b'\\' => {
                    self.pos += 1;
                    let Some(c) = self.peek() else {
                        return Err(self.err("unterminated string"));
                    };
                    self.pos += 1;
                    match c {
                        b'"' => out.push('"'),
                        b'\\' => out.push('\\'),
                        b'/' => out.push('/'),
                        b'b' => out.push('\u{8}'),
                        b'f' => out.push('\u{c}'),
                        b'n' => out.push('\n'),
                        b'r' => out.push('\r'),
                        b't' => out.push('\t'),
                        b'u' => out.push(self.unicode_escape()?),
                        _ => {
                            self.pos -= 1;
                            return Err(self.err("invalid escape"));
                        }
                    }
                }
                _ => return Err(self.err("control character in string")),
            }
        }
    }

    fn unicode_escape(&mut self) -> Result<char, DriverError> {
        let hi = self.hex4()?;
        let code = if (0xD800..0xDC00).contains(&hi) {
            if !self.src[self.pos..].starts_with("\\u") {
                return Err(self.err("unpaired surrogate"));
            }
            self.pos += 2;
            let lo = self.hex4()?;
            if !(0xDC00..0xE000).contains(&lo) {
                return Err(self.err("unpaired surrogate"));
            }
            0x10000 + ((hi - 0xD800) << 10) + (lo - 0xDC00)
        } else {
            hi
        };
        char::from_u32(code).ok_or_else(|| self.err("unpaired surrogate"))
    }

    fn container(&mut self, object: bool) -> Result<NodeId, DriverError> {
        self.depth += 1;
        if self.depth > MAX_DEPTH {
            return Err(DriverError::TooDeep);
        }
        let start = self.pos;
        self.pos += 1;
        let kind = if object {
            NodeKind::Object
        } else {
            NodeKind::Array
        };
        let id = self.b.push(kind, start..start, String::new());
        let close = if object { b'}' } else { b']' };
        loop {
            self.skip_trivia()?;
            if self.eat(close) {
                break;
            }
            if object {
                self.member(id)?;
            } else {
                let child = self.value()?;
                self.b.push_item(id, child);
            }
            self.skip_trivia()?;
            if self.eat(close) {
                break;
            }
            self.expect(b',', if object { "',' or '}'" } else { "',' or ']'" })?;
        }
        let span = start..self.pos;
        let node = self.b.node_mut(id);
        node.span = span.clone();
        node.value_span = span;
        self.depth -= 1;
        Ok(id)
    }

    fn member(&mut self, object: NodeId) -> Result<(), DriverError> {
        if self.peek() != Some(b'"') {
            return Err(self.err("expected a key string or '}'"));
        }
        let key_start = self.pos;
        let key = self.string()?;
        let key_span = key_start..self.pos;
        self.skip_trivia()?;
        self.expect(b':', "':'")?;
        self.skip_trivia()?;
        let child = self.value()?;
        let node = self.b.node_mut(child);
        node.span = key_start..node.value_span.end;
        node.key_span = Some(key_span);
        self.b.add_child(object, key, child);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(s: &str) -> Tree {
        Json.parse(s.as_bytes())
            .unwrap_or_else(|e| panic!("{s:?}: {e}"))
    }

    fn leaf(t: &Tree, path: &[&str]) -> (NodeKind, String) {
        let p: Vec<String> = path.iter().map(|s| (*s).to_owned()).collect();
        let n = t.node(t.get(&p).unwrap());
        (n.kind, n.text.clone())
    }

    #[test]
    fn values() {
        let src =
            r#"{"a": {"b": [1, -2.5e3, "x\ty\u00e9\ud83d\ude00"]}, "t": true, "n": null, "": 0}"#;
        let t = parse(src);
        assert_eq!(
            leaf(&t, &["a", "b", "0"]),
            (NodeKind::Scalar(ScalarType::Integer), "1".into())
        );
        assert_eq!(
            leaf(&t, &["a", "b", "1"]),
            (NodeKind::Scalar(ScalarType::Float), "-2.5e3".into())
        );
        assert_eq!(leaf(&t, &["a", "b", "2"]).1, "x\tyé😀");
        assert_eq!(
            leaf(&t, &["t"]),
            (NodeKind::Scalar(ScalarType::Bool), "true".into())
        );
        assert_eq!(
            leaf(&t, &["n"]),
            (NodeKind::Scalar(ScalarType::Null), "null".into())
        );
        assert_eq!(leaf(&t, &[""]).1, "0");
        let keys: Vec<_> = t
            .node(t.root)
            .children
            .iter()
            .map(|(k, _)| k.as_str())
            .collect();
        assert_eq!(keys, ["a", "t", "n", ""]);
    }

    #[test]
    fn spans() {
        let src = "{ \"k\" : \"v\" , \"o\": {\"x\": 1} }";
        let t = parse(src);
        let k = t.node(t.child(t.root, "k").unwrap());
        assert_eq!(&src[k.value_span.clone()], "\"v\"");
        assert_eq!(&src[k.span.clone()], "\"k\" : \"v\"");
        assert_eq!(&src[k.key_span.clone().unwrap()], "\"k\"");
        let o = t.node(t.child(t.root, "o").unwrap());
        assert_eq!(&src[o.value_span.clone()], "{\"x\": 1}");
        assert_eq!(&src[t.node(t.root).span.clone()], src);
    }

    #[test]
    fn jsonc() {
        let src = "\u{feff}// top\n{\n  /* a */ \"a\": [1, 2,], // tail\n  \"b\": 2,\n}\n";
        let t = parse(src);
        assert_eq!(leaf(&t, &["a", "1"]).1, "2");
        assert_eq!(t.node(t.child(t.root, "a").unwrap()).children.len(), 2);
    }

    #[test]
    fn duplicates_first_position_last_value() {
        let t = parse(r#"{"a": 1, "b": 2, "a": 3}"#);
        let keys: Vec<_> = t
            .node(t.root)
            .children
            .iter()
            .map(|(k, _)| k.as_str())
            .collect();
        assert_eq!(keys, ["a", "b"]);
        assert_eq!(leaf(&t, &["a"]).1, "3");
    }

    #[test]
    fn errors() {
        for bad in [
            "",
            "{",
            "[1,,]",
            "{\"a\"}",
            "01",
            "1.",
            "-",
            "1e",
            "\"\\x\"",
            "\"a",
            "\"\u{1}\"",
            "\"\\ud800\"",
            "\"\\udc00\"",
            "{} x",
            "/* open",
            "[,]",
            "{,}",
            "tru",
            "'a'",
            "+1",
            "NaN",
        ] {
            assert!(Json.parse(bad.as_bytes()).is_err(), "{bad:?}");
        }
        assert_eq!(
            Json.parse(&"[".repeat(MAX_DEPTH + 1).into_bytes()),
            Err(DriverError::TooDeep)
        );
        assert!(
            Json.parse(format!("{}{}", "[".repeat(MAX_DEPTH), "]".repeat(MAX_DEPTH)).as_bytes())
                .is_ok()
        );
        let Err(DriverError::Syntax { line, column, .. }) = Json.parse(b"{\n  \"a\" 1}") else {
            panic!()
        };
        assert_eq!((line, column), (2, 7));
    }

    #[test]
    fn scalar_roots() {
        assert_eq!(parse(" 42 ").node(NodeId(0)).text, "42");
        assert_eq!(parse("\"s\"").render(NodeId(0)).unwrap(), b"s\n");
    }
}
