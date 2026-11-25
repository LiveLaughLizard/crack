//! Extensions: a short, curated list of popular third-party skill packs and MCP servers
//! that install with one click (`/extensions`) or `crack extensions install <id>`.
//! Nothing is installed by default.
//!
//! Skills are fetched from their own repositories at install time and never bundled
//! with crack. Each install keeps the project's license file next to the skills. Every
//! entry here is under a permissive license (MIT or Apache-2.0).

use std::path::{Path, PathBuf};

pub enum Source {
    /// Skill folders (each with a `SKILL.md`) from a GitHub repository. Each path is a
    /// skill folder or a folder of skill folders.
    Skills {
        repo: &'static str,
        paths: &'static [&'static str],
    },
    /// An MCP server added to the user's config.
    Mcp {
        url: Option<&'static str>,
        command: &'static [&'static str],
    },
}

pub struct Entry {
    pub id: &'static str,
    pub name: &'static str,
    pub author: &'static str,
    pub description: &'static str,
    pub license: &'static str,
    pub homepage: &'static str,
    pub source: Source,
}

impl Entry {
    pub fn kind(&self) -> &'static str {
        match self.source {
            Source::Skills { .. } => "skills",
            Source::Mcp { .. } => "MCP server",
        }
    }
}

/// Checked against each repository's license (November 2026).
pub const CATALOG: &[Entry] = &[
    Entry {
        id: "caveman",
        name: "Caveman",
        author: "Julius Brussee",
        description: "Terse, caveman-style replies that cut output tokens; plus terse commits and reviews.",
        license: "Apache-2.0",
        homepage: "https://github.com/JuliusBrussee/caveman",
        source: Source::Skills {
            repo: "JuliusBrussee/caveman",
            paths: &["skills"],
        },
    },
    Entry {
        id: "ponytail",
        name: "Ponytail",
        author: "Dietrich Gebert",
        description: "Write less code: reuse what exists, the standard library and the platform first.",
        license: "MIT",
        homepage: "https://github.com/DietrichGebert/ponytail",
        source: Source::Skills {
            repo: "DietrichGebert/ponytail",
            paths: &["skills"],
        },
    },
    Entry {
        id: "superpowers",
        name: "Superpowers",
        author: "Jesse Vincent",
        description: "A development method: brainstorm, plan, test first, debug systematically, verify.",
        license: "MIT",
        homepage: "https://github.com/obra/superpowers",
        source: Source::Skills {
            repo: "obra/superpowers",
            paths: &["skills"],
        },
    },
    Entry {
        id: "frontend-design",
        name: "Frontend Design",
        author: "Anthropic",
        description: "Distinctive, production-grade web interfaces instead of generic-looking ones.",
        license: "Apache-2.0",
        homepage: "https://github.com/anthropics/skills/tree/main/skills/frontend-design",
        source: Source::Skills {
            repo: "anthropics/skills",
            paths: &["skills/frontend-design"],
        },
    },
    Entry {
        id: "anthropic-skills",
        name: "Anthropic skills",
        author: "Anthropic",
        description: "Anthropic's public skills: documents (docx, pdf, pptx, xlsx), skill-creator, MCP builder, web testing and more.",
        license: "Apache-2.0",
        homepage: "https://github.com/anthropics/skills",
        source: Source::Skills {
            repo: "anthropics/skills",
            paths: &["skills"],
        },
    },
    Entry {
        id: "mattpocock",
        name: "Matt Pocock's skills",
        author: "Matt Pocock",
        description: "Engineering and productivity skills: TDD, specs, triage, code review, grilling a plan.",
        license: "MIT",
        homepage: "https://github.com/mattpocock/skills",
        source: Source::Skills {
            repo: "mattpocock/skills",
            paths: &["skills/engineering", "skills/productivity"],
        },
    },
    Entry {
        id: "agent-skills",
        name: "Agent Skills",
        author: "Addy Osmani",
        description: "Production engineering skills: specs, incremental work, security, performance, shipping.",
        license: "MIT",
        homepage: "https://github.com/addyosmani/agent-skills",
        source: Source::Skills {
            repo: "addyosmani/agent-skills",
            paths: &["skills"],
        },
    },
    Entry {
        id: "context7",
        name: "Context7",
        author: "Upstash",
        description: "Up-to-date library documentation and examples, looked up on demand (hosted server).",
        license: "MIT",
        homepage: "https://github.com/upstash/context7",
        source: Source::Mcp {
            url: Some("https://mcp.context7.com/mcp"),
            command: &[],
        },
    },
    Entry {
        id: "playwright",
        name: "Playwright",
        author: "Microsoft",
        description: "Drive a real browser: open pages, click, type, read and screenshot. Needs Node.js.",
        license: "Apache-2.0",
        homepage: "https://github.com/microsoft/playwright-mcp",
        source: Source::Mcp {
            url: None,
            command: &["npx", "-y", "@playwright/mcp@latest"],
        },
    },
];

