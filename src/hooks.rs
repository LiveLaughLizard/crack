//! Hooks: the user's commands run at points in a turn, in Claude Code's format, so
//! existing hook scripts work.
//!
//! ```toml
//! [[hooks.PostToolUse]]
//! matcher = "Edit|Write"          # regex on the tool name; empty or "*" for all
//! command = "cargo fmt"
//!
//! [[hooks.PreToolUse]]            # Claude Code's nested form works too
//! matcher = "Bash"
//! hooks = [{ type = "command", command = "./scripts/check-bash.sh", timeout = 10 }]
//! ```
//!
//! A hook gets the event as JSON on stdin. Exit 0 continues (stdout may hold a JSON
//! answer), exit 2 blocks with stderr as the reason, anything else is a warning.
//! Hooks from a project's config run only after the user trusts them.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::Deserialize;
use serde_json::{Value, json};

pub const EVENTS: &[&str] = &[
    "SessionStart",
    "SessionEnd",
    "UserPromptSubmit",
    "PreToolUse",
    "PermissionRequest",
    "PermissionDenied",
    "PostToolUse",
    "PostToolUseFailure",
    "PreCompact",
    "PostCompact",
    "Stop",
    "StopFailure",
    "SubagentStart",
    "SubagentStop",
    "TaskCreated",
    "TaskCompleted",
    "CwdChanged",
    "InstructionsLoaded",
    "Notification",
];

/// Claude Code events crack doesn't raise. Plugins often register them; they're skipped
/// quietly instead of warned about.
const IGNORED_EVENTS: &[&str] = &[
    "Setup",
    "PostToolBatch",
    "UserPromptExpansion",
    "PreModelSwitch",
    "PostModelSwitch",
    "ConfigChange",
    "FileChanged",
    "DirectoryAdded",
    "MessageDisplay",
    "TeammateIdle",
    "Elicitation",
    "ElicitationResult",
    "WorktreeCreate",
    "WorktreeRemove",
];

#[derive(Clone, Debug, Default, Deserialize, PartialEq)]
#[serde(default)]
pub struct HookCommand {
    /// `command` (default), `http`, `prompt` or `mcp_tool`.
    #[serde(rename = "type")]
    pub kind: Option<String>,
    pub command: String,
    pub timeout: Option<u64>,
    /// Run only for tool calls matching this permission rule, e.g. `Bash(git push *)`.
    #[serde(rename = "if")]
    pub condition: Option<String>,
    /// Start it and carry on: its answer is ignored.
    #[serde(rename = "async")]
    pub background: bool,
    /// `http`: where to POST the event, and extra headers (`${VAR}` expands).
    pub url: Option<String>,
    pub headers: BTreeMap<String, String>,
    /// `prompt`: what the model judges the event by.
    pub prompt: Option<String>,
    /// `mcp_tool`: the server and tool to call (the event goes in `arguments.input`).
    pub server: Option<String>,
    pub tool: Option<String>,
    pub arguments: Option<Value>,
}

/// One `[[hooks.Event]]` entry.
#[derive(Clone, Debug, Default, Deserialize, PartialEq)]
#[serde(default)]
pub struct HookMatcher {
    pub matcher: Option<String>,
    pub command: Option<String>,
    pub timeout: Option<u64>,
    pub hooks: Vec<HookCommand>,
    /// Set when it came from a project's config rather than the user's own.
    #[serde(skip)]
    pub from_project: bool,
    /// Set for a plugin's hooks: its name and environment (`CLAUDE_PLUGIN_ROOT`...).
    #[serde(skip)]
    pub plugin: Option<String>,
    #[serde(skip)]
    pub env: Vec<(String, String)>,
}

pub type HooksConfig = BTreeMap<String, Vec<HookMatcher>>;

/// What a hook does.
#[derive(Clone, Debug)]
enum Action {
    Command(String),
    Http {
        url: String,
        headers: BTreeMap<String, String>,
    },
    Prompt(String),
    McpTool {
        server: String,
        tool: String,
        arguments: Value,
    },
}

impl Action {
    fn label(&self) -> String {
        match self {
            Action::Command(c) => short(c),
            Action::Http { url, .. } => format!("POST {url}"),
            Action::Prompt(p) => format!("prompt: {}", short(p)),
            Action::McpTool { server, tool, .. } => format!("{server}/{tool}"),
        }
    }
}

/// A hook ready to run.
#[derive(Clone, Debug)]
struct Hook {
    event: String,
    matcher: Option<regex::Regex>,
    action: Action,
    timeout: Duration,
    from_project: bool,
    env: Vec<(String, String)>,
    /// A plugin's hook: run in the sandbox when it's on.
    plugin: bool,
    /// A permission rule the tool call must match.
    condition: Option<String>,
    /// Fire and forget.
    background: bool,
}

impl Hook {
    /// The user's own hook (their config), as opposed to a project's or a plugin's:
    /// its output isn't size-limited.
    fn personal(&self) -> bool {
        !self.from_project && !self.plugin
    }
}

