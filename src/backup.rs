use std::fs;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use anyhow::{Context, Result};
use chrono::Utc;
use tokio::io::AsyncReadExt;
use tokio::process::Command;

use crate::quirks;
use crate::types::{BackupFile, Connection, DbKind};

#[derive(Debug, Clone)]
pub struct BackupOutcome {
    pub path: PathBuf,
    pub bytes: u64,
    pub duration: Duration,
    pub runtime_used: String,
}

/// Where this connection's dumps live on disk.
pub fn dir_for(root: &Path, conn: &Connection) -> PathBuf {
    let slug = slugify(&conn.name);
    let id_short = conn.id.chars().take(8).collect::<String>();
    root.join(format!("{}-{}", slug, id_short))
}

pub fn list(root: &Path, conn: &Connection) -> Vec<BackupFile> {
    let dir = dir_for(root, conn);
    let mut out = Vec::new();
    let Ok(rd) = fs::read_dir(&dir) else { return out };
    for entry in rd.flatten() {
        let path = entry.path();
        if !path.is_file() {
            continue;
        }
        let Ok(meta) = entry.metadata() else { continue };
        let created = meta
            .modified()
            .ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0);
        out.push(BackupFile {
            path,
            bytes: meta.len(),
            created_at: created,
        });
    }
    out.sort_by(|a, b| b.created_at.cmp(&a.created_at));
    out
}

pub fn prune(root: &Path, conn: &Connection, keep: usize) -> Result<usize> {
    let mut files = list(root, conn);
    if files.len() <= keep {
        return Ok(0);
    }
    let to_remove = files.split_off(keep);
    let mut removed = 0;
    for f in to_remove {
        if fs::remove_file(&f.path).is_ok() {
            removed += 1;
        }
    }
    Ok(removed)
}

pub async fn dump(root: &Path, conn: &Connection) -> Result<BackupOutcome> {
    let start = std::time::Instant::now();
    let dir = dir_for(root, conn);
    fs::create_dir_all(&dir).with_context(|| format!("create {}", dir.display()))?;

    let runtime = pick_runtime(conn).context("no runtime available")?;
    let stamp = Utc::now().format("%Y%m%dT%H%M%SZ").to_string();
    let ext = file_extension(conn.kind, &runtime);
    let filename = format!("{}-{}{}", slugify(&conn.name), stamp, ext);
    let path = dir.join(&filename);

    let result = match conn.kind {
        DbKind::Postgres => dump_postgres(conn, &runtime, &path).await,
        DbKind::Mongo => dump_mongo(conn, &runtime, &path).await,
        DbKind::Mysql => dump_mysql(conn, &runtime, &path).await,
        DbKind::Sqlite => dump_sqlite(conn, &path).await,
        DbKind::Redis => dump_redis(conn, &runtime, &path).await,
    };
    if let Err(e) = result {
        let _ = fs::remove_file(&path);
        return Err(e);
    }

    let bytes = fs::metadata(&path)
        .map(|m| m.len())
        .with_context(|| format!("stat {}", path.display()))?;
    if bytes == 0 {
        let _ = fs::remove_file(&path);
        anyhow::bail!("dump produced 0 bytes");
    }
    Ok(BackupOutcome {
        path,
        bytes,
        duration: start.elapsed(),
        runtime_used: runtime.label(),
    })
}

pub async fn test_connection(conn: &Connection) -> Result<String> {
    let runtime = pick_runtime(conn).context("no runtime available")?;
    match conn.kind {
        DbKind::Postgres => test_postgres(conn, &runtime).await,
        DbKind::Mongo => test_mongo(conn, &runtime).await,
        DbKind::Mysql => test_mysql(conn, &runtime).await,
        DbKind::Sqlite => test_sqlite(conn),
        DbKind::Redis => test_redis(conn, &runtime).await,
    }
}

// ────────────────────────────────────────────────────────────────────────────
// Runtime selection
// ────────────────────────────────────────────────────────────────────────────

