//! Project memory: small facts the model or the user saves, recalled in later sessions.
//!
//! Memories belong to a project root (or are global, for preferences everywhere) and
//! live in SQLite with a full-text index. Before each turn the most relevant ones
//! (pinned, plus matches for the prompt) are added to the user's message, within a
//! budget, so the system prompt stays the same and caches well. The model's own
//! memories are flagged until the user reviews them.

use std::path::Path;
use std::sync::{Mutex, OnceLock};

use rusqlite::{Connection, OptionalExtension, params};

pub const KINDS: &[&str] = &["preference", "project", "decision", "gotcha"];

#[derive(Clone, Debug, PartialEq)]
pub struct Memory {
    pub id: i64,
    /// Empty for global memories.
    pub project: String,
    pub kind: String,
    pub text: String,
    pub reason: String,
    pub session: String,
    pub created: i64,
    pub pinned: bool,
    /// The user has seen it (always true for memories the user wrote).
    pub reviewed: bool,
}

const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS memories (
    id INTEGER PRIMARY KEY, project TEXT NOT NULL, kind TEXT NOT NULL, text TEXT NOT NULL,
    reason TEXT NOT NULL DEFAULT '', session TEXT NOT NULL DEFAULT '',
    created INTEGER NOT NULL, updated INTEGER NOT NULL,
    pinned INTEGER NOT NULL DEFAULT 0, reviewed INTEGER NOT NULL DEFAULT 1);
CREATE INDEX IF NOT EXISTS memories_project ON memories (project);
CREATE VIRTUAL TABLE IF NOT EXISTS memories_fts USING fts5(text, kind, content='memories', content_rowid='id');
CREATE TRIGGER IF NOT EXISTS memories_ai AFTER INSERT ON memories BEGIN
    INSERT INTO memories_fts(rowid, text, kind) VALUES (new.id, new.text, new.kind); END;
CREATE TRIGGER IF NOT EXISTS memories_ad AFTER DELETE ON memories BEGIN
    INSERT INTO memories_fts(memories_fts, rowid, text, kind) VALUES ('delete', old.id, old.text, old.kind); END;
CREATE TRIGGER IF NOT EXISTS memories_au AFTER UPDATE ON memories BEGIN
    INSERT INTO memories_fts(memories_fts, rowid, text, kind) VALUES ('delete', old.id, old.text, old.kind);
    INSERT INTO memories_fts(rowid, text, kind) VALUES (new.id, new.text, new.kind); END;
";

pub struct Memories {
    db: Mutex<Connection>,
}

fn now() -> i64 {
    chrono::Utc::now().timestamp()
}

fn row(r: &rusqlite::Row) -> rusqlite::Result<Memory> {
    Ok(Memory {
        id: r.get(0)?,
        project: r.get(1)?,
        kind: r.get(2)?,
        text: r.get(3)?,
        reason: r.get(4)?,
        session: r.get(5)?,
        created: r.get(6)?,
        pinned: r.get::<_, i64>(7)? != 0,
        reviewed: r.get::<_, i64>(8)? != 0,
    })
}

const COLUMNS: &str =
    "m.id, m.project, m.kind, m.text, m.reason, m.session, m.created, m.pinned, m.reviewed";

/// The project key: the project root as text.
pub fn project_key(cwd: &Path) -> String {
    crate::context::project_root(cwd).display().to_string()
}

