//! Subagents the main agent orchestrates. `task` starts one (waiting for its report,
//! or in the background). While it works the two can talk: the main agent's
//! `send_message` reaches it at its next step, and it sends progress, questions and
//! blockers back with `message_main`. `agent_status`, `wait_for_agents` and `stop_agent`
//! round it out. An agent exits once it has no more work and has reported; a message
//! that arrives as it finishes is handled first. Up to `max_parallel_agents` (default
//! 15) work at once; the rest queue. Agents that edit in isolation work in a git
//! worktree and hand back a patch.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use tokio::sync::{mpsc, watch};

use crate::agent::{AgentEvent, Events, TurnConfig};
use crate::providers::{Message, Role, ToolSpec};
use crate::session::Session;
use crate::subagents::AgentDef;
use crate::tools::{Agent, DisplayKind, ToolContext, ToolMeta, ToolOutput};

/// The main agent's team tools besides `task`.
pub const TEAM_TOOLS: &[&str] = &[
    "send_message",
    "agent_status",
    "wait_for_agents",
    "stop_agent",
];

/// What a turn needs to start subagents. Absent once agents may nest no deeper.
pub struct SubagentEnv {
    pub registry: crate::providers::registry::Registry,
    pub defs: Vec<AgentDef>,
    pub parent_model: String,
    pub small_model: Option<String>,
    pub instructions: Vec<crate::context::Instruction>,
    pub max_parallel: usize,
    pub max_steps: u32,
    pub limit: Arc<tokio::sync::Semaphore>,
    /// How deep this level is (the main agent's agents are at 1) and how deep agents
    /// may nest.
    pub depth: u32,
    pub max_depth: u32,
}

impl SubagentEnv {
    pub fn new(
        registry: crate::providers::registry::Registry,
        cwd: &Path,
        parent_model: String,
        small_model: Option<String>,
        instructions: Vec<crate::context::Instruction>,
        max_parallel: usize,
        max_steps: u32,
    ) -> SubagentEnv {
        let max_parallel = max_parallel.clamp(1, 32);
        SubagentEnv {
            registry,
            defs: crate::subagents::discover(cwd),
            parent_model,
            small_model,
            instructions,
            max_parallel,
            max_steps,
            limit: Arc::new(tokio::sync::Semaphore::new(max_parallel)),
            depth: 0,
            max_depth: 2,
        }
    }

    /// How deep agents may nest (1: only the main agent starts them; at most 5).
    pub fn with_max_depth(mut self, max_depth: u32) -> SubagentEnv {
        self.max_depth = max_depth.clamp(1, 5);
        self
    }

    /// What an agent started from here may use to start its own, if nesting allows. Each
    /// level has its own slots, so agents waiting on their children never starve them.
    pub fn child(&self) -> Option<Arc<SubagentEnv>> {
        (self.depth + 1 < self.max_depth).then(|| {
            Arc::new(SubagentEnv {
                registry: self.registry.clone(),
                defs: self.defs.clone(),
                parent_model: self.parent_model.clone(),
                small_model: self.small_model.clone(),
                instructions: self.instructions.clone(),
                max_parallel: self.max_parallel,
                max_steps: self.max_steps,
                limit: Arc::new(tokio::sync::Semaphore::new(self.max_parallel)),
                depth: self.depth + 1,
                max_depth: self.max_depth,
            })
        })
    }

    /// `task` and the team tools, listing the agents `task` can start.
    pub fn specs(&self, parent: Agent) -> Vec<ToolSpec> {
        let mut listing = String::new();
        for def in self
            .defs
            .iter()
            .filter(|d| parent == Agent::Build || read_only(d))
        {
            listing.push_str(&format!("- {}: {}\n", def.name, def.description));
        }
        listing.push_str(
            "- fork: a copy of you that continues from this conversation, on your model, to take one \
             branch of the work. Its prompt only needs the branch: it already knows everything said so far.\n",
        );
        let spec = |name: &str, description: String, schema: Value| ToolSpec {
            name: name.into(),
            description,
            schema,
        };
        vec![
            spec(
                "task",
                format!(
                    "Start a subagent with a fresh context for a self-contained job. It can't see this conversation, \
                     so the prompt must say everything it needs: the goal, what you know, where to look, what to report. \
                     By default you wait for its final report. With run_in_background it starts and you carry on: run \
                     several, steer them with send_message, read their progress notes in agent_status, and collect \
                     reports with wait_for_agents. Call task several times in one response to run agents in parallel \
                     (up to {} work at once; more queue). An agent exits once it has reported. Use `explore` for \
                     read-only searching.\n\nAgents:\n{listing}",
                    self.max_parallel
                ),
                json!({
                    "type": "object",
                    "properties": {
                        "description": {"type": "string", "description": "A short (3-5 word) label for the job."},
                        "prompt": {"type": "string", "description": "The complete task for the subagent."},
                        "subagent_type": {"type": "string", "description": "Which agent to use (default general)."},
                        "name": {"type": "string", "description": "A short handle to address it by, e.g. `tests` or `api-research`."},
                        "run_in_background": {"type": "boolean", "description": "Start it and return at once with its id."}
                    },
                    "required": ["description", "prompt"]
                }),
            ),
            spec(
                "send_message",
                "Send a working subagent a message: new information, a correction, a change of plan or an answer \
                 to its question. It sees the message at its next step and carries on with everything it knows."
                    .into(),
                json!({
                    "type": "object",
                    "properties": {
                        "to": {"type": "string", "description": "The agent's id (a1, a2, ...) or name."},
                        "message": {"type": "string"}
                    },
                    "required": ["to", "message"]
                }),
            ),
            spec(
                "agent_status",
                "Your subagents: what each is doing, its progress notes and questions, and its report when done. \
                 With `id`, everything about that one, including its full report."
                    .into(),
                json!({
                    "type": "object",
                    "properties": {"id": {"type": "string", "description": "An agent's id or name."}}
                }),
            ),
            spec(
                "wait_for_agents",
                "Wait until the given subagents (default: every working one) finish, or one sends a note, and get \
                 their reports and notes. Answer questions with send_message."
                    .into(),
                json!({
                    "type": "object",
                    "properties": {
                        "ids": {"type": "array", "items": {"type": "string"}, "description": "Agent ids or names."},
                        "until": {"type": "string", "enum": ["done", "any_note"], "description": "done (default): all finished; any_note: also return as soon as one sends a note."},
                        "timeout_seconds": {"type": "integer", "description": "Give up after this long (default 900)."}
                    }
                }),
            ),
            spec(
                "stop_agent",
                "Stop a subagent: it's asked to wrap up and report what it has, and stopped if it hasn't within 90 \
                 seconds. With force it stops at once (work in progress may be lost). Its transcript stays."
                    .into(),
                json!({
                    "type": "object",
                    "properties": {
                        "id": {"type": "string", "description": "The agent's id or name."},
                        "force": {"type": "boolean", "description": "Stop it at once instead of asking it to wrap up first."}
                    },
                    "required": ["id"]
                }),
            ),
        ]
    }
}

