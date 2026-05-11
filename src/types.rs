use std::fmt;
use std::path::PathBuf;
use std::time::Duration;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum DbKind {
    Postgres,
    Mongo,
    Mysql,
    Sqlite,
    Redis,
}

impl DbKind {
    pub const ALL: &'static [DbKind] = &[
        DbKind::Postgres,
        DbKind::Mongo,
        DbKind::Mysql,
        DbKind::Sqlite,
        DbKind::Redis,
    ];

    pub fn label(self) -> &'static str {
        match self {
            DbKind::Postgres => "Postgres",
            DbKind::Mongo => "MongoDB",
            DbKind::Mysql => "MySQL",
            DbKind::Sqlite => "SQLite",
            DbKind::Redis => "Redis",
        }
    }

    pub fn default_port(self) -> u16 {
        match self {
            DbKind::Postgres => 5432,
            DbKind::Mongo => 27017,
            DbKind::Mysql => 3306,
            DbKind::Redis => 6379,
            DbKind::Sqlite => 0,
        }
    }

    pub fn uri_scheme(self) -> &'static str {
        match self {
            DbKind::Postgres => "postgres",
            DbKind::Mongo => "mongodb",
            DbKind::Mysql => "mysql",
            DbKind::Redis => "redis",
            DbKind::Sqlite => "sqlite",
        }
    }

    pub fn config_key(self) -> &'static str {
        match self {
            DbKind::Postgres => "postgres",
            DbKind::Mongo => "mongo",
            DbKind::Mysql => "mysql",
            DbKind::Sqlite => "sqlite",
            DbKind::Redis => "redis",
        }
    }

    pub fn from_config_key(s: &str) -> Option<DbKind> {
        match s {
            "postgres" => Some(DbKind::Postgres),
            "mongo" => Some(DbKind::Mongo),
            "mysql" => Some(DbKind::Mysql),
            "sqlite" => Some(DbKind::Sqlite),
            "redis" => Some(DbKind::Redis),
            _ => None,
        }
    }

    pub fn from_image(image: &str) -> Option<DbKind> {
        let lower = image.to_lowercase();
        let bare = lower.rsplit('/').next().unwrap_or(&lower);
        let bare = bare.split(':').next().unwrap_or(bare);
        match bare {
            "postgres" | "postgis" | "timescaledb" | "pgvector" => Some(DbKind::Postgres),
            "mongo" | "mongodb" | "mongodb-community-server" => Some(DbKind::Mongo),
            "mysql" | "mariadb" | "percona" => Some(DbKind::Mysql),
            "redis" | "redis-stack" | "redis-stack-server" | "valkey" => Some(DbKind::Redis),
            _ if lower.contains("postgres") || lower.contains("postgis") || lower.contains("timescaledb") => {
                Some(DbKind::Postgres)
            }
            _ if lower.contains("mongo") => Some(DbKind::Mongo),
            _ if lower.contains("mariadb") || lower.contains("mysql") => Some(DbKind::Mysql),
            _ if lower.contains("redis") || lower.contains("valkey") => Some(DbKind::Redis),
            _ => None,
        }
    }
}

impl fmt::Display for DbKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.label())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct Connection {
    pub id: String,
    pub name: String,
    pub kind: DbKind,
    pub host: String,
    pub port: u16,
    #[serde(default)]
    pub user: Option<String>,
    #[serde(default)]
    pub password: Option<String>,
    /// Specific database/path. For Mongo, an empty string means "all databases".
    #[serde(default)]
    pub database: Option<String>,
    /// Full URI override (takes precedence over host/port/user/etc when present).
    #[serde(default)]
    pub uri: Option<String>,
    /// Container ID — when present we run dump tools via `docker exec`.
    #[serde(default)]
    pub container_id: Option<String>,
    #[serde(default)]
    pub container_name: Option<String>,
    #[serde(default)]
    pub auto_backup: Option<AutoBackup>,
    /// Last successful dump timestamp (epoch seconds).
    #[serde(default)]
    pub last_backup_at: Option<i64>,
}

