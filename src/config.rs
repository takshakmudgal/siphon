use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::types::Connection;

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct Config {
    #[serde(default)]
    pub backup_dir: Option<PathBuf>,
    #[serde(default, rename = "connection")]
    pub connections: Vec<Connection>,
    /// Path this config was loaded from / will be saved to. Not serialized.
    #[serde(skip)]
    pub path: Option<PathBuf>,
}

impl Config {
    pub fn config_path() -> Result<PathBuf> {
        let base = dirs::home_dir().context("no home dir")?.join(".siphon");
        Ok(base.join("config.toml"))
    }

    pub fn default_backup_root() -> Result<PathBuf> {
        Ok(dirs::home_dir().context("no home dir")?.join(".siphon").join("backups"))
    }

    pub fn load() -> Result<Self> {
        let path = Self::config_path()?;
        Self::load_from(&path)
    }

    pub fn load_from(path: &Path) -> Result<Self> {
        if !path.exists() {
            return Ok(Self {
                path: Some(path.to_path_buf()),
                ..Self::default()
            });
        }
        let s = fs::read_to_string(path)
            .with_context(|| format!("read {}", path.display()))?;
        let mut cfg: Config =
            toml::from_str(&s).with_context(|| format!("parse {}", path.display()))?;
        cfg.path = Some(path.to_path_buf());
        Ok(cfg)
    }

    pub fn save(&self) -> Result<()> {
        let path = match &self.path {
            Some(p) => p.clone(),
            None => Self::config_path()?,
        };
        self.save_to(&path)
    }

    pub fn save_to(&self, path: &Path) -> Result<()> {
        if let Some(dir) = path.parent() {
            fs::create_dir_all(dir).ok();
        }
        let s = toml::to_string_pretty(self).context("serialize config")?;
        fs::write(path, s).with_context(|| format!("write {}", path.display()))?;
        // chmod 600 to avoid leaking passwords.
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = fs::set_permissions(path, fs::Permissions::from_mode(0o600));
        }
        Ok(())
    }

    pub fn upsert(&mut self, conn: Connection) {
        if let Some(existing) = self.connections.iter_mut().find(|c| c.id == conn.id) {
            *existing = conn;
        } else {
            self.connections.push(conn);
        }
    }

    pub fn remove(&mut self, id: &str) -> bool {
        let before = self.connections.len();
        self.connections.retain(|c| c.id != id);
        self.connections.len() != before
    }

    pub fn effective_backup_dir(&self) -> Result<PathBuf> {
        match &self.backup_dir {
            Some(p) => Ok(p.clone()),
            None => Self::default_backup_root(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{AutoBackup, DbKind};
    use tempfile::TempDir;

    #[test]
    fn roundtrip_save_and_load() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("siphon.toml");
        let mut cfg = Config::default();
        cfg.connections.push(Connection {
            id: "abc".into(),
            name: "Local".into(),
            kind: DbKind::Postgres,
            host: "localhost".into(),
            port: 5432,
            user: Some("postgres".into()),
            password: Some("secret".into()),
            database: Some("app".into()),
            auto_backup: Some(AutoBackup::default()),
            ..Default::default()
        });
        cfg.save_to(&path).unwrap();
        let loaded = Config::load_from(&path).unwrap();
        assert_eq!(loaded.connections.len(), 1);
        assert_eq!(loaded.connections[0].name, "Local");
        assert_eq!(loaded.connections[0].password.as_deref(), Some("secret"));
        assert!(loaded.connections[0].auto_backup.is_some());
    }

    #[test]
    fn upsert_and_remove() {
        let mut cfg = Config::default();
        cfg.upsert(Connection {
            id: "1".into(),
            name: "a".into(),
            ..Default::default()
        });
        cfg.upsert(Connection {
            id: "1".into(),
            name: "b".into(),
            ..Default::default()
        });
        assert_eq!(cfg.connections.len(), 1);
        assert_eq!(cfg.connections[0].name, "b");
        assert!(cfg.remove("1"));
        assert!(cfg.connections.is_empty());
    }

    #[test]
    fn missing_file_returns_default() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("nope.toml");
        let cfg = Config::load_from(&path).unwrap();
        assert!(cfg.connections.is_empty());
    }
}
