//! Subagents: definitions in Claude Code's format, and choosing their models.
//!
//! A definition is markdown with frontmatter, in `.crack/agents/`, `.claude/agents/`
//! (each from the working directory up to the project root) or
//! `~/.config/crack/agents/`:
//!
//! ```markdown
//! ---
//! name: reviewer
//! description: Reviews a diff for bugs. Use after making changes.
//! tools: read, grep, glob, git_diff        # optional; default: all but `task`
//! model: haiku                             # optional: inherit, haiku/sonnet/opus, or provider/model
//! effort: low                              # optional
//! isolation: worktree                      # optional: edit in a separate git worktree
//! ---
//! You are a careful code reviewer...
//! ```

use std::path::{Path, PathBuf};

use crate::providers::Effort;
use crate::skills::{frontmatter_value, split_frontmatter};

#[derive(Clone, Debug, PartialEq)]
pub struct AgentDef {
    pub name: String,
    pub description: String,
    /// Tool names it may use; `None` for every tool but `task`.
    pub tools: Option<Vec<String>>,
    /// `None` or "inherit": the parent's model. Otherwise an alias or `provider/model`.
    pub model: Option<String>,
    pub effort: Option<Effort>,
    pub worktree: bool,
    pub prompt: String,
    pub path: Option<PathBuf>,
}

pub const READ_ONLY_TOOLS: &[&str] = &[
    "symbols",
    "find_symbol",
    "read_symbol",
    "read",
    "glob",
    "grep",
    "list",
    "git_status",
    "git_diff",
    "git_log",
    "git_show",
    "skill",
];

fn builtins() -> Vec<AgentDef> {
    vec![
        AgentDef {
            name: "explore".into(),
            description: "Fast, read-only codebase search. Use it to find files, definitions, usages and how something \
                works, especially to explore several areas in parallel. Say how thorough to be."
                .into(),
            tools: Some(READ_ONLY_TOOLS.iter().map(|t| (*t).to_owned()).collect()),
            model: Some("small".into()),
            effort: Some(Effort::Low),
            worktree: false,
            prompt: "You are a read-only codebase explorer. Search efficiently: use grep and glob to find candidates, \
                read only what you need, and search several places at once when you can. You can't change anything. \
                Finish with a concise report: what you found, with file paths and line numbers, and anything you \
                couldn't determine."
                .into(),
            path: None,
        },
        AgentDef {
            name: "general".into(),
            description: "A general-purpose agent for self-contained multi-step tasks: research, edits or both."
                .into(),
            tools: None,
            model: None,
            effort: None,
            worktree: false,
            prompt: "You are a capable software engineering agent handling one delegated task. Work independently \
                with your tools until the task is done. Finish with a concise report of what you did and found, \
                with file paths; the agent that delegated this sees only that report."
                .into(),
            path: None,
        },
    ]
}

fn parse(path: &Path) -> Option<AgentDef> {
    let text = std::fs::read_to_string(path).ok()?;
    let (front, body) = split_frontmatter(&text);
    let name = frontmatter_value(front, "name").unwrap_or_else(|| {
        path.file_stem()
            .unwrap_or_default()
            .to_string_lossy()
            .into_owned()
    });
    let description = frontmatter_value(front, "description").unwrap_or_default();
    let tools = frontmatter_value(front, "tools").map(|t| {
        t.trim_matches(['[', ']'])
            .split(',')
            .map(|s| crate::hooks::crack_tool_name(s.trim().trim_matches(['"', '\''])))
            .filter(|s| !s.is_empty())
            .collect()
    });
    Some(AgentDef {
        name,
        description,
        tools,
        model: frontmatter_value(front, "model").filter(|m| !m.is_empty() && m != "inherit"),
        effort: frontmatter_value(front, "effort").and_then(|e| Effort::parse(&e)),
        worktree: frontmatter_value(front, "isolation").as_deref() == Some("worktree"),
        prompt: body.trim().to_owned(),
        path: Some(path.to_owned()),
    })
}

