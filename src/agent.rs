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
    /// What a running subagent is doing, for its `task` call's status line.
    SubagentStatus {
        id: String,
        text: String,
    },
    /// Tokens a subagent used: counted in the session's cost, not its context.
    SubagentUsage(Usage),
    /// What a request cost, in dollars (0 when the price isn't known).
    Cost(f64),
    /// A UserPromptSubmit hook blocked the prompt; take it back out.
    PromptBlocked(String),
    /// Hooks added context to the user's message.
    UserContext(String),
    /// The history was replaced by a summary to make room.
    Compacted {
        messages: Vec<Message>,
        before: u64,
    },
    /// Old tool output was replaced with short notes to make room.
    Trimmed {
        messages: Vec<Message>,
        outputs: usize,
        freed: u64,
    },
    Error(String),
    Done {
        interrupted: bool,
        seconds: f32,
    },
}

#[derive(Clone)]
pub struct TurnConfig {
    /// `provider/model`, for the usage log.
    pub model_ref: String,
    /// Prices and limits for the model.
    pub caps: crate::providers::models::ModelCaps,
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
    /// Lets the model start subagents (`task`); `None` inside a subagent.
    pub subagents: Option<std::sync::Arc<crate::delegate::SubagentEnv>>,
    /// Inside a subagent: its inbox and where `message_main` notes go.
    pub team_link: Option<std::sync::Arc<crate::delegate::Link>>,
    /// The model's context window, when known; compaction needs it.
    pub context_window: Option<u64>,
    /// Compact when the context passes this share of the window.
    pub compact_at: f64,
    /// Trim old tool output when the context passes this share of the window.
    pub trim_at: f64,
    /// How many recent tool results trimming leaves whole.
    pub trim_keep: usize,
    /// Use Claude's server-side compaction when the endpoint offers it.
    pub server_compaction: bool,
    /// Let Claude clear old tool results (context editing) when the endpoint offers it.
    pub clear_tool_results: bool,
    /// Use Anthropic's own web search tool instead of `websearch` (`web_search = "anthropic"`).
    pub server_web_search: bool,
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
    tracing::info!(model = %config.model_ref, agent = config.agent.name(), session = %tools.session_id, "turn started");
    let interrupted = tokio::select! {
        result = turn_loop(&config, &mut history, &tools, &events) => {
            if let Err(error) = result {
                let message = format!("{error:#}");
                let _ = events.send(AgentEvent::Error(message.clone()));
                // The turn ended in a failure rather than a normal stop.
                let outcome = config.hooks.run("StopFailure", None, json!({"error": message})).await;
                notify(&events, &outcome.notices);
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
    tracing::info!(
        interrupted,
        seconds = started.elapsed().as_secs_f32(),
        "turn finished"
    );
    let _ = events.send(AgentEvent::Done {
        interrupted,
        seconds: started.elapsed().as_secs_f32(),
    });
}

pub(crate) async fn turn_loop(
    config: &TurnConfig,
    history: &mut Vec<Message>,
    ctx: &ToolContext,
    events: &Events,
) -> anyhow::Result<()> {
    let client = ctx.http.clone();
    let mut specs = tools::specs(config.agent, ctx);
    if let Some(env) = &config.subagents
        && ctx
            .allow
            .as_ref()
            .is_none_or(|a| a.iter().any(|t| t == "task"))
    {
        specs.extend(env.specs(config.agent));
    }
    // Claude searches with Anthropic's own tool instead of crack's.
    if config.server_web_search && config.endpoint.is_first_party_claude() {
        specs.retain(|s| s.name != "websearch");
    }
    // A subagent can talk back to the agent that started it.
    if config.team_link.is_some() {
        specs.push(crate::delegate::message_main_spec());
    }
    let mut steps = 0;
    let mut context = config.context_tokens;
    let mut compact_failed = false;
    // After a trim that freed too little, wait for the context to grow before retrying.
    let mut trim_retry_at = 0u64;
    let mut just_compacted = false;
    let mut stop_continuations = 0;

    // Hooks for the start of a session and for the prompt.
    let prompt = history.last().map(|m| m.text()).unwrap_or_default();
    let mut added = Vec::new();
    if history.len() == 1 {
        let outcome = config
            .hooks
            .run(
                "SessionStart",
                Some("startup"),
                json!({"source": "startup"}),
            )
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
    // `@server:uri` resources and a `/server:prompt` the user typed.
    if !ctx.mcp.is_empty() {
        added.extend(ctx.mcp.expand_prompt(&prompt).await);
    }
    // A custom or plugin `/command`.
    {
        // It may run `!` snippets: off the async thread.
        let (prompt, cwd, policy, sandbox) = (
            prompt.clone(),
            ctx.cwd.clone(),
            config.policy.clone(),
            ctx.sandbox.clone(),
        );
        if let Ok(Some(text)) = tokio::task::spawn_blocking(move || {
            crate::commands::expand(&prompt, &cwd, &policy, sandbox.as_ref())
        })
        .await
        {
            added.push(text);
        }
    }
    // `@path` mentions attach files.
    added.extend(tools::expand_file_mentions(&prompt, &ctx.cwd));
    // What earlier sessions saved that bears on this request.
    if ctx.memory {
        let project = crate::memory::project_key(&ctx.cwd);
        let prompt = prompt.clone();
        let found = tokio::task::spawn_blocking(move || {
            crate::memory::get().relevant(&project, &prompt, 2_000)
        })
        .await
        .ok()
        .and_then(Result::ok)
        .unwrap_or_default();
        added.extend(crate::memory::reminder(&found));
    }
    // Background shells: what's running and what finished since the last turn.
    added.extend(crate::background::reminder(&ctx.session_id));
    // Subagents: who's working, their notes, and reports not yet seen.
    if config.subagents.is_some() {
        added.extend(crate::delegate::reminder(&ctx.session_id));
    }
    if !added.is_empty() {
        let text = added.join("\n\n");
        if let Some(last) = history.last_mut() {
            last.parts.push(Part::Text { text: text.clone() });
        }
        let _ = events.send(AgentEvent::UserContext(text));
    }

    loop {
        // A subagent: the main agent's messages arrive before its next request.
        if let Some(link) = &config.team_link
            && let Some(text) = link.take_messages()
        {
            match history.last_mut() {
                Some(last) if last.role == Role::User => {
                    last.parts.push(Part::Text { text: text.clone() })
                }
                _ => history.push(Message::user_text(text.clone())),
            }
            let _ = events.send(AgentEvent::Notice(format!(
                "Message from the main agent: {text}"
            )));
        }
        // Trim old tool output first: it keeps the conversation itself, and one trim
        // costs one prompt-cache rewrite, so it runs in batches well before compacting.
        // Skipped when Claude clears old tool results itself.
        if let Some(window) = config.context_window
            && config.trim_at < config.compact_at
            && context as f64 > window as f64 * config.trim_at
            && context >= trim_retry_at
            && !(config.clear_tool_results && config.endpoint.is_first_party_claude())
        {
            let claude = matches!(config.endpoint, crate::providers::Endpoint::Anthropic(_));
            match trim_tool_output(history, config.trim_keep, claude) {
                Some((trimmed, outputs, freed)) if freed >= window / 20 => {
                    let _ = events.send(AgentEvent::Trimmed {
                        messages: trimmed.clone(),
                        outputs,
                        freed,
                    });
                    *history = trimmed;
                    context = context.saturating_sub(freed);
                }
                _ => trim_retry_at = context + window / 20,
            }
        }
        // Make room before the conversation outgrows the context window.
        if let Some(window) = config.context_window
            && context as f64 > window as f64 * config.compact_at
            && history.len() > 1
            && !compact_failed
        {
            let outcome = config
                .hooks
                .run("PreCompact", Some("auto"), json!({"trigger": "auto"}))
                .await;
            notify(events, &outcome.notices);
            match compact(config, history, &specs, &client).await {
                Ok((summary, usage)) => {
                    record_usage(config, ctx, &usage, events);
                    let _ = events.send(AgentEvent::Compacted {
                        messages: summary.clone(),
                        before: context,
                    });
                    *history = summary;
                    just_compacted = true;
                    let after = config
                        .hooks
                        .run("PostCompact", Some("auto"), json!({"trigger": "auto"}))
                        .await;
                    notify(events, &after.notices);
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
                    compaction: false,
                    // Clear old tool results at half the window, well before compacting.
                    clear_tool_results_at: config.clear_tool_results.then(|| {
                        config
                            .context_window
                            .map_or(100_000, |w| (w / 2).max(50_000))
                    }),
                    web_search: config.server_web_search && config.endpoint.is_first_party_claude(),
                },
                &mut sink,
            )
            .await?;

        let _ = events.send(AgentEvent::Usage(completion.usage));
        record_usage(config, ctx, &completion.usage, events);
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
            // Claude paused a long server-side tool run (web search): send the conversation
            // back as it is and it carries on. No new user message.
            StopReason::Other(reason) if reason == "pause_turn" => continue,
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
            // A subagent with a message still to read isn't done yet.
            if config.team_link.as_ref().is_some_and(|l| l.has_messages()) {
                continue;
            }
            // A Stop hook can send the model back to work (a few times at most).
            let outcome = config
                .hooks
                .run(
                    // Inside a subagent its stop is a SubagentStop: the user's Stop hooks are
                    // about their own turns.
                    if config.team_link.is_some() {
                        "SubagentStop"
                    } else {
                        "Stop"
                    },
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
        // 1. In order: announce each call, run its PreToolUse hooks, get permission.
        struct Prepared {
            id: String,
            name: String,
            input: Value,
            blocked: bool,
            stop: Option<String>,
            outcome: Option<tools::ToolOutput>,
            hook_permission: Option<crate::hooks::Permission>,
        }
        let mut prepared = Vec::new();
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
            let (blocked, outcome) = match pre.block {
                Some(reason) => {
                    let refused = tools::ToolOutput::refused(
                        format!("Blocked by a hook: {reason}"),
                        tools::title(&name, &input),
                    );
                    (true, Some(refused))
                }
                None => (false, None),
            };
            prepared.push(Prepared {
                id,
                name,
                input,
                blocked,
                stop: pre.stop,
                outcome,
                hook_permission: pre.permission,
            });
        }
        // Then permission. In review mode, file changes to different files are asked about
        // together, hunk by hunk across all of them.
        let batch = review_batch(
            config,
            &prepared
                .iter()
                .map(|p| (p.name.as_str(), &p.input, p.outcome.is_some()))
                .collect::<Vec<_>>(),
        );
        if batch.len() >= 2 {
            let items: Vec<(String, String, Value)> = batch
                .iter()
                .map(|&i| {
                    (
                        prepared[i].id.clone(),
                        prepared[i].name.clone(),
                        prepared[i].input.clone(),
                    )
                })
                .collect();
            let answers = permit_batch(config, &items, events).await;
            for (&i, answer) in batch.iter().zip(answers) {
                match answer {
                    Ok(Some((tool, changed))) => {
                        prepared[i].name = tool;
                        prepared[i].input = changed;
                    }
                    Ok(None) => {}
                    Err(refusal) => prepared[i].outcome = Some(refusal),
                }
            }
        }
        for (i, p) in prepared.iter_mut().enumerate() {
            if p.outcome.is_some() || batch.len() >= 2 && batch.contains(&i) {
                continue;
            }
            match permit(config, &p.id, &p.name, &p.input, events, p.hook_permission).await {
                Ok(Some((tool, changed))) => {
                    p.name = tool;
                    p.input = changed;
                }
                Ok(None) => {}
                Err(refusal) => p.outcome = Some(refusal),
            }
        }

        // 2. Run them: subagents all at once (up to the limit), other tools in order.
        let tasks = futures::future::join_all(
            prepared
                .iter()
                .enumerate()
                .filter(|(_, p)| p.outcome.is_none() && is_team_tool(&p.name))
                .map(|(i, p)| async move {
                    let output = match &config.subagents {
                        Some(env) if p.name == "task" => {
                            crate::delegate::run(config, env, ctx, &p.id, &p.input, events).await
                        }
                        Some(_) => crate::delegate::team_tool(&p.name, &p.input, ctx).await,
                        None => tools::ToolOutput::error(
                            "Subagents can't start subagents of their own.",
                            tools::title(&p.name, &p.input),
                        ),
                    };
                    (i, output)
                }),
        );
        let others = async {
            let mut done = Vec::new();
            for (i, p) in prepared
                .iter()
                .enumerate()
                .filter(|(_, p)| p.outcome.is_none() && !is_team_tool(&p.name))
            {
                let output = match (&config.team_link, p.name.as_str()) {
                    (Some(link), "message_main") => crate::delegate::message_main(link, &p.input),
                    _ => tools::run(&p.name, &p.input, ctx, config.agent).await,
                };
                done.push((i, output));
            }
            done
        };
        let (task_outputs, other_outputs) = tokio::join!(tasks, others);
        let mut outputs: Vec<Option<tools::ToolOutput>> =
            prepared.iter_mut().map(|p| p.outcome.take()).collect();
        for (i, output) in task_outputs.into_iter().chain(other_outputs) {
            outputs[i] = Some(output);
        }

        // 3. In order: PostToolUse hooks, then the results.
        let mut results = Vec::new();
        for (p, output) in prepared.into_iter().zip(outputs) {
            let Prepared {
                id,
                name,
                input,
                blocked,
                stop,
                ..
            } = p;
            let Some(mut output) = output else { continue };
            // Like Claude Code, PostToolUse runs only for tools that ran (and a failed
            // call is a PostToolUseFailure).
            let post_event = if output.is_error {
                "PostToolUseFailure"
            } else {
                "PostToolUse"
            };
            let post = if blocked {
                crate::hooks::Outcome::default()
            } else {
                config
                    .hooks
                    .run(
                        post_event,
                        Some(&name),
                        json!({
                            "tool_input": crate::hooks::claude_input(&input),
                            "tool_response": {"output": output.content, "is_error": output.is_error},
                        }),
                    )
                    .await
            };
            notify(events, &post.notices);
            // A hook may rewrite an MCP tool's output (third-party data); crack's own tools'
            // output stays as it is.
            if let Some(replacement) = post.updated_output.clone()
                && name.starts_with("mcp__")
            {
                output.content = replacement;
            }
            // What hooks say after a tool goes to the model with its result.
            for note in post.block.iter().chain(&post.context) {
                output.content.push_str(&format!("\n\n[hook] {note}"));
            }
            if let Some(reason) = stop.or(post.stop) {
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
) -> Result<Option<(String, Value)>, tools::ToolOutput> {
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
        Decision::Allow => Ok(None),
        Decision::Deny(reason) => {
            denied(config, name, input, &reason, events).await;
            Err(refuse(format!(
                "Not allowed: {reason}. Don't retry this; if you need it, ask the user to change the permission rules."
            )))
        }
        Decision::Ask(reason) => {
            // A PermissionRequest hook can answer for the user (in headless runs too).
            let request = config
                .hooks
                .run(
                    "PermissionRequest",
                    Some(name),
                    json!({"tool_input": crate::hooks::claude_input(input), "reason": reason}),
                )
                .await;
            notify(events, &request.notices);
            match request.permission {
                Some(crate::hooks::Permission::Allow) => {
                    return Ok(request
                        .updated_input
                        .map(|changed| (name.to_owned(), crate::hooks::crack_input(&changed))));
                }
                Some(crate::hooks::Permission::Deny) => {
                    let why = request.block.unwrap_or_else(|| "a hook denied it".into());
                    denied(config, name, input, &why, events).await;
                    return Err(refuse(format!("Not allowed: {why}.")));
                }
                _ => {}
            }
            let Some(approvals) = &config.approvals else {
                if name == "git_commit" {
                    return Err(refuse(
                        "Commits need a person's approval, and this headless run can't ask. Leave the changes uncommitted and tell the user.".into(),
                    ));
                }
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
                Ok(Answer::Once) => Ok(None),
                Ok(Answer::Session(rule)) | Ok(Answer::Always(rule)) => {
                    config.session_rules.lock().unwrap().push(rule);
                    Ok(None)
                }
                // Only some hunks: write exactly those, and tell the model what was kept.
                Ok(Answer::Partial(selected)) => {
                    match crate::review::proposal(name, input, &config.policy.cwd) {
                        Some(proposal) => Ok(Some((
                            "write".to_owned(),
                            crate::review::partial_input(&proposal, &config.policy.cwd, &selected),
                        ))),
                        None => Ok(None),
                    }
                }
                Ok(Answer::Deny(note)) => {
                    denied(config, name, input, "the user said no", events).await;
                    Err(refuse(match note {
                        Some(note) => format!("The user denied this: {note}"),
                        None => {
                            "The user denied this. Don't retry it; ask what they'd like instead."
                                .into()
                        }
                    }))
                }
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
            Part::Compaction { content, .. } => content.len(),
            Part::Server { block } => block.to_string().len(),
            // Providers charge roughly 1,600 tokens for a typical screenshot.
            Part::Image { .. } => 6_400,
        })
        .sum();
    (chars / 4) as u64
}

/// Asks the model for a summary of `history` and returns the history that replaces
/// it: one user message holding the summary and what to do next.
pub async fn compact(
    config: &TurnConfig,
    history: &[Message],
    tools: &[crate::providers::ToolSpec],
    client: &reqwest::Client,
) -> anyhow::Result<(Vec<Message>, Usage)> {
    // The newest plain user message, when the turn has only just begun.
    let latest_request = history
        .last()
        .filter(|m| m.role == Role::User)
        .and_then(|m| {
            let has_results = m.parts.iter().any(|p| matches!(p, Part::ToolResult { .. }));
            (!has_results).then(|| m.text())
        });
    let next = match &latest_request {
        Some(request) => format!("Now handle the user's latest request:\n\n{request}"),
        None => "Continue the task in progress from where the summary ends.".to_owned(),
    };
    // Tokens spent on a server attempt that fell back still count.
    let mut spent = Usage::default();
    if config.server_compaction && config.endpoint.is_first_party_claude() {
        match server_compact(config, history, tools, client).await {
            Ok((Some(block), usage)) => {
                // The block must come first, as an assistant message of its own.
                return Ok((
                    vec![
                        Message {
                            role: Role::Assistant,
                            parts: vec![block],
                        },
                        Message::user_text(next),
                    ],
                    usage,
                ));
            }
            Ok((None, usage)) => {
                tracing::warn!("server compaction returned no block, summarizing here");
                spent.add(usage);
            }
            Err(error) => tracing::warn!("server compaction failed, summarizing here: {error:#}"),
        }
    }
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
                compaction: false,
                clear_tool_results_at: None,
                web_search: false,
            },
            &mut sink,
        )
        .await?;
    let summary = completion.message.text();
    if summary.trim().is_empty() {
        anyhow::bail!("the model returned an empty summary");
    }
    spent.add(completion.usage);
    Ok((
        vec![Message::user_text(format!(
            "This conversation was compacted to save context. Summary of everything so far:\n\n{summary}\n\n{next}"
        ))],
        spent,
    ))
}

/// Asks Claude to compact the conversation itself (on-demand server compaction) and
/// returns its `compaction` block. Sends the conversation's own system prompt and
/// tools, as the API asks, so the cached prefix is reused.
async fn server_compact(
    config: &TurnConfig,
    history: &[Message],
    tools: &[crate::providers::ToolSpec],
    client: &reqwest::Client,
) -> anyhow::Result<(Option<Part>, Usage)> {
    let mut sink = |_| {};
    let completion = config
        .endpoint
        .complete(
            client,
            ChatRequest {
                model: &config.model,
                system: &config.system,
                messages: history,
                tools,
                effort: config.effort,
                max_tokens: config.max_tokens,
                compaction: true,
                clear_tool_results_at: None,
                web_search: false,
            },
            &mut sink,
        )
        .await?;
    tracing::info!(
        input = completion.usage.input_tokens,
        output = completion.usage.output_tokens,
        "server compaction"
    );
    let block = completion
        .message
        .parts
        .into_iter()
        .find(|p| matches!(p, Part::Compaction { .. }));
    Ok((block, completion.usage))
}

/// Review mode: the calls (by index) whose file changes are asked about together. Two
/// or more edits or writes to different files qualify; when one file is changed twice,
/// each is asked about on its own (the second applies on top of the first).
fn review_batch(config: &TurnConfig, calls: &[(&str, &Value, bool)]) -> Vec<usize> {
    use crate::policy::{Decision, Mode};
    if config.policy.mode != Mode::Review || config.approvals.is_none() {
        return Vec::new();
    }
    let rules = config.session_rules.lock().unwrap().clone();
    let policy = config.policy.clone().with_session_rules(&rules);
    let mut paths = Vec::new();
    let mut batch = Vec::new();
    for (i, (name, input, settled)) in calls.iter().enumerate() {
        if *settled || !matches!(*name, "edit" | "write") {
            continue;
        }
        if !matches!(policy.check(name, input), Decision::Ask(_)) {
            continue;
        }
        let Some(proposal) = crate::review::proposal(name, input, &config.policy.cwd) else {
            continue;
        };
        if paths.contains(&proposal.path) {
            return Vec::new();
        }
        paths.push(proposal.path);
        batch.push(i);
    }
    batch
}

/// Asks once about several file changes (review mode) and splits the answer: each
/// change is applied whole, in part (only its ticked hunks) or not at all.
async fn permit_batch(
    config: &TurnConfig,
    items: &[(String, String, Value)],
    events: &Events,
) -> Vec<Result<Option<(String, Value)>, tools::ToolOutput>> {
    use crate::policy::Answer;
    let refuse = |name: &str, input: &Value, text: &str| {
        Err(tools::ToolOutput::refused(
            text.to_owned(),
            tools::title(name, input),
        ))
    };
    let all = |text: &str| -> Vec<Result<Option<(String, Value)>, tools::ToolOutput>> {
        items.iter().map(|(_, n, i)| refuse(n, i, text)).collect()
    };
    let Some(approvals) = &config.approvals else {
        return all("This needs a person's approval, and this run can't ask.");
    };
    let reason = format!("review mode: {} file changes", items.len());
    for (id, _, _) in items {
        let _ = events.send(AgentEvent::Approval {
            id: id.clone(),
            reason: reason.clone(),
        });
    }
    let changes: Vec<Value> = items
        .iter()
        .map(|(_, name, input)| json!({"tool": name, "input": input}))
        .collect();
    let (respond, answer) = tokio::sync::oneshot::channel();
    let request = ApprovalRequest {
        tool: "review_batch".into(),
        input: json!({ "changes": changes }),
        reason,
        suggestion: String::new(),
        respond,
    };
    if approvals.send(request).is_err() {
        return all("Couldn't ask for approval; the window is gone.");
    }
    match answer.await {
        Ok(Answer::Once) | Ok(Answer::Session(_)) | Ok(Answer::Always(_)) => {
            items.iter().map(|_| Ok(None)).collect()
        }
        Ok(Answer::Deny(note)) => all(&match note {
            Some(note) => format!("The user denied these changes: {note}"),
            None => {
                "The user denied these changes. Don't retry them; ask what they'd like instead."
                    .into()
            }
        }),
        Ok(Answer::Partial(ticks)) => {
            let cwd = &config.policy.cwd;
            let mut at = 0;
            items
                .iter()
                .map(|(_, name, input)| {
                    let Some(proposal) = crate::review::proposal(name, input, cwd) else {
                        return Ok(None);
                    };
                    let n = crate::review::hunks(&proposal.old, &proposal.new).len();
                    let mine: Vec<bool> = ticks.iter().skip(at).take(n).copied().collect();
                    at += n;
                    if mine.iter().all(|t| *t) {
                        Ok(None)
                    } else if !mine.iter().any(|t| *t) {
                        refuse(
                            name,
                            input,
                            "The user rejected this change. Don't redo it unless they ask.",
                        )
                    } else {
                        Ok(Some((
                            "write".to_owned(),
                            crate::review::partial_input(&proposal, cwd, &mine),
                        )))
                    }
                })
                .collect()
        }
        Err(_) => all("The approval was cancelled."),
    }
}

/// `task` and the team tools run concurrently (they may wait on subagents).
fn is_team_tool(name: &str) -> bool {
    name == "task" || crate::delegate::TEAM_TOOLS.contains(&name)
}

/// Tells PermissionDenied hooks a call was refused (by a rule, a hook or the user).
async fn denied(config: &TurnConfig, name: &str, input: &Value, reason: &str, events: &Events) {
    let outcome = config
        .hooks
        .run(
            "PermissionDenied",
            Some(name),
            json!({"tool_input": crate::hooks::claude_input(input), "reason": reason}),
        )
        .await;
    notify(events, &outcome.notices);
}

/// Shows hook output in the transcript.
fn notify(events: &Events, notices: &[String]) {
    for notice in notices {
        let _ = events.send(AgentEvent::Notice(notice.clone()));
    }
}

/// Logs a request's tokens and cost, and reports the cost to the session.
fn record_usage(config: &TurnConfig, ctx: &ToolContext, usage: &Usage, events: &Events) {
    let cost = log_usage(config, &ctx.session_id, &ctx.cwd, usage);
    let _ = events.send(AgentEvent::Cost(cost.unwrap_or(0.0)));
}

/// Writes a request's tokens and cost to the usage log; returns the cost when the
/// model's price is known.
pub fn log_usage(
    config: &TurnConfig,
    session: &str,
    cwd: &std::path::Path,
    usage: &Usage,
) -> Option<f64> {
    let cost = config.caps.cost(usage);
    let project = crate::memory::project_key(cwd);
    crate::usage::get().record(session, &project, &config.model_ref, usage, cost);
    cost
}

/// How a trimmed tool result starts, so it isn't trimmed twice.
const TRIM_MARK: &str = "[Output trimmed to save context:";
/// Output shorter than this stays: trimming it saves next to nothing.
const TRIM_MIN_CHARS: usize = 600;

/// Replaces the output of every tool result but the `keep` most recent with a short
/// note, keeping the calls themselves (observation masking). Returns the new history,
/// how many outputs were trimmed and roughly how many tokens that frees, or `None` when
/// nothing is worth trimming.
///
/// `drop_thinking` removes thinking from the assistant turns before the last trimmed
/// result: Claude's signed thinking is tied to the exact conversation before it, which
/// trimming changes.
pub fn trim_tool_output(
    history: &[Message],
    keep: usize,
    drop_thinking: bool,
) -> Option<(Vec<Message>, usize, u64)> {
    let results: Vec<(usize, usize)> = history
        .iter()
        .enumerate()
        .flat_map(|(m, message)| {
            message
                .parts
                .iter()
                .enumerate()
                .filter(|(_, p)| matches!(p, Part::ToolResult { .. }))
                .map(move |(p, _)| (m, p))
        })
        .collect();
    let old = results.len().checked_sub(keep).filter(|n| *n > 0)?;
    let mut out = history.to_vec();
    let mut trimmed = 0;
    let mut freed = 0u64;
    let mut last = None;
    for &(m, p) in &results[..old] {
        if let Part::ToolResult { content, .. } = &mut out[m].parts[p]
            && content.len() >= TRIM_MIN_CHARS
            && !content.starts_with(TRIM_MARK)
        {
            let note = format!(
                "{TRIM_MARK} {} lines, {} characters. Run the tool again if you need it.]",
                content.lines().count(),
                content.len()
            );
            freed += (content.len() - note.len()) as u64 / 4;
            *content = note;
            trimmed += 1;
            last = Some(m);
        }
    }
    let last = last?;
    if drop_thinking {
        for message in out[..last].iter_mut().filter(|m| m.role == Role::Assistant) {
            message
                .parts
                .retain(|p| !matches!(p, Part::Thinking { .. }));
        }
    }
    Some((out, trimmed, freed))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A conversation of `n` tool calls, each with `size` characters of output.
    fn calls(n: usize, size: usize) -> Vec<Message> {
        let mut history = vec![Message::user_text("go")];
        for i in 0..n {
            history.push(Message {
                role: Role::Assistant,
                parts: vec![
                    Part::Thinking {
                        text: "hmm".into(),
                        signature: Some("sig".into()),
                        redacted: None,
                    },
                    Part::ToolUse {
                        id: format!("t{i}"),
                        name: "bash".into(),
                        input: json!({"command": "cat log"}),
                    },
                ],
            });
            history.push(Message {
                role: Role::User,
                parts: vec![Part::ToolResult {
                    id: format!("t{i}"),
                    content: "line\n".repeat(size / 5),
                    is_error: false,
                }],
            });
        }
        history
    }

    fn content(message: &Message) -> &str {
        match &message.parts[0] {
            Part::ToolResult { content, .. } => content,
            _ => panic!("not a tool result"),
        }
    }

    #[test]
    fn trims_all_but_the_newest_results_and_keeps_the_calls() {
        let history = calls(10, 5_000);
        let (trimmed, outputs, freed) = trim_tool_output(&history, 6, false).unwrap();
        assert_eq!(outputs, 4);
        assert!(freed > 4 * 1_000);
        assert_eq!(trimmed.len(), history.len());
        // Results 0-3 are notes; 4-9 are whole.
        assert!(content(&trimmed[2]).starts_with(TRIM_MARK));
        assert!(content(&trimmed[8]).starts_with(TRIM_MARK));
        assert_eq!(content(&trimmed[10]), content(&history[10]));
        // The calls stay.
        assert!(matches!(trimmed[1].parts[1], Part::ToolUse { .. }));
        // Trimming again finds nothing new.
        assert!(trim_tool_output(&trimmed, 6, false).is_none());
    }

    #[test]
    fn leaves_short_output_and_drops_claude_thinking_only_before_the_cut() {
        assert!(trim_tool_output(&calls(10, 100), 6, false).is_none());
        let (trimmed, _, _) = trim_tool_output(&calls(10, 5_000), 6, true).unwrap();
        let thinking = |m: &Message| m.parts.iter().any(|p| matches!(p, Part::Thinking { .. }));
        // The last trimmed result is message 8; thinking before it goes, after it stays.
        assert!(!thinking(&trimmed[7]));
        assert!(thinking(&trimmed[9]));
        assert!(thinking(&trimmed[19]));
    }
}