/// A subagent's tool for messaging its siblings (agents started by the same parent).
pub fn message_agent_spec() -> ToolSpec {
    ToolSpec {
        name: "message_agent".into(),
        description: "Send a message to another working agent started by the same parent as you (by id or \
                      name): to share a finding it needs, split work, or avoid doing the same thing twice. \
                      It sees the message at its next step. Use message_main for the agent that started you."
            .into(),
        schema: json!({
            "type": "object",
            "properties": {
                "to": {"type": "string", "description": "The agent's id (a1, a2, ...) or name."},
                "message": {"type": "string"}
            },
            "required": ["to", "message"]
        }),
    }
}

/// The `message_agent` tool: delivers to a sibling's inbox, signed by crack.
pub fn message_agent(link: &Link, input: &Value) -> ToolOutput {
    let to = input["to"].as_str().unwrap_or_default();
    let message = input["message"].as_str().unwrap_or_default().trim();
    let title = format!("Message \u{2192} {to}");
    if message.is_empty() {
        return ToolOutput::error("The message is empty.", title);
    }
    let (sender_id, sender) = {
        let i = lock(&link.info);
        (i.id.clone(), i.label())
    };
    let Some((info, inbox, _)) = find(&link.team, to) else {
        return ToolOutput::error(
            format!("No agent '{to}' among the ones your parent started."),
            title,
        );
    };
    let (target_id, label, busy) = {
        let i = lock(&info);
        (i.id.clone(), i.label(), i.state.busy())
    };
    if target_id == sender_id {
        return ToolOutput::error("That's you.", title);
    }
    if !busy {
        return ToolOutput::error(
            format!(
                "{label} has finished and exited; tell the agent that started you instead (message_main)."
            ),
            title,
        );
    }
    inbox
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .push((format!("agent {sender}"), bounded(message)));
    ToolOutput::ok(
        format!("Sent to {label}; it sees it at its next step."),
        ToolMeta {
            title,
            ..Default::default()
        },
    )
}

/// The subagent's own tool for talking back.
pub fn message_main_spec() -> ToolSpec {
    ToolSpec {
        name: "message_main".into(),
        description: "Send the agent that started you a short note while you work: progress at a milestone, a \
                      finding it should know now, a question, or a blocker. It may reply; replies reach you at \
                      your next step. Keep working unless you're blocked. Your final message is still your report."
            .into(),
        schema: json!({
            "type": "object",
            "properties": {
                "message": {"type": "string"},
                "kind": {"type": "string", "enum": ["progress", "question", "blocked"], "description": "Default progress."}
            },
            "required": ["message"]
        }),
    }
}

fn read_only(def: &AgentDef) -> bool {
    def.tools.as_ref().is_some_and(|tools| {
        tools
            .iter()
            .all(|t| crate::subagents::READ_ONLY_TOOLS.contains(&t.as_str()))
    })
}

// ------------------------------------------------------------------------ team

#[derive(Clone, Debug, PartialEq)]
pub enum State {
    /// Waiting for a free slot.
    Queued,
    Running,
    /// Reported and exited.
    Done,
    Failed(String),
    Stopped,
}

impl State {
    pub fn label(&self) -> String {
        match self {
            State::Queued => "queued".into(),
            State::Running => "working".into(),
            State::Done => "done".into(),
            State::Failed(why) => format!("failed: {why}"),
            State::Stopped => "stopped".into(),
        }
    }

    pub fn busy(&self) -> bool {
        matches!(self, State::Queued | State::Running)
    }
}

/// A note a subagent sent while working.
#[derive(Clone, Debug)]
pub struct Note {
    pub kind: String,
    pub text: String,
}

/// What the main agent and the window see of a team member.
#[derive(Clone, Debug)]
pub struct Info {
    pub id: String,
    pub name: Option<String>,
    pub kind: String,
    pub model: String,
    pub description: String,
    pub state: State,
    /// Tool calls made.
    pub steps: u32,
    /// What it's doing right now (its latest tool call).
    pub activity: String,
    /// Notes it sent, oldest first.
    pub notes: Vec<Note>,
    /// Its final report (once done).
    pub report: String,
    /// Its transcript (a saved session).
    pub session_id: String,
    pub started: Instant,
    /// Notes the main agent has already been shown.
    notes_told: usize,
    /// The main agent has been shown the final report.
    report_told: bool,
}

impl Info {
    pub fn label(&self) -> String {
        match &self.name {
            Some(name) => format!("{} ({name})", self.id),
            None => self.id.clone(),
        }
    }

    /// Notes the main agent hasn't seen, marking them seen.
    fn take_new_notes(&mut self) -> Vec<Note> {
        let new = self.notes[self.notes_told.min(self.notes.len())..].to_vec();
        self.notes_told = self.notes.len();
        new
    }
}

/// Messages from the main agent, waiting for the subagent's next step.
/// (sender, text): the sender is set by crack, never taken from the message.
pub type Inbox = Arc<Mutex<Vec<(String, String)>>>;

/// The longest message one agent can send another.
const MAX_MESSAGE: usize = 40_000;

fn bounded(text: &str) -> String {
    if text.chars().count() <= MAX_MESSAGE {
        return text.to_owned();
    }
    let kept: String = text.chars().take(MAX_MESSAGE).collect();
    format!("{kept}\n[... cut at {MAX_MESSAGE} characters]")
}

