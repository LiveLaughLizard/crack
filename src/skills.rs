//! Skills: folders with a `SKILL.md` (YAML frontmatter `name` and `description`, then
//! instructions). The model sees each skill's name and description and loads the
//! full text with the `skill` tool; the user can invoke one with `/<name>`.
//!
//! Searched, nearest first (a nearer skill shadows a farther one of the same name):
//! `.crack/skills`, `.claude/skills` and `.agents/skills` in the working directory and
//! each parent up to the project root, then the same folders under `~/.config/crack`,
//! `~/.claude` and `~/.agents`, then the config file's `skills` entries.

use crate::config::{self, Config};
use crate::context::project_root;
use std::path::{Path, PathBuf};

#[derive(Clone, Debug)]
pub struct Skill {
    pub name: String,
    pub description: String,
    pub path: PathBuf,
    /// Only the user can invoke it; it isn't offered to the model.
    pub user_only: bool,
}

impl Skill {
    pub fn dir(&self) -> &Path {
        self.path.parent().unwrap_or(Path::new("."))
    }

    /// The skill's instructions as the model receives them.
    pub fn load(&self) -> anyhow::Result<String> {
        let text = std::fs::read_to_string(&self.path)?;
        let body = split_frontmatter(&text).1.trim();
        let dir = self.dir();
        let mut files: Vec<String> = ignore::WalkBuilder::new(dir)
            .max_depth(Some(3))
            .build()
            .flatten()
            .filter(|e| e.file_type().is_some_and(|t| t.is_file()) && e.path() != self.path)
            .filter_map(|e| {
                e.path()
                    .strip_prefix(dir)
                    .ok()
                    .map(|p| p.display().to_string())
            })
            .take(100)
            .collect();
        files.sort();
        let mut out = format!(
            "<skill_content name=\"{}\">\n# Skill: {}\n\n{body}\n\nBase directory for this skill: {}\n\
             Relative paths in this skill are relative to that directory.\n",
            self.name,
            self.name,
            dir.display()
        );
        if !files.is_empty() {
            out.push_str("<skill_files>\n");
            for file in files {
                out.push_str(&format!("<file>{file}</file>\n"));
            }
            out.push_str("</skill_files>\n");
        }
        out.push_str("</skill_content>");
        Ok(out)
    }
}

/// Splits `---\nfrontmatter\n---\nbody`.
pub(crate) fn split_frontmatter(text: &str) -> (&str, &str) {
    let trimmed = text.trim_start_matches('\u{feff}');
    if let Some(rest) = trimmed.strip_prefix("---") {
        let rest = rest
            .trim_start_matches(['\r'])
            .strip_prefix('\n')
            .unwrap_or(rest);
        if let Some(end) = rest.find("\n---") {
            let front = &rest[..end];
            let after = &rest[end + 4..];
            let body = after.split_once('\n').map_or("", |(_, b)| b);
            return (front, body);
        }
    }
    ("", trimmed)
}

/// Reads simple `key: value` frontmatter, including `>`/`|` blocks and indented
/// continuation lines.
pub(crate) fn frontmatter_value(front: &str, key: &str) -> Option<String> {
    let mut lines = front.lines().peekable();
    while let Some(line) = lines.next() {
        let Some(rest) = line.strip_prefix(key).and_then(|r| r.strip_prefix(':')) else {
            continue;
        };
        let mut value = rest.trim().to_owned();
        if matches!(value.as_str(), ">" | "|" | ">-" | "|-" | "") {
            value.clear();
        }
        while let Some(next) = lines.peek() {
            if next.starts_with(' ') || next.starts_with('\t') {
                if !value.is_empty() {
                    value.push(' ');
                }
                value.push_str(next.trim());
                lines.next();
            } else {
                break;
            }
        }
        let value = value.trim();
        // YAML quoting: "..." takes backslash escapes, '...' doubles its quotes.
        let value = if value.len() >= 2 && value.starts_with('"') && value.ends_with('"') {
            value[1..value.len() - 1]
                .replace("\\\"", "\"")
                .replace("\\\\", "\\")
        } else if value.len() >= 2 && value.starts_with('\'') && value.ends_with('\'') {
            value[1..value.len() - 1].replace("''", "'")
        } else {
            value.to_owned()
        };
        return (!value.is_empty()).then_some(value);
    }
    None
}

