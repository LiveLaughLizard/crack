//! Permissions: what a tool call may do without asking.
//!
//! The runtime decides, not the model. Every tool call is checked before it runs and
//! gets one of three answers: allow, ask the user, or deny. Rules come from the config
//! (`[permissions]`, Claude Code's syntax), from "allow for this session" answers, and
//! from built-in defaults:
//!
//! - reading and searching inside the project: allowed;
//! - editing inside the project: allowed (`edits = "ask"` to change it);
//! - anything outside the project: ask; secrets (`.env`, `~/.ssh`, keys…): denied;
//! - shell commands: each part of a pipeline or `&&` chain is checked; known-safe
//!   commands run, known-dangerous ones and everything else ask;
//! - MCP tools: ask, once per server with "allow for this session".
//!
//! Deny rules always win, even in YOLO mode.

use globset::{Glob, GlobMatcher};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::path::{Component, Path, PathBuf};

/// `[permissions]` in the config.
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default)]
pub struct PermissionsConfig {
    pub allow: Vec<String>,
    pub ask: Vec<String>,
    pub deny: Vec<String>,
    /// `allow` (default) or `ask` for edits inside the project.
    pub edits: Option<String>,
    /// `ask` (default) or `allow` for shell commands on neither list.
    pub bash: Option<String>,
    /// `allow` (default) or `ask` for `webfetch`.
    pub webfetch: Option<String>,
    /// Run shell commands inside a bubblewrap sandbox (Linux).
    pub sandbox: Option<bool>,
    /// Let sandboxed commands use the network (default false).
    pub sandbox_network: Option<bool>,
    /// Extra writable paths in the sandbox (the project and /tmp always are).
    pub sandbox_writable: Vec<String>,
}

/// How much is allowed without asking. Plan mode is the separate read-only agent.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Mode {
    #[default]
    Normal,
    /// Edits inside the project never ask.
    AcceptEdits,
    /// Every file change asks, showing its hunks to keep or reject one by one.
    Review,
    /// Everything runs without asking, except deny rules. Not in strict builds.
    #[cfg(feature = "yolo")]
    Yolo,
}

