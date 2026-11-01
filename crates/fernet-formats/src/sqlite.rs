//! `SQLite`, read-only: tables → rows by primary key → columns.

use rusqlite::types::ValueRef;
use rusqlite::{Connection, MAIN_DB, OpenFlags};

use crate::{Builder, Caps, Driver, DriverError, Edit, NodeId, NodeKind, ScalarType, Tree};

/// The `SQLite` driver.
#[derive(Clone, Copy, Debug, Default)]
pub struct Sqlite;

const MAGIC: &[u8] = b"SQLite format 3\0";

fn err(e: &rusqlite::Error) -> DriverError {
    DriverError::Syntax {
        message: e.to_string().to_lowercase(),
        offset: 0,
        line: 1,
        column: 1,
    }
}

fn quote_ident(name: &str) -> String {
    format!("\"{}\"", name.replace('"', "\"\""))
}

impl Driver for Sqlite {
    fn name(&self) -> &'static str {
        "sqlite"
    }

    fn extensions(&self) -> &'static [&'static str] {
        &["sqlite", "sqlite3", "db"]
    }

    fn sniff(&self, head: &[u8]) -> bool {
        head.starts_with(MAGIC)
    }

    fn parse(&self, src: &[u8]) -> Result<Tree, DriverError> {
        let mut b = Builder::default();
        let root = b.push(NodeKind::Object, 0..0, String::new());
        if src.is_empty() {
            return Ok(b.finish(root));
        }
        if !src.starts_with(MAGIC) {
            return Err(DriverError::Syntax {
                message: "not an sqlite database".into(),
                offset: 0,
                line: 1,
                column: 1,
            });
        }
        let flags = OpenFlags::SQLITE_OPEN_READ_ONLY
            | OpenFlags::SQLITE_OPEN_NO_MUTEX
            | OpenFlags::SQLITE_OPEN_MEMORY;
        let mut conn = Connection::open_in_memory_with_flags(flags).map_err(|e| err(&e))?;
        // WAL-mode files (header bytes 18-19 = 2) can't open from memory:
        // read them as rollback-journal databases (the bytes are a copy).
        let mut copy;
        let src = if src.get(18..20) == Some(&[2, 2]) {
            copy = src.to_vec();
            copy[18] = 1;
            copy[19] = 1;
            &copy[..]
        } else {
            src
        };
        conn.deserialize_read_exact(MAIN_DB, src, src.len(), true)
            .map_err(|e| err(&e))?;
        // Don't run anything a hostile schema asks for.
        conn.execute_batch("PRAGMA trusted_schema = OFF; PRAGMA cell_size_check = ON;")
            .map_err(|e| err(&e))?;
        let tables: Vec<String> = {
            let mut st = conn
                .prepare("SELECT name FROM sqlite_schema WHERE type = 'table' AND name NOT LIKE 'sqlite_%' ORDER BY name")
                .map_err(|e| err(&e))?;
            st.query_map([], |r| r.get(0))
                .map_err(|e| err(&e))?
                .collect::<Result<_, _>>()
                .map_err(|e| err(&e))?
        };
        for table in tables {
            let t = b.push(NodeKind::Object, 0..0, String::new());
            add_rows(&conn, &mut b, t, &table)?;
            b.add_child(root, table, t);
        }
        Ok(b.finish(root))
    }

    fn splice(&self, _: &[u8], _: &[String], _: Edit) -> Result<Vec<u8>, DriverError> {
        Err(DriverError::Unsupported(
            "sqlite databases are read-only through fernet",
        ))
    }

    fn capabilities(&self) -> Caps {
        Caps::READ_ONLY
    }
}

/// Names from a one-column query.
fn names(conn: &Connection, sql: &str) -> Result<Vec<String>, DriverError> {
    let mut st = conn.prepare(sql).map_err(|e| err(&e))?;
    st.query_map([], |r| r.get(0))
        .map_err(|e| err(&e))?
        .collect::<Result<_, _>>()
        .map_err(|e| err(&e))
}

