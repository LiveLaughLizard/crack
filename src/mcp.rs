//! Model Context Protocol client: connects to MCP servers and offers their tools to
//! the model as `mcp__<server>__<tool>`.
//!
//! Servers come from `[mcp.<name>]` in crack's config and from `.mcp.json` files
//! (Claude Code's format: `{"mcpServers": {...}}`) in the project. Two transports:
//! stdio (a local command) and Streamable HTTP (a URL).

use crate::config::{self, Config};
use crate::providers::sse::SseReader;
use anyhow::{Context as _, anyhow, bail};
use serde::Deserialize;
use serde_json::{Value, json};
use std::collections::{BTreeMap, HashMap};
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::sync::oneshot;

const PROTOCOL_VERSION: &str = "2025-06-18";
const CONNECT_TIMEOUT: Duration = Duration::from_secs(30);

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

#[derive(Clone, Debug)]
pub struct ServerInfo {
    pub name: String,
    pub transport: &'static str,
    pub status: Status,
    pub tools: usize,
}

struct Server {
    name: String,
    config: McpServerConfig,
    status: Status,
    tools: Vec<McpTool>,
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
            })
            .collect()
    }

    pub fn tools(&self) -> Vec<McpTool> {
        self.servers
            .lock()
            .unwrap()
            .iter()
            .filter(|s| s.status == Status::Connected)
            .flat_map(|s| s.tools.clone())
            .collect()
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
        let config = {
            let mut servers = self.servers.lock().unwrap();
            let Some(server) = servers.iter_mut().find(|s| s.name == name) else {
                return;
            };
            server.status = Status::Connecting;
            server.client = None;
            server.tools.clear();
            server.config.clone()
        };
        let result = tokio::time::timeout(CONNECT_TIMEOUT, start(name, &config, &self.http))
            .await
            .unwrap_or_else(|_| Err(anyhow!("no answer within {}s", CONNECT_TIMEOUT.as_secs())));
        let mut servers = self.servers.lock().unwrap();
        let Some(server) = servers.iter_mut().find(|s| s.name == name) else {
            return;
        };
        match result {
            Ok((client, tools)) => {
                server.client = Some(client);
                server.tools = tools;
                server.status = Status::Connected;
            }
            Err(error) => server.status = Status::Failed(format!("{error:#}")),
        }
    }

    /// Calls a tool by its exposed name. Returns the text and whether it is an error.
    pub async fn call(&self, exposed: &str, arguments: Value) -> anyhow::Result<(String, bool)> {
        let (client, tool, timeout) = {
            let servers = self.servers.lock().unwrap();
            let (server, tool) = servers
                .iter()
                .find_map(|s| {
                    s.tools
                        .iter()
                        .find(|t| t.exposed == exposed)
                        .map(|t| (s, t))
                })
                .ok_or_else(|| anyhow!("no MCP tool named {exposed}"))?;
            let client = server
                .client
                .clone()
                .ok_or_else(|| anyhow!("MCP server '{}' is not connected", server.name))?;
            (
                client,
                tool.name.clone(),
                Duration::from_secs(server.config.timeout.unwrap_or(120)),
            )
        };
        let result = tokio::time::timeout(
            timeout,
            client.request("tools/call", json!({"name": tool, "arguments": arguments})),
        )
        .await
        .map_err(|_| anyhow!("the MCP tool took longer than {}s", timeout.as_secs()))??;
        Ok((
            render_content(&result),
            result["isError"].as_bool().unwrap_or(false),
        ))
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
        && let Some(structured) = result.get("structuredContent")
    {
        return structured.to_string();
    }
    parts.join("\n")
}

