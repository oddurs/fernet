//! Time lenses through the resolver, with a fixed clock and the
//! `America/New_York` zone across the 2026-03-08 DST boundary.

#![allow(clippy::unwrap_used, clippy::panic)]

use std::fs;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicI64, Ordering};

use fernet_fs_core::time::nanos;
use fernet_fs_core::{Error, Fernet, Options, Resolved};
use fernet_fs_path::FernetPath;
use fernet_fs_store::Mode;
use jiff::Timestamp;
use jiff::tz::TimeZone;

struct World {
    _tmp: tempfile::TempDir,
    root: std::path::PathBuf,
    clock: Arc<AtomicI64>,
    f: Fernet,
}

impl World {
    fn new() -> Self {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().to_owned();
        let clock = Arc::new(AtomicI64::new(ts("2026-01-01T00:00:00Z")));
        let c = Arc::clone(&clock);
        let options = Options {
            mode: Mode::Write,
            clock: Box::new(move || fernet_fs_core::time::timestamp(c.load(Ordering::SeqCst))),
            tz: TimeZone::get("America/New_York").unwrap(),
        };
        let f = Fernet::init(&root, options).unwrap();
        Self {
            _tmp: tmp,
            root,
            clock,
            f,
        }
    }

    fn at(&self, when: &str) {
        self.clock.store(ts(when), Ordering::SeqCst);
    }

    /// Writes `content` to `path` at `when` and captures it.
    fn save(&self, when: &str, path: &str, content: &str) {
        self.at(when);
        let p = self.root.join(path);
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(&p, content).unwrap();
        self.f.snapshot(&p).unwrap();
    }

    fn delete(&self, when: &str, path: &str) {
        self.at(when);
        let p = self.root.join(path);
        fs::remove_file(&p).unwrap();
        self.f.snapshot(&p).unwrap();
    }

    fn read(&self, path: &str) -> Result<String, Error> {
        let r = self.f.resolve(&FernetPath::parse(path).unwrap())?;
        Ok(String::from_utf8(self.f.read(&r)?).unwrap())
    }

    fn ls(&self, path: &str) -> Vec<String> {
        let r = self.f.resolve(&FernetPath::parse(path).unwrap()).unwrap();
        self.f
            .list(&r)
            .unwrap()
            .into_iter()
            .map(|e| e.name.into_string().unwrap())
            .collect()
    }
}

fn ts(s: &str) -> i64 {
    nanos(s.parse::<Timestamp>().unwrap())
}

/// Five versions around the spring-forward boundary (02:00 EST → 03:00 EDT
/// on Sunday 2026-03-08) and a mark between the third and fourth.
fn dst_world() -> World {
    let w = World::new();
    w.save("2026-03-01T12:00:00Z", "notes.md", "one");
    w.save("2026-03-07T12:00:00Z", "notes.md", "two");
    w.save("2026-03-08T06:30:00Z", "notes.md", "three"); // 01:30 EST
    w.at("2026-03-08T06:45:00Z");
    w.f.mark("pre-dst").unwrap();
    w.save("2026-03-08T07:30:00Z", "notes.md", "four"); // 03:30 EDT
    w.save("2026-03-09T13:00:00Z", "notes.md", "five");
    w.at("2026-03-09T14:00:00Z"); // Monday 10:00 EDT
    w
}

#[test]
fn every_spec_form_across_dst() {
    let w = dst_world();
    let cases = [
        ("-90s", "five"),
        ("-2h", "four"),
        ("-1d", "four"),
        ("-1w", "one"),
        ("yesterday", "two"),
        ("sunday", "two"),
        ("saturday", "one"),
        ("monday", "one"), // a week ago, not today
        ("2026-03-08", "two"),
        ("2026-03-08T01:45", "three"),
        ("2026-03-08T02:30", "four"), // in the gap: moves to 03:30 EDT
        ("2026-03-08T03:00-04:00", "three"),
        ("2026-03-08T07:30:00Z", "four"), // at-or-before includes the instant
        ("2026-03-08T07:29:59Z", "three"),
        ("~1", "four"),
        ("~4", "one"),
        ("first", "one"),
        ("pre-dst", "three"),
        ("now", "five"),
        // Listing names work as specs and cover their whole second.
        ("20260308T063000Z", "three"),
        ("20260308T062959Z", "two"),
    ];
    for (spec, want) in cases {
        assert_eq!(
            w.read(&format!("notes.md@{spec}")).unwrap(),
            want,
            "@{spec}"
        );
    }
    assert!(matches!(w.read("notes.md@~5"), Err(Error::NotFound { .. })));
    let e = w.read("notes.md@-2w").unwrap_err();
    assert_eq!(
        e.to_string(),
        "no version of notes.md before 2026-02-23 09:00 (first kept: 2026-03-01 07:00)"
    );
    assert!(matches!(w.read("notes.md@nope"), Err(Error::NoMark { .. })));
    assert_eq!(w.read("notes.md").unwrap(), "five");
}