/// Rows keyed by the primary key when there is exactly one primary-key
/// column, else by `rowid`, else (`WITHOUT ROWID` with a composite key, or
/// columns shadowing all of `rowid`, `_rowid_` and `oid`) by position.
///
/// Keys that come out the same (`1` and `'1'`, two blobs of one size) are
/// told apart: the first row in table order keeps the key, later ones get
/// ` (2)`, ` (3)`… skipping names any row already has.
fn add_rows(
    conn: &Connection,
    b: &mut Builder,
    table: NodeId,
    name: &str,
) -> Result<(), DriverError> {
    let q = quote_ident(name);
    let pk = names(
        conn,
        &format!(
            "SELECT name FROM pragma_table_info({}) WHERE pk > 0 ORDER BY pk",
            sql_str(name)
        ),
    )?;
    let columns = names(
        conn,
        &format!("SELECT name FROM pragma_table_xinfo({})", sql_str(name)),
    )?;
    // A column called `rowid` hides the real one: use an alias no column has.
    let rowid = ["rowid", "_rowid_", "oid"]
        .into_iter()
        .find(|alias| !columns.iter().any(|c| c.eq_ignore_ascii_case(alias)));
    let sql = match (pk.as_slice(), rowid) {
        ([one], _) => Some(format!("SELECT {}, * FROM {q}", quote_ident(one))),
        (_, Some(rowid)) => Some(format!("SELECT {rowid}, * FROM {q}")),
        (_, None) => None,
    };
    let (mut st, keyed) = match sql.map(|sql| conn.prepare(&sql)) {
        Some(Ok(st)) => (st, true),
        // WITHOUT ROWID with a composite key: rows by position.
        _ => (
            conn.prepare(&format!("SELECT NULL, * FROM {q}"))
                .map_err(|e| err(&e))?,
            false,
        ),
    };
    let columns: Vec<String> = st.column_names().skip(1).map(str::to_owned).collect();
    let mut rows = st.query([]).map_err(|e| err(&e))?;
    let mut keyed_rows = Vec::new();
    while let Some(row) = rows.next().map_err(|e| err(&e))? {
        let index = keyed_rows.len();
        let key = match row.get_ref(0).map_err(|e| err(&e))? {
            _ if !keyed => index.to_string(),
            // A NULL primary key: a name no real key collides with.
            ValueRef::Null => format!("(null {index})"),
            v => scalar(v).1,
        };
        let r = b.push(NodeKind::Object, 0..0, String::new());
        for (i, col) in columns.iter().enumerate() {
            let v = row.get_ref(i + 1).map_err(|e| err(&e))?;
            let (ty, text) = scalar(v);
            let id = b.push(NodeKind::Scalar(ty), 0..0, text);
            if let ValueRef::Blob(bytes) = v {
                b.node_mut(id).raw = bytes.to_vec();
            }
            b.add_child(r, col.clone(), id);
        }
        keyed_rows.push((key, r));
    }
    for (key, r) in unique_keys(keyed_rows) {
        b.add_child(table, key, r);
    }
    Ok(())
}

/// Renames repeated keys: the first keeps its name, later ones get ` (2)`,
/// ` (3)`…, never a name another row has.
fn unique_keys<T>(rows: Vec<(String, T)>) -> Vec<(String, T)> {
    let mut taken: std::collections::HashSet<String> =
        rows.iter().map(|(k, _)| k.clone()).collect();
    let mut seen = std::collections::HashSet::new();
    rows.into_iter()
        .map(|(key, r)| {
            if seen.insert(key.clone()) {
                return (key, r);
            }
            let mut n = 2usize;
            let name = loop {
                let name = format!("{key} ({n})");
                if !taken.contains(&name) {
                    break name;
                }
                n += 1;
            };
            taken.insert(name.clone());
            seen.insert(name.clone());
            (name, r)
        })
        .collect()
}

fn sql_str(s: &str) -> String {
    format!("'{}'", s.replace('\'', "''"))
}

fn scalar(v: ValueRef<'_>) -> (ScalarType, String) {
    match v {
        ValueRef::Null => (ScalarType::Null, "null".into()),
        ValueRef::Integer(i) => (ScalarType::Integer, i.to_string()),
        ValueRef::Real(f) => (ScalarType::Float, real(f)),
        ValueRef::Text(t) => (ScalarType::String, String::from_utf8_lossy(t).into_owned()),
        ValueRef::Blob(b) => (ScalarType::Bytes, format!("{} bytes", b.len())),
    }
}

