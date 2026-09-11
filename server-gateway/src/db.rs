//! The gateway's book of what was spent.
//!
//! SQLite in a file: the whole load is a few writes a second from one box,
//! and a separate database server would be one more thing to wake up for at
//! night. Reads are indexed by licence and time because every request asks
//! the same question — what has this licence spent in the last five hours,
//! and in the last week.

use std::sync::Arc;

use parking_lot::Mutex;
use rusqlite::{Connection, OptionalExtension};

use vd_llm::Usage;

#[derive(Clone)]
pub struct Db {
    conn: Arc<Mutex<Connection>>,
}

/// One answer's cost, as it goes into the book.
#[derive(Debug, Clone)]
pub struct Spend {
    pub license_id: String,
    pub model: String,
    pub credits: f64,
    pub usage: Usage,
}

impl Db {
    pub fn open(path: &str) -> rusqlite::Result<Db> {
        let conn = Connection::open(path)?;
        Db::prepare(&conn)?;
        Ok(Db {
            conn: Arc::new(Mutex::new(conn)),
        })
    }

    #[cfg(test)]
    pub fn memory() -> rusqlite::Result<Db> {
        let conn = Connection::open_in_memory()?;
        Db::prepare(&conn)?;
        Ok(Db {
            conn: Arc::new(Mutex::new(conn)),
        })
    }