/// What the hooks for one event decided.
#[derive(Debug, Default)]
pub struct Outcome {
    /// Stop what was about to happen, with the reason (for the model or the user).
    pub block: Option<String>,
    /// PreToolUse: skip (allow) or force (ask) the approval question.
    pub permission: Option<Permission>,
    /// PreToolUse: run the tool with this input instead.
    pub updated_input: Option<Value>,
    /// Extra text for the model.
    pub context: Vec<String>,
    /// Lines for the transcript: what ran and anything worth showing.
    pub notices: Vec<String>,
    /// `"continue": false`: end the turn.
    pub stop: Option<String>,
    /// PostToolUse: replace the tool's output (MCP tools only).
    pub updated_output: Option<String>,
}

/// Ordered from least to most restrictive: when hooks disagree, the strictest wins.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Permission {
    Allow,
    Ask,
    Deny,
}

/// Asks a model a yes/no question for `prompt` hooks; returns its raw answer.
pub type Judge = std::sync::Arc<
    dyn Fn(String) -> futures::future::BoxFuture<'static, anyhow::Result<String>> + Send + Sync,
>;

/// What `prompt` and `mcp_tool` hooks need from the session.
#[derive(Clone, Default)]
pub struct Services {
    pub judge: Option<Judge>,
    pub mcp: Option<std::sync::Arc<crate::mcp::McpHub>>,
}

impl std::fmt::Debug for Services {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Services")
    }
}

/// The configured hooks for a session.
#[derive(Clone, Debug, Default)]
pub struct Hooks {
    hooks: Vec<Hook>,
    cwd: PathBuf,
    session: String,
    /// Project hooks may run (the user trusted this exact set).
    pub project_trusted: bool,
    /// Where plugin hooks run (the shell-command sandbox), when it's on.
    sandbox: Option<crate::policy::Sandbox>,
    services: Services,
}

impl Hooks {
    pub fn new(config: &HooksConfig, cwd: &Path, session: &str) -> Hooks {
        let mut hooks = Vec::new();
        for (event, matchers) in config {
            if !EVENTS.contains(&event.as_str()) {
                if IGNORED_EVENTS.contains(&event.as_str())
                    || matchers.iter().all(|m| m.plugin.is_some())
                {
                    continue;
                }
                eprintln!(
                    "crack: unknown hook event '{event}' (known: {})",
                    EVENTS.join(", ")
                );
                continue;
            }
            for m in matchers {
                let matcher = m
                    .matcher
                    .as_deref()
                    .filter(|p| !p.is_empty() && *p != "*")
                    .and_then(|p| match regex::Regex::new(&matcher_regex(p)) {
                        Ok(r) => Some(r),
                        Err(error) => {
                            eprintln!("crack: hook matcher '{p}' isn't a valid regex: {error}");
                            None
                        }
                    });
                let flat = m.command.iter().map(|c| HookCommand {
                    command: c.clone(),
                    timeout: m.timeout,
                    ..Default::default()
                });
                for h in flat.chain(m.hooks.iter().cloned()) {
                    let Some(action) = action_of(&h) else {
                        eprintln!(
                            "crack: {event} hook of type '{}' is missing what it needs (or isn't a known type)",
                            h.kind.as_deref().unwrap_or("command")
                        );
                        continue;
                    };
                    // Per hook, defaulting to a minute; never more than ten.
                    let seconds = h.timeout.or(m.timeout).unwrap_or(60).clamp(1, 600);
                    hooks.push(Hook {
                        event: event.clone(),
                        matcher: matcher.clone(),
                        action,
                        timeout: Duration::from_secs(seconds),
                        from_project: m.from_project,
                        env: m.env.clone(),
                        plugin: m.plugin.is_some(),
                        condition: h.condition.clone().filter(|c| !c.trim().is_empty()),
                        background: h.background,
                    });
                }
            }
        }
        let project_trusted = trusted(cwd, &fingerprint(config));
        Hooks {
            hooks,
            cwd: cwd.to_owned(),
            session: session.to_owned(),
            project_trusted,
            sandbox: None,
            services: Services::default(),
        }
    }

    /// Lets `prompt` hooks ask a model and `mcp_tool` hooks call MCP servers.
    pub fn with_services(mut self, services: Services) -> Hooks {
        self.services = services;
        self
    }

    /// Plugin hooks run in this sandbox (when it's active).
    pub fn with_sandbox(mut self, sandbox: crate::policy::Sandbox) -> Hooks {
        self.sandbox = sandbox.active().then_some(sandbox);
        self
    }

    pub fn set_session(&mut self, session: &str) {
        self.session = session.to_owned();
    }

    /// The project's hook commands waiting for trust, as `Event: command` lines.
    pub fn untrusted(&self) -> Vec<String> {
        if self.project_trusted {
            return Vec::new();
        }
        self.hooks
            .iter()
            .filter(|h| h.from_project)
            .map(|h| format!("{}: {}", h.event, h.action.label()))
            .collect()
    }

