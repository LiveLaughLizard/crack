//! Everything a session needs from the place crack was launched: the working
//! directory, configuration, providers, skills and instruction files.

use crate::agent::TurnConfig;
use crate::config::Config;
use crate::context::{self, Instruction, SystemPrompt};
use crate::mcp::McpHub;
use crate::providers::{Effort, Registry};
use crate::skills::{self, Skill};
use crate::tools::{Agent, Todo, ToolContext};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

#[derive(Clone)]
pub struct Workspace {
    pub cwd: PathBuf,
    pub config: Config,
    pub registry: Registry,
    pub skills: Arc<Vec<Skill>>,
    pub instructions: Vec<Instruction>,
    pub http: reqwest::Client,
    /// MCP servers; connect them with `mcp.connect_all()`.
    pub mcp: Arc<McpHub>,
}

/// Session settings chosen on the command line or in the interface.
#[derive(Clone, Debug)]
pub struct Selection {
    /// `provider/model`.
    pub model: Option<String>,
    pub effort: Effort,
    pub agent: Agent,
    /// How much runs without asking (normal, accept edits, YOLO).
    pub mode: crate::policy::Mode,
}

impl Workspace {
    pub fn load(cwd: &Path) -> anyhow::Result<Workspace> {
        let cwd = cwd.canonicalize()?;
        let config = Config::load(&cwd)?;
        let registry = Registry::load(&config);
        let skills = Arc::new(skills::discover(&cwd, &config));
        let instructions = context::instructions(&cwd, &config);
        let http = reqwest::Client::builder()
            .connect_timeout(std::time::Duration::from_secs(20))
            .build()?;
        let mcp = McpHub::new(crate::mcp::configured(&cwd, &config), http.clone());
        Ok(Workspace {
            cwd,
            config,
            registry,
            skills,
            instructions,
            http,
            mcp,
        })
    }

    /// Re-reads instruction files and skills (after the user edits them).
    pub fn reload_context(&mut self) {
        // A fresh hub: the old servers are stopped when it is dropped.
        self.mcp = McpHub::new(
            crate::mcp::configured(&self.cwd, &self.config),
            self.http.clone(),
        );
        self.skills = Arc::new(skills::discover(&self.cwd, &self.config));
        self.instructions = context::instructions(&self.cwd, &self.config);
    }

    /// Defaults from the environment and config, overridden by explicit choices.
    pub fn default_selection(&self) -> Selection {
        Selection {
            model: std::env::var("CRACK_MODEL")
                .ok()
                .or_else(|| self.config.model.clone())
                .or_else(|| self.registry.default_model()),
            effort: self
                .config
                .effort
                .as_deref()
                .and_then(Effort::parse)
                .unwrap_or_default(),
            agent: self
                .config
                .agent
                .as_deref()
                .and_then(Agent::parse)
                .unwrap_or_default(),
            mode: crate::policy::Mode::Normal,
        }
    }

    pub fn turn_config(&self, selection: &Selection) -> anyhow::Result<TurnConfig> {
        let model_ref = selection.model.as_deref().ok_or_else(|| {
            anyhow::anyhow!(
                "no model available: set a provider API key (e.g. ANTHROPIC_API_KEY, OPENAI_API_KEY) \
                 or configure one in ~/.config/crack/config.toml"
            )
        })?;
        let (provider, model) = self.registry.resolve(model_ref)?;
        let system = SystemPrompt {
            cwd: &self.cwd,
            agent: selection.agent,
            model: model_ref,
            instructions: &self.instructions,
            skills: &self.skills,
        }
        .render();
        Ok(TurnConfig {
            endpoint: provider.endpoint.clone(),
            hooks: crate::hooks::Hooks::new(&self.config.hooks, &self.cwd, ""),
            model,
            system,
            effort: selection.effort,
            agent: selection.agent,
            max_tokens: self.config.max_tokens.unwrap_or(64_000),
            max_steps: None,
            policy: crate::policy::Policy::new(
                &self.config.permissions,
                &self.cwd,
                selection.mode,
                crate::policy::Sandbox::resolve(&self.config.permissions).active(),
            ),
            session_rules: Default::default(),
            approvals: None,
            context_window: self.registry.caps(model_ref).context,
            compact_at: self.config.compact_at.unwrap_or(0.8).clamp(0.3, 0.95),
            context_tokens: 0,
        })
    }

    pub fn tool_context(
        &self,
        session: &str,
        todos: Arc<Mutex<Vec<Todo>>>,
        allow: Option<Vec<String>>,
    ) -> ToolContext {
        let snapshots = if self.config.checkpoints.unwrap_or(true) {
            crate::checkpoint::Snapshots::open(&crate::context::project_root(&self.cwd), session)
        } else {
            None
        };
        ToolContext {
            cwd: self.cwd.clone(),
            mcp: self.mcp.clone(),
            sandbox: Some(crate::policy::Sandbox::resolve(&self.config.permissions)),
            skills: self.skills.clone(),
            todos,
            http: self.http.clone(),
            allow,
            loaded: Arc::new(Mutex::new(
                self.instructions.iter().map(|i| i.path.clone()).collect(),
            )),
            checkpoint: Arc::new(crate::checkpoint::Tracker::new(snapshots)),
        }
    }

    pub fn skill(&self, name: &str) -> Option<&Skill> {
        self.skills.iter().find(|s| s.name == name)
    }

    /// Expands `/skill-name rest of request` into the skill's instructions plus the
    /// request. Returns the text for the model and the skill's name.
    pub fn expand_skill(&self, input: &str) -> Option<(String, String)> {
        let rest = input.strip_prefix('/')?;
        let (name, args) = rest.split_once(char::is_whitespace).unwrap_or((rest, ""));
        let skill = self.skill(name)?;
        let body = skill.load().ok()?;
        let request = if args.trim().is_empty() {
            "Follow the skill's instructions.".to_owned()
        } else {
            args.trim().to_owned()
        };
        Some((
            format!("The user invoked the \"{name}\" skill.\n\n{body}\n\n{request}"),
            skill.name.clone(),
        ))
    }

    /// `~/path` for display.
    pub fn short_cwd(&self) -> String {
        let shown = match dirs::home_dir() {
            Some(home) if self.cwd.starts_with(&home) => {
                format!("~/{}", self.cwd.strip_prefix(&home).unwrap().display())
            }
            _ => self.cwd.display().to_string(),
        };
        shown.trim_end_matches('/').to_owned()
    }
}