struct Member {
    info: Arc<Mutex<Info>>,
    inbox: Inbox,
    /// Bumped on every change worth waking a waiter for (a note, the end).
    changed: watch::Receiver<u64>,
    task: tokio::task::AbortHandle,
}

/// The team a turn manages: the session plus who started its agents (`main`, or the
/// subagent that started nested ones). Each agent sees only its own team.
pub fn team_key(config: &TurnConfig, ctx: &ToolContext) -> String {
    let owner = config
        .team_link
        .as_ref()
        .map(|l| lock(&l.info).id.clone())
        .unwrap_or_else(|| "main".into());
    format!("{}#{owner}", ctx.session_id)
}

/// The main agent's team in a session.
pub fn main_team(session: &str) -> String {
    format!("{session}#main")
}

/// Each session's team: (session id, members).
static TEAMS: Mutex<Vec<(String, Vec<Member>)>> = Mutex::new(Vec::new());

fn with_team<T>(session: &str, f: impl FnOnce(&mut Vec<Member>) -> T) -> T {
    let mut teams = TEAMS.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(pos) = teams.iter().position(|(s, _)| s == session) {
        return f(&mut teams[pos].1);
    }
    teams.push((session.to_owned(), Vec::new()));
    let team = &mut teams.last_mut().unwrap().1;
    f(team)
}

fn lock(info: &Arc<Mutex<Info>>) -> std::sync::MutexGuard<'_, Info> {
    info.lock().unwrap_or_else(|e| e.into_inner())
}

fn matches_key(info: &Info, key: &str) -> bool {
    let key = key.trim();
    info.id == key || info.name.as_deref() == Some(key)
}

/// A team member by id (`a3`) or name.
fn find(session: &str, key: &str) -> Option<(Arc<Mutex<Info>>, Inbox, watch::Receiver<u64>)> {
    with_team(session, |team| {
        team.iter()
            .rev()
            .find(|m| matches_key(&lock(&m.info), key))
            .map(|m| (m.info.clone(), m.inbox.clone(), m.changed.clone()))
    })
}

/// A team, oldest first.
pub fn list(team: &str) -> Vec<Info> {
    with_team(team, |members| {
        members.iter().map(|m| lock(&m.info).clone()).collect()
    })
}

/// Every agent in a session, nested ones included (for the sidebar).
pub fn list_all(session: &str) -> Vec<Info> {
    let prefix = format!("{session}#");
    let teams = TEAMS.lock().unwrap_or_else(|e| e.into_inner());
    let mut all: Vec<Info> = teams
        .iter()
        .filter(|(key, _)| key.starts_with(&prefix))
        .flat_map(|(_, members)| members.iter().map(|m| lock(&m.info).clone()))
        .collect();
    all.sort_by_key(|i| i.started);
    all
}

/// Stops every agent of a session, nested ones included (its tab closed, or its run
/// ended).
pub fn stop_session(session: &str) {
    let prefix = format!("{session}#");
    let mut teams = TEAMS.lock().unwrap_or_else(|e| e.into_inner());
    for (_, members) in teams.iter().filter(|(key, _)| key.starts_with(&prefix)) {
        for member in members {
            member.task.abort();
        }
    }
    teams.retain(|(key, _)| !key.starts_with(&prefix));
}

/// Stops every team (crack is exiting).
pub fn stop_all() {
    let mut teams = TEAMS.lock().unwrap_or_else(|e| e.into_inner());
    for (_, team) in teams.iter() {
        for member in team {
            member.task.abort();
        }
    }
    teams.clear();
}

/// Only what's new since the main agent last heard (finished reports and new notes),
/// merged into one message, for delivery between its model calls.
pub fn news(session: &str) -> Option<String> {
    let lines: Vec<String> = with_team(session, |team| {
        team.iter()
            .filter_map(|m| {
                let mut i = lock(&m.info);
                let notes = i.take_new_notes();
                let finished = !i.state.busy() && !i.report_told;
                if !finished && notes.is_empty() {
                    return None;
                }
                let mut text = format!("- {} ({})", i.label(), i.state.label());
                for note in notes {
                    text.push_str(&format!("\n  [{}] {}", note.kind, note.text));
                }
                if finished {
                    i.report_told = true;
                    let preview: String = i.report.chars().take(1_500).collect();
                    let more = if i.report.chars().count() > 1_500 {
                        " [... the rest: agent_status]"
                    } else {
                        ""
                    };
                    text.push_str(&format!("\n  Report: {preview}{more}"));
                }
                Some(text)
            })
            .collect()
    });
    (!lines.is_empty()).then(|| {
        format!(
            "<subagent_updates>\nNews from your subagents:\n{}\n</subagent_updates>",
            lines.join("\n")
        )
    })
}

/// Something the main agent should act on: a finished agent it hasn't heard about, or a
/// question or blocker it hasn't seen. An idle window wakes for these.
pub fn needs_attention(session: &str) -> bool {
    with_team(session, |team| {
        team.iter().any(|m| {
            let i = lock(&m.info);
            (!i.state.busy() && !i.report_told && !matches!(i.state, State::Stopped))
                || i.notes[i.notes_told.min(i.notes.len())..]
                    .iter()
                    .any(|n| n.kind == "question" || n.kind == "blocked")
        })
    })
}

/// How many of a session's agents are still working.
pub fn busy(session: &str) -> usize {
    with_team(session, |team| {
        team.iter().filter(|m| lock(&m.info).state.busy()).count()
    })
}

/// Waits until any working agent changes (finishes or sends a note), or `timeout`.
pub async fn wait_for_change(session: &str, timeout: Duration) {
    let mut receivers: Vec<watch::Receiver<u64>> = with_team(session, |team| {
        team.iter()
            .filter(|m| lock(&m.info).state.busy())
            .map(|m| m.changed.clone())
            .collect()
    });
    if receivers.is_empty() {
        return;
    }
    let waits = receivers.iter_mut().map(|rx| Box::pin(rx.changed()));
    let _ = tokio::time::timeout(timeout, futures::future::select_all(waits)).await;
}

