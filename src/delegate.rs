//! The `task` tool: runs a subagent with a fresh context, its own prompt, tools and
//! model, and hands back its final report. Several run at once; edits can go to a
//! separate git worktree and come back as a patch.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use serde_json::{Value, json};
use tokio::sync::mpsc;

use crate::agent::{AgentEvent, Events, TurnConfig};
use crate::providers::{Message, Role};
use crate::session::Session;
use crate::subagents::AgentDef;
use crate::tools::{Agent, DisplayKind, ToolContext, ToolMeta, ToolOutput};

/// What a turn needs to start subagents. Absent inside a subagent (depth 1).
pub struct SubagentEnv {
    pub registry: crate::providers::registry::Registry,
    pub defs: Vec<AgentDef>,
    pub parent_model: String,
    pub small_model: Option<String>,
    pub instructions: Vec<crate::context::Instruction>,
    pub max_parallel: usize,
    pub max_steps: u32,
    pub limit: Arc<tokio::sync::Semaphore>,
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
        let max_parallel = max_parallel.clamp(1, 16);
        SubagentEnv {
            registry,
            defs: crate::subagents::discover(cwd),
            parent_model,
            small_model,
            instructions,
            max_parallel,
            max_steps,
            limit: Arc::new(tokio::sync::Semaphore::new(max_parallel)),
        }
    }

    /// The `task` tool's definition, listing the agents it can start.
    pub fn spec(&self, parent: Agent) -> crate::providers::ToolSpec {
        let mut listing = String::new();
        for def in self
            .defs
            .iter()
            .filter(|d| parent == Agent::Build || read_only(d))
        {
            listing.push_str(&format!("- {}: {}\n", def.name, def.description));
        }
        crate::providers::ToolSpec {
            name: "task".into(),
            description: format!(
                "Delegate a self-contained job to a subagent with a fresh context. It can't see this conversation, \
                 so the prompt must say everything it needs: the goal, what you know, where to look, and what to report. \
                 It returns one final report. Call task several times in one response to run subagents in parallel \
                 (up to {}). Use `explore` for read-only searching so your own context stays small.\n\nAgents:\n{listing}",
                self.max_parallel
            ),
            schema: json!({
                "type": "object",
                "properties": {
                    "description": {"type": "string", "description": "A short (3-5 word) label for the job."},
                    "prompt": {"type": "string", "description": "The complete task for the subagent."},
                    "subagent_type": {"type": "string", "description": "Which agent to use (default general)."}
                },
                "required": ["description", "prompt"]
            }),
        }
    }
}

fn read_only(def: &AgentDef) -> bool {
    def.tools.as_ref().is_some_and(|tools| {
        tools
            .iter()
            .all(|t| crate::subagents::READ_ONLY_TOOLS.contains(&t.as_str()))
    })
}