    /// Runs the hooks for `event` (filtered by tool name for tool events) in parallel.
    pub async fn run(&self, event: &str, tool: Option<&str>, mut payload: Value) -> Outcome {
        let claude_tool = tool.map(claude_name);
        let call_input = payload.get("tool_input").map(crack_input);
        let root = crate::context::project_root(&self.cwd);
        let selected: Vec<&Hook> = self
            .hooks
            .iter()
            .filter(|h| h.event == event)
            .filter(|h| !h.from_project || self.project_trusted)
            .filter(|h| match (&h.matcher, tool) {
                (None, _) => true,
                (Some(m), Some(tool)) => {
                    m.is_match(tool) || claude_tool.as_deref().is_some_and(|c| m.is_match(c))
                }
                (Some(m), None) => m.is_match(""),
            })
            // `if`: a permission rule the tool call has to match (tool events only).
            .filter(|h| match (&h.condition, tool, &call_input) {
                (None, ..) => true,
                (Some(rule), Some(tool), Some(input)) => {
                    let hit = crate::policy::rule_matches(rule, tool, input, &root, &self.cwd);
                    if !hit {
                        tracing::debug!(
                            event,
                            rule,
                            "hook skipped: its if condition doesn't match"
                        );
                    }
                    hit
                }
                (Some(_), ..) => false,
            })
            .collect();
        let mut outcome = Outcome::default();
        if selected.is_empty() {
            return outcome;
        }
        if let Some(object) = payload.as_object_mut() {
            object.insert("hook_event_name".into(), json!(event));
            object.insert("session_id".into(), json!(self.session));
            object.insert("cwd".into(), json!(self.cwd.display().to_string()));
            object.insert("transcript_path".into(), json!(""));
            // For other events the matcher subject is e.g. how the session started.
            if let Some(tool) = &claude_tool
                && matches!(
                    event,
                    "PreToolUse"
                        | "PostToolUse"
                        | "PostToolUseFailure"
                        | "PermissionRequest"
                        | "PermissionDenied"
                )
            {
                object.insert("tool_name".into(), json!(tool));
            }
        }
        let input = payload.to_string();
        // `async` hooks run on their own; nothing waits for them or reads their answer.
        let (background, waited): (Vec<&Hook>, Vec<&Hook>) =
            selected.into_iter().partition(|h| h.background);
        for hook in background {
            let (hook, cwd, input, sandbox, services) = (
                hook.clone(),
                self.cwd.clone(),
                input.clone(),
                self.sandbox.clone(),
                self.services.clone(),
            );
            let event = event.to_owned();
            tokio::spawn(async move {
                if let Err(error) =
                    execute(&hook, &cwd, &event, &input, sandbox.as_ref(), &services).await
                {
                    tracing::warn!(
                        event,
                        hook = hook.action.label(),
                        "background hook failed: {error:#}"
                    );
                }
            });
        }
        let runs = waited.iter().map(|hook| {
            execute(
                hook,
                &self.cwd,
                event,
                &input,
                self.sandbox.as_ref(),
                &self.services,
            )
        });
        for (hook, result) in waited.iter().zip(futures::future::join_all(runs).await) {
            let label = hook.action.label();
            match result {
                Err(error) => outcome
                    .notices
                    .push(format!("{event} hook `{label}` failed: {error}")),
                Ok((code, stdout, stderr)) => apply(
                    event,
                    &label,
                    hook.personal(),
                    code,
                    &stdout,
                    &stderr,
                    &mut outcome,
                ),
            }
        }
        outcome
    }
}

/// What a configured hook does, if it has what its type needs.
fn action_of(h: &HookCommand) -> Option<Action> {
    match h.kind.as_deref().unwrap_or("command") {
        "command" => (!h.command.trim().is_empty()).then(|| Action::Command(h.command.clone())),
        "http" => Some(Action::Http {
            url: h.url.clone().filter(|u| !u.is_empty())?,
            headers: h.headers.clone(),
        }),
        "prompt" => Some(Action::Prompt(h.prompt.clone().filter(|p| !p.is_empty())?)),
        "mcp_tool" => Some(Action::McpTool {
            server: h.server.clone()?,
            tool: h.tool.clone()?,
            arguments: h.arguments.clone().unwrap_or_else(|| json!({})),
        }),
        _ => None,
    }
}

/// Claude Code's matcher rules: a pattern of only letters, digits, `_`, `-`, spaces,
/// commas and `|` is a list of exact names; anything else is a regex that may match
/// anywhere in the name.
fn matcher_regex(pattern: &str) -> String {
    let simple = pattern
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | ' ' | ',' | '|'));
    if simple {
        let names: Vec<String> = pattern
            .split(['|', ','])
            .map(str::trim)
            .filter(|n| !n.is_empty())
            .map(regex::escape)
            .collect();
        format!("^(?:{})$", names.join("|"))
    } else {
        pattern.to_owned()
    }
}

