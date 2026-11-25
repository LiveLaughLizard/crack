//! The tools the model can call: shell, files, search, web, todos and skills.

use crate::providers::ToolSpec;
use crate::skills::Skill;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::io::AsyncReadExt;

const MAX_OUTPUT: usize = 30_000;
const MAX_LINE: usize = 2_000;

/// Which primary agent is running. Plan can only look.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Agent {
    #[default]
    Build,
    Plan,
}

impl Agent {
    pub fn name(self) -> &'static str {
        match self {
            Agent::Build => "Build",
            Agent::Plan => "Plan",
        }
    }

    pub fn parse(value: &str) -> Option<Agent> {
        match value.trim().to_ascii_lowercase().as_str() {
            "build" => Some(Agent::Build),
            "plan" => Some(Agent::Plan),
            _ => None,
        }
    }

    pub fn next(self) -> Agent {
        match self {
            Agent::Build => Agent::Plan,
            Agent::Plan => Agent::Build,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Todo {
    pub content: String,
    pub status: String,
}

/// What tools share with the session.
#[derive(Clone)]
pub struct ToolContext {
    pub cwd: PathBuf,
    pub skills: Arc<Vec<Skill>>,
    pub todos: Arc<Mutex<Vec<Todo>>>,
    pub http: reqwest::Client,
    /// Tool names the session may use; empty means all for the agent.
    pub allow: Option<Vec<String>>,
    /// MCP servers whose tools are offered alongside the built-in ones.
    pub mcp: Arc<crate::mcp::McpHub>,
    /// Run shell commands in this sandbox (`None`: run them directly).
    pub sandbox: Option<crate::policy::Sandbox>,
    /// Instruction files already in the system prompt or loaded by `read`.
    pub loaded: Arc<Mutex<std::collections::HashSet<PathBuf>>>,
    /// Snapshots files before the turn first changes them, for undo.
    pub checkpoint: Arc<crate::checkpoint::Tracker>,
    /// Use language servers: diagnostics after edits, and the LSP tools.
    pub lsp: bool,
    /// How long to wait for a language server's diagnostics after an edit.
    pub lsp_wait: Duration,
}

/// How the interface draws a finished call.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum DisplayKind {
    #[default]
    Inline,
    Shell,
    Diff,
    Write,
    Todos,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct ToolMeta {
    pub kind: DisplayKind,
    /// One-line summary, e.g. `Read src/main.rs:1-200`.
    pub title: String,
    /// Body of a block display: shell output, a diff, written content.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    /// A subagent's stored transcript (session id), opened by clicking the call.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub link: Option<String>,
}

pub struct ToolOutput {
    pub content: String,
    pub is_error: bool,
    pub meta: ToolMeta,
}

impl ToolOutput {
    /// A call that wasn't run (denied or not approved).
    pub fn refused(content: String, title: String) -> Self {
        Self::error(content, title)
    }

    pub fn ok(content: impl Into<String>, meta: ToolMeta) -> Self {
        Self {
            content: content.into(),
            is_error: false,
            meta,
        }
    }

    pub fn error(content: impl Into<String>, title: String) -> Self {
        Self {
            content: content.into(),
            is_error: true,
            meta: ToolMeta {
                title,
                ..Default::default()
            },
        }
    }
}

fn spec(name: &str, description: &str, schema: Value) -> ToolSpec {
    ToolSpec {
        name: name.into(),
        description: description.into(),
        schema,
    }
}

const READ_ONLY: &[&str] = &[
    "read",
    "glob",
    "grep",
    "list",
    "webfetch",
    "todowrite",
    "skill",
    "git_status",
    "git_diff",
    "git_log",
    "git_show",
    "symbols",
    "find_symbol",
    "read_symbol",
    "definition",
    "references",
    "hover",
    "read_mcp_resource",
];

pub fn specs(agent: Agent, ctx: &ToolContext) -> Vec<ToolSpec> {
    let mut all = vec![
        spec(
            "bash",
            "Run a shell command with bash in the working directory and return its combined stdout and stderr. \
             Use for builds, tests, git, package managers and anything without a dedicated tool. \
             Commands time out after `timeout` seconds (default 120, max 600). Output over 30000 characters is truncated.",
            json!({
                "type": "object",
                "properties": {
                    "command": {"type": "string", "description": "The command to run."},
                    "description": {"type": "string", "description": "What the command does, in 5-10 words."},
                    "workdir": {"type": "string", "description": "Directory to run in; defaults to the working directory."},
                    "timeout": {"type": "integer", "description": "Timeout in seconds."}
                },
                "required": ["command"]
            }),
        ),
        spec(
            "read",
            "Read a file. Returns numbered lines (cat -n format), starting at `offset` (1-based) for up to `limit` lines (default 2000). \
             Reading a directory lists it.",
            json!({
                "type": "object",
                "properties": {
                    "path": {"type": "string", "description": "File path, absolute or relative to the working directory."},
                    "offset": {"type": "integer", "description": "First line to read, 1-based."},
                    "limit": {"type": "integer", "description": "Number of lines to read."}
                },
                "required": ["path"]
            }),
        ),
        spec(
            "write",
            "Create or overwrite a file with the given content. Parent directories are created. Prefer `edit` for changes to existing files.",
            json!({
                "type": "object",
                "properties": {
                    "path": {"type": "string"},
                    "content": {"type": "string"}
                },
                "required": ["path", "content"]
            }),
        ),
        spec(
            "edit",
            "Replace exact text in a file. `old_string` must match the file exactly (including whitespace) and be unique unless \
             `replace_all` is true. Read the file first.",
            json!({
                "type": "object",
                "properties": {
                    "path": {"type": "string"},
                    "old_string": {"type": "string", "description": "Exact text to replace."},
                    "new_string": {"type": "string", "description": "Replacement text."},
                    "replace_all": {"type": "boolean", "description": "Replace every occurrence."}
                },
                "required": ["path", "old_string", "new_string"]
            }),
        ),
        spec(
            "glob",
            "Find files by glob pattern (e.g. `**/*.rs`), respecting .gitignore. Results are sorted by modification time, newest first.",
            json!({
                "type": "object",
                "properties": {
                    "pattern": {"type": "string"},
                    "path": {"type": "string", "description": "Directory to search; defaults to the working directory."}
                },
                "required": ["pattern"]
            }),
        ),
        spec(
            "grep",
            "Search file contents with a regular expression, respecting .gitignore. Returns `path:line: text` matches.",
            json!({
                "type": "object",
                "properties": {
                    "pattern": {"type": "string", "description": "Regular expression (Rust regex syntax)."},
                    "path": {"type": "string", "description": "File or directory to search."},
                    "include": {"type": "string", "description": "Only search files matching this glob, e.g. `*.ts`."},
                    "case_insensitive": {"type": "boolean"}
                },
                "required": ["pattern"]
            }),
        ),
        spec(
            "list",
            "List a directory's files and subdirectories (two levels deep), respecting .gitignore.",
            json!({
                "type": "object",
                "properties": {"path": {"type": "string"}}
            }),
        ),
        spec(
            "webfetch",
            "Fetch a URL and return its content as text (HTML is reduced to text).",
            json!({
                "type": "object",
                "properties": {"url": {"type": "string"}},
                "required": ["url"]
            }),
        ),
        spec(
            "todowrite",
            "Replace the session's todo list. Use it to plan and track multi-step work; keep exactly one item in_progress while working.",
            json!({
                "type": "object",
                "properties": {
                    "todos": {
                        "type": "array",
                        "items": {
                            "type": "object",
                            "properties": {
                                "content": {"type": "string"},
                                "status": {"type": "string", "enum": ["pending", "in_progress", "completed", "cancelled"]}
                            },
                            "required": ["content", "status"]
                        }
                    }
                },
                "required": ["todos"]
            }),
        ),
    ];
    if ctx.cwd.ancestors().any(|d| d.join(".git").exists()) {
        all.extend(crate::git::specs());
    }
    all.extend(crate::code_index::specs());
    if ctx.lsp && crate::lsp::available(&ctx.cwd) {
        all.extend(crate::lsp::specs());
    }
    if !ctx.skills.is_empty() {
        let mut listing = String::from(
            "Load a skill: specialized instructions and resources for a task. Call it when the task matches a skill's \
             description, before starting the work. Available skills:\n",
        );
        for skill in ctx.skills.iter() {
            listing.push_str(&format!("- {}: {}\n", skill.name, skill.description));
        }
        all.push(spec(
            "skill",
            &listing,
            json!({
                "type": "object",
                "properties": {"name": {"type": "string", "description": "The skill's name."}},
                "required": ["name"]
            }),
        ));
    }
    // Air-gapped builds have no tool that reaches the internet.
    all.retain(|tool| cfg!(feature = "cloud") || tool.name != "webfetch");
    // Reading an MCP server's resources is read-only, so every agent gets it.
    let resources = ctx.mcp.resources();
    if !resources.is_empty() {
        let mut listing = String::from(
            "Read a resource from an MCP server (a document, file, record…) by its server and URI. Available:\n",
        );
        for resource in resources.iter().take(40) {
            listing.push_str(&format!(
                "- {} {}: {}\n",
                resource.server, resource.uri, resource.name
            ));
        }
        if resources.len() > 40 {
            listing.push_str(&format!("…and {} more.\n", resources.len() - 40));
        }
        all.push(spec(
            "read_mcp_resource",
            listing.trim_end(),
            json!({
                "type": "object",
                "properties": {
                    "server": {"type": "string", "description": "The MCP server's name."},
                    "uri": {"type": "string", "description": "The resource's URI."}
                },
                "required": ["server", "uri"]
            }),
        ));
    }
    // MCP tools can change things, so only Build gets them.
    if agent == Agent::Build {
        for tool in ctx.mcp.tools() {
            let description = if tool.description.is_empty() {
                format!("{} (from the {} MCP server)", tool.name, tool.server)
            } else {
                format!("{} (from the {} MCP server)", tool.description, tool.server)
            };
            all.push(spec(&tool.exposed, &description, tool.schema));
        }
    }
    all.retain(|tool| {
        (agent == Agent::Build || READ_ONLY.contains(&tool.name.as_str()))
            && ctx
                .allow
                .as_ref()
                .is_none_or(|allow| allow.iter().any(|a| allows(a, &tool.name)))
    });
    all
}

/// An allow-list entry matches a tool by name; `mcp` allows every MCP tool and
/// `mcp__<server>` every tool of one server.
fn allows(entry: &str, tool: &str) -> bool {
    entry == tool
        || (entry == "mcp" && (tool.starts_with("mcp__") || tool == "read_mcp_resource"))
        || (entry.starts_with("mcp__") && tool.starts_with(&format!("{entry}__")))
}

fn str_arg<'a>(input: &'a Value, key: &str) -> Option<&'a str> {
    input.get(key).and_then(Value::as_str)
}

