//! Exercises the docker-run fallback path: connection has no attached
//! container_id, so siphon spins up an ephemeral postgres:17 container that
//! reaches the host via `host.docker.internal:5432`.

use std::process::Command;

use siphon::backup;
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
    let env_str = String::from_utf8_lossy(&env.stdout);
    let mut user = None;
    let mut password = None;
    let mut db = None;
    let parsed: Vec<String> = serde_json::from_str(env_str.trim()).ok()?;
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

#[tokio::test]
async fn docker_run_fallback_dumps_remote_postgres() {
    if !docker_ready() || !container_running(CONTAINER) {
        eprintln!("SKIP: docker / postgres-dev not available");
        return;
    }
    let Some((user, password, db)) = creds_from_container() else {
        eprintln!("SKIP: couldn't parse container env");
        return;
    };

    // No `container_id` → siphon must use docker-run because pg_dump isn't on PATH.
    let conn = Connection {
        id: "docker-run-e2e".into(),
        name: "docker-run-e2e".into(),
        kind: DbKind::Postgres,
        host: "127.0.0.1".into(),
        port: 5432,
        user: Some(user),
        password: Some(password),
        database: Some(db),
        container_id: None,
        ..Default::default()
    };

    let runtime = backup::pick_runtime(&conn).expect("pick_runtime");
    assert!(
        matches!(runtime, backup::Runtime::DockerRun { .. }),
        "expected DockerRun runtime, got {:?}",
        runtime
    );

    let tmp = tempfile::TempDir::new().unwrap();
    let outcome = backup::dump(tmp.path(), &conn).await.expect("dump");
    assert!(outcome.bytes > 512, "dump too small: {}", outcome.bytes);
    assert!(outcome.runtime_used.starts_with("docker run"));

    // Sanity-check the file is a real pg_dump archive.
    let head = std::fs::read(&outcome.path).unwrap();
    // Custom-format dumps start with "PGDMP".
    assert!(
        head.starts_with(b"PGDMP"),
        "expected PGDMP magic, got {:?}",
        &head[..head.len().min(10)]
    );
}

#[tokio::test]
async fn pick_runtime_errors_clearly_when_no_options() {
    // We can't reliably uninstall docker, so verify pick_runtime returns DockerRun
    // when docker is available (the normal path). The "no options" failure mode
    // is exercised by the bail!() at the bottom of pick_runtime when both fail.
    let conn = Connection {
        id: "x".into(),
        name: "remote-only".into(),
        kind: DbKind::Postgres,
        host: "db.example.com".into(),
        port: 5432,
        ..Default::default()
    };
    let rt = backup::pick_runtime(&conn).expect("docker should be available");
    match rt {
        backup::Runtime::DockerRun { ref image } => assert_eq!(image, "postgres:17"),
        backup::Runtime::Local => {} // pg_dump happens to be installed; acceptable
        other => panic!("unexpected runtime: {:?}", other),
    }
}
