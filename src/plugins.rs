//! Claude Code plugins and marketplaces, so plugins written for Claude Code work in
//! crack: their skills, slash commands, subagents, hooks and MCP servers.
//!
//! A marketplace is a repository with `.claude-plugin/marketplace.json` listing
//! plugins. A plugin is a folder with an optional `.claude-plugin/plugin.json` and the
//! usual `skills/`, `commands/`, `agents/`, `hooks/hooks.json` and `.mcp.json`.
//!
//! Everything lives under `~/.config/crack/plugins`: `marketplaces/<name>` (clones),
//! `cache/<marketplace>/<plugin>` (installed copies) and `state.json`. A plugin's own
//! data folder (`CLAUDE_PLUGIN_DATA`) is `<data dir>/plugin-data/<plugin>-<marketplace>`.
//!
//! Hooks and MCP servers run code, so a plugin's are used only after the user approved
//! that exact set (a fingerprint). An update that changes them needs approval again.
//! Skills, commands and agents are instructions and load once the plugin is enabled.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::catalog::{copy_tree, git};

/// Plugins can be large (some carry whole toolchains); refuse past this.
const MAX_PLUGIN_BYTES: u64 = 300 << 20;

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct State {
    #[serde(default)]
    pub marketplaces: BTreeMap<String, Marketplace>,
    /// Keyed `plugin@marketplace`.
    #[serde(default)]
    pub plugins: BTreeMap<String, Installed>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Marketplace {
    /// What the user added: `owner/repo`, a git URL or a local folder.
    pub source: String,
    /// Where its files are: a clone, or the local folder itself.
    pub dir: PathBuf,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Installed {
    pub dir: PathBuf,
    #[serde(default)]
    pub version: String,
    #[serde(default)]
    pub commit: String,
    #[serde(default = "yes")]
    pub enabled: bool,
    /// Fingerprint of the hook and MCP commands the user approved.
    #[serde(default)]
    pub trusted: Option<String>,
}

fn yes() -> bool {
    true
}

pub fn root() -> anyhow::Result<PathBuf> {
    Ok(crate::config::global_dir()
        .ok_or_else(|| anyhow::anyhow!("no config folder"))?
        .join("plugins"))
}

impl State {
    pub fn load() -> State {
        root()
            .ok()
            .and_then(|r| std::fs::read_to_string(r.join("state.json")).ok())
            .and_then(|t| serde_json::from_str(&t).ok())
            .unwrap_or_default()
    }

    pub fn save(&self) -> anyhow::Result<()> {
        let root = root()?;
        std::fs::create_dir_all(&root)?;
        let tmp = root.join("state.json.tmp");
        std::fs::write(&tmp, serde_json::to_string_pretty(self)?)?;
        std::fs::rename(tmp, root.join("state.json"))?;
        Ok(())
    }
}

// ---------------------------------------------------------------- marketplaces

/// A marketplace's `marketplace.json`, read loosely.
#[derive(Clone, Debug, Deserialize)]
pub struct MarketplaceFile {
    pub name: String,
    #[serde(default)]
    pub plugins: Vec<Value>,
    #[serde(default)]
    pub metadata: Value,
}

pub fn read_marketplace(dir: &Path) -> anyhow::Result<MarketplaceFile> {
    let path = dir.join(".claude-plugin/marketplace.json");
    let text = std::fs::read_to_string(&path)
        .map_err(|e| anyhow::anyhow!("no marketplace at {}: {e}", path.display()))?;
    let file: MarketplaceFile =
        serde_json::from_str(&text).map_err(|e| anyhow::anyhow!("{}: {e}", path.display()))?;
    anyhow::ensure!(
        valid_name(&file.name),
        "the marketplace name '{}' isn't valid",
        file.name
    );
    Ok(file)
}

fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 64
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
        && !name.starts_with('.')
}

/// `owner/repo` becomes a GitHub URL; URLs stay; anything else must be a local folder.
fn git_url(source: &str) -> Option<String> {
    if source.starts_with("https://") || source.starts_with("git@") || source.ends_with(".git") {
        return Some(source.to_owned());
    }
    let parts: Vec<&str> = source.split('/').collect();
    (parts.len() == 2
        && !Path::new(source).exists()
        && parts.iter().all(|p| !p.is_empty() && valid_name(p)))
    .then(|| format!("https://github.com/{source}.git"))
}

/// A scratch folder that is removed when dropped.
struct Scratch(PathBuf);

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn scratch(label: &str) -> anyhow::Result<Scratch> {
    let dir = root()?.join(format!(".download-{label}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir)?;
    Ok(Scratch(dir))
}

/// A shallow clone of `url` (at `reference`, a branch or tag, when given).
fn clone(url: &str, reference: Option<&str>, into: &Path) -> anyhow::Result<String> {
    let parent = into.parent().expect("clone target has a parent");
    let name = into.file_name().unwrap().to_string_lossy().into_owned();
    let mut args = vec!["clone", "--depth=1", "--no-tags", "--quiet"];
    if let Some(reference) = reference {
        args.extend(["--branch", reference]);
    }
    args.extend([url, name.as_str()]);
    git(parent, &args)?;
    git(into, &["rev-parse", "HEAD"])
}

/// Adds (or refreshes) a marketplace. Returns its name.
pub fn add_marketplace(source: &str) -> anyhow::Result<String> {
    let mut state = State::load();
    let (name, dir) = if let Some(url) = git_url(source) {
        let work = scratch("marketplace")?;
        let checkout = work.0.join("repo");
        clone(&url, None, &checkout)?;
        let file = read_marketplace(&checkout)?;
        let dir = root()?.join("marketplaces").join(&file.name);
        if dir.exists() {
            std::fs::remove_dir_all(&dir)?;
        }
        std::fs::create_dir_all(dir.parent().unwrap())?;
        std::fs::rename(&checkout, &dir)?;
        (file.name, dir)
    } else {
        let dir = crate::config::expand_home(source);
        let dir = std::fs::canonicalize(&dir)
            .map_err(|e| anyhow::anyhow!("{source}: not a repository or folder ({e})"))?;
        (read_marketplace(&dir)?.name, dir)
    };
    state.marketplaces.insert(
        name.clone(),
        Marketplace {
            source: source.to_owned(),
            dir,
        },
    );
    state.save()?;
    Ok(name)
}

pub fn update_marketplace(name: &str) -> anyhow::Result<()> {
    let state = State::load();
    let market = state
        .marketplaces
        .get(name)
        .ok_or_else(|| anyhow::anyhow!("no marketplace '{name}'"))?;
    if git_url(&market.source).is_some() {
        add_marketplace(&market.source.clone())?;
    }
    Ok(())
}

pub fn remove_marketplace(name: &str) -> anyhow::Result<()> {
    let mut state = State::load();
    let market = state
        .marketplaces
        .remove(name)
        .ok_or_else(|| anyhow::anyhow!("no marketplace '{name}'"))?;
    let installed: Vec<String> = state
        .plugins
        .keys()
        .filter(|k| k.ends_with(&format!("@{name}")))
        .cloned()
        .collect();
    anyhow::ensure!(
        installed.is_empty(),
        "remove its plugins first: {}",
        installed.join(", ")
    );
    // Only delete clones crack made, never a local folder the user pointed at.
    if market.dir.starts_with(root()?.join("marketplaces")) {
        let _ = std::fs::remove_dir_all(&market.dir);
    }
    state.save()
}

/// One plugin a marketplace offers.
#[derive(Clone, Debug)]
pub struct Listing {
    pub name: String,
    pub marketplace: String,
    pub description: String,
    pub version: String,
    pub entry: Value,
}

pub fn listings(state: &State) -> Vec<Listing> {
    let mut out = Vec::new();
    for (market, info) in &state.marketplaces {
        let Ok(file) = read_marketplace(&info.dir) else {
            continue;
        };
        for entry in file.plugins {
            let Some(name) = entry["name"].as_str() else {
                continue;
            };
            out.push(Listing {
                name: name.to_owned(),
                marketplace: market.clone(),
                description: entry["description"].as_str().unwrap_or("").to_owned(),
                version: entry["version"].as_str().unwrap_or("").to_owned(),
                entry: entry.clone(),
            });
        }
    }
    out
}

// --------------------------------------------------------------------- install

/// `plugin@marketplace`, or just `plugin` when only one marketplace has it.
pub fn resolve(state: &State, spec: &str) -> anyhow::Result<Listing> {
    let (name, market) = match spec.split_once('@') {
        Some((n, m)) => (n, Some(m)),
        None => (spec, None),
    };
    let found: Vec<Listing> = listings(state)
        .into_iter()
        .filter(|l| l.name == name && market.is_none_or(|m| l.marketplace == m))
        .collect();
    match found.len() {
        0 => anyhow::bail!("no plugin '{spec}' in your marketplaces (crack plugin list)"),
        1 => Ok(found.into_iter().next().unwrap()),
        _ => anyhow::bail!(
            "'{name}' is in several marketplaces; say which: {}",
            found
                .iter()
                .map(|l| format!("{}@{}", l.name, l.marketplace))
                .collect::<Vec<_>>()
                .join(", ")
        ),
    }
}

/// Fetches a plugin into a scratch folder; returns (folder, commit).
fn fetch(listing: &Listing, market_dir: &Path, work: &Path) -> anyhow::Result<(PathBuf, String)> {
    let source = &listing.entry["source"];
    let into = work.join("plugin");
    match source {
        Value::String(path) => {
            let plugin_root = read_marketplace(market_dir)
                .ok()
                .and_then(|m| m.metadata["pluginRoot"].as_str().map(str::to_owned));
            let relative = if path.starts_with("./") || path == "." {
                path.clone()
            } else {
                // A bare name resolves under metadata.pluginRoot.
                format!("{}/{path}", plugin_root.unwrap_or_else(|| ".".into()))
            };
            anyhow::ensure!(
                !relative.split('/').any(|p| p == ".."),
                "plugin source '{path}' leaves the marketplace"
            );
            let from = market_dir.join(&relative);
            anyhow::ensure!(from.is_dir(), "plugin folder {relative} is missing");
            copy_tree(&from, &into, &mut 0, MAX_PLUGIN_BYTES)?;
            let commit = git(market_dir, &["rev-parse", "HEAD"]).unwrap_or_default();
            Ok((into, commit))
        }
        Value::Object(map) => {
            let kind = map.get("source").and_then(Value::as_str).unwrap_or("");
            let reference = map.get("ref").and_then(Value::as_str);
            let url = match kind {
                "github" => format!(
                    "https://github.com/{}.git",
                    map.get("repo").and_then(Value::as_str).unwrap_or("")
                ),
                "url" | "git-subdir" => map
                    .get("url")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_owned(),
                other => anyhow::bail!(
                    "plugin source type '{other}' isn't supported yet (github, url and git-subdir are)"
                ),
            };
            let checkout = work.join("checkout");
            let commit = clone(&url, reference, &checkout)?;
            if let Some(sha) = map.get("sha").and_then(Value::as_str) {
                anyhow::ensure!(
                    commit.starts_with(sha) || sha.starts_with(&commit),
                    "the repository is at {commit}, not the pinned {sha}"
                );
            }
            let from = match map.get("path").and_then(Value::as_str) {
                Some(sub) if kind == "git-subdir" => {
                    anyhow::ensure!(!sub.split('/').any(|p| p == ".."), "bad subdir {sub}");
                    checkout.join(sub)
                }
                _ => checkout.clone(),
            };
            copy_tree(&from, &into, &mut 0, MAX_PLUGIN_BYTES)?;
            Ok((into, commit))
        }
        _ => anyhow::bail!("plugin '{}' has no source", listing.name),
    }
}

/// What installing or updating would run, for the user to approve.
#[derive(Clone, Debug, Default)]
pub struct Review {
    pub hooks: Vec<String>,
    pub mcp: Vec<String>,
    pub fingerprint: String,
}

impl Review {
    pub fn runs_code(&self) -> bool {
        !self.hooks.is_empty() || !self.mcp.is_empty()
    }

    pub fn describe(&self) -> String {
        let mut out = String::new();
        for hook in &self.hooks {
            out.push_str(&format!("  hook  {hook}\n"));
        }
        for server in &self.mcp {
            out.push_str(&format!("  MCP   {server}\n"));
        }
        out
    }
}

/// The hook and MCP commands a plugin folder would run.
pub fn review(dir: &Path, entry: &Value) -> Review {
    let components = Components::read(dir, entry);
    let mut hooks: Vec<String> = Vec::new();
    for (event, matchers) in &components.hooks {
        for m in matchers {
            for command in m
                .command
                .iter()
                .chain(m.hooks.iter().map(|h| &h.command))
                .filter(|c| !c.is_empty())
            {
                hooks.push(format!("{event}: {command}"));
            }
        }
    }
    let mut mcp: Vec<String> = components
        .mcp
        .iter()
        .map(|(name, server)| match (&server.url, &server.command) {
            (Some(url), _) => format!("{name}: {url}"),
            (None, Some(command)) => format!("{name}: {command} {}", server.args.join(" ")),
            _ => name.clone(),
        })
        .collect();
    hooks.sort();
    mcp.sort();
    let fingerprint = {
        use sha2::Digest;
        let mut hash = sha2::Sha256::new();
        for line in hooks.iter().chain(&mcp) {
            hash.update(line.as_bytes());
            hash.update(b"\n");
        }
        hex::encode(hash.finalize())
    };
    Review {
        hooks,
        mcp,
        fingerprint,
    }
}

/// A plugin fetched and waiting for the user's approval before it's installed.
pub struct Pending {
    pub key: String,
    pub listing: Listing,
    pub review: Review,
    pub version: String,
    commit: String,
    work: Scratch,
}

/// Fetches a plugin (install or update) and reviews what it would run. Nothing is
/// installed until `commit_install`.
pub fn prepare(spec: &str) -> anyhow::Result<Pending> {
    let state = State::load();
    let listing = resolve(&state, spec)?;
    let market = &state.marketplaces[&listing.marketplace];
    let work = scratch(&listing.name)?;
    let (dir, commit) = fetch(&listing, &market.dir, &work.0)?;
    let manifest = manifest(&dir, &listing.entry);
    let version = manifest["version"]
        .as_str()
        .map(str::to_owned)
        .unwrap_or_else(|| commit.chars().take(12).collect());
    let review = review(&dir, &listing.entry);
    Ok(Pending {
        key: format!("{}@{}", listing.name, listing.marketplace),
        listing,
        review,
        version,
        commit,
        work,
    })
}

/// Installs a prepared plugin. `approved` says the user accepted its hooks and MCP
/// servers (when it has any); without approval they stay off until trusted.
pub fn commit_install(pending: Pending, approved: bool) -> anyhow::Result<String> {
    let mut state = State::load();
    let dest = root()?
        .join("cache")
        .join(&pending.listing.marketplace)
        .join(&pending.listing.name);
    std::fs::create_dir_all(dest.parent().unwrap())?;
    // The entry (for inline manifests) travels with the copy.
    std::fs::write(
        pending.work.0.join("plugin").join(".crack-entry.json"),
        serde_json::to_string_pretty(&pending.listing.entry)?,
    )?;
    let updating = dest.exists();
    if updating {
        std::fs::remove_dir_all(&dest)?;
    }
    std::fs::rename(pending.work.0.join("plugin"), &dest)?;
    let previous = state.plugins.get(&pending.key);
    let enabled = previous.is_none_or(|p| p.enabled);
    let trusted = if !pending.review.runs_code() || approved {
        Some(pending.review.fingerprint.clone())
    } else {
        // An unchanged set stays trusted across updates.
        previous
            .and_then(|p| p.trusted.clone())
            .filter(|t| *t == pending.review.fingerprint)
    };
    state.plugins.insert(
        pending.key.clone(),
        Installed {
            dir: dest,
            version: pending.version.clone(),
            commit: pending.commit.clone(),
            enabled,
            trusted,
        },
    );
    state.save()?;
    Ok(format!(
        "{} {} {}.",
        if updating { "Updated" } else { "Installed" },
        pending.key,
        pending.version
    ))
}

pub fn uninstall(spec: &str) -> anyhow::Result<String> {
    let mut state = State::load();
    let key = installed_key(&state, spec)?;
    let plugin = state.plugins.remove(&key).unwrap();
    if plugin.dir.starts_with(root()?.join("cache")) {
        let _ = std::fs::remove_dir_all(&plugin.dir);
    }
    state.save()?;
    Ok(format!("Removed {key}."))
}

pub fn set_enabled(spec: &str, enabled: bool) -> anyhow::Result<String> {
    let mut state = State::load();
    let key = installed_key(&state, spec)?;
    state.plugins.get_mut(&key).unwrap().enabled = enabled;
    state.save()?;
    Ok(format!(
        "{} {key}.",
        if enabled { "Enabled" } else { "Disabled" }
    ))
}

/// Approves an installed plugin's current hooks and MCP servers.
pub fn trust(spec: &str) -> anyhow::Result<Review> {
    let mut state = State::load();
    let key = installed_key(&state, spec)?;
    let plugin = state.plugins.get_mut(&key).unwrap();
    let review = review(&plugin.dir, &entry_of(&plugin.dir));
    plugin.trusted = Some(review.fingerprint.clone());
    state.save()?;
    Ok(review)
}

fn installed_key(state: &State, spec: &str) -> anyhow::Result<String> {
    if state.plugins.contains_key(spec) {
        return Ok(spec.to_owned());
    }
    let matches: Vec<&String> = state
        .plugins
        .keys()
        .filter(|k| k.split('@').next() == Some(spec))
        .collect();
    match matches.as_slice() {
        [one] => Ok((*one).clone()),
        [] => anyhow::bail!("'{spec}' isn't installed"),
        _ => anyhow::bail!("several plugins are named '{spec}'; use name@marketplace"),
    }
}

// ------------------------------------------------------------------ components

fn entry_of(dir: &Path) -> Value {
    std::fs::read_to_string(dir.join(".crack-entry.json"))
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or(Value::Null)
}

/// `plugin.json`, or the marketplace entry when the plugin has none.
fn manifest(dir: &Path, entry: &Value) -> Value {
    std::fs::read_to_string(dir.join(".claude-plugin/plugin.json"))
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or_else(|| entry.clone())
}

/// Paths a manifest field names (`"./x"` or `["./x", "./y"]`), inside the plugin.
fn paths(dir: &Path, value: &Value) -> Vec<PathBuf> {
    let list: Vec<&str> = match value {
        Value::String(s) => vec![s.as_str()],
        Value::Array(a) => a.iter().filter_map(Value::as_str).collect(),
        _ => Vec::new(),
    };
    list.into_iter()
        .filter(|p| !p.split('/').any(|s| s == ".."))
        .map(|p| dir.join(p.trim_start_matches("./")))
        .filter(|p| p.exists())
        .collect()
}

/// A plugin's parts, read from its folder.
#[derive(Debug, Default)]
pub struct Components {
    pub skill_dirs: Vec<PathBuf>,
    pub command_paths: Vec<PathBuf>,
    pub agent_paths: Vec<PathBuf>,
    pub hooks: crate::hooks::HooksConfig,
    pub mcp: BTreeMap<String, crate::mcp::McpServerConfig>,
}

impl Components {
    pub fn read(dir: &Path, entry: &Value) -> Components {
        let manifest = manifest(dir, entry);
        let mut out = Components::default();
        // Skills add to the default folder; commands and agents replace theirs.
        if dir.join("skills").is_dir() {
            out.skill_dirs.push(dir.join("skills"));
        }
        out.skill_dirs.extend(paths(dir, &manifest["skills"]));
        out.command_paths = if manifest.get("commands").is_some_and(|v| !v.is_object()) {
            paths(dir, &manifest["commands"])
        } else {
            vec![dir.join("commands")]
        };
        out.agent_paths = if manifest.get("agents").is_some() {
            paths(dir, &manifest["agents"])
        } else {
            vec![dir.join("agents")]
        };
        // Hooks and MCP servers merge from every source.
        let mut hook_sources: Vec<Value> = Vec::new();
        if let Some(v) = read_json(&dir.join("hooks/hooks.json")) {
            hook_sources.push(v);
        }
        match &manifest["hooks"] {
            Value::Object(_) => hook_sources.push(manifest["hooks"].clone()),
            Value::String(_) | Value::Array(_) => {
                for item in manifest["hooks"]
                    .as_array()
                    .cloned()
                    .unwrap_or_else(|| vec![manifest["hooks"].clone()])
                {
                    match item {
                        Value::Object(_) => hook_sources.push(item),
                        Value::String(_) => {
                            for path in paths(dir, &item) {
                                if path != dir.join("hooks/hooks.json")
                                    && let Some(v) = read_json(&path)
                                {
                                    hook_sources.push(v);
                                }
                            }
                        }
                        _ => {}
                    }
                }
            }
            _ => {}
        }
        for source in hook_sources {
            // Files wrap the events in "hooks"; inline manifests may not.
            let events = source.get("hooks").cloned().unwrap_or(source);
            let Value::Object(events) = events else {
                continue;
            };
            for (event, matchers) in events {
                let Value::Array(matchers) = matchers else {
                    continue;
                };
                for m in matchers {
                    if let Ok(parsed) = serde_json::from_value::<crate::hooks::HookMatcher>(m) {
                        out.hooks.entry(event.clone()).or_default().push(parsed);
                    }
                }
            }
        }
        let mut mcp_sources: Vec<Value> = Vec::new();
        if let Some(v) = read_json(&dir.join(".mcp.json")) {
            mcp_sources.push(v);
        }
        match &manifest["mcpServers"] {
            Value::Object(_) => mcp_sources.push(manifest["mcpServers"].clone()),
            other => {
                for path in paths(dir, other) {
                    if let Some(v) = read_json(&path) {
                        mcp_sources.push(v);
                    }
                }
            }
        }
        for source in mcp_sources {
            let servers = source.get("mcpServers").cloned().unwrap_or(source);
            let Value::Object(servers) = servers else {
                continue;
            };
            for (name, server) in servers {
                if let Ok(parsed) = serde_json::from_value::<crate::mcp::McpServerConfig>(server) {
                    out.mcp.insert(name, parsed);
                }
            }
        }
        out
    }
}

fn read_json(path: &Path) -> Option<Value> {
    serde_json::from_str(&std::fs::read_to_string(path).ok()?).ok()
}

/// The environment a plugin's hooks, MCP servers and commands see.
pub fn plugin_env(key: &str, dir: &Path) -> Vec<(String, String)> {
    let id: String = key
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '_' | '-') {
                c
            } else {
                '-'
            }
        })
        .collect();
    let data = crate::config::data_dir().join("plugin-data").join(id);
    let _ = std::fs::create_dir_all(&data);
    vec![
        ("CLAUDE_PLUGIN_ROOT".into(), dir.display().to_string()),
        ("CLAUDE_PLUGIN_DATA".into(), data.display().to_string()),
        ("CRACK_PLUGIN_ROOT".into(), dir.display().to_string()),
    ]
}

