//! Milestone 5 formats through the resolver: reads and writes with `#`.

#![allow(clippy::unwrap_used)]

use std::fs;

use fernet_fs_core::{Fernet, NewValue};
use fernet_fs_path::FernetPath;

#[test]
fn markdown_and_csv_through_the_resolver() {
    let tmp = tempfile::tempdir().unwrap();
    let p = tmp.path();
    fs::write(
        p.join("README.md"),
        "# App\n\nIntro.\n\n## Install\n\nRun it.\n",
    )
    .unwrap();
    fs::write(p.join("people.csv"), "id,name\n1,ana\n2,bo\n").unwrap();
    let f = Fernet::open(p).unwrap();
    let r = |s: &str| f.resolve(&FernetPath::parse(s).unwrap()).unwrap();
    let names = |s: &str| {
        f.list(&r(s))
            .unwrap()
            .into_iter()
            .map(|e| e.name.into_string().unwrap())
            .collect::<Vec<_>>()
    };

    assert_eq!(names("README.md#"), ["body", "App"]);
    assert_eq!(names("README.md#/App"), ["body", "Install"]);
    assert_eq!(
        f.read(&r("README.md#/App/Install/body")).unwrap(),
        b"\nRun it.\n"
    );
    f.write_value(&r("README.md#/App/Install/body"), "\ncargo install app\n")
        .unwrap();
    f.insert(
        &r("README.md#/App"),
        "License",
        NewValue::Text("MIT".into()),
    )
    .unwrap();
    assert_eq!(
        fs::read_to_string(p.join("README.md")).unwrap(),
        "# App\n\nIntro.\n\n## Install\n\ncargo install app\n\n## License\n\nMIT\n"
    );

    assert_eq!(names("people.csv#"), ["0", "1"]);
    assert_eq!(f.read(&r("people.csv#/1/name")).unwrap(), b"bo\n");
    f.write_value(&r("people.csv#/1/name"), "bo, jr").unwrap();
    f.insert(&r("people.csv#"), "2", NewValue::EmptyObject)
        .unwrap();
    f.write_value(&r("people.csv#/2/name"), "cy").unwrap();
    assert_eq!(
        fs::read_to_string(p.join("people.csv")).unwrap(),
        "id,name\n1,ana\n2,\"bo, jr\"\n,cy\n"
    );
}