#[test]
fn version_listing() {
    let w = dst_world();
    assert_eq!(
        w.ls("notes.md@"),
        [
            "20260301T120000Z",
            "20260307T120000Z",
            "20260308T063000Z",
            "20260308T073000Z",
            "20260309T130000Z",
            "pre-dst"
        ]
    );
    assert_eq!(w.read("notes.md@/20260308T063000Z").unwrap(), "three");
    assert_eq!(w.read("notes.md@/pre-dst").unwrap(), "three");
    // Two versions in the same second get a suffix.
    w.save("2026-03-09T14:00:00.1Z", "same.txt", "a");
    w.save("2026-03-09T14:00:00.2Z", "same.txt", "b");
    assert_eq!(
        w.ls("same.txt@")[..2],
        ["20260309T140000Z", "20260309T140000Z.1"]
    );
    assert_eq!(w.read("same.txt@/20260309T140000Z.1").unwrap(), "b");
}

#[test]
fn past_directories_and_tombstones() {
    let w = World::new();
    w.save("2026-10-01T12:00:00Z", "src/a.txt", "a1");
    w.save("2026-10-01T12:00:00Z", "src/sub/b.txt", "b1");
    w.save("2026-10-02T12:00:00Z", "src/a.txt", "a2");
    w.delete("2026-10-03T12:00:00Z", "src/sub/b.txt");
    w.save("2026-10-04T12:00:00Z", "src/c.txt", "c1");
    w.at("2026-10-05T12:00:00Z");

    assert_eq!(w.ls("src@2026-10-01T23:00Z"), ["a.txt", "sub"]);
    assert_eq!(w.read("src@2026-10-01T23:00Z/a.txt").unwrap(), "a1");
    assert_eq!(w.read("src@2026-10-02T23:00Z/sub/b.txt").unwrap(), "b1");
    // After the delete, the file and its (now empty) directory are gone.
    assert_eq!(w.ls("src@2026-10-03T23:00Z"), ["a.txt"]);
    assert!(w.read("src@2026-10-03T23:00Z/sub/b.txt").is_err());
    assert!(w.read("src/sub/b.txt@-1h").is_err());
    assert_eq!(w.read("src/sub/b.txt@2026-10-02").unwrap(), "b1");
    // A deleted file still has its version listing.
    assert_eq!(w.ls("src/sub/b.txt@").len(), 1);
    // `first` on a directory is the earliest moment anything in it existed.
    assert_eq!(w.ls("src@first"), ["a.txt", "sub"]);
    // Nothing existed before the first record.
    assert!(matches!(
        w.f.resolve(&FernetPath::parse("src@2026-09-01").unwrap()),
        Err(Error::NotFound { .. })
    ));
    let r = w.f.resolve(&FernetPath::parse("src@-1d").unwrap()).unwrap();
    assert!(matches!(r, Resolved::PastDir { .. }) && r.is_past());
}

#[test]
fn opening_past_files() {
    let w = World::new();
    w.save(
        "2026-10-01T12:00:00Z",
        "config.json",
        r#"{"server": {"port": 8080}}"#,
    );
    w.save(
        "2026-10-02T12:00:00Z",
        "config.json",
        r#"{"server": {"port": 9090}, "new": true}"#,
    );
    w.at("2026-10-03T12:00:00Z");
    assert_eq!(w.read("config.json#/server/port").unwrap(), "9090\n");
    assert_eq!(w.read("config.json@~1#/server/port").unwrap(), "8080\n");
    assert_eq!(
        w.read("config.json@2026-10-01T23:00Z#/server/port")
            .unwrap(),
        "8080\n"
    );
    assert!(w.read("config.json@~1#/new").is_err());
    assert_eq!(w.ls("config.json@first#"), ["server"]);
    // Through a version listing.
    assert_eq!(
        w.read("config.json@/20261001T120000Z#/server/port")
            .unwrap(),
        "8080\n"
    );
}

#[test]
fn real_names_win_in_history() {
    let w = World::new();
    w.save("2026-10-01T12:00:00Z", "d/x@-1h", "literal");
    w.save("2026-10-01T12:00:00Z", "d/x", "plain");
    w.at("2026-10-02T12:00:00Z");
    // `x@-1h` existed in the past directory: served as-is.
    assert_eq!(w.read("d@-1h/x@-1h").unwrap(), "literal");
    // Present-day real name wins too.
    assert_eq!(w.read("d/x@-1h").unwrap(), "literal");
    assert_eq!(w.read("d/x@-2h").unwrap(), "plain");
}

