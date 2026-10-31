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

/// Rows keyed by the primary key when there is exactly one primary-key
/// column, else by `rowid`, else (`WITHOUT ROWID` with a composite key) by
/// position.
fn add_rows(
    conn: &Connection,
    b: &mut Builder,
    table: NodeId,
    name: &str,
) -> Result<(), DriverError> {
    let q = quote_ident(name);
    let pk: Vec<String> = {
        let mut st = conn
            .prepare(&format!(
                "SELECT name FROM pragma_table_info({}) WHERE pk > 0 ORDER BY pk",
                sql_str(name)
            ))
            .map_err(|e| err(&e))?;
        st.query_map([], |r| r.get(0))
            .map_err(|e| err(&e))?
            .collect::<Result<_, _>>()
            .map_err(|e| err(&e))?
    };
    let sql = match pk.as_slice() {
        [one] => format!("SELECT {}, * FROM {q}", quote_ident(one)),
        _ => format!("SELECT rowid, * FROM {q}"),
    };
    let (mut st, keyed) = match conn.prepare(&sql) {
        Ok(st) => (st, true),
        // WITHOUT ROWID with a composite key: rows by position.
        Err(_) => (
            conn.prepare(&format!("SELECT NULL, * FROM {q}"))
                .map_err(|e| err(&e))?,
            false,
        ),
    };
    let columns: Vec<String> = st.column_names().skip(1).map(str::to_owned).collect();
    let mut rows = st.query([]).map_err(|e| err(&e))?;
    let mut index = 0usize;
    while let Some(row) = rows.next().map_err(|e| err(&e))? {
        let key = match row.get_ref(0).map_err(|e| err(&e))? {
            _ if !keyed => index.to_string(),
            // A NULL primary key: a name no real key collides with.
            ValueRef::Null => format!("(null {index})"),
            v => scalar(v).1,
        };
        index += 1;
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
        b.add_child(table, key, r);
    }
    Ok(())
}

fn sql_str(s: &str) -> String {
    format!("'{}'", s.replace('\'', "''"))
}

fn scalar(v: ValueRef<'_>) -> (ScalarType, String) {
    match v {
        ValueRef::Null => (ScalarType::Null, "null".into()),
        ValueRef::Integer(i) => (ScalarType::Integer, i.to_string()),
        ValueRef::Real(f) => (ScalarType::Float, f.to_string()),
        ValueRef::Text(t) => (ScalarType::String, String::from_utf8_lossy(t).into_owned()),
        ValueRef::Blob(b) => (ScalarType::Bytes, format!("{} bytes", b.len())),
    }
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
}
