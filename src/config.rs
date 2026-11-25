//! Configuration: `~/.config/crack/config.toml`, overridden by the nearest
//! `.crack/config.toml` or `crack.toml` found walking up from the working directory.

use serde::Deserialize;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default)]
pub struct Config {
    /// Default model as `provider/model`.
    pub model: Option<String>,
    /// Default reasoning effort.
    pub effort: Option<String>,
    /// Default agent: `build` or `plan`.
    pub agent: Option<String>,
    /// Interface theme: `dark` (default) or `light`.
    pub theme: Option<String>,
    /// Output token limit per response.
    pub max_tokens: Option<u32>,
    /// Extra instruction files (paths, `~/` allowed) added to the system prompt.
    pub instructions: Vec<String>,
    /// Extra skill directories.
    pub skills: Vec<String>,
    /// Built-in providers to hide.
    pub disabled_providers: Vec<String>,
    /// What runs without asking (rules, edits, sandbox).
    pub permissions: crate::policy::PermissionsConfig,
    /// Per-model overrides: `[models."provider/model"] context = 131072, input_price = 0.5`.
    pub models: BTreeMap<String, crate::providers::models::ModelCaps>,
    /// Compact the conversation when it fills this share of the context window (0.8).
    pub compact_at: Option<f64>,
    /// Snapshot files around each turn for undo (default true).
    pub checkpoints: Option<bool>,
    /// Commands run at points in a turn, in Claude Code's format (`[[hooks.PreToolUse]]`).
    pub hooks: crate::hooks::HooksConfig,
    /// MCP servers, keyed by name: `command` + `args` (stdio) or `url` (HTTP).
    pub mcp: BTreeMap<String, crate::mcp::McpServerConfig>,
    /// Provider settings, keyed by provider id. Ids that match a built-in provider
    /// override its fields; new ids add providers.
    pub provider: BTreeMap<String, ProviderConfig>,
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default)]
pub struct ProviderConfig {
    /// `anthropic`, `openai`, `bedrock`, `vertex` or `azure`.
    pub kind: Option<String>,
    /// Display name.
    pub name: Option<String>,
    pub base_url: Option<String>,
    /// The key itself. Prefer `api_key_env`.
    pub api_key: Option<String>,
    /// Environment variables to read the key from, first set wins.
    pub api_key_env: Vec<String>,
    /// `bearer` (default), `x-api-key`, `api-key`, `google` or `none`.
    pub auth: Option<String>,
    pub headers: BTreeMap<String, String>,
    pub query: BTreeMap<String, String>,
    /// Models offered in the picker. OpenAI-compatible servers also list their own.
    pub models: Vec<String>,
    /// Ask the server for its model list (`GET /models`).
    pub discover: Option<bool>,
    /// `effort` (reasoning_effort), `nested` (reasoning.effort) or `none`.
    pub reasoning: Option<String>,
    /// Bedrock / Vertex region.
    pub region: Option<String>,
    /// Vertex project.
    pub project: Option<String>,
    /// SigV4 service name for Bedrock.
    pub sigv4_service: Option<String>,
    /// AWS profile for Bedrock (~/.aws; static keys or SSO).
    pub profile: Option<String>,
    /// Send earlier reasoning back as `reasoning_content` (DeepSeek, Kimi thinking mode).
    pub replay_reasoning: Option<bool>,
    /// Extra top-level request fields, e.g. `extra_body = { reasoning_split = true }`.
    pub extra_body: BTreeMap<String, serde_json::Value>,
    /// Send `max_tokens`, capped at this many output tokens.
    pub max_output_tokens: Option<u32>,
    /// A local server: shown only when it answers.
    pub local: Option<bool>,
}