/// How we'll execute the dump tool: inside an existing container, against the
/// host, or via an ephemeral container that has the tool baked in.
#[derive(Debug, Clone)]
pub enum Runtime {
    DockerExec { container_id: String },
    Local,
    DockerRun { image: String },
}

impl Runtime {
    pub fn label(&self) -> String {
        match self {
            Runtime::DockerExec { container_id } => {
                format!("docker exec {}", &container_id[..container_id.len().min(12)])
            }
            Runtime::Local => "local".into(),
            Runtime::DockerRun { image } => format!("docker run {image}"),
        }
    }

    pub fn is_containerised(&self) -> bool {
        matches!(self, Runtime::DockerExec { .. } | Runtime::DockerRun { .. })
    }
}

fn image_for(kind: DbKind) -> &'static str {
    match kind {
        DbKind::Postgres => "postgres:17",
        DbKind::Mongo => "mongo:6",
        DbKind::Mysql => "mysql:8",
        DbKind::Redis => "redis:7",
        DbKind::Sqlite => "alpine:3", // unused — sqlite never goes through docker
    }
}

fn primary_tool(kind: DbKind) -> &'static str {
    match kind {
        DbKind::Postgres => "pg_dump",
        DbKind::Mongo => "mongodump",
        DbKind::Mysql => "mysqldump",
        DbKind::Redis => "redis-cli",
        DbKind::Sqlite => "sqlite3",
    }
}

pub fn pick_runtime(conn: &Connection) -> Result<Runtime> {
    if matches!(conn.kind, DbKind::Sqlite) {
        // SQLite is always local — sqlite3 ships with macOS and most distros.
        return if which::which("sqlite3").is_ok() {
            Ok(Runtime::Local)
        } else {
            anyhow::bail!("sqlite3 not on PATH — install with `brew install sqlite`")
        };
    }
    if let Some(cid) = conn.container_id.as_deref().filter(|s| !s.is_empty()) {
        return Ok(Runtime::DockerExec {
            container_id: cid.to_string(),
        });
    }
    let tool = primary_tool(conn.kind);
    if which::which(tool).is_ok() {
        return Ok(Runtime::Local);
    }
    if which::which("docker").is_ok() {
        return Ok(Runtime::DockerRun {
            image: image_for(conn.kind).to_string(),
        });
    }
    anyhow::bail!(
        "neither {tool} nor docker found on PATH — install one of:\n  • docker (preferred; siphon will fetch the tool image on demand)\n  • {tool} (e.g. `brew install {}`)",
        brew_pkg(conn.kind)
    )
}

fn brew_pkg(kind: DbKind) -> &'static str {
    match kind {
        DbKind::Postgres => "libpq",
        DbKind::Mongo => "mongodb-database-tools",
        DbKind::Mysql => "mysql-client",
        DbKind::Redis => "redis",
        DbKind::Sqlite => "sqlite",
    }
}

fn file_extension(kind: DbKind, runtime: &Runtime) -> &'static str {
    match kind {
        DbKind::Postgres => ".dump",
        DbKind::Mongo => ".archive.gz",
        DbKind::Mysql => {
            if runtime.is_containerised() {
                ".sql.gz"
            } else {
                ".sql"
            }
        }
        DbKind::Sqlite => ".sqlite",
        DbKind::Redis => ".rdb",
    }
}

fn slugify(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        if c.is_ascii_alphanumeric() {
            out.push(c.to_ascii_lowercase());
        } else if matches!(c, ' ' | '-' | '_') {
            out.push('-');
        }
    }
    let trimmed = out.trim_matches('-').to_string();
    if trimmed.is_empty() {
        "db".to_string()
    } else {
        trimmed
    }
}

// ────────────────────────────────────────────────────────────────────────────
// Connection-shape helpers
// ────────────────────────────────────────────────────────────────────────────

