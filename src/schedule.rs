use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use chrono::Utc;
use tokio::sync::{Mutex, mpsc};
use tokio::task::JoinHandle;

use crate::backup;
use crate::config::Config;
use crate::types::Connection;

/// Event emitted by the scheduler so the UI can react.
#[derive(Debug, Clone)]
pub enum SchedulerEvent {
    Started { conn_id: String, name: String },
    Succeeded {
        conn_id: String,
        name: String,
        path: PathBuf,
        bytes: u64,
    },
    Failed { conn_id: String, name: String, error: String },
}

#[derive(Clone)]
pub struct Scheduler {
    config: Arc<Mutex<Config>>,
    tx: mpsc::UnboundedSender<SchedulerEvent>,
}

impl Scheduler {
    pub fn new(
        config: Arc<Mutex<Config>>,
        tx: mpsc::UnboundedSender<SchedulerEvent>,
    ) -> Self {
        Self { config, tx }
    }

    /// Spawn the scheduler background loop. Aborts on drop of the handle.
    pub fn spawn(self) -> JoinHandle<()> {
        tokio::spawn(async move {
            // Tick every 30s — fast enough for short test intervals, light enough
            // to be invisible at idle.
            let mut tick = tokio::time::interval(Duration::from_secs(30));
            tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            loop {
                tick.tick().await;
                self.run_once().await;
            }
        })
    }

    /// Walk the config and run any backup that's due.
    pub async fn run_once(&self) {
        let (root, due) = {
            let cfg = self.config.lock().await;
            let Ok(root) = cfg.effective_backup_dir() else { return };
            let now = Utc::now().timestamp();
            let due: Vec<Connection> = cfg
                .connections
                .iter()
                .filter(|c| match &c.auto_backup {
                    Some(ab) if ab.enabled => {
                        let last = c.last_backup_at.unwrap_or(0);
                        now - last >= ab.interval_secs as i64
                    }
                    _ => false,
                })
                .cloned()
                .collect();
            (root, due)
        };

        for conn in due {
            self.run_one(&root, conn).await;
        }
    }

    async fn run_one(&self, root: &std::path::Path, conn: Connection) {
        let _ = self.tx.send(SchedulerEvent::Started {
            conn_id: conn.id.clone(),
            name: conn.name.clone(),
        });
        match backup::dump(root, &conn).await {
            Ok(outcome) => {
                let _ = self.tx.send(SchedulerEvent::Succeeded {
                    conn_id: conn.id.clone(),
                    name: conn.name.clone(),
                    path: outcome.path,
                    bytes: outcome.bytes,
                });
                let keep = conn.auto_backup.as_ref().map(|a| a.retention).unwrap_or(7);
                let _ = backup::prune(root, &conn, keep);
                let mut cfg = self.config.lock().await;
                if let Some(c) = cfg.connections.iter_mut().find(|c| c.id == conn.id) {
                    c.last_backup_at = Some(Utc::now().timestamp());
                }
                let _ = cfg.save();
            }
            Err(e) => {
                let _ = self.tx.send(SchedulerEvent::Failed {
                    conn_id: conn.id.clone(),
                    name: conn.name.clone(),
                    error: format!("{:#}", e),
                });
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use crate::types::{AutoBackup, Connection, DbKind};

    #[test]
    fn is_due_filter() {
        let mut cfg = Config::default();
        let now = Utc::now().timestamp();
        cfg.connections.push(Connection {
            id: "1".into(),
            name: "disabled".into(),
            kind: DbKind::Postgres,
            auto_backup: Some(AutoBackup {
                enabled: false,
                interval_secs: 60,
                retention: 3,
            }),
            last_backup_at: Some(now - 10_000),
            ..Default::default()
        });
        cfg.connections.push(Connection {
            id: "2".into(),
            name: "due".into(),
            kind: DbKind::Postgres,
            auto_backup: Some(AutoBackup {
                enabled: true,
                interval_secs: 60,
                retention: 3,
            }),
            last_backup_at: Some(now - 120),
            ..Default::default()
        });
        cfg.connections.push(Connection {
            id: "3".into(),
            name: "fresh".into(),
            kind: DbKind::Postgres,
            auto_backup: Some(AutoBackup {
                enabled: true,
                interval_secs: 3600,
                retention: 3,
            }),
            last_backup_at: Some(now - 10),
            ..Default::default()
        });
        cfg.connections.push(Connection {
            id: "4".into(),
            name: "never".into(),
            kind: DbKind::Postgres,
            auto_backup: Some(AutoBackup {
                enabled: true,
                interval_secs: 60,
                retention: 3,
            }),
            last_backup_at: None,
            ..Default::default()
        });
        let due: Vec<_> = cfg
            .connections
            .iter()
            .filter(|c| match &c.auto_backup {
                Some(ab) if ab.enabled => {
                    let last = c.last_backup_at.unwrap_or(0);
                    now - last >= ab.interval_secs as i64
                }
                _ => false,
            })
            .map(|c| c.id.clone())
            .collect();
        assert_eq!(due, vec!["2".to_string(), "4".to_string()]);
    }
}