/// Every definition, the closest first: project folders, then the user's, then built-ins.
pub fn discover(cwd: &Path) -> Vec<AgentDef> {
    let root = crate::context::project_root(cwd);
    let mut dirs = Vec::new();
    for dir in cwd.ancestors() {
        dirs.push(dir.join(".crack/agents"));
        dirs.push(dir.join(".claude/agents"));
        if dir == root {
            break;
        }
    }
    if let Some(config) = crate::config::global_dir() {
        dirs.push(config.join("agents"));
    }
    // Enabled plugins' agents, after the user's own.
    for plugin in crate::plugins::enabled(Some(cwd)) {
        dirs.extend(
            plugin
                .components
                .agent_paths
                .into_iter()
                .filter(|p| p.is_dir()),
        );
    }
    let mut defs: Vec<AgentDef> = Vec::new();
    for dir in dirs {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        let mut files: Vec<PathBuf> = entries
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.extension().is_some_and(|e| e == "md"))
            .collect();
        files.sort();
        for file in files {
            if let Some(def) = parse(&file)
                && !defs.iter().any(|d| d.name == def.name)
            {
                defs.push(def);
            }
        }
    }
    for def in builtins() {
        if !defs.iter().any(|d| d.name == def.name) {
            defs.push(def);
        }
    }
    defs
}

/// The model reference for a subagent: `provider/model`.
pub fn choose_model(
    def: &AgentDef,
    parent: &str,
    registry: &crate::providers::registry::Registry,
    configured: Option<&str>,
) -> String {
    let (provider, parent_model) = parent.split_once('/').unwrap_or(("", parent));
    let wanted = match def.model.as_deref() {
        None => return parent.to_owned(),
        Some("small") => match configured {
            Some(model) => return model.to_owned(),
            None => "small",
        },
        Some(model) if model.contains('/') => return model.to_owned(),
        Some(alias) => alias,
    };
    let known: Vec<String> = registry
        .find(provider)
        .map(|p| p.models.iter().map(|m| m.id.clone()).collect())
        .unwrap_or_default();
    let pick = |candidates: &[&str]| -> Option<String> {
        candidates.iter().find_map(|c| {
            known.iter().find(|k| k.contains(c)).cloned().or_else(|| {
                // The Anthropic family is reachable even when the list wasn't fetched.
                (c.starts_with("claude-") && parent_model.contains("claude"))
                    .then(|| (*c).to_owned())
            })
        })
    };
    let m = parent_model.to_ascii_lowercase();
    let found = match wanted {
        "small" if m.contains("claude") => pick(&["claude-haiku-5-5", "claude-haiku-4-5", "haiku"]),
        "small" if m.contains("gpt") => pick(&["gpt-5-mini", "gpt-5.1-mini", "mini"]),
        "small" if m.contains("gemini") => pick(&["flash-lite", "flash"]),
        "small" => pick(&["mini", "flash", "small", "lite", "haiku"]),
        "haiku" => pick(&["claude-haiku-5-5", "claude-haiku-4-5", "haiku"]),
        "sonnet" => pick(&["claude-sonnet-5-5", "claude-sonnet-5", "sonnet"]),
        "opus" => pick(&["claude-opus-5-5", "claude-opus-5", "opus"]),
        other => pick(&[other]),
    };
    match found {
        Some(model) => format!("{provider}/{model}"),
        None => parent.to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn claude_code_definitions_load() {
        let root = std::env::temp_dir().join(format!("crack-agents-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(root.join(".git")).unwrap();
        std::fs::create_dir_all(root.join(".claude/agents")).unwrap();
        std::fs::write(
            root.join(".claude/agents/reviewer.md"),
            "---\nname: reviewer\ndescription: Reviews diffs.\ntools: Read, Grep, Bash\nmodel: haiku\n---\nBe picky.\n",
        )
        .unwrap();
        let defs = discover(&root);
        let reviewer = defs.iter().find(|d| d.name == "reviewer").unwrap();
        assert_eq!(
            reviewer.tools.as_deref(),
            Some(&["read".to_owned(), "grep".into(), "bash".into()][..])
        );
        assert_eq!(reviewer.model.as_deref(), Some("haiku"));
        assert_eq!(reviewer.prompt, "Be picky.");
        assert!(defs.iter().any(|d| d.name == "explore"));
        std::fs::remove_dir_all(root).unwrap();
    }
}
