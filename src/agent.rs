//! The agent loop: send the conversation, stream the reply, run the tools it asks
//! for, send their results back, until the model stops calling tools.

use crate::providers::{
    ChatRequest, Effort, Endpoint, Message, Part, Role, StopReason, StreamEvent, Usage,
};
use crate::tools::{self, Agent, ToolContext, ToolMeta};
use serde_json::{Value, json};
use std::time::Instant;
use tokio::sync::{mpsc, watch};

/// What the loop reports, in order. The interface and headless mode both feed these
/// into a [`crate::session::Session`].
#[derive(Clone, Debug)]
pub enum AgentEvent {
    Text(String),
    Thinking(String),
    /// A tool call began streaming.
    ToolStart {
        id: String,
        name: String,
    },
    /// A tool call's input is complete and it is about to run.
    ToolCall {
        id: String,
        name: String,
        input: Value,
    },
    ToolResult {
        id: String,
        is_error: bool,
        meta: ToolMeta,
        content: String,
    },
    /// A message to append to the model history.
    Message(Message),
    Usage(Usage),
    Notice(String),
    /// A tool call is waiting for approval.
    Approval {
        id: String,
        reason: String,
    },
    /// The turn changed files: the snapshots on either side.
    Checkpoint {
        before: String,
        after: String,
        files: Vec<(char, String)>,
    },
    /// A UserPromptSubmit hook blocked the prompt; take it back out.
    PromptBlocked(String),
    /// Hooks added context to the user's message.
    UserContext(String),
    /// The history was replaced by a summary to make room.
    Compacted {
        messages: Vec<Message>,
        before: u64,
    },
    Error(String),
    Done {
        interrupted: bool,
        seconds: f32,
    },
}

#[derive(Clone)]
pub struct TurnConfig {
    pub endpoint: Endpoint,
    /// The user's hooks (`[[hooks.*]]`), with the session id set.
    pub hooks: crate::hooks::Hooks,
    pub model: String,
    pub system: String,
    pub effort: Effort,
    pub agent: Agent,
    pub max_tokens: u32,
    /// Stop after this many model requests (headless runs).
    pub max_steps: Option<u32>,
    /// What may run without asking.
    pub policy: crate::policy::Policy,
    /// "Allow for this session" answers, shared with the tab so they outlive the turn.
    pub session_rules: std::sync::Arc<std::sync::Mutex<Vec<String>>>,
    /// Where to ask for approval; `None` (headless) refuses what would need it.
    pub approvals: Option<mpsc::UnboundedSender<ApprovalRequest>>,
    /// The model's context window, when known; compaction needs it.
    pub context_window: Option<u64>,
    /// Compact when the context passes this share of the window.
    pub compact_at: f64,
    /// How full the context was at the end of the last turn.
    pub context_tokens: u64,
}

/// A tool call waiting for the user's yes or no.
pub struct ApprovalRequest {
    pub tool: String,
    pub input: Value,
    pub reason: String,
    /// The rule "allow for this session" would add, e.g. `Bash(cargo test *)`.
    pub suggestion: String,
    pub respond: tokio::sync::oneshot::Sender<crate::policy::Answer>,
}

pub type Events = mpsc::UnboundedSender<AgentEvent>;

