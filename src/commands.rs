//! Custom slash commands in Claude Code's format: Markdown files whose body becomes the
//! prompt, with `$ARGUMENTS` (everything after the command) and `$1`, `$2`, ...
//!
//! Searched: `.crack/commands` and `.claude/commands` in the project, then
//! `~/.config/crack/commands` and `~/.claude/commands`, then enabled plugins' commands
//! as `/<plugin>:<name>` (also plain `/<name>` when no other command has that name).
//! Subfolders add `:` segments: `commands/db/migrate.md` is `/db:migrate`.

use std::path::{Path, PathBuf};

#[derive(Clone, Debug)]
pub struct Command {
    pub name: String,
    pub description: String,
    pub argument_hint: Option<String>,
    /// `allowed-tools` from the frontmatter: what its `!` snippets may run.
    pub allowed_tools: Vec<String>,
    pub path: PathBuf,
    /// For a plugin's command: `CLAUDE_PLUGIN_ROOT` and friends.
    pub env: Vec<(String, String)>,
}

fn collect(dir: &Path, prefix: &str, env: &[(String, String)], out: &mut Vec<Command>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let mut entries: Vec<_> = entries.flatten().map(|e| e.path()).collect();
    entries.sort();
    for path in entries {
        let stem = path
            .file_stem()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_default();
        if stem.starts_with('.') {
            continue;
        }
        if path.is_dir() {
            collect(&path, &format!("{prefix}{stem}:"), env, out);
        } else if path.extension().is_some_and(|e| e == "md") {
            let text = std::fs::read_to_string(&path).unwrap_or_default();
            let (front, body) = crate::skills::split_frontmatter(&text);
            let description = crate::skills::frontmatter_value(front, "description")
                .unwrap_or_else(|| {
                    body.lines()
                        .find(|l| !l.trim().is_empty())
                        .unwrap_or("")
                        .trim_start_matches('#')
                        .trim()
                        .chars()
                        .take(100)
                        .collect()
                });
            let name = format!("{prefix}{stem}");
            if !out.iter().any(|c| c.name == name) {
                out.push(Command {
                    name,
                    description,
                    argument_hint: crate::skills::frontmatter_value(front, "argument-hint"),
                    allowed_tools: crate::skills::frontmatter_value(front, "allowed-tools")
                        .map(|v| split_tools(&v))
                        .unwrap_or_default(),
                    path,
                    env: env.to_vec(),
                });
            }
        }
    }
}

pub fn discover(cwd: &Path) -> Vec<Command> {
    let mut out = Vec::new();
    let root = crate::context::project_root(cwd);
    for dir in cwd.ancestors() {
        for sub in [".crack/commands", ".claude/commands"] {
            collect(&dir.join(sub), "", &[], &mut out);
        }
        if dir == root {
            break;
        }
    }
    if let Some(global) = crate::config::global_dir() {
        collect(&global.join("commands"), "", &[], &mut out);
    }
    if let Some(home) = dirs::home_dir() {
        collect(&home.join(".claude/commands"), "", &[], &mut out);
    }
    let mut plugin_commands = Vec::new();
    for plugin in crate::plugins::enabled(Some(cwd)) {
        for path in &plugin.components.command_paths {
            let prefix = format!("{}:", plugin.name);
            if path.is_dir() {
                collect(path, &prefix, &plugin.env, &mut plugin_commands);
            } else if path.extension().is_some_and(|e| e == "md") {
                // A single file named in the manifest.
                let dir = path.parent().unwrap_or(Path::new("."));
                let mut one = Vec::new();
                collect(dir, &prefix, &plugin.env, &mut one);
                plugin_commands.extend(one.into_iter().filter(|c| &c.path == path));
            }
        }
    }
    // Plain names for plugin commands nothing else uses.
    let mut aliases = Vec::new();
    for command in &plugin_commands {
        let short = command.name.split_once(':').map_or("", |(_, s)| s);
        let unique = plugin_commands
            .iter()
            .filter(|c| c.name.split_once(':').is_some_and(|(_, s)| s == short))
            .count()
            == 1;
        if unique && !out.iter().any(|c| c.name == short) {
            let mut alias = command.clone();
            alias.name = short.to_owned();
            aliases.push(alias);
        }
    }
    out.extend(plugin_commands);
    out.extend(aliases);
    out
}

/// Runs a command body's `` !`command` `` snippets and puts their output in their
/// place, as Claude Code does. Only what the permission policy allows without asking
/// runs (the safe list, the user's allow rules and the command file's own
/// `allowed-tools`), in the sandbox when it's on; anything else is left as a note.
pub fn run_shell_snippets(
    body: &str,
    allowed_tools: &[String],
    policy: &crate::policy::Policy,
    sandbox: Option<&crate::policy::Sandbox>,
    cwd: &Path,
) -> String {
    let snippet = regex::Regex::new(r"!`([^`\n]+)`").expect("valid regex");
    if !snippet.is_match(body) {
        return body.to_owned();
    }
    let policy = policy.clone().with_session_rules(allowed_tools);
    snippet
        .replace_all(body, |caps: &regex::Captures| {
            let command = caps[1].trim();
            match policy.check("bash", &serde_json::json!({"command": command})) {
                crate::policy::Decision::Allow => run_snippet(command, sandbox, cwd),
                crate::policy::Decision::Ask(why) | crate::policy::Decision::Deny(why) => {
                    format!(
                        "[`{command}` was not run: {why}. Run it yourself if you need its output.]"
                    )
                }
            }
        })
        .into_owned()
}

