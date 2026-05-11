//! End-to-end: dump the postgres-dev container, drop the marker table, then
//! restore from the dump and verify the marker is back.

use std::process::Command;

use siphon::backup;
use siphon::restore;
use siphon::types::{Connection, DbKind};

const CONTAINER: &str = "postgres-dev";

fn docker_ready() -> bool {
    Command::new("docker")
        .arg("version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

fn container_running(name: &str) -> bool {
    Command::new("docker")
        .args(["ps", "--filter", &format!("name=^{}$", name), "--format", "{{.Names}}"])
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim() == name)
        .unwrap_or(false)
}

fn creds_from_container() -> Option<(String, String, String)> {
    let env = Command::new("docker")
        .args(["inspect", CONTAINER, "--format", "{{json .Config.Env}}"])
        .output()
        .ok()?;
    let parsed: Vec<String> = serde_json::from_str(String::from_utf8_lossy(&env.stdout).trim()).ok()?;
    let mut user = None;
    let mut password = None;
    let mut db = None;
    for kv in parsed {
        if let Some((k, v)) = kv.split_once('=') {
            match k {
                "POSTGRES_USER" => user = Some(v.to_string()),
                "POSTGRES_PASSWORD" => password = Some(v.to_string()),
                "POSTGRES_DB" => db = Some(v.to_string()),
                _ => {}
            }
        }
    }
    Some((
        user.unwrap_or_else(|| "postgres".into()),
        password.unwrap_or_default(),
        db.unwrap_or_else(|| "postgres".into()),
    ))
}

fn psql_inside(conn: &Connection, sql: &str) -> std::process::Output {
    Command::new("docker")
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
            "-tAc",
            sql,
        ])
        .output()
        .expect("psql exec")
}

#[tokio::test]
async fn postgres_dump_then_restore_round_trip() {
    if !docker_ready() || !container_running(CONTAINER) {
        eprintln!("SKIP: docker / postgres-dev not available");
        return;
    }
    let Some((user, password, db)) = creds_from_container() else {
        eprintln!("SKIP: couldn't parse container env");
        return;
    };

    // Use the attached-container runtime so we don't pull pg_dump locally.
    let conn = Connection {
        id: "e2e-restore".into(),
        name: "e2e-restore".into(),
        kind: DbKind::Postgres,
        host: "127.0.0.1".into(),
        port: 5432,
        user: Some(user),
        password: Some(password),
        database: Some(db),
        container_id: Some(
            Command::new("docker")
                .args(["inspect", "-f", "{{.Id}}", CONTAINER])
                .output()
                .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
                .unwrap_or_default(),
        ),
        container_name: Some(CONTAINER.into()),
        ..Default::default()
    };

    // Seed a marker with a distinguishable row value.
    let seed = psql_inside(
        &conn,
        "DROP TABLE IF EXISTS restore_marker; \
         CREATE TABLE restore_marker(k text primary key, v text); \
         INSERT INTO restore_marker(k, v) VALUES('canary','before-restore');",
    );
    assert!(seed.status.success(), "seed failed: {}", String::from_utf8_lossy(&seed.stderr));

    let tmp = tempfile::TempDir::new().unwrap();
    let outcome = backup::dump(tmp.path(), &conn).await.expect("dump");

    // Mutate the row so we can prove the restore actually overwrote it.
    let mutate = psql_inside(
        &conn,
        "UPDATE restore_marker SET v='after-mutation' WHERE k='canary';",
    );
    assert!(mutate.status.success());
    let after_mutation = psql_inside(&conn, "SELECT v FROM restore_marker WHERE k='canary';");
    assert_eq!(
        String::from_utf8_lossy(&after_mutation.stdout).trim(),
        "after-mutation"
    );

    // Restore from the dump.
    let restore_outcome = restore::restore(&conn, &outcome.path).await.expect("restore");
    assert!(restore_outcome.runtime_used.starts_with("docker exec"));

    let after_restore = psql_inside(&conn, "SELECT v FROM restore_marker WHERE k='canary';");
    assert!(after_restore.status.success(), "post-restore query failed");
    assert_eq!(
        String::from_utf8_lossy(&after_restore.stdout).trim(),
        "before-restore",
        "restore did not put the original value back"
    );

    // Cleanup the marker table.
    let _ = psql_inside(&conn, "DROP TABLE IF EXISTS restore_marker;");
}

#[tokio::test]
async fn restore_missing_file_errors_clearly() {
    if !docker_ready() {
        return;
    }
    let conn = Connection {
        id: "x".into(),
        name: "x".into(),
        kind: DbKind::Postgres,
        host: "127.0.0.1".into(),
        port: 5432,
        ..Default::default()
    };
    let err = restore::restore(&conn, std::path::Path::new("/nope/missing.dump"))
        .await
        .unwrap_err()
        .to_string();
    assert!(err.contains("not found"), "got: {err}");
}

#[tokio::test]
async fn redis_restore_returns_friendly_message() {
    let conn = Connection {
        id: "r".into(),
        name: "r".into(),
        kind: DbKind::Redis,
        host: "127.0.0.1".into(),
        port: 6379,
        ..Default::default()
    };
    // Provide a real-ish file so we get past the not-found bail.
    let tmp = tempfile::NamedTempFile::new().unwrap();
    let err = restore::restore(&conn, tmp.path())
        .await
        .unwrap_err()
        .to_string();
    assert!(err.contains("redis restore isn't supported"));
}