/// Runs one user turn to completion. `history` already ends with the user's message.
pub async fn run_turn(
    config: TurnConfig,
    mut history: Vec<Message>,
    tools: ToolContext,
    events: Events,
    mut cancel: watch::Receiver<bool>,
) {
    let started = Instant::now();
    let interrupted = tokio::select! {
        result = turn_loop(&config, &mut history, &tools, &events) => {
            if let Err(error) = result {
                let _ = events.send(AgentEvent::Error(format!("{error:#}")));
            }
            false
        }
        _ = async {
            // A dropped sender means nobody can cancel: wait forever.
            if cancel.wait_for(|c| *c).await.is_err() {
                std::future::pending::<()>().await;
            }
        } => true,
    };
    if interrupted {
        // A tool_use without a tool_result makes the next request invalid.
        if let Some(last) = history.last()
            && last.role == Role::Assistant
        {
            let results: Vec<Part> = last
                .parts
                .iter()
                .filter_map(|p| match p {
                    Part::ToolUse { id, .. } => Some(Part::ToolResult {
                        id: id.clone(),
                        content: "Interrupted by the user before this tool finished.".into(),
                        is_error: true,
                    }),
                    _ => None,
                })
                .collect();
            if !results.is_empty() {
                let _ = events.send(AgentEvent::Message(Message {
                    role: Role::User,
                    parts: results,
                }));
            }
        }
    }
    // Even an interrupted turn records what it changed, so it can be undone.
    let tracker = tools.checkpoint.clone();
    if let Ok(Some((before, after, files))) =
        tokio::task::spawn_blocking(move || tracker.finish()).await
    {
        let _ = events.send(AgentEvent::Checkpoint {
            before,
            after,
            files,
        });
    }
    let _ = events.send(AgentEvent::Done {
        interrupted,
        seconds: started.elapsed().as_secs_f32(),
    });
}

