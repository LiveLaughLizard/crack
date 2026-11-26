//! The database: `~/.local/share/crack/crack.db` (SQLite, WAL mode).
//!
//! Reads run on a shared connection. Writes go to a dedicated thread through a
//! channel, so saving never blocks the interface; `flush` waits for them (before
//! quitting, at the end of a headless run).
//!
//! On first open, sessions saved as JSON by earlier versions, the remembered window
//! state and the prompt history are imported; the old files are moved aside, not
//! deleted.

use crate::config;
use rusqlite::{Connection, OptionalExtension, params};
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::sync::{Mutex, OnceLock};

/// Schema changes, applied in order. Never edit a released step; add a new one.
const MIGRATIONS: &[&str] = &[
    // 1: sessions and their transcripts.
    "CREATE TABLE sessions (
        id TEXT PRIMARY KEY,
        project TEXT NOT NULL,
        cwd TEXT NOT NULL,
        title TEXT NOT NULL,
        model TEXT NOT NULL DEFAULT '',
        created INTEGER NOT NULL,
        updated INTEGER NOT NULL,
        usage TEXT NOT NULL DEFAULT '{}',
        context_tokens INTEGER NOT NULL DEFAULT 0,
        todos TEXT NOT NULL DEFAULT '[]'
    );
    CREATE INDEX sessions_by_project ON sessions (project, updated DESC);
    CREATE TABLE messages (
        session TEXT NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
        seq INTEGER NOT NULL,
        data TEXT NOT NULL,
        PRIMARY KEY (session, seq)
    );
    CREATE TABLE entries (
        session TEXT NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
        seq INTEGER NOT NULL,
        data TEXT NOT NULL,
        PRIMARY KEY (session, seq)
    );
    CREATE VIRTUAL TABLE session_search USING fts5(session UNINDEXED, title, body);
    CREATE TABLE kv (key TEXT PRIMARY KEY, value TEXT NOT NULL);",
];

/// A session row and the parts of it that changed since the last save.
pub struct SessionWrite {
    pub id: String,
    pub project: String,
    pub cwd: String,
    pub title: String,
    pub model: String,
    pub created: i64,
    pub updated: i64,
    pub usage: String,
    pub context_tokens: i64,
    pub todos: String,
    /// Model messages to append, starting at `messages_from`.
    pub messages_from: usize,
    pub messages: Vec<String>,
    /// Transcript entries from `entries_from` on replace what's stored there.
    pub entries_from: usize,
    pub entries: Vec<String>,
    /// Text for full-text search (title and messages), when it changed.
    pub search_body: Option<String>,
}

#[derive(Clone, Debug)]
pub struct SessionRow {
    pub id: String,
    pub cwd: PathBuf,
    pub title: String,
    pub model: String,
    pub created: i64,
    pub updated: i64,
    pub usage: String,
    pub context_tokens: i64,
    pub todos: String,
    pub messages: Vec<String>,
    pub entries: Vec<String>,
}

#[derive(Clone, Debug)]
pub struct SessionSummary {
    pub id: String,
    pub title: String,
    pub cwd: PathBuf,
    pub updated: i64,
}

enum Job {
    Save(Box<SessionWrite>),
    Delete(String),
    SetKv(String, String),
    Flush(mpsc::Sender<()>),
}

pub struct Store {
    reader: Mutex<Connection>,
    writer: mpsc::Sender<Job>,
    /// Errors from the writer thread, reported on the next flush.
    errors: std::sync::Arc<Mutex<Vec<String>>>,
}

static STORE: OnceLock<Store> = OnceLock::new();

/// The database in `config::data_dir()`, opened on first use.
pub fn get() -> &'static Store {
    STORE.get_or_init(|| {
        let path = config::data_dir().join("crack.db");
        Store::open(&path).unwrap_or_else(|error| {
            eprintln!(
                "warning: could not open {}: {error:#}; using an in-memory database",
                path.display()
            );
            Store::open(Path::new(":memory:")).expect("an in-memory database always opens")
        })
    })
}