impl Mode {
    pub fn label(self) -> &'static str {
        match self {
            Mode::Normal => "normal",
            Mode::AcceptEdits => "accept edits",
            Mode::Review => "review edits",
            #[cfg(feature = "yolo")]
            Mode::Yolo => "YOLO",
        }
    }

    pub fn parse(value: &str) -> Option<Mode> {
        match value.trim().to_ascii_lowercase().as_str() {
            "normal" | "ask" | "default" => Some(Mode::Normal),
            "accept-edits" | "accept_edits" | "acceptedits" => Some(Mode::AcceptEdits),
            "review" | "review-edits" | "review_edits" => Some(Mode::Review),
            #[cfg(feature = "yolo")]
            "yolo" | "auto" | "bypass" => Some(Mode::Yolo),
            _ => None,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Decision {
    Allow,
    /// Needs the user's yes; the text says why.
    Ask(String),
    Deny(String),
}

/// One permission rule: `Bash`, `Bash(cargo test*)`, `Edit(src/**)`, `Read(~/notes/**)`,
/// `WebFetch(domain:docs.rs)`, `mcp__github`, `mcp__github__create_issue`.
#[derive(Clone, Debug)]
pub struct Rule {
    text: String,
    tool: String,
    pattern: Option<Pattern>,
}

#[derive(Clone, Debug)]
enum Pattern {
    /// A shell command glob (`*` matches anything, including spaces).
    Command(GlobMatcher),
    /// A path glob.
    Path(GlobMatcher),
    Domain(String),
}

impl Rule {
    pub fn parse(text: &str, root: &Path) -> Option<Rule> {
        let text = text.trim();
        let (tool, pattern) = match text.split_once('(') {
            Some((tool, rest)) => (tool.trim(), Some(rest.strip_suffix(')')?.trim())),
            None => (text, None),
        };
        let tool = canonical_tool(tool);
        let pattern = match pattern {
            None | Some("") | Some("*") => None,
            // Claude Code's `git status:*` means any command starting with `git status`.
            Some(p) if tool == "bash" => Some(Pattern::Command(
                globset::GlobBuilder::new(&match p.strip_suffix(":*") {
                    Some(prefix) => format!("{}*", prefix.trim()),
                    None => p.trim().to_owned(),
                })
                .literal_separator(false)
                .build()
                .ok()?
                .compile_matcher(),
            )),
            Some(p) if tool == "webfetch" => Some(Pattern::Domain(
                p.strip_prefix("domain:").unwrap_or(p).to_owned(),
            )),
            Some(p) => {
                let absolute = absolutize(&crate::config::expand_home(p), root);
                let glob = Glob::new(&absolute.display().to_string())
                    .ok()?
                    .compile_matcher();
                Some(Pattern::Path(glob))
            }
        };
        Some(Rule {
            text: text.to_owned(),
            tool,
            pattern,
        })
    }

    fn matches(&self, call: &Call) -> bool {
        if self.tool.starts_with("mcp__") {
            return call.tool == self.tool || call.tool.starts_with(&format!("{}__", self.tool));
        }
        if self.tool != call.family {
            return false;
        }
        match (&self.pattern, &call.subject) {
            (None, _) => true,
            (Some(Pattern::Command(glob)), Subject::Command(command)) => glob.is_match(command),
            (Some(Pattern::Path(glob)), Subject::Path(path)) => glob.is_match(path),
            (Some(Pattern::Domain(domain)), Subject::Url(url)) => {
                host(url).is_some_and(|h| h == *domain || h.ends_with(&format!(".{domain}")))
            }
            _ => false,
        }
    }
}

/// Rule families: `Edit` covers write and edit, `Read` covers the read-only file tools.
fn canonical_tool(name: &str) -> String {
    match name.to_ascii_lowercase().as_str() {
        "bash" | "shell" => "bash".into(),
        "edit" | "write" | "multiedit" => "edit".into(),
        "read" | "glob" | "grep" | "list" | "ls" => "read".into(),
        "webfetch" | "fetch" => "webfetch".into(),
        other => other.to_owned(),
    }
}

fn host(url: &str) -> Option<String> {
    reqwest::Url::parse(url).ok()?.host_str().map(str::to_owned)
}

/// Makes `path` absolute (against `base`) and removes `.` and `..` without touching disk.
fn absolutize(path: &Path, base: &Path) -> PathBuf {
    let joined = if path.is_absolute() {
        path.to_owned()
    } else {
        base.join(path)
    };
    let mut out = PathBuf::new();
    for component in joined.components() {
        match component {
            Component::ParentDir => {
                out.pop();
            }
            Component::CurDir => {}
            other => out.push(other),
        }
    }
    out
}

enum Subject {
    None,
    Command(String),
    Path(PathBuf),
    Url(String),
}

struct Call<'a> {
    tool: &'a str,
    family: String,
    subject: Subject,
}

/// Whether a tool call matches one permission rule, e.g. `Bash(git push *)` or
/// `Edit(src/**)`. Hooks use it for their `if` conditions.
pub fn rule_matches(rule: &str, tool: &str, input: &Value, root: &Path, cwd: &Path) -> bool {
    let Some(rule) = Rule::parse(rule, root) else {
        return false;
    };
    let family = canonical_tool(tool);
    let path = |key: &str| {
        input
            .get(key)
            .and_then(Value::as_str)
            .map(|p| absolutize(&crate::config::expand_home(p), cwd))
            .unwrap_or_else(|| cwd.to_owned())
    };
    let subject = match family.as_str() {
        "bash" => Subject::Command(
            input["command"]
                .as_str()
                .unwrap_or_default()
                .trim()
                .to_owned(),
        ),
        "edit" | "read" => Subject::Path(path("path")),
        "webfetch" => Subject::Url(input["url"].as_str().unwrap_or_default().to_owned()),
        _ => Subject::None,
    };
    rule.matches(&Call {
        tool,
        family,
        subject,
    })
}

/// Commands that only read or build: run without asking.
const SAFE: &[&str] = &[
    "ls",
    "pwd",
    "cat",
    "head",
    "tail",
    "wc",
    "file",
    "stat",
    "du",
    "df",
    "tree",
    "echo",
    "printf",
    "true",
    "false",
    "which",
    "whoami",
    "date",
    "uname",
    "env",
    "printenv",
    "basename",
    "dirname",
    "realpath",
    "readlink",
    "sort",
    "uniq",
    "cut",
    "tr",
    "diff",
    "cmp",
    "grep",
    "rg",
    "ag",
    "fd",
    "find",
    "jq",
    "yq",
    "sed -n",
    "awk",
    "less",
    "more",
    "column",
    "nl",
    "seq",
    "sleep",
    "test",
    "[",
    "git status",
    "git diff",
    "git log",
    "git show",
    "git branch",
    "git rev-parse",
    "git ls-files",
    "git blame",
    "git remote -v",
    "git tag",
    "git stash list",
    "cargo check",
    "cargo build",
    "cargo test",
    "cargo clippy",
    "cargo fmt",
    "cargo run",
    "cargo doc",
    "cargo tree",
    "cargo metadata",
    "cargo nextest",
    "cargo bench",
    "rustc --version",
    "go build",
    "go test",
    "go vet",
    "go fmt",
    "gofmt",
    "make",
    "just",
    "npm test",
    "npm run",
    "npm ls",
    "pnpm test",
    "pnpm run",
    "yarn test",
    "yarn run",
    "bun test",
    "bun run",
    "npx tsc",
    "tsc",
    "eslint",
    "prettier --check",
    "pytest",
    "python -m pytest",
    "ruff check",
    "mypy",
    "node --version",
    "python --version",
    "python3 --version",
];

/// Commands that can destroy work, publish, escalate or fetch-and-run: always ask.
const DANGER: &[&str] = &[
    "rm -rf",
    "rm -fr",
    "rm -r",
    "sudo",
    "doas",
    "su",
    "chmod -R",
    "chown",
    "dd",
    "mkfs",
    "shred",
    "git push",
    "git reset --hard",
    "git clean",
    "git checkout --",
    "git restore",
    "git rebase",
    "git branch -D",
    "git filter-branch",
    "git commit --amend",
    "cargo publish",
    "npm publish",
    "pnpm publish",
    "yarn publish",
    "docker push",
    "gh release",
    "kubectl delete",
    "terraform apply",
    "terraform destroy",
    "systemctl",
    "reboot",
    "shutdown",
    "kill -9",
    "pkill",
    "killall",
    "curl",
    "wget",
    "ssh",
    "scp",
    "rsync",
];

/// Paths whose contents never go to the model.
/// Text that looks like it contains a credential (API keys, tokens, private keys).
pub fn looks_secret(text: &str) -> bool {
    const PREFIXES: &[&str] = &[
        "sk-",
        "sk_live_",
        "AKIA",
        "ASIA",
        "ghp_",
        "gho_",
        "github_pat_",
        "xoxb-",
        "xoxp-",
        "AIza",
        "glpat-",
        "-----BEGIN",
    ];
    if text.split_whitespace().any(|w| {
        let w = w.trim_matches(|c: char| !c.is_ascii_alphanumeric() && c != '-' && c != '_');
        PREFIXES
            .iter()
            .any(|p| w.starts_with(p) && w.len() >= p.len() + 12)
    }) || text.contains("-----BEGIN")
    {
        return true;
    }
    let lower = text.to_ascii_lowercase();
    [
        "password=",
        "password:",
        "secret=",
        "token=",
        "api_key=",
        "apikey=",
    ]
    .iter()
    .any(|k| {
        lower.find(k).is_some_and(|i| {
            lower[i + k.len()..]
                .trim_start()
                .chars()
                .take_while(|c| !c.is_whitespace())
                .count()
                >= 6
        })
    })
}

pub fn is_secret_path(path: &Path) -> bool {
    is_secret(path)
}

fn is_secret(path: &Path) -> bool {
    let text = path.display().to_string();
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_default();
    let home = dirs::home_dir().unwrap_or_default();
    let secret_dirs = [
        ".ssh",
        ".gnupg",
        ".aws",
        ".azure",
        ".kube",
        ".docker",
        ".config/gcloud",
        ".password-store",
    ];
    (name == ".env"
        || (name.starts_with(".env.") && !name.ends_with(".example") && !name.ends_with(".sample")))
        || name.ends_with(".pem")
        || name.ends_with(".key")
        || name == "id_rsa"
        || name == "id_ed25519"
        || name == ".netrc"
        || name == ".npmrc"
        || name == ".pypirc"
        || text.ends_with("crack/credentials.toml")
        || secret_dirs.iter().any(|d| path.starts_with(home.join(d)))
}

/// Splits a shell command into its simple commands: on `;`, `&&`, `||`, `|`, `&` and
/// newlines, outside quotes. Returns `None` when it can't be analysed (command
/// substitution, `eval`), which means "ask".
pub fn split_command(command: &str) -> Option<Vec<String>> {
    let mut parts = Vec::new();
    let mut current = String::new();
    let mut chars = command.chars().peekable();
    let (mut single, mut double) = (false, false);
    while let Some(c) = chars.next() {
        match c {
            '\\' if !single => {
                current.push(c);
                if let Some(next) = chars.next() {
                    current.push(next);
                }
            }
            '\'' if !double => {
                single = !single;
                current.push(c);
            }
            '"' if !single => {
                double = !double;
                current.push(c);
            }
            '`' if !single => return None,
            '$' if !single && chars.peek() == Some(&'(') => return None,
            ';' | '|' | '&' | '\n' if !single && !double => {
                if matches!(chars.peek(), Some('|') | Some('&')) {
                    chars.next();
                }
                parts.push(std::mem::take(&mut current));
            }
            _ => current.push(c),
        }
    }
    parts.push(current);
    Some(
        parts
            .into_iter()
            .map(|p| p.trim().to_owned())
            .filter(|p| !p.is_empty())
            .collect(),
    )
}

/// `cargo test --all` against list entries like `cargo test` (whole words only).
fn starts_with_words(command: &str, prefix: &str) -> bool {
    // Skip leading `VAR=value` assignments.
    let words: Vec<&str> = command
        .split_whitespace()
        .skip_while(|w| w.contains('=') && !w.starts_with('-'))
        .collect();
    let prefix: Vec<&str> = prefix.split_whitespace().collect();
    words.len() >= prefix.len()
        && words
            .iter()
            .zip(&prefix)
            .all(|(w, p)| w == p || w.ends_with(&format!("/{p}")))
}

/// The rule offered by "allow for this session": the command's program and
/// subcommand (`cargo test *`), a folder, or an MCP server.
pub fn suggest_rule(tool: &str, input: &Value, root: &Path, cwd: &Path) -> String {
    // Commits always ask, so there's no rule to offer.
    if tool == "git_commit" {
        return String::new();
    }
    if let Some(server) = tool
        .strip_prefix("mcp__")
        .and_then(|rest| rest.split("__").next())
    {
        return format!("mcp__{server}");
    }
    match canonical_tool(tool).as_str() {
        "bash" => {
            let command = input["command"].as_str().unwrap_or_default();
            let first = split_command(command)
                .and_then(|parts| parts.into_iter().find(|p| !is_safe(p)))
                .unwrap_or_else(|| command.to_owned());
            let words: Vec<&str> = first.split_whitespace().take(2).collect();
            let prefix = match words.as_slice() {
                [program, sub] if !sub.starts_with('-') && !sub.contains('/') => {
                    format!("{program} {sub}")
                }
                [program, ..] => program.to_string(),
                [] => String::new(),
            };
            format!("Bash({prefix} *)")
        }
        family @ ("edit" | "read") => {
            let path = input["path"].as_str().unwrap_or(".");
            let path = absolutize(&crate::config::expand_home(path), cwd);
            let dir = if path.is_dir() {
                path
            } else {
                path.parent().map(Path::to_owned).unwrap_or(path)
            };
            let shown = dir
                .strip_prefix(root)
                .map(|p| {
                    if p.as_os_str().is_empty() {
                        ".".into()
                    } else {
                        p.display().to_string()
                    }
                })
                .unwrap_or_else(|_| dir.display().to_string());
            let label = if family == "edit" { "Edit" } else { "Read" };
            format!("{label}({shown}/**)")
        }
        "webfetch" => match host(input["url"].as_str().unwrap_or_default()) {
            Some(h) => format!("WebFetch(domain:{h})"),
            None => "WebFetch".into(),
        },
        other => other.to_owned(),
    }
}

fn is_safe(part: &str) -> bool {
    SAFE.iter().any(|s| starts_with_words(part, s))
        && !DANGER.iter().any(|d| starts_with_words(part, d))
}

/// A word of the command that names a secret file (`cat .env`, `~/.ssh/id_rsa`).
fn mentions_secret(part: &str, cwd: &Path) -> Option<String> {
    part.split_whitespace()
        .map(|w| {
            w.trim_matches(['"', '\'', ';'])
                .trim_start_matches('<')
                .trim_start_matches('>')
        })
        .filter(|w| !w.is_empty() && !w.starts_with('-'))
        .find(|w| is_secret(&absolutize(&crate::config::expand_home(w), cwd)))
        .map(str::to_owned)
}

/// Output redirections to files outside `root` (`> /etc/x`, `>> ~/.bashrc`).
fn redirects_outside(part: &str, root: &Path, cwd: &Path) -> Option<String> {
    let mut words = part.split_whitespace().peekable();
    while let Some(word) = words.next() {
        let target = if let Some(rest) = word.strip_prefix(">>").or_else(|| word.strip_prefix('>'))
        {
            if rest.is_empty() {
                words.peek().copied().map(str::to_owned)
            } else {
                Some(rest.to_owned())
            }
        } else {
            None
        };
        if let Some(target) = target
            && target != "/dev/null"
            && !target.starts_with('&')
        {
            let path = absolutize(
                &crate::config::expand_home(target.trim_matches(['"', '\''])),
                cwd,
            );
            if !path.starts_with(root) && !path.starts_with("/tmp") {
                return Some(path.display().to_string());
            }
        }
    }
    None
}

/// The checks for one session.
#[derive(Clone)]
pub struct Policy {
    pub root: PathBuf,
    pub cwd: PathBuf,
    pub mode: Mode,
    allow: Vec<Rule>,
    ask: Vec<Rule>,
    deny: Vec<Rule>,
    edits_ask: bool,
    bash_allow: bool,
    webfetch_ask: bool,
    pub sandbox: bool,
}

impl Policy {
    /// `sandboxed`: shell commands run in the sandbox, so unknown ones needn't ask.
    pub fn new(config: &PermissionsConfig, cwd: &Path, mode: Mode, sandboxed: bool) -> Policy {
        let root = crate::context::project_root(cwd);
        let parse = |rules: &[String]| rules.iter().filter_map(|r| Rule::parse(r, &root)).collect();
        Policy {
            allow: parse(&config.allow),
            ask: parse(&config.ask),
            deny: parse(&config.deny),
            edits_ask: config.edits.as_deref() == Some("ask"),
            bash_allow: config.bash.as_deref() == Some("allow"),
            webfetch_ask: config.webfetch.as_deref() == Some("ask"),
            sandbox: sandboxed,
            root,
            cwd: cwd.to_owned(),
            mode,
        }
    }

    /// Adds "allow for this session" rules.
    pub fn with_session_rules(mut self, rules: &[String]) -> Policy {
        let root = self.root.clone();
        self.allow
            .extend(rules.iter().filter_map(|r| Rule::parse(r, &root)));
        self
    }

    fn path_of(&self, input: &Value, key: &str) -> Option<PathBuf> {
        input
            .get(key)
            .and_then(Value::as_str)
            .map(|p| absolutize(&crate::config::expand_home(p), &self.cwd))
    }

    pub fn check(&self, tool: &str, input: &Value) -> Decision {
        let family = canonical_tool(tool);
        let subject = match family.as_str() {
            "bash" => Subject::Command(
                input["command"]
                    .as_str()
                    .unwrap_or_default()
                    .trim()
                    .to_owned(),
            ),
            "edit" | "read" => match self.path_of(input, "path") {
                Some(path) => Subject::Path(path),
                None => Subject::Path(self.cwd.clone()),
            },
            "webfetch" => Subject::Url(input["url"].as_str().unwrap_or_default().to_owned()),
            _ => Subject::None,
        };
        let call = Call {
            tool,
            family,
            subject,
        };

        // Explicit rules first: deny, then ask, then allow.
        if let Some(rule) = self.deny.iter().find(|r| r.matches(&call)) {
            return Decision::Deny(format!("denied by the rule `{}`", rule.text));
        }
        if let Subject::Path(path) = &call.subject
            && is_secret(path)
        {
            return Decision::Deny(format!("{} may hold secrets", path.display()));
        }
        // Shell commands are judged part by part, so `echo ok && rm -rf ~` isn't `echo`.
        if let Subject::Command(command) = &call.subject {
            return self.check_command(&call, command);
        }
        // A rename rewrites many files at once: ask first, unless edits don't ask.
        if tool == "rename_symbol"
            && self.mode == Mode::Normal
            && !self.allow.iter().any(|r| r.matches(&call))
        {
            return Decision::Ask("renames change every file that uses the symbol".into());
        }
        // Review mode: every file change is shown and asked about, rules or not.
        if self.mode == Mode::Review && matches!(tool, "edit" | "write" | "multiedit") {
            return Decision::Ask("review mode: you approve each change".into());
        }
        // Commits are never automatic, not even in YOLO mode.
        if tool == "git_commit" {
            return Decision::Ask("commits always ask first".into());
        }
        if let Some(rule) = self.ask.iter().find(|r| r.matches(&call)) {
            return Decision::Ask(format!("the rule `{}` asks first", rule.text));
        }
        if self.yolo() || self.allow.iter().any(|r| r.matches(&call)) {
            return Decision::Allow;
        }
        match call.family.as_str() {
            "read" => match &call.subject {
                // Full outputs of trimmed shell commands.
                Subject::Path(path) if path.starts_with(crate::noise::dir()) => Decision::Allow,
                Subject::Path(path) if !path.starts_with(&self.root) => {
                    Decision::Ask(format!("reads outside the project: {}", path.display()))
                }
                _ => Decision::Allow,
            },
            "edit" => match &call.subject {
                Subject::Path(path) if !path.starts_with(&self.root) => Decision::Ask(format!(
                    "changes a file outside the project: {}",
                    path.display()
                )),
                _ if self.edits_ask && self.mode == Mode::Normal => {
                    Decision::Ask("edits ask first (permissions.edits)".into())
                }
                _ => Decision::Allow,
            },
            "webfetch" if self.webfetch_ask => Decision::Ask("web fetches ask first".into()),
            _ if tool.starts_with("mcp__") => {
                let server = tool
                    .trim_start_matches("mcp__")
                    .split("__")
                    .next()
                    .unwrap_or("");
                Decision::Ask(format!("uses the {server} MCP server"))
            }
            _ => Decision::Allow,
        }
    }

    fn yolo(&self) -> bool {
        #[cfg(feature = "yolo")]
        {
            self.mode == Mode::Yolo
        }
        #[cfg(not(feature = "yolo"))]
        {
            false
        }
    }

    fn check_command(&self, call: &Call, command: &str) -> Decision {
        let Some(parts) = split_command(command) else {
            if self.yolo() || self.allow.iter().any(|r| r.matches(call)) {
                return Decision::Allow;
            }
            return Decision::Ask(
                "uses command substitution, which can't be checked part by part".into(),
            );
        };
        let mut reason: Option<String> = None;
        for part in &parts {
            let one = Call {
                tool: call.tool,
                family: "bash".into(),
                subject: Subject::Command(part.clone()),
            };
            if let Some(rule) = self.deny.iter().find(|r| r.matches(&one)) {
                return Decision::Deny(format!("`{part}` is denied by the rule `{}`", rule.text));
            }
            if self.yolo() {
                continue;
            }
            let decision = if let Some(rule) = self.ask.iter().find(|r| r.matches(&one)) {
                Some(format!("the rule `{}` asks first", rule.text))
            } else if let Some(target) = redirects_outside(part, &self.root, &self.cwd) {
                Some(format!("writes outside the project: {target}"))
            } else if let Some(secret) = mentions_secret(part, &self.cwd) {
                // The sandbox can't hide secrets inside the project, like `.env`.
                Some(format!("touches {secret}, which may hold secrets"))
            } else if self.allow.iter().any(|r| r.matches(&one)) {
                // Rules the user allowed (in config or this session) beat the built-in lists.
                None
            } else if DANGER.iter().any(|d| starts_with_words(part, d)) {
                Some(format!("`{part}` can change things that are hard to undo"))
            } else if is_safe(part) || self.bash_allow {
                None
            } else if self.sandbox {
                // Inside the sandbox an unknown command can only touch the project.
                None
            } else {
                Some(format!("runs `{part}`"))
            };
            if reason.is_none() {
                reason = decision;
            }
        }
        match reason {
            Some(reason) => Decision::Ask(reason),
            None => Decision::Allow,
        }
    }
}

/// The user's answer to an approval request.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Answer {
    Once,
    /// Allow calls matching this rule for the rest of the session.
    Session(String),
    /// Allow it from now on: save the rule to the project config.
    Always(String),
    Deny(Option<String>),
    /// Apply only these hunks of a file change (review mode).
    Partial(Vec<bool>),
}