/// Replaces `${VAR}` for the plugin variables.
pub fn substitute(text: &str, env: &[(String, String)]) -> String {
    let mut out = text.to_owned();
    for (key, value) in env {
        out = out.replace(&format!("${{{key}}}"), value);
    }
    out
}

/// An enabled plugin, ready to load.
pub struct Loaded {
    pub key: String,
    pub name: String,
    pub components: Components,
    /// Its hooks and MCP servers are approved as they are now.
    pub trusted: bool,
    pub env: Vec<(String, String)>,
}

pub fn enabled() -> Vec<Loaded> {
    let state = State::load();
    state
        .plugins
        .iter()
        .filter(|(_, p)| p.enabled && p.dir.is_dir())
        .map(|(key, p)| {
            let entry = entry_of(&p.dir);
            let components = Components::read(&p.dir, &entry);
            let fingerprint = review(&p.dir, &entry).fingerprint;
            Loaded {
                key: key.clone(),
                name: key.split('@').next().unwrap_or(key).to_owned(),
                trusted: p.trusted.as_deref() == Some(fingerprint.as_str()),
                env: plugin_env(key, &p.dir),
                components,
            }
        })
        .collect()
}

/// Adds enabled plugins' skills, hooks and MCP servers to the config. Commands and
/// agents are read where they're used (`commands`, `subagents`).
pub fn apply(config: &mut crate::config::Config) {
    for plugin in enabled() {
        for dir in &plugin.components.skill_dirs {
            config.skills.push(dir.display().to_string());
        }
        if !plugin.trusted {
            continue;
        }
        for (event, matchers) in &plugin.components.hooks {
            for m in matchers {
                let mut m = m.clone();
                m.plugin = Some(plugin.key.clone());
                m.env = plugin.env.clone();
                config.hooks.entry(event.clone()).or_default().push(m);
            }
        }
        for (name, server) in &plugin.components.mcp {
            let mut server = server.clone();
            server.command = server.command.map(|c| substitute(&c, &plugin.env));
            server.args = server
                .args
                .iter()
                .map(|a| substitute(a, &plugin.env))
                .collect();
            server.url = server.url.map(|u| substitute(&u, &plugin.env));
            for (k, v) in &plugin.env {
                server.env.entry(k.clone()).or_insert_with(|| v.clone());
            }
            config
                .mcp
                .insert(format!("plugin_{}_{name}", plugin.name), server);
        }
    }
}

