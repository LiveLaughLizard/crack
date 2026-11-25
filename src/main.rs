//! crack: a coding agent with a GPUI interface that behaves like a terminal agent.
//!
//! `crack` opens the interface on the directory it was launched from (or `--dir`).
//! `crack run "..."` does one turn headless for scripts and CI.

mod agent;
mod config;
mod context;
mod headless;
mod mcp;
mod policy;
mod providers;
mod rg;
mod runtime;
mod sandbox;
mod session;
mod skills;
mod state;
mod store;
mod tools;
mod ui;
mod workspace;

use flag_rs::{CommandBuilder, CompletionResult, Context, Flag, Shell};
use providers::Effort;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use tools::Agent;
use workspace::{Selection, Workspace};

/// What the command line asked for. The flag-rs handlers record it; `main` acts on
/// it afterwards (the window has to open on the main thread, outside the parser).
enum Mode {
    Gui {
        common: Common,
        prompt: Option<String>,
    },
    Run {
        common: Common,
        prompt_args: Vec<String>,
        format: headless::Format,
        max_steps: Option<u32>,
        tools: Option<Vec<String>>,
        permission_mode: policy::Mode,
        sandbox: Option<bool>,
    },
    Models {
        common: Common,
        provider: Option<String>,
    },
    Providers {
        common: Common,
    },
    Skills {
        common: Common,
    },
    Sessions {
        common: Common,
    },
    Mcp {
        common: Common,
    },
    Completion(Shell),
}

struct Common {
    dir: PathBuf,
    model: Option<String>,
    effort: Option<Effort>,
    agent: Option<Agent>,
    session: Option<String>,
}

fn common(ctx: &Context) -> flag_rs::Result<Common> {
    let dir = match ctx.flag("dir") {
        Some(dir) => PathBuf::from(dir),
        None => std::env::current_dir().map_err(flag_rs::Error::Io)?,
    };
    let effort = ctx
        .flag("effort")
        .map(|e| {
            Effort::parse(e).ok_or_else(|| {
                flag_rs::Error::Validation(format!(
                    "unknown effort '{e}' (default, minimal, low, medium, high, xhigh, max)"
                ))
            })
        })
        .transpose()?;
    let agent = ctx
        .flag("agent")
        .map(|a| {
            Agent::parse(a).ok_or_else(|| {
                flag_rs::Error::Validation(format!("unknown agent '{a}' (build, plan)"))
            })
        })
        .transpose()?;
    Ok(Common {
        dir,
        model: ctx.flag("model").cloned(),
        effort,
        agent,
        session: ctx.flag("session").cloned(),
    })
}

/// `--model` completes from the providers configured for the current directory.
fn complete_models(_ctx: &Context, prefix: &str) -> flag_rs::Result<CompletionResult> {
    let cwd = std::env::current_dir().unwrap_or_default();
    let mut result = CompletionResult::new();
    if let Ok(workspace) = Workspace::load(&cwd) {
        for provider in workspace.registry.available() {
            for model in &provider.models {
                let id = format!("{}/{}", provider.id, model.id);
                if id.starts_with(prefix) {
                    result = result.add_with_description(id, provider.name.clone());
                }
            }
        }
    }
    Ok(result)
}