fn resolve(cwd: &Path, path: &str) -> PathBuf {
    let path = crate::config::expand_home(path);
    if path.is_absolute() {
        path
    } else {
        cwd.join(path)
    }
}

/// A path as shown to the user: relative to the working directory when inside it.
pub fn display_path(cwd: &Path, path: &Path) -> String {
    path.strip_prefix(cwd)
        .map(|p| p.display().to_string())
        .unwrap_or_else(|_| path.display().to_string())
}

/// The one-line title shown while a call runs.
pub fn title(name: &str, input: &Value) -> String {
    let arg = |key: &str| str_arg(input, key).unwrap_or_default().to_owned();
    match name {
        "bash" => format!("$ {}", arg("command")),
        "read" => format!("Read {}", arg("path")),
        "write" => format!("Write {}", arg("path")),
        "edit" => format!("Edit {}", arg("path")),
        "task" => format!(
            "Task: {} ({})",
            arg("description"),
            input["subagent_type"]
                .as_str()
                .filter(|s| !s.is_empty())
                .unwrap_or("general")
        ),
        "symbols" => format!("Symbols in {}", arg("path")),
        "find_symbol" => format!("Find symbol \"{}\"", arg("name")),
        "read_symbol" => format!("Read symbol {}", arg("name")),
        "git_status" => "git status".into(),
        "git_diff" => format!("git diff {}", arg("path")).trim_end().to_owned(),
        "git_log" => format!("git log {}", arg("path")).trim_end().to_owned(),
        "git_show" => format!("git show {}", arg("ref")).trim_end().to_owned(),
        "git_commit" => format!("Commit: {}", arg("message")),
        "glob" => format!("Glob \"{}\"", arg("pattern")),
        "grep" => format!("Grep \"{}\"", arg("pattern")),
        "list" => format!(
            "List {}",
            input.get("path").and_then(Value::as_str).unwrap_or(".")
        ),
        "webfetch" => format!("WebFetch {}", arg("url")),
        "read_mcp_resource" => format!("Read {}:{}", arg("server"), arg("uri")),
        "todowrite" => "Todos".into(),
        "skill" => format!("Skill \"{}\"", arg("name")),
        other => match other
            .strip_prefix("mcp__")
            .and_then(|rest| rest.split_once("__"))
        {
            // mcp__server__tool -> "server · tool {first argument}"
            Some((server, tool)) => {
                let first = input
                    .as_object()
                    .and_then(|o| o.values().find_map(Value::as_str))
                    .map(|v| format!(" {}", v.chars().take(60).collect::<String>()))
                    .unwrap_or_default();
                format!("{server} · {tool}{first}")
            }
            None => other.to_owned(),
        },
    }
}

