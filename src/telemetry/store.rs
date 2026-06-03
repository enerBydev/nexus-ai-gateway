//! SQLite analytics store for telemetry data.
//!
//! Schema:
//! - `daily_fingerprints`: per-fingerprint daily records (HMAC hashes only, zero PII)
//! - `daily_stats`: aggregated daily statistics for /analytics endpoint
//!
//! Retention: 30 days by default, auto-purged on startup and daily.

use std::path::Path;
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result};
use rusqlite::{params, Connection};

use crate::telemetry::fingerprint::ClientFingerprint;

/// Aggregated daily stats entry — returned by /analytics endpoint.
#[derive(Debug, Clone, serde::Serialize)]
pub struct DailyStatsEntry {
    pub date: String,
    pub total_requests: i64,
    pub unique_fingerprints: i64,
    pub models_used: serde_json::Value,
    pub client_types: serde_json::Value,
    pub avg_message_count: f64,
    pub tool_use_ratio: f64,
}

/// Thread-safe SQLite store for telemetry analytics.
#[derive(Debug, Clone)]
pub struct TelemetryStore {
    conn: Arc<Mutex<Connection>>,
}

impl TelemetryStore {
    /// Open (or create) the telemetry database at the given path.
    /// Creates parent directories if needed.
    /// Creates tables if they don't exist.
    pub fn open(path: &Path) -> Result<Self> {
        // Create parent directory if needed
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).with_context(|| {
                format!("creating telemetry db directory: {}", parent.display())
            })?;
        }

        let conn = Connection::open(path)
            .with_context(|| format!("opening telemetry db: {}", path.display()))?;

        // Enable WAL mode for better concurrent read performance
        conn.execute_batch("PRAGMA journal_mode=WAL; PRAGMA synchronous=NORMAL;")?;

        // Create schema
        conn.execute_batch(SCHEMA)?;

        Ok(Self { conn: Arc::new(Mutex::new(conn)) })
    }

    /// Open an in-memory database (for testing).
    #[cfg(test)]
    pub fn open_in_memory() -> Result<Self> {
        let conn = Connection::open_in_memory()?;
        conn.execute_batch(SCHEMA)?;
        Ok(Self { conn: Arc::new(Mutex::new(conn)) })
    }

    /// Record a client fingerprint for today's date.
    /// Upserts into daily_fingerprints and updates daily_stats.
    pub fn record_fingerprint(&self, fp: &ClientFingerprint) -> Result<()> {
        let conn = self.conn.lock().map_err(|e| anyhow::anyhow!("db lock: {e}"))?;
        let today = chrono::Local::now().format("%Y-%m-%d").to_string();
        let now = chrono::Local::now().to_rfc3339();
        let ct_label = fp.client_type.to_string();

        // Combine both fingerprints as composite key (IP + key = unique user)
        let composite = format!("{}:{}", fp.fingerprint_ip, fp.fingerprint_key);

        // Upsert daily_fingerprints
        conn.execute(
            "INSERT INTO daily_fingerprints (date, fingerprint, client_type, request_count, last_seen, model_stats)
             VALUES (?1, ?2, ?3, 1, ?4, ?5)
             ON CONFLICT(date, fingerprint) DO UPDATE SET
               request_count = request_count + 1,
               last_seen = excluded.last_seen,
               model_stats = excluded.model_stats",
            params![
                today,
                composite,
                ct_label,
                now,
                serde_json::json!({ fp.model.clone(): 1 }).to_string(),
            ],
        ).with_context(|| "inserting daily_fingerprint")?;

        // CR fix: Proper accumulation for daily_stats (read-modify-write)
        let tool_flag_f = if fp.has_tool_use { 1.0 } else { 0.0 };
        let existing_stats: Option<(i64, String, String, f64, f64)> = conn
            .query_row(
                "SELECT total_requests, models_used, client_types, avg_message_count, tool_use_ratio \
                 FROM daily_stats WHERE date = ?1",
                params![today],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?, row.get(4)?)),
            )
            .ok();

        if let Some((reqs, models_json, types_json, avg_msg, tool_ratio)) = existing_stats {
            // Merge model counts
            let mut models: std::collections::HashMap<String, i64> =
                serde_json::from_str(&models_json).unwrap_or_default();
            *models.entry(fp.model.clone()).or_insert(0) += 1;

            // Merge client type counts
            let mut types: std::collections::HashMap<String, i64> =
                serde_json::from_str(&types_json).unwrap_or_default();
            *types.entry(ct_label.clone()).or_insert(0) += 1;

            // Running average for message_count and tool_use_ratio
            let total = reqs as f64;
            let new_avg_msg = (avg_msg * total + fp.message_count as f64) / (total + 1.0);
            let new_tool_ratio = (tool_ratio * total + tool_flag_f) / (total + 1.0);

            conn.execute(
                "UPDATE daily_stats SET \
                 total_requests = total_requests + 1, \
                 models_used = ?1, client_types = ?2, \
                 avg_message_count = ?3, tool_use_ratio = ?4 \
                 WHERE date = ?5",
                params![
                    serde_json::to_string(&models).unwrap_or_default(),
                    serde_json::to_string(&types).unwrap_or_default(),
                    new_avg_msg,
                    new_tool_ratio,
                    today,
                ],
            )
            .with_context(|| "updating daily_stats")?;
        } else {
            // First request of the day — insert
            conn.execute(
                "INSERT INTO daily_stats (date, total_requests, unique_fingerprints, models_used, client_types, avg_message_count, tool_use_ratio) \
                 VALUES (?1, 1, 0, ?2, ?3, ?4, ?5)",
                params![
                    today,
                    serde_json::json!({ fp.model.clone(): 1 }).to_string(),
                    serde_json::json!({ ct_label: 1 }).to_string(),
                    fp.message_count as f64,
                    tool_flag_f,
                ],
            ).with_context(|| "inserting daily_stats")?;
        }

        // Recalculate unique_fingerprints for today
        let unique: i64 = conn
            .query_row(
                "SELECT COUNT(DISTINCT fingerprint) FROM daily_fingerprints WHERE date = ?1",
                params![today],
                |row| row.get(0),
            )
            .unwrap_or(0);

        conn.execute(
            "UPDATE daily_stats SET unique_fingerprints = ?1 WHERE date = ?2",
            params![unique, today],
        )?;

        Ok(())
    }

    /// Get daily stats for the last N days (for /analytics endpoint).
    pub fn get_daily_stats(&self, days: u32) -> Result<Vec<DailyStatsEntry>> {
        let conn = self.conn.lock().map_err(|e| anyhow::anyhow!("db lock: {e}"))?;
        let cutoff = chrono::Local::now() - chrono::Duration::days(days as i64);
        let cutoff_str = cutoff.format("%Y-%m-%d").to_string();

        let mut stmt = conn.prepare(
            "SELECT date, total_requests, unique_fingerprints, models_used, client_types, avg_message_count, tool_use_ratio
             FROM daily_stats WHERE date >= ?1 ORDER BY date DESC",
        )?;

        let entries = stmt
            .query_map(params![cutoff_str], |row| {
                let date: String = row.get(0)?;
                let total_requests: i64 = row.get(1)?;
                let unique_fingerprints: i64 = row.get(2)?;
                let models_used_str: String = row.get(3)?;
                let client_types_str: String = row.get(4)?;
                let avg_message_count: f64 = row.get(5)?;
                let tool_use_ratio: f64 = row.get(6)?;

                Ok(DailyStatsEntry {
                    date,
                    total_requests,
                    unique_fingerprints,
                    models_used: serde_json::from_str(&models_used_str)
                        .unwrap_or(serde_json::json!({})),
                    client_types: serde_json::from_str(&client_types_str)
                        .unwrap_or(serde_json::json!({})),
                    avg_message_count,
                    tool_use_ratio,
                })
            })?
            .filter_map(|e| e.ok())
            .collect();

        Ok(entries)
    }

    /// Get the count of unique fingerprints seen today (for Prometheus gauge).
    pub fn get_unique_fingerprint_count_today(&self) -> Result<u64> {
        let conn = self.conn.lock().map_err(|e| anyhow::anyhow!("db lock: {e}"))?;
        let today = chrono::Local::now().format("%Y-%m-%d").to_string();
        let count: i64 = conn
            .query_row(
                "SELECT COUNT(DISTINCT fingerprint) FROM daily_fingerprints WHERE date = ?1",
                params![today],
                |row| row.get(0),
            )
            .unwrap_or(0);
        Ok(count as u64)
    }

    /// Purge records older than `retention_days`.
    /// Returns the number of rows deleted.
    pub fn purge_old_records(&self, retention_days: u32) -> Result<u64> {
        let conn = self.conn.lock().map_err(|e| anyhow::anyhow!("db lock: {e}"))?;
        let cutoff = chrono::Local::now() - chrono::Duration::days(retention_days as i64);
        let cutoff_str = cutoff.format("%Y-%m-%d").to_string();

        let deleted_fps = conn
            .execute("DELETE FROM daily_fingerprints WHERE date < ?1", params![cutoff_str])
            .with_context(|| "purging old fingerprints")?;

        let deleted_stats = conn
            .execute("DELETE FROM daily_stats WHERE date < ?1", params![cutoff_str])
            .with_context(|| "purging old daily_stats")?;

        let total = (deleted_fps + deleted_stats) as u64;
        if total > 0 {
            tracing::info!("🧹 Purged {total} old telemetry records (>{retention_days} days)");
        }
        Ok(total)
    }
}