async fn turn_loop(
    config: &TurnConfig,
    history: &mut Vec<Message>,
    ctx: &ToolContext,
    events: &Events,
) -> anyhow::Result<()> {
    let client = ctx.http.clone();
    let specs = tools::specs(config.agent, ctx);
    let mut steps = 0;
    let mut context = config.context_tokens;
    let mut compact_failed = false;
    let mut just_compacted = false;
    let mut stop_continuations = 0;

    // Hooks for the start of a session and for the prompt.
    let prompt = history.last().map(|m| m.text()).unwrap_or_default();
    let mut added = Vec::new();
    if history.len() == 1 {
        let outcome = config
            .hooks
            .run("SessionStart", None, json!({"source": "startup"}))
            .await;
        notify(events, &outcome.notices);
        added.extend(outcome.context);
    }
    let outcome = config
        .hooks
        .run("UserPromptSubmit", None, json!({"prompt": prompt}))
        .await;
    notify(events, &outcome.notices);
    if let Some(reason) = outcome.block.or(outcome.stop) {
        let _ = events.send(AgentEvent::PromptBlocked(reason));
        return Ok(());
    }
    added.extend(outcome.context);
    if !added.is_empty() {
        let text = added.join("\n\n");
        if let Some(last) = history.last_mut() {
            last.parts.push(Part::Text { text: text.clone() });
        }
        let _ = events.send(AgentEvent::UserContext(text));
    }

    loop {
        // Make room before the conversation outgrows the context window.
        if let Some(window) = config.context_window
            && context as f64 > window as f64 * config.compact_at
            && history.len() > 1
            && !compact_failed
        {
            let outcome = config
                .hooks
                .run("PreCompact", None, json!({"trigger": "auto"}))
                .await;
            notify(events, &outcome.notices);
            match compact(config, history, &client).await {
                Ok(summary) => {
                    let _ = events.send(AgentEvent::Compacted {
                        messages: summary.clone(),
                        before: context,
                    });
                    *history = summary;
                    just_compacted = true;
                }
                Err(error) => {
                    // Don't try again every step; the request may still fit.
                    compact_failed = true;
                    let _ = events.send(AgentEvent::Notice(format!(
                        "Couldn't compact the conversation: {error:#}"
                    )));
                }
            }
        }
        steps += 1;
        if let Some(max) = config.max_steps
            && steps > max
        {
            let _ = events.send(AgentEvent::Notice(format!(
                "Stopped after {max} model requests (--max-steps)."
            )));
            return Ok(());
        }

        let mut sink = |event: StreamEvent| {
            let _ = events.send(match event {
                StreamEvent::Text(text) => AgentEvent::Text(text),
                StreamEvent::Thinking(text) => AgentEvent::Thinking(text),
                StreamEvent::ToolStart { id, name } => AgentEvent::ToolStart { id, name },
            });
        };
        let completion = config
            .endpoint
            .complete(
                &client,
                ChatRequest {
                    model: &config.model,
                    system: &config.system,
                    messages: history,
                    tools: &specs,
                    effort: config.effort,
                    max_tokens: config.max_tokens,
                },
                &mut sink,
            )
            .await?;

        let _ = events.send(AgentEvent::Usage(completion.usage));
        context = completion.usage.context() + completion.usage.output_tokens;
        if just_compacted {
            just_compacted = false;
            if let Some(window) = config.context_window
                && context as f64 > window as f64 * config.compact_at
            {
                // The summary alone is too big; compacting again won't help.
                compact_failed = true;
                let _ = events.send(AgentEvent::Notice(
                    "The context is still nearly full after compacting; start a new session soon."
                        .into(),
                ));
            }
        }
        history.push(completion.message.clone());
        let _ = events.send(AgentEvent::Message(completion.message.clone()));

        match &completion.stop {
            StopReason::Refusal(explanation) => {
                let _ = events.send(AgentEvent::Error(format!(
                    "The model declined this request{}",
                    explanation
                        .as_ref()
                        .map(|e| format!(": {e}"))
                        .unwrap_or_else(|| ".".into())
                )));
                return Ok(());
            }
            StopReason::MaxTokens => {
                let _ = events.send(AgentEvent::Notice(
                    "The response hit the output token limit.".into(),
                ));
            }
            _ => {}
        }

        let calls: Vec<(String, String, Value)> = completion
            .message
            .parts
            .iter()
            .filter_map(|p| match p {
                Part::ToolUse { id, name, input } => {
                    Some((id.clone(), name.clone(), input.clone()))
                }
                _ => None,
            })
            .collect();
        if calls.is_empty() {
            // A Stop hook can send the model back to work (a few times at most).
            let outcome = config
                .hooks
                .run(
                    "Stop",
                    None,
                    json!({"stop_hook_active": stop_continuations > 0}),
                )
                .await;
            notify(events, &outcome.notices);
            match outcome.block {
                Some(reason) if stop_continuations < 3 && outcome.stop.is_none() => {
                    stop_continuations += 1;
                    let message =
                        Message::user_text(format!("A Stop hook asked you to continue: {reason}"));
                    history.push(message.clone());
                    let _ = events.send(AgentEvent::Message(message));
                    continue;
                }
                _ => return Ok(()),
            }
        }
        // Truncated tool input can't be trusted; ask for the call again.
        if completion.stop == StopReason::MaxTokens {
            let results = calls
                .iter()
                .map(|(id, _, _)| Part::ToolResult {
                    id: id.clone(),
                    content: "The response was cut off by max_tokens before this tool call was complete; it was not run. Issue it again, smaller if needed.".into(),
                    is_error: true,
                })
                .collect();
            let message = Message {
                role: Role::User,
                parts: results,
            };
            history.push(message.clone());
            let _ = events.send(AgentEvent::Message(message));
            continue;
        }

        let mut stop_after_tools = false;
        let mut results = Vec::new();
        for (id, name, input) in calls {
            let _ = events.send(AgentEvent::ToolCall {
                id: id.clone(),
                name: name.clone(),
                input: input.clone(),
            });
            let mut input = input;
            let tool_input = crate::hooks::claude_input(&input);
            let pre = config
                .hooks
                .run(
                    "PreToolUse",
                    Some(&name),
                    json!({"tool_input": tool_input, "permission_mode": config.policy.mode.label()}),
                )
                .await;
            notify(events, &pre.notices);
            if let Some(updated) = &pre.updated_input {
                input = crate::hooks::crack_input(updated);
            }
            let blocked = pre.block.is_some();
            let mut output = if let Some(reason) = pre.block {
                tools::ToolOutput::refused(
                    format!("Blocked by a hook: {reason}"),
                    tools::title(&name, &input),
                )
            } else {
                match permit(config, &id, &name, &input, events, pre.permission).await {
                    Ok(()) => tools::run(&name, &input, ctx, config.agent).await,
                    Err(refusal) => refusal,
                }
            };
            // Like Claude Code, PostToolUse runs only for tools that ran.
            let post = if blocked {
                crate::hooks::Outcome::default()
            } else {
                config
                .hooks
                .run(
                    "PostToolUse",
                    Some(&name),
                    json!({
                        "tool_input": crate::hooks::claude_input(&input),
                        "tool_response": {"output": output.content, "is_error": output.is_error},
                    }),
                )
                .await
            };
            notify(events, &post.notices);
            // What hooks say after a tool goes to the model with its result.
            for note in post.block.iter().chain(&post.context) {
                output.content.push_str(&format!("\n\n[hook] {note}"));
            }
            if let Some(reason) = pre.stop.or(post.stop) {
                let _ = events.send(AgentEvent::Notice(format!(
                    "A hook ended the turn: {reason}"
                )));
                stop_after_tools = true;
            }
            let _ = events.send(AgentEvent::ToolResult {
                id: id.clone(),
                is_error: output.is_error,
                meta: output.meta,
                content: output.content.clone(),
            });
            results.push(Part::ToolResult {
                id,
                content: output.content,
                is_error: output.is_error,
            });
        }
        // All results go back in one message.
        let message = Message {
            role: Role::User,
            parts: results,
        };
        history.push(message.clone());
        let _ = events.send(AgentEvent::Message(message));
        if stop_after_tools {
            return Ok(());
        }
    }
}