/// Connects, initializes and lists tools.
async fn start(
    name: &str,
    config: &McpServerConfig,
    http: &reqwest::Client,
) -> anyhow::Result<(Arc<Client>, Vec<McpTool>)> {
    let client = Arc::new(match (&config.url, &config.command) {
        (Some(url), _) => Client::Http(HttpClient {
            http: http.clone(),
            url: url.clone(),
            headers: config.headers.clone(),
            session: Mutex::new(None),
            next_id: AtomicU64::new(1),
        }),
        (None, Some(command)) => Client::Stdio(StdioClient::spawn(command, config)?),
        (None, None) => bail!("set either `command` or `url`"),
    });
    let init = client
        .request(
            "initialize",
            json!({
                "protocolVersion": PROTOCOL_VERSION,
                "capabilities": {},
                "clientInfo": {"name": "crack", "version": env!("CARGO_PKG_VERSION")},
            }),
        )
        .await
        .context("initialize")?;
    if init["capabilities"].get("tools").is_none() {
        // Servers without tools are fine; there's just nothing to offer.
        client
            .notify("notifications/initialized", json!({}))
            .await?;
        return Ok((client, Vec::new()));
    }
    client
        .notify("notifications/initialized", json!({}))
        .await?;

    let mut tools = Vec::new();
    let mut cursor: Option<String> = None;
    loop {
        let params = match &cursor {
            Some(c) => json!({"cursor": c}),
            None => json!({}),
        };
        let page = client
            .request("tools/list", params)
            .await
            .context("tools/list")?;
        for tool in page["tools"].as_array().into_iter().flatten() {
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
        cursor = page["nextCursor"].as_str().map(str::to_owned);
        if cursor.is_none() {
            break;
        }
    }
    Ok((client, tools))
}

enum Client {
    Stdio(StdioClient),
    Http(HttpClient),
}

impl Client {
    async fn request(&self, method: &str, params: Value) -> anyhow::Result<Value> {
        match self {
            Client::Stdio(c) => c.request(method, params).await,
            Client::Http(c) => c.request(method, params).await,
        }
    }

    async fn notify(&self, method: &str, params: Value) -> anyhow::Result<()> {
        match self {
            Client::Stdio(c) => {
                c.send(json!({"jsonrpc": "2.0", "method": method, "params": params}))
                    .await
            }
            Client::Http(c) => c.notify(method, params).await,
        }
    }
}

fn rpc_result(message: &Value) -> anyhow::Result<Value> {
    if let Some(error) = message.get("error") {
        bail!(
            "{} (code {})",
            error["message"].as_str().unwrap_or("error"),
            error["code"]
        );
    }
    Ok(message["result"].clone())
}

type Pending = Arc<Mutex<HashMap<u64, oneshot::Sender<Value>>>>;

struct StdioClient {
    stdin: Arc<tokio::sync::Mutex<tokio::process::ChildStdin>>,
    pending: Pending,
    next_id: AtomicU64,
    /// Last lines the server wrote to stderr, for error messages.
    stderr: Arc<Mutex<Vec<String>>>,
    _child: tokio::process::Child,
}

impl StdioClient {
    fn spawn(command: &str, config: &McpServerConfig) -> anyhow::Result<StdioClient> {
        let mut cmd = tokio::process::Command::new(command);
        cmd.args(&config.args)
            .envs(&config.env)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true);
        if let Some(dir) = &config.cwd {
            cmd.current_dir(config::expand_home(dir));
        }
        let mut child = cmd
            .spawn()
            .with_context(|| format!("couldn't start `{command}`"))?;
        let stdin = child.stdin.take().expect("piped");
        let stdout = child.stdout.take().expect("piped");
        let mut stderr_pipe = child.stderr.take().expect("piped");
        let pending: Pending = Arc::default();
        let stderr = Arc::new(Mutex::new(Vec::new()));

        let tail = stderr.clone();
        tokio::spawn(async move {
            let mut buffer = String::new();
            let _ = stderr_pipe.read_to_string(&mut buffer).await;
            let lines: Vec<String> = buffer.lines().rev().take(5).map(str::to_owned).collect();
            *tail.lock().unwrap() = lines.into_iter().rev().collect();
        });

        let (reply_tx, mut reply_rx) = tokio::sync::mpsc::unbounded_channel::<Value>();
        let reader_pending = pending.clone();
        tokio::spawn(async move {
            let mut lines = BufReader::new(stdout).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                let Ok(message) = serde_json::from_str::<Value>(&line) else {
                    continue;
                };
                let id = message.get("id");
                if message.get("method").is_some() {
                    // A request from the server: answer pings, decline the rest.
                    if let Some(id) = id {
                        let reply = if message["method"] == "ping" {
                            json!({"jsonrpc": "2.0", "id": id, "result": {}})
                        } else {
                            json!({"jsonrpc": "2.0", "id": id,
                                   "error": {"code": -32601, "message": "not supported by crack"}})
                        };
                        let _ = reply_tx.send(reply);
                    }
                } else if let Some(id) = id.and_then(Value::as_u64)
                    && let Some(waiter) = reader_pending.lock().unwrap().remove(&id)
                {
                    let _ = waiter.send(message);
                }
            }
            // The server exited: wake everyone still waiting.
            reader_pending.lock().unwrap().clear();
        });

        // Replies to server requests share stdin with our own requests.
        let stdin = Arc::new(tokio::sync::Mutex::new(stdin));
        let writer = stdin.clone();
        tokio::spawn(async move {
            while let Some(reply) = reply_rx.recv().await {
                let Ok(mut line) = serde_json::to_vec(&reply) else {
                    continue;
                };
                line.push(b'\n');
                let mut stdin = writer.lock().await;
                if stdin.write_all(&line).await.is_err() {
                    break;
                }
                let _ = stdin.flush().await;
            }
        });

        Ok(StdioClient {
            stdin,
            pending,
            next_id: AtomicU64::new(1),
            stderr,
            _child: child,
        })
    }

    async fn send(&self, message: Value) -> anyhow::Result<()> {
        let mut line = serde_json::to_vec(&message)?;
        line.push(b'\n');
        let mut stdin = self.stdin.lock().await;
        stdin.write_all(&line).await?;
        stdin.flush().await?;
        Ok(())
    }

    async fn request(&self, method: &str, params: Value) -> anyhow::Result<Value> {
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        let (tx, rx) = oneshot::channel();
        self.pending.lock().unwrap().insert(id, tx);
        self.send(json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}))
            .await
            .map_err(|e| anyhow!("{e}: {}", self.stderr_tail()))?;
        match rx.await {
            Ok(message) => rpc_result(&message),
            Err(_) => Err(anyhow!("the server exited. {}", self.stderr_tail())),
        }
    }

    fn stderr_tail(&self) -> String {
        self.stderr.lock().unwrap().join(" | ")
    }
}