impl Memories {
    pub fn open_at(path: &Path) -> anyhow::Result<Memories> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let db = Connection::open(path)?;
        db.busy_timeout(std::time::Duration::from_secs(10))?;
        db.pragma_update(None, "journal_mode", "WAL")?;
        db.execute_batch(SCHEMA)?;
        Ok(Memories { db: Mutex::new(db) })
    }

    fn db(&self) -> std::sync::MutexGuard<'_, Connection> {
        self.db.lock().unwrap_or_else(|e| e.into_inner())
    }

    #[allow(clippy::too_many_arguments)]
    pub fn add(
        &self,
        project: &str,
        kind: &str,
        text: &str,
        reason: &str,
        session: &str,
        by_model: bool,
    ) -> anyhow::Result<i64> {
        let kind = if KINDS.contains(&kind) {
            kind
        } else {
            "project"
        };
        let text = text.trim();
        if text.is_empty() {
            anyhow::bail!("a memory needs text");
        }
        let db = self.db();
        // Saying the same thing twice updates it instead of duplicating it.
        if let Some(id) = db
            .query_row(
                "SELECT id FROM memories WHERE project = ?1 AND text = ?2",
                params![project, text],
                |r| r.get::<_, i64>(0),
            )
            .optional()?
        {
            db.execute(
                "UPDATE memories SET updated = ?2, kind = ?3 WHERE id = ?1",
                params![id, now(), kind],
            )?;
            return Ok(id);
        }
        db.execute(
            "INSERT INTO memories (project, kind, text, reason, session, created, updated, reviewed)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?6, ?7)",
            params![project, kind, text, reason, session, now(), !by_model as i64],
        )?;
        Ok(db.last_insert_rowid())
    }

    /// This project's memories and the global ones, newest first.
    pub fn list(&self, project: &str) -> anyhow::Result<Vec<Memory>> {
        let db = self.db();
        let mut statement = db.prepare(&format!(
            "SELECT {COLUMNS} FROM memories m WHERE m.project = ?1 OR m.project = ''
             ORDER BY m.reviewed, m.pinned DESC, m.updated DESC"
        ))?;
        let rows = statement.query_map(params![project], row)?;
        Ok(rows.flatten().collect())
    }

    /// Full-text search within this project (and global memories).
    pub fn search(&self, project: &str, query: &str, limit: usize) -> anyhow::Result<Vec<Memory>> {
        let words: Vec<String> = query
            .split(|c: char| !c.is_alphanumeric())
            .filter(|w| w.len() > 2)
            .map(|w| format!("\"{}\"*", w.to_lowercase()))
            .collect();
        if words.is_empty() {
            return Ok(Vec::new());
        }
        let db = self.db();
        let mut statement = db.prepare(&format!(
            "SELECT {COLUMNS} FROM memories_fts f JOIN memories m ON m.id = f.rowid
             WHERE memories_fts MATCH ?2 AND (m.project = ?1 OR m.project = '')
             ORDER BY bm25(memories_fts) LIMIT ?3"
        ))?;
        let rows = statement.query_map(params![project, words.join(" OR "), limit as i64], row)?;
        Ok(rows.flatten().collect())
    }

    pub fn get(&self, id: i64) -> anyhow::Result<Option<Memory>> {
        Ok(self
            .db()
            .query_row(
                &format!("SELECT {COLUMNS} FROM memories m WHERE m.id = ?1"),
                params![id],
                row,
            )
            .optional()?)
    }

    pub fn set_pinned(&self, id: i64, pinned: bool) -> anyhow::Result<()> {
        self.db().execute(
            "UPDATE memories SET pinned = ?2, reviewed = 1 WHERE id = ?1",
            params![id, pinned as i64],
        )?;
        Ok(())
    }

    pub fn mark_reviewed(&self, id: i64) -> anyhow::Result<()> {
        self.db().execute(
            "UPDATE memories SET reviewed = 1 WHERE id = ?1",
            params![id],
        )?;
        Ok(())
    }

    #[allow(dead_code)] // In-place edits; the panel edits by re-saving.
    pub fn update_text(&self, id: i64, text: &str) -> anyhow::Result<()> {
        self.db().execute(
            "UPDATE memories SET text = ?2, updated = ?3, reviewed = 1 WHERE id = ?1",
            params![id, text.trim(), now()],
        )?;
        Ok(())
    }

    pub fn delete(&self, id: i64) -> anyhow::Result<()> {
        self.db()
            .execute("DELETE FROM memories WHERE id = ?1", params![id])?;
        Ok(())
    }

    /// What to remind the model of for this prompt: pinned memories, then the best
    /// matches, within about `budget` characters.
    pub fn relevant(
        &self,
        project: &str,
        prompt: &str,
        budget: usize,
    ) -> anyhow::Result<Vec<Memory>> {
        let mut chosen: Vec<Memory> = self
            .list(project)?
            .into_iter()
            .filter(|m| m.pinned)
            .collect();
        for m in self.search(project, prompt, 12)? {
            if !chosen.iter().any(|c| c.id == m.id) {
                chosen.push(m);
            }
        }
        let mut used = 0;
        chosen.retain(|m| {
            used += m.text.len() + 20;
            used <= budget
        });
        Ok(chosen)
    }

    /// Writes this project's memories as markdown, for a team to review and commit.
    pub fn export(&self, project: &str) -> anyhow::Result<String> {
        let mut out = String::from(
            "# Project memory\n\nFacts crack remembers about this project. Edit freely; this file is a snapshot.\n",
        );
        for kind in KINDS {
            let items: Vec<Memory> = self
                .list(project)?
                .into_iter()
                .filter(|m| m.project == project && m.kind == *kind)
                .collect();
            if items.is_empty() {
                continue;
            }
            out.push_str(&format!(
                "\n## {}{}\n\n",
                kind[..1].to_uppercase(),
                &kind[1..]
            ));
            for m in items {
                out.push_str(&format!(
                    "- {}{}\n",
                    m.text,
                    if m.pinned { " (pinned)" } else { "" }
                ));
            }
        }
        Ok(out)
    }
}

