use std::collections::HashMap;
use std::time::Duration;

use anyhow::{Context, Result};
use serde::Deserialize;
use tokio::process::Command;

use crate::types::{DbKind, DetectedSource};

const TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Debug, Deserialize)]
struct PsLine {
    #[serde(rename = "ID")]
    id: String,
    #[serde(rename = "Image")]
    image: String,
    #[serde(rename = "Names")]
    names: String,
}

#[derive(Debug, Deserialize)]
struct InspectResult {
    #[serde(rename = "Config")]
    config: InspectConfig,
    #[serde(rename = "NetworkSettings")]
    network: InspectNetwork,
}

#[derive(Debug, Deserialize)]
struct InspectConfig {
    #[serde(rename = "Env")]
    env: Option<Vec<String>>,
}

#[derive(Debug, Deserialize)]
struct InspectNetwork {
    #[serde(rename = "Ports")]
    ports: Option<HashMap<String, Option<Vec<PortBinding>>>>,
}

#[derive(Debug, Deserialize)]
struct PortBinding {
    #[serde(rename = "HostPort")]
    host_port: String,
}

pub async fn scan() -> Result<Vec<DetectedSource>> {
    if which::which("docker").is_err() {
        return Ok(vec![]);
    }
    let lines = run_ps().await.unwrap_or_default();
    let mut detected = Vec::new();
    for line in lines {
        let Some(kind) = DbKind::from_image(&line.image) else { continue };
        if let Ok(src) = inspect_one(&line.id, kind, &line.image, &line.names).await {
            detected.push(src);
        }
    }
    Ok(detected)
}

async fn run_ps() -> Result<Vec<PsLine>> {
    let output = tokio::time::timeout(
        TIMEOUT,
        Command::new("docker")
            .args(["ps", "--format", "{{json .}}"])
            .output(),
    )
    .await
    .context("docker ps timed out")??;
    if !output.status.success() {
        anyhow::bail!("docker ps failed: {}", String::from_utf8_lossy(&output.stderr));
    }
    let mut out = Vec::new();
    for line in String::from_utf8_lossy(&output.stdout).lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        if let Ok(p) = serde_json::from_str::<PsLine>(line) {
            out.push(p);
        }
    }
    Ok(out)
}

