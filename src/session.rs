//! A conversation: the model history, what the interface shows, and persistence
//! in the database (`store`). Saves are incremental: new model messages are appended,
//! and transcript entries are rewritten only from the first one that changed.

use crate::agent::AgentEvent;
use crate::providers::{Message, Usage};
use crate::store;
use crate::tools::{Agent, Todo, ToolMeta};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::path::{Path, PathBuf};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ToolState {
    Pending,
    Running,
    Done,
    Error,
}

/// One row of the transcript.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Entry {
    User {
        text: String,
        #[serde(default)]
        agent: Agent,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        skill: Option<String>,
    },
    Text {
        text: String,
    },
    Thinking {
        text: String,
        #[serde(default)]
        done: bool,
    },
    Tool {
        id: String,
        name: String,
        #[serde(default)]
        input: Value,
        state: ToolState,
        #[serde(default)]
        meta: ToolMeta,
    },
    Footer {
        agent: Agent,
        model: String,
        seconds: f32,
        interrupted: bool,
    },
    Notice {
        text: String,
    },
    Error {
        text: String,
    },
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Session {
    pub id: String,
    pub title: String,
    pub cwd: PathBuf,
    pub created: i64,
    pub updated: i64,
    pub messages: Vec<Message>,
    pub entries: Vec<Entry>,
    #[serde(default)]
    pub usage: Usage,
    /// Tokens in the most recent request: how full the context is.
    #[serde(default)]
    pub context_tokens: u64,
    #[serde(default)]
    pub todos: Vec<Todo>,
    #[serde(default)]
    pub model: String,
    /// How many `messages` are already in the database.
    #[serde(skip)]
    saved_messages: usize,
    /// The first entry changed since the last save (`entries.len()` when clean).
    #[serde(skip)]
    dirty_from: usize,
}

fn now() -> i64 {
    chrono::Utc::now().timestamp()
}

impl Session {
    pub fn new(cwd: &Path) -> Self {
        let id = format!(
            "{}-{}",
            chrono::Utc::now().format("%Y%m%d%H%M%S"),
            &uuid::Uuid::new_v4().simple().to_string()[..6]
        );
        Self {
            id,
            title: "New session".into(),
            cwd: cwd.to_owned(),
            created: now(),
            updated: now(),
            messages: Vec::new(),
            entries: Vec::new(),
            usage: Usage::default(),
            context_tokens: 0,
            todos: Vec::new(),
            model: String::new(),
            saved_messages: 0,
            dirty_from: 0,
        }
    }

    /// Marks entry `index` (and everything after it) as needing a save.
    fn touch(&mut self, index: usize) {
        self.dirty_from = self.dirty_from.min(index);
    }