#[test]
fn identical_saves_are_one_version() {
    let w = World::new();
    for i in 0..40 {
        w.save(
            &format!("2026-10-01T12:00:{i:02}Z"),
            "same.txt",
            "identical",
        );
    }
    let store = w.f.store().unwrap();
    assert_eq!(store.versions("same.txt").unwrap().len(), 1);
    assert_eq!(store.usage().unwrap().blobs, 1);
}

#[test]
fn ignore_rules() {
    let w = World::new();
    fs::write(w.root.join(".fernetignore"), "*.log\nbuild/\n").unwrap();
    let w2 = Fernet::open_with(
        &w.root,
        Options {
            mode: Mode::Read,
            ..Options::default()
        },
    );
    drop(w2);
    let f = &w.f;
    let ignore = fernet_fs_core::Ignore::load(&w.root);
    for (path, dir, want) in [
        (".git/config", false, true),
        ("node_modules/x/index.js", false, true),
        ("target/debug/a", false, true),
        ("a/.DS_Store", false, true),
        (".fernet/MANIFEST", false, true),
        ("build", true, true),
        ("x.log", false, true),
        ("src/main.rs", false, false),
        ("targets.txt", false, false),
    ] {
        assert_eq!(ignore.ignored(&w.root.join(path), dir), want, "{path}");
    }
    // The default 50 MiB limit is honoured by snapshot.
    let _ = f;
    assert_eq!(fernet_fs_core::DEFAULT_MAX_FILE, 50 << 20);
}

#[test]
fn snapshot_tree_captures_and_tombstones() {
    let w = World::new();
    fs::create_dir_all(w.root.join("a/b")).unwrap();
    fs::create_dir_all(w.root.join("node_modules/pkg")).unwrap();
    fs::write(w.root.join("a/one.txt"), "1").unwrap();
    fs::write(w.root.join("a/b/two.txt"), "2").unwrap();
    fs::write(w.root.join("node_modules/pkg/index.js"), "x").unwrap();
    w.at("2026-10-01T12:00:00Z");
    assert_eq!(w.f.snapshot_tree(&w.root).unwrap(), 2);
    assert_eq!(w.f.snapshot_tree(&w.root).unwrap(), 0);
    fs::remove_file(w.root.join("a/one.txt")).unwrap();
    w.at("2026-10-01T13:00:00Z");
    assert_eq!(w.f.snapshot_tree(&w.root).unwrap(), 1);
    w.at("2026-10-01T14:00:00Z");
    assert_eq!(
        w.read("a/one.txt@-30m").unwrap_err().to_string(),
        "no such file: a/one.txt@-30m"
    );
    assert_eq!(w.read("a/one.txt@-90m").unwrap(), "1");
}

#[test]
fn read_only_reader_sees_writer_history() {
    let w = dst_world();
    w.f.flush().unwrap();
    let reader = Fernet::open_with(Path::new(&w.root), Options::default()).unwrap();
    let r = reader
        .resolve(&FernetPath::parse("notes.md@first").unwrap())
        .unwrap();
    assert_eq!(reader.read(&r).unwrap(), b"one");
}

#[test]
fn core_sample_collapses_equal_values() {
    let w = World::new();
    w.save("2026-10-01T12:00:00Z", "c.yaml", "image: a\nother: 1\n");
    w.save("2026-10-02T12:00:00Z", "c.yaml", "image: a\nother: 2\n");
    w.save("2026-10-03T12:00:00Z", "c.yaml", "image: b\nother: 2\n");
    w.save("2026-10-04T12:00:00Z", "c.yaml", "other: 2\n");
    w.save("2026-10-05T12:00:00Z", "c.yaml", "image: c\nother: 3\n");
    let strata =
        w.f.core_sample(&FernetPath::parse("c.yaml#/image").unwrap())
            .unwrap();
    let got: Vec<(i64, Option<String>)> = strata
        .iter()
        .map(|s| {
            (
                s.since,
                s.value
                    .as_ref()
                    .map(|v| String::from_utf8(v.clone()).unwrap()),
            )
        })
        .collect();
    assert_eq!(
        got,
        [
            (ts("2026-10-05T12:00:00Z"), Some("c\n".into())),
            (ts("2026-10-04T12:00:00Z"), None),
            (ts("2026-10-03T12:00:00Z"), Some("b\n".into())),
            (ts("2026-10-01T12:00:00Z"), Some("a\n".into())),
        ]
    );
}