fn build_cli(mode: Arc<Mutex<Option<Mode>>>) -> flag_rs::Command {
    let set = |mode: &Arc<Mutex<Option<Mode>>>, value: Mode| {
        *mode.lock().unwrap() = Some(value);
    };

    let gui_mode = mode.clone();
    let run_mode = mode.clone();
    let models_mode = mode.clone();
    let providers_mode = mode.clone();
    let skills_mode = mode.clone();
    let sessions_mode = mode.clone();
    let mcp_mode = mode.clone();
    let completion_mode = mode.clone();

    CommandBuilder::new("crack")
        .short("A coding agent with a GPUI interface")
        .long(
            "crack opens a window on the directory it was launched from and works there like a \
             terminal coding agent: it reads AGENTS.md, loads skills, runs shell commands and edits files.\n\n\
             Run `crack run \"<prompt>\"` for one headless turn in scripts and CI.",
        )
        .example("crack")
        .example("crack -C ~/src/app -m anthropic/claude-opus-5-5 -e high")
        .example("crack run \"fix the failing tests\" --format json")
        .example("git diff | crack run \"review this change\" -a plan")
        .flags(vec![
            Flag::string("dir")
                .short('C')
                .usage("Working directory (default: the current directory)"),
            Flag::string("model")
                .short('m')
                .usage("Model as provider/model, e.g. anthropic/claude-opus-5-5, ollama/qwen3")
                .completion(complete_models),
            Flag::choice("effort", &["default", "minimal", "low", "medium", "high", "xhigh", "max"])
                .short('e')
                .usage("Reasoning effort"),
            Flag::choice("agent", &["build", "plan"])
                .short('a')
                .usage("Agent: build (full tools) or plan (read-only)"),
            Flag::string("session")
                .short('s')
                .usage("Continue a session: an id, or `last`"),
        ])
        .run(move |ctx| {
            let prompt = (!ctx.args().is_empty()).then(|| ctx.args().join(" "));
            set(
                &gui_mode,
                Mode::Gui {
                    common: common(ctx)?,
                    prompt,
                },
            );
            Ok(())
        })
        .subcommands(vec![
            CommandBuilder::new("run")
                .short("Run one prompt headless and exit (for scripts and CI)")
                .long(
                    "Runs the agent on one prompt without opening a window. The prompt comes from the \
                     arguments, stdin, or both (piped input is appended). Text output streams the answer \
                     to stdout and tool activity to stderr; --format json writes JSON lines.\n\n\
                     Exit codes: 0 success, 1 error, 130 interrupted.",
                )
                .example("crack run \"update the changelog for this release\"")
                .example("crack run --format json --max-steps 30 \"fix lint errors\"")
                .example("crack run -a plan --tools read,grep,glob \"where is auth handled?\"")
                .flags(vec![
                    Flag::choice("format", &["text", "json"])
                        .short('f')
                        .usage("Output format")
                        .default_str("text"),
                    Flag::int("max-steps").usage("Stop after this many model requests"),
                    Flag::string("tools").usage(
                        "Comma-separated tools to allow (bash,read,write,edit,glob,grep,list,webfetch,todowrite,skill)",
                    ),
                    Flag::string("permission-mode").usage(if cfg!(feature = "yolo") {
                        "normal (default: refuse what would need approval), accept-edits, or auto (run everything except deny rules)"
                    } else {
                        "normal (default: refuse what would need approval) or accept-edits"
                    }),
                    Flag::choice("sandbox", &["on", "off"]).usage(if cfg!(feature = "force-sandbox") {
                        "This build always sandboxes shell commands"
                    } else {
                        "Run shell commands in the sandbox (default on; off for containers without user namespaces)"
                    }),
                ])
                .run(move |ctx| {
                    let format = match ctx.flag_str_or("format", "text") {
                        "json" => headless::Format::Json,
                        _ => headless::Format::Text,
                    };
                    let max_steps = ctx
                        .flag("max-steps")
                        .and_then(|s| s.parse::<u32>().ok());
                    let tools = ctx.flag("tools").map(|t| {
                        t.split(',')
                            .map(|s| s.trim().to_owned())
                            .filter(|s| !s.is_empty())
                            .collect()
                    });
                    let permission_mode = match ctx.flag("permission-mode") {
                        None => policy::Mode::Normal,
                        Some(m) => policy::Mode::parse(m).ok_or_else(|| {
                            flag_rs::Error::Validation(if cfg!(feature = "yolo") {
                                format!("unknown permission mode '{m}' (normal, accept-edits, auto)")
                            } else {
                                format!("unknown permission mode '{m}' (normal, accept-edits); this build has no auto mode")
                            })
                        })?,
                    };
                    let sandbox = match ctx.flag("sandbox").map(String::as_str) {
                        Some("off") if cfg!(feature = "force-sandbox") => {
                            return Err(flag_rs::Error::Validation(
                                "this build always sandboxes shell commands; --sandbox off isn't available".into(),
                            ));
                        }
                        Some(value) => Some(value == "on"),
                        None => None,
                    };
                    set(
                        &run_mode,
                        Mode::Run {
                            common: common(ctx)?,
                            prompt_args: ctx.args().to_vec(),
                            format,
                            max_steps,
                            tools,
                            permission_mode,
                            sandbox,
                        },
                    );
                    Ok(())
                })
                .build(),
            CommandBuilder::new("models")
                .short("List available models (optionally for one provider)")
                .run(move |ctx| {
                    set(
                        &models_mode,
                        Mode::Models {
                            common: common(ctx)?,
                            provider: ctx.args().first().cloned(),
                        },
                    );
                    Ok(())
                })
                .build(),
            CommandBuilder::new("providers")
                .short("List providers and whether they are configured")
                .run(move |ctx| {
                    set(
                        &providers_mode,
                        Mode::Providers {
                            common: common(ctx)?,
                        },
                    );
                    Ok(())
                })
                .build(),
            CommandBuilder::new("skills")
                .short("List the skills found for the working directory")
                .run(move |ctx| {
                    set(
                        &skills_mode,
                        Mode::Skills {
                            common: common(ctx)?,
                        },
                    );
                    Ok(())
                })
                .build(),
            CommandBuilder::new("mcp")
                .short("Connect the configured MCP servers and list their tools")
                .run(move |ctx| {
                    set(
                        &mcp_mode,
                        Mode::Mcp {
                            common: common(ctx)?,
                        },
                    );
                    Ok(())
                })
                .build(),
            CommandBuilder::new("sessions")
                .short("List saved sessions for this project")
                .run(move |ctx| {
                    set(
                        &sessions_mode,
                        Mode::Sessions {
                            common: common(ctx)?,
                        },
                    );
                    Ok(())
                })
                .build(),
            CommandBuilder::new("completion")
                .short("Print a shell completion script (bash, zsh, fish)")
                .example("source <(crack completion bash)")
                .run(move |ctx| {
                    let shell = match ctx.args().first().map(String::as_str) {
                        Some("bash") => Shell::Bash,
                        Some("zsh") => Shell::Zsh,
                        Some("fish") => Shell::Fish,
                        _ => {
                            return Err(flag_rs::Error::ArgumentParsing(
                                "shell required: bash, zsh or fish".into(),
                            ));
                        }
                    };
                    set(&completion_mode, Mode::Completion(shell));
                    Ok(())
                })
                .build(),
        ])
        .build()
}

