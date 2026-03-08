use super::{
    now_ts, ExpandEvent, FeedbackMetrics, QueryEvent, QueryHandle, EXPAND_EVENTS_CAP,
    QUERY_EVENTS_CAP, RETENTION_DAYS, TOP_K_GLOBS,
};
use crate::NodeType;
use rusqlite::{params, Connection};
use std::collections::HashMap;
use std::fs;
use std::path::Path;

pub struct FeedbackStore {
    pub(super) conn: Connection,
}

impl FeedbackStore {
    pub fn open(repo_root: &Path) -> crate::Result<Self> {
        let canopy_dir = repo_root.join(".canopy");
        fs::create_dir_all(&canopy_dir)?;
        let db_path = canopy_dir.join("feedback.db");
        let conn = Connection::open(db_path)?;

        conn.execute_batch(
            "
            PRAGMA journal_mode = WAL;
            PRAGMA busy_timeout = 5000;
            PRAGMA foreign_keys = ON;
            PRAGMA journal_size_limit = 67108864;

            CREATE TABLE IF NOT EXISTS query_events (
                id INTEGER PRIMARY KEY,
                timestamp INTEGER NOT NULL,
                query_text TEXT NOT NULL,
                predicted_globs TEXT,
                files_indexed INTEGER DEFAULT 0,
                handles_returned INTEGER DEFAULT 0,
                total_tokens INTEGER DEFAULT 0
            );

            CREATE TABLE IF NOT EXISTS query_handles (
                id INTEGER PRIMARY KEY,
                query_event_id INTEGER NOT NULL REFERENCES query_events(id) ON DELETE CASCADE,
                handle_id TEXT NOT NULL,
                file_path TEXT NOT NULL,
                node_type INTEGER NOT NULL,
                token_count INTEGER NOT NULL,
                first_match_glob TEXT,
                returned_at INTEGER NOT NULL
            );

            CREATE TABLE IF NOT EXISTS expand_events (
                id INTEGER PRIMARY KEY,
                query_event_id INTEGER REFERENCES query_events(id) ON DELETE SET NULL,
                handle_id TEXT NOT NULL,
                file_path TEXT NOT NULL,
                node_type INTEGER NOT NULL,
                token_count INTEGER NOT NULL,
                auto_expanded INTEGER NOT NULL DEFAULT 0,
                expanded_at INTEGER NOT NULL
            );

            CREATE INDEX IF NOT EXISTS idx_query_handles_handle ON query_handles(handle_id);
            CREATE INDEX IF NOT EXISTS idx_query_handles_glob ON query_handles(first_match_glob);
            CREATE INDEX IF NOT EXISTS idx_expand_events_query_event ON expand_events(query_event_id);
            CREATE INDEX IF NOT EXISTS idx_expand_events_handle ON expand_events(handle_id);
            CREATE INDEX IF NOT EXISTS idx_query_events_ts ON query_events(timestamp);
            CREATE INDEX IF NOT EXISTS idx_expand_events_ts ON expand_events(expanded_at);
            ",
        )?;

        let store = Self { conn };
        store.prune()?;
        Ok(store)
    }

    pub fn record_query_event(&self, event: &QueryEvent) -> crate::Result<i64> {
        let predicted_globs = match &event.predicted_globs {
            Some(globs) if !globs.is_empty() => Some(serde_json::to_string(globs)?),
            _ => None,
        };

        self.conn.execute(
            "INSERT INTO query_events (timestamp, query_text, predicted_globs, files_indexed, handles_returned, total_tokens)
             VALUES (?, ?, ?, ?, ?, ?)",
            params![
                now_ts(),
                event.query_text,
                predicted_globs,
                event.files_indexed as i64,
                event.handles_returned as i64,
                event.total_tokens as i64,
            ],
        )?;

        Ok(self.conn.last_insert_rowid())
    }

    pub fn record_query_handles(
        &self,
        query_event_id: i64,
        handles: &[QueryHandle],
    ) -> crate::Result<()> {
        if handles.is_empty() {
            return Ok(());
        }

        let ts = now_ts();
        let mut stmt = self.conn.prepare(
            "INSERT INTO query_handles
             (query_event_id, handle_id, file_path, node_type, token_count, first_match_glob, returned_at)
             VALUES (?, ?, ?, ?, ?, ?, ?)",
        )?;

        for handle in handles {
            stmt.execute(params![
                query_event_id,
                handle.handle_id,
                handle.file_path,
                handle.node_type.as_int() as i64,
                handle.token_count as i64,
                handle.first_match_glob,
                ts,
            ])?;
        }

        Ok(())
    }

