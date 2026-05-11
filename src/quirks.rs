//! Provider quirks: turn a hostname into the connection knobs the user would
//! otherwise have to set by hand (sslmode, etc.). All read-only — we never
//! modify the remote DB, just our outbound connection params.

use crate::types::DbKind;

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Quirks {
    pub require_ssl: bool,
    pub provider: Option<&'static str>,
}

impl Quirks {
    /// One-line note about the inferred provider, for the UI.
    pub fn note(&self) -> Option<String> {
        let provider = self.provider?;
        let bits = if self.require_ssl {
            format!("{provider} · sslmode=require auto-applied")
        } else {
            provider.to_string()
        };
        Some(bits)
    }
}

pub fn for_host(host: &str, kind: DbKind) -> Quirks {
    let h = host.trim().to_lowercase();
    if h.is_empty() {
        return Quirks::default();
    }
    let needs_ssl_postgres = matches!(kind, DbKind::Postgres | DbKind::Mysql);
    let needs_ssl_mongo = matches!(kind, DbKind::Mongo);

    // Postgres / MySQL managed hosts that mandate TLS.
    if h.ends_with(".supabase.co") || h.ends_with(".supabase.com") || h.ends_with(".pooler.supabase.com") {
        return Quirks {
            require_ssl: needs_ssl_postgres,
            provider: Some("Supabase"),
        };
    }
    if h.ends_with(".rds.amazonaws.com") {
        return Quirks {
            require_ssl: needs_ssl_postgres,
            provider: Some("Amazon RDS"),
        };
    }
    if h.ends_with(".neon.tech") || h.contains(".neon.tech") {
        return Quirks {
            require_ssl: needs_ssl_postgres,
            provider: Some("Neon"),
        };
    }
    if h.ends_with(".aivencloud.com") {
        return Quirks {
            require_ssl: needs_ssl_postgres || needs_ssl_mongo,
            provider: Some("Aiven"),
        };
    }
    if h.ends_with(".render.com") || h.ends_with(".oregon-postgres.render.com") {
        return Quirks {
            require_ssl: needs_ssl_postgres,
            provider: Some("Render"),
        };
    }
    if h.ends_with(".railway.app") || h.ends_with(".railway.internal") {
        return Quirks {
            require_ssl: needs_ssl_postgres,
            provider: Some("Railway"),
        };
    }
    if h.ends_with(".ondigitalocean.com") || h.ends_with(".db.ondigitalocean.com") {
        return Quirks {
            require_ssl: needs_ssl_postgres || needs_ssl_mongo,
            provider: Some("DigitalOcean"),
        };
    }
    if h.ends_with(".googleusercontent.com") || h.ends_with(".sql.goog") {
        return Quirks {
            require_ssl: needs_ssl_postgres,
            provider: Some("Google Cloud SQL"),
        };
    }
    if h.ends_with(".postgres.database.azure.com")
        || h.ends_with(".mysql.database.azure.com")
    {
        return Quirks {
            require_ssl: needs_ssl_postgres,
            provider: Some("Azure DB"),
        };
    }
    if h.ends_with(".cosmos.azure.com") || h.ends_with(".mongo.cosmos.azure.com") {
        return Quirks {
            require_ssl: needs_ssl_mongo,
            provider: Some("Cosmos DB"),
        };
    }
    if h.ends_with(".mongodb.net") {
        return Quirks {
            require_ssl: needs_ssl_mongo,
            provider: Some("MongoDB Atlas"),
        };
    }
    if h.ends_with(".timescaledb.io") || h.ends_with(".tsdb.cloud.timescale.com") {
        return Quirks {
            require_ssl: needs_ssl_postgres,
            provider: Some("Timescale Cloud"),
        };
    }
    if h.ends_with(".cockroachlabs.cloud") {
        return Quirks {
            require_ssl: needs_ssl_postgres,
            provider: Some("Cockroach Cloud"),
        };
    }
    Quirks::default()
}