    fn touch_last(&mut self) {
        let last = self.entries.len().saturating_sub(1);
        self.touch(last);
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Adds the user's message. `model_text` is what the model receives (it can carry
    /// a loaded skill); `shown` is what the transcript shows.
    pub fn push_user(
        &mut self,
        shown: &str,
        model_text: String,
        agent: Agent,
        skill: Option<String>,
    ) {
        if self.messages.is_empty() {
            let title: String = shown
                .lines()
                .next()
                .unwrap_or("")
                .chars()
                .take(60)
                .collect();
            if !title.trim().is_empty() {
                self.title = title;
            }
        }
        self.entries.push(Entry::User {
            text: shown.to_owned(),
            agent,
            skill,
        });
        self.messages.push(Message::user_text(model_text));
        self.updated = now();
    }

    fn close_thinking(&mut self) {
        if let Some(Entry::Thinking { done, .. }) = self.entries.last_mut() {
            *done = true;
        }
    }

    pub fn apply(&mut self, event: &AgentEvent, agent: Agent, model: &str) {
        // Every event appends an entry or changes the last one; older entries are
        // touched explicitly below (tool results, interrupted calls).
        self.touch_last();
        match event {
            AgentEvent::Text(piece) => {
                self.close_thinking();
                match self.entries.last_mut() {
                    Some(Entry::Text { text }) => text.push_str(piece),
                    _ => self.entries.push(Entry::Text {
                        text: piece.trim_start().to_owned(),
                    }),
                }
            }
            AgentEvent::Thinking(piece) => match self.entries.last_mut() {
                Some(Entry::Thinking { text, done: false }) => text.push_str(piece),
                _ => self.entries.push(Entry::Thinking {
                    text: piece.clone(),
                    done: false,
                }),
            },
            AgentEvent::ToolStart { id, name } => {
                self.close_thinking();
                self.entries.push(Entry::Tool {
                    id: id.clone(),
                    name: name.clone(),
                    input: Value::Null,
                    state: ToolState::Pending,
                    meta: ToolMeta {
                        title: name.clone(),
                        ..Default::default()
                    },
                });
            }
            AgentEvent::ToolCall { id, name, input } => {
                self.close_thinking();
                let title = crate::tools::title(name, input);
                match self.tool_entry(id) {
                    Some(Entry::Tool {
                        input: i,
                        state,
                        meta,
                        ..
                    }) => {
                        *i = input.clone();
                        *state = ToolState::Running;
                        meta.title = title;
                    }
                    _ => self.entries.push(Entry::Tool {
                        id: id.clone(),
                        name: name.clone(),
                        input: input.clone(),
                        state: ToolState::Running,
                        meta: ToolMeta {
                            title,
                            ..Default::default()
                        },
                    }),
                }
            }
            AgentEvent::ToolResult {
                id,
                is_error,
                meta,
                content,
            } => {
                let mut meta = meta.clone();
                if *is_error && meta.detail.is_none() {
                    meta.detail = Some(content.clone());
                }
                if let Some(Entry::Tool { state, meta: m, .. }) = self.tool_entry(id) {
                    *state = if *is_error {
                        ToolState::Error
                    } else {
                        ToolState::Done
                    };
                    *m = meta;
                }
            }
            AgentEvent::Message(message) => {
                self.messages.push(message.clone());
                self.updated = now();
            }
            AgentEvent::Usage(usage) => {
                self.usage.add(*usage);
                self.context_tokens = usage.context() + usage.output_tokens;
            }
            AgentEvent::Notice(text) => self.entries.push(Entry::Notice { text: text.clone() }),
            AgentEvent::Error(text) => {
                self.close_thinking();
                self.entries.push(Entry::Error { text: text.clone() })
            }
            AgentEvent::Done {
                interrupted,
                seconds,
            } => {
                self.close_thinking();
                // Calls that never ran (interrupted) are marked as failed.
                for (index, entry) in self.entries.iter_mut().enumerate() {
                    if let Entry::Tool { state, .. } = entry
                        && matches!(state, ToolState::Pending | ToolState::Running)
                    {
                        *state = ToolState::Error;
                        self.dirty_from = self.dirty_from.min(index);
                    }
                }
                self.entries.push(Entry::Footer {
                    agent,
                    model: model.to_owned(),
                    seconds: *seconds,
                    interrupted: *interrupted,
                });
                self.model = model.to_owned();
                self.updated = now();
            }
        }
    }

    fn tool_entry(&mut self, tool_id: &str) -> Option<&mut Entry> {
        let index = self
            .entries
            .iter()
            .rposition(|e| matches!(e, Entry::Tool { id, .. } if id == tool_id))?;
        self.touch(index);
        self.entries.get_mut(index)
    }

    /// Saves what changed since the last save, in the background.
    pub fn save(&mut self) -> anyhow::Result<()> {
        self.save_to(store::get());
        Ok(())
    }

    pub(crate) fn save_to(&mut self, store: &store::Store) {
        if self.messages.is_empty() && self.entries.is_empty() {
            return;
        }
        let messages_from = self.saved_messages.min(self.messages.len());
        let new_messages = &self.messages[messages_from..];
        let entries_from = self.dirty_from.min(self.entries.len());
        // Search text changes only when the model history does.
        let search_body = (!new_messages.is_empty() || messages_from == 0).then(|| {
            self.messages
                .iter()
                .map(|m| m.text())
                .filter(|t| !t.is_empty())
                .collect::<Vec<_>>()
                .join("\n")
        });
        store.save_session(store::SessionWrite {
            id: self.id.clone(),
            project: crate::context::project_root(&self.cwd)
                .display()
                .to_string(),
            cwd: self.cwd.display().to_string(),
            title: self.title.clone(),
            model: self.model.clone(),
            created: self.created,
            updated: self.updated,
            usage: to_json(&self.usage),
            context_tokens: self.context_tokens as i64,
            todos: to_json(&self.todos),
            messages_from,
            messages: new_messages.iter().map(to_json).collect(),
            entries_from,
            entries: self.entries[entries_from..].iter().map(to_json).collect(),
            search_body,
        });
        self.saved_messages = self.messages.len();
        self.dirty_from = self.entries.len();
    }

    pub fn load(id: &str) -> anyhow::Result<Session> {
        Self::load_from(store::get(), id)
    }

    pub(crate) fn load_from(store: &store::Store, id: &str) -> anyhow::Result<Session> {
        let row = store.load_session(id)?;
        let parse = |what: &str, text: &str| -> anyhow::Result<serde_json::Value> {
            serde_json::from_str(text).map_err(|e| anyhow::anyhow!("session {id}: bad {what}: {e}"))
        };
        let messages: Vec<Message> = row
            .messages
            .iter()
            .map(|m| Ok(serde_json::from_value(parse("message", m)?)?))
            .collect::<anyhow::Result<_>>()?;
        let entries: Vec<Entry> = row
            .entries
            .iter()
            .map(|e| Ok(serde_json::from_value(parse("entry", e)?)?))
            .collect::<anyhow::Result<_>>()?;
        Ok(Session {
            id: row.id,
            title: row.title,
            cwd: row.cwd,
            created: row.created,
            updated: row.updated,
            saved_messages: messages.len(),
            dirty_from: entries.len(),
            messages,
            entries,
            usage: serde_json::from_str(&row.usage).unwrap_or_default(),
            context_tokens: row.context_tokens.max(0) as u64,
            todos: serde_json::from_str(&row.todos).unwrap_or_default(),
            model: row.model,
        })
    }

    /// Sessions started in `project` (or below it), newest first.
    pub fn list(project: &Path) -> Vec<SessionSummary> {
        store::get().list_sessions(project, None)
    }

    /// Sessions in `project` whose title or conversation matches `query`, best first.
    pub fn search(project: &Path, query: &str) -> Vec<SessionSummary> {
        store::get().list_sessions(project, Some(query))
    }
}

pub use store::SessionSummary;

/// A part of the session as JSON text.
fn to_json<T: serde::Serialize>(value: &T) -> String {
    serde_json::to_string(value).unwrap_or_else(|_| "null".into())
}
