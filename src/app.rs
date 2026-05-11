use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;

use tokio::sync::Mutex;

use crate::config::Config;
use crate::types::{AutoBackup, Connection, DbKind, DetectedSource};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Focus {
    List,
    Details,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SelKind {
    Saved,
    Detected,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Selection {
    pub kind: SelKind,
    pub index: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToastKind {
    Info,
    Success,
    Error,
}

#[derive(Debug, Clone)]
pub struct Toast {
    pub message: String,
    pub kind: ToastKind,
    pub at: Instant,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FormMode {
    Fields,
    Uri,
}

#[derive(Debug, Clone)]
pub struct ConnForm {
    pub editing_id: Option<String>,
    pub mode: FormMode,
    pub kind: DbKind,
    pub name: String,
    pub host: String,
    pub port: String,
    pub user: String,
    pub password: String,
    pub database: String,
    pub uri: String,
    pub container_id: Option<String>,
    pub container_name: Option<String>,
    pub field: usize,
    pub show_password: bool,
    pub error: Option<String>,
}

impl ConnForm {
    pub fn new_blank() -> Self {
        Self {
            editing_id: None,
            mode: FormMode::Fields,
            kind: DbKind::Postgres,
            name: String::new(),
            host: "127.0.0.1".into(),
            port: DbKind::Postgres.default_port().to_string(),
            user: String::new(),
            password: String::new(),
            database: String::new(),
            uri: String::new(),
            container_id: None,
            container_name: None,
            field: 0,
            show_password: false,
            error: None,
        }
    }

    pub fn from_detected(src: &DetectedSource) -> Self {
        let mut f = Self::new_blank();
        f.kind = src.kind;
        f.name = src.display_name();
        f.host = "127.0.0.1".into();
        f.port = src.host_port.to_string();
        f.user = src.user.clone().unwrap_or_default();
        f.password = src.password.clone().unwrap_or_default();
        f.database = src.database.clone().unwrap_or_default();
        f.container_id = Some(src.container_id.clone());
        f.container_name = Some(src.container_name.clone());
        f
    }

    pub fn from_existing(c: &Connection) -> Self {
        let mut f = Self::new_blank();
        f.editing_id = Some(c.id.clone());
        f.kind = c.kind;
        f.name = c.name.clone();
        f.host = c.host.clone();
        f.port = c.port.to_string();
        f.user = c.user.clone().unwrap_or_default();
        f.password = c.password.clone().unwrap_or_default();
        f.database = c.database.clone().unwrap_or_default();
        f.uri = c.uri.clone().unwrap_or_default();
        f.container_id = c.container_id.clone();
        f.container_name = c.container_name.clone();
        f.mode = if f.uri.is_empty() {
            FormMode::Fields
        } else {
            FormMode::Uri
        };
        f
    }

    pub fn field_count(&self) -> usize {
        // 0: name, 1: kind
        // Uri:    2 = uri
        // Fields: 2..=6 = host, port, user, password, database
        match self.mode {
            FormMode::Uri => 3,
            FormMode::Fields => {
                if matches!(self.kind, DbKind::Sqlite) {
                    // name, kind, path (database field reused as file path)
                    3
                } else {
                    7
                }
            }
        }
    }

    pub fn field_label(&self, idx: usize) -> &'static str {
        match (self.mode, self.kind, idx) {
            (_, _, 0) => "name",
            (_, _, 1) => "kind",
            (FormMode::Uri, _, 2) => "uri",
            (FormMode::Fields, DbKind::Sqlite, 2) => "file",
            (FormMode::Fields, _, 2) => "host",
            (FormMode::Fields, _, 3) => "port",
            (FormMode::Fields, _, 4) => "user",
            (FormMode::Fields, _, 5) => "password",
            (FormMode::Fields, _, 6) => "database",
            _ => "?",
        }
    }

    /// The textual value of the current focused field — used for inline editing.
    pub fn field_text(&self, idx: usize) -> &str {
        match (self.mode, self.kind, idx) {
            (_, _, 0) => &self.name,
            (FormMode::Uri, _, 2) => &self.uri,
            (FormMode::Fields, DbKind::Sqlite, 2) => &self.database,
            (FormMode::Fields, _, 2) => &self.host,
            (FormMode::Fields, _, 3) => &self.port,
            (FormMode::Fields, _, 4) => &self.user,
            (FormMode::Fields, _, 5) => &self.password,
            (FormMode::Fields, _, 6) => &self.database,
            _ => "",
        }
    }

    pub fn field_text_mut(&mut self, idx: usize) -> Option<&mut String> {
        match (self.mode, self.kind, idx) {
            (_, _, 0) => Some(&mut self.name),
            (FormMode::Uri, _, 2) => Some(&mut self.uri),
            (FormMode::Fields, DbKind::Sqlite, 2) => Some(&mut self.database),
            (FormMode::Fields, _, 2) => Some(&mut self.host),
            (FormMode::Fields, _, 3) => Some(&mut self.port),
            (FormMode::Fields, _, 4) => Some(&mut self.user),
            (FormMode::Fields, _, 5) => Some(&mut self.password),
            (FormMode::Fields, _, 6) => Some(&mut self.database),
            _ => None,
        }
    }

    pub fn cycle_kind(&mut self, forward: bool) {
        let all = DbKind::ALL;
        let i = all.iter().position(|k| *k == self.kind).unwrap_or(0);
        let next = if forward {
            (i + 1) % all.len()
        } else {
            (i + all.len() - 1) % all.len()
        };
        self.kind = all[next];
        let default_port = self.kind.default_port();
        if default_port > 0 {
            self.port = default_port.to_string();
        }
        // clamp field index after layout change
        if self.field >= self.field_count() {
            self.field = self.field_count().saturating_sub(1);
        }
    }

    pub fn toggle_mode(&mut self) {
        self.mode = match self.mode {
            FormMode::Fields => FormMode::Uri,
            FormMode::Uri => FormMode::Fields,
        };
        self.field = 0;
    }

    pub fn validate(&self) -> Result<Connection, String> {
        if self.name.trim().is_empty() {
            return Err("name is required".into());
        }
        let port = self.port.trim().parse::<u16>().unwrap_or_else(|_| self.kind.default_port());
        let (host, user, password, database, uri) = match self.mode {
            FormMode::Uri => {
                if self.uri.trim().is_empty() {
                    return Err("uri is required in URI mode".into());
                }
                (String::new(), None, None, None, Some(self.uri.trim().to_string()))
            }
            FormMode::Fields => {
                if matches!(self.kind, DbKind::Sqlite) {
                    if self.database.trim().is_empty() {
                        return Err("file path is required for SQLite".into());
                    }
                    (
                        String::new(),
                        None,
                        None,
                        Some(self.database.trim().to_string()),
                        None,
                    )
                } else {
                    (
                        if self.host.trim().is_empty() {
                            "127.0.0.1".to_string()
                        } else {
                            self.host.trim().to_string()
                        },
                        opt(&self.user),
                        opt(&self.password),
                        opt(&self.database),
                        None,
                    )
                }
            }
        };
        let id = self
            .editing_id
            .clone()
            .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
        Ok(Connection {
            id,
            name: self.name.trim().to_string(),
            kind: self.kind,
            host,
            port,
            user,
            password,
            database,
            uri,
            container_id: self.container_id.clone(),
            container_name: self.container_name.clone(),
            auto_backup: None,
            last_backup_at: None,
        })
    }
}

fn opt(s: &str) -> Option<String> {
    let t = s.trim();
    if t.is_empty() {
        None
    } else {
        Some(t.to_string())
    }
}

#[derive(Debug, Clone)]
pub struct AutoForm {
    pub conn_id: String,
    pub enabled: bool,
    pub interval_idx: usize,
    pub retention: String,
    pub field: usize, // 0: enabled, 1: interval, 2: retention
    pub error: Option<String>,
}

pub const AUTO_INTERVALS_SECS: &[(u64, &str)] = &[
    (15 * 60, "15 minutes"),
    (30 * 60, "30 minutes"),
    (60 * 60, "1 hour"),
    (6 * 3600, "6 hours"),
    (12 * 3600, "12 hours"),
    (24 * 3600, "24 hours"),
    (7 * 24 * 3600, "weekly"),
];

impl AutoForm {
    pub fn for_connection(c: &Connection) -> Self {
        let ab = c.auto_backup.clone().unwrap_or_default();
        let interval_idx = AUTO_INTERVALS_SECS
            .iter()
            .position(|(s, _)| *s == ab.interval_secs)
            .unwrap_or(2);
        Self {
            conn_id: c.id.clone(),
            enabled: ab.enabled,
            interval_idx,
            retention: ab.retention.to_string(),
            field: 0,
            error: None,
        }
    }

    pub fn interval_secs(&self) -> u64 {
        AUTO_INTERVALS_SECS[self.interval_idx].0
    }

    pub fn interval_label(&self) -> &'static str {
        AUTO_INTERVALS_SECS[self.interval_idx].1
    }

    pub fn build(&self) -> Result<AutoBackup, String> {
        let retention = self
            .retention
            .trim()
            .parse::<usize>()
            .map_err(|_| "retention must be a number".to_string())?;
        if retention == 0 {
            return Err("retention must be ≥ 1".into());
        }
        Ok(AutoBackup {
            enabled: self.enabled,
            interval_secs: self.interval_secs(),
            retention,
        })
    }
}

#[derive(Debug, Clone)]
pub enum ConfirmKind {
    Delete { conn_id: String, name: String },
    Dump { conn_id: String, name: String },
}

#[derive(Debug, Clone)]
pub enum Dialog {
    Form(ConnForm),
    Auto(AutoForm),
    Confirm(ConfirmKind),
    Progress { label: String },
    Help,
}

#[derive(Debug, Clone)]
pub struct RunningDump {
    pub conn_id: String,
    pub name: String,
    pub started: Instant,
}

pub struct App {
    pub quit: bool,
    pub config: Arc<Mutex<Config>>,
    pub backup_root: PathBuf,
    pub detected: Vec<DetectedSource>,
    pub focus: Focus,
    pub sel: Selection,
    pub dialog: Option<Dialog>,
    pub toast: Option<Toast>,
    pub scanning_detect: bool,
    pub running: Option<RunningDump>,
    pub last_detect_at: Option<Instant>,
    /// Cached snapshot of `config.connections` for synchronous UI access. Refreshed on every loop tick.
    pub conn_cache: Vec<Connection>,
}

impl App {
    pub fn new(config: Arc<Mutex<Config>>, backup_root: PathBuf) -> Self {
        Self {
            quit: false,
            config,
            backup_root,
            detected: Vec::new(),
            focus: Focus::List,
            sel: Selection {
                kind: SelKind::Saved,
                index: 0,
            },
            dialog: None,
            toast: None,
            scanning_detect: true,
            running: None,
            last_detect_at: None,
            conn_cache: Vec::new(),
        }
    }

    pub fn toast(&mut self, msg: impl Into<String>, kind: ToastKind) {
        self.toast = Some(Toast {
            message: msg.into(),
            kind,
            at: Instant::now(),
        });
    }

    pub fn clear_expired_toast(&mut self, ttl: std::time::Duration) {
        if let Some(t) = &self.toast {
            if t.at.elapsed() > ttl {
                self.toast = None;
            }
        }
    }

    pub fn current_saved(&self) -> Option<&Connection> {
        if self.sel.kind != SelKind::Saved {
            return None;
        }
        self.conn_cache.get(self.sel.index)
    }

    pub fn current_detected(&self) -> Option<&DetectedSource> {
        if self.sel.kind != SelKind::Detected {
            return None;
        }
        self.detected.get(self.sel.index)
    }

    /// Move selection by `delta`, wrapping across the saved/detected boundary.
    pub fn move_selection(&mut self, delta: isize) {
        let saved_n = self.conn_cache.len();
        let detected_n = self.detected.len();
        let total = saved_n + detected_n;
        if total == 0 {
            return;
        }
        let cur = match self.sel.kind {
            SelKind::Saved => self.sel.index,
            SelKind::Detected => saved_n + self.sel.index,
        };
        let next = ((cur as isize + delta).rem_euclid(total as isize)) as usize;
        if next < saved_n {
            self.sel = Selection {
                kind: SelKind::Saved,
                index: next,
            };
        } else {
            self.sel = Selection {
                kind: SelKind::Detected,
                index: next - saved_n,
            };
        }
    }

    /// Snap selection to a valid position (after data refresh).
    pub fn clamp_selection(&mut self) {
        let saved_n = self.conn_cache.len();
        let detected_n = self.detected.len();
        if saved_n == 0 && detected_n == 0 {
            self.sel = Selection {
                kind: SelKind::Saved,
                index: 0,
            };
            return;
        }
        match self.sel.kind {
            SelKind::Saved => {
                if self.sel.index >= saved_n {
                    if detected_n > 0 {
                        self.sel = Selection {
                            kind: SelKind::Detected,
                            index: 0,
                        };
                    } else if saved_n > 0 {
                        self.sel.index = saved_n - 1;
                    }
                }
            }
            SelKind::Detected => {
                if self.sel.index >= detected_n {
                    if saved_n > 0 {
                        self.sel = Selection {
                            kind: SelKind::Saved,
                            index: saved_n.saturating_sub(1),
                        };
                    } else {
                        self.sel.index = detected_n.saturating_sub(1);
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{DbKind, DetectedSource};

    fn pg_conn(id: &str, name: &str) -> Connection {
        Connection {
            id: id.into(),
            name: name.into(),
            kind: DbKind::Postgres,
            host: "127.0.0.1".into(),
            port: 5432,
            ..Default::default()
        }
    }

    fn detected_pg(id: &str) -> DetectedSource {
        DetectedSource {
            kind: DbKind::Postgres,
            container_id: id.into(),
            container_name: id.into(),
            image: "postgres:17".into(),
            host_port: 5432,
            user: Some("postgres".into()),
            password: Some("x".into()),
            database: Some("db".into()),
        }
    }

    fn app_with(saved: Vec<Connection>, detected: Vec<DetectedSource>) -> App {
        let cfg = Arc::new(Mutex::new(Config::default()));
        let mut app = App::new(cfg, PathBuf::from("/tmp/siphon"));
        app.conn_cache = saved;
        app.detected = detected;
        app
    }

    #[test]
    fn move_wraps_across_saved_and_detected() {
        let mut app = app_with(
            vec![pg_conn("1", "a"), pg_conn("2", "b")],
            vec![detected_pg("c1"), detected_pg("c2")],
        );
        app.move_selection(1);
        assert_eq!(app.sel, Selection { kind: SelKind::Saved, index: 1 });
        app.move_selection(1);
        assert_eq!(app.sel, Selection { kind: SelKind::Detected, index: 0 });
        app.move_selection(1);
        assert_eq!(app.sel, Selection { kind: SelKind::Detected, index: 1 });
        app.move_selection(1);
        assert_eq!(app.sel, Selection { kind: SelKind::Saved, index: 0 });
        app.move_selection(-1);
        assert_eq!(app.sel, Selection { kind: SelKind::Detected, index: 1 });
    }

    #[test]
    fn form_validate_requires_name() {
        let form = ConnForm::new_blank();
        assert!(form.validate().is_err());
    }

    #[test]
    fn form_validate_produces_connection() {
        let mut form = ConnForm::new_blank();
        form.name = "Local".into();
        form.user = "postgres".into();
        let conn = form.validate().unwrap();
        assert_eq!(conn.name, "Local");
        assert_eq!(conn.host, "127.0.0.1");
        assert_eq!(conn.port, 5432);
        assert_eq!(conn.user.as_deref(), Some("postgres"));
    }

    #[test]
    fn form_uri_mode_requires_uri() {
        let mut form = ConnForm::new_blank();
        form.name = "x".into();
        form.toggle_mode();
        assert!(form.validate().is_err());
        form.uri = "postgres://x/y".into();
        let c = form.validate().unwrap();
        assert_eq!(c.uri.as_deref(), Some("postgres://x/y"));
    }

    #[test]
    fn form_sqlite_uses_database_as_file() {
        let mut form = ConnForm::new_blank();
        form.name = "lite".into();
        form.kind = DbKind::Sqlite;
        form.database = "/tmp/x.sqlite".into();
        let c = form.validate().unwrap();
        assert_eq!(c.kind, DbKind::Sqlite);
        assert_eq!(c.database.as_deref(), Some("/tmp/x.sqlite"));
    }

    #[test]
    fn form_cycle_kind_updates_port() {
        let mut form = ConnForm::new_blank();
        assert_eq!(form.port, "5432");
        form.cycle_kind(true);
        assert_eq!(form.kind, DbKind::Mongo);
        assert_eq!(form.port, "27017");
    }

    #[test]
    fn auto_form_validates_retention() {
        let conn = pg_conn("1", "a");
        let mut f = AutoForm::for_connection(&conn);
        f.retention = "0".into();
        assert!(f.build().is_err());
        f.retention = "abc".into();
        assert!(f.build().is_err());
        f.retention = "5".into();
        assert_eq!(f.build().unwrap().retention, 5);
    }

    #[test]
    fn form_from_detected_carries_creds() {
        let d = detected_pg("dev");
        let f = ConnForm::from_detected(&d);
        assert_eq!(f.user, "postgres");
        assert_eq!(f.password, "x");
        assert_eq!(f.container_id.as_deref(), Some("dev"));
    }
}