/// Runs one hook. Every type answers like a command hook: (exit code, stdout, stderr).
async fn execute(
    hook: &Hook,
    cwd: &Path,
    event: &str,
    input: &str,
    sandbox: Option<&crate::policy::Sandbox>,
    services: &Services,
) -> anyhow::Result<(i32, String, String)> {
    match &hook.action {
        Action::Command(command) => run_command(hook, command, cwd, input, sandbox).await,
        Action::Http { url, headers } => {
            let mut request = crate::workspace::http_client()?
                .post(expand_env(url))
                .header("content-type", "application/json")
                .timeout(hook.timeout)
                .body(input.to_owned());
            for (name, value) in headers {
                request = request.header(name.as_str(), expand_env(value));
            }
            let response = request.send().await?;
            let status = response.status();
            let body = response.text().await.unwrap_or_default();
            // 2xx answers like exit 0 (a JSON body is the hook's answer); anything else is
            // a non-blocking error.
            Ok(if status.is_success() {
                (0, body.trim().to_owned(), String::new())
            } else {
                (1, String::new(), format!("HTTP {status}: {}", body.trim()))
            })
        }
        Action::Prompt(prompt) => {
            let Some(judge) = &services.judge else {
                anyhow::bail!("prompt hooks need a model, and none is set up here");
            };
            let question = format!(
                "You check events in a coding agent against a rule. The rule:\n\n{prompt}\n\n\
                 The {event} event, as JSON:\n{input}\n\n\
                 Reply with only a JSON object: {{\"ok\": true}} if it may go ahead, or \
                 {{\"ok\": false, \"reason\": \"...\"}} to stop it, with a short reason the agent can act on."
            );
            let answer = tokio::time::timeout(hook.timeout, judge(question))
                .await
                .map_err(|_| anyhow::anyhow!("timed out after {}s", hook.timeout.as_secs()))??;
            let verdict = answer
                .find('{')
                .zip(answer.rfind('}'))
                .and_then(|(a, b)| serde_json::from_str::<Value>(&answer[a..=b]).ok())
                .ok_or_else(|| {
                    anyhow::anyhow!("the model didn't answer with JSON: {}", short(&answer))
                })?;
            Ok(if verdict["ok"] == json!(false) {
                let reason = verdict["reason"]
                    .as_str()
                    .unwrap_or("A prompt hook stopped it.");
                (
                    0,
                    json!({"decision": "block", "reason": reason}).to_string(),
                    String::new(),
                )
            } else {
                (0, String::new(), String::new())
            })
        }
        Action::McpTool {
            server,
            tool,
            arguments,
        } => {
            let Some(hub) = &services.mcp else {
                anyhow::bail!("mcp_tool hooks need MCP servers, and none are set up here");
            };
            let mut arguments = arguments.clone();
            if let Some(object) = arguments.as_object_mut() {
                object.insert(
                    "input".into(),
                    serde_json::from_str(input).unwrap_or(Value::Null),
                );
            }
            let exposed = format!("mcp__{server}__{tool}");
            let call = hub.call(&exposed, arguments);
            let (text, is_error) = tokio::time::timeout(hook.timeout, call)
                .await
                .map_err(|_| anyhow::anyhow!("timed out after {}s", hook.timeout.as_secs()))??;
            Ok(if is_error {
                (1, String::new(), text)
            } else {
                (0, text.trim().to_owned(), String::new())
            })
        }
    }
}

/// `${VAR}` from the environment (for http hook URLs and headers).
fn expand_env(text: &str) -> String {
    let var = regex::Regex::new(r"\$\{([A-Za-z_][A-Za-z0-9_]*)\}").expect("valid regex");
    var.replace_all(text, |c: &regex::Captures| {
        std::env::var(&c[1]).unwrap_or_default()
    })
    .into_owned()
}

async fn run_command(
    hook: &Hook,
    command: &str,
    cwd: &Path,
    input: &str,
    sandbox: Option<&crate::policy::Sandbox>,
) -> anyhow::Result<(i32, String, String)> {
    use tokio::io::AsyncWriteExt;
    // Plugin hooks are third-party code: in the sandbox, with their data folder
    // writable. The user's own hooks run as before.
    let argv = match sandbox.filter(|_| hook.plugin) {
        Some(sandbox) => {
            let mut sandbox = sandbox.clone();
            sandbox.writable.extend(
                hook.env
                    .iter()
                    .filter(|(k, _)| k == "CLAUDE_PLUGIN_DATA")
                    .map(|(_, v)| PathBuf::from(v)),
            );
            let root = crate::context::project_root(cwd);
            crate::policy::sandboxed(command, &sandbox, &root)
                .ok_or_else(|| anyhow::anyhow!("the sandbox couldn't start"))?
        }
        None => crate::procs::shell_argv(command),
    };
    let mut child = tokio::process::Command::new(&argv[0])
        .args(&argv[1..])
        .current_dir(cwd)
        .env("CRACK_PROJECT_DIR", cwd)
        .env("CLAUDE_PROJECT_DIR", cwd)
        .env("CRACK", "1")
        .envs(hook.env.iter().map(|(k, v)| (k, v)))
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true)
        .spawn()?;
    if let Some(mut stdin) = child.stdin.take() {
        // A hook that ignores stdin closes it early; that's fine.
        let _ = stdin.write_all(input.as_bytes()).await;
    }
    match tokio::time::timeout(hook.timeout, child.wait_with_output()).await {
        Ok(output) => {
            let output = output?;
            Ok((
                output.status.code().unwrap_or(-1),
                String::from_utf8_lossy(&output.stdout).trim().to_owned(),
                String::from_utf8_lossy(&output.stderr).trim().to_owned(),
            ))
        }
        Err(_) => anyhow::bail!("timed out after {}s", hook.timeout.as_secs()),
    }
}