/// Runs one `task` call to completion.
pub async fn run(
    config: &TurnConfig,
    env: &SubagentEnv,
    ctx: &ToolContext,
    id: &str,
    input: &Value,
    events: &Events,
) -> ToolOutput {
    let description = input["description"].as_str().unwrap_or("task").to_owned();
    let prompt = input["prompt"].as_str().unwrap_or_default().to_owned();
    let kind = input["subagent_type"]
        .as_str()
        .filter(|s| !s.is_empty())
        .unwrap_or("general");
    let title = format!("Task: {description} ({kind})");
    let fail = |text: String| ToolOutput::error(text, title.clone());
    let Some(def) = env.defs.iter().find(|d| d.name == kind).cloned() else {
        let names: Vec<&str> = env.defs.iter().map(|d| d.name.as_str()).collect();
        return fail(format!(
            "No agent named '{kind}'. Available: {}",
            names.join(", ")
        ));
    };
    if config.agent == Agent::Plan && !read_only(&def) {
        return fail(format!(
            "In Plan mode only read-only agents can run; '{kind}' can change files."
        ));
    }
    if prompt.trim().is_empty() {
        return fail("The prompt is empty.".into());
    }

    let _permit = env.limit.acquire().await;
    let model_ref = crate::subagents::choose_model(
        &def,
        &env.parent_model,
        &env.registry,
        env.small_model.as_deref(),
    );
    let (provider, model) = match env.registry.resolve(&model_ref) {
        Ok((p, m)) => (p.endpoint.clone(), m),
        Err(error) => return fail(format!("Can't start {kind}: {error:#}")),
    };
    let title = format!("Task: {description} ({kind} · {model})");
    let fail = |text: String| ToolOutput::error(text, title.clone());

    // An agent that edits in isolation gets its own worktree.
    let worktree = if def.worktree && config.agent == Agent::Build {
        match Worktree::create(&ctx.cwd, id) {
            Ok(w) => Some(w),
            Err(error) => return fail(format!("Couldn't create a worktree: {error:#}")),
        }
    } else {
        None
    };
    let cwd = worktree
        .as_ref()
        .map(|w| w.path.clone())
        .unwrap_or_else(|| ctx.cwd.clone());

    let mut system = crate::context::SystemPrompt {
        cwd: &cwd,
        agent: Agent::Build,
        model: &model_ref,
        instructions: &env.instructions,
        skills: &ctx.skills,
    }
    .render();
    system.push_str("\n# Your role\nYou are a subagent: another agent delegated this task to you. Only your final message goes back to it, so end with a complete, concise report.\n\n");
    system.push_str(&def.prompt);

    let mut policy = config.policy.clone();
    if let Some(w) = &worktree {
        policy.root = w.path.clone();
        policy.cwd = w.path.clone();
    }
    let sub_config = TurnConfig {
        hooks: config.hooks.clone(),
        endpoint: provider,
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
        context_tokens: 0,
        subagents: None,
    };
    let sub_ctx = ToolContext {
        cwd: cwd.clone(),
        allow: def.tools.clone(),
        todos: Arc::new(Mutex::new(Vec::new())),
        loaded: Default::default(),
        // Edits in the main tree belong to this turn's checkpoint, so undo covers them.
        checkpoint: if worktree.is_some() {
            Default::default()
        } else {
            ctx.checkpoint.clone()
        },
        // Memory reminders are for the main conversation.
        memory: false,
        ..ctx.clone()
    };

    let mut session = Session::new(&cwd);
    session.id = format!("sub-{}", session.id);
    session.push_user(
        &format!("{description} ({kind})\n\n{prompt}"),
        prompt.clone(),
        Agent::Build,
        None,
    );
    session.title = format!("\u{21b3} {description}");
    let mut history = vec![Message::user_text(prompt)];
    let (tx, mut rx) = mpsc::unbounded_channel();
    let started = std::time::Instant::now();
    let send_status = |text: String| {
        let _ = events.send(AgentEvent::SubagentStatus {
            id: id.to_owned(),
            text,
        });
    };
    send_status(format!("starting on {model_ref}"));

    let mut steps = 0u32;
    let work = async {
        let result = crate::agent::turn_loop(&sub_config, &mut history, &sub_ctx, &tx).await;
        drop(tx);
        result
    };
    let watch = async {
        while let Some(event) = rx.recv().await {
            match &event {
                AgentEvent::ToolCall { name, input, .. } => {
                    steps += 1;
                    send_status(format!(
                        "{steps} \u{b7} {}",
                        crate::tools::title(name, input)
                    ));
                }
                AgentEvent::Approval { reason, .. } => {
                    send_status(format!("waiting for your approval: {reason}"))
                }
                AgentEvent::Usage(usage) => {
                    let _ = events.send(AgentEvent::SubagentUsage(*usage));
                }
                _ => {}
            }
            session.apply(&event, Agent::Build, &model_ref);
        }
    };
    let (result, ()) = tokio::join!(work, watch);
    session.apply(
        &AgentEvent::Done {
            interrupted: false,
            seconds: started.elapsed().as_secs_f32(),
        },
        Agent::Build,
        &model_ref,
    );
    let _ = session.save();

    let hook = config
        .hooks
        .run(
            "SubagentStop",
            None,
            json!({"subagent_type": kind, "description": description}),
        )
        .await;
    for notice in hook.notices {
        let _ = events.send(AgentEvent::Notice(notice));
    }

    let report = history
        .iter()
        .rev()
        .find(|m| m.role == Role::Assistant && !m.text().trim().is_empty())
        .map(|m| m.text())
        .unwrap_or_default();
    let mut meta = ToolMeta {
        title,
        kind: DisplayKind::Inline,
        link: Some(session.id.clone()),
        ..Default::default()
    };
    let mut content = match (&result, report.trim().is_empty()) {
        (Err(error), _) => {
            meta.detail = Some(format!("{error:#}"));
            return ToolOutput {
                content: format!("The subagent failed: {error:#}\n\n{report}"),
                is_error: true,
                meta,
            };
        }
        (Ok(()), true) => "The subagent finished without a report.".to_owned(),
        (Ok(()), false) => report,
    };
    if let Some(worktree) = worktree {
        match worktree.finish(id) {
            Ok(Some((patch, path, files))) => {
                content.push_str(&format!(
                    "\n\nThe subagent worked in a separate git worktree. Its changes to {files} file(s) are NOT applied; \
                     they're saved as a patch at {}. Review it, then apply it with `git apply {}` if it's right.",
                    path.display(),
                    path.display()
                ));
                meta.kind = DisplayKind::Diff;
                meta.detail = Some(patch);
            }
            Ok(None) => content.push_str("\n\n(The subagent made no file changes.)"),
            Err(error) => content.push_str(&format!(
                "\n\n(Couldn't collect the worktree's changes: {error:#})"
            )),
        }
    }
    ToolOutput {
        content,
        is_error: false,
        meta,
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