fn parse(path: &Path) -> Option<Skill> {
    let text = std::fs::read_to_string(path).ok()?;
    let (front, body) = split_frontmatter(&text);
    let folder = path.parent()?.file_name()?.to_string_lossy().into_owned();
    let name = frontmatter_value(front, "name").unwrap_or(folder);
    let description = frontmatter_value(front, "description").unwrap_or_else(|| {
        body.lines()
            .map(str::trim)
            .find(|l| !l.is_empty() && !l.starts_with('#'))
            .unwrap_or("")
            .chars()
            .take(200)
            .collect()
    });
    let user_only = frontmatter_value(front, "disable-model-invocation").as_deref() == Some("true");
    Some(Skill {
        name,
        description,
        path: path.to_owned(),
        user_only,
    })
}

fn scan(dir: &Path, out: &mut Vec<Skill>) {
    if !dir.is_dir() {
        return;
    }
    let mut found: Vec<PathBuf> = walk_for_skills(dir, 0);
    found.sort();
    for path in found {
        if let Some(skill) = parse(&path)
            && !out.iter().any(|s| s.name == skill.name)
        {
            out.push(skill);
        }
    }
}

fn walk_for_skills(dir: &Path, depth: usize) -> Vec<PathBuf> {
    let mut found = Vec::new();
    let Ok(entries) = std::fs::read_dir(dir) else {
        return found;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            if entry.file_name().to_string_lossy().starts_with('.') || depth >= 4 {
                continue;
            }
            let skill_file = path.join("SKILL.md");
            if skill_file.is_file() {
                found.push(skill_file);
            } else {
                found.extend(walk_for_skills(&path, depth + 1));
            }
        }
    }
    found
}

/// Finds a skill by name. Plugins written for Claude Code name their skills
/// `plugin:skill`; without an exact match, the part after the last `:` is tried.
pub fn find<'a>(skills: &'a [Skill], name: &str) -> Option<&'a Skill> {
    skills.iter().find(|s| s.name == name).or_else(|| {
        let short = name.rsplit_once(':')?.1;
        skills.iter().find(|s| s.name == short)
    })
}

pub fn discover(cwd: &Path, config: &Config) -> Vec<Skill> {
    let mut skills = Vec::new();
    let root = project_root(cwd);
    for dir in cwd.ancestors() {
        for sub in [".crack/skills", ".claude/skills", ".agents/skills"] {
            scan(&dir.join(sub), &mut skills);
        }
        if dir == root {
            break;
        }
    }
    if let Some(global) = config::global_dir() {
        scan(&global.join("skills"), &mut skills);
    }
    if let Some(home) = dirs::home_dir() {
        scan(&home.join(".claude/skills"), &mut skills);
        scan(&home.join(".agents/skills"), &mut skills);
    }
    for extra in &config.skills {
        let path = config::expand_home(extra);
        let path = if path.is_absolute() {
            path
        } else {
            cwd.join(path)
        };
        if path.join("SKILL.md").is_file() {
            if let Some(skill) = parse(&path.join("SKILL.md"))
                && !skills.iter().any(|s| s.name == skill.name)
            {
                skills.push(skill);
            }
        } else {
            scan(&path, &mut skills);
        }
    }
    skills
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frontmatter_forms() {
        let text = "---\nname: \"my-skill\"\ndescription: >\n  Folded \\\"text\\\"\n  over lines.\nother: 'it''s'\n---\n# Body\n";
        let (front, body) = split_frontmatter(text);
        assert_eq!(
            frontmatter_value(front, "name").as_deref(),
            Some("my-skill")
        );
        assert_eq!(
            frontmatter_value(front, "description").as_deref(),
            Some("Folded \\\"text\\\" over lines.")
        );
        assert_eq!(frontmatter_value(front, "other").as_deref(), Some("it's"));
        assert_eq!(body.trim(), "# Body");
        assert_eq!(
            frontmatter_value("description: \"say \\\"hi\\\"\"", "description").as_deref(),
            Some("say \"hi\"")
        );
    }
}
