//! `crack run`: one turn without a window, for scripts and CI pipelines.
//!
//! Text mode streams the answer to stdout and tool activity to stderr. JSON mode
//! writes one JSON event per line to stdout.

use crate::agent::{self, AgentEvent};
use crate::session::Session;
use crate::tools::Todo;
use crate::workspace::{Selection, Workspace};
use serde_json::json;
use std::io::{IsTerminal, Read, Write};
use std::sync::{Arc, Mutex};
use tokio::sync::{mpsc, watch};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Format {
    Text,
    Json,
}

pub struct RunOptions {
    pub prompt: String,
    pub selection: Selection,
    pub format: Format,
    pub max_steps: Option<u32>,
    pub tools: Option<Vec<String>>,
    /// `last`, or a session id to continue.
    pub session: Option<String>,
}

/// Reads the prompt from arguments and/or piped stdin. Piped input is appended to an
/// argument prompt, so `git diff | crack run "review this"` works.
///
/// With no prompt arguments, or a `-` among them, stdin is read to the end. With a
/// prompt, stdin is only read if data (or EOF) shows up within a second: CI runners
/// and some harnesses leave stdin as a pipe that never closes, and waiting on it
/// would hang the run.
pub fn gather_prompt(args: &[String]) -> anyhow::Result<String> {
    let explicit = args.iter().any(|a| a == "-");
    let joined = args
        .iter()
        .filter(|a| *a != "-")
        .cloned()
        .collect::<Vec<_>>()
        .join(" ");
    let stdin = std::io::stdin();
    let mut piped = String::new();
    let read = if explicit || joined.trim().is_empty() {
        true
    } else if stdin.is_terminal() {
        false
    } else {
        let ready = stdin_ready(std::time::Duration::from_secs(1));
        if !ready {
            eprintln!("note: ignoring stdin (no input within 1s); pass `-` to wait for it");
        }
        ready
    };
    if read {
        stdin.lock().read_to_string(&mut piped)?;
    }
    let prompt = match (joined.trim(), piped.trim()) {
        ("", "") => anyhow::bail!("no prompt: pass it as arguments or on stdin"),
        ("", piped) => piped.to_owned(),
        (args, "") => args.to_owned(),
        (args, piped) => format!("{args}\n\n<stdin>\n{piped}\n</stdin>"),
    };
    Ok(prompt)
}