/// For the main agent before each turn: who's working, their new notes, and reports
/// it hasn't seen.
pub fn reminder(session: &str) -> Option<String> {
    let lines: Vec<String> = with_team(session, |team| {
        team.iter()
            .filter_map(|m| {
                let mut i = lock(&m.info);
                let notes = i.take_new_notes();
                let mut text = String::new();
                if i.state.busy() {
                    text.push_str(&format!(
                        "- {} is {} ({}): {}",
                        i.label(),
                        i.state.label(),
                        i.kind,
                        i.activity
                    ));
                } else if !i.report_told {
                    i.report_told = true;
                    let preview: String = i.report.chars().take(1_500).collect();
                    let more = if i.report.chars().count() > 1_500 {
                        " [... the rest: agent_status]"
                    } else {
                        ""
                    };
                    text.push_str(&format!(
                        "- {} {}:\n{preview}{more}",
                        i.label(),
                        i.state.label()
                    ));
                } else if notes.is_empty() {
                    return None;
                } else {
                    text.push_str(&format!("- {}", i.label()));
                }
                for note in notes {
                    text.push_str(&format!("\n  [{}] {}", note.kind, note.text));
                }
                Some(text)
            })
            .collect()
    });
    (!lines.is_empty()).then(|| {
        format!(
            "<subagents>\nYour subagents:\n{}\n(Answer questions and steer working agents with send_message.)\n</subagents>",
            lines.join("\n")
        )
    })
}

/// A running subagent's line back to its team entry: its inbox, and where its notes go.
pub struct Link {
    pub info: Arc<Mutex<Info>>,
    pub inbox: Inbox,
    /// The team it belongs to: its siblings, for `message_agent`.
    pub team: String,
    changed: Arc<watch::Sender<u64>>,
}

impl Link {
    fn bump(&self) {
        self.changed.send_modify(|n| *n += 1);
    }

    /// Messages from the main agent since the last step, as text for the subagent.
    pub fn take_messages(&self) -> Option<String> {
        let messages: Vec<(String, String)> =
            std::mem::take(&mut *self.inbox.lock().unwrap_or_else(|e| e.into_inner()));
        (!messages.is_empty()).then(|| {
            messages
                .iter()
                .map(|(from, m)| format!("<message from=\"{from}\">\n{m}\n</message>"))
                .collect::<Vec<_>>()
                .join("\n")
        })
    }

    pub fn has_messages(&self) -> bool {
        !self
            .inbox
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .is_empty()
    }
}

/// The subagent's `message_main` tool.
pub fn message_main(link: &Link, input: &Value) -> ToolOutput {
    let text = input["message"]
        .as_str()
        .unwrap_or_default()
        .trim()
        .to_owned();
    let kind = input["kind"].as_str().unwrap_or("progress").to_owned();
    if text.is_empty() {
        return ToolOutput::error("The message is empty.", "Message main".into());
    }
    let label = {
        let mut info = lock(&link.info);
        info.notes.push(Note {
            kind: kind.clone(),
            text: text.clone(),
        });
        info.label()
    };
    link.bump();
    ToolOutput::ok(
        if kind == "question" || kind == "blocked" {
            "Sent. A reply, if any, reaches you at a later step; keep going with what you can."
                .to_owned()
        } else {
            "Sent.".to_owned()
        },
        ToolMeta {
            title: format!(
                "{label} \u{2192} main: {}",
                text.chars().take(80).collect::<String>()
            ),
            ..Default::default()
        },
    )
}