pub async fn run(name: &str, input: &Value, ctx: &ToolContext, agent: Agent) -> ToolOutput {
    let title = title(name, input);
    if let Some(raw) = input.get("__invalid_json") {
        return ToolOutput::error(
            format!("INVALID_JSON: the tool input was not valid JSON: {raw}"),
            title,
        );
    }
    let allowed = specs(agent, ctx).iter().any(|s| s.name == name);
    if !allowed {
        return ToolOutput::error(
            format!(
                "Tool '{name}' is not available to the {} agent.",
                agent.name()
            ),
            title,
        );
    }
    let result = match name {
        "bash" | "write" | "edit" => {
            let tracker = ctx.checkpoint.clone();
            let _ = tokio::task::spawn_blocking(move || tracker.before_change()).await;
            let result = match name {
                "bash" => bash(input, ctx).await,
                "write" => write(input, ctx).await,
                _ => edit(input, ctx).await,
            };
            // After an edit, the language server's errors for that file go to the model.
            match result {
                Ok(mut output) if name != "bash" && !output.is_error && ctx.lsp => {
                    if let Some(path) = str_arg(input, "path") {
                        let path = resolve(&ctx.cwd, path);
                        let hub = crate::lsp::Hub::for_project(&ctx.cwd, ctx.sandbox.clone());
                        if let Some(report) = hub.diagnostics_after_edit(&path, ctx.lsp_wait).await
                        {
                            output.content.push_str(&format!("\n\n{report}"));
                        }
                    }
                    Ok(output)
                }
                other => other,
            }
        }
        "read" => read(input, ctx).await,
        "glob" => glob(input, ctx).await,
        "grep" => grep(input, ctx).await,
        "list" => list(input, ctx).await,
        "read_mcp_resource" => read_mcp_resource(input, ctx, &title).await,
        #[cfg(feature = "cloud")]
        "webfetch" => webfetch(input, ctx).await,
        git if git.starts_with("git_") => crate::git::tool(git, input, ctx).await,
        "definition" | "references" | "hover" | "rename_symbol" => {
            if name == "rename_symbol" {
                let tracker = ctx.checkpoint.clone();
                let _ = tokio::task::spawn_blocking(move || tracker.before_change()).await;
            }
            crate::lsp::tool(name, input, &ctx.cwd, ctx.sandbox.clone())
                .await
                .map(|(content, title)| {
                    ToolOutput::ok(
                        content,
                        ToolMeta {
                            title,
                            ..Default::default()
                        },
                    )
                })
        }
        "symbols" | "find_symbol" | "read_symbol" => {
            let (name, input, cwd) = (name.to_owned(), input.clone(), ctx.cwd.clone());
            tokio::task::spawn_blocking(move || crate::code_index::tool(&name, &input, &cwd))
                .await
                .map_err(anyhow::Error::from)
                .and_then(|r| r)
                .map(|(content, title)| {
                    ToolOutput::ok(
                        content,
                        ToolMeta {
                            title,
                            ..Default::default()
                        },
                    )
                })
        }
        "todowrite" => todowrite(input, ctx),
        "skill" => skill(input, ctx).await,
        mcp if mcp.starts_with("mcp__") => {
            // MCP servers can change files too.
            let tracker = ctx.checkpoint.clone();
            let _ = tokio::task::spawn_blocking(move || tracker.before_change()).await;
            run_mcp(mcp, input, ctx, &title).await
        }
        other => Err(anyhow::anyhow!("unknown tool '{other}'")),
    };
    result.unwrap_or_else(|error| ToolOutput::error(format!("Error: {error:#}"), title))
}