fn host_for(conn: &Connection, runtime: &Runtime) -> String {
    let raw = if conn.host.is_empty() {
        "127.0.0.1".to_string()
    } else {
        conn.host.clone()
    };
    match runtime {
        Runtime::DockerExec { .. } => {
            // We're inside the container — DB is on its own localhost.
            "127.0.0.1".into()
        }
        Runtime::DockerRun { .. } => {
            // Ephemeral container — rewrite host-side loopback to docker's bridge.
            if matches!(raw.as_str(), "127.0.0.1" | "localhost" | "::1") {
                "host.docker.internal".into()
            } else {
                raw
            }
        }
        Runtime::Local => raw,
    }
}

fn port_string(conn: &Connection) -> String {
    let port = if conn.port == 0 {
        conn.kind.default_port()
    } else {
        conn.port
    };
    port.to_string()
}

/// Build a postgres URI for the connection, applying quirks.
fn postgres_uri(conn: &Connection, runtime: &Runtime) -> String {
    let host = host_for(conn, runtime);
    let port = port_string(conn);
    let user = conn.user.clone().unwrap_or_else(|| "postgres".into());
    let pw = conn.password.clone().unwrap_or_default();
    let db = conn
        .database
        .clone()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "postgres".to_string());
    let auth = if pw.is_empty() {
        urlencode(&user)
    } else {
        format!("{}:{}", urlencode(&user), urlencode(&pw))
    };
    let raw = if let Some(uri) = conn.uri.as_deref().filter(|s| !s.is_empty()) {
        uri.to_string()
    } else {
        format!("postgresql://{auth}@{host}:{port}/{db}")
    };
    quirks::apply_to_uri(&raw, DbKind::Postgres)
}

fn mongo_uri(conn: &Connection, runtime: &Runtime) -> String {
    let raw = if let Some(u) = conn.uri.as_deref().filter(|s| !s.is_empty()) {
        u.to_string()
    } else {
        let host = host_for(conn, runtime);
        let port = if conn.port == 0 { 27017 } else { conn.port };
        let userpass = match (conn.user.as_deref(), conn.password.as_deref()) {
            (Some(u), Some(p)) if !u.is_empty() => format!("{}:{}@", urlencode(u), urlencode(p)),
            (Some(u), None) if !u.is_empty() => format!("{}@", urlencode(u)),
            _ => String::new(),
        };
        let auth = if userpass.is_empty() {
            String::new()
        } else {
            "?authSource=admin".to_string()
        };
        let db = conn
            .database
            .as_deref()
            .filter(|s| !s.is_empty())
            .unwrap_or("");
        format!("mongodb://{userpass}{host}:{port}/{db}{auth}")
    };
    quirks::apply_to_uri(&raw, DbKind::Mongo)
}

// ────────────────────────────────────────────────────────────────────────────
// Postgres
// ────────────────────────────────────────────────────────────────────────────

async fn dump_postgres(conn: &Connection, runtime: &Runtime, out: &Path) -> Result<()> {
    let uri = postgres_uri(conn, runtime);
    let args = vec!["-F".to_string(), "c".to_string(), "--dbname".into(), uri];
    spawn_to_file("pg_dump", &args, runtime, out, &[]).await
}

async fn test_postgres(conn: &Connection, runtime: &Runtime) -> Result<String> {
    let uri = postgres_uri(conn, runtime);
    let args = vec![
        "-tAc".to_string(),
        "select version();".into(),
        "--dbname".into(),
        uri,
    ];
    let out = run_capture("psql", &args, runtime, &[]).await?;
    Ok(out.lines().next().unwrap_or("ok").trim().to_string())
}

// ────────────────────────────────────────────────────────────────────────────
// MongoDB
// ────────────────────────────────────────────────────────────────────────────

async fn dump_mongo(conn: &Connection, runtime: &Runtime, out: &Path) -> Result<()> {
    let uri = mongo_uri(conn, runtime);
    let mut args = vec![
        "--archive".to_string(),
        "--gzip".into(),
        format!("--uri={}", uri),
    ];
    if let Some(db) = conn.database.as_deref().filter(|s| !s.is_empty()) {
        args.push(format!("--db={}", db));
    }
    spawn_to_file("mongodump", &args, runtime, out, &[]).await
}