async fn inspect_one(
    id: &str,
    kind: DbKind,
    image: &str,
    names: &str,
) -> Result<DetectedSource> {
    let output = tokio::time::timeout(
        TIMEOUT,
        Command::new("docker").args(["inspect", id]).output(),
    )
    .await
    .context("docker inspect timed out")??;
    if !output.status.success() {
        anyhow::bail!(
            "docker inspect {id} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    let parsed: Vec<InspectResult> = serde_json::from_slice(&output.stdout)
        .context("parse docker inspect output")?;
    let result = parsed.into_iter().next().context("empty inspect")?;

    let env = parse_env(result.config.env.as_deref().unwrap_or(&[]));
    let host_port = pick_host_port(kind, result.network.ports.as_ref());

    let container_name = names.split(',').next().unwrap_or(names).trim().trim_start_matches('/').to_string();

    let (user, password, database) = extract_credentials(kind, &env);

    Ok(DetectedSource {
        kind,
        container_id: id.to_string(),
        container_name,
        image: image.to_string(),
        host_port: host_port.unwrap_or(kind.default_port()),
        user,
        password,
        database,
    })
}

fn parse_env(env: &[String]) -> HashMap<String, String> {
    env.iter()
        .filter_map(|kv| kv.split_once('='))
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect()
}

fn pick_host_port(
    kind: DbKind,
    ports: Option<&HashMap<String, Option<Vec<PortBinding>>>>,
) -> Option<u16> {
    let target = kind.default_port();
    let want = format!("{}/tcp", target);
    let map = ports?;
    if let Some(Some(bindings)) = map.get(&want) {
        if let Some(b) = bindings.first() {
            if let Ok(p) = b.host_port.parse::<u16>() {
                return Some(p);
            }
        }
    }
    // Fallback: any tcp binding.
    for (proto, bindings) in map.iter() {
        if !proto.ends_with("/tcp") {
            continue;
        }
        if let Some(bindings) = bindings {
            if let Some(b) = bindings.first() {
                if let Ok(p) = b.host_port.parse::<u16>() {
                    return Some(p);
                }
            }
        }
    }
    None
}

fn extract_credentials(
    kind: DbKind,
    env: &HashMap<String, String>,
) -> (Option<String>, Option<String>, Option<String>) {
    let g = |k: &str| env.get(k).cloned();
    match kind {
        DbKind::Postgres => (
            g("POSTGRES_USER").or_else(|| Some("postgres".into())),
            g("POSTGRES_PASSWORD"),
            g("POSTGRES_DB"),
        ),
        DbKind::Mongo => (
            g("MONGO_INITDB_ROOT_USERNAME").or_else(|| g("MONGODB_ROOT_USER")),
            g("MONGO_INITDB_ROOT_PASSWORD").or_else(|| g("MONGODB_ROOT_PASSWORD")),
            g("MONGO_INITDB_DATABASE"),
        ),
        DbKind::Mysql => {
            let user = g("MYSQL_USER").or_else(|| g("MARIADB_USER")).or_else(|| Some("root".into()));
            let pw = g("MYSQL_PASSWORD")
                .or_else(|| g("MARIADB_PASSWORD"))
                .or_else(|| g("MYSQL_ROOT_PASSWORD"))
                .or_else(|| g("MARIADB_ROOT_PASSWORD"));
            let db = g("MYSQL_DATABASE").or_else(|| g("MARIADB_DATABASE"));
            (user, pw, db)
        }
        DbKind::Redis => (
            None,
            g("REDIS_PASSWORD").or_else(|| g("REDIS_ARGS").and_then(|args| extract_redis_password(&args))),
            None,
        ),
        DbKind::Sqlite => (None, None, None),
    }
}

fn extract_redis_password(args: &str) -> Option<String> {
    // e.g. REDIS_ARGS="--requirepass mysecret"
    let mut it = args.split_whitespace().peekable();
    while let Some(tok) = it.next() {
        if tok == "--requirepass" {
            return it.next().map(|s| s.to_string());
        }
        if let Some(p) = tok.strip_prefix("--requirepass=") {
            return Some(p.to_string());
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn env_parsing_handles_equals_in_values() {
        let env = vec!["A=1".into(), "B=foo=bar".into(), "C=".into()];
        let m = parse_env(&env);
        assert_eq!(m.get("A").unwrap(), "1");
        assert_eq!(m.get("B").unwrap(), "foo=bar");
        assert_eq!(m.get("C").unwrap(), "");
    }

    #[test]
    fn postgres_credentials_extracted() {
        let env: HashMap<String, String> = [
            ("POSTGRES_USER".to_string(), "takshak".to_string()),
            ("POSTGRES_PASSWORD".to_string(), "secret".to_string()),
            ("POSTGRES_DB".to_string(), "app".to_string()),
        ]
        .into_iter()
        .collect();
        let (u, p, d) = extract_credentials(DbKind::Postgres, &env);
        assert_eq!(u.unwrap(), "takshak");
        assert_eq!(p.unwrap(), "secret");
        assert_eq!(d.unwrap(), "app");
    }

    #[test]
    fn postgres_user_defaults_when_env_absent() {
        let env: HashMap<String, String> = HashMap::new();
        let (u, _, _) = extract_credentials(DbKind::Postgres, &env);
        assert_eq!(u.as_deref(), Some("postgres"));
    }

    #[test]
    fn mysql_credentials_fall_back() {
        let env: HashMap<String, String> = [(
            "MYSQL_ROOT_PASSWORD".to_string(),
            "root-secret".to_string(),
        )]
        .into_iter()
        .collect();
        let (u, p, _) = extract_credentials(DbKind::Mysql, &env);
        assert_eq!(u.unwrap(), "root");
        assert_eq!(p.unwrap(), "root-secret");
    }

    #[test]
    fn redis_requirepass_args() {
        assert_eq!(
            extract_redis_password("--requirepass myhardpass --maxmemory 100mb"),
            Some("myhardpass".into())
        );
        assert_eq!(
            extract_redis_password("--requirepass=anotherpw"),
            Some("anotherpw".into())
        );
        assert_eq!(extract_redis_password("--appendonly yes"), None);
    }

    #[test]
    fn pick_host_port_prefers_match() {
        let mut ports = HashMap::new();
        ports.insert(
            "5432/tcp".to_string(),
            Some(vec![PortBinding {
                host_port: "55432".into(),
            }]),
        );
        ports.insert("80/tcp".to_string(), None);
        assert_eq!(pick_host_port(DbKind::Postgres, Some(&ports)), Some(55432));
    }
}
