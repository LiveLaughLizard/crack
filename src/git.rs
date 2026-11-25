//! Git tools: status, diff, log and show with structured output, plus a commit tool
//! that always asks first. They run in the sandbox like shell commands, with the
//! repository's own command-running settings (fsmonitor, hooks, external diff) off.

use std::time::Duration;

use serde_json::Value;

use crate::tools::{DisplayKind, ToolContext, ToolMeta, ToolOutput};

const MAX_OUTPUT: usize = 30_000;

/// Settings that would let a repository run commands when crack reads it.
const SAFE: &[&str] = &[
    "-c",
    "core.fsmonitor=false",
    "-c",
    "core.hooksPath=/dev/null",
    "-c",
    "core.pager=cat",
    "-c",
    "color.ui=never",
    "-c",
    "diff.external=",
    "-c",
    "core.quotepath=false",
];

fn quote(arg: &str) -> String {
    if !arg.is_empty()
        && arg
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "-_./=:@^~+,".contains(c))
    {
        arg.to_owned()
    } else {
        format!("'{}'", arg.replace('\'', r"'\''"))
    }
}

/// A ref or path from the model: never an option.
fn checked<'a>(value: Option<&'a str>, what: &str) -> anyhow::Result<Option<&'a str>> {
    match value.map(str::trim).filter(|v| !v.is_empty()) {
        Some(v) if v.starts_with('-') => anyhow::bail!("{what} can't start with '-'"),
        other => Ok(other),
    }
}

/// Runs git with `args` in the working directory (sandboxed when the sandbox is on).
pub async fn run(ctx: &ToolContext, args: &[&str]) -> anyhow::Result<String> {
    run_in(&ctx.cwd, ctx.sandbox.as_ref(), args).await
}

pub async fn run_in(
    cwd: &std::path::Path,
    sandbox: Option<&crate::policy::Sandbox>,
    args: &[&str],
) -> anyhow::Result<String> {
    let mut words: Vec<String> = vec!["git".into(), "--no-pager".into()];
    words.extend(SAFE.iter().map(|s| (*s).to_owned()));
    words.extend(args.iter().map(|a| (*a).to_owned()));
    let command = words.iter().map(|w| quote(w)).collect::<Vec<_>>().join(" ");
    let root = crate::context::project_root(cwd);
    let argv = match sandbox {
        Some(sandbox) if sandbox.active() => crate::policy::sandboxed(&command, sandbox, &root)
            .ok_or_else(|| anyhow::anyhow!("the sandbox isn't available"))?,
        _ => vec!["bash".into(), "-c".into(), command],
    };
    let output = tokio::time::timeout(
        Duration::from_secs(30),
        tokio::process::Command::new(&argv[0])
            .args(&argv[1..])
            .current_dir(cwd)
            .env("GIT_TERMINAL_PROMPT", "0")
            .env("GIT_OPTIONAL_LOCKS", "0")
            .stdin(std::process::Stdio::null())
            .kill_on_drop(true)
            .output(),
    )
    .await
    .map_err(|_| anyhow::anyhow!("git timed out"))??;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!("{}", stderr.trim());
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

fn truncate(mut text: String) -> String {
    if text.len() > MAX_OUTPUT {
        let mut cut = MAX_OUTPUT;
        while !text.is_char_boundary(cut) {
            cut -= 1;
        }
        text.truncate(cut);
        text.push_str("\n… (truncated; narrow it with `path`)");
    }
    text
}

/// The branch and changed files, parsed from `git status --porcelain=v2 --branch`.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Status {
    pub branch: String,
    pub upstream: Option<String>,
    pub ahead: u32,
    pub behind: u32,
    pub staged: Vec<(char, String)>,
    pub unstaged: Vec<(char, String)>,
    pub untracked: Vec<String>,
    pub conflicted: Vec<String>,
}

impl Status {
    pub fn parse(text: &str) -> Status {
        let mut status = Status::default();
        for line in text.lines() {
            let mut fields = line.splitn(2, ' ');
            let (kind, rest) = (fields.next().unwrap_or(""), fields.next().unwrap_or(""));
            match kind {
                "#" => {
                    let (key, value) = rest.split_once(' ').unwrap_or((rest, ""));
                    match key {
                        "branch.head" => status.branch = value.to_owned(),
                        "branch.upstream" => status.upstream = Some(value.to_owned()),
                        "branch.ab" => {
                            for part in value.split_whitespace() {
                                if let Some(n) = part.strip_prefix('+') {
                                    status.ahead = n.parse().unwrap_or(0);
                                } else if let Some(n) = part.strip_prefix('-') {
                                    status.behind = n.parse().unwrap_or(0);
                                }
                            }
                        }
                        _ => {}
                    }
                }
                "1" | "2" => {
                    // XY sub mH mI mW hH hI path (type 2 adds a score and the old path)
                    let parts: Vec<&str> =
                        rest.splitn(if kind == "1" { 8 } else { 9 }, ' ').collect();
                    let xy: Vec<char> = parts.first().unwrap_or(&"..").chars().collect();
                    let path = parts
                        .last()
                        .unwrap_or(&"")
                        .split('\t')
                        .next()
                        .unwrap_or("")
                        .to_owned();
                    if xy.first().is_some_and(|c| *c != '.') {
                        status.staged.push((xy[0], path.clone()));
                    }
                    if xy.get(1).is_some_and(|c| *c != '.') {
                        status.unstaged.push((xy[1], path));
                    }
                }
                "u" => status
                    .conflicted
                    .push(rest.rsplit(' ').next().unwrap_or("").to_owned()),
                "?" => status.untracked.push(rest.to_owned()),
                _ => {}
            }
        }
        status
    }