fn connect(path: &Path) -> anyhow::Result<Connection> {
    let connection = Connection::open(path)?;
    connection.busy_timeout(std::time::Duration::from_secs(5))?;
    connection.execute_batch(
        "PRAGMA journal_mode = WAL; PRAGMA foreign_keys = ON; PRAGMA synchronous = NORMAL;",
    )?;
    Ok(connection)
}

fn migrate(connection: &mut Connection) -> anyhow::Result<()> {
    connection
        .execute_batch("CREATE TABLE IF NOT EXISTS schema_version (version INTEGER NOT NULL)")?;
    let current: i64 = connection.query_row(
        "SELECT COALESCE(MAX(version), 0) FROM schema_version",
        [],
        |r| r.get(0),
    )?;
    for (index, sql) in MIGRATIONS.iter().enumerate().skip(current as usize) {
        let tx = connection.transaction()?;
        tx.execute_batch(sql)?;
        tx.execute(
            "INSERT INTO schema_version (version) VALUES (?1)",
            [index as i64 + 1],
        )?;
        tx.commit()?;
    }
    Ok(())
}

impl Store {
    pub fn open(path: &Path) -> anyhow::Result<Store> {
        if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
            std::fs::create_dir_all(parent)?;
        }
        let memory = path == Path::new(":memory:");
        let mut writer_connection = if memory {
            // Shared cache so the reader sees the writer's in-memory database.
            Connection::open("file:crack?mode=memory&cache=shared")?
        } else {
            connect(path)?
        };
        migrate(&mut writer_connection)?;
        let reader = if memory {
            Connection::open("file:crack?mode=memory&cache=shared")?
        } else {
            connect(path)?
        };

        let (tx, rx) = mpsc::channel::<Job>();
        let errors = std::sync::Arc::new(Mutex::new(Vec::new()));
        let thread_errors = errors.clone();
        std::thread::Builder::new()
            .name("crack-store".into())
            .spawn(move || {
                let mut connection = writer_connection;
                for job in rx {
                    let result = match job {
                        Job::Save(write) => save(&mut connection, &write),
                        Job::Delete(id) => connection
                            .execute("DELETE FROM sessions WHERE id = ?1", [&id])
                            .and_then(|_| {
                                connection
                                    .execute("DELETE FROM session_search WHERE session = ?1", [&id])
                            })
                            .map(|_| ())
                            .map_err(Into::into),
                        Job::SetKv(key, value) => connection
                            .execute(
                                "INSERT INTO kv (key, value) VALUES (?1, ?2)
                                 ON CONFLICT(key) DO UPDATE SET value = excluded.value",
                                params![key, value],
                            )
                            .map(|_| ())
                            .map_err(Into::into),
                        Job::Flush(done) => {
                            let _ = done.send(());
                            Ok(())
                        }
                    };
                    if let Err(error) = result {
                        thread_errors.lock().unwrap().push(format!("{error:#}"));
                    }
                }
            })?;