const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS daily_fingerprints (
    date          TEXT NOT NULL,
    fingerprint   TEXT NOT NULL,
    client_type   TEXT NOT NULL,
    request_count INTEGER NOT NULL DEFAULT 1,
    last_seen     TEXT NOT NULL,
    model_stats   TEXT NOT NULL DEFAULT '{}',
    PRIMARY KEY (date, fingerprint)
);

CREATE TABLE IF NOT EXISTS daily_stats (
    date                TEXT PRIMARY KEY,
    total_requests      INTEGER NOT NULL DEFAULT 0,
    unique_fingerprints INTEGER NOT NULL DEFAULT 0,
    models_used         TEXT NOT NULL DEFAULT '{}',
    client_types        TEXT NOT NULL DEFAULT '{}',
    avg_message_count   REAL NOT NULL DEFAULT 0,
    tool_use_ratio      REAL NOT NULL DEFAULT 0
);

CREATE INDEX IF NOT EXISTS idx_fingerprints_date ON daily_fingerprints(date);
"#;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::telemetry::fingerprint::ClientType;

    fn make_fingerprint(model: &str, fp_ip: &str, fp_key: &str) -> ClientFingerprint {
        ClientFingerprint {
            fingerprint_ip: fp_ip.to_string(),
            fingerprint_key: fp_key.to_string(),
            client_type: ClientType::ClaudeCode,
            user_agent_category: "claude_code".to_string(),
            message_count: 5,
            has_tool_use: true,
            has_system_prompt: true,
            model: model.to_string(),
            is_streaming: true,
        }
    }

    #[test]
    fn open_in_memory_creates_schema() {
        let store = TelemetryStore::open_in_memory().unwrap();
        // Verify tables exist by querying them
        let count = store.get_unique_fingerprint_count_today().unwrap();
        assert_eq!(count, 0);
    }

    #[test]
    fn record_fingerprint_increments_unique() {
        let store = TelemetryStore::open_in_memory().unwrap();
        let fp = make_fingerprint("claude-sonnet-4-6", "abc123", "def456");
        store.record_fingerprint(&fp).unwrap();

        let count = store.get_unique_fingerprint_count_today().unwrap();
        assert_eq!(count, 1);
    }

    #[test]
    fn same_fingerprint_does_not_increase_unique() {
        let store = TelemetryStore::open_in_memory().unwrap();
        let fp = make_fingerprint("claude-sonnet-4-6", "abc123", "def456");
        store.record_fingerprint(&fp).unwrap();
        store.record_fingerprint(&fp).unwrap();

        let count = store.get_unique_fingerprint_count_today().unwrap();
        assert_eq!(count, 1, "Same fingerprint should count as one unique user");
    }

    #[test]
    fn different_fingerprints_increase_unique() {
        let store = TelemetryStore::open_in_memory().unwrap();
        let fp1 = make_fingerprint("claude-sonnet-4-6", "abc123", "def456");
        let fp2 = make_fingerprint("claude-opus-4-5", "xyz789", "uvw012");
        store.record_fingerprint(&fp1).unwrap();
        store.record_fingerprint(&fp2).unwrap();

        let count = store.get_unique_fingerprint_count_today().unwrap();
        assert_eq!(count, 2, "Different fingerprints should count as two unique users");
    }

    #[test]
    fn get_daily_stats_returns_entries() {
        let store = TelemetryStore::open_in_memory().unwrap();
        let fp = make_fingerprint("claude-sonnet-4-6", "abc123", "def456");
        store.record_fingerprint(&fp).unwrap();

        let stats = store.get_daily_stats(1).unwrap();
        assert!(!stats.is_empty());
        assert!(stats[0].total_requests >= 1);
    }

    #[test]
    fn purge_removes_old_records() {
        let store = TelemetryStore::open_in_memory().unwrap();
        // Insert a record with a date far in the past
        let conn = store.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO daily_stats (date, total_requests, unique_fingerprints, models_used, client_types, avg_message_count, tool_use_ratio)
             VALUES ('2020-01-01', 10, 2, '{}', '{}', 5.0, 0.5)",
            [],
        ).unwrap();
        drop(conn);

        let purged = store.purge_old_records(30).unwrap();
        assert!(purged > 0, "Old records should be purged");
    }

    #[test]
    fn daily_stats_entry_serializes_to_json() {
        let entry = DailyStatsEntry {
            date: "2026-05-31".to_string(),
            total_requests: 1500,
            unique_fingerprints: 3,
            models_used: serde_json::json!({"claude-sonnet-4-6": 1200}),
            client_types: serde_json::json!({"claude_code": 2}),
            avg_message_count: 12.3,
            tool_use_ratio: 0.78,
        };
        let json = serde_json::to_string(&entry).unwrap();
        assert!(json.contains("2026-05-31"));
        assert!(json.contains("1500"));
    }
}