/// Augment a URI string with `sslmode=require` (postgres/mysql) or `tls=true`
/// (mongo) when the host is known-managed.
pub fn apply_to_uri(uri: &str, kind: DbKind) -> String {
    let parsed = match url::Url::parse(uri) {
        Ok(u) => u,
        Err(_) => return uri.to_string(),
    };
    let host = parsed.host_str().unwrap_or("");
    let q = for_host(host, kind);
    if !q.require_ssl {
        return uri.to_string();
    }
    match kind {
        DbKind::Postgres | DbKind::Mysql => ensure_query(uri, "sslmode", "require"),
        DbKind::Mongo => ensure_query(uri, "tls", "true"),
        _ => uri.to_string(),
    }
}

fn ensure_query(uri: &str, key: &str, value: &str) -> String {
    let mut url = match url::Url::parse(uri) {
        Ok(u) => u,
        Err(_) => return uri.to_string(),
    };
    if url.query_pairs().any(|(k, _)| k.eq_ignore_ascii_case(key)) {
        return uri.to_string();
    }
    url.query_pairs_mut().append_pair(key, value);
    url.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn supabase_requires_ssl() {
        let q = for_host("db.ombbwqymtttgvyieauxg.supabase.co", DbKind::Postgres);
        assert!(q.require_ssl);
        assert_eq!(q.provider, Some("Supabase"));
    }

    #[test]
    fn rds_requires_ssl() {
        let q = for_host("prod.cluster-xyz.us-east-1.rds.amazonaws.com", DbKind::Postgres);
        assert!(q.require_ssl);
        assert_eq!(q.provider, Some("Amazon RDS"));
    }

    #[test]
    fn neon_requires_ssl() {
        let q = for_host("ep-cool-bird-12345.us-east-2.aws.neon.tech", DbKind::Postgres);
        assert!(q.require_ssl);
        assert_eq!(q.provider, Some("Neon"));
    }

    #[test]
    fn atlas_requires_tls_for_mongo() {
        let q = for_host("cluster0.abcd1.mongodb.net", DbKind::Mongo);
        assert!(q.require_ssl);
        assert_eq!(q.provider, Some("MongoDB Atlas"));
    }

    #[test]
    fn unknown_host_no_ssl() {
        let q = for_host("my-server.example.com", DbKind::Postgres);
        assert!(!q.require_ssl);
        assert!(q.provider.is_none());
    }

    #[test]
    fn apply_to_uri_adds_sslmode_once() {
        let uri = "postgres://u:p@db.foo.supabase.co:5432/postgres";
        let out = apply_to_uri(uri, DbKind::Postgres);
        assert!(out.contains("sslmode=require"));
        // Idempotent.
        assert_eq!(apply_to_uri(&out, DbKind::Postgres), out);
    }

    #[test]
    fn apply_to_uri_preserves_explicit_sslmode() {
        let uri = "postgres://u:p@db.foo.supabase.co:5432/postgres?sslmode=verify-full";
        let out = apply_to_uri(uri, DbKind::Postgres);
        assert!(out.contains("sslmode=verify-full"));
        assert_eq!(out.matches("sslmode=").count(), 1);
    }

    #[test]
    fn apply_to_uri_mongo_uses_tls() {
        let uri = "mongodb://u:p@cluster0.abc.mongodb.net/db";
        let out = apply_to_uri(uri, DbKind::Mongo);
        assert!(out.contains("tls=true"));
    }

    #[test]
    fn apply_to_uri_unknown_host_unchanged() {
        let uri = "postgres://u:p@example.com/db";
        assert_eq!(apply_to_uri(uri, DbKind::Postgres), uri);
    }

    #[test]
    fn empty_host_returns_default() {
        let q = for_host("", DbKind::Postgres);
        assert!(!q.require_ssl);
    }

    #[test]
    fn host_case_insensitive() {
        let q = for_host("DB.PROJECT.SUPABASE.CO", DbKind::Postgres);
        assert!(q.require_ssl);
    }
}