        let store = Store {
            reader: Mutex::new(reader),
            writer: tx,
            errors,
        };
        if !memory {
            store.import_legacy(path.parent().unwrap_or(Path::new(".")));
        }
        Ok(store)
    }

    pub fn save_session(&self, write: SessionWrite) {
        let _ = self.writer.send(Job::Save(Box::new(write)));
    }

    pub fn delete_session(&self, id: &str) {
        let _ = self.writer.send(Job::Delete(id.to_owned()));
    }

    pub fn set_kv(&self, key: &str, value: String) {
        let _ = self.writer.send(Job::SetKv(key.to_owned(), value));
    }

    /// Waits until every queued write is on disk. Returns write errors since the last flush.
    pub fn flush(&self) -> Vec<String> {
        let (tx, rx) = mpsc::channel();
        if self.writer.send(Job::Flush(tx)).is_ok() {
            let _ = rx.recv_timeout(std::time::Duration::from_secs(10));
        }
        std::mem::take(&mut *self.errors.lock().unwrap())
    }

    pub fn kv(&self, key: &str) -> Option<String> {
        self.reader
            .lock()
            .unwrap()
            .query_row("SELECT value FROM kv WHERE key = ?1", [key], |r| r.get(0))
            .optional()
            .ok()
            .flatten()
    }

    /// Every non-empty value whose key starts with `prefix`.
    pub fn kv_prefix(&self, prefix: &str) -> Vec<(String, String)> {
        let connection = self.reader.lock().unwrap();
        let Ok(mut statement) = connection
            .prepare("SELECT key, value FROM kv WHERE key >= ?1 AND key < ?2 AND value != ''")
        else {
            return Vec::new();
        };
        let end = format!("{prefix}\u{10FFFF}");
        statement
            .query_map(params![prefix, end], |r| Ok((r.get(0)?, r.get(1)?)))
            .map(|rows| rows.flatten().collect())
            .unwrap_or_default()
    }

    pub fn load_session(&self, id: &str) -> anyhow::Result<SessionRow> {
        let connection = self.reader.lock().unwrap();
        let mut row = connection
            .query_row(
                "SELECT id, cwd, title, model, created, updated, usage, context_tokens, todos
                 FROM sessions WHERE id = ?1",
                [id],
                |r| {
                    Ok(SessionRow {
                        id: r.get(0)?,
                        cwd: PathBuf::from(r.get::<_, String>(1)?),
                        title: r.get(2)?,
                        model: r.get(3)?,
                        created: r.get(4)?,
                        updated: r.get(5)?,
                        usage: r.get(6)?,
                        context_tokens: r.get(7)?,
                        todos: r.get(8)?,
                        messages: Vec::new(),
                        entries: Vec::new(),
                    })
                },
            )
            .optional()?
            .ok_or_else(|| anyhow::anyhow!("no session {id}"))?;
        for (table, target) in [
            ("messages", &mut row.messages),
            ("entries", &mut row.entries),
        ] {
            let mut statement = connection.prepare(&format!(
                "SELECT data FROM {table} WHERE session = ?1 ORDER BY seq"
            ))?;
            *target = statement
                .query_map([id], |r| r.get::<_, String>(0))?
                .collect::<Result<_, _>>()?;
        }
        Ok(row)
    }

    /// Sessions in `project` or below it, newest first. With a query, only those whose
    /// title or messages match it (full-text search).
    pub fn list_sessions(&self, project: &Path, query: Option<&str>) -> Vec<SessionSummary> {
        let connection = self.reader.lock().unwrap();
        let project = project.display().to_string();
        let like = format!("{}%", project.replace('%', "\\%").replace('_', "\\_"));
        let map = |r: &rusqlite::Row| {
            Ok(SessionSummary {
                id: r.get(0)?,
                title: r.get(1)?,
                cwd: PathBuf::from(r.get::<_, String>(2)?),
                updated: r.get(3)?,
            })
        };
        let result: rusqlite::Result<Vec<SessionSummary>> =
            match query.map(fts_query).filter(|q| !q.is_empty()) {
                None => connection
                    .prepare(
                        "SELECT id, title, cwd, updated FROM sessions
                     WHERE project = ?1 OR cwd LIKE ?2 ESCAPE '\\' ORDER BY updated DESC",
                    )
                    .and_then(|mut s| s.query_map(params![project, like], map)?.collect()),
                Some(q) => connection
                    .prepare(
                        "SELECT s.id, s.title, s.cwd, s.updated FROM session_search f
                     JOIN sessions s ON s.id = f.session
                     WHERE session_search MATCH ?3 AND (s.project = ?1 OR s.cwd LIKE ?2 ESCAPE '\\')
                     ORDER BY rank",
                    )
                    .and_then(|mut s| s.query_map(params![project, like, q], map)?.collect()),
            };
        result.unwrap_or_default()
    }

    /// Imports JSON sessions, state.json and prompt-history.json from older versions.
    fn import_legacy(&self, data_dir: &Path) {
        let sessions_dir = data_dir.join("sessions");
        if let Ok(entries) = std::fs::read_dir(&sessions_dir) {
            let mut imported = 0;
            for entry in entries.flatten() {
                let path = entry.path();
                if path.extension().is_none_or(|x| x != "json") {
                    continue;
                }
                let Ok(bytes) = std::fs::read(&path) else {
                    continue;
                };
                match serde_json::from_slice::<crate::session::Session>(&bytes) {
                    Ok(mut session) => {
                        session.save_to(self);
                        imported += 1;
                    }
                    Err(error) => eprintln!("warning: skipped {}: {error}", path.display()),
                }
            }
            let errors = self.flush();
            if errors.is_empty() {
                let _ = std::fs::rename(&sessions_dir, data_dir.join("sessions.imported"));
                if imported > 0 {
                    eprintln!(
                        "Imported {imported} session(s) into {}",
                        data_dir.join("crack.db").display()
                    );
                }
            } else {
                eprintln!(
                    "warning: session import failed, kept the JSON files: {}",
                    errors.join("; ")
                );
            }
        }
        for (file, key) in [
            ("state.json", "state"),
            ("prompt-history.json", "prompt_history"),
        ] {
            let path = data_dir.join(file);
            if let Ok(text) = std::fs::read_to_string(&path) {
                if self.kv(key).is_none() {
                    self.set_kv(key, text);
                    self.flush();
                }
                let _ = std::fs::rename(&path, path.with_extension("json.imported"));
            }
        }
    }
}