fn truncate_middle(text: &str, max: usize) -> String {
    if text.len() <= max {
        return text.to_owned();
    }
    let head_end = floor_char(text, max / 2);
    let tail_start = floor_char(text, text.len() - max / 2);
    format!(
        "{}\n\n... [{} characters truncated] ...\n\n{}",
        &text[..head_end],
        tail_start - head_end,
        &text[tail_start..]
    )
}

fn floor_char(text: &str, mut index: usize) -> usize {
    while index > 0 && !text.is_char_boundary(index) {
        index -= 1;
    }
    index
}

/// Kills a shell command's whole process group when dropped (interrupted or timed out).
struct KillGroup(Option<u32>);

impl Drop for KillGroup {
    fn drop(&mut self) {
        #[cfg(unix)]
        if let Some(pid) = self.0 {
            unsafe {
                libc::killpg(pid as i32, libc::SIGKILL);
            }
        }
    }
}

/// PATH for shell commands: the user's, plus the built-in `rg` when ripgrep isn't installed.
fn shell_path() -> std::ffi::OsString {
    let path = std::env::var_os("PATH").unwrap_or_default();
    match crate::rg::shim_dir() {
        Some(dir) => {
            let mut dirs = vec![dir];
            dirs.extend(std::env::split_paths(&path));
            std::env::join_paths(dirs).unwrap_or(path)
        }
        None => path,
    }
}

async fn bash(input: &Value, ctx: &ToolContext) -> anyhow::Result<ToolOutput> {
    let command = str_arg(input, "command").ok_or_else(|| anyhow::anyhow!("missing command"))?;
    let workdir = str_arg(input, "workdir")
        .map(|d| resolve(&ctx.cwd, d))
        .unwrap_or_else(|| ctx.cwd.clone());
    let timeout = input
        .get("timeout")
        .and_then(Value::as_u64)
        .unwrap_or(120)
        .clamp(1, 600);

    // In the sandbox, bash runs under bwrap: the project writable, the rest read-only.
    let root = crate::context::project_root(&ctx.cwd);
    let argv: Vec<String> = match &ctx.sandbox {
        Some(sandbox) if sandbox.active() => crate::policy::sandboxed(command, sandbox, &root)
            .ok_or_else(|| anyhow::anyhow!("the sandbox (bwrap) could not be started"))?,
        Some(sandbox) if sandbox.forced => {
            anyhow::bail!("this build requires the sandbox, and bubblewrap (bwrap) isn't installed")
        }
        _ => vec!["bash".into(), "-c".into(), command.to_owned()],
    };
    let mut child = tokio::process::Command::new(&argv[0])
        .args(&argv[1..])
        .current_dir(&workdir)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .env("CRACK", "1")
        .env("PATH", shell_path())
        .env("GIT_PAGER", "cat")
        .env("PAGER", "cat")
        .kill_on_drop(true)
        // Its own process group, so an interrupted command's children stop too.
        .process_group(0)
        .spawn()?;
    let mut group = KillGroup(child.id());
    let mut stdout = child.stdout.take().expect("piped");
    let mut stderr = child.stderr.take().expect("piped");
    let collect = async {
        let mut out = Vec::new();
        let mut err = Vec::new();
        let (a, b, status) = tokio::join!(
            stdout.read_to_end(&mut out),
            stderr.read_to_end(&mut err),
            child.wait()
        );
        a?;
        b?;
        anyhow::Ok((out, err, status?))
    };
    let (text, code, timed_out) =
        match tokio::time::timeout(Duration::from_secs(timeout), collect).await {
            Ok(result) => {
                let (out, err, status) = result?;
                // Finished: leave anything it started in the background alone.
                group.0 = None;
                let mut text = String::from_utf8_lossy(&out).into_owned();
                let err = String::from_utf8_lossy(&err);
                if !err.trim().is_empty() {
                    if !text.is_empty() && !text.ends_with('\n') {
                        text.push('\n');
                    }
                    text.push_str(&err);
                }
                (text, status.code(), false)
            }
            Err(_) => (String::new(), None, true),
        };
    let text = truncate_middle(text.trim_end(), MAX_OUTPUT);
    let mut content = text.clone();
    if timed_out {
        content = format!("Command timed out after {timeout}s and was killed.");
    } else if code != Some(0) {
        content.push_str(&format!(
            "\n\n(exit code {})",
            code.map_or("signal".to_owned(), |c| c.to_string())
        ));
    }
    Ok(ToolOutput {
        is_error: timed_out,
        meta: ToolMeta {
            kind: DisplayKind::Shell,
            title: format!("$ {command}"),
            detail: Some(content.clone()),
            link: None,
        },
        content,
    })
}

