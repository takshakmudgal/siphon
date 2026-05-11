//! Tests for the auto-backup scheduler against a SQLite source.

use std::sync::Arc;

use siphon::config::Config;
use siphon::schedule::{Scheduler, SchedulerEvent};
use siphon::types::{AutoBackup, Connection, DbKind};
use tokio::sync::{Mutex, mpsc};

#[tokio::test]
async fn scheduler_fires_due_backup_and_updates_last_at() {
    if which::which("sqlite3").is_err() {
        return;
    }
    let tmp = tempfile::TempDir::new().unwrap();
    let src = tmp.path().join("src.sqlite");
    std::process::Command::new("sqlite3")
        .arg(&src)
        .arg("CREATE TABLE t(x int); INSERT INTO t VALUES (1);")
        .status()
        .unwrap();

    let cfg_path = tmp.path().join("config.toml");
    let mut cfg = Config::load_from(&cfg_path).unwrap();
    cfg.backup_dir = Some(tmp.path().to_path_buf());
    cfg.connections.push(Connection {
        id: "auto-1".into(),
        name: "auto-sqlite".into(),
        kind: DbKind::Sqlite,
        database: Some(src.to_string_lossy().to_string()),
        auto_backup: Some(AutoBackup {
            enabled: true,
            interval_secs: 0, // always due
            retention: 3,
        }),
        last_backup_at: None,
        ..Default::default()
    });
    let config = Arc::new(Mutex::new(cfg));

    let (tx, mut rx) = mpsc::unbounded_channel::<SchedulerEvent>();
    let sched = Scheduler::new(config.clone(), tx);
    sched.run_once().await;

    // Drain events.
    let mut saw_success = false;
    while let Ok(ev) = rx.try_recv() {
        if let SchedulerEvent::Succeeded { name, .. } = ev {
            assert_eq!(name, "auto-sqlite");
            saw_success = true;
        }
    }
    assert!(saw_success, "scheduler did not emit a success event");

    let cfg = config.lock().await;
    let conn = &cfg.connections[0];
    assert!(conn.last_backup_at.unwrap_or(0) > 0, "last_backup_at was not updated");
}

#[tokio::test]
async fn scheduler_skips_when_not_due() {
    let tmp = tempfile::TempDir::new().unwrap();
    let cfg_path = tmp.path().join("config.toml");
    let mut cfg = Config::load_from(&cfg_path).unwrap();
    cfg.backup_dir = Some(tmp.path().to_path_buf());
    cfg.connections.push(Connection {
        id: "fresh".into(),
        name: "fresh".into(),
        kind: DbKind::Sqlite,
        database: Some(tmp.path().join("does-not-exist.sqlite").to_string_lossy().to_string()),
        auto_backup: Some(AutoBackup {
            enabled: true,
            interval_secs: 3_600,
            retention: 3,
        }),
        last_backup_at: Some(chrono::Utc::now().timestamp()),
        ..Default::default()
    });
    let config = Arc::new(Mutex::new(cfg));
    let (tx, mut rx) = mpsc::unbounded_channel::<SchedulerEvent>();
    Scheduler::new(config, tx).run_once().await;
    assert!(rx.try_recv().is_err(), "should have skipped");
}