async fn test_mongo(conn: &Connection, runtime: &Runtime) -> Result<String> {
    let uri = mongo_uri(conn, runtime);
    // Prefer mongosh, fall back to mongo (deprecated but still in mongo:6).
    let probe_args = vec![
        uri.clone(),
        "--quiet".into(),
        "--eval".into(),
        "db.runCommand({ping:1}).ok".into(),
    ];
    match run_capture("mongosh", &probe_args, runtime, &[]).await {
        Ok(s) => Ok(s.trim().to_string()),
        Err(_) => run_capture("mongo", &probe_args, runtime, &[])
            .await
            .map(|s| s.trim().to_string()),
    }
}

// ────────────────────────────────────────────────────────────────────────────
// MySQL / MariaDB
// ────────────────────────────────────────────────────────────────────────────

async fn dump_mysql(conn: &Connection, runtime: &Runtime, out: &Path) -> Result<()> {
    let host = host_for(conn, runtime);
    let port = port_string(conn);
    let user = conn.user.clone().unwrap_or_else(|| "root".into());
    let db_arg = match conn.database.as_deref().filter(|s| !s.is_empty()) {
        Some(d) => shell_escape(d),
        None => "--all-databases".to_string(),
    };
    let password = conn.password.clone().unwrap_or_default();

    if runtime.is_containerised() {
        // Shell pipe so the dump comes out gzipped from the container.
        let inner = format!(
            "mysqldump -h{} -P{} -u{} {} | gzip",
            shell_escape(&host),
            port,
            shell_escape(&user),
            db_arg,
        );
        let env = vec![("MYSQL_PWD".to_string(), password)];
        spawn_to_file_shell(&inner, runtime, out, &env).await
    } else {
        let mut args = vec![
            format!("-h{host}"),
            format!("-P{port}"),
            format!("-u{user}"),
        ];
        match conn.database.as_deref().filter(|s| !s.is_empty()) {
            Some(d) => args.push(d.to_string()),
            None => args.push("--all-databases".into()),
        }
        let env = vec![("MYSQL_PWD".to_string(), password)];
        spawn_to_file("mysqldump", &args, runtime, out, &env).await
    }
}

async fn test_mysql(conn: &Connection, runtime: &Runtime) -> Result<String> {
    let host = host_for(conn, runtime);
    let user = conn.user.clone().unwrap_or_else(|| "root".into());
    let password = conn.password.clone().unwrap_or_default();
    let args = vec![
        format!("-h{host}"),
        format!("-P{}", port_string(conn)),
        format!("-u{user}"),
        "-e".into(),
        "select version();".into(),
    ];
    let env = vec![("MYSQL_PWD".to_string(), password)];
    let out = run_capture("mysql", &args, runtime, &env).await?;
    Ok(out.lines().last().unwrap_or("ok").trim().to_string())
}

// ────────────────────────────────────────────────────────────────────────────
// SQLite (always local — bypasses Runtime)
// ────────────────────────────────────────────────────────────────────────────

async fn dump_sqlite(conn: &Connection, out: &Path) -> Result<()> {
    let src = sqlite_path(conn)?;
    let exe = which::which("sqlite3").map_err(|_| anyhow::anyhow!("sqlite3 not on PATH"))?;
    let status = Command::new(&exe)
        .arg(&src)
        .arg(format!(".backup '{}'", out.display()))
        .status()
        .await
        .context("spawn sqlite3")?;
    if !status.success() {
        anyhow::bail!("sqlite3 backup failed");
    }
    Ok(())
}

fn test_sqlite(conn: &Connection) -> Result<String> {
    let path = sqlite_path(conn)?;
    let meta = fs::metadata(&path).with_context(|| format!("stat {}", path.display()))?;
    Ok(format!("{} bytes", meta.len()))
}

