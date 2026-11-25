//! A conversation: the model history, what the interface shows, and persistence
//! under `~/.local/share/crack/sessions`.

use crate::agent::AgentEvent;
use crate::config;
use crate::providers::{Message, Usage};
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
        }
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
                for entry in &mut self.entries {
                    if let Entry::Tool { state, .. } = entry
                        && matches!(state, ToolState::Pending | ToolState::Running)
                    {
                        *state = ToolState::Error;
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
        self.entries
            .iter_mut()
            .rev()
            .find(|e| matches!(e, Entry::Tool { id, .. } if id == tool_id))
    }

    fn dir() -> PathBuf {
        config::data_dir().join("sessions")
    }

    pub fn save(&self) -> anyhow::Result<()> {
        if self.messages.is_empty() {
            return Ok(());
        }
        let dir = Self::dir();
        std::fs::create_dir_all(&dir)?;
        let tmp = dir.join(format!("{}.json.tmp", self.id));
        std::fs::write(&tmp, serde_json::to_vec(self)?)?;
        std::fs::rename(tmp, dir.join(format!("{}.json", self.id)))?;
        Ok(())
    }

    pub fn load(id: &str) -> anyhow::Result<Session> {
        let text = std::fs::read(Self::dir().join(format!("{id}.json")))?;
        Ok(serde_json::from_slice(&text)?)
    }

    /// Sessions started in `project` (or below it), newest first.
    pub fn list(project: &Path) -> Vec<SessionSummary> {
        let Ok(entries) = std::fs::read_dir(Self::dir()) else {
            return Vec::new();
        };
        let mut out: Vec<SessionSummary> = entries
            .flatten()
            .filter(|e| e.path().extension().is_some_and(|x| x == "json"))
            .filter_map(|e| {
                let bytes = std::fs::read(e.path()).ok()?;
                let summary: SessionSummary = serde_json::from_slice(&bytes).ok()?;
                summary.cwd.starts_with(project).then_some(summary)
            })
            .collect();
        out.sort_by_key(|s| std::cmp::Reverse(s.updated));
        out
    }
}

#[derive(Clone, Debug, Deserialize)]
pub struct SessionSummary {
    pub id: String,
    pub title: String,
    pub cwd: PathBuf,
    pub updated: i64,
}