    pub fn record_expand_event(&self, event: &ExpandEvent) -> crate::Result<()> {
        self.conn.execute(
            "INSERT INTO expand_events
             (query_event_id, handle_id, file_path, node_type, token_count, auto_expanded, expanded_at)
             VALUES (?, ?, ?, ?, ?, ?, ?)",
            params![
                event.query_event_id,
                event.handle_id,
                event.file_path,
                event.node_type.as_int() as i64,
                event.token_count as i64,
                if event.auto_expanded { 1 } else { 0 },
                now_ts(),
            ],
        )?;
        Ok(())
    }

    pub fn get_glob_scores(
        &self,
        globs: &[String],
        half_life_days: f64,
    ) -> crate::Result<HashMap<String, f64>> {
        let mut scores = HashMap::new();
        if globs.is_empty() {
            return Ok(scores);
        }

        let now = now_ts();
        let half_life_secs = (half_life_days * 86_400.0).max(1.0);

        let mut stmt = self.conn.prepare(
            "SELECT qe.timestamp,
                    CASE
                        WHEN EXISTS (
                            SELECT 1
                            FROM expand_events ee
                            WHERE ee.query_event_id = qh.query_event_id
                              AND ee.handle_id = qh.handle_id
                        ) THEN 1 ELSE 0
                    END AS expanded
             FROM query_handles qh
             JOIN query_events qe ON qe.id = qh.query_event_id
             WHERE qh.first_match_glob = ?",
        )?;

        for glob in globs {
            let mut returned_weight = 0.0f64;
            let mut expanded_weight = 0.0f64;

            let rows = stmt.query_map(params![glob], |row| {
                let ts: i64 = row.get(0)?;
                let expanded: i64 = row.get(1)?;
                Ok((ts, expanded))
            })?;

            for row in rows {
                let (ts, expanded) = row?;
                let age_secs = (now - ts).max(0) as f64;
                let decay = (-age_secs * std::f64::consts::LN_2 / half_life_secs).exp();
                returned_weight += decay;
                if expanded > 0 {
                    expanded_weight += decay;
                }
            }

            if returned_weight > 0.0 {
                scores.insert(glob.clone(), expanded_weight / returned_weight);
            }
        }

        Ok(scores)
    }

    pub fn get_node_type_priors(&self) -> crate::Result<HashMap<NodeType, f64>> {
        let mut priors = HashMap::new();
        let mut stmt = self.conn.prepare(
            "SELECT qh.node_type,
                    COUNT(*) AS returned_count,
                    SUM(
                        CASE
                            WHEN EXISTS (
                                SELECT 1
                                FROM expand_events ee
                                WHERE ee.query_event_id = qh.query_event_id
                                  AND ee.handle_id = qh.handle_id
                            ) THEN 1 ELSE 0
                        END
                    ) AS expanded_count
             FROM query_handles qh
             GROUP BY qh.node_type",
        )?;

        let rows = stmt.query_map([], |row| {
            let node_type: i64 = row.get(0)?;
            let returned_count: i64 = row.get(1)?;
            let expanded_count: i64 = row.get(2)?;
            Ok((node_type, returned_count, expanded_count))
        })?;

        for row in rows {
            let (node_type, returned_count, expanded_count) = row?;
            if returned_count <= 0 {
                continue;
            }
            if let Some(kind) = NodeType::from_int(node_type as u8) {
                priors.insert(kind, expanded_count as f64 / returned_count as f64);
            }
        }

        Ok(priors)
    }