    pub fn changed(&self) -> usize {
        let mut paths: Vec<&str> = self
            .staged
            .iter()
            .chain(&self.unstaged)
            .map(|(_, p)| p.as_str())
            .chain(self.untracked.iter().map(String::as_str))
            .chain(self.conflicted.iter().map(String::as_str))
            .collect();
        paths.sort();
        paths.dedup();
        paths.len()
    }

    fn render(&self) -> String {
        let mut out = format!("branch: {}", self.branch);
        match &self.upstream {
            Some(upstream) => out.push_str(&format!(
                " (tracking {upstream}, ahead {}, behind {})",
                self.ahead, self.behind
            )),
            None => out.push_str(" (no upstream)"),
        }
        let section = |out: &mut String, title: &str, items: Vec<String>| {
            if !items.is_empty() {
                out.push_str(&format!("\n{title}:"));
                for item in items {
                    out.push_str(&format!("\n  {item}"));
                }
            }
        };
        let fmt = |list: &[(char, String)]| list.iter().map(|(c, p)| format!("{c} {p}")).collect();
        section(&mut out, "staged", fmt(&self.staged));
        section(&mut out, "unstaged", fmt(&self.unstaged));
        section(&mut out, "untracked", self.untracked.clone());
        section(&mut out, "conflicts", self.conflicted.clone());
        if self.changed() == 0 {
            out.push_str("\nworking tree clean");
        }
        out.trim_end().to_owned()
    }
}

pub async fn status(ctx: &ToolContext) -> anyhow::Result<Status> {
    Ok(Status::parse(
        &run(
            ctx,
            &[
                "status",
                "--porcelain=v2",
                "--branch",
                "--untracked-files=all",
            ],
        )
        .await?,
    ))
}

fn str_arg<'a>(input: &'a Value, key: &str) -> Option<&'a str> {
    input.get(key).and_then(Value::as_str)
}

pub async fn tool(name: &str, input: &Value, ctx: &ToolContext) -> anyhow::Result<ToolOutput> {
    let path = checked(str_arg(input, "path"), "path")?;
    let reference = checked(str_arg(input, "ref"), "ref")?;
    let (content, title, kind) = match name {
        "git_status" => {
            let status = status(ctx).await?;
            (
                status.render(),
                format!("git status ({} changed)", status.changed()),
                DisplayKind::Inline,
            )
        }
        "git_diff" => {
            let mut args = vec!["diff", "--no-ext-diff", "--no-textconv", "--no-renames"];
            if input
                .get("staged")
                .and_then(Value::as_bool)
                .unwrap_or(false)
            {
                args.push("--cached");
            }
            if input.get("stat").and_then(Value::as_bool).unwrap_or(false) {
                args.push("--stat");
            }
            args.extend(reference);
            args.push("--");
            args.extend(path);
            let diff = run(ctx, &args).await?;
            let diff = if diff.trim().is_empty() {
                "No differences.".to_owned()
            } else {
                truncate(diff)
            };
            let what = match (reference, path) {
                (Some(r), Some(p)) => format!("git diff {r} -- {p}"),
                (Some(r), None) => format!("git diff {r}"),
                (None, Some(p)) => format!("git diff {p}"),
                (None, None) => "git diff".into(),
            };
            (diff, what, DisplayKind::Diff)
        }
        "git_log" => {
            let limit = input
                .get("limit")
                .and_then(Value::as_u64)
                .unwrap_or(20)
                .clamp(1, 200)
                .to_string();
            let mut args = vec![
                "log",
                "-n",
                &limit,
                "--date=short",
                "--format=%h %ad %an%d%n    %s",
            ];
            args.extend(reference);
            args.push("--");
            args.extend(path);
            let log = run(ctx, &args).await?;
            let log = if log.trim().is_empty() {
                "No commits.".to_owned()
            } else {
                truncate(log)
            };
            (
                log,
                format!(
                    "git log{}",
                    path.map(|p| format!(" {p}")).unwrap_or_default()
                ),
                DisplayKind::Inline,
            )
        }
        "git_show" => {
            let reference = reference.unwrap_or("HEAD");
            let mut args = vec![
                "show",
                "--no-ext-diff",
                "--no-textconv",
                "--stat",
                "--patch",
                "--format=commit %H%nAuthor: %an <%ae>%nDate:   %ad%n%n%B",
                reference,
                "--",
            ];
            args.extend(path);
            (
                truncate(run(ctx, &args).await?),
                format!("git show {reference}"),
                DisplayKind::Diff,
            )
        }
        "git_commit" => {
            let message = str_arg(input, "message")
                .map(str::trim)
                .filter(|m| !m.is_empty())
                .ok_or_else(|| anyhow::anyhow!("missing message"))?;
            let mut args = vec!["commit", "--no-verify", "--cleanup=strip", "-m", message];
            if input.get("all").and_then(Value::as_bool).unwrap_or(false) {
                args.push("--all");
            }
            let out = run(ctx, &args).await?;
            (
                out.trim().to_owned(),
                format!("git commit: {}", message.lines().next().unwrap_or("")),
                DisplayKind::Inline,
            )
        }
        other => anyhow::bail!("unknown git tool {other}"),
    };
    Ok(ToolOutput::ok(
        content,
        ToolMeta {
            title,
            kind,
            ..Default::default()
        },
    ))
}

