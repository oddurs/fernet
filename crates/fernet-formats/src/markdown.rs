//! Markdown: headings become directories, nested by level; each section's
//! text before its first child heading is a file named `body`.

use std::ops::Range;

use pulldown_cmark::{Event, HeadingLevel, Parser, Tag, TagEnd};

use crate::{
    Builder, Caps, Driver, DriverError, Edit, NewValue, NodeId, NodeKind, ScalarType, Tree,
};

/// The Markdown driver.
#[derive(Clone, Copy, Debug, Default)]
pub struct Markdown;

/// The name of a section's own text.
pub(crate) const BODY: &str = "body";

struct Heading {
    level: usize,
    /// The heading's markup, including its line ending.
    range: Range<usize>,
    text: String,
}

/// Every heading, in order.
fn headings(s: &str) -> Vec<Heading> {
    let mut out = Vec::new();
    let mut current: Option<(usize, Range<usize>, String)> = None;
    for (ev, range) in Parser::new(s).into_offset_iter() {
        match ev {
            Event::Start(Tag::Heading { level, .. }) => {
                current = Some((level_num(level), range, String::new()));
            }
            Event::Text(t) | Event::Code(t) => {
                if let Some((_, _, text)) = &mut current {
                    text.push_str(&t);
                }
            }
            Event::End(TagEnd::Heading(_)) => {
                if let Some((level, range, text)) = current.take() {
                    // End right after the heading's own line, so a body starts on
                    // its own line. (Ranges sometimes include the newline.)
                    let end = if s[..range.end].ends_with('\n') {
                        range.end
                    } else {
                        s[range.end..]
                            .find('\n')
                            .map_or(s.len(), |i| range.end + i + 1)
                    };
                    // Start at the line's start: a heading inside a list item
                    // or blockquote keeps its `- ` or `> ` with it, not with
                    // the body before it.
                    let start = s[..range.start].rfind('\n').map_or(0, |i| i + 1);
                    out.push(Heading {
                        level,
                        range: start..end,
                        text: text.trim().to_owned(),
                    });
                }
            }
            _ => {}
        }
    }
    out
}

fn level_num(l: HeadingLevel) -> usize {
    match l {
        HeadingLevel::H1 => 1,
        HeadingLevel::H2 => 2,
        HeadingLevel::H3 => 3,
        HeadingLevel::H4 => 4,
        HeadingLevel::H5 => 5,
        HeadingLevel::H6 => 6,
    }
}

/// A body's text: its slice minus one trailing newline (the file adds it back).
fn body_text(slice: &str) -> String {
    slice.strip_suffix('\n').unwrap_or(slice).to_owned()
}

impl Driver for Markdown {
    fn name(&self) -> &'static str {
        "markdown"
    }

    fn extensions(&self) -> &'static [&'static str] {
        &["md", "markdown", "mdown", "mkd"]
    }

    fn sniff(&self, _head: &[u8]) -> bool {
        // Plain text often looks like Markdown; only the extension decides.
        false
    }

    fn parse(&self, src: &[u8]) -> Result<Tree, DriverError> {
        let (s, start) = crate::text(src)?;
        // Parse after a byte-order mark, and shift ranges back.
        let hs: Vec<Heading> = headings(&s[start..])
            .into_iter()
            .map(|h| Heading {
                range: h.range.start + start..h.range.end + start,
                ..h
            })
            .collect();
        let mut b = Builder::default();
        let root = b.push(NodeKind::Object, 0..s.len(), String::new());
        // Stack of (level, node).
        let mut stack: Vec<(usize, NodeId)> = vec![(0, root)];
        let first = hs.first().map_or(s.len(), |h| h.range.start);
        let body = b.push(
            NodeKind::Scalar(ScalarType::Text),
            start..first,
            body_text(&s[start..first]),
        );
        b.add_child(root, BODY.to_owned(), body);
        for (i, h) in hs.iter().enumerate() {
            while stack.last().is_some_and(|&(l, _)| l >= h.level) {
                stack.pop();
            }
            let parent = stack.last().map_or(root, |&(_, n)| n);
            let end = hs[i + 1..]
                .iter()
                .find(|n| n.level <= h.level)
                .map_or(s.len(), |n| n.range.start);
            let body_end = hs.get(i + 1).map_or(s.len(), |n| n.range.start);
            let section = b.push(NodeKind::Object, h.range.start..end, String::new());
            b.node_mut(section).span = h.range.start..end;
            b.node_mut(section).key_span = Some(h.range.start..h.range.end.min(end));
            // A heading's line can run into the next heading (headings in
            // lists): clamp so spans never cross.
            let body_start = h.range.end.min(body_end);
            let body = b.push(
                NodeKind::Scalar(ScalarType::Text),
                body_start..body_end,
                body_text(&s[body_start..body_end]),
            );
            b.add_child(section, BODY.to_owned(), body);
            b.add_child(parent, unique_key(&b, parent, &h.text), section);
            stack.push((h.level, section));
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
                if node.kind != NodeKind::Scalar(ScalarType::Text) {
                    return Err(DriverError::WrongKind(
                        "only a section's body can be written",
                    ));
                }
                let text = crate::strip_newline(&text);
                let span = node.value_span.clone();
                // Keep the line ending before whatever follows (a heading).
                let newline = s[span.clone()].ends_with('\n') || span.end < s.len();
                // A heading at the very end with no newline: the body starts
                // on the next line.
                let lead = if span.is_empty()
                    && span.start == s.len()
                    && !s.is_empty()
                    && !s.ends_with('\n')
                {
                    "\n"
                } else {
                    ""
                };
                let literal = if newline {
                    format!("{lead}{text}\n")
                } else {
                    format!("{lead}{text}")
                };
                let out = crate::replace(src, span, &literal);
                crate::check_scalar(self, &out, ptr, ScalarType::Text, text)?;
                same_elsewhere(&leaves(&tree), &leaves(&self.parse(&out)?), ptr)?;
                return Ok(out);
            }
            Edit::Insert { key, value } => {
                return self.insert(src, s, &tree, ptr, id, &key, &value);
            }
            Edit::Remove => {
                let Some((last, _)) = ptr.split_last() else {
                    return Err(DriverError::Unsupported("can't remove the whole document"));
                };
                if last == BODY {
                    // A section always has a body: removing it empties it.
                    let span = node.value_span.clone();
                    let keep = if span.end < s.len() && !span.is_empty() {
                        "\n"
                    } else {
                        ""
                    };
                    crate::replace(src, span, keep)
                } else {
                    let out = crate::replace(src, node.span.clone(), "");
                    same_elsewhere(&leaves(&tree), &leaves(&self.parse(&out)?), ptr)?;
                    return Ok(out);
                }
            }
        };
        self.parse(&out)?;
        Ok(out)
    }

    fn capabilities(&self) -> Caps {
        Caps::FULL
    }
}