fn load_workspace(common: &Common) -> anyhow::Result<(Workspace, Selection)> {
    let workspace = Workspace::load(&common.dir)
        .map_err(|e| anyhow::anyhow!("{}: {e}", common.dir.display()))?;
    let mut selection = workspace.default_selection();
    if let Some(model) = &common.model {
        selection.model = Some(model.clone());
    }
    if let Some(effort) = common.effort {
        selection.effort = effort;
    }
    if let Some(agent) = common.agent {
        selection.agent = agent;
    }
    Ok((workspace, selection))
}

fn main() {
    // The sandbox helper (crack runs itself to lock a command down) skips everything else.
    let raw: Vec<String> = std::env::args().collect();
    if raw.get(1).map(String::as_str) == Some(sandbox::HELPER) {
        sandbox::run_helper(&raw[2..]);
    }
    // Behave like other CLI tools when output is piped into `head` and the like.
    #[cfg(unix)]
    unsafe {
        libc::signal(libc::SIGPIPE, libc::SIG_DFL);
    }
    // Run as the built-in ripgrep when called as `rg` (the link shell commands see)
    // or as `crack rg ...`.
    let invoked_as = raw
        .first()
        .and_then(|a| Path::new(a).file_name())
        .and_then(|n| n.to_str());
    if invoked_as == Some("rg") {
        std::process::exit(rg::main(&raw[1..]));
    }
    if raw.get(1).map(String::as_str) == Some("rg") {
        std::process::exit(rg::main(&raw[2..]));
    }
    let mode = Arc::new(Mutex::new(None));
    let cli = build_cli(mode.clone());
    let args: Vec<String> = std::env::args().skip(1).collect();
    if let Err(error) = cli.execute(args) {
        eprintln!("Error: {error}");
        std::process::exit(2);
    }
    let Some(mode) = mode.lock().unwrap().take() else {
        // Help or completion output was printed.
        return;
    };
    match dispatch(mode, &cli) {
        Ok(code) => std::process::exit(code),
        Err(error) => {
            eprintln!("Error: {error:#}");
            std::process::exit(1);
        }
    }
}