    fn prepare(conn: &Connection) -> rusqlite::Result<()> {
        // WAL so a long read of the usage table does not block the write that
        // closes the request that is producing it.
        let _: String = conn.query_row("PRAGMA journal_mode = WAL", [], |row| row.get(0))?;
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS usage (
                 id INTEGER PRIMARY KEY AUTOINCREMENT,
                 license_id TEXT NOT NULL,
                 ts INTEGER NOT NULL,
                 model TEXT NOT NULL,
                 credits REAL NOT NULL,
                 prompt_tokens INTEGER NOT NULL DEFAULT 0,
                 cached_tokens INTEGER NOT NULL DEFAULT 0,
                 completion_tokens INTEGER NOT NULL DEFAULT 0
             );
             CREATE INDEX IF NOT EXISTS usage_by_license ON usage(license_id, ts);
             CREATE TABLE IF NOT EXISTS revoked (
                 license_id TEXT PRIMARY KEY,
                 at INTEGER NOT NULL,
                 reason TEXT NOT NULL DEFAULT ''
             );",
        )
    }

    pub fn record(&self, spend: &Spend, now: i64) -> rusqlite::Result<()> {
        self.conn.lock().execute(
            "INSERT INTO usage (license_id, ts, model, credits, prompt_tokens, cached_tokens, completion_tokens)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            rusqlite::params![
                spend.license_id,
                now,
                spend.model,
                spend.credits,
                spend.usage.prompt_tokens,
                spend.usage.cached_tokens,
                spend.usage.completion_tokens,
            ],
        )?;
        Ok(())
    }

    /// Credits spent by this licence since a moment.
    pub fn spent_since(&self, license_id: &str, since: i64) -> rusqlite::Result<f64> {
        self.conn.lock().query_row(
            "SELECT COALESCE(SUM(credits), 0) FROM usage WHERE license_id = ?1 AND ts >= ?2",
            rusqlite::params![license_id, since],
            |row| row.get(0),
        )
    }

    /// When the oldest spend still inside a window happened — which is when
    /// the window first has room again.
    pub fn oldest_since(&self, license_id: &str, since: i64) -> rusqlite::Result<Option<i64>> {
        self.conn.lock().query_row(
            "SELECT MIN(ts) FROM usage WHERE license_id = ?1 AND ts >= ?2",
            rusqlite::params![license_id, since],
            |row| row.get::<_, Option<i64>>(0),
        )
    }

    /// Cache hit rate over a window, as a fraction of prompt tokens — the
    /// number that says whether the prompt is actually built the way §4 of
    /// the plan says it should be.
    pub fn cache_share_since(&self, since: i64) -> rusqlite::Result<f64> {
        let (prompt, cached): (i64, i64) = self.conn.lock().query_row(
            "SELECT COALESCE(SUM(prompt_tokens), 0), COALESCE(SUM(cached_tokens), 0)
             FROM usage WHERE ts >= ?1",
            rusqlite::params![since],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )?;
        if prompt <= 0 {
            return Ok(0.0);
        }
        Ok(cached as f64 / prompt as f64)
    }

    pub fn is_revoked(&self, license_id: &str) -> rusqlite::Result<bool> {
        let found: Option<String> = self
            .conn
            .lock()
            .query_row(
                "SELECT license_id FROM revoked WHERE license_id = ?1",
                rusqlite::params![license_id],
                |row| row.get(0),
            )
            .optional()?;
        Ok(found.is_some())
    }

    /// Withdraw a licence that is still cryptographically valid — the only
    /// way back from a leaked key short of rotating the signing key and
    /// reissuing everyone's.
    pub fn revoke(&self, license_id: &str, reason: &str, now: i64) -> rusqlite::Result<()> {
        self.conn.lock().execute(
            "INSERT OR REPLACE INTO revoked (license_id, at, reason) VALUES (?1, ?2, ?3)",
            rusqlite::params![license_id, now, reason],
        )?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spend(credits: f64) -> Spend {
        Spend {
            license_id: "VD-PRO-1".into(),
            model: "deepseek-chat".into(),
            credits,
            usage: Usage {
                prompt_tokens: 1000,
                completion_tokens: 100,
                total_tokens: 1100,
                cached_tokens: 600,
            },
        }
    }

    #[test]
    fn spending_adds_up_inside_the_window_only() {
        let db = Db::memory().unwrap();
        db.record(&spend(10.0), 1_000).unwrap();
        db.record(&spend(5.0), 2_000).unwrap();
        assert_eq!(db.spent_since("VD-PRO-1", 0).unwrap(), 15.0);
        assert_eq!(db.spent_since("VD-PRO-1", 1_500).unwrap(), 5.0);
        assert_eq!(db.spent_since("VD-PRO-1", 3_000).unwrap(), 0.0);
    }

    /// One licence's spending is not another's.
    #[test]
    fn licences_are_counted_apart() {
        let db = Db::memory().unwrap();
        db.record(&spend(10.0), 1_000).unwrap();
        db.record(
            &Spend {
                license_id: "VD-PRO-2".into(),
                ..spend(99.0)
            },
            1_000,
        )
        .unwrap();
        assert_eq!(db.spent_since("VD-PRO-1", 0).unwrap(), 10.0);
    }

    /// The window reopens when its oldest entry falls out of it, so that is
    /// the moment to tell the operator about.
    #[test]
    fn the_oldest_spend_says_when_there_is_room_again() {
        let db = Db::memory().unwrap();
        db.record(&spend(1.0), 1_000).unwrap();
        db.record(&spend(1.0), 4_000).unwrap();
        assert_eq!(db.oldest_since("VD-PRO-1", 0).unwrap(), Some(1_000));
        assert_eq!(db.oldest_since("VD-PRO-1", 2_000).unwrap(), Some(4_000));
        assert_eq!(db.oldest_since("VD-PRO-1", 9_000).unwrap(), None);
    }

    #[test]
    fn cache_share_is_cached_over_prompt_tokens() {
        let db = Db::memory().unwrap();
        db.record(&spend(1.0), 1_000).unwrap();
        db.record(&spend(1.0), 1_100).unwrap();
        assert!((db.cache_share_since(0).unwrap() - 0.6).abs() < 1e-9);
        assert_eq!(db.cache_share_since(5_000).unwrap(), 0.0);
    }

    #[test]
    fn a_revoked_licence_stays_revoked() {
        let db = Db::memory().unwrap();
        assert!(!db.is_revoked("VD-PRO-1").unwrap());
        db.revoke("VD-PRO-1", "leaked", 1_000).unwrap();
        assert!(db.is_revoked("VD-PRO-1").unwrap());
        // Revoking twice is not an error: the same key, leaked twice.
        db.revoke("VD-PRO-1", "leaked again", 2_000).unwrap();
        assert!(db.is_revoked("VD-PRO-1").unwrap());
    }
}