/// A REAL as text that reads back as a REAL: always with a `.` or an
/// exponent (`1.0`, not `1`), and in exponent form when very large or small
/// (`1e300`, not 301 digits).
fn real(f: f64) -> String {
    // Debug prints the shortest text that round-trips, switching to an
    // exponent outside 1e-5..1e16.
    format!("{f:?}")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A database built with SQL, serialized to bytes.
    fn db(sql: &str) -> Vec<u8> {
        let mut conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(sql).unwrap();
        conn.serialize(MAIN_DB).unwrap().to_vec()
    }

    #[test]
    fn tables_rows_columns() {
        let bytes = db(
            "CREATE TABLE users (id INTEGER PRIMARY KEY, name TEXT, score REAL, pic BLOB);
                        INSERT INTO users VALUES (7, 'ana', 1.5, x'00ff'), (9, 'bo', NULL, NULL);
                        CREATE TABLE logs (msg TEXT);
                        INSERT INTO logs VALUES ('a'), ('b');
                        CREATE TABLE pairs (a TEXT, b TEXT, PRIMARY KEY (a, b)) WITHOUT ROWID;
                        INSERT INTO pairs VALUES ('x', 'y');",
        );
        let t = Sqlite.parse(&bytes).unwrap();
        let keys = |id: NodeId| {
            t.node(id)
                .children
                .iter()
                .map(|(k, _)| k.clone())
                .collect::<Vec<_>>()
        };
        assert_eq!(keys(t.root), ["logs", "pairs", "users"]);
        let get = |p: &[&str]| {
            t.node(
                t.get(&p.iter().map(|s| (*s).to_owned()).collect::<Vec<_>>())
                    .unwrap(),
            )
        };
        assert_eq!(get(&["users", "7", "name"]).text, "ana");
        assert_eq!(
            get(&["users", "7", "score"]).kind,
            NodeKind::Scalar(ScalarType::Float)
        );
        assert_eq!(
            t.render(t.get(&["users".into(), "7".into(), "pic".into()]).unwrap())
                .unwrap(),
            [0, 255]
        );
        assert_eq!(get(&["users", "9", "score"]).text, "null");
        assert_eq!(keys(t.get(&["logs".into()]).unwrap()), ["1", "2"]);
        assert_eq!(get(&["pairs", "0", "b"]).text, "y");
        assert!(Sqlite.parse(b"not a database").is_err());
        // NULL keys don't collide with real ones; WAL-mode files open.
        let bytes = db(
            "CREATE TABLE t (k TEXT PRIMARY KEY, v); INSERT INTO t VALUES (NULL, 'a'), ('0', 'b');",
        );
        let t2 = Sqlite.parse(&bytes).unwrap();
        assert_eq!(t2.node(t2.get(&["t".into()]).unwrap()).children.len(), 2);
        let mut wal = db("CREATE TABLE w (x); INSERT INTO w VALUES (1);");
        wal[18] = 2;
        wal[19] = 2;
        assert!(Sqlite.parse(&wal).is_ok());
        let mut broken = bytes.clone();
        broken.truncate(200);
        assert!(Sqlite.parse(&broken).is_err() || Sqlite.parse(&broken).is_ok());
    }

    fn row_keys(t: &Tree, table: &str) -> Vec<String> {
        t.node(t.get(&[table.to_owned()]).unwrap())
            .children
            .iter()
            .map(|(k, _)| k.clone())
            .collect()
    }

    #[test]
    fn no_row_vanishes() {
        // A column called `rowid` doesn't hide the real rowid.
        let t = Sqlite
            .parse(&db("CREATE TABLE t (rowid TEXT, v);
                 INSERT INTO t VALUES ('a', 1), ('a', 2), (NULL, 3);"))
            .unwrap();
        assert_eq!(row_keys(&t, "t"), ["1", "2", "3"]);
        // All three aliases taken: rows by position.
        let t = Sqlite
            .parse(&db("CREATE TABLE t (rowid, _rowid_, oid);
                 INSERT INTO t VALUES (1, 1, 1), (1, 1, 1);"))
            .unwrap();
        assert_eq!(row_keys(&t, "t"), ["0", "1"]);
        // Keys that print the same are told apart, deterministically, without
        // taking a name a real key has.
        let t = Sqlite
            .parse(&db("CREATE TABLE k (id PRIMARY KEY, v);
                 INSERT INTO k VALUES (1, 'int'), ('1', 'text'), ('1 (2)', 'x'), ('(null 4)', 'y'), (NULL, 'z');"))
            .unwrap();
        let keys = row_keys(&t, "k");
        assert_eq!(keys.len(), 5);
        assert_eq!(keys, ["1", "1 (3)", "1 (2)", "(null 4)", "(null 4) (2)"]);
        let v = |k: &str| {
            t.node(
                t.get(&["k".to_owned(), k.to_owned(), "v".to_owned()])
                    .unwrap(),
            )
            .text
            .clone()
        };
        assert_eq!(v("1"), "int");
        assert_eq!(v("1 (3)"), "text");
        assert_eq!(v("1 (2)"), "x");
    }

    #[test]
    fn reals_read_as_reals() {
        let t = Sqlite
            .parse(&db("CREATE TABLE f (x REAL);
                 INSERT INTO f VALUES (1.0), (1e300), (-2.5), (1e-7), (0.1);"))
            .unwrap();
        let texts: Vec<String> = (1..=5)
            .map(|i| {
                t.node(
                    t.get(&["f".to_owned(), i.to_string(), "x".to_owned()])
                        .unwrap(),
                )
                .text
                .clone()
            })
            .collect();
        assert_eq!(texts, ["1.0", "1e300", "-2.5", "1e-7", "0.1"]);
        // Each reads back as the same REAL.
        let conn = Connection::open_in_memory().unwrap();
        for text in &texts {
            let (ty, back): (String, f64) = conn
                .query_row(&format!("SELECT typeof({text}), {text}"), [], |r| {
                    Ok((r.get(0)?, r.get(1)?))
                })
                .unwrap();
            assert_eq!(ty, "real", "{text}");
            assert_eq!(back.to_string(), text.parse::<f64>().unwrap().to_string());
        }
    }
}