/// Starts a subagent for a `task` call. Returns its team entry and a receiver that
/// changes when it does.
fn start(
    config: &TurnConfig,
    env: &SubagentEnv,
    ctx: &ToolContext,
    call_id: &str,
    input: &Value,
    events: &Events,
    parent_history: &[Message],
) -> Result<(Arc<Mutex<Info>>, watch::Receiver<u64>), String> {
    let description = input["description"].as_str().unwrap_or("task").to_owned();
    let prompt = input["prompt"].as_str().unwrap_or_default().to_owned();
    let name = input["name"]
        .as_str()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_owned);
    let kind = input["subagent_type"]
        .as_str()
        .filter(|s| !s.is_empty())
        .unwrap_or("general")
        .to_owned();
    // A fork is the general agent carrying on from this conversation; it can't fork again.
    let fork = kind == "fork";
    if fork
        && config
            .team_link
            .as_ref()
            .is_some_and(|l| lock(&l.info).kind == "fork")
    {
        return Err(
            "A fork can't fork again; do the work, or start a fresh agent with a full prompt."
                .into(),
        );
    }
    let lookup = if fork { "general" } else { kind.as_str() };
    let Some(def) = env.defs.iter().find(|d| d.name == lookup).cloned() else {
        let names: Vec<&str> = env.defs.iter().map(|d| d.name.as_str()).collect();
        return Err(format!(
            "No agent named '{kind}'. Available: {}",
            names.join(", ")
        ));
    };
    if config.agent == Agent::Plan && !read_only(&def) {
        return Err(format!(
            "In Plan mode only read-only agents can run; '{kind}' can change files."
        ));
    }
    if prompt.trim().is_empty() {
        return Err("The prompt is empty.".into());
    }
    let session_key = team_key(config, ctx);
    if let Some(name) = &name
        && list(&session_key)
            .iter()
            .any(|i| i.name.as_deref() == Some(name) && i.state.busy())
    {
        return Err(format!(
            "An agent named '{name}' is already working; pick another name."
        ));
    }
    // A fork always runs on the model whose conversation it continues.
    let (model_ref, endpoint, model) = if fork {
        (
            config.model_ref.clone(),
            config.endpoint.clone(),
            config.model.clone(),
        )
    } else {
        let model_ref = crate::subagents::choose_model(
            &def,
            &env.parent_model,
            &env.registry,
            env.small_model.as_deref(),
        );
        match env.registry.resolve(&model_ref) {
            Ok((p, m)) => (model_ref, p.endpoint.clone(), m),
            Err(error) => return Err(format!("Can't start {kind}: {error:#}")),
        }
    };
    let worktree = if def.worktree && config.agent == Agent::Build {
        match Worktree::create(&ctx.cwd, call_id) {
            Ok(w) => Some(w),
            Err(error) => return Err(format!("Couldn't create a worktree: {error:#}")),
        }
    } else {
        None
    };
    let cwd = worktree
        .as_ref()
        .map(|w| w.path.clone())
        .unwrap_or_else(|| ctx.cwd.clone());

    let mut system = if fork {
        config.system.clone()
    } else {
        crate::context::SystemPrompt {
            cwd: &cwd,
            agent: Agent::Build,
            model: &model_ref,
            instructions: &env.instructions,
            skills: &ctx.skills,
        }
        .render()
    };
    if fork {
        system.push_str(
            "\n# You are a fork\nYou are a copy of the agent whose conversation you see, started to take one \
             branch of its work (the last message). Do that branch yourself, directly; don't hand it on.\n",
        );
    }
    // A fork starts from the conversation so far (minus the call that started it).
    let initial: Vec<Message> = if fork {
        parent_history.to_vec()
    } else {
        Vec::new()
    };
    system.push_str(
        "\n# Your role\nYou are a subagent: another agent started you for this task and may run others alongside you. \
         Keep it informed with message_main: a short note at real milestones, as soon as you find something it \
         should act on, and right away when you're blocked or need a decision (then keep going with what you can). \
         It may message you while you work; its messages appear before your next step, and they take priority. \
         Your final message is your report: complete and concise, with what you did, what you found, and anything \
         left open. You exit after it.\n\n",
    );
    system.push_str(&def.prompt);

    let mut policy = config.policy.clone();
    if let Some(w) = &worktree {
        policy.root = w.path.clone();
        policy.cwd = w.path.clone();
    }

    // Ids are unique across the session, nested agents included.
    let id = format!("a{}", list_all(&ctx.session_id).len() + 1);
    let mut session = Session::new(&cwd);
    session.id = format!("sub-{}", session.id);
    session.title = format!("\u{21b3} {description}");
    let info = Arc::new(Mutex::new(Info {
        id: id.clone(),
        name: name.clone(),
        kind: kind.clone(),
        model: model_ref.clone(),
        description: description.clone(),
        state: State::Queued,
        steps: 0,
        activity: "waiting for a free slot".into(),
        notes: Vec::new(),
        report: String::new(),
        session_id: session.id.clone(),
        started: Instant::now(),
        notes_told: 0,
        report_told: false,
    }));
    let inbox: Inbox = Arc::default();
    let (changed_tx, changed_rx) = watch::channel(0u64);
    let link = Arc::new(Link {
        info: info.clone(),
        inbox: inbox.clone(),
        team: session_key.clone(),
        changed: Arc::new(changed_tx),
    });

    let sub_config = TurnConfig {
        model_ref: model_ref.clone(),
        caps: env.registry.caps(&model_ref),
        hooks: config.hooks.clone(),
        endpoint,
        model,
        system,
        effort: def.effort.unwrap_or(config.effort),
        agent: if read_only(&def) {
            Agent::Plan
        } else {
            Agent::Build
        },
        max_tokens: config.max_tokens,
        max_steps: Some(env.max_steps),
        policy,
        session_rules: config.session_rules.clone(),
        approvals: config.approvals.clone(),
        context_window: env.registry.caps(&model_ref).context,
        compact_at: config.compact_at,
        trim_at: config.trim_at,
        trim_keep: config.trim_keep,
        server_compaction: config.server_compaction,
        clear_tool_results: config.clear_tool_results,
        server_web_search: config.server_web_search,
        context_tokens: 0,
        // Agents may start their own, down to max_agent_depth.
        subagents: env.child(),
        team_link: Some(link.clone()),
    };
    let sub_ctx = ToolContext {
        cwd: cwd.clone(),
        allow: def.tools.clone().map(|mut tools| {
            tools.push("message_main".into());
            tools.push("message_agent".into());
            tools
        }),
        todos: Arc::new(Mutex::new(Vec::new())),
        loaded: Default::default(),
        // Edits in the main tree belong to this turn's checkpoint, so undo covers them.
        checkpoint: if worktree.is_some() {
            Default::default()
        } else {
            ctx.checkpoint.clone()
        },
        memory: false,
        ..ctx.clone()
    };
    session.push_user(
        &format!("{description} ({kind})\n\n{prompt}"),
        prompt.clone(),
        Agent::Build,
        None,
    );

    let limit = env.limit.clone();
    let parent_events = events.clone();
    let hooks = config.hooks.clone();
    let task_info = info.clone();
    let task_link = link.clone();
    let call = call_id.to_owned();
    let task_id = id.clone();
    let handle = tokio::spawn(async move {
        let info = task_info;
        let link = task_link;
        let status = |text: String| {
            let _ = parent_events.send(AgentEvent::SubagentStatus {
                id: call.clone(),
                text,
            });
        };
        let _permit = limit.acquire_owned().await;
        {
            let mut i = lock(&info);
            i.state = State::Running;
            i.activity = format!("starting on {}", i.model);
        }
        link.bump();
        status(format!("starting on {model_ref}"));

        // Hooks hear about the new task; SubagentStart may add context to its prompt.
        let mut notices = hooks
            .run(
                "TaskCreated",
                None,
                json!({"task_id": task_id, "kind": "agent", "subagent_type": kind, "description": description}),
            )
            .await
            .notices;
        let start = hooks
            .run(
                "SubagentStart",
                Some(&kind),
                json!({"agent_id": task_id, "subagent_type": kind, "description": description}),
            )
            .await;
        notices.extend(start.notices);
        for notice in notices {
            let _ = parent_events.send(AgentEvent::Notice(notice));
        }
        let mut prompt = prompt;
        if !start.context.is_empty() {
            prompt.push_str(&format!("\n\n{}", start.context.join("\n\n")));
        }
        let mut history = initial;
        history.push(Message::user_text(prompt));
        let (tx, mut rx) = mpsc::unbounded_channel();
        let started = Instant::now();
        let work = async {
            let result = crate::agent::turn_loop(&sub_config, &mut history, &sub_ctx, &tx).await;
            drop(tx);
            result
        };
        let watch_events = async {
            while let Some(event) = rx.recv().await {
                match &event {
                    AgentEvent::ToolCall { name, input, .. } => {
                        let text = crate::tools::title(name, input);
                        let steps = {
                            let mut i = lock(&info);
                            i.steps += 1;
                            i.activity = text.clone();
                            i.steps
                        };
                        status(format!("{steps} \u{b7} {text}"));
                    }
                    AgentEvent::Approval { reason, .. } => {
                        lock(&info).activity = format!("waiting for the user's approval: {reason}");
                        status(format!("waiting for your approval: {reason}"));
                    }
                    AgentEvent::Usage(usage) => {
                        let _ = parent_events.send(AgentEvent::SubagentUsage(*usage));
                    }
                    AgentEvent::Cost(cost) => {
                        let _ = parent_events.send(AgentEvent::Cost(*cost));
                    }
                    _ => {}
                }
                session.apply(&event, Agent::Build, &model_ref);
            }
        };
        let (result, ()) = tokio::join!(work, watch_events);
        session.apply(
            &AgentEvent::Done {
                interrupted: false,
                seconds: started.elapsed().as_secs_f32(),
            },
            Agent::Build,
            &model_ref,
        );
        let _ = session.save();

        // (Its SubagentStop hooks ran as it stopped, inside the turn.)
        let finished = hooks
            .run(
                "TaskCompleted",
                None,
                json!({"task_id": task_id, "kind": "agent", "subagent_type": kind, "description": description}),
            )
            .await;
        for notice in finished.notices {
            let _ = parent_events.send(AgentEvent::Notice(notice));
        }

        let mut report = history
            .iter()
            .rev()
            .find(|m| m.role == Role::Assistant && !m.text().trim().is_empty())
            .map(|m| m.text())
            .unwrap_or_default();
        if report.trim().is_empty() {
            report = "The subagent finished without a report.".into();
        }
        let mut patch_detail = None;
        if let Some(worktree) = worktree {
            match worktree.finish(&call) {
                Ok(Some((patch, path, files))) => {
                    report.push_str(&format!(
                        "\n\nThe subagent worked in a separate git worktree. Its changes to {files} file(s) are NOT applied; \
                         they're saved as a patch at {}. Review it, then apply it with `git apply {}` if it's right.",
                        path.display(),
                        path.display()
                    ));
                    patch_detail = Some(patch);
                }
                Ok(None) => report.push_str("\n\n(The subagent made no file changes.)"),
                Err(error) => report.push_str(&format!(
                    "\n\n(Couldn't collect the worktree's changes: {error:#})"
                )),
            }
        }
        {
            let mut i = lock(&info);
            i.report = report;
            i.state = match &result {
                Ok(()) => State::Done,
                Err(error) => State::Failed(format!("{error:#}")),
            };
            i.activity = "finished".into();
        }
        link.bump();
        patch_detail
    });
    let abort = handle.abort_handle();
    with_team(&session_key, |team| {
        team.push(Member {
            info: info.clone(),
            inbox,
            changed: changed_rx.clone(),
            task: abort,
        })
    });
    // Keep the patch for the waiting `task` call (it shows it as a diff).
    let patches = PATCHES.get_or_init(Default::default);
    let key = format!("{session_key}/{id}");
    let slot = patches.clone();
    tokio::spawn(async move {
        if let Ok(Some(patch)) = handle.await {
            slot.lock()
                .unwrap_or_else(|e| e.into_inner())
                .push((key, patch));
        }
    });
    Ok((info, changed_rx))
}

