//! End-to-end test: detect the locally-running `postgres-dev` container,
//! dump it, then verify the dump is restorable.
//!
//! Skipped (with a printed reason) if docker or the container isn't around.

use std::path::PathBuf;
use std::process::Command;

use siphon::backup;
use siphon::detect;
use siphon::types::{Connection, DbKind};

const CONTAINER: &str = "postgres-dev";

fn docker_available() -> bool {
    Command::new("docker")
        .arg("version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

fn container_running(name: &str) -> bool {
    let out = Command::new("docker")
        .args(["ps", "--filter", &format!("name=^{}$", name), "--format", "{{.Names}}"])
        .output();
    match out {
        Ok(o) => String::from_utf8_lossy(&o.stdout).trim() == name,
        Err(_) => false,
    }
}

#[tokio::test]
async fn detects_postgres_container() {
    if !docker_available() || !container_running(CONTAINER) {
        eprintln!("SKIP: no docker / postgres-dev container");
        return;
    }
    let sources = detect::scan().await.expect("scan failed");
    let pg = sources
        .iter()
        .find(|s| s.container_name == CONTAINER && matches!(s.kind, DbKind::Postgres))
        .expect("postgres-dev not detected");
    assert!(pg.user.is_some(), "user should be picked up from env");
    assert_eq!(pg.host_port, 5432);
}

#[tokio::test]
async fn dumps_postgres_container_end_to_end() {
    if !docker_available() || !container_running(CONTAINER) {
        eprintln!("SKIP: no docker / postgres-dev container");
        return;
    }
    let sources = detect::scan().await.expect("scan failed");
    let pg = sources
        .into_iter()
        .find(|s| s.container_name == CONTAINER && matches!(s.kind, DbKind::Postgres))
        .expect("postgres-dev not detected");

    let conn = Connection {
        id: "e2e-postgres".into(),
        name: "e2e-postgres".into(),
        kind: DbKind::Postgres,
        host: "127.0.0.1".into(),
        port: pg.host_port,
        user: pg.user.clone(),
        password: pg.password.clone(),
        database: pg.database.clone(),
        container_id: Some(pg.container_id.clone()),
        container_name: Some(pg.container_name.clone()),
        ..Default::default()
    };

    // Connect-test first.
    let v = backup::test_connection(&conn)
        .await
        .expect("psql version probe failed");
    assert!(v.contains("PostgreSQL"), "expected version string, got {v:?}");

    // Now seed a marker table so we can confirm the dump contains real content.
    let _ = Command::new("docker")
        .args([
            "exec",
            "-e",
            &format!("PGPASSWORD={}", conn.password.as_deref().unwrap_or("")),
            CONTAINER,
            "psql",
            "-U",
            conn.user.as_deref().unwrap_or("postgres"),
            "-d",
            conn.database.as_deref().unwrap_or("postgres"),
            "-c",
            "CREATE TABLE IF NOT EXISTS siphon_marker(id int primary key, note text); \
             INSERT INTO siphon_marker(id, note) VALUES (1,'hello-siphon') ON CONFLICT DO NOTHING;",
        ])
        .output()
        .expect("seed failed");

    let tmp = tempfile::TempDir::new().unwrap();
    let outcome = backup::dump(tmp.path(), &conn)
        .await
        .expect("dump failed");

    assert!(outcome.path.exists(), "dump path missing");
    assert!(outcome.bytes > 1024, "dump suspiciously small: {}", outcome.bytes);

    // Verify the dump round-trips: list its contents via pg_restore inside the same container.
    let dump_inside = format!("/tmp/siphon-e2e-{}.dump", std::process::id());
    let cp = Command::new("docker")
        .args(["cp", outcome.path.to_str().unwrap(), &format!("{}:{}", CONTAINER, dump_inside)])
        .output()
        .expect("docker cp");
    assert!(cp.status.success(), "docker cp failed: {:?}", String::from_utf8_lossy(&cp.stderr));

    let listing = Command::new("docker")
        .args(["exec", CONTAINER, "pg_restore", "-l", &dump_inside])
        .output()
        .expect("pg_restore -l");
    assert!(listing.status.success(), "pg_restore -l failed");
    let listing_str = String::from_utf8_lossy(&listing.stdout);
    assert!(
        listing_str.contains("siphon_marker"),
        "marker table missing from dump TOC:\n{}",
        listing_str
    );

    // Cleanup the inside-container scratch file.
    let _ = Command::new("docker")
        .args(["exec", CONTAINER, "rm", "-f", &dump_inside])
        .status();
}

#[tokio::test]
async fn dump_dir_layout_is_stable() {
    // Pure unit-ish check that the directory naming is deterministic.
    let conn = Connection {
        id: "abcdef0123456789".into(),
        name: "Local Postgres".into(),
        kind: DbKind::Postgres,
        ..Default::default()
    };
    let root = PathBuf::from("/tmp/siphon-e2e-root");
    let dir = backup::dir_for(&root, &conn);
    assert_eq!(
        dir.to_string_lossy(),
        "/tmp/siphon-e2e-root/local-postgres-abcdef01"
    );
}