/// How much of a field the hooks you didn't write (a project's, a plugin's) may put
/// into the conversation. Your own hooks aren't limited.
fn cap(text: &str, personal: bool, chars: usize) -> String {
    if personal || text.chars().count() <= chars {
        return text.to_owned();
    }
    let kept: String = text.chars().take(chars).collect();
    format!("{kept}… [cut at {chars} characters]")
}

/// `additionalContext` from others' hooks: at most 8,000 characters and 200 lines.
fn cap_context(text: &str, personal: bool) -> String {
    if personal {
        return text.to_owned();
    }
    let lines: Vec<&str> = text.lines().collect();
    let text = if lines.len() > 200 {
        format!(
            "{}\n[… {} more lines cut]",
            lines[..200].join("\n"),
            lines.len() - 200
        )
    } else {
        text.to_owned()
    };
    cap(&text, false, 8_000)
}

/// Folds one hook's answer into the outcome. Several hooks for one event combine:
/// the strictest permission answer wins, context and notices add up.
fn apply(
    event: &str,
    label: &str,
    personal: bool,
    code: i32,
    stdout: &str,
    stderr: &str,
    outcome: &mut Outcome,
) {
    let decide = |permission: Permission, reason: Option<String>, outcome: &mut Outcome| {
        if outcome.permission.is_none_or(|p| permission >= p) {
            outcome.permission = Some(permission);
            if permission == Permission::Deny {
                outcome.block = Some(reason.unwrap_or_else(|| "Denied by a hook.".into()));
            }
        }
    };
    match code {
        0 => {
            let answer: Option<Value> = stdout
                .starts_with('{')
                .then(|| serde_json::from_str(stdout).ok())
                .flatten();
            let Some(answer) = answer else {
                // Plain stdout is context for these two events; elsewhere it's just shown.
                if !stdout.is_empty() {
                    if matches!(event, "UserPromptSubmit" | "SessionStart") {
                        outcome.context.push(cap_context(stdout, personal));
                    } else {
                        outcome
                            .notices
                            .push(format!("{event} `{label}`: {}", first_lines(stdout)));
                    }
                }
                return;
            };
            let text =
                |key: &str, limit: usize| answer[key].as_str().map(|s| cap(s, personal, limit));
            if answer["continue"] == json!(false) {
                outcome.stop = Some(
                    text("stopReason", 2_000).unwrap_or_else(|| "A hook ended the turn.".into()),
                );
            }
            if let Some(message) = text("systemMessage", 4_000) {
                outcome.notices.push(message);
            }
            if answer["decision"] == json!("block") {
                outcome.block =
                    Some(text("reason", 2_000).unwrap_or_else(|| "Blocked by a hook.".into()));
            }
            let specific = &answer["hookSpecificOutput"];
            let reason = specific["permissionDecisionReason"]
                .as_str()
                .map(|s| cap(s, personal, 2_000));
            match specific["permissionDecision"].as_str() {
                Some("allow") => decide(Permission::Allow, reason, outcome),
                Some("ask") => decide(Permission::Ask, reason, outcome),
                Some("deny") => decide(Permission::Deny, reason, outcome),
                // "defer" (or nothing): no opinion; the normal rules decide.
                _ => {}
            }
            // PermissionRequest answers for the user: allow (maybe with new input) or deny.
            let decision = &specific["decision"];
            match decision["behavior"].as_str() {
                Some("allow") => {
                    decide(Permission::Allow, None, outcome);
                    if decision["updatedInput"].is_object() {
                        outcome.updated_input = Some(decision["updatedInput"].clone());
                    }
                }
                Some("deny") => {
                    let message = decision["message"]
                        .as_str()
                        .map(|s| cap(s, personal, 2_000));
                    decide(Permission::Deny, message, outcome)
                }
                _ => {}
            }
            if specific["updatedInput"].is_object() {
                outcome.updated_input = Some(specific["updatedInput"].clone());
            }
            if let Some(context) = specific["additionalContext"].as_str() {
                outcome.context.push(cap_context(context, personal));
            }
            match &specific["updatedToolOutput"] {
                Value::Null => {}
                Value::String(s) => outcome.updated_output = Some(cap(s, personal, 40_000)),
                other => outcome.updated_output = Some(cap(&other.to_string(), personal, 40_000)),
            }
        }
        2 => {
            let reason = if stderr.is_empty() {
                format!("Blocked by the {event} hook `{label}`.")
            } else {
                cap(stderr, personal, 2_000)
            };
            outcome.block = Some(reason);
        }
        _ => {
            let detail = if stderr.is_empty() { stdout } else { stderr };
            outcome.notices.push(format!(
                "{event} hook `{label}` exited with {code}: {}",
                first_lines(detail)
            ));
        }
    }
}