impl Connection {
    /// Stable identity for the *underlying* database, used to reject duplicate
    /// saved connections. Two configs that point at the same DB return equal
    /// fingerprints regardless of cosmetic differences (name casing,
    /// container_id presence, etc.).
    pub fn fingerprint(&self) -> String {
        if self.kind == DbKind::Sqlite {
            let p = self
                .database
                .as_deref()
                .or(self.uri.as_deref())
                .unwrap_or("");
            let canon = std::path::PathBuf::from(p)
                .canonicalize()
                .map(|p| p.to_string_lossy().to_string())
                .unwrap_or_else(|_| p.to_string());
            return format!("sqlite:{canon}");
        }
        if let Some(uri) = self.uri.as_deref().filter(|s| !s.is_empty()) {
            if let Ok(u) = url::Url::parse(uri) {
                let host = u.host_str().unwrap_or("").to_lowercase();
                let port = u.port().unwrap_or(self.kind.default_port());
                let db = u.path().trim_start_matches('/').to_lowercase();
                return format!("{}:{}:{}:{}", self.kind.config_key(), host, port, db);
            }
        }
        let host = if self.host.is_empty() {
            "127.0.0.1".to_string()
        } else {
            self.host.to_lowercase()
        };
        let port = if self.port == 0 {
            self.kind.default_port()
        } else {
            self.port
        };
        let db = self
            .database
            .as_deref()
            .unwrap_or("")
            .to_lowercase();
        format!("{}:{}:{}:{}", self.kind.config_key(), host, port, db)
    }
}