async fn read(input: &Value, ctx: &ToolContext) -> anyhow::Result<ToolOutput> {
    let raw = str_arg(input, "path").ok_or_else(|| anyhow::anyhow!("missing path"))?;
    let path = resolve(&ctx.cwd, raw);
    let shown = display_path(&ctx.cwd, &path);
    if path.is_dir() {
        return list(&json!({"path": raw}), ctx).await;
    }
    let bytes = tokio::fs::read(&path).await?;
    if bytes.iter().take(8_192).any(|b| *b == 0) {
        return Ok(ToolOutput::ok(
            format!("{shown} is a binary file ({} bytes).", bytes.len()),
            ToolMeta {
                title: format!("Read {shown}"),
                ..Default::default()
            },
        ));
    }
    let text = String::from_utf8_lossy(&bytes);
    let offset = input
        .get("offset")
        .and_then(Value::as_u64)
        .unwrap_or(1)
        .max(1) as usize;
    let limit = input
        .get("limit")
        .and_then(Value::as_u64)
        .unwrap_or(2_000)
        .max(1) as usize;
    let total = text.lines().count();
    let mut out = String::new();
    let mut last = offset;
    for (number, line) in text.lines().enumerate().skip(offset - 1).take(limit) {
        let line = if line.len() > MAX_LINE {
            format!("{}… [line truncated]", &line[..floor_char(line, MAX_LINE)])
        } else {
            line.to_owned()
        };
        out.push_str(&format!("{:>6}\t{line}\n", number + 1));
        last = number + 1;
    }
    if total == 0 {
        out.push_str("(empty file)\n");
    } else if last < total {
        out.push_str(&format!(
            "\n(File has {total} lines; showing {offset}-{last}. Use offset to read more.)\n"
        ));
    }
    let mut content = truncate_middle(&out, 120_000);
    content.push_str(&nested_instructions(&path, ctx));
    Ok(ToolOutput::ok(
        content,
        ToolMeta {
            title: format!("Read {shown}:{offset}-{last}"),
            ..Default::default()
        },
    ))
}

fn unified_diff(path: &str, old: &str, new: &str) -> String {
    similar::TextDiff::from_lines(old, new)
        .unified_diff()
        .context_radius(3)
        .header(&format!("a/{path}"), &format!("b/{path}"))
        .to_string()
}

async fn write(input: &Value, ctx: &ToolContext) -> anyhow::Result<ToolOutput> {
    let raw = str_arg(input, "path").ok_or_else(|| anyhow::anyhow!("missing path"))?;
    let content = str_arg(input, "content").ok_or_else(|| anyhow::anyhow!("missing content"))?;
    let path = resolve(&ctx.cwd, raw);
    let shown = display_path(&ctx.cwd, &path);
    let existed = path.exists();
    if let Some(parent) = path.parent() {
        tokio::fs::create_dir_all(parent).await?;
    }
    tokio::fs::write(&path, content).await?;
    let lines = content.lines().count();
    Ok(ToolOutput::ok(
        format!(
            "{} {shown} ({lines} lines).",
            if existed { "Overwrote" } else { "Created" }
        ),
        ToolMeta {
            kind: DisplayKind::Write,
            title: format!("Wrote {shown}"),
            detail: Some(truncate_middle(content, 8_000)),
            link: None,
        },
    ))
}

async fn edit(input: &Value, ctx: &ToolContext) -> anyhow::Result<ToolOutput> {
    let raw = str_arg(input, "path").ok_or_else(|| anyhow::anyhow!("missing path"))?;
    let old = str_arg(input, "old_string").ok_or_else(|| anyhow::anyhow!("missing old_string"))?;
    let new = str_arg(input, "new_string").ok_or_else(|| anyhow::anyhow!("missing new_string"))?;
    let replace_all = input
        .get("replace_all")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let path = resolve(&ctx.cwd, raw);
    let shown = display_path(&ctx.cwd, &path);
    let title = format!("Edit {shown}");

    if old.is_empty() {
        // An empty old_string creates the file.
        if path.exists() {
            return Ok(ToolOutput::error(
                "old_string is empty but the file exists; give the text to replace.",
                title,
            ));
        }
        return write(&json!({"path": raw, "content": new}), ctx).await;
    }
    if old == new {
        return Ok(ToolOutput::error(
            "old_string and new_string are identical.",
            title,
        ));
    }
    let before = tokio::fs::read_to_string(&path).await?;
    let count = before.matches(old).count();
    if count == 0 {
        return Ok(ToolOutput::error(
            format!(
                "old_string was not found in {shown}. Read the file and copy the text exactly."
            ),
            title,
        ));
    }
    if count > 1 && !replace_all {
        return Ok(ToolOutput::error(
            format!(
                "old_string occurs {count} times in {shown}. Add surrounding context to make it unique, or set replace_all."
            ),
            title,
        ));
    }
    let after = if replace_all {
        before.replace(old, new)
    } else {
        before.replacen(old, new, 1)
    };
    tokio::fs::write(&path, &after).await?;
    let diff = unified_diff(&shown, &before, &after);
    Ok(ToolOutput::ok(
        format!(
            "Edited {shown} ({} replacement{}).",
            if replace_all { count } else { 1 },
            if replace_all && count != 1 { "s" } else { "" }
        ),
        ToolMeta {
            kind: DisplayKind::Diff,
            title,
            detail: Some(truncate_middle(&diff, 12_000)),
            link: None,
        },
    ))
}