fn sqlite_path(conn: &Connection) -> Result<PathBuf> {
    let src = conn
        .database
        .as_deref()
        .or(conn.uri.as_deref())
        .ok_or_else(|| anyhow::anyhow!("sqlite needs a database file path"))?;
    let src_path = src.strip_prefix("sqlite://").unwrap_or(src);
    let expanded = shellexpand_tilde(src_path);
    let p = PathBuf::from(expanded.as_ref());
    if !p.exists() {
        anyhow::bail!("sqlite file not found: {}", p.display());
    }
    Ok(p)
}

// ────────────────────────────────────────────────────────────────────────────
// Redis
// ────────────────────────────────────────────────────────────────────────────

async fn dump_redis(conn: &Connection, runtime: &Runtime, out: &Path) -> Result<()> {
    let mut args: Vec<String> = vec![
        "-h".into(),
        host_for(conn, runtime),
        "-p".into(),
        port_string(conn),
    ];
    if let Some(pw) = conn.password.as_deref().filter(|s| !s.is_empty()) {
        args.push("-a".into());
        args.push(pw.to_string());
    }
    args.push("--rdb".into());
    args.push("-".into());
    spawn_to_file("redis-cli", &args, runtime, out, &[]).await
}

async fn test_redis(conn: &Connection, runtime: &Runtime) -> Result<String> {
    let mut args: Vec<String> = vec![
        "-h".into(),
        host_for(conn, runtime),
        "-p".into(),
        port_string(conn),
    ];
    if let Some(pw) = conn.password.as_deref().filter(|s| !s.is_empty()) {
        args.push("-a".into());
        args.push(pw.to_string());
    }
    args.push("PING".into());
    let out = run_capture("redis-cli", &args, runtime, &[]).await?;
    Ok(out.trim().to_string())
}

// ────────────────────────────────────────────────────────────────────────────
// helpers
// ────────────────────────────────────────────────────────────────────────────

fn urlencode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{:02X}", b)),
        }
    }
    out
}

fn shell_escape(s: &str) -> String {
    if s.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.' | '/')) {
        s.to_string()
    } else {
        let escaped = s.replace('\'', "'\\''");
        format!("'{}'", escaped)
    }
}

fn shellexpand_tilde(p: &str) -> std::borrow::Cow<'_, str> {
    if let Some(rest) = p.strip_prefix("~/") {
        if let Some(home) = dirs::home_dir() {
            return std::borrow::Cow::Owned(home.join(rest).to_string_lossy().to_string());
        }
    } else if p == "~" {
        if let Some(home) = dirs::home_dir() {
            return std::borrow::Cow::Owned(home.to_string_lossy().to_string());
        }
    }
    std::borrow::Cow::Borrowed(p)
}

/// Render the runtime-specific invocation for a tool with args & env.
fn shape(
    tool: &str,
    args: &[String],
    runtime: &Runtime,
    env: &[(String, String)],
) -> (String, Vec<String>) {
    match runtime {
        Runtime::Local => (tool.to_string(), args.to_vec()),
        Runtime::DockerExec { container_id } => {
            let mut a: Vec<String> = vec!["exec".into(), "-i".into()];
            for (k, v) in env {
                a.push("-e".into());
                a.push(format!("{k}={v}"));
            }
            a.push(container_id.clone());
            a.push(tool.to_string());
            a.extend(args.iter().cloned());
            ("docker".to_string(), a)
        }
        Runtime::DockerRun { image } => {
            let mut a: Vec<String> = vec![
                "run".into(),
                "--rm".into(),
                "-i".into(),
                "--add-host=host.docker.internal:host-gateway".into(),
            ];
            for (k, v) in env {
                a.push("-e".into());
                a.push(format!("{k}={v}"));
            }
            a.push(image.clone());
            a.push(tool.to_string());
            a.extend(args.iter().cloned());
            ("docker".to_string(), a)
        }
    }
}