impl ProviderConfig {
    /// Fills unset fields from `base`.
    pub fn merged_over(mut self, base: &ProviderConfig) -> ProviderConfig {
        macro_rules! inherit {
            ($($field:ident),*) => { $( if self.$field.is_none() { self.$field = base.$field.clone(); } )* };
        }
        inherit!(
            kind,
            name,
            base_url,
            api_key,
            auth,
            discover,
            reasoning,
            region,
            project,
            sigv4_service,
            profile,
            replay_reasoning,
            max_output_tokens,
            local
        );
        if self.api_key_env.is_empty() {
            self.api_key_env = base.api_key_env.clone();
        }
        if self.models.is_empty() {
            self.models = base.models.clone();
        }
        for (k, v) in &base.headers {
            self.headers.entry(k.clone()).or_insert_with(|| v.clone());
        }
        for (k, v) in &base.query {
            self.query.entry(k.clone()).or_insert_with(|| v.clone());
        }
        for (k, v) in &base.extra_body {
            self.extra_body
                .entry(k.clone())
                .or_insert_with(|| v.clone());
        }
        self
    }
}

impl Config {
    pub fn load(cwd: &Path) -> anyhow::Result<Config> {
        let mut config = Config::default();
        let mut files = Vec::new();
        if let Some(global) = global_dir() {
            files.push(global.join("config.toml"));
        }
        let project_file = project_config(cwd);
        if let Some(project) = &project_file {
            files.push(project.clone());
        }
        for file in files {
            if !file.is_file() {
                continue;
            }
            let text = std::fs::read_to_string(&file)?;
            let mut layer: Config =
                toml::from_str(&text).map_err(|e| anyhow::anyhow!("{}: {e}", file.display()))?;
            if project_file.as_ref() == Some(&file) {
                for hook in layer.hooks.values_mut().flatten() {
                    hook.from_project = true;
                }
            }
            config.overlay(layer);
        }
        // Keys saved from the Providers dialog.
        for (id, key) in load_credentials() {
            let provider = config.provider.entry(id).or_default();
            if provider.api_key.is_none() {
                provider.api_key = Some(key);
            }
        }
        Ok(config)
    }

    fn overlay(&mut self, layer: Config) {
        if layer.model.is_some() {
            self.model = layer.model;
        }
        if layer.effort.is_some() {
            self.effort = layer.effort;
        }
        if layer.agent.is_some() {
            self.agent = layer.agent;
        }
        if layer.theme.is_some() {
            self.theme = layer.theme;
        }
        if layer.max_tokens.is_some() {
            self.max_tokens = layer.max_tokens;
        }
        self.instructions.extend(layer.instructions);
        self.skills.extend(layer.skills);
        self.disabled_providers.extend(layer.disabled_providers);
        self.mcp.extend(layer.mcp);
        self.models.extend(layer.models);
        if layer.compact_at.is_some() {
            self.compact_at = layer.compact_at;
        }
        if layer.checkpoints.is_some() {
            self.checkpoints = layer.checkpoints;
        }
        for (event, hooks) in layer.hooks {
            self.hooks.entry(event).or_default().extend(hooks);
        }
        // Project rules add to global ones; settings override.
        let p = layer.permissions;
        self.permissions.allow.extend(p.allow);
        self.permissions.ask.extend(p.ask);
        self.permissions.deny.extend(p.deny);
        self.permissions.sandbox_writable.extend(p.sandbox_writable);
        macro_rules! take { ($($f:ident),*) => { $( if p.$f.is_some() { self.permissions.$f = p.$f; } )* } }
        take!(edits, bash, webfetch, sandbox, sandbox_network);
        for (id, provider) in layer.provider {
            let merged = match self.provider.remove(&id) {
                Some(existing) => provider.merged_over(&existing),
                None => provider,
            };
            self.provider.insert(id, merged);
        }
    }
}

/// `~/.config/crack` (or `$CRACK_CONFIG_DIR`).
pub fn global_dir() -> Option<PathBuf> {
    if let Ok(dir) = std::env::var("CRACK_CONFIG_DIR") {
        return Some(PathBuf::from(dir));
    }
    dirs::config_dir().map(|d| d.join("crack"))
}

/// `~/.local/share/crack` (the database, logs), or `$CRACK_DATA_DIR`.
pub fn data_dir() -> PathBuf {
    if let Ok(dir) = std::env::var("CRACK_DATA_DIR") {
        return PathBuf::from(dir);
    }
    dirs::data_dir()
        .unwrap_or_else(std::env::temp_dir)
        .join("crack")
}