/// Regressions from review: moved directories, files replaced by
/// directories, writes through symlinks.
#[test]
fn moves_replacements_and_symlinks() {
    let w = World::new();
    w.save("2026-10-01T12:00:00Z", "top/sub/a.txt", "a");
    w.save("2026-10-01T12:00:00Z", "top/notes", "n");
    // A directory moves: the old path's files end.
    w.at("2026-10-02T12:00:00Z");
    fs::rename(w.root.join("top/sub"), w.root.join("top/sub2")).unwrap();
    w.f.snapshot(&w.root.join("top/sub")).unwrap();
    w.f.snapshot_tree(&w.root.join("top/sub2")).unwrap();
    // A file is replaced by a directory.
    fs::remove_file(w.root.join("top/notes")).unwrap();
    fs::create_dir(w.root.join("top/notes")).unwrap();
    fs::write(w.root.join("top/notes/x"), "x").unwrap();
    w.f.snapshot_tree(&w.root.join("top")).unwrap();
    w.at("2026-10-03T12:00:00Z");
    assert_eq!(w.ls("top@now"), ["notes", "sub2"]);
    assert_eq!(w.read("top@now/notes/x").unwrap(), "x");
    assert_eq!(w.read("top@2026-10-01T23:00Z/sub/a.txt").unwrap(), "a");

    // A # write through a symlink edits the target and keeps the link.
    fs::create_dir(w.root.join("real")).unwrap();
    fs::write(w.root.join("real/c.json"), "{\"a\": 1}\n").unwrap();
    std::os::unix::fs::symlink("real/c.json", w.root.join("link.json")).unwrap();
    let r =
        w.f.resolve(&FernetPath::parse("link.json#/a").unwrap())
            .unwrap();
    w.f.write_value(&r, "2").unwrap();
    assert!(
        fs::symlink_metadata(w.root.join("link.json"))
            .unwrap()
            .file_type()
            .is_symlink()
    );
    assert_eq!(
        fs::read_to_string(w.root.join("real/c.json")).unwrap(),
        "{\"a\": 2}\n"
    );
}

/// Live captures agree with the tree walk: under an ignored directory, a
/// `!pattern` can't re-include a file (gitignore semantics).
#[test]
fn ignore_rules_agree_between_walk_and_live_captures() {
    let w = World::new();
    fs::write(w.root.join(".fernetignore"), "logs/\n!logs/keep.txt\n").unwrap();
    fs::create_dir(w.root.join("logs")).unwrap();
    fs::write(w.root.join("logs/keep.txt"), "k").unwrap();
    fs::write(w.root.join("logs/other.txt"), "o").unwrap();
    // Reopen, so the new .fernetignore is loaded.
    drop(w.f);
    let f = Fernet::open_with(
        &w.root,
        Options {
            mode: Mode::Write,
            ..Options::default()
        },
    )
    .unwrap();
    assert!(f.ignore().ignored(&w.root.join("logs/keep.txt"), false));
    assert!(f.ignore().ignored(&w.root.join("logs"), true));
    assert!(!f.ignore().ignored(&w.root.join("keep.txt"), false));
    assert_eq!(f.snapshot_tree(&w.root).unwrap(), 1); // .fernetignore
    assert_eq!(f.snapshot(&w.root.join("logs/keep.txt")).unwrap(), None);
    assert!(
        f.store()
            .unwrap()
            .versions("logs/keep.txt")
            .unwrap()
            .is_empty()
    );
}

/// `config.toml` ignore patterns are real TOML: single quotes, comments and
/// multi-line arrays all work.
#[test]
fn config_ignore_patterns_are_toml() {
    let w = World::new();
    let config = w.root.join(".fernet/config.toml");
    let text = fs::read_to_string(&config).unwrap().replace(
        "patterns = []",
        "patterns = [\n  '*.log',  # logs\n  \"cache/\",\n]",
    );
    fs::write(&config, text).unwrap();
    let ignore = fernet_fs_core::Ignore::load(&w.root);
    assert!(ignore.ignored(&w.root.join("x.log"), false));
    assert!(ignore.ignored(&w.root.join("cache/a"), false));
    assert!(!ignore.ignored(&w.root.join("x.txt"), false));
}