fn first_lines(text: &str) -> String {
    let lines: Vec<&str> = text.lines().take(6).collect();
    let more = text.lines().count() > 6;
    format!("{}{}", lines.join("\n"), if more { "\n…" } else { "" })
}

fn short(command: &str) -> String {
    let first = command.lines().next().unwrap_or("");
    if first.chars().count() > 60 {
        format!("{}…", first.chars().take(60).collect::<String>())
    } else {
        first.to_owned()
    }
}

/// Runs an event's hooks outside a turn (a tab closing, the folder changing) without
/// waiting for them. `blocking` waits instead (a headless run about to exit).
pub fn fire(
    config: &crate::config::Config,
    cwd: &Path,
    session: &str,
    event: &'static str,
    subject: Option<&str>,
    payload: Value,
    blocking: bool,
) {
    let hooks = Hooks::new(&config.hooks, cwd, session)
        .with_sandbox(crate::policy::Sandbox::resolve(&config.permissions));
    if !hooks.hooks.iter().any(|h| h.event == *event) {
        return;
    }
    let subject = subject.map(str::to_owned);
    let run = async move {
        let outcome = hooks.run(event, subject.as_deref(), payload).await;
        for notice in outcome.notices {
            tracing::info!(event, "{notice}");
        }
    };
    if blocking {
        crate::runtime::block_on(run);
    } else {
        crate::runtime::spawn(run);
    }
}

/// Claude Code's name for a crack tool, for matchers and `tool_name`.
pub fn claude_name(tool: &str) -> String {
    match tool {
        "bash" => "Bash".into(),
        "edit" => "Edit".into(),
        "write" => "Write".into(),
        "read" => "Read".into(),
        "grep" => "Grep".into(),
        "glob" => "Glob".into(),
        "list" => "LS".into(),
        "webfetch" => "WebFetch".into(),
        "todowrite" => "TodoWrite".into(),
        "skill" => "Skill".into(),
        "task" => "Task".into(),
        other => other.to_owned(),
    }
}

/// crack's name for a tool written Claude Code's way (`Read` → `read`), for agent files.
pub fn crack_tool_name(name: &str) -> String {
    match name {
        "LS" => "list".into(),
        "TodoWrite" => "todowrite".into(),
        "WebFetch" => "webfetch".into(),
        "Task" => "task".into(),
        other if other.starts_with("mcp__") => other.to_owned(),
        other => other.to_ascii_lowercase(),
    }
}

/// A tool's input with Claude Code's key names added (`file_path` for `path`).
pub fn claude_input(input: &Value) -> Value {
    let mut input = input.clone();
    if let Some(object) = input.as_object_mut() {
        if let Some(path) = object.get("path").cloned() {
            object.entry("file_path").or_insert(path);
        }
        for (ours, theirs) in [("old", "old_string"), ("new", "new_string")] {
            if let Some(value) = object.get(ours).cloned() {
                object.entry(theirs).or_insert(value);
            }
        }
    }
    input
}

/// Undoes `claude_input` on a hook's rewritten input.
pub fn crack_input(input: &Value) -> Value {
    let mut input = input.clone();
    if let Some(object) = input.as_object_mut() {
        for (ours, theirs) in [
            ("path", "file_path"),
            ("old", "old_string"),
            ("new", "new_string"),
        ] {
            if let Some(value) = object.remove(theirs) {
                object.insert(ours.into(), value);
            }
        }
    }
    input
}

fn fingerprint(config: &HooksConfig) -> String {
    use sha2::Digest;
    let mut text = String::new();
    for (event, matchers) in config {
        for m in matchers.iter().filter(|m| m.from_project) {
            text.push_str(&format!(
                "{event}\0{:?}\0{:?}\0{:?}\0{:?}\n",
                m.matcher, m.command, m.timeout, m.hooks
            ));
        }
    }
    hex::encode(sha2::Sha256::digest(text.as_bytes()))
}

fn trust_key(cwd: &Path) -> String {
    format!(
        "hooks-trusted:{}",
        crate::context::project_root(cwd).display()
    )
}

fn trusted(cwd: &Path, fingerprint: &str) -> bool {
    // No project hooks: nothing to trust.
    fingerprint == hex::encode(<sha2::Sha256 as sha2::Digest>::digest(b""))
        || (!cfg!(test) && crate::store::get().kv(&trust_key(cwd)).as_deref() == Some(fingerprint))
}