/// Worktree patches of finished agents, for the `task` call that shows them.
/// (session/agent, patch) pairs.
type Patches = Arc<Mutex<Vec<(String, String)>>>;

static PATCHES: std::sync::OnceLock<Patches> = std::sync::OnceLock::new();

fn take_patch(session: &str, id: &str) -> Option<String> {
    let key = format!("{session}/{id}");
    let mut patches = PATCHES.get()?.lock().unwrap_or_else(|e| e.into_inner());
    let pos = patches.iter().position(|(k, _)| *k == key)?;
    Some(patches.remove(pos).1)
}

/// Stops an agent if the `task` call waiting on it is dropped (the user interrupted).
struct StopOnDrop {
    session: String,
    id: String,
    armed: bool,
}

impl Drop for StopOnDrop {
    fn drop(&mut self) {
        if self.armed {
            stop(&self.session, &self.id);
        }
    }
}

fn stop(session: &str, key: &str) -> Option<String> {
    with_team(session, |team| {
        let member = team
            .iter()
            .rev()
            .find(|m| matches_key(&lock(&m.info), key))?;
        let mut i = lock(&member.info);
        if i.state.busy() {
            member.task.abort();
            i.state = State::Stopped;
            i.activity = "stopped".into();
            i.report_told = true;
        }
        Some(i.label())
    })
}

/// Runs a `task` call: starts the agent, and unless it's in the background, waits for
/// its report.
pub async fn run(
    config: &TurnConfig,
    env: &SubagentEnv,
    ctx: &ToolContext,
    call_id: &str,
    input: &Value,
    events: &Events,
    parent_history: &[Message],
) -> ToolOutput {
    let description = input["description"].as_str().unwrap_or("task");
    let kind = input["subagent_type"]
        .as_str()
        .filter(|s| !s.is_empty())
        .unwrap_or("general");
    let title = format!("Task: {description} ({kind})");
    let (info, mut changed) = match start(config, env, ctx, call_id, input, events, parent_history)
    {
        Ok(started) => started,
        Err(error) => return ToolOutput::error(error, title),
    };
    let (id, label, model, session_id) = {
        let i = lock(&info);
        (
            i.id.clone(),
            i.label(),
            i.model.clone(),
            i.session_id.clone(),
        )
    };
    let title = format!("Task: {description} ({kind} \u{b7} {model} \u{b7} {id})");
    let meta = |detail: Option<String>, kind: DisplayKind| ToolMeta {
        title: title.clone(),
        kind,
        detail,
        link: Some(session_id.clone()),
    };
    if input["run_in_background"].as_bool() == Some(true) {
        return ToolOutput::ok(
            format!(
                "Started {label} in the background. Steer it with send_message (to: \"{id}\"), check on it with \
                 agent_status, and collect its report with wait_for_agents."
            ),
            meta(None, DisplayKind::Inline),
        );
    }
    let mut guard = StopOnDrop {
        session: team_key(config, ctx),
        id: id.clone(),
        armed: true,
    };
    while lock(&info).state.busy() {
        if changed.changed().await.is_err() {
            break;
        }
    }
    guard.armed = false;
    let mut i = lock(&info);
    i.report_told = true;
    i.notes_told = i.notes.len();
    let notes: String = i
        .notes
        .iter()
        .map(|n| format!("\n- [{}] {}", n.kind, n.text))
        .collect();
    let report = i.report.clone();
    let state = i.state.clone();
    drop(i);
    let patch = take_patch(&team_key(config, ctx), &id);
    let kind = if patch.is_some() {
        DisplayKind::Diff
    } else {
        DisplayKind::Inline
    };
    match state {
        State::Failed(why) => ToolOutput {
            content: format!("{label} failed: {why}\n\n{report}"),
            is_error: true,
            meta: meta(Some(why), kind),
        },
        State::Stopped => ToolOutput::error(
            format!("{label} was stopped before it reported."),
            title.clone(),
        ),
        _ => ToolOutput {
            content: if notes.is_empty() {
                report
            } else {
                format!("{report}\n\nNotes it sent while working:{notes}")
            },
            is_error: false,
            meta: meta(patch, kind),
        },
    }
}

