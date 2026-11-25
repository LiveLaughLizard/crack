//! Model Context Protocol client, on the official Rust SDK (rmcp): connects to MCP
//! servers and offers their tools to the model as `mcp__<server>__<tool>`, their
//! resources through `read_mcp_resource` and `@server:uri`, and their prompts as
//! `/server:prompt` commands.
//!
//! Servers come from `[mcp.<name>]` in crack's config and from `.mcp.json` files
//! (Claude Code's format: `{"mcpServers": {...}}`) in the project. Two transports:
//! stdio (a local command) and Streamable HTTP (a URL), with OAuth sign-in for HTTP
//! servers that need it.

use crate::config::{self, Config};
use anyhow::{Context as _, anyhow, bail};
use rmcp::model::{ClientCapabilities, ClientConfig, Implementation, ProtocolVersion};
use rmcp::service::{NotificationContext, RunningService};
use rmcp::transport::auth::OAuthState;
use rmcp::transport::streamable_http_client::StreamableHttpClientTransportConfig;
use rmcp::transport::{
    AuthClient, AuthError, AuthorizationManager, AuthorizationRequest, CredentialStore,
    StoredCredentials, StreamableHttpClientTransport, TokioChildProcess,
};
use rmcp::{ClientHandler, Peer, RoleClient, ServiceExt};
use serde::Deserialize;
use serde_json::{Value, json};
use std::collections::{BTreeMap, HashMap};
use std::path::Path;
use std::sync::{Arc, Mutex, Weak};
use std::time::Duration;

const CONNECT_TIMEOUT: Duration = Duration::from_secs(30);
const SIGN_IN_TIMEOUT: Duration = Duration::from_secs(300);

/// One server's settings. Either `command` (stdio) or `url` (HTTP).
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default)]
pub struct McpServerConfig {
    pub command: Option<String>,
    pub args: Vec<String>,
    pub env: BTreeMap<String, String>,
    pub cwd: Option<String>,
    pub url: Option<String>,
    pub headers: BTreeMap<String, String>,
    pub enabled: Option<bool>,
    /// Seconds a tool call may take (default 120).
    pub timeout: Option<u64>,
    /// `.mcp.json` spells the transport out: `stdio`, `http` or `sse`.
    #[serde(rename = "type")]
    pub kind: Option<String>,
}

impl McpServerConfig {
    fn transport(&self) -> &'static str {
        if self.url.is_some() { "http" } else { "stdio" }
    }
}

/// Expands `${VAR}` and `${VAR:-default}` from the environment.
fn expand(value: &str) -> String {
    let mut out = String::new();
    let mut rest = value;
    while let Some(start) = rest.find("${") {
        out.push_str(&rest[..start]);
        let after = &rest[start + 2..];
        let Some(end) = after.find('}') else {
            out.push_str(&rest[start..]);
            return out;
        };
        let inner = &after[..end];
        let (name, default) = inner.split_once(":-").unwrap_or((inner, ""));
        out.push_str(&std::env::var(name).unwrap_or_else(|_| default.to_owned()));
        rest = &after[end + 1..];
    }
    out.push_str(rest);
    out
}

fn expanded(mut config: McpServerConfig) -> McpServerConfig {
    config.command = config.command.map(|c| expand(&c));
    config.args = config.args.iter().map(|a| expand(a)).collect();
    config.url = config.url.map(|u| expand(&u));
    for value in config.env.values_mut().chain(config.headers.values_mut()) {
        *value = expand(value);
    }
    config
}