fn project_config(cwd: &Path) -> Option<PathBuf> {
    let home = dirs::home_dir();
    for dir in cwd.ancestors() {
        for candidate in [dir.join(".crack/config.toml"), dir.join("crack.toml")] {
            if candidate.is_file() {
                return Some(candidate);
            }
        }
        if Some(dir) == home.as_deref() {
            break;
        }
    }
    None
}

/// Expands a leading `~/`.
pub fn expand_home(path: &str) -> PathBuf {
    match path.strip_prefix("~/") {
        Some(rest) => dirs::home_dir().unwrap_or_default().join(rest),
        None => PathBuf::from(path),
    }
}

/// `~/.config/crack/credentials.toml`: API keys saved from the interface, readable
/// only by the user.
pub fn credentials_path() -> Option<PathBuf> {
    global_dir().map(|d| d.join("credentials.toml"))
}

pub fn global_config_path() -> Option<PathBuf> {
    global_dir().map(|d| d.join("config.toml"))
}

/// Provider id -> saved API key.
pub fn load_credentials() -> Vec<(String, String)> {
    let Some(text) = credentials_path().and_then(|p| std::fs::read_to_string(p).ok()) else {
        return Vec::new();
    };
    let Ok(doc) = text.parse::<toml_edit::DocumentMut>() else {
        return Vec::new();
    };
    doc.iter()
        .filter_map(|(id, item)| {
            let key = item.get("api_key")?.as_str()?.to_owned();
            (!key.is_empty()).then(|| (id.to_owned(), key))
        })
        .collect()
}

/// Whether a key for `id` is saved in the credentials file.
pub fn has_saved_key(id: &str) -> bool {
    load_credentials().iter().any(|(p, _)| p == id)
}

fn read_doc(path: &Path) -> anyhow::Result<toml_edit::DocumentMut> {
    match std::fs::read_to_string(path) {
        Ok(text) => Ok(text
            .parse::<toml_edit::DocumentMut>()
            .map_err(|e| anyhow::anyhow!("{}: {e}", path.display()))?),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(toml_edit::DocumentMut::new()),
        Err(e) => Err(e.into()),
    }
}

fn write_private(path: &Path, text: &str) -> anyhow::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let tmp = path.with_extension("toml.tmp");
    std::fs::write(&tmp, text)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600))?;
    }
    std::fs::rename(tmp, path)?;
    Ok(())
}

/// A provider setting to save: `None` removes it.
pub enum Setting {
    Text(Option<String>),
    List(Vec<String>),
}

/// Saves provider settings to the global config (comments and other settings are
/// kept) and the key, when given, to the credentials file. An empty key removes it.
pub fn save_provider(
    id: &str,
    settings: &[(&str, Setting)],
    api_key: Option<&str>,
) -> anyhow::Result<()> {
    if !settings.is_empty() {
        let path = global_config_path().ok_or_else(|| anyhow::anyhow!("no config directory"))?;
        let mut doc = read_doc(&path)?;
        if !doc.contains_key("provider") {
            let mut table = toml_edit::Table::new();
            table.set_implicit(true);
            doc["provider"] = toml_edit::Item::Table(table);
        }
        let providers = doc["provider"]
            .as_table_mut()
            .ok_or_else(|| anyhow::anyhow!("`provider` in {} is not a table", path.display()))?;
        if !providers.contains_key(id) {
            providers.insert(id, toml_edit::Item::Table(toml_edit::Table::new()));
        }
        let table = providers[id]
            .as_table_mut()
            .ok_or_else(|| anyhow::anyhow!("provider.{id} is not a table"))?;
        for (key, setting) in settings {
            match setting {
                Setting::Text(Some(value)) if !value.trim().is_empty() => {
                    table[*key] = toml_edit::value(value.trim());
                }
                Setting::List(values) if !values.is_empty() => {
                    let mut array = toml_edit::Array::new();
                    for v in values {
                        array.push(v.as_str());
                    }
                    table[*key] = toml_edit::value(array);
                }
                _ => {
                    table.remove(key);
                }
            }
        }
        if table.is_empty() {
            providers.remove(id);
        }
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(&path, doc.to_string())?;
    }

    if let Some(key) = api_key {
        let path = credentials_path().ok_or_else(|| anyhow::anyhow!("no config directory"))?;
        let mut doc = read_doc(&path)?;
        if key.trim().is_empty() {
            doc.remove(id);
        } else {
            let mut table = toml_edit::Table::new();
            table["api_key"] = toml_edit::value(key.trim());
            doc[id] = toml_edit::Item::Table(table);
        }
        write_private(&path, &doc.to_string())?;
    }
    Ok(())
}