/// The main agent's team tools: send_message, agent_status, wait_for_agents, stop_agent.
pub async fn team_tool(name: &str, input: &Value, team: &str) -> ToolOutput {
    let session = team;
    let ok = |content: String, title: String| {
        ToolOutput::ok(
            content,
            ToolMeta {
                title,
                ..Default::default()
            },
        )
    };
    match name {
        "send_message" => {
            let to = input["to"].as_str().unwrap_or_default();
            let message = input["message"].as_str().unwrap_or_default().trim();
            let title = format!("Message \u{2192} {to}");
            if message.is_empty() {
                return ToolOutput::error("The message is empty.", title);
            }
            let Some((info, inbox, _)) = find(session, to) else {
                return ToolOutput::error(
                    format!("No subagent '{to}'. agent_status lists them."),
                    title,
                );
            };
            let (state, label) = {
                let i = lock(&info);
                (i.state.clone(), i.label())
            };
            if !state.busy() {
                return ToolOutput::error(
                    format!(
                        "{label} is {} and has exited (agents exit once they report). Start a new task, with what it \
                         needs from that report, to continue the work.",
                        state.label()
                    ),
                    title,
                );
            }
            inbox
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .push(("the agent that started you".into(), bounded(message)));
            ok(
                format!(
                    "Sent to {label}; it sees the message at its next step. Its reply or report comes through agent_status / wait_for_agents."
                ),
                title,
            )
        }
        "agent_status" => {
            let team = list(session);
            if team.is_empty() {
                return ok("You haven't started any subagents.".into(), "Agents".into());
            }
            if let Some(key) = input["id"].as_str().filter(|k| !k.trim().is_empty()) {
                let Some((info, _, _)) = find(session, key) else {
                    return ToolOutput::error(
                        format!("No subagent '{key}'."),
                        "Agent status".into(),
                    );
                };
                let mut i = lock(&info);
                i.notes_told = i.notes.len();
                if !i.state.busy() {
                    i.report_told = true;
                }
                let mut text = format!(
                    "{} \u{b7} {} \u{b7} {} \u{b7} {} \u{b7} {} steps \u{b7} {}s\nTask: {}\nNow: {}",
                    i.label(),
                    i.kind,
                    i.model,
                    i.state.label(),
                    i.steps,
                    i.started.elapsed().as_secs(),
                    i.description,
                    i.activity
                );
                for note in &i.notes {
                    text.push_str(&format!("\n[{}] {}", note.kind, note.text));
                }
                if !i.report.is_empty() {
                    text.push_str(&format!("\n\nReport:\n{}", i.report));
                }
                return ok(text, format!("Agent {}", i.label()));
            }
            let lines: Vec<String> = with_team(session, |team| {
                team.iter()
                    .map(|m| {
                        let mut i = lock(&m.info);
                        let new = i.take_new_notes();
                        let mut line = format!(
                            "- {} \u{b7} {} \u{b7} {} \u{b7} {} steps \u{b7} {}",
                            i.label(),
                            i.kind,
                            i.state.label(),
                            i.steps,
                            if i.state.busy() {
                                i.activity.clone()
                            } else {
                                i.description.clone()
                            }
                        );
                        for note in new {
                            line.push_str(&format!("\n  [{}] {}", note.kind, note.text));
                        }
                        if !i.state.busy() && !i.report_told {
                            i.report_told = true;
                            let preview: String = i.report.chars().take(600).collect();
                            line.push_str(&format!("\n  Report: {preview}"));
                        }
                        line
                    })
                    .collect()
            });
            ok(lines.join("\n"), format!("Agents ({})", team.len()))
        }
        "wait_for_agents" => {
            let timeout = Duration::from_secs(
                input["timeout_seconds"]
                    .as_u64()
                    .unwrap_or(900)
                    .clamp(5, 3600),
            );
            let any_note = input["until"].as_str() == Some("any_note");
            let wanted: Vec<String> = input["ids"]
                .as_array()
                .map(|a| {
                    a.iter()
                        .filter_map(|v| v.as_str().map(str::to_owned))
                        .collect()
                })
                .unwrap_or_default();
            let mut members: Vec<(Arc<Mutex<Info>>, watch::Receiver<u64>)> = Vec::new();
            if wanted.is_empty() {
                with_team(session, |team| {
                    for m in team.iter() {
                        let i = lock(&m.info);
                        if i.state.busy() || !i.report_told || i.notes_told < i.notes.len() {
                            members.push((m.info.clone(), m.changed.clone()));
                        }
                    }
                });
            } else {
                for key in &wanted {
                    match find(session, key) {
                        Some((info, _, changed)) => members.push((info, changed)),
                        None => {
                            return ToolOutput::error(
                                format!("No subagent '{key}'."),
                                "Wait".into(),
                            );
                        }
                    }
                }
            }
            if members.is_empty() {
                return ok(
                    "No subagents are working, and you've seen every report.".into(),
                    "Wait".into(),
                );
            }
            let deadline = tokio::time::Instant::now() + timeout;
            // With any_note, notes not read yet count as news right away.
            let notes_before: Vec<usize> = members
                .iter()
                .map(|(i, _)| {
                    let i = lock(i);
                    if any_note {
                        i.notes_told.min(i.notes.len())
                    } else {
                        i.notes.len()
                    }
                })
                .collect();
            loop {
                let all_done = members.iter().all(|(i, _)| !lock(i).state.busy());
                let new_note = members
                    .iter()
                    .zip(&notes_before)
                    .any(|((i, _), before)| lock(i).notes.len() > *before);
                if all_done || (any_note && new_note) {
                    break;
                }
                let waits = members.iter_mut().map(|(_, rx)| Box::pin(rx.changed()));
                if tokio::time::timeout_at(deadline, futures::future::select_all(waits))
                    .await
                    .is_err()
                {
                    break;
                }
            }
            let mut text = String::new();
            for (info, _) in &members {
                let mut i = lock(info);
                let notes = i.take_new_notes();
                text.push_str(&format!("## {} \u{b7} {}\n", i.label(), i.state.label()));
                for note in notes {
                    text.push_str(&format!("[{}] {}\n", note.kind, note.text));
                }
                if i.state.busy() {
                    text.push_str(&format!("Still working: {}\n\n", i.activity));
                } else {
                    i.report_told = true;
                    text.push_str(&format!("{}\n\n", i.report.trim()));
                }
            }
            let still = members.iter().filter(|(i, _)| lock(i).state.busy()).count();
            if still > 0 && tokio::time::Instant::now() >= deadline {
                text.push_str(&format!("(Timed out with {still} still working.)"));
            }
            ok(
                text.trim_end().to_owned(),
                format!("Waited for {} agent(s)", members.len()),
            )
        }
        "stop_agent" => {
            let key = input["id"].as_str().unwrap_or_default().to_owned();
            let Some((info, inbox, _)) = find(session, &key) else {
                return ToolOutput::error(format!("No subagent '{key}'."), "Stop agent".into());
            };
            let (label, busy) = {
                let i = lock(&info);
                (i.label(), i.state.busy())
            };
            if !busy {
                return ok(
                    format!("{label} has already finished."),
                    format!("Stop {label}"),
                );
            }
            if input["force"].as_bool() == Some(true) {
                stop(session, &key);
                return ok(format!("Stopped {label}."), format!("Stop {label}"));
            }
            // Gracefully: ask it to wrap up, and stop it if it hasn't reported in 90 seconds.
            inbox.lock().unwrap_or_else(|e| e.into_inner()).push((
                "the agent that started you".into(),
                "Please wrap up now: don't start anything new, and send your report with what you have, \
                 including what's left undone."
                    .into(),
            ));
            let (team, id) = (session.to_owned(), key.clone());
            tokio::spawn(async move {
                tokio::time::sleep(Duration::from_secs(90)).await;
                stop(&team, &id);
            });
            ok(
                format!(
                    "Asked {label} to wrap up and report; it will be stopped in 90 seconds if it hasn't."
                ),
                format!("Stop {label}"),
            )
        }
        other => ToolOutput::error(format!("Unknown team tool {other}"), other.to_owned()),
    }
}

