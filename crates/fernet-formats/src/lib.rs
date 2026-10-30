//! Span-tracking format drivers for fernet.
//!
//! A [`Driver`] parses a structured file (JSON, TOML, YAML, …) into a [`Tree`]
//! whose nodes record exact byte spans in the source. Reads walk the tree; the
//! spans are what later make writes into byte splices (invariant 4).
//!
//! Mapping to the filesystem: objects and arrays are directories (array
//! entries are named `0`, `1`, …); scalars are regular files containing
//! [`Node::text`] plus one trailing `\n`.
#![forbid(unsafe_code)]

mod json;
mod toml;
mod yaml;

use std::collections::HashMap;
use std::ops::Range;
use std::path::Path;

pub use json::Json;
pub use toml::Toml;
pub use yaml::Yaml;

/// How many bytes of a file drivers may sniff.
pub const SNIFF_LEN: usize = 4096;

/// Default nesting limit for every driver, to keep hostile input from
/// exhausting the stack.
pub const MAX_DEPTH: usize = 512;

/// A format driver.
pub trait Driver: Send + Sync {
    /// Short lowercase name, used in messages (`json`, `toml`, `yaml`).
    fn name(&self) -> &'static str;

    /// File extensions (lowercase, without the dot) this driver claims.
    fn extensions(&self) -> &'static [&'static str];

    /// Content sniff for files whose extension no driver claims. `head` is at
    /// most [`SNIFF_LEN`] bytes. Should be conservative.
    fn sniff(&self, head: &[u8]) -> bool;

    /// Decide by extension first, then by sniffing `head`. [`Registry`] calls
    /// `extensions` and `sniff` separately so that one driver's sniff never
    /// beats another driver's extension.
    fn detect(&self, path: &Path, head: &[u8]) -> bool {
        match extension(path) {
            Some(ext) if self.extensions().contains(&ext.as_str()) => true,
            _ => self.sniff(head),
        }
    }

    /// Parses `src` into a tree with byte spans.
    ///
    /// # Errors
    ///
    /// [`DriverError::Syntax`] for malformed input, [`DriverError::TooDeep`]
    /// past [`MAX_DEPTH`], [`DriverError::NotUtf8`] for non-UTF-8 text.
    fn parse(&self, src: &[u8]) -> Result<Tree, DriverError>;

    /// Applies `edit` at `ptr` and returns the new bytes. Every byte outside
    /// the edited span is unchanged (invariant 4).
    ///
    /// # Errors
    ///
    /// [`DriverError::Unsupported`] when the driver can't make this edit. No
    /// driver writes before milestone 4.
    fn splice(&self, src: &[u8], ptr: &[String], edit: Edit) -> Result<Vec<u8>, DriverError>;

    /// What this driver can do.
    fn capabilities(&self) -> Caps;
}

/// Driver capabilities.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
#[allow(clippy::struct_excessive_bools)] // A flag set; each bool is independent.
pub struct Caps {
    /// Can parse.
    pub read: bool,
    /// Can [`Edit::SetScalar`].
    pub write_scalar: bool,
    /// Can [`Edit::Insert`].
    pub insert: bool,
    /// Can [`Edit::Remove`].
    pub remove: bool,
    /// Can insert empty objects (`mkdir`).
    pub mkdir: bool,
}

impl Caps {
    /// Read-only.
    pub const READ_ONLY: Self = Self {
        read: true,
        write_scalar: false,
        insert: false,
        remove: false,
        mkdir: false,
    };
}

/// An edit for [`Driver::splice`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Edit {
    /// Replace a scalar's value, preserving its type.
    SetScalar(String),
    /// Insert a new key (or array entry) under the target.
    Insert {
        /// The new key.
        key: String,
        /// Its value.
        value: NewValue,
    },
    /// Remove the target.
    Remove,
}

/// A value for [`Edit::Insert`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum NewValue {
    /// `{}`
    EmptyObject,
    /// `[]`
    EmptyArray,
    /// A scalar from text; the driver picks the type.
    Text(String),
}

/// Errors from drivers.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum DriverError {
    /// The input isn't valid in this format.
    #[error("{message} at {line}:{column}")]
    Syntax {
        /// What went wrong, lowercase.
        message: String,
        /// Byte offset of the problem.
        offset: usize,
        /// 1-based line.
        line: usize,
        /// 1-based column, in characters.
        column: usize,
    },
    /// The input isn't UTF-8.
    #[error("not utf-8 at byte {offset}")]
    NotUtf8 {
        /// Byte offset of the first invalid byte.
        offset: usize,
    },
    /// Nesting deeper than [`MAX_DEPTH`].
    #[error("nested deeper than {MAX_DEPTH} levels")]
    TooDeep,
    /// The driver can't perform this operation.
    #[error("{0}")]
    Unsupported(&'static str),
}