/// Installed plugins whose hooks or MCP servers wait for approval.
pub fn untrusted() -> Vec<(String, Review)> {
    let state = State::load();
    state
        .plugins
        .iter()
        .filter(|(_, p)| p.enabled && p.dir.is_dir())
        .filter_map(|(key, p)| {
            let review = review(&p.dir, &entry_of(&p.dir));
            (review.runs_code() && p.trusted.as_deref() != Some(review.fingerprint.as_str()))
                .then(|| (key.clone(), review))
        })
        .collect()
}

/// `crack plugin list`.
pub fn report() -> String {
    let state = State::load();
    let mut out = String::new();
    if state.marketplaces.is_empty() {
        out.push_str(
            "No marketplaces yet. Add one: crack plugin marketplace add <owner/repo|url|folder>\n",
        );
        return out;
    }
    out.push_str("Marketplaces:\n");
    for (name, m) in &state.marketplaces {
        out.push_str(&format!("  {name:<20} {}\n", m.source));
    }
    let waiting: Vec<String> = untrusted().into_iter().map(|(k, _)| k).collect();
    out.push_str("\nPlugins (install with `crack plugin install <name>[@marketplace]`):\n");
    for l in listings(&state) {
        let key = format!("{}@{}", l.name, l.marketplace);
        let status = match state.plugins.get(&key) {
            None => String::new(),
            Some(p) if !p.enabled => format!("  [installed {}, disabled]", p.version),
            Some(p) if waiting.contains(&key) => {
                format!(
                    "  [installed {}, hooks need approval: crack plugin trust {key}]",
                    p.version
                )
            }
            Some(p) => format!("  [installed {}]", p.version),
        };
        out.push_str(&format!("  {key:<32} {}{status}\n", l.description));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A plugin folder with every kind of part, in Claude Code's layout.
    fn sample(dir: &Path) {
        std::fs::create_dir_all(dir.join(".claude-plugin")).unwrap();
        std::fs::create_dir_all(dir.join("skills/tidy")).unwrap();
        std::fs::create_dir_all(dir.join("commands")).unwrap();
        std::fs::create_dir_all(dir.join("hooks")).unwrap();
        std::fs::write(
            dir.join(".claude-plugin/plugin.json"),
            r#"{"name": "demo", "version": "1.2.0",
                "hooks": {"UserPromptSubmit": [{"hooks": [{"type": "command", "command": "echo inline"}]}]}}"#,
        )
        .unwrap();
        std::fs::write(
            dir.join("skills/tidy/SKILL.md"),
            "---\nname: tidy\ndescription: Tidy up\n---\nTidy.",
        )
        .unwrap();
        std::fs::write(dir.join("commands/hello.md"), "Say hello to $ARGUMENTS.").unwrap();
        std::fs::write(
            dir.join("hooks/hooks.json"),
            r#"{"hooks": {"SessionStart": [{"matcher": "startup|clear", "hooks": [
                {"type": "command", "command": "\"${CLAUDE_PLUGIN_ROOT}/hooks/start\""}]}],
                "SessionEnd": [{"hooks": [{"type": "command", "command": "true"}]}]}}"#,
        )
        .unwrap();
        std::fs::write(
            dir.join(".mcp.json"),
            r#"{"mcpServers": {"tool": {"command": "${CLAUDE_PLUGIN_ROOT}/bin/server", "args": ["--x"]}}}"#,
        )
        .unwrap();
    }

    #[test]
    fn reads_every_part_and_fingerprints_what_runs() {
        let dir = std::env::temp_dir().join(format!("crack-plugin-{}", uuid::Uuid::new_v4()));
        sample(&dir);
        let parts = Components::read(&dir, &Value::Null);
        assert_eq!(parts.skill_dirs, vec![dir.join("skills")]);
        assert_eq!(parts.command_paths, vec![dir.join("commands")]);
        // File hooks and inline manifest hooks merge.
        assert!(parts.hooks.contains_key("SessionStart"));
        assert!(parts.hooks.contains_key("UserPromptSubmit"));
        assert!(parts.mcp.contains_key("tool"));

        let review = review(&dir, &Value::Null);
        assert!(review.runs_code());
        assert_eq!(review.hooks.len(), 3);
        assert_eq!(review.mcp.len(), 1);
        // Changing a hook command changes the fingerprint.
        std::fs::write(
            dir.join("hooks/hooks.json"),
            r#"{"hooks": {"SessionStart": [{"hooks": [{"type": "command", "command": "curl evil"}]}]}}"#,
        )
        .unwrap();
        assert_ne!(
            review.fingerprint,
            super::review(&dir, &Value::Null).fingerprint
        );

        let env = plugin_env("demo@market", &dir);
        assert_eq!(
            substitute("${CLAUDE_PLUGIN_ROOT}/bin/server", &env),
            format!("{}/bin/server", dir.display())
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn sources_resolve_to_git_urls_or_folders() {
        assert_eq!(
            git_url("obra/superpowers").as_deref(),
            Some("https://github.com/obra/superpowers.git")
        );
        assert_eq!(
            git_url("https://example.com/x.git").as_deref(),
            Some("https://example.com/x.git")
        );
        assert!(git_url("./local-folder").is_none());
        assert!(git_url("a/b/c").is_none());
    }
}