/// Removes a provider's saved key and settings.
pub fn forget_provider(id: &str) -> anyhow::Result<()> {
    if let Some(path) = global_config_path().filter(|p| p.is_file()) {
        let mut doc = read_doc(&path)?;
        if let Some(providers) = doc.get_mut("provider").and_then(|p| p.as_table_mut()) {
            providers.remove(id);
        }
        std::fs::write(&path, doc.to_string())?;
    }
    if let Some(path) = credentials_path().filter(|p| p.is_file()) {
        let mut doc = read_doc(&path)?;
        doc.remove(id);
        write_private(&path, &doc.to_string())?;
    }
    Ok(())
}

/// Adds a permission rule to the project's config (`.crack/config.toml`, or an
/// existing `crack.toml`), for "always allow".
pub fn add_permission_rule(cwd: &Path, list: &str, rule: &str) -> anyhow::Result<PathBuf> {
    let root = crate::context::project_root(cwd);
    let path = [root.join("crack.toml"), root.join(".crack/config.toml")]
        .into_iter()
        .find(|p| p.is_file())
        .unwrap_or_else(|| root.join(".crack/config.toml"));
    let mut doc = read_doc(&path)?;
    if !doc.contains_key("permissions") {
        doc["permissions"] = toml_edit::Item::Table(toml_edit::Table::new());
    }
    let table = doc["permissions"]
        .as_table_mut()
        .ok_or_else(|| anyhow::anyhow!("`permissions` in {} is not a table", path.display()))?;
    if !table.contains_key(list) {
        table[list] = toml_edit::value(toml_edit::Array::new());
    }
    let array = table[list]
        .as_array_mut()
        .ok_or_else(|| anyhow::anyhow!("permissions.{list} in {} is not a list", path.display()))?;
    if !array.iter().any(|v| v.as_str() == Some(rule)) {
        array.push(rule);
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(&path, doc.to_string())?;
    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn save_load_and_forget_provider() {
        let dir = std::env::temp_dir().join(format!("crack-cfg-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        // SAFETY: the only test that touches this variable.
        unsafe { std::env::set_var("CRACK_CONFIG_DIR", &dir) };
        std::fs::write(dir.join("config.toml"), "# my settings\nmodel = \"x/y\"\n").unwrap();

        save_provider(
            "gpu",
            &[
                ("kind", Setting::Text(Some("openai".into()))),
                ("base_url", Setting::Text(Some("http://box:8000/v1".into()))),
                ("models", Setting::List(vec!["a".into(), "b".into()])),
                ("region", Setting::Text(None)),
            ],
            Some("sk-secret"),
        )
        .unwrap();

        let text = std::fs::read_to_string(dir.join("config.toml")).unwrap();
        assert!(text.contains("# my settings"), "comments are kept: {text}");
        assert!(!text.contains("sk-secret"), "keys stay out of config.toml");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(dir.join("credentials.toml"))
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(mode & 0o777, 0o600);
        }

        let config = Config::load(&dir).unwrap();
        let gpu = &config.provider["gpu"];
        assert_eq!(gpu.api_key.as_deref(), Some("sk-secret"));
        assert_eq!(gpu.base_url.as_deref(), Some("http://box:8000/v1"));
        assert_eq!(gpu.models, vec!["a", "b"]);
        assert_eq!(config.model.as_deref(), Some("x/y"));

        let registry = crate::providers::Registry::load(&config);
        let info = registry.find("gpu").unwrap();
        assert!(info.available);
        assert_eq!(info.source.as_deref(), Some("key saved in crack"));

        forget_provider("gpu").unwrap();
        let config = Config::load(&dir).unwrap();
        assert!(!config.provider.contains_key("gpu"));
        unsafe { std::env::remove_var("CRACK_CONFIG_DIR") };
        std::fs::remove_dir_all(dir).unwrap();
    }
}