/// All configured servers: `.mcp.json` from the project root down to the working
/// directory, then crack's config (which wins on a name clash).
pub fn configured(cwd: &Path, config: &Config) -> BTreeMap<String, McpServerConfig> {
    #[derive(Deserialize)]
    struct McpJson {
        #[serde(rename = "mcpServers", default)]
        servers: BTreeMap<String, McpServerConfig>,
    }
    let mut out = BTreeMap::new();
    let root = crate::context::project_root(cwd);
    let mut dirs: Vec<&Path> = cwd
        .ancestors()
        .take_while(|d| d.starts_with(&root))
        .collect();
    dirs.reverse();
    let global = config::global_dir().map(|d| d.join("mcp.json"));
    let files = global
        .into_iter()
        .chain(dirs.iter().map(|d| d.join(".mcp.json")));
    for file in files {
        if let Ok(text) = std::fs::read_to_string(&file)
            && let Ok(parsed) = serde_json::from_str::<McpJson>(&text)
        {
            out.extend(parsed.servers);
        }
    }
    out.extend(config.mcp.clone());
    out.into_iter()
        .map(|(name, server)| (name, expanded(server)))
        .collect()
}

#[derive(Clone, Debug, PartialEq)]
pub enum Status {
    Connecting,
    Connected,
    /// The server wants OAuth: sign in from the MCP picker or `crack mcp login`.
    NeedsAuth,
    Failed(String),
    Disabled,
}

#[derive(Clone, Debug)]
pub struct McpTool {
    pub server: String,
    pub name: String,
    pub description: String,
    pub schema: Value,
    /// The name the model sees: `mcp__<server>__<tool>`.
    pub exposed: String,
}

/// A resource a server offers (`resources/list`).
#[derive(Clone, Debug, PartialEq)]
pub struct McpResource {
    pub server: String,
    pub uri: String,
    pub name: String,
    pub description: String,
    pub mime_type: Option<String>,
}

/// A prompt a server offers (`prompts/list`), used as `/server:prompt`.
#[derive(Clone, Debug, PartialEq)]
pub struct McpPrompt {
    pub server: String,
    pub name: String,
    pub description: String,
    /// `(name, required)`, in order.
    pub arguments: Vec<(String, bool)>,
}

impl McpPrompt {
    /// The slash command: `server:prompt`.
    pub fn command(&self) -> String {
        format!("{}:{}", self.server, self.name)
    }
}

#[derive(Clone, Debug)]
pub struct ServerInfo {
    pub name: String,
    pub transport: &'static str,
    pub status: Status,
    pub tools: usize,
    pub resources: usize,
    pub prompts: usize,
}

type Client = RunningService<RoleClient, Handler>;

struct Server {
    name: String,
    config: McpServerConfig,
    status: Status,
    tools: Vec<McpTool>,
    resources: Vec<McpResource>,
    prompts: Vec<McpPrompt>,
    client: Option<Arc<Client>>,
}

/// The MCP servers of a workspace.
pub struct McpHub {
    servers: Mutex<Vec<Server>>,
    http: reqwest::Client,
}

