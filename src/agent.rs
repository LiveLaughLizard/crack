//! The agent loop: send the conversation, stream the reply, run the tools it asks
//! for, send their results back, until the model stops calling tools.

use crate::providers::{
    ChatRequest, Effort, Endpoint, Message, Part, Role, StopReason, StreamEvent, Usage,
};
use crate::tools::{self, Agent, ToolContext, ToolMeta};
use serde_json::Value;
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
    Error(String),
    Done {
        interrupted: bool,
        seconds: f32,
    },
}

#[derive(Clone)]
pub struct TurnConfig {
    pub endpoint: Endpoint,
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
    loop {
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
            return Ok(());
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

        let mut results = Vec::new();
        for (id, name, input) in calls {
            let _ = events.send(AgentEvent::ToolCall {
                id: id.clone(),
                name: name.clone(),
                input: input.clone(),
            });
            let output = match permit(config, &id, &name, &input, events).await {
                Ok(()) => tools::run(&name, &input, ctx, config.agent).await,
                Err(refusal) => refusal,
            };
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
) -> Result<(), tools::ToolOutput> {
    use crate::policy::{Answer, Decision};
    let rules = config.session_rules.lock().unwrap().clone();
    let decision = config
        .policy
        .clone()
        .with_session_rules(&rules)
        .check(name, input);
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