fn walker(root: &Path) -> ignore::Walk {
    ignore::WalkBuilder::new(root)
        .hidden(false)
        .filter_entry(|e| e.file_name() != ".git")
        .build()
}

async fn glob(input: &Value, ctx: &ToolContext) -> anyhow::Result<ToolOutput> {
    let pattern = str_arg(input, "pattern")
        .ok_or_else(|| anyhow::anyhow!("missing pattern"))?
        .to_owned();
    let root = str_arg(input, "path")
        .map(|p| resolve(&ctx.cwd, p))
        .unwrap_or_else(|| ctx.cwd.clone());
    let cwd = ctx.cwd.clone();
    let shown_root = display_path(&cwd, &root);
    let title_pattern = pattern.clone();
    let matches = tokio::task::spawn_blocking(move || -> anyhow::Result<Vec<String>> {
        let matcher = globset::GlobBuilder::new(&pattern)
            .literal_separator(false)
            .build()?
            .compile_matcher();
        let mut found: Vec<(std::time::SystemTime, PathBuf)> = Vec::new();
        for entry in walker(&root).flatten() {
            if !entry.file_type().is_some_and(|t| t.is_file()) {
                continue;
            }
            let relative = entry.path().strip_prefix(&root).unwrap_or(entry.path());
            if matcher.is_match(relative) || matcher.is_match(entry.file_name()) {
                let modified = entry
                    .metadata()
                    .ok()
                    .and_then(|m| m.modified().ok())
                    .unwrap_or(std::time::UNIX_EPOCH);
                found.push((modified, entry.path().to_owned()));
            }
        }
        found.sort_by_key(|f| std::cmp::Reverse(f.0));
        Ok(found
            .into_iter()
            .map(|(_, p)| display_path(&cwd, &p))
            .collect())
    })
    .await??;
    let count = matches.len();
    let mut content = matches
        .iter()
        .take(200)
        .cloned()
        .collect::<Vec<_>>()
        .join("\n");
    if count == 0 {
        content = "No files found.".into();
    } else if count > 200 {
        content.push_str(&format!("\n\n({count} matches; showing the 200 newest)"));
    }
    Ok(ToolOutput::ok(
        content,
        ToolMeta {
            title: format!(
                "Glob \"{title_pattern}\" in {} ({count} matches)",
                if shown_root.is_empty() {
                    "."
                } else {
                    &shown_root
                }
            ),
            ..Default::default()
        },
    ))
}

async fn grep(input: &Value, ctx: &ToolContext) -> anyhow::Result<ToolOutput> {
    let pattern = str_arg(input, "pattern")
        .ok_or_else(|| anyhow::anyhow!("missing pattern"))?
        .to_owned();
    let root = str_arg(input, "path")
        .map(|p| resolve(&ctx.cwd, p))
        .unwrap_or_else(|| ctx.cwd.clone());
    let include = str_arg(input, "include").map(str::to_owned);
    let insensitive = input
        .get("case_insensitive")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let cwd = ctx.cwd.clone();
    let shown_root = display_path(&cwd, &root);
    let title_pattern = pattern.clone();
    let (lines, count) =
        tokio::task::spawn_blocking(move || -> anyhow::Result<(Vec<String>, usize)> {
            use grep::searcher::{BinaryDetection, SearcherBuilder, sinks::Lossy};
            let matcher = grep::regex::RegexMatcherBuilder::new()
                .case_insensitive(insensitive)
                .line_terminator(Some(b'\n'))
                .build(&pattern)?;
            let mut searcher = SearcherBuilder::new()
                .line_number(true)
                .binary_detection(BinaryDetection::quit(0))
                .build();
            let include = include
                .map(|g| globset::Glob::new(&g).map(|g| g.compile_matcher()))
                .transpose()?;
            let mut lines = Vec::new();
            let mut count = 0;
            for entry in walker(&root).flatten() {
                if !entry.file_type().is_some_and(|t| t.is_file()) {
                    continue;
                }
                if let Some(include) = &include
                    && !include.is_match(entry.file_name())
                    && !include.is_match(entry.path())
                {
                    continue;
                }
                let shown = display_path(&cwd, entry.path());
                // Unreadable files are skipped, like ripgrep does with a warning.
                let _ = searcher.search_path(
                    &matcher,
                    entry.path(),
                    Lossy(|number, line| {
                        count += 1;
                        if lines.len() < 250 {
                            let line = line.trim();
                            let line = &line[..floor_char(line, 300)];
                            lines.push(format!("{shown}:{number}: {line}"));
                        }
                        Ok(true)
                    }),
                );
            }
            Ok((lines, count))
        })
        .await??;
    let mut content = lines.join("\n");
    if count == 0 {
        content = "No matches found.".into();
    } else if count > lines.len() {
        content.push_str(&format!("\n\n({count} matches; showing {})", lines.len()));
    }
    Ok(ToolOutput::ok(
        content,
        ToolMeta {
            title: format!(
                "Grep \"{title_pattern}\" in {} ({count} matches)",
                if shown_root.is_empty() {
                    "."
                } else {
                    &shown_root
                }
            ),
            ..Default::default()
        },
    ))
}

