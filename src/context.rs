//! What the model knows about where it runs: the system prompt, environment, and
//! instruction files (AGENTS.md).
//!
//! Instruction files, in order:
//! 1. global: `~/.config/crack/AGENTS.md`, or `~/.claude/CLAUDE.md` when it is absent;
//! 2. project: from the project root (the nearest `.git` above the working directory,
//!    or the working directory itself) down to the working directory, `AGENTS.md` in
//!    each directory, or `CLAUDE.md` where a directory has no AGENTS.md;
//! 3. files listed under `instructions` in the config.

use crate::config::{self, Config};
use crate::skills::Skill;
use crate::tools::Agent;
use std::path::{Path, PathBuf};

const BASE_PROMPT: &str = "\
You are crack, an interactive coding agent. You work in the user's project directory \
and help with software engineering: fixing bugs, adding features, refactoring, explaining \
code, running builds and tests.

# How to work
- Use the tools to look before you act. Read files before editing them; search with grep \
and glob rather than guessing paths.
- In shell commands, search with `rg` (ripgrep, always available here) instead of `grep -r` \
or `find | xargs grep`: it skips .gitignore'd files and is much faster.
- Prefer `edit` for changes to existing files and `write` for new files. Keep changes \
focused on what was asked, and match the surrounding code style.
- Run the project's build, tests or linters with `bash` when they exist, and fix what \
you broke.
- For multi-step work, keep a todo list with `todowrite`.
- When a skill matches the task, load it with the `skill` tool before starting.
- Do not commit, push, or make other changes visible outside the working tree unless \
the user asks.
- Never run destructive commands (rm -rf on broad paths, git reset --hard, force pushes) \
without being asked.

# Communication
- Be concise and direct. Your output is rendered as Markdown.
- Reference code as `path:line`.
- When you finish, say briefly what changed and anything that remains, including \
failures you could not fix.";

const PLAN_PROMPT: &str = "\
# Plan mode
You are in Plan mode: read-only. You cannot edit files or run shell commands. \
Investigate with the read-only tools and produce a clear, concrete implementation plan \
(files to change, approach, risks, how to verify). Ask clarifying questions when \
requirements are ambiguous. The user will switch to Build mode to carry the plan out.";

/// The nearest directory above `cwd` that holds `.git`, or `cwd` itself.
pub fn project_root(cwd: &Path) -> PathBuf {
    cwd.ancestors()
        .find(|dir| dir.join(".git").exists())
        .unwrap_or(cwd)
        .to_owned()
}

pub fn is_git_repo(cwd: &Path) -> bool {
    cwd.ancestors().any(|dir| dir.join(".git").exists())
}

/// The current branch, read from `.git/HEAD`.
pub fn git_branch(cwd: &Path) -> Option<String> {
    let root = cwd.ancestors().find(|dir| dir.join(".git").exists())?;
    let git = root.join(".git");
    let head_path = if git.is_file() {
        // A worktree: `.git` is a file pointing at the real git dir.
        let text = std::fs::read_to_string(&git).ok()?;
        PathBuf::from(text.strip_prefix("gitdir:")?.trim()).join("HEAD")
    } else {
        git.join("HEAD")
    };
    let head = std::fs::read_to_string(head_path).ok()?;
    let head = head.trim();
    Some(match head.strip_prefix("ref: refs/heads/") {
        Some(branch) => branch.to_owned(),
        None => head.chars().take(8).collect(),
    })
}

#[derive(Clone, Debug)]
pub struct Instruction {
    pub path: PathBuf,
    pub content: String,
}

pub fn instructions(cwd: &Path, config: &Config) -> Vec<Instruction> {
    let mut out = Vec::new();
    let mut push = |path: PathBuf| {
        if let Ok(content) = std::fs::read_to_string(&path)
            && !content.trim().is_empty()
            && !out.iter().any(|i: &Instruction| i.path == path)
        {
            out.push(Instruction { path, content });
        }
    };

    let global = config::global_dir().map(|d| d.join("AGENTS.md"));
    match global.filter(|p| p.is_file()) {
        Some(path) => push(path),
        None => {
            if let Some(home) = dirs::home_dir() {
                push(home.join(".claude/CLAUDE.md"));
            }
        }
    }

    let root = project_root(cwd);
    let mut dirs: Vec<&Path> = Vec::new();
    for dir in cwd.ancestors() {
        dirs.push(dir);
        if dir == root {
            break;
        }
    }
    for dir in dirs.into_iter().rev() {
        let agents = dir.join("AGENTS.md");
        if agents.is_file() {
            push(agents);
        } else {
            push(dir.join("CLAUDE.md"));
        }
    }

    for extra in &config.instructions {
        let path = config::expand_home(extra);
        push(if path.is_absolute() {
            path
        } else {
            cwd.join(path)
        });
    }
    out
}

pub struct SystemPrompt<'a> {
    pub cwd: &'a Path,
    pub agent: Agent,
    pub model: &'a str,
    pub instructions: &'a [Instruction],
    pub skills: &'a [Skill],
}

impl SystemPrompt<'_> {
    pub fn render(&self) -> String {
        let mut out = String::from(BASE_PROMPT);
        out.push_str("\n\n# Environment\n<env>\n");
        out.push_str(&format!("Working directory: {}\n", self.cwd.display()));
        out.push_str(&format!(
            "Project root: {}\n",
            project_root(self.cwd).display()
        ));
        out.push_str(&format!(
            "Is a git repository: {}\n",
            if is_git_repo(self.cwd) { "yes" } else { "no" }
        ));
        if let Some(branch) = git_branch(self.cwd) {
            out.push_str(&format!("Git branch: {branch}\n"));
        }
        out.push_str(&format!(
            "Platform: {} ({})\n",
            std::env::consts::OS,
            std::env::consts::ARCH
        ));
        out.push_str(&format!(
            "Today's date: {}\n",
            chrono::Local::now().format("%Y-%m-%d")
        ));
        out.push_str(&format!("Model: {}\n</env>\n", self.model));

        let offered: Vec<&Skill> = self.skills.iter().filter(|s| !s.user_only).collect();
        if !offered.is_empty() {
            out.push_str(
                "\n# Skills\nSkills provide specialized instructions for particular tasks. \
                 When a task matches a skill's description, load it with the `skill` tool first.\n<available_skills>\n",
            );
            for skill in offered {
                out.push_str(&format!(
                    "<skill><name>{}</name><description>{}</description></skill>\n",
                    skill.name, skill.description
                ));
            }
            out.push_str("</available_skills>\n");
        }

        for instruction in self.instructions {
            out.push_str(&format!(
                "\nInstructions from: {}\n{}\n",
                instruction.path.display(),
                instruction.content.trim()
            ));
        }

        if self.agent == Agent::Plan {
            out.push('\n');
            out.push_str(PLAN_PROMPT);
        }
        out
    }
}