impl Markdown {
    /// Appends a section `key` under the section at `ptr`, one level below
    /// it, and checks that nothing else moved.
    #[allow(clippy::too_many_arguments)] // The parsed state splice already has.
    fn insert(
        self,
        src: &[u8],
        s: &str,
        tree: &Tree,
        ptr: &[String],
        id: NodeId,
        key: &str,
        value: &NewValue,
    ) -> Result<Vec<u8>, DriverError> {
        let node = tree.node(id);
        if node.kind != NodeKind::Object {
            return Err(DriverError::WrongKind("not a section"));
        }
        if tree.child(id, key).is_some() {
            return Err(DriverError::Exists);
        }
        if key.contains('\n') || key.trim().is_empty() {
            return Err(DriverError::InvalidValue(
                "a heading is one non-empty line".into(),
            ));
        }
        // One level below the parent heading (the root is level 0).
        let parent_level = if ptr.is_empty() {
            0
        } else {
            headings(s)
                .iter()
                .find(|h| h.range.start == node.span.start)
                .map_or(0, |h| h.level)
        };
        if parent_level >= 6 {
            return Err(DriverError::Unsupported(
                "markdown headings stop at level 6",
            ));
        }
        let level = parent_level + 1;
        let body = match value {
            NewValue::EmptyObject => String::new(),
            NewValue::Text(t) => format!("\n{}\n", crate::strip_newline(t)),
            NewValue::EmptyArray => {
                return Err(DriverError::InvalidValue(
                    "markdown has no lists of sections".into(),
                ));
            }
        };
        let at = node.span.end;
        let lead = if at == 0 || s[..at].ends_with("\n\n") {
            ""
        } else if s[..at].ends_with('\n') {
            "\n"
        } else {
            "\n\n"
        };
        let trail = if at < s.len() { "\n" } else { "" };
        let heading = format!("{lead}{} {}\n{body}{trail}", "#".repeat(level), key.trim());
        let out = crate::replace(src, at..at, &heading);
        // The new section must be exactly what was asked for, and
        // nothing else may move (markup in the key, headings in the body).
        let after = self.parse(&out)?;
        let mut child = ptr.to_vec();
        child.push(key.to_owned());
        let mut body_ptr = child.clone();
        body_ptr.push(BODY.to_owned());
        let new_body = after.get(&body_ptr).map(|id| after.node(id).text.clone());
        // What the body reads back as: its slice minus one newline.
        let want = body_text(&format!("{body}{trail}"));
        if new_body.as_deref() != Some(want.as_str()) {
            return Err(DriverError::InvalidValue(
                "that heading or body would change the document's structure".into(),
            ));
        }
        same_elsewhere(&leaves(tree), &leaves(&after), &child)?;
        Ok(out)
    }
}

/// Every body, by pointer.
fn leaves(t: &Tree) -> std::collections::BTreeMap<Vec<String>, String> {
    fn walk(
        t: &Tree,
        id: NodeId,
        p: &mut Vec<String>,
        out: &mut std::collections::BTreeMap<Vec<String>, String>,
    ) {
        let n = t.node(id);
        if let NodeKind::Scalar(_) = n.kind {
            out.insert(p.clone(), n.text.clone());
        }
        for (k, c) in &n.children {
            p.push(k.clone());
            walk(t, *c, p, out);
            p.pop();
        }
    }
    let mut out = std::collections::BTreeMap::new();
    walk(t, t.root, &mut Vec::new(), &mut out);
    out
}