/// The effective sandbox settings.
#[derive(Clone, Debug)]
pub struct Sandbox {
    /// Wanted: forced by the build, or the user's toggle, config, or the default (on).
    pub wanted: bool,
    /// A sandbox works here (Landlock, Seatbelt, an AppContainer or bubblewrap).
    pub available: bool,
    /// This build forces the sandbox (`force-sandbox`); it can't be turned off.
    pub forced: bool,
    pub network: bool,
    pub writable: Vec<PathBuf>,
}

static SANDBOX_OVERRIDE: std::sync::OnceLock<bool> = std::sync::OnceLock::new();

/// `crack run --sandbox on|off`: wins over the saved toggle and config (not over a
/// forced build).
pub fn override_sandbox(on: bool) {
    let _ = SANDBOX_OVERRIDE.set(on);
}

impl Sandbox {
    pub fn resolve(config: &PermissionsConfig) -> Sandbox {
        let state = crate::state::State::load();
        let forced = cfg!(feature = "force-sandbox");
        let wanted = forced
            || SANDBOX_OVERRIDE
                .get()
                .copied()
                .or(state.sandbox)
                .or(config.sandbox)
                .unwrap_or(true);
        let home = dirs::home_dir().unwrap_or_default();
        // Package caches builds write to; the rest of $HOME stays read-only.
        let mut writable: Vec<PathBuf> = [
            ".cargo/registry",
            ".cargo/git",
            ".cache",
            ".npm",
            "go/pkg/mod",
            ".local/share/pnpm",
        ]
        .iter()
        .map(|p| home.join(p))
        .collect();
        writable.extend(
            config
                .sandbox_writable
                .iter()
                .map(|p| crate::config::expand_home(p)),
        );
        Sandbox {
            wanted,
            available: crate::sandbox::native_available()
                || crate::sandbox::seatbelt_available()
                || crate::sandbox::windows_available()
                || bwrap_installed(),
            forced,
            network: state
                .sandbox_network
                .or(config.sandbox_network)
                .unwrap_or(true),
            writable,
        }
    }

