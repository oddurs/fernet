//! `config.toml`, parsed as TOML (`docs/FORMAT.md` §7).
//!
//! Only the settings in FORMAT.md §7 are read; anything else is ignored. A file
//! that doesn't parse, or a known setting with the wrong type, is a
//! [`Config::problems`] entry: callers fall back to defaults for what they
//! can't read, and say so (the CLI warns; compaction refuses).

use std::fs;
use std::path::Path;

use toml_edit::{Document, Item, Value};

/// Integer settings and their tables.
const INTEGERS: [(&str, &str); 8] = [
    ("log", "group_commit_ms"),
    ("log", "segment_bytes"),
    ("capture", "debounce_ms"),
    ("retention", "snow_hours"),
    ("retention", "firn_days"),
    ("retention", "ice_days"),
    ("limits", "max_file_size"),
    ("limits", "store_warn_bytes"),
];

/// The settings in `config.toml`.
#[derive(Debug, Default)]
pub struct Config {
    doc: Option<Document<String>>,
    problems: Vec<String>,
}

impl Config {
    /// Reads `dir/config.toml` (`dir` is the `.fernet` directory). A missing
    /// file means defaults and isn't a problem.
    #[must_use]
    pub fn load(dir: &Path) -> Self {
        let path = dir.join("config.toml");
        match fs::read_to_string(&path) {
            Ok(text) => Self::parse(&text),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Self::default(),
            Err(e) => Self {
                doc: None,
                problems: vec![format!("config.toml: {e}; using defaults")],
            },
        }
    }

    /// Parses settings from TOML text.
    #[must_use]
    pub fn parse(text: &str) -> Self {
        let doc = match Document::parse(text.to_owned()) {
            Ok(d) => d,
            Err(e) => {
                let msg = e.to_string();
                let first = msg.lines().next().unwrap_or("parse error").trim();
                return Self {
                    doc: None,
                    problems: vec![format!(
                        "config.toml is not valid toml ({first}); using defaults"
                    )],
                };
            }
        };
        let mut c = Self {
            doc: Some(doc),
            problems: Vec::new(),
        };
        for (table, key) in INTEGERS {
            if let Some(item) = c.item(table, key)
                && read_u64(item).is_none()
            {
                c.problems.push(format!(
                    "config.toml: [{table}] {key} must be a non-negative integer; using the default"
                ));
            }
        }
        if let Some(item) = c.item("ignore", "patterns")
            && read_strings(item).is_none()
        {
            c.problems.push(
                "config.toml: [ignore] patterns must be an array of strings; ignoring it".into(),
            );
        }
        c
    }

    fn item(&self, table: &str, key: &str) -> Option<&Item> {
        let doc = self.doc.as_ref()?;
        doc.get(table)?.as_table_like()?.get(key)
    }

    /// An integer setting by key (`snow_hours`, `debounce_ms`, …), looked up
    /// in its table. `None` if absent, of the wrong type, or not a setting
    /// fernet knows.
    #[must_use]
    pub fn u64(&self, key: &str) -> Option<u64> {
        let (table, _) = INTEGERS.iter().find(|(_, k)| *k == key)?;
        read_u64(self.item(table, key)?)
    }

    /// `[ignore] patterns`; empty if absent or malformed.
    #[must_use]
    pub fn patterns(&self) -> Vec<String> {
        self.item("ignore", "patterns")
            .and_then(read_strings)
            .unwrap_or_default()
    }

    /// What couldn't be read, as messages for the user. Empty when the file
    /// is fine.
    #[must_use]
    pub fn problems(&self) -> &[String] {
        &self.problems
    }
}

fn read_u64(item: &Item) -> Option<u64> {
    u64::try_from(item.as_integer()?).ok()
}

fn read_strings(item: &Item) -> Option<Vec<String>> {
    let array = item.as_array()?;
    array
        .iter()
        .map(|v| match v {
            Value::String(s) => Some(s.value().clone()),
            _ => None,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn real_toml() {
        let c = Config::parse(
            "[retention]\nsnow_hours = 168  # a week\nfirn_days = 1_000\n\
             [ignore]\npatterns = [\n  '*.log',\n  \"build/\", # trailing\n]\n",
        );
        assert!(c.problems().is_empty(), "{:?}", c.problems());
        assert_eq!(c.u64("snow_hours"), Some(168));
        assert_eq!(c.u64("firn_days"), Some(1000));
        assert_eq!(c.u64("ice_days"), None);
        assert_eq!(c.patterns(), ["*.log", "build/"]);
    }

    #[test]
    fn keys_live_in_their_tables() {
        let c = Config::parse("snow_hours = 5\n[log]\nsnow_hours = 6\n");
        assert_eq!(c.u64("snow_hours"), None);
        let c = Config::parse("retention = { snow_hours = 7 }\n");
        assert_eq!(c.u64("snow_hours"), Some(7));
    }

    #[test]
    fn problems_are_reported() {
        let c = Config::parse("[retention\nsnow_hours = 1\n");
        assert_eq!(c.problems().len(), 1);
        assert_eq!(c.u64("snow_hours"), None);
        let c = Config::parse("[retention]\nsnow_hours = -1\nfirn_days = \"30\"\n");
        assert_eq!(c.problems().len(), 2);
        let c = Config::parse("[ignore]\npatterns = [1]\n");
        assert_eq!(c.problems().len(), 1);
        assert!(c.patterns().is_empty());
        assert!(Config::parse(crate::DEFAULT_CONFIG).problems().is_empty());
    }
}
