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
    "UserPromptSubmit",
    "PreToolUse",
    "PostToolUse",
    "PreCompact",
    "Stop",
    "SubagentStop",
    "Notification",
];

/// Claude Code events crack doesn't raise. Plugins often register them; they're skipped
/// quietly instead of warned about.
const IGNORED_EVENTS: &[&str] = &[
    "SessionEnd",
    "SubagentStart",
    "PostToolUseFailure",
    "PermissionRequest",
    "PostCompact",
    "Setup",
    "StopFailure",
];

#[derive(Clone, Debug, Default, Deserialize, PartialEq)]
#[serde(default)]
pub struct HookCommand {
    #[serde(rename = "type")]
    pub kind: Option<String>,
    pub command: String,
    pub timeout: Option<u64>,
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

/// A hook ready to run.
#[derive(Clone, Debug)]
struct Hook {
    event: String,
    matcher: Option<regex::Regex>,
    command: String,
    timeout: Duration,
    from_project: bool,
    env: Vec<(String, String)>,
    /// A plugin's hook: run in the sandbox when it's on.
    plugin: bool,
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
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Permission {
    Allow,
    Ask,
    Deny,
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
                let commands = m.command.iter().map(|c| (c.clone(), m.timeout)).chain(
                    m.hooks
                        .iter()
                        .filter(|h| h.kind.as_deref().unwrap_or("command") == "command")
                        .map(|h| (h.command.clone(), h.timeout.or(m.timeout))),
                );
                for (command, timeout) in commands {
                    hooks.push(Hook {
                        event: event.clone(),
                        matcher: matcher.clone(),
                        command,
                        timeout: Duration::from_secs(timeout.unwrap_or(60)),
                        from_project: m.from_project,
                        env: m.env.clone(),
                        plugin: m.plugin.is_some(),
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
        }
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
            .map(|h| format!("{}: {}", h.event, h.command))
            .collect()
    }

    /// Runs the hooks for `event` (filtered by tool name for tool events) in parallel.
    pub async fn run(&self, event: &str, tool: Option<&str>, mut payload: Value) -> Outcome {
        let claude_tool = tool.map(claude_name);
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
                && matches!(event, "PreToolUse" | "PostToolUse")
            {
                object.insert("tool_name".into(), json!(tool));
            }
        }
        let input = payload.to_string();
        let runs = selected
            .iter()
            .map(|hook| run_one(hook, &self.cwd, &input, self.sandbox.as_ref()));
        for (hook, result) in selected.iter().zip(futures::future::join_all(runs).await) {
            let label = short(&hook.command);
            match result {
                Err(error) => outcome
                    .notices
                    .push(format!("{event} hook `{label}` failed: {error}")),
                Ok((code, stdout, stderr)) => {
                    apply(event, &label, code, &stdout, &stderr, &mut outcome)
                }
            }
        }
        outcome
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

async fn run_one(
    hook: &Hook,
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
            crate::policy::sandboxed(&hook.command, &sandbox, &root)
                .ok_or_else(|| anyhow::anyhow!("the sandbox couldn't start"))?
        }
        None => crate::procs::shell_argv(&hook.command),
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

/// Folds one hook's result into the outcome, per Claude Code's rules.
fn apply(event: &str, label: &str, code: i32, stdout: &str, stderr: &str, outcome: &mut Outcome) {
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
                        outcome.context.push(stdout.to_owned());
                    } else {
                        outcome
                            .notices
                            .push(format!("{event} `{label}`: {}", first_lines(stdout)));
                    }
                }
                return;
            };
            if answer["continue"] == json!(false) {
                outcome.stop = Some(
                    answer["stopReason"]
                        .as_str()
                        .unwrap_or("A hook ended the turn.")
                        .to_owned(),
                );
            }
            if let Some(message) = answer["systemMessage"].as_str() {
                outcome.notices.push(message.to_owned());
            }
            if answer["decision"] == json!("block") {
                outcome.block = Some(
                    answer["reason"]
                        .as_str()
                        .unwrap_or("Blocked by a hook.")
                        .to_owned(),
                );
            }
            let specific = &answer["hookSpecificOutput"];
            match specific["permissionDecision"].as_str() {
                Some("allow") => {
                    outcome.permission = outcome.permission.or(Some(Permission::Allow))
                }
                Some("ask") => outcome.permission = Some(Permission::Ask),
                Some("deny") => {
                    outcome.permission = Some(Permission::Deny);
                    outcome.block = Some(
                        specific["permissionDecisionReason"]
                            .as_str()
                            .unwrap_or("Denied by a hook.")
                            .to_owned(),
                    );
                }
                _ => {}
            }
            if specific["updatedInput"].is_object() {
                outcome.updated_input = Some(specific["updatedInput"].clone());
            }
            if let Some(context) = specific["additionalContext"].as_str() {
                outcome.context.push(context.to_owned());
            }
        }
        2 => {
            let reason = if stderr.is_empty() {
                format!("Blocked by the {event} hook `{label}`.")
            } else {
                stderr.to_owned()
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