/// Like `shape`, but the shell snippet is the only argument (used to pipe
/// through gzip inside the container).
fn shape_shell(
    shell_cmd: &str,
    runtime: &Runtime,
    env: &[(String, String)],
) -> (String, Vec<String>) {
    match runtime {
        Runtime::Local => (
            "sh".to_string(),
            vec!["-c".into(), shell_cmd.to_string()],
        ),
        Runtime::DockerExec { container_id } => {
            let mut a: Vec<String> = vec!["exec".into(), "-i".into()];
            for (k, v) in env {
                a.push("-e".into());
                a.push(format!("{k}={v}"));
            }
            a.push(container_id.clone());
            a.push("sh".into());
            a.push("-c".into());
            a.push(shell_cmd.to_string());
            ("docker".to_string(), a)
        }
        Runtime::DockerRun { image } => {
            let mut a: Vec<String> = vec![
                "run".into(),
                "--rm".into(),
                "-i".into(),
                "--add-host=host.docker.internal:host-gateway".into(),
            ];
            for (k, v) in env {
                a.push("-e".into());
                a.push(format!("{k}={v}"));
            }
            a.push(image.clone());
            a.push("sh".into());
            a.push("-c".into());
            a.push(shell_cmd.to_string());
            ("docker".to_string(), a)
        }
    }
}

async fn spawn_to_file(
    tool: &str,
    args: &[String],
    runtime: &Runtime,
    out: &Path,
    env: &[(String, String)],
) -> Result<()> {
    let (exe, args) = shape(tool, args, runtime, env);
    spawn_and_pipe(&exe, &args, out, runtime, tool, env).await
}

async fn spawn_to_file_shell(
    cmd: &str,
    runtime: &Runtime,
    out: &Path,
    env: &[(String, String)],
) -> Result<()> {
    let (exe, args) = shape_shell(cmd, runtime, env);
    spawn_and_pipe(&exe, &args, out, runtime, "shell", env).await
}

async fn spawn_and_pipe(
    exe: &str,
    args: &[String],
    out: &Path,
    runtime: &Runtime,
    tool_label: &str,
    extra_env: &[(String, String)],
) -> Result<()> {
    let file = std::fs::File::create(out).with_context(|| format!("create {}", out.display()))?;
    let mut cmd = Command::new(exe);
    cmd.args(args);
    cmd.stdin(Stdio::null());
    cmd.stdout(Stdio::from(file));
    cmd.stderr(Stdio::piped());
    // When running locally we have to set the env on the host process;
    // for docker exec/run we already injected `-e KEY=VAL`.
    if matches!(runtime, Runtime::Local) {
        for (k, v) in extra_env {
            cmd.env(k, v);
        }
    }
    let mut child = cmd
        .spawn()
        .with_context(|| format!("spawn {} {:?}", exe, args))?;

    let mut stderr_buf = String::new();
    if let Some(mut stderr) = child.stderr.take() {
        let _ = stderr.read_to_string(&mut stderr_buf).await;
    }
    let status = child.wait().await?;
    if !status.success() {
        let _ = fs::remove_file(out);
        let trimmed = stderr_buf.trim();
        let last = trimmed.lines().last().unwrap_or("").trim();
        let err = if last.is_empty() {
            format!("{tool_label} exited {:?}", status.code())
        } else {
            last.to_string()
        };
        anyhow::bail!("{}", err);
    }
    Ok(())
}

