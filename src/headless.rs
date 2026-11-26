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
    /// Stop once the run has cost this many dollars.
    pub max_cost: Option<f64>,
    /// Image files to attach to the prompt.
    pub images: Vec<std::path::PathBuf>,
    pub tools: Option<Vec<String>>,
    /// `last`, or a session id to continue.
    pub session: Option<String>,
    /// Ask the program running crack about approvals, over stdin and stdout.
    pub host_permissions: bool,
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
        if options.host_permissions {
            config.approvals = Some(host_approvals());
        }
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
        // A session whose last turn never finished (crack was killed) is repaired first.
        if crate::recovery::interrupted(&crate::memory::project_key(&workspace.cwd))
            .contains(&session.id)
        {
            session = crate::recovery::repair(&session.id, &workspace.cwd)?;
        }
        session.push_user(&shown, model_text, selection.agent, skill);
        let mut images = Vec::new();
        for path in &options.images {
            let media_type = match path
                .extension()
                .and_then(|e| e.to_str())
                .map(str::to_ascii_lowercase)
                .as_deref()
            {
                Some("png") => "image/png",
                Some("jpg" | "jpeg") => "image/jpeg",
                Some("webp") => "image/webp",
                Some("gif") => "image/gif",
                _ => anyhow::bail!("{} isn't a PNG, JPEG, WebP or GIF image", path.display()),
            };
            let bytes =
                std::fs::read(path).map_err(|e| anyhow::anyhow!("{}: {e}", path.display()))?;
            let name = path
                .file_name()
                .map_or("image".into(), |n| n.to_string_lossy().into_owned());
            images.push((
                name,
                media_type.to_owned(),
                base64::Engine::encode(&base64::engine::general_purpose::STANDARD, bytes),
            ));
        }
        session.attach_images(images);
        crate::recovery::mark_running(&session.id, &crate::memory::project_key(&workspace.cwd));
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
        // Subagents still working when the turn ends: wait for their news (up to
        // background_wait_secs) and carry on with it, rather than exit with work in flight.
        let ceiling =
            std::time::Duration::from_secs(workspace.config.background_wait_secs.unwrap_or(600));
        let team = crate::delegate::main_team(&session.id);
        let task = tokio::spawn(async move {
            let mut history = agent::run_turn(
                config.clone(),
                history,
                tools.clone(),
                events_tx.clone(),
                cancel.clone(),
            )
            .await;
            let deadline = tokio::time::Instant::now() + ceiling;
            while !*cancel.borrow() {
                let attention = crate::delegate::needs_attention(&team);
                let busy = crate::delegate::busy(&team);
                if busy == 0 && !attention {
                    break;
                }
                let now = tokio::time::Instant::now();
                if now >= deadline {
                    let _ = events_tx.send(AgentEvent::Notice(format!(
                        "Stopping {busy} subagent(s) still working after {}s (background_wait_secs).",
                        ceiling.as_secs()
                    )));
                    break;
                }
                if !attention {
                    crate::delegate::wait_for_change(&team, deadline - now).await;
                    continue;
                }
                let message = crate::providers::Message::user_text(
                    "Background work you started has news (below). Continue the task with it.",
                );
                history.push(message.clone());
                let _ = events_tx.send(AgentEvent::Message(message));
                history = agent::run_turn(
                    config.clone(),
                    history,
                    tools.clone(),
                    events_tx.clone(),
                    cancel.clone(),
                )
                .await;
            }
        });

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
        let mut spent = 0.0;
        let mut over_budget = false;

        loop {
            let event = tokio::select! {
                event = events.recv() => match event { Some(e) => e, None => break },
                _ = tokio::signal::ctrl_c() => {
                    let _ = cancel_tx.send(true);
                    continue;
                }
            };
            if let AgentEvent::Cost(cost) = &event {
                spent += cost;
                if let Some(max) = options.max_cost
                    && spent > max
                    && !over_budget
                {
                    over_budget = true;
                    failed = true;
                    let _ = cancel_tx.send(true);
                    eprintln!(
                        "crack: stopped: this run has cost ${spent:.2}, over --max-cost ${max:.2}"
                    );
                }
            }
            session.apply(&event, selection.agent, &model_ref);
            // Save as it goes, so a killed run keeps what it did (and can be repaired).
            if matches!(
                event,
                AgentEvent::Message(_)
                    | AgentEvent::ToolResult { .. }
                    | AgentEvent::Checkpoint { .. }
            ) {
                let _ = session.save();
            }
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
                        AgentEvent::SubagentUsage(_) | AgentEvent::Cost(_) => continue,
                        AgentEvent::SubagentStatus { id, text } => {
                            json!({"type": "subagent_status", "id": id, "text": text})
                        }
                        AgentEvent::Checkpoint { files, .. } => {
                            json!({"type": "files_changed", "files": files.iter().map(|(s, p)| json!({"status": s.to_string(), "path": p})).collect::<Vec<_>>()})
                        }
                        AgentEvent::Compacted { before, .. } => {
                            json!({"type": "compacted", "tokens_before": before, "tokens_after": session.context_tokens})
                        }
                        AgentEvent::Trimmed { outputs, freed, .. } => {
                            json!({"type": "trimmed", "outputs": outputs, "tokens_freed": freed})
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
                                "session": session.id, "model": model_ref, "usage": session.usage, "cost": session.cost,
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
        crate::recovery::clear(&session.id);
        crate::hooks::fire(
            &workspace.config,
            &workspace.cwd,
            &session.id,
            "SessionEnd",
            Some("run_finished"),
            serde_json::json!({"reason": "run_finished"}),
            true,
        );
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

/// `--permission-prompt stdio`: approvals go to the program running crack. Each one is
/// written to stdout as a line of JSON,
/// `{"type":"permission_request","id":"p1","tool":…,"input":…,"reason":…,"suggestion":…}`,
/// and answered with a line on stdin,
/// `{"type":"permission_response","id":"p1","decision":"allow"|"allow_session"|"deny","message":…}`.
/// Requests are answered in any order. If stdin closes, everything still waiting (and
/// anything asked later) is denied.
fn host_approvals() -> tokio::sync::mpsc::UnboundedSender<agent::ApprovalRequest> {
    use std::collections::HashMap;
    use std::io::BufRead;
    type Pending = Arc<Mutex<Option<HashMap<String, agent::ApprovalRequest>>>>;
    let pending: Pending = Arc::new(Mutex::new(Some(HashMap::new())));
    let (tx, mut rx) = mpsc::unbounded_channel::<agent::ApprovalRequest>();

    let reader = pending.clone();
    std::thread::spawn(move || {
        for line in std::io::stdin().lock().lines() {
            let Ok(line) = line else { break };
            let Ok(reply) = serde_json::from_str::<serde_json::Value>(&line) else {
                eprintln!("crack: ignoring a line on stdin that isn't JSON");
                continue;
            };
            if reply["type"] != "permission_response" {
                continue;
            }
            let id = reply["id"].as_str().unwrap_or_default();
            let request = reader
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .as_mut()
                .and_then(|p| p.remove(id));
            let Some(request) = request else {
                eprintln!("crack: no permission request '{id}' is waiting");
                continue;
            };
            let message = reply["message"].as_str().map(str::to_owned);
            let answer = match reply["decision"].as_str() {
                Some("allow") => crate::policy::Answer::Once,
                Some("allow_session") => crate::policy::Answer::Session(request.suggestion.clone()),
                _ => crate::policy::Answer::Deny(message),
            };
            let _ = request.respond.send(answer);
        }
        // The host is gone: nothing more can be approved.
        if let Some(waiting) = reader.lock().unwrap_or_else(|e| e.into_inner()).take() {
            for (_, request) in waiting {
                let _ = request.respond.send(crate::policy::Answer::Deny(Some(
                    "The permission host closed stdin.".into(),
                )));
            }
        }
    });

    tokio::spawn(async move {
        let mut next = 0u64;
        while let Some(request) = rx.recv().await {
            next += 1;
            let id = format!("p{next}");
            let line = serde_json::json!({
                "type": "permission_request",
                "id": id,
                "tool": request.tool,
                "input": request.input,
                "reason": request.reason,
                "suggestion": request.suggestion,
            });
            let mut guard = pending.lock().unwrap_or_else(|e| e.into_inner());
            match guard.as_mut() {
                Some(waiting) => {
                    waiting.insert(id, request);
                    drop(guard);
                    let mut out = std::io::stdout().lock();
                    let _ = writeln!(out, "{line}");
                    let _ = out.flush();
                }
                None => {
                    let _ = request.respond.send(crate::policy::Answer::Deny(Some(
                        "The permission host closed stdin.".into(),
                    )));
                }
            }
        }
    });
    tx
}