    pub fn compute_metrics(&self, lookback_days: f64) -> crate::Result<FeedbackMetrics> {
        let cutoff = now_ts() - (lookback_days.max(0.0) * 86_400.0) as i64;

        let sample_count: i64 = self.conn.query_row(
            "SELECT COUNT(*) FROM query_events WHERE timestamp >= ?",
            params![cutoff],
            |row| row.get(0),
        )?;

        let (returned_count, expanded_count): (i64, i64) = self.conn.query_row(
            "SELECT
                COUNT(*),
                SUM(
                    CASE
                        WHEN EXISTS (
                            SELECT 1
                            FROM expand_events ee
                            WHERE ee.query_event_id = qh.query_event_id
                              AND ee.handle_id = qh.handle_id
                        ) THEN 1 ELSE 0
                    END
                )
             FROM query_handles qh
             JOIN query_events qe ON qe.id = qh.query_event_id
             WHERE qe.timestamp >= ?",
            params![cutoff],
            |row| Ok((row.get(0)?, row.get::<_, Option<i64>>(1)?.unwrap_or(0))),
        )?;

        let avg_tokens_per_expand: Option<f64> = self.conn.query_row(
            "SELECT AVG(token_count)
             FROM expand_events
             WHERE expanded_at >= ?",
            params![cutoff],
            |row| row.get(0),
        )?;

        let mut glob_denominator = 0usize;
        let mut glob_hits = 0usize;

        let mut stmt_events = self.conn.prepare(
            "SELECT id, predicted_globs
             FROM query_events
             WHERE timestamp >= ?
               AND predicted_globs IS NOT NULL",
        )?;
        let event_rows = stmt_events.query_map(params![cutoff], |row| {
            let id: i64 = row.get(0)?;
            let predicted_globs: String = row.get(1)?;
            Ok((id, predicted_globs))
        })?;

        let mut stmt_hit = self.conn.prepare(
            "SELECT 1
             FROM query_handles qh
             WHERE qh.query_event_id = ?
               AND qh.first_match_glob = ?
               AND EXISTS (
                    SELECT 1
                    FROM expand_events ee
                    WHERE ee.query_event_id = qh.query_event_id
                      AND ee.handle_id = qh.handle_id
               )
             LIMIT 1",
        )?;

        for event in event_rows {
            let (query_event_id, predicted_globs_json) = event?;
            let globs: Vec<String> =
                serde_json::from_str(&predicted_globs_json).unwrap_or_default();
            for glob in globs.into_iter().take(TOP_K_GLOBS) {
                glob_denominator += 1;
                let has_hit = stmt_hit.exists(params![query_event_id, glob])?;
                if has_hit {
                    glob_hits += 1;
                }
            }
        }

        let handle_expand_accept_rate = if returned_count > 0 {
            expanded_count as f64 / returned_count as f64
        } else {
            0.0
        };
        let glob_hit_rate_at_k = if glob_denominator > 0 {
            glob_hits as f64 / glob_denominator as f64
        } else {
            0.0
        };

        Ok(FeedbackMetrics {
            glob_hit_rate_at_k,
            handle_expand_accept_rate,
            avg_tokens_per_expand: avg_tokens_per_expand.unwrap_or(0.0),
            sample_count: sample_count.max(0) as usize,
        })
    }

    pub(super) fn prune(&self) -> crate::Result<()> {
        let cutoff = now_ts() - RETENTION_DAYS * 86_400;
        self.conn.execute(
            "DELETE FROM query_events WHERE timestamp < ?",
            params![cutoff],
        )?;
        self.conn.execute(
            "DELETE FROM expand_events WHERE expanded_at < ?",
            params![cutoff],
        )?;

        self.conn.execute(
            "DELETE FROM query_events
             WHERE id IN (
                SELECT id
                FROM query_events
                ORDER BY timestamp ASC
                LIMIT (
                    SELECT CASE WHEN COUNT(*) > ? THEN COUNT(*) - ? ELSE 0 END
                    FROM query_events
                )
             )",
            params![QUERY_EVENTS_CAP, QUERY_EVENTS_CAP],
        )?;

        self.conn.execute(
            "DELETE FROM expand_events
             WHERE id IN (
                SELECT id
                FROM expand_events
                ORDER BY expanded_at ASC
                LIMIT (
                    SELECT CASE WHEN COUNT(*) > ? THEN COUNT(*) - ? ELSE 0 END
                    FROM expand_events
                )
             )",
            params![EXPAND_EVENTS_CAP, EXPAND_EVENTS_CAP],
        )?;

        Ok(())
    }
}
