//! crack: a coding agent with a GPUI interface that behaves like a terminal agent.
//!
//! `crack` opens the interface on the directory it was launched from (or `--dir`).
//! `crack run "..."` does one turn headless for scripts and CI.

mod agent;
mod background;
mod catalog;
mod checkpoint;
mod code_index;
mod commands;
mod config;
mod context;
mod delegate;
mod doctor;
mod export;
mod git;
mod headless;
mod highlight;
mod hooks;
mod logging;
mod lsp;
mod mcp;
mod memory;
mod noise;
mod plugins;
mod policy;
mod procs;
mod providers;
mod recovery;
mod review;
mod rg;
mod runtime;
mod sandbox;
mod session;
mod skills;
mod state;
mod store;
mod subagents;
mod tools;
mod ui;
mod update;
mod usage;
mod websearch;
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
        max_cost: Option<f64>,
        tools: Option<Vec<String>>,
        permission_mode: policy::Mode,
        sandbox: Option<bool>,
        /// Also log to stderr at this level (`--log debug`).
        log: Option<String>,
        /// Image files to attach to the prompt.
        images: Vec<std::path::PathBuf>,
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
        args: Vec<String>,
    },
    Hooks {
        common: Common,
        trust: bool,
    },
    Extensions {
        common: Common,
        args: Vec<String>,
    },
    Plugin {
        args: Vec<String>,
    },
    Export {
        common: Common,
        args: Vec<String>,
    },
    Update {
        check: bool,
    },
    Usage {
        common: Common,
        days: i64,
        all: bool,
    },
    Doctor {
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
    let hooks_mode = mode.clone();
    let extensions_mode = mode.clone();
    let plugin_mode = mode.clone();
    let export_mode = mode.clone();
    let update_mode = mode.clone();
    let usage_mode = mode.clone();
    let doctor_mode = mode.clone();
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
                    Flag::string("image").usage("Attach image files to the prompt (comma-separated: a.png,b.jpg)"),
                    Flag::string("log").usage("Also print crack's log to stderr at this level: error, warn, info, debug or trace"),
                    Flag::string("max-cost").usage("Stop the run once it has cost this many dollars (e.g. 0.50)"),
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
                            max_cost: ctx
                                .flag("max-cost")
                                .map(|v| {
                                    v.parse::<f64>().map_err(|_| {
                                        flag_rs::Error::Validation(format!("--max-cost needs a number of dollars, got '{v}'"))
                                    })
                                })
                                .transpose()?,
                            tools,
                            permission_mode,
                            sandbox,
                            log: ctx.flag("log").cloned(),
                            images: ctx
                                .flag("image")
                                .map(|v| v.split(',').filter(|s| !s.trim().is_empty()).map(|s| s.trim().into()).collect())
                                .unwrap_or_default(),
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
                .short("List MCP servers and what they offer; `crack mcp login|logout <server>` for OAuth")
                .run(move |ctx| {
                    set(
                        &mcp_mode,
                        Mode::Mcp {
                            common: common(ctx)?,
                            args: ctx.args().to_vec(),
                        },
                    );
                    Ok(())
                })
                .build(),
            CommandBuilder::new("update")
                .short("Update crack from GitHub Releases (signed); `crack update check` only looks")
                .run(move |ctx| {
                    let check = matches!(ctx.args().first().map(String::as_str), Some("check" | "--check"));
                    set(&update_mode, Mode::Update { check });
                    Ok(())
                })
                .build(),
            CommandBuilder::new("export")
                .short("Export a session to share: `crack export [last|<id>] [html] [<file>]` (Markdown to stdout by default)")
                .run(move |ctx| {
                    set(
                        &export_mode,
                        Mode::Export {
                            common: common(ctx)?,
                            args: ctx.args().to_vec(),
                        },
                    );
                    Ok(())
                })
                .build(),
            CommandBuilder::new("plugin")
                .short("Claude Code plugins: `crack plugin marketplace add <repo>`, `install <name>`, `list`")
                .run(move |ctx| {
                    set(
                        &plugin_mode,
                        Mode::Plugin {
                            args: ctx.args().to_vec(),
                        },
                    );
                    Ok(())
                })
                .build(),
            CommandBuilder::new("extensions")
                .short("Popular skill packs and MCP servers: `crack extensions install|update|remove <id>`")
                .run(move |ctx| {
                    set(
                        &extensions_mode,
                        Mode::Extensions {
                            common: common(ctx)?,
                            args: ctx.args().to_vec(),
                        },
                    );
                    Ok(())
                })
                .build(),
            CommandBuilder::new("doctor")
                .short("Check providers, the sandbox, tools, language servers and MCP servers")
                .run(move |ctx| {
                    set(&doctor_mode, Mode::Doctor { common: common(ctx)? });
                    Ok(())
                })
                .build(),
            CommandBuilder::new("usage")
                .short("Show tokens and cost by model and day (`crack usage 7`, `crack usage all`)")
                .run(move |ctx| {
                    let mut days = 30;
                    let mut all = false;
                    for arg in ctx.args() {
                        match arg.as_str() {
                            "all" => all = true,
                            n => {
                                days = n.parse().map_err(|_| {
                                    flag_rs::Error::Validation(format!("expected a number of days or `all`, got '{n}'"))
                                })?
                            }
                        }
                    }
                    set(
                        &usage_mode,
                        Mode::Usage {
                            common: common(ctx)?,
                            days,
                            all,
                        },
                    );
                    Ok(())
                })
                .build(),
            CommandBuilder::new("hooks")
                .short("List the configured hooks; `crack hooks trust` allows this project's")
                .run(move |ctx| {
                    let trust = match ctx.args().first().map(String::as_str) {
                        None => false,
                        Some("trust") => true,
                        Some(other) => {
                            return Err(flag_rs::Error::Validation(format!(
                                "unknown hooks command '{other}' (try `crack hooks trust`)"
                            )));
                        }
                    };
                    set(
                        &hooks_mode,
                        Mode::Hooks {
                            common: common(ctx)?,
                            trust,
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
    let stderr_log = match &mode {
        Mode::Run { log, .. } => log.clone(),
        _ => None,
    };
    let guard = logging::init(stderr_log.as_deref());
    tracing::info!(version = env!("CARGO_PKG_VERSION"), "crack starting");
    if matches!(mode, Mode::Gui { .. } | Mode::Run { .. }) {
        crate::recovery::reap_orphans();
    }
    let result = dispatch(mode, &cli);
    // Background shell commands end with crack (headless runs and the window alike).
    background::kill_all();
    // Exiting skips destructors: flush the log first.
    drop(guard);
    match result {
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
            max_cost,
            tools,
            permission_mode,
            sandbox,
            log: _,
            images,
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
                    max_cost,
                    tools,
                    session: common.session,
                    images,
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
        Mode::Mcp { common, args } => {
            let (workspace, _) = load_workspace(&common)?;
            match (args.first().map(String::as_str), args.get(1)) {
                (Some("login"), Some(name)) => {
                    let hub = workspace.mcp.clone();
                    runtime::block_on(hub.sign_in(name, |url| {
                        println!("Open this link to sign in to {name}:\n\n  {url}\n");
                        let _ = std::process::Command::new("xdg-open")
                            .arg(&url)
                            .stdout(std::process::Stdio::null())
                            .stderr(std::process::Stdio::null())
                            .spawn();
                    }))?;
                    let status = hub
                        .snapshot()
                        .into_iter()
                        .find(|s| &s.name == name)
                        .map(|s| s.status);
                    println!("Signed in. {name}: {status:?}");
                    return Ok(0);
                }
                (Some("logout"), Some(name)) => {
                    workspace.mcp.sign_out(name)?;
                    println!("Forgot the sign-in for {name}.");
                    return Ok(0);
                }
                (Some(other), _) => anyhow::bail!(
                    "unknown: crack mcp {other} (try `crack mcp`, `crack mcp login <server>` or `crack mcp logout <server>`)"
                ),
                (None, _) => {}
            }
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
                    mcp::Status::Connected => format!(
                        "connected, {} tools, {} resources, {} prompts",
                        server.tools, server.resources, server.prompts
                    ),
                    mcp::Status::NeedsAuth => {
                        failed = true;
                        format!("needs sign-in: crack mcp login {}", server.name)
                    }
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
                for resource in workspace.mcp.resources() {
                    if resource.server == server.name {
                        let reference = format!("@{}:{}", server.name, resource.uri);
                        println!("  {reference:<40} {}", resource.name);
                    }
                }
                for prompt in workspace.mcp.prompts() {
                    if prompt.server == server.name {
                        let command = format!("/{}", prompt.command());
                        println!(
                            "  {command:<40} {}",
                            prompt.description.lines().next().unwrap_or("")
                        );
                    }
                }
            }
            Ok(if failed { 1 } else { 0 })
        }
        Mode::Doctor { common } => {
            let (mut workspace, selection) = load_workspace(&common)?;
            Ok(doctor::run(&mut workspace, &selection))
        }
        Mode::Usage { common, days, all } => {
            let (workspace, _) = load_workspace(&common)?;
            let project = memory::project_key(&workspace.cwd);
            print!(
                "{}",
                usage::report(days, (!all).then_some(project.as_str()))
            );
            Ok(0)
        }
        Mode::Plugin { args } => Ok(plugin_command(&args)),
        Mode::Update { check } => {
            if !cfg!(feature = "cloud") {
                eprintln!("This air-gapped build doesn't update itself.");
                return Ok(1);
            }
            let http = workspace::http_client()?;
            match runtime::block_on(update::run(&http, check)) {
                Ok(message) => {
                    println!("{message}");
                    Ok(0)
                }
                Err(error) => {
                    eprintln!("{error:#}");
                    Ok(1)
                }
            }
        }
        Mode::Export { common, args } => {
            let (workspace, _) = load_workspace(&common)?;
            let mut id = "last".to_owned();
            let mut html = false;
            let mut file = None;
            for arg in args {
                match arg.as_str() {
                    "html" | "--html" => html = true,
                    "md" | "markdown" => html = false,
                    a if a.contains('.') || a.contains('/') => file = Some(a.to_owned()),
                    a => id = a.to_owned(),
                }
            }
            let session = if id == "last" {
                let root = crate::context::project_root(&workspace.cwd);
                let summary = session::Session::list(&root)
                    .into_iter()
                    .next()
                    .ok_or_else(|| anyhow::anyhow!("no sessions in this project yet"))?;
                session::Session::load(&summary.id)?
            } else {
                session::Session::load(&id)?
            };
            let (text, redacted) = if html {
                export::html(&session)
            } else {
                export::markdown(&session)
            };
            match file {
                Some(path) => {
                    std::fs::write(&path, text)?;
                    eprintln!(
                        "Wrote {path}{}",
                        if redacted > 0 {
                            format!(" ({redacted} secret-looking item(s) redacted)")
                        } else {
                            String::new()
                        }
                    );
                }
                None => print!("{text}"),
            }
            Ok(0)
        }
        Mode::Extensions { common, args } => {
            let (workspace, _) = load_workspace(&common)?;
            match (args.first().map(String::as_str), args.get(1)) {
                (None, _) => {
                    print!("{}", catalog::report(&workspace.config));
                    Ok(0)
                }
                (Some(action @ ("install" | "update" | "remove")), Some(id)) => {
                    let Some(entry) = catalog::find(id) else {
                        eprintln!("No extension '{id}'; `crack extensions` lists them.");
                        return Ok(1);
                    };
                    if action != "remove" && matches!(entry.source, catalog::Source::Plugin { .. })
                    {
                        let spec = match catalog::plugin_spec(entry) {
                            Ok(spec) => spec,
                            Err(error) => {
                                eprintln!("Couldn't add {}: {error:#}", entry.name);
                                return Ok(1);
                            }
                        };
                        let code = plugin_command(&["install".to_owned(), spec]);
                        if code == 0 {
                            catalog::retire_skill_pack(entry.id);
                        }
                        return Ok(code);
                    }
                    let result = if action == "remove" {
                        catalog::remove(entry)
                    } else {
                        catalog::install(entry)
                    };
                    match result {
                        Ok(message) => {
                            println!("{message}");
                            Ok(0)
                        }
                        Err(error) => {
                            eprintln!("Couldn't {action} {}: {error:#}", entry.name);
                            Ok(1)
                        }
                    }
                }
                _ => {
                    eprintln!("Usage: crack extensions [install|update|remove <id>]");
                    Ok(2)
                }
            }
        }
        Mode::Hooks { common, trust } => {
            let (workspace, _) = load_workspace(&common)?;
            let config = &workspace.config.hooks;
            if config.values().all(Vec::is_empty) {
                println!("No hooks configured. Add them as [[hooks.EVENT]] in crack's config.");
                return Ok(0);
            }
            if trust {
                hooks::trust(config, &workspace.cwd);
                store::get().flush();
                println!("Trusted this project's hooks (until they change).");
                return Ok(0);
            }
            let hooks = hooks::Hooks::new(config, &workspace.cwd, "");
            for (event, matchers) in config {
                for m in matchers {
                    let source = match (m.from_project, hooks.project_trusted) {
                        (false, _) => "user",
                        (true, true) => "project, trusted",
                        (true, false) => "project, NOT trusted",
                    };
                    let commands: Vec<&str> = m
                        .command
                        .iter()
                        .map(String::as_str)
                        .chain(m.hooks.iter().map(|h| h.command.as_str()))
                        .collect();
                    let matcher = m.matcher.as_deref().unwrap_or("*");
                    for command in commands {
                        println!("{event:<17} {matcher:<14} {command}   ({source})");
                    }
                }
            }
            if !hooks.untrusted().is_empty() {
                println!(
                    "\nThis project's hooks won't run until you trust them: crack hooks trust"
                );
            }
            Ok(0)
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

/// `crack plugin ...`. Returns the exit code.
fn plugin_command(args: &[String]) -> i32 {
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    let yes = args.contains(&"--yes") || args.contains(&"-y");
    let args: Vec<&str> = args
        .into_iter()
        .filter(|a| *a != "--yes" && *a != "-y")
        .collect();
    let result: anyhow::Result<String> = match args.as_slice() {
        [] | ["list"] => Ok(plugins::report()),
        ["marketplace", "add", source] => {
            plugins::add_marketplace(source).map(|name| format!("Added the {name} marketplace."))
        }
        ["marketplace", "update", name] => {
            plugins::update_marketplace(name).map(|_| format!("Updated {name}."))
        }
        ["marketplace", "remove", name] => {
            plugins::remove_marketplace(name).map(|_| format!("Removed {name}."))
        }
        ["install" | "update", spec] => (|| {
            let pending = plugins::prepare(spec)?;
            let mut approved = yes;
            if pending.review.runs_code() && !yes {
                println!(
                    "{} {} runs these on your machine:\n{}",
                    pending.key,
                    pending.version,
                    pending.review.describe()
                );
                if std::io::IsTerminal::is_terminal(&std::io::stdin()) {
                    print!("Allow them? [y/N] ");
                    let _ = std::io::Write::flush(&mut std::io::stdout());
                    let mut answer = String::new();
                    let _ = std::io::stdin().read_line(&mut answer);
                    approved = matches!(answer.trim(), "y" | "Y" | "yes");
                }
            }
            let runs_code = pending.review.runs_code();
            let key = pending.key.clone();
            let mut message = plugins::commit_install(pending, approved)?;
            if runs_code && !approved {
                message.push_str(&format!(
                    " Its hooks and MCP servers stay off until you run `crack plugin trust {key}`."
                ));
            }
            Ok(message)
        })(),
        ["remove" | "uninstall", spec] => plugins::uninstall(spec),
        ["enable", spec] => plugins::set_enabled(spec, true),
        ["disable", spec] => plugins::set_enabled(spec, false),
        ["trust", spec] => plugins::trust(spec).map(|review| {
            if review.runs_code() {
                format!("Approved these for {spec}:\n{}", review.describe())
            } else {
                format!("{spec} runs no hooks or MCP servers.")
            }
        }),
        _ => Err(anyhow::anyhow!(
            "usage: crack plugin [list | marketplace add|update|remove <source> | install|update|remove|enable|disable|trust <name>[@marketplace]] [--yes]"
        )),
    };
    match result {
        Ok(message) => {
            println!("{}", message.trim_end());
            0
        }
        Err(error) => {
            eprintln!("{error:#}");
            1
        }
    }
}