/// Checks that every body outside `ptr` (the edited node and anything below
/// it) is unchanged.
fn same_elsewhere(
    before: &std::collections::BTreeMap<Vec<String>, String>,
    after: &std::collections::BTreeMap<Vec<String>, String>,
    ptr: &[String],
) -> Result<(), DriverError> {
    let outside = |m: &std::collections::BTreeMap<Vec<String>, String>| {
        m.iter()
            .filter(|(k, _)| !k.starts_with(ptr))
            .map(|(k, v)| (k.clone(), v.trim_end_matches('\n').to_owned()))
            .collect::<Vec<_>>()
    };
    if outside(before) == outside(after) {
        Ok(())
    } else {
        Err(DriverError::InvalidValue(
            "the edit would change other sections of the document".into(),
        ))
    }
}

/// `name`, or `name (2)`, `name (3)`… if taken (`body` is always taken).
fn unique_key(b: &Builder, parent: NodeId, name: &str) -> String {
    let taken = |k: &str| b.has_child(parent, k);
    if !taken(name) && name != BODY {
        return name.to_owned();
    }
    // Each name can collide at most once per existing child, so this ends.
    (2..=b.child_count(parent) + 2)
        .map(|n| format!("{name} ({n})"))
        .find(|k| !taken(k))
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    const DOC: &str = "Intro text.\n\n# Title\n\nAbout.\n\n## Install\n\nRun it.\n\n## Usage\n\nUse it.\n\n### Flags\n\n`-v`\n\n# Other\n\nEnd.\n";

    fn keys(t: &Tree, id: NodeId) -> Vec<&str> {
        t.node(id)
            .children
            .iter()
            .map(|(k, _)| k.as_str())
            .collect()
    }

    fn get<'t>(t: &'t Tree, p: &[&str]) -> &'t crate::Node {
        let p: Vec<String> = p.iter().map(|s| (*s).to_owned()).collect();
        t.node(t.get(&p).unwrap_or_else(|| panic!("{p:?}")))
    }

    #[test]
    fn sections_nest_by_level() {
        let t = Markdown.parse(DOC.as_bytes()).unwrap();
        assert_eq!(keys(&t, t.root), ["body", "Title", "Other"]);
        assert_eq!(get(&t, &["body"]).text, "Intro text.\n");
        let title = t.get(&["Title".into()]).unwrap();
        assert_eq!(keys(&t, title), ["body", "Install", "Usage"]);
        assert_eq!(
            get(&t, &["Title", "Usage", "Flags", "body"]).text,
            "\n`-v`\n"
        );
        assert_eq!(get(&t, &["Title", "Install", "body"]).text, "\nRun it.\n");
        assert_eq!(
            &DOC[get(&t, &["Title", "Usage"]).span.clone()],
            "## Usage\n\nUse it.\n\n### Flags\n\n`-v`\n\n"
        );
    }

    #[test]
    fn duplicate_and_reserved_names() {
        let t = Markdown.parse(b"# A\n# A\n# body\n").unwrap();
        assert_eq!(keys(&t, t.root), ["body", "A", "A (2)", "body (2)"]);
    }

    #[test]
    fn writes() {
        let set = |p: &[&str], v: &str| {
            let p: Vec<String> = p.iter().map(|s| (*s).to_owned()).collect();
            String::from_utf8(
                Markdown
                    .splice(DOC.as_bytes(), &p, Edit::SetScalar(v.into()))
                    .unwrap(),
            )
            .unwrap()
        };
        assert_eq!(
            set(&["Title", "Install", "body"], "\nUse cargo.\n\n"),
            DOC.replace("Run it.", "Use cargo.")
        );
        let p = |v: &[&str]| v.iter().map(|s| (*s).to_owned()).collect::<Vec<_>>();
        // A body that would become a heading is refused.
        assert!(
            Markdown
                .splice(
                    DOC.as_bytes(),
                    &p(&["Title", "Install", "body"]),
                    Edit::SetScalar("# sneaky\n".into())
                )
                .is_err()
        );
        let ins = Markdown
            .splice(
                DOC.as_bytes(),
                &p(&["Title"]),
                Edit::Insert {
                    key: "License".into(),
                    value: NewValue::Text("MIT".into()),
                },
            )
            .unwrap();
        let t = Markdown.parse(&ins).unwrap();
        assert_eq!(get(&t, &["Title", "License", "body"]).text, "\nMIT\n");
        assert_eq!(
            keys(&t, t.get(&p(&["Title"])).unwrap()),
            ["body", "Install", "Usage", "License"]
        );
        let rm = Markdown
            .splice(DOC.as_bytes(), &p(&["Title", "Usage"]), Edit::Remove)
            .unwrap();
        assert_eq!(
            String::from_utf8(rm).unwrap(),
            DOC.replace("## Usage\n\nUse it.\n\n### Flags\n\n`-v`\n\n", "")
        );
    }
}