async fn run_capture(
    tool: &str,
    args: &[String],
    runtime: &Runtime,
    env: &[(String, String)],
) -> Result<String> {
    let (exe, args) = shape(tool, args, runtime, env);
    let mut cmd = Command::new(&exe);
    cmd.args(&args);
    if matches!(runtime, Runtime::Local) {
        for (k, v) in env {
            cmd.env(k, v);
        }
    }
    let output = tokio::time::timeout(Duration::from_secs(20), cmd.output())
        .await
        .context("connection test timed out")??;
    if !output.status.success() {
        let err = String::from_utf8_lossy(&output.stderr).trim().to_string();
        let err = if err.is_empty() {
            format!("{} exited {:?}", exe, output.status.code())
        } else {
            err
        };
        anyhow::bail!("{}", err);
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::DbKind;
    use std::time::SystemTime;

    fn pg_remote() -> Connection {
        Connection {
            id: "x".into(),
            name: "Remote".into(),
            kind: DbKind::Postgres,
            host: "db.foo.supabase.co".into(),
            port: 5432,
            user: Some("postgres".into()),
            password: Some("pw".into()),
            database: Some("postgres".into()),
            ..Default::default()
        }
    }

    fn pg_container() -> Connection {
        Connection {
            kind: DbKind::Postgres,
            host: "127.0.0.1".into(),
            port: 5432,
            user: Some("u".into()),
            password: Some("p".into()),
            container_id: Some("abc123".into()),
            ..Default::default()
        }
    }

    #[test]
    fn slugify_keeps_alnum() {
        assert_eq!(slugify("Local Postgres"), "local-postgres");
        assert_eq!(slugify("DB/with*weird:chars"), "dbwithweirdchars");
        assert_eq!(slugify("---"), "db");
    }

    #[test]
    fn pick_runtime_prefers_container_when_attached() {
        let rt = pick_runtime(&pg_container()).unwrap();
        assert!(matches!(rt, Runtime::DockerExec { .. }));
    }

    #[test]
    fn host_rewrite_for_docker_run_uses_internal_dns() {
        let mut c = pg_remote();
        c.host = "127.0.0.1".into();
        let rt = Runtime::DockerRun {
            image: "postgres:17".into(),
        };
        assert_eq!(host_for(&c, &rt), "host.docker.internal");
        let rt2 = Runtime::DockerExec {
            container_id: "x".into(),
        };
        assert_eq!(host_for(&c, &rt2), "127.0.0.1");
        let rt3 = Runtime::Local;
        assert_eq!(host_for(&c, &rt3), "127.0.0.1");
    }

    #[test]
    fn host_passthrough_for_remote() {
        let rt = Runtime::DockerRun {
            image: "postgres:17".into(),
        };
        assert_eq!(host_for(&pg_remote(), &rt), "db.foo.supabase.co");
    }

    #[test]
    fn postgres_uri_applies_supabase_ssl() {
        let rt = Runtime::DockerRun {
            image: "postgres:17".into(),
        };
        let uri = postgres_uri(&pg_remote(), &rt);
        assert!(uri.contains("sslmode=require"), "got: {uri}");
        assert!(uri.contains("supabase.co"));
    }

    #[test]
    fn postgres_uri_no_ssl_for_unknown_host() {
        let mut c = pg_remote();
        c.host = "example.com".into();
        let rt = Runtime::DockerRun {
            image: "postgres:17".into(),
        };
        let uri = postgres_uri(&c, &rt);
        assert!(!uri.contains("sslmode"), "got: {uri}");
    }

    #[test]
    fn mongo_uri_atlas_adds_tls() {
        let c = Connection {
            kind: DbKind::Mongo,
            host: "cluster.abc.mongodb.net".into(),
            port: 27017,
            user: Some("u".into()),
            password: Some("p".into()),
            ..Default::default()
        };
        let rt = Runtime::DockerRun {
            image: "mongo:6".into(),
        };
        let uri = mongo_uri(&c, &rt);
        assert!(uri.contains("tls=true"), "got: {uri}");
    }

    #[test]
    fn explicit_uri_is_respected_then_quirked() {
        let c = Connection {
            kind: DbKind::Postgres,
            uri: Some("postgres://u:p@db.foo.supabase.co:5432/x".into()),
            ..Default::default()
        };
        let rt = Runtime::Local;
        let out = postgres_uri(&c, &rt);
        assert!(out.contains("supabase.co"));
        assert!(out.contains("sslmode=require"));
    }

    #[test]
    fn file_extension_uses_runtime() {
        let local = Runtime::Local;
        assert_eq!(file_extension(DbKind::Mysql, &local), ".sql");
        let docker = Runtime::DockerRun {
            image: "mysql:8".into(),
        };
        assert_eq!(file_extension(DbKind::Mysql, &docker), ".sql.gz");
        assert_eq!(file_extension(DbKind::Postgres, &local), ".dump");
        assert_eq!(file_extension(DbKind::Postgres, &docker), ".dump");
    }

    #[test]
    fn shape_local_passthrough() {
        let rt = Runtime::Local;
        let (exe, args) = shape("pg_dump", &["-Fc".into(), "db".into()], &rt, &[]);
        assert_eq!(exe, "pg_dump");
        assert_eq!(args, vec!["-Fc", "db"]);
    }

    #[test]
    fn shape_docker_exec_prefix() {
        let rt = Runtime::DockerExec {
            container_id: "abc".into(),
        };
        let (exe, args) = shape(
            "pg_dump",
            &["-Fc".into()],
            &rt,
            &[("PGPASSWORD".into(), "x".into())],
        );
        assert_eq!(exe, "docker");
        assert_eq!(args[0], "exec");
        assert!(args.iter().any(|a| a == "-e"));
        assert!(args.iter().any(|a| a == "PGPASSWORD=x"));
        assert!(args.iter().any(|a| a == "abc"));
        assert!(args.iter().any(|a| a == "pg_dump"));
    }

    #[test]
    fn shape_docker_run_includes_add_host() {
        let rt = Runtime::DockerRun {
            image: "postgres:17".into(),
        };
        let (exe, args) = shape("pg_dump", &[], &rt, &[]);
        assert_eq!(exe, "docker");
        assert!(args.iter().any(|a| a.contains("host.docker.internal:host-gateway")));
        assert!(args.iter().any(|a| a == "postgres:17"));
    }

    #[test]
    fn prune_keeps_newest_by_mtime() {
        let tmp = tempfile::TempDir::new().unwrap();
        let conn = Connection {
            id: "abc12345xx".into(),
            name: "Local".into(),
            kind: DbKind::Postgres,
            ..Default::default()
        };
        let dir = dir_for(tmp.path(), &conn);
        fs::create_dir_all(&dir).unwrap();
        for (i, name) in ["a.dump", "b.dump", "c.dump", "d.dump"].iter().enumerate() {
            let p = dir.join(name);
            fs::write(&p, b"data").unwrap();
            let when = SystemTime::UNIX_EPOCH + Duration::from_secs(1_000_000 + i as u64 * 100);
            let f = fs::OpenOptions::new().write(true).open(&p).unwrap();
            f.set_modified(when).unwrap();
        }
        let removed = prune(tmp.path(), &conn, 2).unwrap();
        assert_eq!(removed, 2);
        let remaining = list(tmp.path(), &conn);
        assert_eq!(remaining.len(), 2);
        assert!(remaining[0].path.ends_with("d.dump"));
        assert!(remaining[1].path.ends_with("c.dump"));
    }

    #[test]
    fn shell_escape_quotes_when_needed() {
        assert_eq!(shell_escape("plain"), "plain");
        assert_eq!(shell_escape("ab cd"), "'ab cd'");
        assert_eq!(shell_escape("o'reilly"), "'o'\\''reilly'");
    }

    #[test]
    fn shellexpand_tilde_expands_home() {
        let home = dirs::home_dir().unwrap();
        let out = shellexpand_tilde("~/foo.db");
        assert!(out.starts_with(home.to_string_lossy().as_ref()));
        assert!(out.ends_with("foo.db"));
        assert_eq!(shellexpand_tilde("/abs/x").as_ref(), "/abs/x");
    }

    #[test]
    fn urlencode_handles_specials() {
        assert_eq!(urlencode("p@ss!"), "p%40ss%21");
        assert_eq!(urlencode("simple"), "simple");
        assert_eq!(urlencode("foo bar"), "foo%20bar");
    }
}