impl DriverError {
    /// A syntax error at `offset` in `src`, with line and column filled in.
    #[must_use]
    pub fn syntax(src: &str, offset: usize, message: impl Into<String>) -> Self {
        let offset = offset.min(src.len());
        let before = src.get(..offset).unwrap_or(src);
        let line = before.matches('\n').count() + 1;
        let line_start = before.rfind('\n').map_or(0, |i| i + 1);
        let column = before[line_start..].chars().count() + 1;
        Self::Syntax {
            message: message.into(),
            offset,
            line,
            column,
        }
    }
}

/// Decodes `src` as UTF-8, skipping a leading byte-order mark. Returns the
/// text (the whole input, BOM included, so spans index the original bytes)
/// and the offset where content starts.
///
/// # Errors
///
/// [`DriverError::NotUtf8`].
pub fn text(src: &[u8]) -> Result<(&str, usize), DriverError> {
    let s = std::str::from_utf8(src).map_err(|e| DriverError::NotUtf8 {
        offset: e.valid_up_to(),
    })?;
    Ok((s, if s.starts_with('\u{feff}') { 3 } else { 0 }))
}

/// Index of a node in [`Tree::nodes`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct NodeId(pub usize);

/// A parsed document.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Tree {
    /// Every node. A node may be reachable from more than one parent (YAML
    /// aliases), but the graph is acyclic.
    pub nodes: Vec<Node>,
    /// The document root.
    pub root: NodeId,
}

/// One node in a [`Tree`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Node {
    /// Object, array or scalar.
    pub kind: NodeKind,
    /// The whole node including its key, for removal. For a TOML table this is
    /// its header line only, since a table can be split across the file.
    pub span: Range<usize>,
    /// Just the value, for [`Edit::SetScalar`]. For strings this includes the
    /// quotes; for YAML block scalars, the `|` or `>` header.
    pub value_span: Range<usize>,
    /// Where the key text sits, if the node has a key.
    pub key_span: Option<Range<usize>>,
    /// Children, ordered as in the source. Array children are keyed `0`, `1`,
    /// …. Keys are unique: for duplicates the first position and the last
    /// value win.
    pub children: Vec<(String, NodeId)>,
    /// For scalars, the value as fernet shows it (without the trailing
    /// newline): strings unquoted and unescaped, everything else as written in
    /// the source. Empty for objects and arrays.
    pub text: String,
}

/// Node kinds.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum NodeKind {
    /// A directory of keys.
    Object,
    /// A directory of `0`, `1`, ….
    Array,
    /// A regular file.
    Scalar(ScalarType),
}

/// Scalar types, used for type-preserving writes.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ScalarType {
    /// A string.
    String,
    /// An integer.
    Integer,
    /// A floating-point number.
    Float,
    /// `true` or `false`.
    Bool,
    /// `null`.
    Null,
    /// A date, time or date-time (TOML).
    Datetime,
    /// Free text (Markdown bodies, later).
    Text,
}

impl Tree {
    /// The node with this id.
    ///
    /// # Panics
    ///
    /// If `id` doesn't come from this tree.
    #[must_use]
    pub fn node(&self, id: NodeId) -> &Node {
        &self.nodes[id.0]
    }

    /// The child of `id` called `key`.
    #[must_use]
    pub fn child(&self, id: NodeId, key: &str) -> Option<NodeId> {
        self.node(id)
            .children
            .iter()
            .find(|(k, _)| k == key)
            .map(|&(_, c)| c)
    }

    /// Walks `pointer` (decoded keys) from the root.
    #[must_use]
    pub fn get(&self, pointer: &[String]) -> Option<NodeId> {
        pointer
            .iter()
            .try_fold(self.root, |id, key| self.child(id, key))
    }

    /// The bytes a scalar file shows: [`Node::text`] plus `\n`. `None` for
    /// objects and arrays.
    #[must_use]
    pub fn render(&self, id: NodeId) -> Option<Vec<u8>> {
        let node = self.node(id);
        matches!(node.kind, NodeKind::Scalar(_)).then(|| {
            let mut out = Vec::with_capacity(node.text.len() + 1);
            out.extend_from_slice(node.text.as_bytes());
            out.push(b'\n');
            out
        })
    }
}

/// Builds a [`Tree`] incrementally, deduplicating keys.
#[derive(Default)]
pub(crate) struct Builder {
    nodes: Vec<Node>,
    /// Key → index in `children`, for objects with duplicates possible.
    keys: HashMap<NodeId, HashMap<String, usize>>,
}