struct HttpClient {
    http: reqwest::Client,
    url: String,
    headers: BTreeMap<String, String>,
    /// `Mcp-Session-Id` from the initialize response.
    session: Mutex<Option<String>>,
    next_id: AtomicU64,
}

impl HttpClient {
    fn post(&self, body: &Value) -> reqwest::RequestBuilder {
        let mut builder = self
            .http
            .post(&self.url)
            .header("content-type", "application/json")
            .header("accept", "application/json, text/event-stream")
            .header("mcp-protocol-version", PROTOCOL_VERSION)
            .json(body);
        for (name, value) in &self.headers {
            builder = builder.header(name, value);
        }
        if let Some(session) = self.session.lock().unwrap().clone() {
            builder = builder.header("mcp-session-id", session);
        }
        builder
    }

    /// Sends, retrying once when the request never reached the server (a pooled
    /// keep-alive connection the server had already closed).
    async fn send(&self, body: &Value) -> anyhow::Result<reqwest::Response> {
        match self.post(body).send().await {
            Err(error) if error.is_request() || error.is_connect() => {
                Ok(self.post(body).send().await?)
            }
            result => Ok(result?),
        }
    }

    async fn request(&self, method: &str, params: Value) -> anyhow::Result<Value> {
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        let body = json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params});
        let response = self.send(&body).await?;
        if let Some(session) = response
            .headers()
            .get("mcp-session-id")
            .and_then(|v| v.to_str().ok())
        {
            *self.session.lock().unwrap() = Some(session.to_owned());
        }
        if response.status() == reqwest::StatusCode::UNAUTHORIZED {
            bail!("the server needs authorization: add an Authorization header to its config");
        }
        if !response.status().is_success() {
            let status = response.status();
            let body = response.text().await.unwrap_or_default();
            bail!(
                "HTTP {status}: {}",
                body.chars().take(300).collect::<String>()
            );
        }
        let is_sse = response
            .headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok())
            .is_some_and(|v| v.contains("event-stream"));
        if !is_sse {
            return rpc_result(&response.json::<Value>().await?);
        }
        // A stream: the reply is the message carrying our id.
        let mut reader = SseReader::new(response);
        while let Some(event) = reader.next().await? {
            if let Ok(message) = serde_json::from_str::<Value>(&event.data)
                && message["id"].as_u64() == Some(id)
            {
                return rpc_result(&message);
            }
        }
        bail!("the stream ended without a reply")
    }

    async fn notify(&self, method: &str, params: Value) -> anyhow::Result<()> {
        let response = self
            .send(&json!({"jsonrpc": "2.0", "method": method, "params": params}))
            .await?;
        if !response.status().is_success() {
            bail!("HTTP {}", response.status());
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_and_expansion() {
        assert_eq!(
            exposed_name("git hub", "create.issue"),
            "mcp__git_hub__create_issue"
        );
        assert!(exposed_name("s", &"x".repeat(100)).len() <= 64);
        // SAFETY: only this test touches the variable.
        unsafe { std::env::set_var("CRACK_TEST_TOKEN", "abc") };
        assert_eq!(expand("Bearer ${CRACK_TEST_TOKEN}"), "Bearer abc");
        assert_eq!(expand("${CRACK_MISSING:-fallback}/x"), "fallback/x");
    }
}