pub fn find(id: &str) -> Option<&'static Entry> {
    CATALOG.iter().find(|e| e.id == id)
}

/// What crack writes next to an installed pack, so it can update and remove it.
const MANIFEST: &str = ".crack-extension.json";

/// Where skill packs go: `~/.config/crack/skills/<id>`, which crack already searches.
fn pack_dir(id: &str) -> anyhow::Result<PathBuf> {
    Ok(crate::config::global_dir()
        .ok_or_else(|| anyhow::anyhow!("no config folder"))?
        .join("skills")
        .join(id))
}

#[derive(Debug, PartialEq)]
pub enum Status {
    NotInstalled,
    /// Skills: the commit and the skill names.
    Skills {
        commit: String,
        skills: Vec<String>,
    },
    Mcp,
}

pub fn status(entry: &Entry, config: &crate::config::Config) -> Status {
    match entry.source {
        Source::Mcp { .. } => {
            if config.mcp.contains_key(entry.id) {
                Status::Mcp
            } else {
                Status::NotInstalled
            }
        }
        Source::Skills { .. } => {
            let manifest = pack_dir(entry.id)
                .ok()
                .and_then(|d| std::fs::read_to_string(d.join(MANIFEST)).ok())
                .and_then(|t| serde_json::from_str::<serde_json::Value>(&t).ok());
            match manifest {
                Some(m) => Status::Skills {
                    commit: m["commit"].as_str().unwrap_or_default().to_owned(),
                    skills: m["skills"]
                        .as_array()
                        .map(|a| {
                            a.iter()
                                .filter_map(|s| s.as_str().map(str::to_owned))
                                .collect()
                        })
                        .unwrap_or_default(),
                },
                None => Status::NotInstalled,
            }
        }
    }
}

/// Installs (or updates) an entry. Blocking: it runs git. Returns what was done.
pub fn install(entry: &Entry) -> anyhow::Result<String> {
    match &entry.source {
        Source::Mcp { url, command } => {
            crate::config::set_user_mcp(entry.id, *url, command)?;
            Ok(format!(
                "Added the {} MCP server to your config.",
                entry.name
            ))
        }
        Source::Skills { repo, paths } => install_skills(entry, repo, paths),
    }
}

pub fn remove(entry: &Entry) -> anyhow::Result<String> {
    match entry.source {
        Source::Mcp { .. } => {
            crate::config::remove_user_mcp(entry.id)?;
            Ok(format!("Removed the {} MCP server.", entry.name))
        }
        Source::Skills { .. } => {
            let dir = pack_dir(entry.id)?;
            // Only a folder crack installed: never one the user made with that name.
            anyhow::ensure!(
                dir.join(MANIFEST).is_file(),
                "{} wasn't installed by crack; remove it yourself if you want it gone",
                dir.display()
            );
            std::fs::remove_dir_all(&dir)?;
            Ok(format!("Removed {}.", entry.name))
        }
    }
}

