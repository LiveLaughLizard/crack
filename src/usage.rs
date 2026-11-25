//! A log of every model request: tokens and, when the price is known, cost. Feeds the
//! session cost in the sidebar, `crack usage`, `/usage`, and `--max-cost`.

use std::path::Path;
use std::sync::{Mutex, OnceLock};

use rusqlite::{Connection, params};

use crate::providers::Usage;

const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS usage_log (
    id INTEGER PRIMARY KEY, at INTEGER NOT NULL, session TEXT NOT NULL, project TEXT NOT NULL,
    model TEXT NOT NULL, input INTEGER NOT NULL, output INTEGER NOT NULL,
    cache_read INTEGER NOT NULL, cache_write INTEGER NOT NULL, cost REAL);
CREATE INDEX IF NOT EXISTS usage_log_session ON usage_log (session);
CREATE INDEX IF NOT EXISTS usage_log_at ON usage_log (at);
";

pub struct UsageLog {
    db: Mutex<Connection>,
}

/// One line of a usage report.
#[derive(Debug, Default, PartialEq)]
pub struct Total {
    pub label: String,
    pub requests: u64,
    pub input: u64,
    pub output: u64,
    /// Input tokens read from the prompt cache (part of `input`).
    pub cached: u64,
    pub cost: f64,
    /// Requests whose price wasn't known.
    pub unpriced: u64,
}

impl UsageLog {
    pub fn open_at(path: &Path) -> anyhow::Result<UsageLog> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let db = Connection::open(path)?;
        db.busy_timeout(std::time::Duration::from_secs(10))?;
        db.pragma_update(None, "journal_mode", "WAL")?;
        db.execute_batch(SCHEMA)?;
        Ok(UsageLog { db: Mutex::new(db) })
    }

    fn db(&self) -> std::sync::MutexGuard<'_, Connection> {
        self.db.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub fn record(
        &self,
        session: &str,
        project: &str,
        model: &str,
        usage: &Usage,
        cost: Option<f64>,
    ) {
        let _ = self.db().execute(
            "INSERT INTO usage_log (at, session, project, model, input, output, cache_read, cache_write, cost)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
            params![
                chrono::Utc::now().timestamp(),
                session,
                project,
                model,
                usage.input_tokens as i64,
                usage.output_tokens as i64,
                usage.cache_read_tokens as i64,
                usage.cache_write_tokens as i64,
                cost
            ],
        );
    }

    /// Models with sessions of 5+ requests in the last week that never read from the
    /// prompt cache: a sign the provider isn't caching for crack.
    pub fn never_cached(&self) -> Vec<String> {
        let since = chrono::Utc::now().timestamp() - 7 * 86_400;
        let db = self.db();
        let Ok(mut statement) = db.prepare(
            "SELECT model FROM (
                 SELECT model, session, COUNT(*) AS n, SUM(cache_read) AS hits
                 FROM usage_log WHERE at >= ?1 GROUP BY model, session)
             GROUP BY model HAVING MAX(n) >= 5 AND SUM(hits) = 0 ORDER BY model",
        ) else {
            return Vec::new();
        };
        statement
            .query_map(params![since], |r| r.get(0))
            .map(|rows| rows.flatten().collect())
            .unwrap_or_default()
    }

    /// What a session (and its subagents, logged under it) has cost so far.
    pub fn session_cost(&self, session: &str) -> f64 {
        self.db()
            .query_row(
                "SELECT COALESCE(SUM(cost), 0) FROM usage_log WHERE session = ?1",
                params![session],
                |r| r.get(0),
            )
            .unwrap_or(0.0)
    }

    /// Totals since `since` (unix seconds), grouped by `group` ("model" or "day"),
    /// optionally for one project.
    pub fn totals(
        &self,
        since: i64,
        group: &str,
        project: Option<&str>,
    ) -> anyhow::Result<Vec<Total>> {
        let key = match group {
            "day" => "date(at, 'unixepoch', 'localtime')",
            "project" => "project",
            _ => "model",
        };
        let db = self.db();
        let mut statement = db.prepare(&format!(
            "SELECT {key}, COUNT(*), SUM(input + cache_read + cache_write), SUM(output), COALESCE(SUM(cost), 0),
                    SUM(cost IS NULL), SUM(cache_read)
             FROM usage_log WHERE at >= ?1 AND (?2 IS NULL OR project = ?2)
             GROUP BY 1 ORDER BY 5 DESC, 1"
        ))?;
        let rows = statement.query_map(params![since, project], |r| {
            Ok(Total {
                label: r.get(0)?,
                requests: r.get::<_, i64>(1)? as u64,
                input: r.get::<_, i64>(2)? as u64,
                output: r.get::<_, i64>(3)? as u64,
                cost: r.get(4)?,
                unpriced: r.get::<_, i64>(5)? as u64,
                cached: r.get::<_, Option<i64>>(6)?.unwrap_or(0) as u64,
            })
        })?;
        Ok(rows.flatten().collect())
    }
}