    /// Commands actually run in the sandbox.
    pub fn active(&self) -> bool {
        self.wanted && self.available
    }

    /// Which sandbox runs commands, for display.
    pub fn backend(&self) -> &'static str {
        if crate::sandbox::native_available() {
            "built-in"
        } else if crate::sandbox::seatbelt_available() {
            "sandbox-exec"
        } else if crate::sandbox::windows_available() {
            "AppContainer"
        } else if bwrap_installed() {
            "bubblewrap"
        } else {
            "none"
        }
    }
}

/// Home folders whose contents a sandboxed command can't read.
pub fn hidden_paths() -> Vec<PathBuf> {
    let home = dirs::home_dir().unwrap_or_default();
    [
        ".ssh",
        ".gnupg",
        ".aws",
        ".azure",
        ".kube",
        ".docker",
        ".config/gcloud",
        ".password-store",
        ".config/crack",
    ]
    .iter()
    .map(|p| home.join(p))
    .filter(|p| p.exists())
    .collect()
}

/// Wraps a shell command so it runs sandboxed: the system read-only, the project,
/// /tmp and package caches writable, secrets hidden, and no network unless allowed.
/// Uses Seatbelt on macOS, an AppContainer on Windows, and on Linux the built-in
/// sandbox (Landlock) where the kernel has it, else bubblewrap.
pub fn sandboxed(command: &str, settings: &Sandbox, root: &Path) -> Option<Vec<String>> {
    if crate::sandbox::seatbelt_available() {
        let mut writable = vec![
            root.to_owned(),
            PathBuf::from("/tmp"),
            PathBuf::from("/private/var/folders"),
        ];
        writable.extend(settings.writable.iter().filter(|p| p.exists()).cloned());
        return Some(crate::sandbox::seatbelt_command(
            command,
            &writable,
            &hidden_paths(),
            settings.network,
        ));
    }
    if crate::sandbox::native_available() {
        let mut writable = vec![
            root.to_owned(),
            PathBuf::from("/tmp"),
            PathBuf::from("/dev"),
        ];
        writable.extend(settings.writable.iter().filter(|p| p.exists()).cloned());
        return crate::sandbox::command(command, &writable, &hidden_paths(), settings.network);
    }
    if crate::sandbox::windows_available() {
        // The container gets its own temporary folder, and can't read the profile.
        let mut writable = vec![root.to_owned()];
        writable.extend(settings.writable.iter().filter(|p| p.exists()).cloned());
        return crate::sandbox::command(command, &writable, &[], settings.network);
    }
    let bwrap = which("bwrap")?;
    let mut args: Vec<String> = vec![
        bwrap.display().to_string(),
        "--ro-bind".into(),
        "/".into(),
        "/".into(),
        "--dev".into(),
        "/dev".into(),
        "--proc".into(),
        "/proc".into(),
        "--tmpfs".into(),
        "/tmp".into(),
        "--bind".into(),
        root.display().to_string(),
        root.display().to_string(),
        "--die-with-parent".into(),
    ];
    for path in &settings.writable {
        if path.exists() {
            args.extend([
                "--bind".into(),
                path.display().to_string(),
                path.display().to_string(),
            ]);
        }
    }
    for path in hidden_paths().into_iter().filter(|p| p.is_dir()) {
        args.extend(["--tmpfs".into(), path.display().to_string()]);
    }
    if !settings.network {
        args.push("--unshare-net".into());
    }
    args.extend(["bash".into(), "-c".into(), command.to_owned()]);
    Some(args)
}