fn dispatch(mode: Mode, cli: &flag_rs::Command) -> anyhow::Result<i32> {
    match mode {
        Mode::Completion(shell) => {
            println!("{}", cli.generate_completion(shell));
            Ok(0)
        }
        Mode::Gui { common, prompt } => {
            let (workspace, mut selection) = load_workspace(&common)?;
            // The window remembers the last effort; an explicit -e wins.
            if common.effort.is_none()
                && let Some(effort) = state::State::load().effort
            {
                selection.effort = effort;
            }
            ui::run(workspace, selection, prompt, common.session);
            Ok(0)
        }
        Mode::Run {
            common,
            prompt_args,
            format,
            max_steps,
            tools,
            permission_mode,
            sandbox,
        } => {
            if let Some(on) = sandbox {
                policy::override_sandbox(on);
            }
            let (workspace, mut selection) = load_workspace(&common)?;
            selection.mode = permission_mode;
            let prompt = headless::gather_prompt(&prompt_args)?;
            headless::run(
                workspace,
                headless::RunOptions {
                    prompt,
                    selection,
                    format,
                    max_steps,
                    tools,
                    session: common.session,
                },
            )
        }
        Mode::Models { common, provider } => {
            let (mut workspace, selection) = load_workspace(&common)?;
            let client = workspace.http.clone();
            runtime::block_on(workspace.registry.discover(&client));
            for p in workspace.registry.available() {
                if provider.as_ref().is_some_and(|id| id != &p.id) {
                    continue;
                }
                for model in &p.models {
                    let id = format!("{}/{}", p.id, model.id);
                    let marker = if selection.model.as_deref() == Some(&id) {
                        " *"
                    } else {
                        ""
                    };
                    println!("{id}{marker}");
                }
            }
            Ok(0)
        }
        Mode::Providers { common } => {
            let (mut workspace, _) = load_workspace(&common)?;
            println!("Build: {}\n", providers::registry::build_flavor());
            let client = workspace.http.clone();
            runtime::block_on(workspace.registry.discover(&client));
            for p in &workspace.registry.providers {
                let status = if p.available {
                    format!("ready ({} models)", p.models.len())
                } else if p.local {
                    "not running".to_owned()
                } else {
                    p.note.clone().unwrap_or_else(|| "unavailable".into())
                };
                println!("{:<12} {:<20} {status}", p.id, p.name);
            }
            Ok(0)
        }
        Mode::Skills { common } => {
            let (workspace, _) = load_workspace(&common)?;
            if workspace.skills.is_empty() {
                println!(
                    "No skills found. Add folders with a SKILL.md under .crack/skills, .claude/skills or ~/.config/crack/skills."
                );
            }
            for skill in workspace.skills.iter() {
                println!("{:<24} {}", skill.name, skill.description);
                println!("{:<24} {}", "", skill.path.display());
            }
            for instruction in &workspace.instructions {
                println!("instructions: {}", instruction.path.display());
            }
            Ok(0)
        }
        Mode::Mcp { common } => {
            let (workspace, _) = load_workspace(&common)?;
            if workspace.mcp.is_empty() {
                println!(
                    "No MCP servers configured. Add [mcp.<name>] to ~/.config/crack/config.toml or a .mcp.json to the project."
                );
                return Ok(0);
            }
            runtime::block_on(workspace.mcp.connect_all());
            let tools = workspace.mcp.tools();
            let mut failed = false;
            for server in workspace.mcp.snapshot() {
                let status = match &server.status {
                    mcp::Status::Connected => format!("connected, {} tools", server.tools),
                    mcp::Status::Failed(why) => {
                        failed = true;
                        format!("failed: {why}")
                    }
                    mcp::Status::Disabled => "disabled".into(),
                    mcp::Status::Connecting => "connecting".into(),
                };
                println!("{} ({}): {status}", server.name, server.transport);
                for tool in tools.iter().filter(|t| t.server == server.name) {
                    println!(
                        "  {:<40} {}",
                        tool.exposed,
                        tool.description.lines().next().unwrap_or("")
                    );
                }
            }
            Ok(if failed { 1 } else { 0 })
        }
        Mode::Sessions { common } => {
            let (workspace, _) = load_workspace(&common)?;
            for s in session::Session::list(&context::project_root(&workspace.cwd)) {
                let when = chrono::DateTime::from_timestamp(s.updated, 0)
                    .map(|d| {
                        d.with_timezone(&chrono::Local)
                            .format("%Y-%m-%d %H:%M")
                            .to_string()
                    })
                    .unwrap_or_default();
                println!("{}  {when}  {}", s.id, s.title);
            }
            Ok(0)
        }
    }
}