pub fn get() -> &'static UsageLog {
    static LOG: OnceLock<UsageLog> = OnceLock::new();
    LOG.get_or_init(|| {
        UsageLog::open_at(&crate::config::data_dir().join("crack.db"))
            .expect("couldn't open the usage log")
    })
}

/// A plain-text report: by model, then by day.
pub fn report(days: i64, project: Option<&str>) -> String {
    let since = chrono::Utc::now().timestamp() - days * 86_400;
    let log = get();
    let fmt = |t: &Total| {
        let cost = if t.unpriced == t.requests {
            "      ?".to_owned()
        } else if t.unpriced > 0 {
            format!("${:>7.2}+", t.cost)
        } else {
            format!("${:>7.2}", t.cost)
        };
        format!(
            "{:<34} {:>6} req  {:>11} in  {:>4} cached  {:>10} out  {cost}",
            t.label,
            t.requests,
            crate::ui::render::thousands(t.input),
            format!("{}%", (t.cached * 100).checked_div(t.input).unwrap_or(0)),
            crate::ui::render::thousands(t.output)
        )
    };
    let mut out = format!(
        "Usage over the last {days} days{}:\n",
        project.map(|p| format!(" in {p}")).unwrap_or_default()
    );
    let by_model = log.totals(since, "model", project).unwrap_or_default();
    if by_model.is_empty() {
        out.push_str("\nNo model requests yet.\n");
        return out;
    }
    let total: f64 = by_model.iter().map(|t| t.cost).sum();
    out.push_str("\nBy model\n");
    for t in &by_model {
        out.push_str(&format!("  {}\n", fmt(t)));
    }
    out.push_str("\nBy day\n");
    for t in log.totals(since, "day", project).unwrap_or_default() {
        out.push_str(&format!("  {}\n", fmt(&t)));
    }
    out.push_str(&format!("\nTotal: ${total:.2}"));
    if by_model.iter().any(|t| t.unpriced > 0) {
        out.push_str(" (+ requests to models without a known price; set prices with [models.\"provider/model\"])");
    }
    out.push('\n');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn records_and_totals() {
        let path = std::env::temp_dir().join(format!("crack-usage-{}.db", uuid::Uuid::new_v4()));
        let log = UsageLog::open_at(&path).unwrap();
        let usage = Usage {
            input_tokens: 1000,
            output_tokens: 200,
            cache_read_tokens: 0,
            cache_write_tokens: 0,
        };
        log.record("s1", "/p", "anthropic/claude-opus-5-5", &usage, Some(0.008));
        log.record("s1", "/p", "anthropic/claude-opus-5-5", &usage, Some(0.008));
        log.record("s2", "/q", "ollama/llama", &usage, None);
        assert!((log.session_cost("s1") - 0.016).abs() < 1e-9);
        let by_model = log.totals(0, "model", None).unwrap();
        assert_eq!(by_model[0].label, "anthropic/claude-opus-5-5");
        assert_eq!(by_model[0].requests, 2);
        assert_eq!(by_model[1].unpriced, 1);
        assert_eq!(log.totals(0, "model", Some("/q")).unwrap().len(), 1);
        // s1 has 2 requests: too few to judge. Five uncached requests are flagged.
        assert!(log.never_cached().is_empty());
        for _ in 0..5 {
            log.record("s3", "/p", "zai/glm", &usage, None);
        }
        assert_eq!(log.never_cached(), vec!["zai/glm".to_owned()]);
        let _ = std::fs::remove_file(path);
    }
}