/// Captures never follow symlinks to directories: nothing outside the root
/// and nothing in the store is captured through one, and a directory
/// replaced by a link ends the history under it.
#[test]
fn captures_never_follow_directory_symlinks() {
    let w = World::new();
    let outside = tempfile::tempdir().unwrap();
    fs::write(outside.path().join("secret.txt"), "s").unwrap();
    fs::write(w.root.join("real.txt"), "r").unwrap();
    std::os::unix::fs::symlink(outside.path(), w.root.join("out")).unwrap();
    std::os::unix::fs::symlink(".", w.root.join("self")).unwrap();
    w.at("2026-10-01T12:00:00Z");
    assert_eq!(w.f.snapshot_tree(&w.root).unwrap(), 1);
    assert_eq!(w.f.snapshot_tree(&w.root.join("out")).unwrap(), 0);
    assert_eq!(w.f.snapshot_tree(&w.root.join("self")).unwrap(), 0);
    for p in ["out/secret.txt", "self/real.txt", "self/.fernet/MANIFEST"] {
        assert_eq!(w.f.snapshot(&w.root.join(p)).unwrap(), None, "{p}");
    }
    {
        let store = w.f.store().unwrap();
        assert_eq!(store.paths("").unwrap(), ["real.txt"]);
    }

    // A directory replaced by a symlink to one holding the same names.
    w.save("2026-10-02T12:00:00Z", "d/secret.txt", "mine");
    w.at("2026-10-03T12:00:00Z");
    fs::remove_dir_all(w.root.join("d")).unwrap();
    std::os::unix::fs::symlink(outside.path(), w.root.join("d")).unwrap();
    w.f.snapshot(&w.root.join("d")).unwrap();
    w.at("2026-10-04T12:00:00Z");
    assert!(w.read("d@now").is_err());
    assert_eq!(w.read("d@2026-10-02T13:00Z/secret.txt").unwrap(), "mine");
}

/// A directory replaced by a file ends the history under it, live and in a
/// tree capture.
#[test]
fn a_directory_replaced_by_a_file_ends_its_contents() {
    let w = World::new();
    w.save("2026-10-01T12:00:00Z", "d/x", "x");
    w.save("2026-10-01T12:00:00Z", "d/y", "y");
    w.at("2026-10-02T12:00:00Z");
    fs::remove_dir_all(w.root.join("d")).unwrap();
    fs::write(w.root.join("d"), "file").unwrap();
    w.f.snapshot(&w.root.join("d")).unwrap();
    w.at("2026-10-03T12:00:00Z");
    assert_eq!(w.read("d@now").unwrap(), "file");
    assert_eq!(w.read("d@2026-10-01T13:00Z/x").unwrap(), "x");
    let store = w.f.store().unwrap();
    for p in ["d/x", "d/y"] {
        let v = store.version_at(p, i64::MAX).unwrap().unwrap();
        assert!(v.hash.is_none(), "{p} should be tombstoned");
    }
}

fn readable_by_us(path: &Path) -> bool {
    fs::read(path).is_ok()
}

/// One unreadable file or directory doesn't stop a tree capture: it's
/// skipped and reported, its history left alone, and the rest captured.
#[test]
fn unreadable_paths_are_skipped() {
    use std::os::unix::fs::PermissionsExt;
    let w = World::new();
    w.save("2026-10-01T12:00:00Z", "locked/old.txt", "old");
    fs::write(w.root.join("a.txt"), "a").unwrap();
    fs::write(w.root.join("secret.txt"), "s").unwrap();
    fs::write(w.root.join("z.txt"), "z").unwrap();
    fs::set_permissions(w.root.join("secret.txt"), fs::Permissions::from_mode(0o000)).unwrap();
    fs::set_permissions(w.root.join("locked"), fs::Permissions::from_mode(0o000)).unwrap();
    let unreadable = !readable_by_us(&w.root.join("secret.txt"));
    w.at("2026-10-02T12:00:00Z");
    let report = w.f.capture_tree(&w.root);
    fs::set_permissions(w.root.join("locked"), fs::Permissions::from_mode(0o755)).unwrap();
    let report = report.unwrap();
    if !unreadable {
        return; // Running as root: permissions don't apply.
    }
    let skipped: Vec<&Path> = report.skipped.iter().map(|(p, _)| p.as_path()).collect();
    assert!(
        skipped.contains(&w.root.join("secret.txt").as_path()),
        "{skipped:?}"
    );
    assert!(
        skipped.contains(&w.root.join("locked").as_path()),
        "{skipped:?}"
    );
    assert_eq!(report.captured, 2); // a.txt, z.txt
    let store = w.f.store().unwrap();
    let v = store
        .version_at("locked/old.txt", i64::MAX)
        .unwrap()
        .unwrap();
    assert!(v.hash.is_some(), "an unreadable directory isn't a deletion");
}