/// A scratch folder removed when dropped.
struct Scratch(PathBuf);

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn git(dir: &Path, args: &[&str]) -> anyhow::Result<String> {
    let output = std::process::Command::new("git")
        .current_dir(dir)
        // Never prompt for credentials; check symlinks out as plain files.
        .env("GIT_TERMINAL_PROMPT", "0")
        .args([
            "-c",
            "core.symlinks=false",
            "-c",
            "core.hooksPath=/dev/null",
        ])
        .args(args)
        .output()
        .map_err(|e| anyhow::anyhow!("couldn't run git: {e}"))?;
    if !output.status.success() {
        anyhow::bail!(
            "git {}: {}",
            args.first().unwrap_or(&""),
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_owned())
}

fn install_skills(entry: &Entry, repo: &str, paths: &[&str]) -> anyhow::Result<String> {
    let dest = pack_dir(entry.id)?;
    if dest.exists() && !dest.join(MANIFEST).is_file() {
        anyhow::bail!(
            "{} already exists and wasn't installed by crack; move it first",
            dest.display()
        );
    }
    let parent = dest.parent().expect("pack dirs have a parent");
    std::fs::create_dir_all(parent)?;
    let scratch = Scratch(parent.join(format!(".{}-download-{}", entry.id, std::process::id())));
    let _ = std::fs::remove_dir_all(&scratch.0);
    std::fs::create_dir_all(&scratch.0)?;
    let clone = scratch.0.join("repo");

    // Only the skill folders (and the repository's top-level files, for the license).
    let url = format!("https://github.com/{repo}.git");
    git(
        &scratch.0,
        &[
            "clone",
            "--depth=1",
            "--filter=blob:none",
            "--sparse",
            "--no-tags",
            "--quiet",
            &url,
            "repo",
        ],
    )?;
    let mut sparse = vec!["sparse-checkout", "set"];
    sparse.extend(paths);
    git(&clone, &sparse)?;
    let commit = git(&clone, &["rev-parse", "HEAD"])?;

    let staging = scratch.0.join("staging");
    std::fs::create_dir_all(&staging)?;
    let mut names = Vec::new();
    for path in paths {
        let from = clone.join(path);
        let skill_dirs: Vec<PathBuf> = if from.join("SKILL.md").is_file() {
            vec![from]
        } else {
            let mut dirs: Vec<PathBuf> = std::fs::read_dir(&from)
                .map_err(|e| anyhow::anyhow!("{path} isn't in {repo}: {e}"))?
                .flatten()
                .map(|e| e.path())
                .filter(|p| p.join("SKILL.md").is_file())
                .collect();
            dirs.sort();
            dirs
        };
        for dir in skill_dirs {
            let name = dir
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default();
            copy_tree(&dir, &staging.join(&name), &mut 0)?;
            names.push(name);
        }
    }
    anyhow::ensure!(!names.is_empty(), "no skills found in {repo}");
    // The project's license travels with its skills.
    for file in std::fs::read_dir(&clone)?.flatten() {
        let name = file.file_name().to_string_lossy().into_owned();
        if name.to_ascii_uppercase().starts_with("LICENSE")
            || name.to_ascii_uppercase().starts_with("NOTICE")
        {
            std::fs::copy(file.path(), staging.join(&name))?;
        }
    }
    let manifest = serde_json::json!({
        "id": entry.id,
        "repo": repo,
        "commit": commit,
        "license": entry.license,
        "skills": names,
        "installed": chrono::Utc::now().to_rfc3339(),
    });
    std::fs::write(
        staging.join(MANIFEST),
        serde_json::to_string_pretty(&manifest)?,
    )?;

    let updating = dest.exists();
    if updating {
        std::fs::remove_dir_all(&dest)?;
    }
    std::fs::rename(&staging, &dest)?;
    Ok(format!(
        "{} {} ({} skills: {}). Source: {repo} at {}.",
        if updating { "Updated" } else { "Installed" },
        entry.name,
        names.len(),
        names.join(", "),
        &commit[..commit.len().min(8)]
    ))
}

/// Copies regular files and folders (no symlinks), up to 50 MB in all.
fn copy_tree(from: &Path, to: &Path, total: &mut u64) -> anyhow::Result<()> {
    std::fs::create_dir_all(to)?;
    for entry in std::fs::read_dir(from)?.flatten() {
        let kind = entry.file_type()?;
        let name = entry.file_name();
        if name == ".git" {
            continue;
        }
        if kind.is_dir() {
            copy_tree(&entry.path(), &to.join(&name), total)?;
        } else if kind.is_file() {
            *total += entry.metadata()?.len();
            anyhow::ensure!(
                *total < 50 << 20,
                "the skills are over 50 MB; not installing"
            );
            std::fs::copy(entry.path(), to.join(&name))?;
        }
    }
    Ok(())
}

/// `crack extensions`: the catalog with what's installed.
pub fn report(config: &crate::config::Config) -> String {
    let mut out = String::from("Extensions (install with `crack extensions install <id>`):\n\n");
    for entry in CATALOG {
        let state = match status(entry, config) {
            Status::NotInstalled => String::new(),
            Status::Mcp => "  [installed]".into(),
            Status::Skills { commit, skills } => format!(
                "  [installed: {} skills @ {}]",
                skills.len(),
                &commit[..commit.len().min(8)]
            ),
        };
        out.push_str(&format!(
            "  {:<17} {} ({}, {}, {}){state}\n  {:<17} {}\n  {:<17} {}\n\n",
            entry.id,
            entry.name,
            entry.kind(),
            entry.author,
            entry.license,
            "",
            entry.description,
            "",
            entry.homepage
        ));
    }
    out.push_str(
        "Skills are instructions the model follows, from third parties: read them before you\n\
         rely on them. They install to ~/.config/crack/skills/<id>.\n",
    );
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn catalog_entries_are_unique_and_permissive() {
        let mut ids: Vec<_> = CATALOG.iter().map(|e| e.id).collect();
        ids.sort();
        ids.dedup();
        assert_eq!(ids.len(), CATALOG.len());
        for entry in CATALOG {
            assert!(
                ["MIT", "Apache-2.0"].contains(&entry.license),
                "{}",
                entry.id
            );
        }
    }
}
