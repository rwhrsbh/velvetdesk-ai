//! The gateway's book of what was spent.
//!
//! SQLite in a file: the whole load is a few writes a second from one box,
//! and a separate database server would be one more thing to wake up for at
//! night. Reads are indexed by licence and time because every request asks
//! the same question — what has this licence spent in the last five hours,
//! and in the last week.

use std::collections::HashMap;
use std::sync::Arc;

use parking_lot::Mutex;
use rusqlite::{Connection, OptionalExtension};

use vd_llm::provider::ProviderKind;
use vd_llm::Usage;

use crate::config::Tier;
use crate::registry::{KeyRow, LicenseRow, ModelRow, UpstreamRow};

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
             );

             -- What the gateway may spend money on. These live in the
             -- database rather than the config file so the admin page can
             -- change them without a deploy: a key that stopped working at
             -- two in the morning is replaced from a browser.
             CREATE TABLE IF NOT EXISTS upstream (
                 id TEXT PRIMARY KEY,
                 label TEXT NOT NULL DEFAULT '',
                 kind TEXT NOT NULL,
                 base_url TEXT NOT NULL,
                 api_version TEXT NOT NULL DEFAULT 'v1beta',
                 reasoning_dialect TEXT NOT NULL DEFAULT 'auto',
                 extra_headers TEXT NOT NULL DEFAULT '[]',
                 enabled INTEGER NOT NULL DEFAULT 1,
                 position INTEGER NOT NULL DEFAULT 0
             );
             CREATE TABLE IF NOT EXISTS upstream_key (
                 id INTEGER PRIMARY KEY AUTOINCREMENT,
                 upstream_id TEXT NOT NULL,
                 api_key TEXT NOT NULL,
                 added_at INTEGER NOT NULL,
                 UNIQUE(upstream_id, api_key)
             );
             CREATE TABLE IF NOT EXISTS model (
                 name TEXT PRIMARY KEY,
                 upstream_id TEXT NOT NULL,
                 upstream_name TEXT NOT NULL DEFAULT '',
                 price_in REAL NOT NULL DEFAULT 0,
                 price_cached REAL,
                 price_out REAL NOT NULL DEFAULT 0,
                 context_tokens INTEGER,
                 enabled INTEGER NOT NULL DEFAULT 1,
                 position INTEGER NOT NULL DEFAULT 0,
                 -- Takes dictation rather than chat. Voice is billed per clip
                 -- because a transcription reports no tokens to count.
                 voice INTEGER NOT NULL DEFAULT 0,
                 price_request REAL NOT NULL DEFAULT 0
             );
             CREATE TABLE IF NOT EXISTS tier (
                 name TEXT PRIMARY KEY,
                 credits_5h REAL NOT NULL,
                 credits_week REAL NOT NULL,
                 max_peers INTEGER NOT NULL DEFAULT 2
             );
             -- Licences the gateway has issued. The licence itself proves what
             -- it says; this is the ledger, so an operator can be found and
             -- their key revoked without asking them for it.
             CREATE TABLE IF NOT EXISTS license (
                 license_id TEXT PRIMARY KEY,
                 tier TEXT NOT NULL,
                 expires_at INTEGER NOT NULL DEFAULT 0,
                 max_peers INTEGER NOT NULL DEFAULT 2,
                 issued_at INTEGER NOT NULL,
                 note TEXT NOT NULL DEFAULT ''
             );",
        )?;
        Db::add_column(conn, "model", "voice", "INTEGER NOT NULL DEFAULT 0");
        Db::add_column(conn, "model", "price_request", "REAL NOT NULL DEFAULT 0");
        Ok(())
    }

    /// Add a column a older database does not have yet.
    ///
    /// The gateway upgrades in place — the database on the VPS predates the
    /// columns a new build wants — and sqlite has no `ADD COLUMN IF NOT
    /// EXISTS`, so a duplicate-column error is the expected answer and not a
    /// failure to start.
    fn add_column(conn: &Connection, table: &str, column: &str, definition: &str) {
        let _ = conn.execute(
            &format!("ALTER TABLE {table} ADD COLUMN {column} {definition}"),
            [],
        );
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

    /// The device count recorded for one licence, when the ledger has an
    /// entry for it.
    ///
    /// The number inside a licence is what it was sold with; this is what the
    /// operator has since been given. A team that outgrew its plan gets the
    /// bigger number here and keeps the key it paid for.
    pub fn peers_for(&self, license_id: &str) -> rusqlite::Result<Option<u32>> {
        let conn = self.conn.lock();
        let found: Option<i64> = conn
            .query_row(
                "SELECT max_peers FROM license WHERE license_id = ?1",
                [license_id],
                |row| row.get(0),
            )
            .optional()?;
        Ok(found.filter(|peers| *peers > 0).map(|peers| peers as u32))
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
    // ------------------------------------------------------------ upstreams

    pub fn list_upstreams(&self) -> rusqlite::Result<Vec<UpstreamRow>> {
        let conn = self.conn.lock();
        let mut statement = conn.prepare(
            "SELECT id, label, kind, base_url, api_version, reasoning_dialect,
                    extra_headers, enabled, position
             FROM upstream ORDER BY position, id",
        )?;
        let rows = statement
            .query_map([], |row| {
                let kind: String = row.get(2)?;
                let headers: String = row.get(6)?;
                Ok(UpstreamRow {
                    id: row.get(0)?,
                    label: row.get(1)?,
                    kind: if kind == "gemini" {
                        ProviderKind::Gemini
                    } else {
                        ProviderKind::OpenaiCompatible
                    },
                    base_url: row.get(3)?,
                    api_version: row.get(4)?,
                    reasoning_dialect: row.get(5)?,
                    extra_headers: serde_json::from_str(&headers).unwrap_or_default(),
                    enabled: row.get::<_, i64>(7)? != 0,
                    position: row.get(8)?,
                    key_count: 0,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    pub fn save_upstream(&self, upstream: &UpstreamRow) -> rusqlite::Result<()> {
        let kind = match upstream.kind {
            ProviderKind::Gemini => "gemini",
            ProviderKind::OpenaiCompatible => "openai_compatible",
        };
        self.conn.lock().execute(
            "INSERT INTO upstream (id, label, kind, base_url, api_version, reasoning_dialect,
                                   extra_headers, enabled, position)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)
             ON CONFLICT(id) DO UPDATE SET
                 label = excluded.label,
                 kind = excluded.kind,
                 base_url = excluded.base_url,
                 api_version = excluded.api_version,
                 reasoning_dialect = excluded.reasoning_dialect,
                 extra_headers = excluded.extra_headers,
                 enabled = excluded.enabled,
                 position = excluded.position",
            rusqlite::params![
                upstream.id,
                upstream.label,
                kind,
                upstream.base_url,
                upstream.api_version,
                upstream.reasoning_dialect,
                serde_json::to_string(&upstream.extra_headers).unwrap_or_else(|_| "[]".into()),
                upstream.enabled as i64,
                upstream.position,
            ],
        )?;
        Ok(())
    }

    /// Remove an upstream, its keys and its models together: a model pointing
    /// at an upstream that no longer exists is a request that fails at the
    /// worst possible moment.
    pub fn delete_upstream(&self, id: &str) -> rusqlite::Result<()> {
        let conn = self.conn.lock();
        conn.execute("DELETE FROM model WHERE upstream_id = ?1", [id])?;
        conn.execute("DELETE FROM upstream_key WHERE upstream_id = ?1", [id])?;
        conn.execute("DELETE FROM upstream WHERE id = ?1", [id])?;
        Ok(())
    }

    // ----------------------------------------------------------------- keys

    /// The keys themselves, for the pool. They go no further than this.
    pub fn upstream_keys(&self, upstream_id: &str) -> rusqlite::Result<Vec<String>> {
        let conn = self.conn.lock();
        let mut statement =
            conn.prepare("SELECT api_key FROM upstream_key WHERE upstream_id = ?1 ORDER BY id")?;
        let rows = statement
            .query_map([upstream_id], |row| row.get(0))?
            .collect::<rusqlite::Result<Vec<String>>>()?;
        Ok(rows)
    }

    /// The keys as the admin page sees them: masked, with the id needed to
    /// delete one.
    pub fn list_keys(&self, upstream_id: &str) -> rusqlite::Result<Vec<KeyRow>> {
        let conn = self.conn.lock();
        let mut statement = conn.prepare(
            "SELECT id, api_key, added_at FROM upstream_key WHERE upstream_id = ?1 ORDER BY id",
        )?;
        let rows = statement
            .query_map([upstream_id], |row| {
                let key: String = row.get(1)?;
                Ok(KeyRow {
                    id: row.get(0)?,
                    masked: vd_llm::mask_key(&key),
                    added_at: row.get(2)?,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    /// Add a key. The same key twice is not an error and not a second key.
    pub fn add_key(&self, upstream_id: &str, key: &str, now: i64) -> rusqlite::Result<()> {
        self.conn.lock().execute(
            "INSERT OR IGNORE INTO upstream_key (upstream_id, api_key, added_at)
             VALUES (?1, ?2, ?3)",
            rusqlite::params![upstream_id, key.trim(), now],
        )?;
        Ok(())
    }

    pub fn delete_key(&self, id: i64) -> rusqlite::Result<()> {
        self.conn
            .lock()
            .execute("DELETE FROM upstream_key WHERE id = ?1", [id])?;
        Ok(())
    }

    // --------------------------------------------------------------- models

    pub fn list_models(&self) -> rusqlite::Result<Vec<ModelRow>> {
        let conn = self.conn.lock();
        let mut statement = conn.prepare(
            "SELECT name, upstream_id, upstream_name, price_in, price_cached, price_out,
                    context_tokens, enabled, position, voice, price_request
             FROM model ORDER BY position, name",
        )?;
        let rows = statement
            .query_map([], |row| {
                Ok(ModelRow {
                    name: row.get(0)?,
                    upstream_id: row.get(1)?,
                    upstream_name: row.get(2)?,
                    price_in: row.get(3)?,
                    price_cached: row.get(4)?,
                    price_out: row.get(5)?,
                    context_tokens: row.get::<_, Option<i64>>(6)?.map(|n| n as u32),
                    enabled: row.get::<_, i64>(7)? != 0,
                    position: row.get(8)?,
                    voice: row.get::<_, i64>(9)? != 0,
                    price_request: row.get(10)?,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    pub fn save_model(&self, model: &ModelRow) -> rusqlite::Result<()> {
        self.conn.lock().execute(
            "INSERT INTO model (name, upstream_id, upstream_name, price_in, price_cached,
                                price_out, context_tokens, enabled, position, voice,
                                price_request)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)
             ON CONFLICT(name) DO UPDATE SET
                 upstream_id = excluded.upstream_id,
                 upstream_name = excluded.upstream_name,
                 price_in = excluded.price_in,
                 price_cached = excluded.price_cached,
                 price_out = excluded.price_out,
                 context_tokens = excluded.context_tokens,
                 enabled = excluded.enabled,
                 position = excluded.position,
                 voice = excluded.voice,
                 price_request = excluded.price_request",
            rusqlite::params![
                model.name,
                model.upstream_id,
                model.upstream_name,
                model.price_in,
                model.price_cached,
                model.price_out,
                model.context_tokens.map(|n| n as i64),
                model.enabled as i64,
                model.position,
                model.voice as i64,
                model.price_request,
            ],
        )?;
        Ok(())
    }

    pub fn delete_model(&self, name: &str) -> rusqlite::Result<()> {
        self.conn
            .lock()
            .execute("DELETE FROM model WHERE name = ?1", [name])?;
        Ok(())
    }

    // ---------------------------------------------------------------- tiers

    pub fn list_tiers(&self) -> rusqlite::Result<HashMap<String, Tier>> {
        let conn = self.conn.lock();
        let mut statement =
            conn.prepare("SELECT name, credits_5h, credits_week, max_peers FROM tier")?;
        let rows = statement
            .query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    Tier {
                        credits_5h: row.get(1)?,
                        credits_week: row.get(2)?,
                        max_peers: row.get::<_, i64>(3)? as u32,
                    },
                ))
            })?
            .collect::<rusqlite::Result<HashMap<_, _>>>()?;
        Ok(rows)
    }

    pub fn save_tier(&self, name: &str, tier: &Tier) -> rusqlite::Result<()> {
        self.conn.lock().execute(
            "INSERT INTO tier (name, credits_5h, credits_week, max_peers)
             VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT(name) DO UPDATE SET
                 credits_5h = excluded.credits_5h,
                 credits_week = excluded.credits_week,
                 max_peers = excluded.max_peers",
            rusqlite::params![name, tier.credits_5h, tier.credits_week, tier.max_peers],
        )?;
        Ok(())
    }

    pub fn delete_tier(&self, name: &str) -> rusqlite::Result<()> {
        self.conn
            .lock()
            .execute("DELETE FROM tier WHERE name = ?1", [name])?;
        Ok(())
    }

    // ------------------------------------------------------------- licences

    /// Write down a licence that was issued. The licence proves itself; this
    /// is so an operator can be found later without asking them for it.
    pub fn record_license(
        &self,
        license_id: &str,
        tier: &str,
        expires_at: i64,
        max_peers: u32,
        note: &str,
        now: i64,
    ) -> rusqlite::Result<()> {
        self.conn.lock().execute(
            "INSERT INTO license (license_id, tier, expires_at, max_peers, issued_at, note)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)
             ON CONFLICT(license_id) DO UPDATE SET
                 tier = excluded.tier,
                 expires_at = excluded.expires_at,
                 max_peers = excluded.max_peers,
                 note = excluded.note",
            rusqlite::params![license_id, tier, expires_at, max_peers, now, note],
        )?;
        Ok(())
    }

    pub fn list_licenses(&self) -> rusqlite::Result<Vec<LicenseRow>> {
        let conn = self.conn.lock();
        let mut statement = conn.prepare(
            "SELECT l.license_id, l.tier, l.expires_at, l.max_peers, l.issued_at, l.note,
                    (SELECT COUNT(*) FROM revoked r WHERE r.license_id = l.license_id)
             FROM license l ORDER BY l.issued_at DESC",
        )?;
        let rows = statement
            .query_map([], |row| {
                Ok(LicenseRow {
                    license_id: row.get(0)?,
                    tier: row.get(1)?,
                    expires_at: row.get(2)?,
                    max_peers: row.get::<_, i64>(3)? as u32,
                    issued_at: row.get(4)?,
                    note: row.get(5)?,
                    revoked: row.get::<_, i64>(6)? > 0,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    pub fn unrevoke(&self, license_id: &str) -> rusqlite::Result<()> {
        self.conn
            .lock()
            .execute("DELETE FROM revoked WHERE license_id = ?1", [license_id])?;
        Ok(())
    }

    // ------------------------------------------------------------ reporting

    /// What each licence has spent since a moment, busiest first.
    pub fn usage_by_license(&self, since: i64) -> rusqlite::Result<Vec<(String, f64, i64)>> {
        let conn = self.conn.lock();
        let mut statement = conn.prepare(
            "SELECT license_id, COALESCE(SUM(credits), 0), COUNT(*)
             FROM usage WHERE ts >= ?1 GROUP BY license_id ORDER BY 2 DESC LIMIT 200",
        )?;
        let rows = statement
            .query_map([since], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    /// What each model has cost since a moment, and how much of its prompt
    /// came out of a cache.
    pub fn usage_by_model(&self, since: i64) -> rusqlite::Result<Vec<(String, f64, i64, i64)>> {
        let conn = self.conn.lock();
        let mut statement = conn.prepare(
            "SELECT model, COALESCE(SUM(credits), 0), COALESCE(SUM(prompt_tokens), 0),
                    COALESCE(SUM(cached_tokens), 0)
             FROM usage WHERE ts >= ?1 GROUP BY model ORDER BY 2 DESC LIMIT 200",
        )?;
        let rows = statement
            .query_map([since], |row| {
                Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
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