/// A temporary git worktree for one subagent.
struct Worktree {
    repo: PathBuf,
    path: PathBuf,
    base: String,
    /// Removed already (by `finish`).
    removed: bool,
}

impl Drop for Worktree {
    /// An interrupted subagent leaves no worktree behind.
    fn drop(&mut self) {
        if !self.removed
            && let Ok(top) = git(&self.path, &["rev-parse", "--show-toplevel"])
        {
            let _ = git(&self.repo, &["worktree", "remove", "--force", &top]);
        }
    }
}

fn git(dir: &Path, args: &[&str]) -> anyhow::Result<String> {
    let output = std::process::Command::new("git")
        .arg("-C")
        .arg(dir)
        .args([
            "-c",
            "core.hooksPath=/dev/null",
            "-c",
            "core.fsmonitor=false",
        ])
        .args(args)
        .output()?;
    if !output.status.success() {
        anyhow::bail!(
            "git {}: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_owned())
}

impl Worktree {
    fn create(cwd: &Path, id: &str) -> anyhow::Result<Worktree> {
        let repo = PathBuf::from(git(cwd, &["rev-parse", "--show-toplevel"])?);
        // Start from the working tree as it is now (tracked changes included), not just HEAD.
        let stash = git(&repo, &["stash", "create"]).unwrap_or_default();
        let base = if stash.is_empty() {
            git(&repo, &["rev-parse", "HEAD"])?
        } else {
            stash
        };
        let safe_id: String = id
            .chars()
            .filter(|c| c.is_ascii_alphanumeric())
            .take(24)
            .collect();
        let path = crate::config::data_dir()
            .join("worktrees")
            .join(format!("{}-{safe_id}", uuid::Uuid::new_v4().simple()));
        std::fs::create_dir_all(path.parent().unwrap())?;
        git(
            &repo,
            &[
                "worktree",
                "add",
                "--detach",
                &path.display().to_string(),
                &base,
            ],
        )?;
        // Keep the same relative working directory inside the worktree.
        let relative = cwd.strip_prefix(&repo).unwrap_or(Path::new(""));
        let path = path.join(relative);
        Ok(Worktree {
            repo,
            path,
            base,
            removed: false,
        })
    }

    /// Collects the changes as a patch, saves it, and removes the worktree.
    fn finish(mut self, id: &str) -> anyhow::Result<Option<(String, PathBuf, usize)>> {
        let top = PathBuf::from(git(&self.path, &["rev-parse", "--show-toplevel"])?);
        let result = (|| {
            git(&top, &["add", "--all"])?;
            let patch = git(
                &top,
                &[
                    "diff",
                    "--cached",
                    "--binary",
                    "--no-color",
                    "--no-ext-diff",
                    &self.base,
                ],
            )?;
            if patch.is_empty() {
                return Ok(None);
            }
            let files = git(&top, &["diff", "--cached", "--name-only", &self.base])?
                .lines()
                .count();
            let dir = crate::config::data_dir().join("patches");
            std::fs::create_dir_all(&dir)?;
            let safe_id: String = id
                .chars()
                .filter(|c| c.is_ascii_alphanumeric())
                .take(24)
                .collect();
            let file = dir.join(format!(
                "{}-{safe_id}.patch",
                chrono::Local::now().format("%Y%m%d-%H%M%S")
            ));
            std::fs::write(&file, format!("{patch}\n"))?;
            Ok(Some((patch, file, files)))
        })();
        let _ = git(
            &self.repo,
            &["worktree", "remove", "--force", &top.display().to_string()],
        );
        self.removed = true;
        result
    }
}