async fn list(input: &Value, ctx: &ToolContext) -> anyhow::Result<ToolOutput> {
    let root = str_arg(input, "path")
        .map(|p| resolve(&ctx.cwd, p))
        .unwrap_or_else(|| ctx.cwd.clone());
    let shown = display_path(&ctx.cwd, &root);
    let shown = if shown.is_empty() {
        ".".to_owned()
    } else {
        shown
    };
    let walk_root = root.clone();
    let entries = tokio::task::spawn_blocking(move || {
        let mut entries: Vec<String> = ignore::WalkBuilder::new(&walk_root)
            .hidden(false)
            .max_depth(Some(2))
            .filter_entry(|e| e.file_name() != ".git")
            .build()
            .flatten()
            .filter(|e| e.depth() > 0)
            .map(|e| {
                let relative = e.path().strip_prefix(&walk_root).unwrap_or(e.path());
                let mut s = relative.display().to_string();
                if e.file_type().is_some_and(|t| t.is_dir()) {
                    s.push('/');
                }
                s
            })
            .collect();
        entries.sort();
        entries
    })
    .await?;
    let count = entries.len();
    let mut content = entries.into_iter().take(500).collect::<Vec<_>>().join("\n");
    if count > 500 {
        content.push_str(&format!("\n\n({count} entries; showing 500)"));
    }
    if count == 0 {
        content = "(empty directory)".into();
    }
    Ok(ToolOutput::ok(
        format!("{}/\n{content}", root.display()),
        ToolMeta {
            title: format!("List {shown} ({count} entries)"),
            ..Default::default()
        },
    ))
}

#[cfg(feature = "cloud")]
async fn webfetch(input: &Value, ctx: &ToolContext) -> anyhow::Result<ToolOutput> {
    let url = str_arg(input, "url").ok_or_else(|| anyhow::anyhow!("missing url"))?;
    let response = ctx
        .http
        .get(url)
        .header("user-agent", "crack/0.1 (+https://github.com)")
        .timeout(Duration::from_secs(30))
        .send()
        .await?;
    let status = response.status();
    let is_html = response
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.contains("html"));
    let body = response.text().await?;
    let text = if is_html { html_to_text(&body) } else { body };
    let content = truncate_middle(&text, 60_000);
    Ok(ToolOutput {
        is_error: !status.is_success(),
        content: if status.is_success() {
            content
        } else {
            format!("HTTP {status}\n{content}")
        },
        meta: ToolMeta {
            title: format!("WebFetch {url}"),
            ..Default::default()
        },
    })
}

#[cfg(feature = "cloud")]
/// Rough HTML to text: drops scripts, styles and tags, keeps line structure.
fn html_to_text(html: &str) -> String {
    let strip_blocks =
        regex::Regex::new(r"(?is)<script\b.*?</script>|<style\b.*?</style>|<noscript\b.*?</noscript>|<svg\b.*?</svg>|<head\b.*?</head>")
            .unwrap();
    let breaks = regex::Regex::new(r"(?i)<(br|/p|/div|/li|/h[1-6]|/tr|/pre)[^>]*>").unwrap();
    let tags = regex::Regex::new(r"(?s)<[^>]+>").unwrap();
    let blank = regex::Regex::new(r"\n\s*\n\s*\n+").unwrap();
    let text = strip_blocks.replace_all(html, "");
    let text = breaks.replace_all(&text, "\n");
    let text = tags.replace_all(&text, "");
    let text = text
        .replace("&nbsp;", " ")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&amp;", "&");
    let text: String = text.lines().map(str::trim).collect::<Vec<_>>().join("\n");
    blank.replace_all(&text, "\n\n").trim().to_owned()
}

fn todowrite(input: &Value, ctx: &ToolContext) -> anyhow::Result<ToolOutput> {
    let todos: Vec<Todo> = serde_json::from_value(
        input
            .get("todos")
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("missing todos"))?,
    )?;
    let rendered = render_todos(&todos);
    let remaining = todos
        .iter()
        .filter(|t| t.status == "pending" || t.status == "in_progress")
        .count();
    *ctx.todos.lock().unwrap() = todos;
    Ok(ToolOutput::ok(
        format!("Todo list updated ({remaining} remaining).\n{rendered}"),
        ToolMeta {
            kind: DisplayKind::Todos,
            title: "Todos".into(),
            detail: Some(rendered),
            link: None,
        },
    ))
}

pub fn render_todos(todos: &[Todo]) -> String {
    todos
        .iter()
        .map(|t| {
            let mark = match t.status.as_str() {
                "completed" => "[✓]",
                "in_progress" => "[•]",
                "cancelled" => "[✗]",
                _ => "[ ]",
            };
            format!("{mark} {}", t.content)
        })
        .collect::<Vec<_>>()
        .join("\n")
}

async fn skill(input: &Value, ctx: &ToolContext) -> anyhow::Result<ToolOutput> {
    let name = str_arg(input, "name").ok_or_else(|| anyhow::anyhow!("missing name"))?;
    let skill = ctx.skills.iter().find(|s| s.name == name).ok_or_else(|| {
        let names: Vec<&str> = ctx.skills.iter().map(|s| s.name.as_str()).collect();
        anyhow::anyhow!("no skill named '{name}'. Available: {}", names.join(", "))
    })?;
    Ok(ToolOutput::ok(
        skill.load()?,
        ToolMeta {
            title: format!("Skill \"{name}\""),
            ..Default::default()
        },
    ))
}