/// The git tools' definitions, offered when the working directory is in a repository.
pub fn specs() -> Vec<crate::providers::ToolSpec> {
    use serde_json::json;
    let spec = |name: &str, description: &str, schema: Value| crate::providers::ToolSpec {
        name: name.into(),
        description: description.into(),
        schema,
    };
    vec![
        spec(
            "git_status",
            "Show the git branch, its upstream with ahead/behind counts, and staged, unstaged, untracked and conflicted files.",
            json!({"type": "object", "properties": {}}),
        ),
        spec(
            "git_diff",
            "Show a git diff: unstaged changes by default, staged with `staged`, or against a ref (e.g. `main`, `HEAD~3`). \
             Narrow it to a file or folder with `path`; `stat` gives a per-file summary instead.",
            json!({"type": "object", "properties": {
                "staged": {"type": "boolean", "description": "Diff what's staged for commit."},
                "ref": {"type": "string", "description": "Compare the working tree (or the index, with staged) to this ref."},
                "path": {"type": "string", "description": "Limit to this file or folder."},
                "stat": {"type": "boolean", "description": "Summary of changed files and line counts only."}
            }}),
        ),
        spec(
            "git_log",
            "Show recent commits (hash, date, author, refs, subject), optionally for one path or from a ref.",
            json!({"type": "object", "properties": {
                "path": {"type": "string", "description": "Only commits touching this file or folder."},
                "ref": {"type": "string", "description": "Start from this ref instead of HEAD; ranges like main..HEAD work."},
                "limit": {"type": "integer", "description": "How many commits (default 20, max 200)."}
            }}),
        ),
        spec(
            "git_show",
            "Show one commit: its message, files changed and patch. Narrow the patch with `path`.",
            json!({"type": "object", "properties": {
                "ref": {"type": "string", "description": "The commit (default HEAD)."},
                "path": {"type": "string", "description": "Only this file or folder's changes."}
            }}),
        ),
        spec(
            "git_commit",
            "Commit to git with this message. The user always approves the commit first. Commits what's staged; \
             with `all`, also every modified tracked file (new files must be staged with `git add` first).",
            json!({"type": "object", "properties": {
                "message": {"type": "string", "description": "The full commit message: a short subject line, a blank line, then the body if needed."},
                "all": {"type": "boolean", "description": "Also commit all modified tracked files (git commit --all)."}
            }, "required": ["message"]}),
        ),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_porcelain_v2() {
        let text = "# branch.oid abc\n# branch.head main\n# branch.upstream origin/main\n# branch.ab +2 -1\n\
                    1 M. N... 100644 100644 100644 aaa bbb src/staged.rs\n\
                    1 .M N... 100644 100644 100644 aaa bbb src/edited file.rs\n\
                    1 MM N... 100644 100644 100644 aaa bbb both.rs\n\
                    2 R. N... 100644 100644 100644 aaa bbb R100 new.rs\told.rs\n\
                    u UU N... 100644 100644 100644 100644 aaa bbb ccc conflict.rs\n\
                    ? notes.txt\n";
        let s = Status::parse(text);
        assert_eq!((s.branch.as_str(), s.ahead, s.behind), ("main", 2, 1));
        assert_eq!(
            s.staged,
            [
                ('M', "src/staged.rs".into()),
                ('M', "both.rs".into()),
                ('R', "new.rs".into())
            ]
        );
        assert_eq!(
            s.unstaged,
            [('M', "src/edited file.rs".into()), ('M', "both.rs".into())]
        );
        assert_eq!(s.untracked, ["notes.txt"]);
        assert_eq!(s.conflicted, ["conflict.rs"]);
        assert_eq!(s.changed(), 6);
        assert!(
            s.render()
                .contains("tracking origin/main, ahead 2, behind 1")
        );
    }

    #[test]
    fn refuses_option_injection() {
        assert!(checked(Some("--output=/etc/passwd"), "ref").is_err());
        assert_eq!(checked(Some("HEAD~2"), "ref").unwrap(), Some("HEAD~2"));
        assert_eq!(quote("it's"), r"'it'\''s'");
    }
}