/// Checks a tool call against the policy and asks the user when it needs a yes.
/// `Err` carries the result the model gets instead of running the tool.
async fn permit(
    config: &TurnConfig,
    id: &str,
    name: &str,
    input: &Value,
    events: &Events,
    hook: Option<crate::hooks::Permission>,
) -> Result<(), tools::ToolOutput> {
    use crate::policy::{Answer, Decision};
    let rules = config.session_rules.lock().unwrap().clone();
    let decision = config
        .policy
        .clone()
        .with_session_rules(&rules)
        .check(name, input);
    // A PreToolUse hook can answer the question (allow) or insist on one (ask);
    // deny rules still win.
    let decision = match (decision, hook) {
        (Decision::Ask(_), Some(crate::hooks::Permission::Allow)) => Decision::Allow,
        (Decision::Allow, Some(crate::hooks::Permission::Ask)) => {
            Decision::Ask("a hook asked for confirmation".into())
        }
        (decision, _) => decision,
    };
    let title = tools::title(name, input);
    let refuse = |text: String| tools::ToolOutput::refused(text, title.clone());
    match decision {
        Decision::Allow => Ok(()),
        Decision::Deny(reason) => Err(refuse(format!(
            "Not allowed: {reason}. Don't retry this; if you need it, ask the user to change the permission rules."
        ))),
        Decision::Ask(reason) => {
            let Some(approvals) = &config.approvals else {
                return Err(refuse(format!(
                    "This needs a person's approval ({reason}), and this headless run can't ask. \
                     Work without it, or tell the user which permission rule to add \
                     (`[permissions] allow` in crack's config) or which --permission-mode to use."
                )));
            };
            let suggestion =
                crate::policy::suggest_rule(name, input, &config.policy.root, &config.policy.cwd);
            // Notification hooks run alongside, without holding up the question.
            let hooks = config.hooks.clone();
            let message = format!("crack needs your permission: {}", tools::title(name, input));
            tokio::spawn(async move {
                hooks
                    .run("Notification", None, json!({"message": message}))
                    .await;
            });
            let (respond, answer) = tokio::sync::oneshot::channel();
            let _ = events.send(AgentEvent::Approval {
                id: id.to_owned(),
                reason: reason.clone(),
            });
            let request = ApprovalRequest {
                tool: name.to_owned(),
                input: input.clone(),
                reason,
                suggestion,
                respond,
            };
            if approvals.send(request).is_err() {
                return Err(refuse(
                    "Couldn't ask for approval; the window is gone.".into(),
                ));
            }
            match answer.await {
                Ok(Answer::Once) => Ok(()),
                Ok(Answer::Session(rule)) | Ok(Answer::Always(rule)) => {
                    config.session_rules.lock().unwrap().push(rule);
                    Ok(())
                }
                Ok(Answer::Deny(note)) => Err(refuse(match note {
                    Some(note) => format!("The user denied this: {note}"),
                    None => {
                        "The user denied this. Don't retry it; ask what they'd like instead.".into()
                    }
                })),
                Err(_) => Err(refuse("The approval was cancelled.".into())),
            }
        }
    }
}