impl Default for DbKind {
    fn default() -> Self {
        DbKind::Postgres
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AutoBackup {
    pub enabled: bool,
    pub interval_secs: u64,
    pub retention: usize,
}

impl AutoBackup {
    pub fn interval(&self) -> Duration {
        Duration::from_secs(self.interval_secs)
    }
}

impl Default for AutoBackup {
    fn default() -> Self {
        Self {
            enabled: true,
            interval_secs: 60 * 60,
            retention: 7,
        }
    }
}

#[derive(Debug, Clone)]
pub struct DetectedSource {
    pub kind: DbKind,
    pub container_id: String,
    pub container_name: String,
    pub image: String,
    pub host_port: u16,
    pub user: Option<String>,
    pub password: Option<String>,
    pub database: Option<String>,
}

impl DetectedSource {
    pub fn fingerprint(&self) -> String {
        format!("docker:{}", self.container_id)
    }

    pub fn display_name(&self) -> String {
        if self.container_name.is_empty() {
            format!("{} · {}", self.kind, self.image)
        } else {
            self.container_name.clone()
        }
    }
}

#[derive(Debug, Clone)]
pub struct BackupFile {
    pub path: PathBuf,
    pub bytes: u64,
    pub created_at: i64,
}

pub fn human_bytes(n: u64) -> String {
    const KB: f64 = 1024.0;
    let n = n as f64;
    if n < KB {
        return format!("{n:.0} B");
    }
    let mb = n / KB / KB;
    if mb < 1.0 {
        return format!("{:.1} KB", n / KB);
    }
    if mb < 1024.0 {
        return format!("{mb:.1} MB");
    }
    format!("{:.2} GB", mb / 1024.0)
}

pub fn human_duration(secs: u64) -> String {
    if secs < 60 {
        return format!("{secs}s");
    }
    if secs < 3600 {
        return format!("{}m", secs / 60);
    }
    if secs < 86_400 {
        let h = secs / 3600;
        let m = (secs % 3600) / 60;
        if m == 0 {
            format!("{h}h")
        } else {
            format!("{h}h{m}m")
        }
    } else {
        format!("{}d", secs / 86_400)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn image_detection() {
        assert_eq!(DbKind::from_image("postgres:17"), Some(DbKind::Postgres));
        assert_eq!(DbKind::from_image("postgres"), Some(DbKind::Postgres));
        assert_eq!(
            DbKind::from_image("docker.io/library/postgres:16-alpine"),
            Some(DbKind::Postgres)
        );
        assert_eq!(DbKind::from_image("mongo:7"), Some(DbKind::Mongo));
        assert_eq!(DbKind::from_image("mongo"), Some(DbKind::Mongo));
        assert_eq!(DbKind::from_image("mysql:8"), Some(DbKind::Mysql));
        assert_eq!(DbKind::from_image("mariadb:11"), Some(DbKind::Mysql));
        assert_eq!(DbKind::from_image("redis:7-alpine"), Some(DbKind::Redis));
        assert_eq!(
            DbKind::from_image("postgis/postgis:16-3.4"),
            Some(DbKind::Postgres)
        );
        assert_eq!(
            DbKind::from_image("timescale/timescaledb:latest-pg16"),
            Some(DbKind::Postgres)
        );
        assert_eq!(DbKind::from_image("nginx:latest"), None);
        assert_eq!(DbKind::from_image("corentinth/it-tools"), None);
    }

    #[test]
    fn human_bytes_rounds_reasonably() {
        assert_eq!(human_bytes(0), "0 B");
        assert_eq!(human_bytes(900), "900 B");
        assert_eq!(human_bytes(2048), "2.0 KB");
        assert!(human_bytes(2_500_000).ends_with("MB"));
        assert!(human_bytes(3_000_000_000).ends_with("GB"));
    }

    #[test]
    fn fingerprint_equal_for_same_db() {
        let a = Connection {
            kind: DbKind::Postgres,
            host: "DB.example.com".into(),
            port: 5432,
            database: Some("prod".into()),
            ..Default::default()
        };
        let b = Connection {
            kind: DbKind::Postgres,
            host: "db.example.com".into(),
            port: 5432,
            database: Some("PROD".into()),
            // different cosmetic fields:
            name: "different name".into(),
            user: Some("ignored".into()),
            ..Default::default()
        };
        assert_eq!(a.fingerprint(), b.fingerprint());
    }

    #[test]
    fn fingerprint_differs_when_db_differs() {
        let a = Connection {
            kind: DbKind::Postgres,
            host: "db.example.com".into(),
            port: 5432,
            database: Some("prod".into()),
            ..Default::default()
        };
        let b = Connection {
            kind: DbKind::Postgres,
            host: "db.example.com".into(),
            port: 5432,
            database: Some("staging".into()),
            ..Default::default()
        };
        assert_ne!(a.fingerprint(), b.fingerprint());
    }

    #[test]
    fn fingerprint_uri_mode_matches_field_mode_for_same_db() {
        let by_fields = Connection {
            kind: DbKind::Postgres,
            host: "db.example.com".into(),
            port: 5432,
            database: Some("prod".into()),
            ..Default::default()
        };
        let by_uri = Connection {
            kind: DbKind::Postgres,
            uri: Some("postgres://x:y@db.example.com:5432/prod".into()),
            ..Default::default()
        };
        assert_eq!(by_fields.fingerprint(), by_uri.fingerprint());
    }

    #[test]
    fn fingerprint_kind_changes_value() {
        let pg = Connection {
            kind: DbKind::Postgres,
            host: "h".into(),
            port: 5432,
            database: Some("d".into()),
            ..Default::default()
        };
        let my = Connection {
            kind: DbKind::Mysql,
            host: "h".into(),
            port: 5432,
            database: Some("d".into()),
            ..Default::default()
        };
        assert_ne!(pg.fingerprint(), my.fingerprint());
    }

    #[test]
    fn human_duration_buckets() {
        assert_eq!(human_duration(5), "5s");
        assert_eq!(human_duration(120), "2m");
        assert_eq!(human_duration(3600), "1h");
        assert_eq!(human_duration(3660), "1h1m");
        assert_eq!(human_duration(86_400 * 3), "3d");
    }
}
