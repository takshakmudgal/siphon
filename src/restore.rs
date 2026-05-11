//! Restore a previously-saved dump back into a database.
//!
//! Mirrors the dispatch shape of `backup.rs`: pick a runtime (docker exec /
//! local / ephemeral docker run), then route to the kind-specific restorer.
//! Restores always read the dump file via the child process's stdin so we
//! don't have to translate host paths into container paths.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use anyhow::{Context, Result};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::process::Command;

use crate::backup::{Runtime, pick_runtime};
use crate::quirks;
use crate::types::{Connection, DbKind};

#[derive(Debug, Clone)]
pub struct RestoreOutcome {
    pub bytes_in: u64,
    pub duration: Duration,
    pub runtime_used: String,
}

pub async fn restore(conn: &Connection, dump: &Path) -> Result<RestoreOutcome> {
    if !dump.exists() {
        anyhow::bail!("dump file not found: {}", dump.display());
    }
    let start = std::time::Instant::now();
    let runtime = pick_runtime(conn).context("no runtime available")?;
    match conn.kind {
        DbKind::Postgres => restore_postgres(conn, &runtime, dump).await?,
        DbKind::Mongo => restore_mongo(conn, &runtime, dump).await?,
        DbKind::Mysql => restore_mysql(conn, &runtime, dump).await?,
        DbKind::Sqlite => restore_sqlite(conn, dump).await?,
        DbKind::Redis => anyhow::bail!(
            "redis restore isn't supported — RDB files have to be placed in the redis data dir and the server restarted. Do that manually then RESTART your container."
        ),
    }
    let bytes_in = std::fs::metadata(dump).map(|m| m.len()).unwrap_or(0);
    Ok(RestoreOutcome {
        bytes_in,
        duration: start.elapsed(),
        runtime_used: runtime.label(),
    })
}

// ────────────────────────────────────────────────────────────────────────────
// Per-kind restorers
// ────────────────────────────────────────────────────────────────────────────

async fn restore_postgres(conn: &Connection, runtime: &Runtime, dump: &Path) -> Result<()> {
    let uri = postgres_uri(conn, runtime);
    let args = vec![
        "--clean".to_string(),
        "--if-exists".into(),
        "--no-owner".into(),
        "--no-privileges".into(),
        "-d".into(),
        uri,
    ];
    spawn_with_stdin(
        "pg_restore",
        &args,
        runtime,
        dump,
        Decompress::None,
        &[],
    )
    .await
}

async fn restore_mongo(conn: &Connection, runtime: &Runtime, dump: &Path) -> Result<()> {
    let uri = mongo_uri(conn, runtime);
    let gzip = dump
        .extension()
        .and_then(|s| s.to_str())
        .map(|s| s.eq_ignore_ascii_case("gz") || dump.to_string_lossy().ends_with(".archive.gz"))
        .unwrap_or(false);
    let mut args = vec![
        "--archive".to_string(),
        "--drop".into(),
        format!("--uri={uri}"),
    ];
    if gzip {
        args.push("--gzip".into());
    }
    spawn_with_stdin(
        "mongorestore",
        &args,
        runtime,
        dump,
        Decompress::None,
        &[],
    )
    .await
}

async fn restore_mysql(conn: &Connection, runtime: &Runtime, dump: &Path) -> Result<()> {
    let host = host_for(conn, runtime);
    let port = port_string(conn);
    let user = conn.user.clone().unwrap_or_else(|| "root".into());
    let password = conn.password.clone().unwrap_or_default();
    let db = conn
        .database
        .clone()
        .filter(|s| !s.is_empty())
        .unwrap_or_default();

    let decompress = if dump
        .extension()
        .and_then(|s| s.to_str())
        .map(|s| s.eq_ignore_ascii_case("gz"))
        .unwrap_or(false)
    {
        Decompress::Gzip
    } else {
        Decompress::None
    };

    let mut args = vec![
        format!("-h{host}"),
        format!("-P{port}"),
        format!("-u{user}"),
    ];
    if !db.is_empty() {
        args.push(db);
    }
    let env = vec![("MYSQL_PWD".to_string(), password)];
    spawn_with_stdin("mysql", &args, runtime, dump, decompress, &env).await
}

async fn restore_sqlite(conn: &Connection, dump: &Path) -> Result<()> {
    let dest = sqlite_path(conn)?;
    // sqlite3 .restore is the integrity-preserving path.
    let exe = which::which("sqlite3").map_err(|_| anyhow::anyhow!("sqlite3 not on PATH"))?;
    let status = Command::new(&exe)
        .arg(&dest)
        .arg(format!(".restore '{}'", dump.display()))
        .status()
        .await
        .context("spawn sqlite3 .restore")?;
    if !status.success() {
        anyhow::bail!("sqlite3 .restore failed");
    }
    Ok(())
}

// ────────────────────────────────────────────────────────────────────────────
// Stdin-piped spawn
// ────────────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy)]
enum Decompress {
    None,
    Gzip,
}