/// Looked up once per process.
fn bwrap_installed() -> bool {
    static FOUND: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *FOUND.get_or_init(|| which("bwrap").is_some())
}

pub fn which(program: &str) -> Option<PathBuf> {
    // Windows finds `git` as `git.exe` (any PATHEXT extension).
    let extensions: Vec<String> = if cfg!(windows) {
        std::iter::once(String::new())
            .chain(
                std::env::var("PATHEXT")
                    .unwrap_or_else(|_| ".EXE;.CMD;.BAT;.COM".into())
                    .split(';')
                    .filter(|e| !e.is_empty())
                    .map(str::to_ascii_lowercase),
            )
            .collect()
    } else {
        vec![String::new()]
    };
    std::env::split_paths(&std::env::var_os("PATH")?)
        .flat_map(|dir| {
            extensions
                .iter()
                .map(move |ext| dir.join(format!("{program}{ext}")))
                .collect::<Vec<_>>()
        })
        .find(|p| p.is_file())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn policy(config: PermissionsConfig) -> Policy {
        let root = std::env::temp_dir().join("crack-policy-project");
        std::fs::create_dir_all(root.join(".git")).unwrap();
        Policy::new(&config, &root, Mode::Normal, false)
    }

    fn bash(p: &Policy, command: &str) -> Decision {
        p.check("bash", &json!({ "command": command }))
    }

    #[test]
    fn shell_commands_are_checked_part_by_part() {
        let p = policy(Default::default());
        assert_eq!(bash(&p, "ls -la"), Decision::Allow);
        assert_eq!(bash(&p, "cargo test --all && git status"), Decision::Allow);
        assert!(matches!(bash(&p, "echo ok && rm -rf ~"), Decision::Ask(_)));
        assert!(matches!(bash(&p, "ls | sh -c 'x'"), Decision::Ask(_)));
        assert!(matches!(bash(&p, "echo $(whoami)"), Decision::Ask(_)));
        assert!(matches!(bash(&p, "echo hi > ~/.bashrc"), Decision::Ask(_)));
        assert_eq!(bash(&p, "echo hi > /dev/null"), Decision::Allow);
        assert!(matches!(bash(&p, "git push origin main"), Decision::Ask(_)));
        assert!(matches!(bash(&p, "python script.py"), Decision::Ask(_)));
        // Secrets inside the project, which the sandbox can't hide.
        assert!(matches!(bash(&p, "cat .env"), Decision::Ask(_)));
        assert!(matches!(
            bash(&p, "grep KEY ~/.ssh/id_ed25519"),
            Decision::Ask(_)
        ));
        assert_eq!(bash(&p, "cat .env.example"), Decision::Allow);
        // Quoted separators are not splits.
        assert_eq!(
            split_command("echo 'a && b'; ls").unwrap(),
            vec!["echo 'a && b'", "ls"]
        );
    }

    #[test]
    fn rules_and_session_answers() {
        let config = PermissionsConfig {
            allow: vec!["Bash(python *)".into()],
            deny: vec!["Bash(git push*)".into()],
            ..Default::default()
        };
        let p = policy(config);
        assert_eq!(bash(&p, "python script.py"), Decision::Allow);
        assert!(matches!(bash(&p, "git push --force"), Decision::Deny(_)));
        // Deny wins even when the dangerous part hides behind a safe one.
        assert!(matches!(bash(&p, "ls && git push"), Decision::Deny(_)));

        let p = policy(Default::default());
        let rule = suggest_rule(
            "bash",
            &json!({"command": "npm install left-pad"}),
            &p.root,
            &p.cwd,
        );
        assert_eq!(rule, "Bash(npm install *)");
        let p = p.with_session_rules(&[rule]);
        assert_eq!(bash(&p, "npm install react"), Decision::Allow);
        assert!(matches!(bash(&p, "npm publish"), Decision::Ask(_)));
        // Allowing a dangerous command for the session stops it asking.
        let p = p.with_session_rules(&["Bash(git push *)".into()]);
        assert_eq!(bash(&p, "git push --dry-run"), Decision::Allow);
        assert!(matches!(bash(&p, "git reset --hard"), Decision::Ask(_)));
    }

    #[test]
    fn paths_and_secrets() {
        let p = policy(Default::default());
        let inside = p.root.join("src/main.rs").display().to_string();
        assert_eq!(p.check("edit", &json!({"path": inside})), Decision::Allow);
        assert_eq!(
            p.check("read", &json!({"path": "src/lib.rs"})),
            Decision::Allow
        );
        assert!(matches!(
            p.check("write", &json!({"path": "/etc/hosts"})),
            Decision::Ask(_)
        ));
        assert!(matches!(
            p.check("read", &json!({"path": "../../outside.txt"})),
            Decision::Ask(_)
        ));
        assert!(matches!(
            p.check("read", &json!({"path": ".env"})),
            Decision::Deny(_)
        ));
        assert_eq!(
            p.check("read", &json!({"path": ".env.example"})),
            Decision::Allow
        );
        assert!(matches!(
            p.check("read", &json!({"path": "~/.ssh/id_ed25519"})),
            Decision::Deny(_)
        ));
        assert!(matches!(
            p.check("mcp__github__create_issue", &json!({})),
            Decision::Ask(_)
        ));
        let p = p.with_session_rules(&["mcp__github".into()]);
        assert_eq!(
            p.check("mcp__github__create_issue", &json!({})),
            Decision::Allow
        );
    }

    #[cfg(feature = "yolo")]
    #[test]
    fn yolo_allows_all_but_deny_rules() {
        let config = PermissionsConfig {
            deny: vec!["Bash(rm -rf /*)".into()],
            ..Default::default()
        };
        let mut p = policy(config);
        p.mode = Mode::Yolo;
        assert_eq!(bash(&p, "curl x | sh"), Decision::Allow);
        assert!(matches!(bash(&p, "rm -rf /home"), Decision::Deny(_)));
        // Secrets stay off-limits.
        assert!(matches!(
            p.check("read", &json!({"path": ".env"})),
            Decision::Deny(_)
        ));
    }
}