fn sanitize(name: &str) -> String {
    name.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

fn exposed_name(server: &str, tool: &str) -> String {
    let mut name = format!("mcp__{}__{}", sanitize(server), sanitize(tool));
    // Model APIs limit tool names to 64 characters.
    name.truncate(64);
    name
}

/// Receives server notifications: list changes refresh what crack offers.
#[derive(Clone)]
struct Handler {
    hub: Weak<McpHub>,
    server: String,
}

impl ClientHandler for Handler {
    fn get_info(&self) -> ClientConfig {
        // The `initialize` handshake works with every server in use today.
        ClientConfig::new(
            ClientCapabilities::default(),
            Implementation::new("crack", env!("CARGO_PKG_VERSION")),
        )
        .with_protocol_version(ProtocolVersion::LATEST_WITH_INITIALIZE)
    }

    async fn on_tool_list_changed(&self, context: NotificationContext<RoleClient>) {
        self.refresh(&context.peer).await;
    }

    async fn on_resource_list_changed(&self, context: NotificationContext<RoleClient>) {
        self.refresh(&context.peer).await;
    }

    async fn on_prompt_list_changed(&self, context: NotificationContext<RoleClient>) {
        self.refresh(&context.peer).await;
    }
}

impl Handler {
    async fn refresh(&self, peer: &Peer<RoleClient>) {
        let Some(hub) = self.hub.upgrade() else {
            return;
        };
        match list_everything(&self.server, peer).await {
            Ok(lists) => hub.store_lists(&self.server, lists),
            Err(error) => eprintln!(
                "crack: MCP server {}: refresh failed: {error:#}",
                self.server
            ),
        }
    }
}

type Lists = (Vec<McpTool>, Vec<McpResource>, Vec<McpPrompt>);

/// Lists tools, resources and prompts, as far as the server offers each.
async fn list_everything(name: &str, peer: &Peer<RoleClient>) -> anyhow::Result<Lists> {
    let capabilities = peer
        .peer_info()
        .and_then(|info| serde_json::to_value(&*info).ok())
        .map(|info| info["capabilities"].clone())
        .unwrap_or(Value::Null);
    let offers = |what: &str| capabilities.get(what).is_some_and(|v| !v.is_null());
    let mut tools = Vec::new();
    if offers("tools") {
        for tool in peer.list_all_tools().await.context("tools/list")? {
            let tool = serde_json::to_value(&tool)?;
            let tool_name = tool["name"].as_str().unwrap_or_default().to_owned();
            let mut schema = tool["inputSchema"].clone();
            if !schema.is_object() {
                schema = json!({"type": "object", "properties": {}});
            }
            tools.push(McpTool {
                exposed: exposed_name(name, &tool_name),
                server: name.to_owned(),
                description: tool["description"].as_str().unwrap_or_default().to_owned(),
                schema,
                name: tool_name,
            });
        }
    }
    let mut resources = Vec::new();
    if offers("resources") {
        // A server that advertises resources but can't list them still has tools.
        if let Ok(list) = peer.list_all_resources().await {
            for resource in list {
                let r = serde_json::to_value(&resource)?;
                resources.push(McpResource {
                    server: name.to_owned(),
                    uri: r["uri"].as_str().unwrap_or_default().to_owned(),
                    name: r["name"].as_str().unwrap_or_default().to_owned(),
                    description: r["description"].as_str().unwrap_or_default().to_owned(),
                    mime_type: r["mimeType"].as_str().map(str::to_owned),
                });
            }
        }
    }
    let mut prompts = Vec::new();
    if offers("prompts")
        && let Ok(list) = peer.list_all_prompts().await
    {
        for prompt in list {
            let p = serde_json::to_value(&prompt)?;
            prompts.push(McpPrompt {
                server: name.to_owned(),
                name: p["name"].as_str().unwrap_or_default().to_owned(),
                description: p["description"].as_str().unwrap_or_default().to_owned(),
                arguments: p["arguments"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .map(|a| {
                        (
                            a["name"].as_str().unwrap_or_default().to_owned(),
                            a["required"].as_bool().unwrap_or(false),
                        )
                    })
                    .collect(),
            });
        }
    }
    Ok((tools, resources, prompts))
}

impl McpHub {
    pub fn new(configs: BTreeMap<String, McpServerConfig>, http: reqwest::Client) -> Arc<McpHub> {
        let servers = configs
            .into_iter()
            .map(|(name, config)| Server {
                status: if config.enabled == Some(false) {
                    Status::Disabled
                } else {
                    Status::Connecting
                },
                name,
                config,
                tools: Vec::new(),
                resources: Vec::new(),
                prompts: Vec::new(),
                client: None,
            })
            .collect();
        Arc::new(McpHub {
            servers: Mutex::new(servers),
            http,
        })
    }

    pub fn is_empty(&self) -> bool {
        self.servers.lock().unwrap().is_empty()
    }

    pub fn snapshot(&self) -> Vec<ServerInfo> {
        self.servers
            .lock()
            .unwrap()
            .iter()
            .map(|s| ServerInfo {
                name: s.name.clone(),
                transport: s.config.transport(),
                status: s.status.clone(),
                tools: s.tools.len(),
                resources: s.resources.len(),
                prompts: s.prompts.len(),
            })
            .collect()
    }

    fn connected<T: Clone>(&self, pick: impl Fn(&Server) -> &Vec<T>) -> Vec<T> {
        self.servers
            .lock()
            .unwrap()
            .iter()
            .filter(|s| s.status == Status::Connected)
            .flat_map(|s| pick(s).clone())
            .collect()
    }

    pub fn tools(&self) -> Vec<McpTool> {
        self.connected(|s| &s.tools)
    }

    pub fn resources(&self) -> Vec<McpResource> {
        self.connected(|s| &s.resources)
    }

    pub fn prompts(&self) -> Vec<McpPrompt> {
        self.connected(|s| &s.prompts)
    }

    fn store_lists(&self, name: &str, (tools, resources, prompts): Lists) {
        let mut servers = self.servers.lock().unwrap();
        if let Some(server) = servers.iter_mut().find(|s| s.name == name) {
            server.tools = tools;
            server.resources = resources;
            server.prompts = prompts;
        }
    }

    /// Connects every enabled server, concurrently.
    pub async fn connect_all(self: &Arc<Self>) {
        let names: Vec<String> = self
            .servers
            .lock()
            .unwrap()
            .iter()
            .filter(|s| s.status != Status::Disabled)
            .map(|s| s.name.clone())
            .collect();
        futures::future::join_all(names.iter().map(|name| self.connect(name))).await;
    }

    /// (Re)connects one server.
    pub async fn connect(self: &Arc<Self>, name: &str) {
        let (config, old) = {
            let mut servers = self.servers.lock().unwrap();
            let Some(server) = servers.iter_mut().find(|s| s.name == name) else {
                return;
            };
            server.status = Status::Connecting;
            server.tools.clear();
            server.resources.clear();
            server.prompts.clear();
            (server.config.clone(), server.client.take())
        };
        // Close the old connection (and stop its process) before starting again.
        drop(old);
        let handler = Handler {
            hub: Arc::downgrade(self),
            server: name.to_owned(),
        };
        let result = tokio::time::timeout(CONNECT_TIMEOUT, start(handler, &config, &self.http))
            .await
            .unwrap_or_else(|_| {
                Err(Failure::Other(anyhow!(
                    "no answer within {}s",
                    CONNECT_TIMEOUT.as_secs()
                )))
            });
        let result = match result {
            Ok(client) => list_everything(name, client.peer())
                .await
                .map(|lists| (client, lists))
                .map_err(Failure::Other),
            Err(error) => Err(error),
        };
        let mut servers = self.servers.lock().unwrap();
        let Some(server) = servers.iter_mut().find(|s| s.name == name) else {
            return;
        };
        match result {
            Ok((client, (tools, resources, prompts))) => {
                server.client = Some(Arc::new(client));
                server.tools = tools;
                server.resources = resources;
                server.prompts = prompts;
                server.status = Status::Connected;
            }
            Err(Failure::NeedsAuth) => server.status = Status::NeedsAuth,
            Err(Failure::Other(error)) => server.status = Status::Failed(format!("{error:#}")),
        }
    }

    fn client(&self, server: &str) -> anyhow::Result<(Arc<Client>, Duration)> {
        let servers = self.servers.lock().unwrap();
        let entry = servers
            .iter()
            .find(|s| s.name == server)
            .ok_or_else(|| anyhow!("no MCP server named '{server}'"))?;
        let client = entry
            .client
            .clone()
            .ok_or_else(|| anyhow!("MCP server '{server}' is not connected"))?;
        Ok((
            client,
            Duration::from_secs(entry.config.timeout.unwrap_or(120)),
        ))
    }

    /// Calls a tool by its exposed name. Returns the text and whether it is an error.
    pub async fn call(&self, exposed: &str, arguments: Value) -> anyhow::Result<(String, bool)> {
        let (server, tool) = self
            .tools()
            .into_iter()
            .find(|t| t.exposed == exposed)
            .map(|t| (t.server, t.name))
            .ok_or_else(|| anyhow!("no MCP tool named {exposed}"))?;
        let (client, timeout) = self.client(&server)?;
        let arguments = if arguments.is_object() {
            arguments
        } else {
            json!({})
        };
        let params = serde_json::from_value(json!({"name": tool, "arguments": arguments}))?;
        let result = tokio::time::timeout(timeout, client.call_tool(params))
            .await
            .map_err(|_| anyhow!("the MCP tool took longer than {}s", timeout.as_secs()))??;
        let result = serde_json::to_value(&result)?;
        Ok((
            render_content(&result),
            result["isError"].as_bool().unwrap_or(false),
        ))
    }

    /// Reads a resource's contents as text (binary parts are described).
    pub async fn read_resource(&self, server: &str, uri: &str) -> anyhow::Result<String> {
        let (client, timeout) = self.client(server)?;
        let params = serde_json::from_value(json!({"uri": uri}))?;
        let result = tokio::time::timeout(timeout, client.read_resource(params))
            .await
            .map_err(|_| anyhow!("reading {uri} took longer than {}s", timeout.as_secs()))??;
        let result = serde_json::to_value(&result)?;
        let parts: Vec<String> = result["contents"]
            .as_array()
            .into_iter()
            .flatten()
            .map(|c| match c["text"].as_str() {
                Some(text) => text.to_owned(),
                None => format!(
                    "[binary resource {} ({})]",
                    c["uri"].as_str().unwrap_or(uri),
                    c["mimeType"].as_str().unwrap_or("unknown type")
                ),
            })
            .collect();
        Ok(parts.join("\n"))
    }

    /// Fills in a prompt (`prompts/get`) and returns its messages as text.
    pub async fn get_prompt(
        &self,
        server: &str,
        prompt: &str,
        arguments: BTreeMap<String, String>,
    ) -> anyhow::Result<String> {
        let (client, timeout) = self.client(server)?;
        let params = serde_json::from_value(json!({"name": prompt, "arguments": arguments}))?;
        let result = tokio::time::timeout(timeout, client.get_prompt(params))
            .await
            .map_err(|_| anyhow!("the prompt took longer than {}s", timeout.as_secs()))??;
        let result = serde_json::to_value(&result)?;
        let mut parts = Vec::new();
        for message in result["messages"].as_array().into_iter().flatten() {
            let content = &message["content"];
            let text = match content["type"].as_str() {
                Some("text") => content["text"].as_str().unwrap_or_default().to_owned(),
                Some("resource") => content["resource"]["text"]
                    .as_str()
                    .map(str::to_owned)
                    .unwrap_or_else(|| format!("[resource {}]", content["resource"]["uri"])),
                Some(other) => format!("[{other} content]"),
                None => String::new(),
            };
            if message["role"] == "assistant" {
                parts.push(format!("(assistant) {text}"));
            } else {
                parts.push(text);
            }
        }
        Ok(parts.join("\n\n"))
    }

    /// Signs in to an HTTP server with OAuth: `open` is given the browser URL, and
    /// the browser comes back to a localhost callback. Then the server reconnects.
    pub async fn sign_in(
        self: &Arc<Self>,
        name: &str,
        open: impl FnOnce(String) + Send,
    ) -> anyhow::Result<()> {
        let url = {
            let servers = self.servers.lock().unwrap();
            let server = servers
                .iter()
                .find(|s| s.name == name)
                .ok_or_else(|| anyhow!("no MCP server named '{name}'"))?;
            server.config.url.clone().ok_or_else(|| {
                anyhow!("'{name}' isn't an HTTP server; there's nothing to sign in to")
            })?
        };
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let redirect = format!(
            "http://127.0.0.1:{}/callback",
            listener.local_addr()?.port()
        );
        let mut manager = AuthorizationManager::new(url.as_str()).await?;
        manager.set_credential_store(FileStore { url: url.clone() });
        let mut state = OAuthState::Unauthorized(manager);
        state
            .start_authorization(AuthorizationRequest::new(&redirect).with_client_name("crack"))
            .await
            .context("starting sign-in")?;
        open(state.get_authorization_url().await?);
        let callback = tokio::time::timeout(SIGN_IN_TIMEOUT, accept_callback(&listener, name))
            .await
            .map_err(|_| {
                anyhow!(
                    "sign-in wasn't finished within {} minutes",
                    SIGN_IN_TIMEOUT.as_secs() / 60
                )
            })??;
        state
            .handle_callback_url(&callback)
            .await
            .context("finishing sign-in")?;
        self.connect(name).await;
        Ok(())
    }

    /// What a prompt pulls in from MCP: the text of each `@server:uri` resource it
    /// mentions, and a `/server:prompt args` at its start filled in. Each block is
    /// extra context for the user's message; failures say what went wrong.
    pub async fn expand_prompt(&self, text: &str) -> Vec<String> {
        let mut blocks = Vec::new();
        if let Some(rest) = text.trim_start().strip_prefix('/') {
            let (command, args) = rest.split_once(char::is_whitespace).unwrap_or((rest, ""));
            if let Some(prompt) = self.prompts().into_iter().find(|p| p.command() == command) {
                let arguments = prompt_arguments(&prompt, args);
                blocks.push(
                    match self
                        .get_prompt(&prompt.server, &prompt.name, arguments)
                        .await
                    {
                        Ok(body) => format!("The user ran the MCP prompt /{command}:\n\n{body}"),
                        Err(error) => format!("The MCP prompt /{command} failed: {error:#}"),
                    },
                );
            }
        }
        let resources = self.resources();
        let mut seen = Vec::new();
        for word in text.split_whitespace() {
            let Some(reference) = word.strip_prefix('@') else {
                continue;
            };
            let reference = reference.trim_end_matches([',', '.', ';', ')', '?', '!']);
            let Some((server, uri)) = reference.split_once(':') else {
                continue;
            };
            if seen.iter().any(|s| s == reference)
                || !resources.iter().any(|r| r.server == server && r.uri == uri)
            {
                continue;
            }
            seen.push(reference.to_owned());
            blocks.push(match self.read_resource(server, uri).await {
                Ok(body) => format!("Contents of the MCP resource @{server}:{uri}:\n\n{body}"),
                Err(error) => format!("Couldn't read the MCP resource @{server}:{uri}: {error:#}"),
            });
        }
        blocks
    }

    /// Forgets a server's OAuth tokens.
    pub fn sign_out(&self, name: &str) -> anyhow::Result<()> {
        let url = self
            .servers
            .lock()
            .unwrap()
            .iter()
            .find(|s| s.name == name)
            .and_then(|s| s.config.url.clone())
            .ok_or_else(|| anyhow!("no HTTP MCP server named '{name}'"))?;
        config::save_mcp_oauth(&credentials_file()?, &url, None)
    }
}

/// `key=value` words become named arguments; any other text fills the first
/// argument not given by name.
fn prompt_arguments(prompt: &McpPrompt, args: &str) -> BTreeMap<String, String> {
    let mut named = BTreeMap::new();
    let mut loose = Vec::new();
    for word in args.split_whitespace() {
        match word.split_once('=') {
            Some((key, value)) if prompt.arguments.iter().any(|(name, _)| name == key) => {
                named.insert(key.to_owned(), value.to_owned());
            }
            _ => loose.push(word),
        }
    }
    if !loose.is_empty()
        && let Some((name, _)) = prompt
            .arguments
            .iter()
            .find(|(name, _)| !named.contains_key(name))
    {
        named.insert(name.clone(), loose.join(" "));
    }
    named
}

/// Waits for the browser to come back to the localhost callback; returns its URL.
async fn accept_callback(listener: &tokio::net::TcpListener, name: &str) -> anyhow::Result<String> {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    loop {
        let (mut socket, _) = listener.accept().await?;
        let mut buffer = vec![0u8; 8192];
        let n = socket.read(&mut buffer).await?;
        let head = String::from_utf8_lossy(&buffer[..n]);
        let target = head
            .lines()
            .next()
            .and_then(|line| line.split_whitespace().nth(1))
            .unwrap_or("/")
            .to_owned();
        if !target.starts_with("/callback") {
            // Browsers also ask for /favicon.ico and the like.
            let _ = socket
                .write_all(
                    b"HTTP/1.1 404 Not Found\r\ncontent-length: 0\r\nconnection: close\r\n\r\n",
                )
                .await;
            continue;
        }
        let body = format!(
            "<!doctype html><meta charset=utf-8><title>crack</title>\
             <body style=\"font-family:sans-serif;padding:3em\">\
             <h2>Signed in to {name}</h2><p>You can close this tab and go back to crack.</p>"
        );
        let _ = socket
            .write_all(
                format!(
                    "HTTP/1.1 200 OK\r\ncontent-type: text/html; charset=utf-8\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                    body.len()
                )
                .as_bytes(),
            )
            .await;
        let port = listener.local_addr()?.port();
        return Ok(format!("http://127.0.0.1:{port}{target}"));
    }
}

/// Tests point the credentials file somewhere private.
#[cfg(test)]
static TEST_CREDENTIALS: Mutex<Option<std::path::PathBuf>> = Mutex::new(None);

/// crack's credentials file, where OAuth tokens are kept.
fn credentials_file() -> anyhow::Result<std::path::PathBuf> {
    #[cfg(test)]
    if let Some(path) = TEST_CREDENTIALS.lock().unwrap().clone() {
        return Ok(path);
    }
    config::credentials_path().ok_or_else(|| anyhow!("no config directory"))
}

/// OAuth tokens in crack's credentials file (0600), per server URL.
struct FileStore {
    url: String,
}

#[async_trait::async_trait]
impl CredentialStore for FileStore {
    async fn load(&self) -> Result<Option<StoredCredentials>, AuthError> {
        let Ok(path) = credentials_file() else {
            return Ok(None);
        };
        Ok(config::load_mcp_oauth(&path, &self.url)
            .and_then(|json| serde_json::from_str(&json).ok()))
    }

    async fn save(&self, credentials: StoredCredentials) -> Result<(), AuthError> {
        let json = serde_json::to_string(&credentials)
            .map_err(|e| AuthError::CredentialStoreError(e.to_string()))?;
        credentials_file()
            .and_then(|path| config::save_mcp_oauth(&path, &self.url, Some(&json)))
            .map_err(|e| AuthError::CredentialStoreError(format!("{e:#}")))
    }

    async fn clear(&self) -> Result<(), AuthError> {
        credentials_file()
            .and_then(|path| config::save_mcp_oauth(&path, &self.url, None))
            .map_err(|e| AuthError::CredentialStoreError(format!("{e:#}")))
    }
}

/// Tool results as text: text parts joined; other kinds described.
fn render_content(result: &Value) -> String {
    let mut parts = Vec::new();
    for item in result["content"].as_array().into_iter().flatten() {
        match item["type"].as_str().unwrap_or_default() {
            "text" => parts.push(item["text"].as_str().unwrap_or_default().to_owned()),
            "resource" => {
                let resource = &item["resource"];
                match resource["text"].as_str() {
                    Some(text) => parts.push(text.to_owned()),
                    None => parts.push(format!(
                        "[resource {}]",
                        resource["uri"].as_str().unwrap_or("?")
                    )),
                }
            }
            "resource_link" => parts.push(format!(
                "[resource {}]",
                item["uri"].as_str().unwrap_or("?")
            )),
            "image" => parts.push(format!(
                "[image: {}]",
                item["mimeType"].as_str().unwrap_or("image")
            )),
            "audio" => parts.push("[audio]".into()),
            other => parts.push(format!("[{other} content]")),
        }
    }
    if parts.is_empty()
        && let Some(structured) = result.get("structuredContent").filter(|v| !v.is_null())
    {
        return structured.to_string();
    }
    parts.join("\n")
}

/// Why a connection didn't come up.
enum Failure {
    NeedsAuth,
    Other(anyhow::Error),
}

/// Starts the transport and runs the `initialize` handshake.
async fn start(
    handler: Handler,
    config: &McpServerConfig,
    http: &reqwest::Client,
) -> Result<Client, Failure> {
    match (&config.url, &config.command) {
        (Some(url), _) => start_http(handler, url, config, http).await,
        (None, Some(command)) => start_stdio(handler, command, config)
            .await
            .map_err(Failure::Other),
        (None, None) => Err(Failure::Other(anyhow!("set either `command` or `url`"))),
    }
}

async fn start_stdio(
    handler: Handler,
    command: &str,
    config: &McpServerConfig,
) -> anyhow::Result<Client> {
    let mut cmd = tokio::process::Command::new(command);
    cmd.args(&config.args).envs(&config.env);
    if let Some(dir) = &config.cwd {
        cmd.current_dir(config::expand_home(dir));
    }
    let (transport, stderr) = TokioChildProcess::builder(cmd)
        .stderr(std::process::Stdio::piped())
        .spawn()
        .with_context(|| format!("couldn't start `{command}`"))?;
    // Keep the last lines the server wrote to stderr, for error messages.
    let tail = Arc::new(Mutex::new(Vec::<String>::new()));
    if let Some(stderr) = stderr {
        let tail = tail.clone();
        tokio::spawn(async move {
            use tokio::io::AsyncBufReadExt;
            let mut lines = tokio::io::BufReader::new(stderr).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                let mut tail = tail.lock().unwrap();
                tail.push(line);
                let excess = tail.len().saturating_sub(5);
                tail.drain(..excess);
            }
        });
    }
    match handler.serve(transport).await {
        Ok(client) => Ok(client),
        Err(error) => {
            // Give stderr a moment to arrive; it usually says what went wrong.
            tokio::time::sleep(Duration::from_millis(100)).await;
            let tail = tail.lock().unwrap().join(" | ");
            if tail.is_empty() {
                bail!("{error}")
            } else {
                bail!("{error}: {tail}")
            }
        }
    }
}

async fn start_http(
    handler: Handler,
    url: &str,
    config: &McpServerConfig,
    http: &reqwest::Client,
) -> Result<Client, Failure> {
    let mut headers = HashMap::new();
    for (name, value) in &config.headers {
        let name = reqwest::header::HeaderName::from_bytes(name.as_bytes())
            .map_err(|e| Failure::Other(anyhow!("header {name}: {e}")))?;
        let value = reqwest::header::HeaderValue::from_str(value)
            .map_err(|e| Failure::Other(anyhow!("header {name}: {e}")))?;
        headers.insert(name, value);
    }
    let transport_config =
        StreamableHttpClientTransportConfig::with_uri(url).custom_headers(headers);
    let result = match stored_authorization(url).await {
        Some(manager) => {
            let client = AuthClient::new(http.clone(), manager);
            handler
                .serve(StreamableHttpClientTransport::with_client(
                    client,
                    transport_config,
                ))
                .await
        }
        None => {
            handler
                .serve(StreamableHttpClientTransport::with_client(
                    http.clone(),
                    transport_config,
                ))
                .await
        }
    };
    result.map_err(|error| {
        if error.is_authorization_required() {
            Failure::NeedsAuth
        } else {
            Failure::Other(anyhow!("{error}"))
        }
    })
}

/// The authorization saved for a server by an earlier sign-in, if any.
async fn stored_authorization(url: &str) -> Option<AuthorizationManager> {
    config::load_mcp_oauth(&credentials_file().ok()?, url)?;
    let mut manager = AuthorizationManager::new(url).await.ok()?;
    manager.set_credential_store(FileStore {
        url: url.to_owned(),
    });
    match manager.initialize_from_store().await {
        Ok(true) => Some(manager),
        _ => None,
    }
}

#[cfg(test)]
mod tests;
