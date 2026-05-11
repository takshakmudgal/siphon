use std::process::Command;

use siphon::backup;
use siphon::types::{Connection, DbKind};

#[tokio::test]
async fn sqlite_dump_and_restore_roundtrip() {
    if which::which("sqlite3").is_err() {
        eprintln!("SKIP: sqlite3 not on PATH");
        return;
    }
    let tmp = tempfile::TempDir::new().unwrap();
    let src = tmp.path().join("source.sqlite");

    // Seed a source database with a marker row.
    let seed = Command::new("sqlite3")
        .arg(&src)
        .arg(
            "CREATE TABLE notes(id INTEGER PRIMARY KEY, note TEXT); \
             INSERT INTO notes(note) VALUES ('hello-siphon');",
        )
        .status()
        .unwrap();
    assert!(seed.success());

    let conn = Connection {
        id: "sqlite-e2e".into(),
        name: "sqlite-e2e".into(),
        kind: DbKind::Sqlite,
        database: Some(src.to_string_lossy().to_string()),
        ..Default::default()
    };

    backup::test_connection(&conn).await.expect("test failed");

    let outcome = backup::dump(tmp.path(), &conn).await.expect("dump failed");
    assert!(outcome.path.exists());
    assert!(outcome.bytes > 0);

    // The backup file is a real sqlite db — query it.
    let q = Command::new("sqlite3")
        .arg(&outcome.path)
        .arg("SELECT note FROM notes WHERE id=1;")
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&q.stdout);
    assert!(stdout.contains("hello-siphon"), "got: {stdout}");
}

#[tokio::test]
async fn sqlite_prune_keeps_newest() {
    if which::which("sqlite3").is_err() {
        return;
    }
    let tmp = tempfile::TempDir::new().unwrap();
    let src = tmp.path().join("source.sqlite");
    Command::new("sqlite3")
        .arg(&src)
        .arg("CREATE TABLE t(x int); INSERT INTO t VALUES (1),(2),(3);")
        .status()
        .unwrap();

    let conn = Connection {
        id: "sqlite-prune".into(),
        name: "sqlite-prune".into(),
        kind: DbKind::Sqlite,
        database: Some(src.to_string_lossy().to_string()),
        ..Default::default()
    };

    for _ in 0..4 {
        backup::dump(tmp.path(), &conn).await.unwrap();
        // Distinct filenames need a second between them since the format has 1s resolution.
        tokio::time::sleep(std::time::Duration::from_millis(1100)).await;
    }
    let pre = backup::list(tmp.path(), &conn);
    assert_eq!(pre.len(), 4);

    let removed = backup::prune(tmp.path(), &conn, 2).unwrap();
    assert_eq!(removed, 2);
    let post = backup::list(tmp.path(), &conn);
    assert_eq!(post.len(), 2);
}