/// Quotes each word so user input can't be misread as FTS syntax; words must all match.
fn fts_query(query: &str) -> String {
    query
        .split_whitespace()
        .map(|word| format!("\"{}\"*", word.replace('"', "")))
        .collect::<Vec<_>>()
        .join(" ")
}

fn save(connection: &mut Connection, write: &SessionWrite) -> anyhow::Result<()> {
    let tx = connection.transaction()?;
    tx.execute(
        "INSERT INTO sessions (id, project, cwd, title, model, created, updated, usage, context_tokens, todos)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)
         ON CONFLICT(id) DO UPDATE SET
            project = excluded.project, cwd = excluded.cwd, title = excluded.title,
            model = excluded.model, updated = excluded.updated, usage = excluded.usage,
            context_tokens = excluded.context_tokens, todos = excluded.todos",
        params![
            write.id, write.project, write.cwd, write.title, write.model, write.created,
            write.updated, write.usage, write.context_tokens, write.todos
        ],
    )?;
    {
        let mut insert = tx.prepare(
            "INSERT INTO messages (session, seq, data) VALUES (?1, ?2, ?3)
             ON CONFLICT(session, seq) DO UPDATE SET data = excluded.data",
        )?;
        for (i, data) in write.messages.iter().enumerate() {
            insert.execute(params![write.id, (write.messages_from + i) as i64, data])?;
        }
    }
    // Compaction can shorten the history.
    tx.execute(
        "DELETE FROM messages WHERE session = ?1 AND seq >= ?2",
        params![
            write.id,
            (write.messages_from + write.messages.len()) as i64
        ],
    )?;
    tx.execute(
        "DELETE FROM entries WHERE session = ?1 AND seq >= ?2",
        params![write.id, write.entries_from as i64],
    )?;
    {
        let mut insert =
            tx.prepare("INSERT INTO entries (session, seq, data) VALUES (?1, ?2, ?3)")?;
        for (i, data) in write.entries.iter().enumerate() {
            insert.execute(params![write.id, (write.entries_from + i) as i64, data])?;
        }
    }
    if let Some(body) = &write.search_body {
        tx.execute("DELETE FROM session_search WHERE session = ?1", [&write.id])?;
        tx.execute(
            "INSERT INTO session_search (session, title, body) VALUES (?1, ?2, ?3)",
            params![write.id, write.title, body],
        )?;
    }
    tx.commit()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::AgentEvent;
    use crate::providers::Message;
    use crate::session::{Entry, Session, ToolState};
    use crate::tools::{Agent, ToolMeta};

    fn temp_dir() -> PathBuf {
        let dir = std::env::temp_dir().join(format!("crack-store-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn incremental_saves_round_trip() {
        let dir = temp_dir();
        let store = Store::open(&dir.join("crack.db")).unwrap();
        let project = dir.join("project");
        let mut session = Session::new(&project);
        session.push_user("fix the bug", "fix the bug".into(), Agent::Build, None);
        session.apply(&AgentEvent::Text("Looking".into()), Agent::Build, "m");
        session.apply(
            &AgentEvent::ToolCall {
                id: "t1".into(),
                name: "bash".into(),
                input: serde_json::json!({"command": "ls"}),
            },
            Agent::Build,
            "m",
        );
        session.save_to(&store);
        assert!(store.flush().is_empty());

        // The tool finishes: an already-saved entry changes, and more is appended.
        session.apply(
            &AgentEvent::ToolResult {
                id: "t1".into(),
                is_error: false,
                meta: ToolMeta {
                    title: "$ ls".into(),
                    ..Default::default()
                },
                content: "a b".into(),
            },
            Agent::Build,
            "m",
        );
        session.apply(&AgentEvent::Text(" and done".into()), Agent::Build, "m");
        session.apply(
            &AgentEvent::Done {
                interrupted: false,
                seconds: 1.5,
            },
            Agent::Build,
            "m",
        );
        session.save_to(&store);
        assert!(store.flush().is_empty());

        let loaded = Session::load_from(&store, &session.id).unwrap();
        assert_eq!(loaded.messages.len(), session.messages.len());
        assert_eq!(loaded.entries.len(), session.entries.len());
        assert_eq!(loaded.title, "fix the bug");
        assert!(loaded.entries.iter().any(|e| matches!(
            e,
            Entry::Tool {
                state: ToolState::Done,
                ..
            }
        )));
        assert!(matches!(loaded.entries.last(), Some(Entry::Footer { .. })));

        // Compaction replaces the history with a shorter one; the store must shrink too.
        session.apply(
            &AgentEvent::Message(Message::user_text("more")),
            Agent::Build,
            "m",
        );
        session.apply(
            &AgentEvent::Message(Message::user_text("and more")),
            Agent::Build,
            "m",
        );
        session.save_to(&store);
        let summary = vec![Message::user_text("summary")];
        session.apply(
            &AgentEvent::Compacted {
                messages: summary,
                before: 9000,
            },
            Agent::Build,
            "m",
        );
        session.save_to(&store);
        assert!(store.flush().is_empty());
        let loaded = Session::load_from(&store, &session.id).unwrap();
        assert_eq!(loaded.messages.len(), 1);
        assert_eq!(loaded.messages[0].text(), "summary");

        // Listing, search and delete.
        let listed = store.list_sessions(&project, None);
        assert_eq!(listed.len(), 1);
        assert_eq!(store.list_sessions(&project, Some("bug")).len(), 1);
        assert_eq!(
            store.list_sessions(&project, Some("nonexistentword")).len(),
            0
        );
        assert_eq!(store.list_sessions(&dir.join("elsewhere"), None).len(), 0);
        store.delete_session(&session.id);
        store.flush();
        assert!(store.list_sessions(&project, None).is_empty());
        assert!(store.list_sessions(&project, Some("bug")).is_empty());

        store.set_kv("k", "v".into());
        store.flush();
        assert_eq!(store.kv("k").as_deref(), Some("v"));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn imports_legacy_json() {
        let dir = temp_dir();
        let project = dir.join("project");
        let mut session = Session::new(&project);
        session.push_user("old session", "old session".into(), Agent::Build, None);
        std::fs::create_dir_all(dir.join("sessions")).unwrap();
        std::fs::write(
            dir.join("sessions").join(format!("{}.json", session.id)),
            serde_json::to_vec(&session).unwrap(),
        )
        .unwrap();
        std::fs::write(dir.join("prompt-history.json"), "[\"one\",\"two\"]").unwrap();

        let store = Store::open(&dir.join("crack.db")).unwrap();
        let listed = store.list_sessions(&project, None);
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].title, "old session");
        assert_eq!(
            store.kv("prompt_history").as_deref(),
            Some("[\"one\",\"two\"]")
        );
        // The old files are kept, moved aside.
        assert!(dir.join("sessions.imported").is_dir());
        assert!(!dir.join("sessions").exists());
        assert!(dir.join("prompt-history.json.imported").is_file());
        let _ = std::fs::remove_dir_all(dir);
    }
}