/// Trusts this project's current hooks (until they change).
pub fn trust(config: &HooksConfig, cwd: &Path) {
    crate::store::get().set_kv(&trust_key(cwd), fingerprint(config));
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config(toml_text: &str) -> HooksConfig {
        #[derive(Deserialize)]
        struct Wrapper {
            hooks: HooksConfig,
        }
        toml::from_str::<Wrapper>(toml_text).unwrap().hooks
    }

    // The tests that use it run bash scripts, so Unix only.
    #[cfg(unix)]
    fn hooks(toml_text: &str) -> Hooks {
        let config = config(toml_text);
        let mut hooks = Hooks::new(&config, &std::env::temp_dir(), "test");
        hooks.project_trusted = true;
        hooks
    }

    #[test]
    fn matchers_follow_claude_code_rules() {
        let exact = regex::Regex::new(&matcher_regex("Edit|Write")).unwrap();
        assert!(exact.is_match("Edit") && !exact.is_match("MultiEdit"));
        let list = regex::Regex::new(&matcher_regex("startup, clear")).unwrap();
        assert!(list.is_match("clear") && !list.is_match("resume"));
        // Anything else is a regex that may match anywhere.
        let pattern = regex::Regex::new(&matcher_regex("mcp__.*")).unwrap();
        assert!(pattern.is_match("mcp__github__issue"));
    }

    #[test]
    fn the_strictest_answer_wins_in_any_order() {
        let answer = |d: &str| {
            json!({"hookSpecificOutput": {"permissionDecision": d, "permissionDecisionReason": d}})
                .to_string()
        };
        for order in [
            ["allow", "deny", "ask"],
            ["deny", "ask", "allow"],
            ["ask", "allow", "deny"],
        ] {
            let mut outcome = Outcome::default();
            for d in order {
                apply("PreToolUse", "h", true, 0, &answer(d), "", &mut outcome);
            }
            assert_eq!(outcome.permission, Some(Permission::Deny), "{order:?}");
            assert_eq!(outcome.block.as_deref(), Some("deny"));
        }
        let mut outcome = Outcome::default();
        apply(
            "PreToolUse",
            "h",
            true,
            0,
            &answer("allow"),
            "",
            &mut outcome,
        );
        apply("PreToolUse", "h", true, 0, &answer("ask"), "", &mut outcome);
        assert_eq!(outcome.permission, Some(Permission::Ask));
    }

    #[test]
    fn only_others_hooks_are_size_limited() {
        let long = "x".repeat(5_000);
        let answer = json!({"decision": "block", "reason": long}).to_string();
        let mut theirs = Outcome::default();
        apply("Stop", "h", false, 0, &answer, "", &mut theirs);
        assert!(theirs.block.as_ref().unwrap().chars().count() < 2_100);
        let mut mine = Outcome::default();
        apply("Stop", "h", true, 0, &answer, "", &mut mine);
        assert_eq!(mine.block.as_ref().unwrap().len(), 5_000);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn if_conditions_and_background_hooks() {
        let h = hooks(
            r#"
            [[hooks.PreToolUse]]
            hooks = [
              { type = "command", command = "echo blocked >&2; exit 2", if = "Bash(git push *)" },
              { type = "command", command = "sleep 5; exit 2", async = true },
            ]
            "#,
        );
        let started = std::time::Instant::now();
        let push = h
            .run(
                "PreToolUse",
                Some("bash"),
                json!({"tool_input": {"command": "git push origin main"}}),
            )
            .await;
        assert_eq!(push.block.as_deref(), Some("blocked"));
        let status = h
            .run(
                "PreToolUse",
                Some("bash"),
                json!({"tool_input": {"command": "git status"}}),
            )
            .await;
        assert!(
            status.block.is_none(),
            "the if condition skips it; the async hook isn't waited for"
        );
        assert!(started.elapsed() < std::time::Duration::from_secs(3));
    }

    #[tokio::test]
    async fn http_hooks_post_the_event_and_answer_with_json() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut buf = vec![0u8; 8192];
            let n = socket.read(&mut buf).await.unwrap();
            let request = String::from_utf8_lossy(&buf[..n]).to_string();
            let reason = if request.contains("rm -rf") {
                "no deleting"
            } else {
                "?"
            };
            let body = json!({"hookSpecificOutput": {"permissionDecision": "deny", "permissionDecisionReason": reason}}).to_string();
            let reply = format!(
                "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\n\r\n{body}",
                body.len()
            );
            socket.write_all(reply.as_bytes()).await.unwrap();
        });
        let config = config(&format!(
            "[[hooks.PreToolUse]]\nhooks = [{{ type = \"http\", url = \"http://{addr}/check\" }}]\n"
        ));
        let h = Hooks::new(&config, &std::env::temp_dir(), "test");
        let outcome = h
            .run(
                "PreToolUse",
                Some("bash"),
                json!({"tool_input": {"command": "rm -rf build"}}),
            )
            .await;
        assert_eq!(outcome.permission, Some(Permission::Deny));
        assert_eq!(outcome.block.as_deref(), Some("no deleting"));
    }

    // These hooks are bash scripts.
    #[cfg(unix)]
    #[tokio::test]
    async fn plugin_hooks_get_their_env_and_session_start_matches_the_source() {
        let mut config = config(
            r#"
            [[hooks.SessionStart]]
            matcher = "startup|clear|compact"
            command = 'echo "root=$CLAUDE_PLUGIN_ROOT"'
            "#,
        );
        // Events crack doesn't raise are skipped quietly for plugins.
        config.insert("SessionEnd".into(), config["SessionStart"].clone());
        for m in config.values_mut().flatten() {
            m.plugin = Some("demo@market".into());
            m.env = vec![("CLAUDE_PLUGIN_ROOT".into(), "/plugins/demo".into())];
        }
        let h = Hooks::new(&config, &std::env::temp_dir(), "test");
        let start = h
            .run(
                "SessionStart",
                Some("startup"),
                json!({"source": "startup"}),
            )
            .await;
        assert_eq!(start.context, vec!["root=/plugins/demo".to_owned()]);
        let resume = h
            .run("SessionStart", Some("resume"), json!({"source": "resume"}))
            .await;
        assert!(resume.context.is_empty());
    }

    // These hooks are bash scripts.
    #[cfg(unix)]
    #[tokio::test]
    async fn block_git_push_and_format_after_edits() {
        let h = hooks(
            r#"
            [[hooks.PreToolUse]]
            matcher = "Bash"
            command = '''jq -r .tool_input.command | grep -q '^git push' && { echo "pushing is off-limits" >&2; exit 2; }; exit 0'''

            [[hooks.PostToolUse]]
            matcher = "Edit|Write"
            hooks = [{ type = "command", command = "echo formatted $(jq -r .tool_input.file_path)" }]
            "#,
        );
        let push = h
            .run(
                "PreToolUse",
                Some("bash"),
                json!({"tool_input": {"command": "git push origin main"}}),
            )
            .await;
        assert_eq!(push.block.as_deref(), Some("pushing is off-limits"));
        let ls = h
            .run(
                "PreToolUse",
                Some("bash"),
                json!({"tool_input": {"command": "ls"}}),
            )
            .await;
        assert!(ls.block.is_none());
        let edit = h
            .run(
                "PostToolUse",
                Some("edit"),
                json!({"tool_input": claude_input(&json!({"path": "src/a.rs"}))}),
            )
            .await;
        assert_eq!(
            edit.notices,
            ["PostToolUse `echo formatted $(jq -r .tool_input.file_path)`: formatted src/a.rs"]
        );
        // The matcher doesn't fire for other tools.
        assert!(
            h.run("PostToolUse", Some("read"), json!({}))
                .await
                .notices
                .is_empty()
        );
    }

    // Bash quoting.
    #[cfg(unix)]
    #[tokio::test]
    async fn json_answers_and_context() {
        let h = hooks(
            r#"
            [[hooks.UserPromptSubmit]]
            command = "echo 'Today is release day.'"

            [[hooks.PreToolUse]]
            matcher = "Write"
            command = '''echo '{"hookSpecificOutput": {"hookEventName": "PreToolUse", "permissionDecision": "allow", "updatedInput": {"file_path": "safe.txt", "content": "x"}}}' '''

            [[hooks.Stop]]
            command = "echo '{\"decision\": \"block\", \"reason\": \"run the tests first\"}'"

            [[hooks.Notification]]
            command = "exit 1"
            "#,
        );
        let prompt = h
            .run("UserPromptSubmit", None, json!({"prompt": "ship it"}))
            .await;
        assert_eq!(prompt.context, ["Today is release day."]);
        let write = h.run("PreToolUse", Some("write"), json!({})).await;
        assert_eq!(write.permission, Some(Permission::Allow));
        assert_eq!(
            crack_input(write.updated_input.as_ref().unwrap())["path"],
            "safe.txt"
        );
        let stop = h
            .run("Stop", None, json!({"stop_hook_active": false}))
            .await;
        assert_eq!(stop.block.as_deref(), Some("run the tests first"));
        let note = h
            .run("Notification", None, json!({"message": "approve?"}))
            .await;
        assert!(note.notices[0].contains("exited with 1"));
    }

    // These hooks are bash scripts.
    #[cfg(unix)]
    #[tokio::test]
    async fn project_hooks_wait_for_trust_and_time_out() {
        let mut config = config(
            r#"
            [[hooks.UserPromptSubmit]]
            command = "echo from the project"
            [[hooks.Stop]]
            command = "sleep 5"
            timeout = 1
            "#,
        );
        for m in config.values_mut().flatten() {
            m.from_project = true;
        }
        let mut h = Hooks::new(&config, &std::env::temp_dir(), "test");
        h.project_trusted = false;
        assert_eq!(h.untrusted().len(), 2);
        assert!(
            h.run("UserPromptSubmit", None, json!({}))
                .await
                .context
                .is_empty()
        );
        h.project_trusted = true;
        assert_eq!(
            h.run("UserPromptSubmit", None, json!({})).await.context,
            ["from the project"]
        );
        let slow = h.run("Stop", None, json!({})).await;
        assert!(slow.notices[0].contains("timed out"), "{:?}", slow.notices);
    }
}