/// The shared memory store (in crack's database).
pub fn get() -> &'static Memories {
    static MEMORIES: OnceLock<Memories> = OnceLock::new();
    MEMORIES.get_or_init(|| {
        Memories::open_at(&crate::config::data_dir().join("crack.db"))
            .expect("couldn't open the memory database")
    })
}

/// Text for the start of a turn: the memories that apply.
pub fn reminder(memories: &[Memory]) -> Option<String> {
    if memories.is_empty() {
        return None;
    }
    let mut out = String::from(
        "<memory>\nThings you saved about this project in earlier sessions (verify before relying on details that may have changed):\n",
    );
    for m in memories {
        out.push_str(&format!("- [{}] {}\n", m.kind, m.text));
    }
    out.push_str("</memory>");
    Some(out)
}

pub fn specs() -> Vec<crate::providers::ToolSpec> {
    use serde_json::json;
    vec![
        crate::providers::ToolSpec {
            name: "remember".into(),
            description: "Save a fact for future sessions in this project: a user preference or correction, how the \
                project works, a decision and why, or a gotcha that cost time. Save what would help a future session; \
                not things already in AGENTS.md or obvious from the code, and never secrets. Keep it to one or two \
                sentences. The user can review and delete memories."
                .into(),
            schema: json!({"type": "object", "properties": {
                "text": {"type": "string", "description": "The fact, self-contained."},
                "kind": {"type": "string", "enum": KINDS},
                "reason": {"type": "string", "description": "Why it's worth remembering."},
                "global": {"type": "boolean", "description": "A personal preference that applies in every project."}
            }, "required": ["text", "kind"]}),
        },
        crate::providers::ToolSpec {
            name: "recall".into(),
            description: "Search what was saved in earlier sessions about this project (relevant memories are also shown \
                automatically at the start of each request)."
                .into(),
            schema: json!({"type": "object", "properties": {"query": {"type": "string"}}, "required": ["query"]}),
        },
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn remember_recall_and_isolation() {
        let path = std::env::temp_dir().join(format!("crack-memory-{}.db", uuid::Uuid::new_v4()));
        let m = Memories::open_at(&path).unwrap();
        let a = m
            .add(
                "/proj/a",
                "preference",
                "Use tabs, never spaces, in the Makefile.",
                "user corrected me",
                "s1",
                true,
            )
            .unwrap();
        m.add(
            "/proj/a",
            "gotcha",
            "Integration tests need DATABASE_URL set.",
            "",
            "s1",
            true,
        )
        .unwrap();
        m.add(
            "/proj/b",
            "project",
            "Project B deploys with fly.io.",
            "",
            "s2",
            false,
        )
        .unwrap();
        m.add("", "preference", "Answer concisely.", "", "s3", false)
            .unwrap();
        // Same text again: updated, not duplicated.
        assert_eq!(
            m.add(
                "/proj/a",
                "preference",
                "Use tabs, never spaces, in the Makefile.",
                "",
                "s4",
                true
            )
            .unwrap(),
            a
        );

        let in_a = m.list("/proj/a").unwrap();
        assert_eq!(in_a.len(), 3, "a's two plus the global one");
        assert!(
            in_a.iter().all(|x| x.project != "/proj/b"),
            "never another project's"
        );
        assert!(!in_a.iter().find(|x| x.id == a).unwrap().reviewed);

        let found = m
            .search("/proj/a", "how do I run the integration tests?", 5)
            .unwrap();
        assert_eq!(found[0].kind, "gotcha");
        assert!(m.search("/proj/a", "fly.io deploys", 5).unwrap().is_empty());

        m.set_pinned(a, true).unwrap();
        let relevant = m.relevant("/proj/a", "edit the Makefile", 2000).unwrap();
        assert_eq!(relevant[0].id, a);
        assert!(
            reminder(&relevant)
                .unwrap()
                .contains("[preference] Use tabs")
        );

        m.update_text(a, "Use tabs in Makefiles.").unwrap();
        assert_eq!(
            m.search("/proj/a", "makefiles", 5).unwrap()[0].text,
            "Use tabs in Makefiles."
        );
        assert!(
            m.export("/proj/a")
                .unwrap()
                .contains("## Preference\n\n- Use tabs in Makefiles. (pinned)")
        );
        m.delete(a).unwrap();
        assert!(m.search("/proj/a", "makefiles", 5).unwrap().is_empty());
        let _ = std::fs::remove_file(path);
    }
}
