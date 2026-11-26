//! `crack doctor`: checks everything crack depends on and says what to fix.

use std::time::Duration;

use crate::workspace::{Selection, Workspace};

struct Report {
    problems: usize,
    warnings: usize,
}

impl Report {
    fn ok(&self, what: &str, detail: impl AsRef<str>) {
        println!("  \u{2713} {what}: {}", detail.as_ref());
    }
    fn warn(&mut self, what: &str, detail: impl AsRef<str>) {
        self.warnings += 1;
        println!("  ! {what}: {}", detail.as_ref());
    }
    fn fail(&mut self, what: &str, detail: impl AsRef<str>) {
        self.problems += 1;
        println!("  \u{2717} {what}: {}", detail.as_ref());
    }
}

/// Runs the checks; returns the exit code (1 if anything is broken).
pub fn run(workspace: &mut Workspace, selection: &Selection) -> i32 {
    let mut r = Report {
        problems: 0,
        warnings: 0,
    };
    println!(
        "crack {} ({} build)",
        env!("CARGO_PKG_VERSION"),
        crate::providers::registry::build_flavor()
    );
    println!(
        "Project: {}",
        crate::context::project_root(&workspace.cwd).display()
    );

    println!("\nFiles");
    let data = crate::config::data_dir();
    match std::fs::create_dir_all(&data).and_then(|_| tempfile_in(&data)) {
        Ok(()) => r.ok("data folder", data.display().to_string()),
        Err(error) => r.fail(
            "data folder",
            format!("{} isn't writable: {error}", data.display()),
        ),
    }
    r.ok("log folder", crate::logging::dir().display().to_string());
    match crate::config::global_dir() {
        Some(dir) => r.ok("config folder", dir.display().to_string()),
        None => r.warn("config folder", "no home folder found"),
    }

    println!("\nModels");
    let client = workspace.http.clone();
    crate::runtime::block_on(workspace.registry.discover(&client));
    let available: Vec<&str> = workspace
        .registry
        .providers
        .iter()
        .filter(|p| p.available)
        .map(|p| p.id.as_str())
        .collect();
    if available.is_empty() {
        r.fail("providers", "none configured: set an API key (e.g. ANTHROPIC_API_KEY) or run crack and press ctrl-x p");
    } else {
        r.ok("providers", available.join(", "));
    }
    let model = selection
        .model
        .clone()
        .or_else(|| workspace.registry.default_model());
    match &model {
        Some(model) => {
            let (provider, name) = model.split_once('/').unwrap_or(("", model));
            let config = crate::providers::registry::provider_config(&workspace.config, provider);
            let test = crate::runtime::block_on(async {
                tokio::time::timeout(
                    Duration::from_secs(20),
                    crate::providers::registry::test_connection(
                        provider,
                        &config,
                        Some(name),
                        &client,
                    ),
                )
                .await
            });
            match test {
                Ok(Ok(message)) => r.ok("default model", format!("{model}: {message}")),
                Ok(Err(error)) => r.fail("default model", format!("{model}: {error:#}")),
                Err(_) => r.fail("default model", format!("{model}: no answer within 20s")),
            }
            let caps = workspace.registry.caps(model);
            match (caps.context, caps.input_price) {
                (Some(window), Some(_)) => r.ok("model limits", format!("{} token window, priced", crate::ui::render::thousands(window))),
                (Some(window), None) => r.ok("model limits", format!("{} token window; no price, so costs aren't tracked", crate::ui::render::thousands(window))),
                (None, _) => r.warn("model limits", "unknown context window, so crack can't compact in time; set [models.\"provider/model\"] context = N"),
            }
        }
        None => r.fail("default model", "none: pick one with -m provider/model"),
    }

    for model in crate::usage::get().never_cached() {
        r.warn(
            "prompt cache",
            format!("{model} never read from the cache this week; its provider may not be caching, so every request pays full price"),
        );
    }

    println!("\nSafety");
    let sandbox = crate::policy::Sandbox::resolve(&workspace.config.permissions);
    if !sandbox.wanted {
        r.warn(
            "sandbox",
            "off: shell commands run with your full permissions",
        );
    } else if !sandbox.available {
        let missing = if cfg!(windows) {
            "commands couldn't run in an AppContainer here"
        } else {
            "neither Landlock (Linux 5.13+) nor bubblewrap is available"
        };
        r.fail(
            "sandbox",
            format!("on, but {missing}, so shell commands will ask first"),
        );
    } else {
        let root = crate::context::project_root(&workspace.cwd);
        let works = crate::policy::sandboxed("true", &sandbox, &root).is_some_and(|argv| {
            std::process::Command::new(&argv[0])
                .args(&argv[1..])
                .current_dir(&root)
                .status()
                .is_ok_and(|s| s.success())
        });
        if works {
            r.ok(
                "sandbox",
                format!(
                    "{} \u{b7} network {}",
                    sandbox.backend(),
                    if sandbox.network {
                        "allowed"
                    } else {
                        "blocked"
                    }
                ),
            );
        } else {
            r.fail(
                "sandbox",
                format!(
                    "{} couldn't run a command; turn it off in ctrl-p or check user namespaces",
                    sandbox.backend()
                ),
            );
        }
    }
    let hooks = crate::hooks::Hooks::new(&workspace.config.hooks, &workspace.cwd, "");
    let untrusted = hooks.untrusted();
    if !untrusted.is_empty() {
        r.warn(
            "hooks",
            format!(
                "{} project hook(s) not trusted yet: crack hooks trust",
                untrusted.len()
            ),
        );
    }

    println!("\nTools");
    match crate::policy::which("git") {
        Some(path) => r.ok("git", path.display().to_string()),
        None => r.warn(
            "git",
            "not found: undo snapshots, git tools and worktrees need it",
        ),
    }
    match crate::policy::which("rg") {
        Some(path) => r.ok("ripgrep", path.display().to_string()),
        None => r.ok(
            "ripgrep",
            "built in (rg isn't installed; crack provides one)",
        ),
    }
    match font() {
        Some(font) => r.ok("font", font),
        None => r.warn(
            "font",
            "no preferred monospace font found; install JetBrains Mono or set CRACK_FONT",
        ),
    }

    let root = crate::context::project_root(&workspace.cwd);
    let servers = crate::lsp::relevant(&root);
    if !servers.is_empty() {
        println!("\nLanguage servers");
        for spec in servers {
            match crate::policy::which(spec.command[0]) {
                Some(path) => r.ok(spec.name, path.display().to_string()),
                None => r.warn(
                    spec.name,
                    match spec.install {
                        Some(command) => format!("not installed: {command}"),
                        None => "not installed".into(),
                    },
                ),
            }
        }
    }

    if !workspace.mcp.is_empty() {
        println!("\nMCP servers");
        crate::runtime::block_on(workspace.mcp.connect_all());
        for server in workspace.mcp.snapshot() {
            match &server.status {
                crate::mcp::Status::Connected => {
                    r.ok(&server.name, format!("{} tools", server.tools))
                }
                crate::mcp::Status::NeedsAuth => r.warn(
                    &server.name,
                    format!("needs sign-in: crack mcp login {}", server.name),
                ),
                crate::mcp::Status::Failed(why) => r.fail(&server.name, why),
                other => r.warn(&server.name, format!("{other:?}")),
            }
        }
    }

    println!();
    match (r.problems, r.warnings) {
        (0, 0) => println!("Everything looks good."),
        (0, w) => println!("No problems; {w} thing(s) to look at."),
        (p, w) => println!("{p} problem(s), {w} warning(s)."),
    }
    i32::from(r.problems > 0)
}

fn tempfile_in(dir: &std::path::Path) -> std::io::Result<()> {
    let path = dir.join(format!(".doctor-{}", std::process::id()));
    std::fs::write(&path, b"ok")?;
    std::fs::remove_file(path)
}

/// The monospace font the window will use, via fontconfig.
fn font() -> Option<String> {
    let output = std::process::Command::new("fc-list")
        .args([":spacing=mono", "family"])
        .output()
        .ok()?;
    let installed = String::from_utf8_lossy(&output.stdout).to_string();
    let preferred = std::env::var("CRACK_FONT").ok();
    preferred
        .into_iter()
        .chain(
            [
                "JetBrains Mono",
                "Berkeley Mono",
                "Iosevka",
                "Fira Code",
                "Fira Mono",
                "Cascadia Code",
                "Source Code Pro",
                "Hack",
                "DejaVu Sans Mono",
                "Liberation Mono",
            ]
            .map(String::from),
        )
        .find(|f| {
            installed
                .lines()
                .any(|l| l.split(',').any(|name| name.trim() == f))
        })
}