impl Builder {
    pub(crate) fn push(
        &mut self,
        kind: NodeKind,
        value_span: Range<usize>,
        text: String,
    ) -> NodeId {
        let id = NodeId(self.nodes.len());
        self.nodes.push(Node {
            kind,
            span: value_span.clone(),
            value_span,
            key_span: None,
            children: Vec::new(),
            text,
        });
        id
    }

    pub(crate) fn node_mut(&mut self, id: NodeId) -> &mut Node {
        &mut self.nodes[id.0]
    }

    /// Adds `child` under `parent` as `key`. A duplicate key keeps its first
    /// position and takes the new value.
    pub(crate) fn add_child(&mut self, parent: NodeId, key: String, child: NodeId) {
        let index = self.keys.entry(parent).or_default();
        let children = &mut self.nodes[parent.0].children;
        if let Some(&i) = index.get(&key) {
            children[i].1 = child;
        } else {
            index.insert(key.clone(), children.len());
            children.push((key, child));
        }
    }

    /// Appends `child` to an array.
    pub(crate) fn push_item(&mut self, parent: NodeId, child: NodeId) {
        let children = &mut self.nodes[parent.0].children;
        children.push((children.len().to_string(), child));
    }

    pub(crate) fn finish(self, root: NodeId) -> Tree {
        Tree {
            nodes: self.nodes,
            root,
        }
    }
}

/// The lowercase extension of `path`, if any.
fn extension(path: &Path) -> Option<String> {
    path.extension()
        .and_then(|e| e.to_str())
        .map(str::to_ascii_lowercase)
}

/// The set of drivers, and detection across them.
pub struct Registry {
    drivers: Vec<Box<dyn Driver>>,
}

impl Default for Registry {
    /// JSON, TOML and YAML.
    fn default() -> Self {
        // Sniff order matters: TOML before JSON, so `[[package]]` is a table header.
        Self {
            drivers: vec![Box::new(Toml), Box::new(Json), Box::new(Yaml)],
        }
    }
}

impl Registry {
    /// Picks a driver for a file. An extension claimed by a driver decides.
    /// Otherwise the first driver whose sniff accepts `head` wins. Files that
    /// look binary (a NUL byte in `head`) get no driver.
    #[must_use]
    pub fn detect(&self, path: &Path, head: &[u8]) -> Option<&dyn Driver> {
        if let Some(ext) = extension(path) {
            if let Some(d) = self
                .drivers
                .iter()
                .find(|d| d.extensions().contains(&ext.as_str()))
            {
                return Some(d.as_ref());
            }
        }
        if head.contains(&0) {
            return None;
        }
        self.drivers
            .iter()
            .find(|d| d.sniff(head))
            .map(AsRef::as_ref)
    }

    /// The driver called `name`.
    #[must_use]
    pub fn by_name(&self, name: &str) -> Option<&dyn Driver> {
        self.drivers
            .iter()
            .find(|d| d.name() == name)
            .map(AsRef::as_ref)
    }
}

/// The first non-blank, non-comment line of `head`, for sniffing.
pub(crate) fn first_line<'a>(head: &'a str, comment: &str) -> Option<&'a str> {
    head.trim_start_matches('\u{feff}')
        .lines()
        .map(str::trim)
        .find(|l| !l.is_empty() && !l.starts_with(comment))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detect_by_extension_then_sniff() {
        let r = Registry::default();
        let name = |p: &str, head: &str| r.detect(Path::new(p), head.as_bytes()).map(Driver::name);
        assert_eq!(name("a.json", ""), Some("json"));
        assert_eq!(name("A.JSON", ""), Some("json"));
        assert_eq!(name("tsconfig.json", "// c\n{"), Some("json"));
        assert_eq!(name("Cargo.toml", ""), Some("toml"));
        assert_eq!(name("ci.yml", ""), Some("yaml"));
        assert_eq!(name("x.yaml", "{}"), Some("yaml"));
        assert_eq!(name(".babelrc", "{\"a\": 1}"), Some("json"));
        assert_eq!(name("Cargo.lock", "# gen\n[[package]]\n"), Some("toml"));
        assert_eq!(name("config", "---\na: 1\n"), Some("yaml"));
        assert_eq!(name("notes.txt", "hello"), None);
        assert_eq!(name("blob", "{\0"), None);
    }

    #[test]
    fn syntax_positions() {
        let DriverError::Syntax { line, column, .. } = DriverError::syntax("ab\ncdé\nx", 7, "m")
        else {
            unreachable!()
        };
        assert_eq!((line, column), (2, 4));
    }

    #[test]
    fn bom() {
        assert_eq!(text(b"\xef\xbb\xbf{}").unwrap().1, 3);
        assert_eq!(text(b"{}").unwrap().1, 0);
        assert_eq!(text(b"a\xff"), Err(DriverError::NotUtf8 { offset: 1 }));
    }
}