/// AGENTS.md files between a read file and the working directory that the model
/// hasn't seen yet (deeper directories have their own rules), appended once each.
fn nested_instructions(file: &Path, ctx: &ToolContext) -> String {
    let mut out = String::new();
    let Some(dir) = file.parent() else {
        return out;
    };
    if !dir.starts_with(&ctx.cwd) {
        return out;
    }
    let mut loaded = ctx.loaded.lock().unwrap();
    let mut found = Vec::new();
    for ancestor in dir.ancestors() {
        if ancestor == ctx.cwd {
            break;
        }
        let agents = ancestor.join("AGENTS.md");
        let candidate = if agents.is_file() {
            agents
        } else {
            ancestor.join("CLAUDE.md")
        };
        if candidate.is_file() && candidate != file && !loaded.contains(&candidate) {
            found.push(candidate);
        }
    }
    for path in found.into_iter().rev() {
        if let Ok(text) = std::fs::read_to_string(&path) {
            out.push_str(&format!(
                "\n<system-reminder>\nInstructions from: {}\n{}\n</system-reminder>\n",
                path.display(),
                text.trim()
            ));
        }
        loaded.insert(path);
    }
    out
}

async fn read_mcp_resource(
    input: &Value,
    ctx: &ToolContext,
    title: &str,
) -> anyhow::Result<ToolOutput> {
    let server = str_arg(input, "server").ok_or_else(|| anyhow::anyhow!("missing server"))?;
    let uri = str_arg(input, "uri").ok_or_else(|| anyhow::anyhow!("missing uri"))?;
    let text = ctx.mcp.read_resource(server, uri).await?;
    Ok(ToolOutput::ok(
        if text.is_empty() {
            "(empty resource)".to_owned()
        } else {
            truncate_middle(&text, MAX_OUTPUT)
        },
        ToolMeta {
            title: title.to_owned(),
            ..Default::default()
        },
    ))
}

async fn run_mcp(
    name: &str,
    input: &Value,
    ctx: &ToolContext,
    title: &str,
) -> anyhow::Result<ToolOutput> {
    let (text, is_error) = ctx.mcp.call(name, input.clone()).await?;
    let text = truncate_middle(&text, MAX_OUTPUT);
    Ok(ToolOutput {
        is_error,
        content: if text.is_empty() {
            "(no output)".into()
        } else {
            text.clone()
        },
        meta: ToolMeta {
            title: title.to_owned(),
            detail: is_error.then_some(text),
            ..Default::default()
        },
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ctx(dir: &Path) -> ToolContext {
        ToolContext {
            cwd: dir.to_owned(),
            skills: Arc::new(vec![]),
            todos: Arc::new(Mutex::new(vec![])),
            http: reqwest::Client::new(),
            allow: None,
            loaded: Default::default(),
            checkpoint: Default::default(),
            lsp: false,
            lsp_wait: Duration::from_secs(3),
            mcp: crate::mcp::McpHub::new(Default::default(), reqwest::Client::new()),
            sandbox: None,
        }
    }

    #[cfg(feature = "cloud")]
    #[test]
    fn html_to_text_drops_scripts_and_tags() {
        let html = "<html><head><title>x</title></head><body><script>var a = 1;</script>\n<p>Hello &amp; <b>welcome</b></p><style>p{}</style><div>Bye</div></body></html>";
        let text = html_to_text(html);
        assert!(text.contains("Hello & welcome"), "{text}");
        assert!(text.contains("Bye"));
        assert!(!text.contains("var a") && !text.contains("p{}") && !text.contains('<'));
    }

    #[tokio::test]
    async fn edit_requires_unique_match_and_reports_diff() {
        let dir = std::env::temp_dir().join(format!("crack-test-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("f.txt"), "a\nb\na\n").unwrap();
        let ctx = ctx(&dir);
        let out = run(
            "edit",
            &json!({"path": "f.txt", "old_string": "a", "new_string": "c"}),
            &ctx,
            Agent::Build,
        )
        .await;
        assert!(out.is_error && out.content.contains("2 times"));
        let out = run(
            "edit",
            &json!({"path": "f.txt", "old_string": "b\n", "new_string": "B\n"}),
            &ctx,
            Agent::Build,
        )
        .await;
        assert!(!out.is_error, "{}", out.content);
        assert!(out.meta.detail.unwrap().contains("+B"));
        assert_eq!(
            std::fs::read_to_string(dir.join("f.txt")).unwrap(),
            "a\nB\na\n"
        );
        // Plan can't write.
        let out = run(
            "write",
            &json!({"path": "g.txt", "content": "x"}),
            &ctx,
            Agent::Plan,
        )
        .await;
        assert!(out.is_error);
        let out = run(
            "bash",
            &json!({"command": "echo hi; exit 3"}),
            &ctx,
            Agent::Build,
        )
        .await;
        assert!(out.content.contains("hi") && out.content.contains("exit code 3"));
        let out = run("grep", &json!({"pattern": "^B$"}), &ctx, Agent::Build).await;
        assert!(out.content.contains("f.txt:2"), "{}", out.content);
        std::fs::remove_dir_all(dir).unwrap();
    }
}