pub fn run(mut workspace: Workspace, options: RunOptions) -> anyhow::Result<i32> {
    crate::runtime::block_on(async move {
        if options.selection.model.is_none()
            || workspace.registry.providers.iter().any(|p| {
                p.local
                    && options
                        .selection
                        .model
                        .as_deref()
                        .is_some_and(|m| m.starts_with(&format!("{}/", p.id)))
            })
        {
            let client = workspace.http.clone();
            workspace.registry.discover(&client).await;
        }
        let mut selection = options.selection.clone();
        if selection.model.is_none() {
            selection.model = workspace.registry.default_model();
        }
        // MCP servers connect before the turn so their tools are on offer.
        if !workspace.mcp.is_empty() {
            workspace.mcp.connect_all().await;
            for server in workspace.mcp.snapshot() {
                if let crate::mcp::Status::Failed(why) = &server.status {
                    eprintln!("warning: MCP server '{}' failed: {why}", server.name);
                }
            }
        }
        let mut config = workspace.turn_config(&selection)?;
        config.max_steps = options.max_steps;
        let model_ref = selection.model.clone().unwrap_or_default();

        let mut session = match options.session.as_deref() {
            Some("last") => {
                match Session::list(&crate::context::project_root(&workspace.cwd)).first() {
                    Some(summary) => Session::load(&summary.id)?,
                    None => Session::new(&workspace.cwd),
                }
            }
            Some(id) => Session::load(id)?,
            None => Session::new(&workspace.cwd),
        };
        let (shown, model_text, skill) = match workspace.expand_skill(&options.prompt) {
            Some((text, name)) => (options.prompt.clone(), text, Some(name)),
            None => (options.prompt.clone(), options.prompt.clone(), None),
        };
        session.push_user(&shown, model_text, selection.agent, skill);
        config.context_tokens = session.context_tokens;
        config.hooks.set_session(&session.id);
        let untrusted = config.hooks.untrusted();
        if !untrusted.is_empty() {
            eprintln!(
                "warning: this project defines hooks that won't run until trusted ({}); run `crack hooks trust`",
                untrusted.join("; ")
            );
        }

        let todos: Arc<Mutex<Vec<Todo>>> = Arc::new(Mutex::new(session.todos.clone()));
        let tools = workspace.tool_context(&session.id, todos.clone(), options.tools.clone());
        let (events_tx, mut events) = mpsc::unbounded_channel();
        let (cancel_tx, cancel) = watch::channel(false);
        let history = session.messages.clone();
        let task = tokio::spawn(agent::run_turn(config, history, tools, events_tx, cancel));

        let format = options.format;
        let mut failed = false;
        let mut interrupted = false;
        let mut stdout = std::io::stdout();
        let mut stderr = std::io::stderr();
        let color = stderr.is_terminal() && std::env::var_os("NO_COLOR").is_none();
        let dim = |s: &str| {
            if color {
                format!("\x1b[2m{s}\x1b[0m")
            } else {
                s.to_owned()
            }
        };
        let mut at_line_start = true;

        loop {
            let event = tokio::select! {
                event = events.recv() => match event { Some(e) => e, None => break },
                _ = tokio::signal::ctrl_c() => {
                    let _ = cancel_tx.send(true);
                    continue;
                }
            };
            session.apply(&event, selection.agent, &model_ref);
            match format {
                Format::Json => {
                    let line = match &event {
                        AgentEvent::Text(t) => json!({"type": "text", "text": t}),
                        AgentEvent::Thinking(t) => json!({"type": "thinking", "text": t}),
                        AgentEvent::ToolStart { .. }
                        | AgentEvent::Message(_)
                        | AgentEvent::Approval { .. } => continue,
                        AgentEvent::ToolCall { id, name, input } => {
                            json!({"type": "tool_call", "id": id, "name": name, "input": input})
                        }
                        AgentEvent::ToolResult {
                            id,
                            is_error,
                            meta,
                            content,
                        } => json!({
                            "type": "tool_result", "id": id, "is_error": is_error,
                            "title": meta.title, "output": content,
                        }),
                        AgentEvent::Usage(u) => json!({"type": "usage", "usage": u}),
                        AgentEvent::Notice(t) => json!({"type": "notice", "text": t}),
                        AgentEvent::PromptBlocked(reason) => {
                            failed = true;
                            json!({"type": "error", "text": format!("A hook blocked this prompt: {reason}")})
                        }
                        AgentEvent::UserContext(_) => continue,
                        AgentEvent::SubagentUsage(_) => continue,
                        AgentEvent::SubagentStatus { id, text } => {
                            json!({"type": "subagent_status", "id": id, "text": text})
                        }
                        AgentEvent::Checkpoint { files, .. } => {
                            json!({"type": "files_changed", "files": files.iter().map(|(s, p)| json!({"status": s.to_string(), "path": p})).collect::<Vec<_>>()})
                        }
                        AgentEvent::Compacted { before, .. } => {
                            json!({"type": "compacted", "tokens_before": before, "tokens_after": session.context_tokens})
                        }
                        AgentEvent::Error(t) => {
                            failed = true;
                            json!({"type": "error", "text": t})
                        }
                        AgentEvent::Done {
                            interrupted: i,
                            seconds,
                        } => {
                            interrupted = *i;
                            json!({
                                "type": "done", "interrupted": i, "seconds": seconds,
                                "session": session.id, "model": model_ref, "usage": session.usage,
                                "result": session.messages.iter().rev()
                                    .find(|m| m.role == crate::providers::Role::Assistant)
                                    .map(|m| m.text()).unwrap_or_default(),
                            })
                        }
                    };
                    writeln!(stdout, "{line}")?;
                    stdout.flush()?;
                }
                Format::Text => match &event {
                    AgentEvent::Text(t) => {
                        write!(stdout, "{t}")?;
                        stdout.flush()?;
                        at_line_start = t.ends_with('\n');
                    }
                    AgentEvent::ToolCall { name, input, .. } => {
                        if !at_line_start {
                            writeln!(stdout)?;
                            at_line_start = true;
                        }
                        writeln!(
                            stderr,
                            "{}",
                            dim(&format!("→ {}", crate::tools::title(name, input)))
                        )?;
                    }
                    AgentEvent::ToolResult {
                        is_error: true,
                        content,
                        ..
                    } => {
                        let first = content.lines().next().unwrap_or_default();
                        writeln!(stderr, "{}", dim(&format!("  ✗ {first}")))?;
                    }
                    AgentEvent::Notice(t) => writeln!(stderr, "{}", dim(t))?,
                    AgentEvent::Error(t) => {
                        failed = true;
                        writeln!(stderr, "Error: {t}")?;
                    }
                    AgentEvent::Done { interrupted: i, .. } => {
                        interrupted = *i;
                        if !at_line_start {
                            writeln!(stdout)?;
                        }
                    }
                    _ => {}
                },
            }
        }
        let _ = task.await;
        session.todos = todos.lock().unwrap().clone();
        if let Err(error) = session.save() {
            eprintln!("warning: could not save the session: {error}");
        }
        for error in crate::store::get().flush() {
            eprintln!("warning: could not save the session: {error}");
        }
        Ok(if interrupted {
            130
        } else if failed {
            1
        } else {
            0
        })
    })
}

/// Whether stdin has data or is at EOF within `timeout`.
#[cfg(unix)]
fn stdin_ready(timeout: std::time::Duration) -> bool {
    let mut fd = libc::pollfd {
        fd: 0,
        events: libc::POLLIN,
        revents: 0,
    };
    // POLLHUP (writer closed) also means read_to_string returns promptly.
    let n = unsafe { libc::poll(&mut fd, 1, timeout.as_millis() as libc::c_int) };
    n > 0 && fd.revents & (libc::POLLIN | libc::POLLHUP) != 0
}

#[cfg(not(unix))]
fn stdin_ready(_timeout: std::time::Duration) -> bool {
    true
}