fn run_snippet(command: &str, sandbox: Option<&crate::policy::Sandbox>, cwd: &Path) -> String {
    let root = crate::context::project_root(cwd);
    let argv = match sandbox.filter(|s| s.active()) {
        Some(s) => match crate::policy::sandboxed(command, s, &root) {
            Some(argv) => argv,
            None => return format!("[`{command}` was not run: the sandbox couldn't start]"),
        },
        None => crate::procs::shell_argv(command),
    };
    let output = std::process::Command::new(&argv[0])
        .args(&argv[1..])
        .current_dir(cwd)
        .stdin(std::process::Stdio::null())
        .output();
    match output {
        Ok(out) => {
            let mut text = String::from_utf8_lossy(&out.stdout).into_owned();
            text.push_str(&String::from_utf8_lossy(&out.stderr));
            crate::tools::truncate_middle(text.trim_end(), 8_000)
        }
        Err(error) => format!("[`{command}` failed to start: {error}]"),
    }
}

/// Fills in a command's body: `$ARGUMENTS`, `$1`..`$9` and plugin variables.
pub fn render(command: &Command, args: &str) -> anyhow::Result<String> {
    let text = std::fs::read_to_string(&command.path)?;
    let body = crate::skills::split_frontmatter(&text).1.trim();
    let words: Vec<&str> = args.split_whitespace().collect();
    let mut out = body.replace("$ARGUMENTS", args);
    // Highest first, so $1 doesn't eat the start of $10.
    for i in (1..=9).rev() {
        out = out.replace(&format!("${i}"), words.get(i - 1).copied().unwrap_or(""));
    }
    Ok(crate::plugins::substitute(&out, &command.env))
}

/// `Bash(git status:*), Read` → rules, splitting on commas outside parentheses.
fn split_tools(value: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut depth = 0;
    let mut current = String::new();
    for c in value.chars() {
        match c {
            '(' => depth += 1,
            ')' => depth -= 1,
            ',' if depth == 0 => {
                out.push(std::mem::take(&mut current).trim().to_owned());
                continue;
            }
            _ => {}
        }
        current.push(c);
    }
    out.push(current.trim().to_owned());
    out.retain(|t| !t.is_empty());
    out
}

/// For a prompt starting with `/name`: the command's text, for the model to follow,
/// with its `!` snippets run (see `run_shell_snippets`).
pub fn expand(
    prompt: &str,
    cwd: &Path,
    policy: &crate::policy::Policy,
    sandbox: Option<&crate::policy::Sandbox>,
) -> Option<String> {
    let rest = prompt.trim_start().strip_prefix('/')?;
    let (name, args) = rest
        .split_once(char::is_whitespace)
        .map_or((rest, ""), |(n, a)| (n, a.trim()));
    let first_line_args = args.lines().next().unwrap_or("").trim();
    let command = discover(cwd).into_iter().find(|c| c.name == name)?;
    let text = render(&command, first_line_args).ok()?;
    let text = run_shell_snippets(&text, &command.allowed_tools, policy, sandbox, cwd);
    Some(format!(
        "<command-message>The user ran the /{name} command. Its instructions:</command-message>\n{text}"
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn arguments_and_namespaces() {
        let dir = std::env::temp_dir().join(format!("crack-commands-{}", uuid::Uuid::new_v4()));
        let commands = dir.join(".crack/commands");
        std::fs::create_dir_all(commands.join("db")).unwrap();
        std::fs::write(
            commands.join("review.md"),
            "---\ndescription: Review a file\nargument-hint: <file>\n---\nReview $1 carefully. All: $ARGUMENTS",
        )
        .unwrap();
        std::fs::write(commands.join("db/migrate.md"), "Migrate to $1.").unwrap();
        std::fs::create_dir_all(dir.join(".git")).unwrap();

        let policy = crate::policy::Policy::new(
            &Default::default(),
            &dir,
            crate::policy::Mode::Normal,
            false,
        );
        let found = discover(&dir);
        let review = found.iter().find(|c| c.name == "review").unwrap();
        assert_eq!(review.description, "Review a file");
        assert_eq!(review.argument_hint.as_deref(), Some("<file>"));
        assert!(found.iter().any(|c| c.name == "db:migrate"));

        let text = expand("/review src/main.rs fast", &dir, &policy, None).unwrap();
        assert!(text.contains("Review src/main.rs carefully. All: src/main.rs fast"));
        assert!(
            expand("/db:migrate v2", &dir, &policy, None)
                .unwrap()
                .contains("Migrate to v2.")
        );
        assert!(expand("/nope", &dir, &policy, None).is_none());

        // `!` snippets: safe ones run, others are left as a note.
        std::fs::write(
            commands.join("status.md"),
            "---\nallowed-tools: Bash(printf:*)\n---\nEcho: !`echo hello there`\nPrintf: !`printf granted`\nRisky: !`rm -rf /tmp/nothing-here`",
        )
        .unwrap();
        let text = expand("/status", &dir, &policy, None).unwrap();
        assert!(text.contains("Echo: hello there"), "{text}");
        assert!(text.contains("Printf: granted"), "{text}");
        assert!(text.contains("was not run"), "{text}");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