async fn spawn_with_stdin(
    tool: &str,
    args: &[String],
    runtime: &Runtime,
    dump: &Path,
    decompress: Decompress,
    extra_env: &[(String, String)],
) -> Result<()> {
    let (exe, args) = shape(tool, args, runtime, extra_env);
    let mut cmd = Command::new(&exe);
    cmd.args(&args);
    cmd.stdin(Stdio::piped());
    cmd.stdout(Stdio::piped());
    cmd.stderr(Stdio::piped());
    if matches!(runtime, Runtime::Local) {
        for (k, v) in extra_env {
            cmd.env(k, v);
        }
    }
    let mut child = cmd
        .spawn()
        .with_context(|| format!("spawn {} {:?}", exe, args))?;

    let mut child_stdin = child.stdin.take().context("no stdin on child")?;
    let dump = dump.to_path_buf();

    let pipe_task = tokio::spawn(async move {
        match decompress {
            Decompress::None => {
                let mut f = tokio::fs::File::open(&dump).await?;
                tokio::io::copy(&mut f, &mut child_stdin).await?;
            }
            Decompress::Gzip => {
                // Spawn a host-side gunzip to avoid pulling a Rust gzip crate.
                let mut g = Command::new("gzip")
                    .arg("-dc")
                    .arg(&dump)
                    .stdout(Stdio::piped())
                    .stderr(Stdio::piped())
                    .spawn()?;
                let mut g_out = g.stdout.take().context("no stdout on gunzip")?;
                tokio::io::copy(&mut g_out, &mut child_stdin).await?;
                let status = g.wait().await?;
                if !status.success() {
                    anyhow::bail!("gunzip failed");
                }
            }
        }
        child_stdin.flush().await?;
        drop(child_stdin);
        Ok::<(), anyhow::Error>(())
    });

    let mut stderr_buf = String::new();
    if let Some(mut stderr) = child.stderr.take() {
        let _ = stderr.read_to_string(&mut stderr_buf).await;
    }
    if let Some(mut stdout) = child.stdout.take() {
        let mut sink = String::new();
        let _ = stdout.read_to_string(&mut sink).await;
    }
    let status = child.wait().await?;
    let _ = pipe_task.await;
    if !status.success() {
        let trimmed = stderr_buf.trim();
        let last = trimmed.lines().rev().take(3).collect::<Vec<_>>();
        let last = last.into_iter().rev().collect::<Vec<_>>().join(" / ");
        let msg = if last.is_empty() {
            format!("{tool} exited {:?}", status.code())
        } else {
            last
        };
        anyhow::bail!("{}", msg);
    }
    Ok(())
}

// ────────────────────────────────────────────────────────────────────────────
// Shared with backup.rs — keep the wire format identical
// ────────────────────────────────────────────────────────────────────────────

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

fn host_for(conn: &Connection, runtime: &Runtime) -> String {
    let raw = if conn.host.is_empty() {
        "127.0.0.1".to_string()
    } else {
        conn.host.clone()
    };
    match runtime {
        Runtime::DockerExec { .. } => "127.0.0.1".into(),
        Runtime::DockerRun { .. } => {
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

fn postgres_uri(conn: &Connection, runtime: &Runtime) -> String {
    let host = host_for(conn, runtime);
    let port = port_string(conn);
    let user = conn.user.clone().unwrap_or_else(|| "postgres".into());
    let pw = conn.password.clone().unwrap_or_default();
    let db = conn
        .database
        .clone()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "postgres".into());
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

fn sqlite_path(conn: &Connection) -> Result<PathBuf> {
    let src = conn
        .database
        .as_deref()
        .or(conn.uri.as_deref())
        .ok_or_else(|| anyhow::anyhow!("sqlite needs a database file path"))?;
    let p = src.strip_prefix("sqlite://").unwrap_or(src);
    let expanded = if let Some(rest) = p.strip_prefix("~/") {
        dirs::home_dir()
            .map(|h| h.join(rest))
            .unwrap_or_else(|| PathBuf::from(p))
    } else {
        PathBuf::from(p)
    };
    Ok(expanded)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shape_local_passthrough() {
        let rt = Runtime::Local;
        let (exe, args) = shape("pg_restore", &["-d".into(), "x".into()], &rt, &[]);
        assert_eq!(exe, "pg_restore");
        assert_eq!(args, vec!["-d", "x"]);
    }

    #[test]
    fn shape_docker_exec_prefix() {
        let rt = Runtime::DockerExec {
            container_id: "abc".into(),
        };
        let (exe, args) = shape(
            "pg_restore",
            &["-d".into(), "x".into()],
            &rt,
            &[("PGPASSWORD".into(), "s".into())],
        );
        assert_eq!(exe, "docker");
        assert!(args.contains(&"exec".to_string()));
        assert!(args.contains(&"abc".to_string()));
        assert!(args.contains(&"PGPASSWORD=s".to_string()));
    }

    #[test]
    fn postgres_uri_applies_quirks() {
        let c = Connection {
            kind: DbKind::Postgres,
            host: "db.foo.supabase.co".into(),
            port: 5432,
            user: Some("u".into()),
            password: Some("p".into()),
            database: Some("postgres".into()),
            ..Default::default()
        };
        let uri = postgres_uri(&c, &Runtime::Local);
        assert!(uri.contains("sslmode=require"));
    }

    #[test]
    fn mongo_uri_atlas_adds_tls() {
        let c = Connection {
            kind: DbKind::Mongo,
            host: "cluster.abc.mongodb.net".into(),
            port: 27017,
            ..Default::default()
        };
        let uri = mongo_uri(&c, &Runtime::Local);
        assert!(uri.contains("tls=true"));
    }
}
