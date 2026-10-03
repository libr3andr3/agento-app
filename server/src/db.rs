//! SQLite conveniences. Postgres used to do hashing, clocks and intervals in
//! SQL; on the phone those live here so every query stays a plain comparison.

use chrono::{DateTime, Duration, Utc};
use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};
use std::str::FromStr;

pub type Db = sqlx::SqlitePool;

/// Opens (creating if needed) the on-device database and applies migrations.
pub async fn open(url: &str) -> anyhow::Result<Db> {
    let opts = SqliteConnectOptions::from_str(url)?
        .create_if_missing(true)
        .foreign_keys(true)
        .journal_mode(sqlx::sqlite::SqliteJournalMode::Wal)
        .busy_timeout(std::time::Duration::from_secs(5));
    // One writer at a time is what SQLite does anyway; a small pool keeps
    // reads flowing while a customer turn is being written.
    let db = SqlitePoolOptions::new().max_connections(4).connect_with(opts).await?;
    sqlx::migrate!("./migrations").run(&db).await?;
    Ok(db)
}

/// Hex SHA-256 — the at-rest form of every token (device, OTP, proof).
pub fn sha256_hex(bytes: &[u8]) -> String {
    use sha2::Digest;
    hex::encode(sha2::Sha256::digest(bytes))
}

pub fn now() -> DateTime<Utc> {
    Utc::now()
}

pub fn ago(d: Duration) -> DateTime<Utc> {
    Utc::now() - d
}

pub fn hence(d: Duration) -> DateTime<Utc> {
    Utc::now() + d
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sha256_hex_vector() {
        assert_eq!(sha256_hex(b"abc"), "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad");
    }

    #[test]
    fn clock_helpers_are_ordered() {
        let (a, n, h) = (ago(Duration::minutes(1)), now(), hence(Duration::minutes(1)));
        assert!(a < n && n < h);
        assert!((h - a - Duration::minutes(2)).num_seconds().abs() <= 1);
    }

    #[tokio::test]
    async fn open_migrates_and_enforces_foreign_keys() {
        let db = crate::testkit::db().await;
        let tables: Vec<String> = sqlx::query_scalar("SELECT name FROM sqlite_master WHERE type = 'table'").fetch_all(&db).await.unwrap();
        for t in ["businesses", "outcomes", "coins", "settings", "agent_identity"] {
            assert!(tables.iter().any(|x| x == t), "{t} missing");
        }
        let fk = sqlx::query("INSERT INTO outcomes (id, business_id, kind, client_hash) VALUES ('x', X'00', 'sale', 'h')").execute(&db).await;
        assert!(fk.is_err(), "foreign keys are on");
        // Re-opening the same file is idempotent.
        let mode: String = sqlx::query_scalar("PRAGMA journal_mode").fetch_one(&db).await.unwrap();
        assert_eq!(mode, "wal");
        assert!(open("sqlite:///nonexistent-agente-dir/sub/x.db").await.is_err());
    }
}