const SUMMARY_PROMPT: &str = "\
Summarize this conversation so that a fresh assistant can continue the work without \
seeing it. Include, under short markdown headings:
- the user's goals and requests (quote exact wording where it matters);
- decisions made and why;
- files read and changed, with paths, and what changed;
- commands run and the results that matter (errors, test results);
- the todo list and anything still open or failing;
- the exact next step.
Be concise but leave out nothing needed to continue. Reply with the summary only.";

/// A rough token count (four characters a token), used until the model reports usage.
pub fn estimate_tokens(history: &[Message]) -> u64 {
    let chars: usize = history
        .iter()
        .flat_map(|m| &m.parts)
        .map(|p| match p {
            Part::Text { text } | Part::Thinking { text, .. } => text.len(),
            Part::ToolUse { input, .. } => input.to_string().len(),
            Part::ToolResult { content, .. } => content.len(),
        })
        .sum();
    (chars / 4) as u64
}

/// Asks the model for a summary of `history` and returns the history that replaces
/// it: one user message holding the summary and what to do next.
pub async fn compact(
    config: &TurnConfig,
    history: &[Message],
    client: &reqwest::Client,
) -> anyhow::Result<Vec<Message>> {
    // The newest plain user message, when the turn has only just begun.
    let latest_request = history
        .last()
        .filter(|m| m.role == Role::User)
        .and_then(|m| {
            let has_results = m.parts.iter().any(|p| matches!(p, Part::ToolResult { .. }));
            (!has_results).then(|| m.text())
        });
    let mut request_history = history.to_vec();
    // Ask in the last user message when there is one (after tool results), so roles
    // still alternate.
    match request_history.last_mut() {
        Some(last) if last.role == Role::User => last.parts.push(Part::Text {
            text: SUMMARY_PROMPT.to_owned(),
        }),
        _ => request_history.push(Message::user_text(SUMMARY_PROMPT)),
    }
    let mut sink = |_| {};
    let completion = config
        .endpoint
        .complete(
            client,
            ChatRequest {
                model: &config.model,
                system: "You write precise summaries of coding sessions.",
                messages: &request_history,
                tools: &[],
                effort: Effort::Low,
                max_tokens: 16_000,
            },
            &mut sink,
        )
        .await?;
    let summary = completion.message.text();
    if summary.trim().is_empty() {
        anyhow::bail!("the model returned an empty summary");
    }
    let next = match latest_request {
        Some(request) => format!("Now handle the user's latest request:\n\n{request}"),
        None => "Continue the task in progress from where the summary ends.".to_owned(),
    };
    Ok(vec![Message::user_text(format!(
        "This conversation was compacted to save context. Summary of everything so far:\n\n{summary}\n\n{next}"
    ))])
}

/// Shows hook output in the transcript.
fn notify(events: &Events, notices: &[String]) {
    for notice in notices {
        let _ = events.send(AgentEvent::Notice(notice.clone()));
    }
}
