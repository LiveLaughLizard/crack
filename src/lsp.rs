//! Language servers: diagnostics after edits, and definition, references, hover and
//! rename tools.
//!
//! Servers are found from project files (Cargo.toml, package.json, ...), started only
//! if installed, and run in the sandbox like shell commands (rust-analyzer and others
//! run build scripts). Missing ones can be installed from the sidebar after the user
//! confirms the command.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::sync::{Notify, oneshot};

#[derive(Clone, Debug)]
pub struct ServerSpec {
    pub name: &'static str,
    pub command: &'static [&'static str],
    /// Any of these in the project root means the language is in use.
    pub markers: &'static [&'static str],
    pub extensions: &'static [&'static str],
    pub language_id: fn(&str) -> &'static str,
    /// How to install it, when there's one standard way.
    pub install: Option<&'static str>,
}

fn ts_language(ext: &str) -> &'static str {
    match ext {
        "tsx" => "typescriptreact",
        "jsx" => "javascriptreact",
        "js" | "mjs" | "cjs" => "javascript",
        _ => "typescript",
    }
}

pub const SERVERS: &[ServerSpec] = &[
    ServerSpec {
        name: "rust-analyzer",
        command: &["rust-analyzer"],
        markers: &["Cargo.toml"],
        extensions: &["rs"],
        language_id: |_| "rust",
        install: Some("rustup component add rust-analyzer"),
    },
    ServerSpec {
        name: "typescript-language-server",
        command: &["typescript-language-server", "--stdio"],
        markers: &["package.json", "tsconfig.json", "jsconfig.json"],
        extensions: &["ts", "tsx", "js", "jsx", "mjs", "cjs", "mts", "cts"],
        language_id: ts_language,
        install: Some("npm install -g typescript-language-server typescript"),
    },
    ServerSpec {
        name: "pyright",
        command: &["pyright-langserver", "--stdio"],
        markers: &[
            "pyproject.toml",
            "setup.py",
            "setup.cfg",
            "requirements.txt",
            "Pipfile",
        ],
        extensions: &["py", "pyi"],
        language_id: |_| "python",
        install: Some("npm install -g pyright"),
    },
    ServerSpec {
        name: "gopls",
        command: &["gopls"],
        markers: &["go.mod", "go.work"],
        extensions: &["go"],
        language_id: |_| "go",
        install: Some("go install golang.org/x/tools/gopls@latest"),
    },
    ServerSpec {
        name: "clangd",
        command: &["clangd"],
        markers: &[
            "compile_commands.json",
            "CMakeLists.txt",
            "compile_flags.txt",
            "meson.build",
        ],
        extensions: &["c", "h", "cc", "cpp", "cxx", "hpp", "hh", "hxx"],
        language_id: |ext| if ext == "c" || ext == "h" { "c" } else { "cpp" },
        install: None,
    },
];

#[derive(Clone, Debug, PartialEq)]
pub enum Status {
    NotInstalled,
    /// Installed; starts when a file of its language is touched.
    Idle,
    Running,
    Failed(String),
}

/// A running language server.
struct Client {
    stdin: tokio::sync::Mutex<tokio::process::ChildStdin>,
    next_id: AtomicI64,
    pending: Mutex<HashMap<i64, oneshot::Sender<Value>>>,
    /// uri → (version seen, diagnostics)
    diagnostics: Mutex<HashMap<String, Vec<Value>>>,
    published: Notify,
    /// uri → version of each open document
    open: tokio::sync::Mutex<HashMap<String, i32>>,
    /// Work the server reports in progress (`$/progress` tokens).
    busy: Mutex<std::collections::HashSet<String>>,
    /// rust-analyzer says when it's done loading (`experimental/serverStatus`).
    quiescent: std::sync::atomic::AtomicBool,
    progressed: Notify,
    _child: Mutex<tokio::process::Child>,
}

impl Client {
    async fn start(
        spec: &ServerSpec,
        root: &Path,
        sandbox: Option<&crate::policy::Sandbox>,
    ) -> anyhow::Result<Arc<Client>> {
        let command = spec
            .command
            .iter()
            .map(|w| shell_quote(w))
            .collect::<Vec<_>>()
            .join(" ");
        let argv = match sandbox {
            Some(s) if s.active() => crate::policy::sandboxed(&format!("exec {command}"), s, root)
                .ok_or_else(|| anyhow::anyhow!("the sandbox isn't available"))?,
            _ => spec.command.iter().map(|s| (*s).to_owned()).collect(),
        };
        // The server's own log, for when it misbehaves.
        let log =
            std::fs::File::create(crate::config::data_dir().join(format!("lsp-{}.log", spec.name)))
                .map(std::process::Stdio::from)
                .unwrap_or_else(|_| std::process::Stdio::null());
        let mut child = tokio::process::Command::new(&argv[0])
            .args(&argv[1..])
            .current_dir(root)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(log)
            .kill_on_drop(true)
            .spawn()?;
        let stdin = child.stdin.take().expect("piped");
        let stdout = child.stdout.take().expect("piped");
        let client = Arc::new(Client {
            stdin: tokio::sync::Mutex::new(stdin),
            next_id: AtomicI64::new(1),
            pending: Mutex::new(HashMap::new()),
            diagnostics: Mutex::new(HashMap::new()),
            published: Notify::new(),
            open: tokio::sync::Mutex::new(HashMap::new()),
            busy: Mutex::new(Default::default()),
            // Servers without status notices count as ready once initialized.
            quiescent: std::sync::atomic::AtomicBool::new(spec.name != "rust-analyzer"),
            progressed: Notify::new(),
            _child: Mutex::new(child),
        });
        let reader = client.clone();
        tokio::spawn(async move {
            let mut stdout = BufReader::new(stdout);
            while let Ok(Some(message)) = read_message(&mut stdout).await {
                reader.dispatch(message).await;
            }
            // The server is gone: fail whatever is waiting.
            reader.pending.lock().unwrap().clear();
        });
        let root_uri = uri(root);
        let init = client
            .request(
                "initialize",
                json!({
                    "processId": std::process::id(),
                    "rootUri": root_uri,
                    "workspaceFolders": [{"uri": root_uri, "name": root.file_name().and_then(|n| n.to_str()).unwrap_or("project")}],
                    "capabilities": {
                        "textDocument": {
                            "synchronization": {"didSave": true},
                            "publishDiagnostics": {"relatedInformation": false},
                            "definition": {"linkSupport": false},
                            "references": {},
                            "hover": {"contentFormat": ["plaintext", "markdown"]},
                            "rename": {"prepareSupport": false}
                        },
                        "workspace": {"workspaceEdit": {"documentChanges": true}, "configuration": true},
                        "window": {"workDoneProgress": true},
                        "experimental": {"serverStatusNotification": true}
                    },
                    "clientInfo": {"name": "crack", "version": env!("CARGO_PKG_VERSION")}
                }),
                Duration::from_secs(60),
            )
            .await
            .map_err(|error| {
                // It died on startup: its log usually says why.
                let log = std::fs::read_to_string(crate::config::data_dir().join(format!("lsp-{}.log", spec.name))).unwrap_or_default();
                match log.lines().find(|l| !l.trim().is_empty()) {
                    Some(line) => anyhow::anyhow!("{error}: {}", line.trim()),
                    None => error,
                }
            })?;
        if init.get("error").is_some() {
            anyhow::bail!("initialize failed: {}", init["error"]);
        }
        client.notify("initialized", json!({})).await?;
        Ok(client)
    }

    async fn send(&self, message: Value) -> anyhow::Result<()> {
        let body = message.to_string();
        let mut stdin = self.stdin.lock().await;
        stdin
            .write_all(format!("Content-Length: {}\r\n\r\n{body}", body.len()).as_bytes())
            .await?;
        stdin.flush().await?;
        Ok(())
    }

    async fn notify(&self, method: &str, params: Value) -> anyhow::Result<()> {
        self.send(json!({"jsonrpc": "2.0", "method": method, "params": params}))
            .await
    }

    /// Sends a request and waits for the whole response message.
    async fn request(
        &self,
        method: &str,
        params: Value,
        timeout: Duration,
    ) -> anyhow::Result<Value> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = oneshot::channel();
        self.pending.lock().unwrap().insert(id, tx);
        self.send(json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}))
            .await?;
        match tokio::time::timeout(timeout, rx).await {
            Ok(Ok(response)) => Ok(response),
            Ok(Err(_)) => anyhow::bail!("the language server stopped"),
            Err(_) => {
                self.pending.lock().unwrap().remove(&id);
                anyhow::bail!("{method} timed out")
            }
        }
    }

    async fn dispatch(&self, message: Value) {
        let method = message["method"].as_str();
        match (message.get("id"), method) {
            // A response to one of ours.
            (Some(id), None) => {
                if let Some(id) = id.as_i64()
                    && let Some(tx) = self.pending.lock().unwrap().remove(&id)
                {
                    let _ = tx.send(message);
                }
            }
            // A request from the server: answer so it never waits on us.
            (Some(id), Some(method)) => {
                let result = match method {
                    "workspace/configuration" => {
                        let items = message["params"]["items"].as_array().map_or(0, Vec::len);
                        Value::Array(vec![Value::Null; items])
                    }
                    _ => Value::Null,
                };
                let _ = self
                    .send(json!({"jsonrpc": "2.0", "id": id, "result": result}))
                    .await;
            }
            (None, Some("$/progress")) => {
                let params = &message["params"];
                let token = params["token"].to_string();
                match params["value"]["kind"].as_str() {
                    Some("begin") => {
                        self.busy.lock().unwrap().insert(token);
                    }
                    Some("end") => {
                        self.busy.lock().unwrap().remove(&token);
                    }
                    _ => {}
                }
                self.progressed.notify_waiters();
            }
            (None, Some("experimental/serverStatus")) => {
                let quiescent = message["params"]["quiescent"].as_bool().unwrap_or(true);
                self.quiescent.store(quiescent, Ordering::Relaxed);
                self.progressed.notify_waiters();
            }
            (None, Some("textDocument/publishDiagnostics")) => {
                let params = &message["params"];
                if let Some(uri) = params["uri"].as_str() {
                    let list = params["diagnostics"]
                        .as_array()
                        .cloned()
                        .unwrap_or_default();
                    self.diagnostics
                        .lock()
                        .unwrap()
                        .insert(uri.to_owned(), list);
                    self.published.notify_waiters();
                }
            }
            _ => {}
        }
    }

    /// Waits until the server has finished loading and indexing (or `timeout` passes).
    async fn wait_ready(&self, timeout: Duration) {
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            let notified = self.progressed.notified();
            let ready =
                self.quiescent.load(Ordering::Relaxed) && self.busy.lock().unwrap().is_empty();
            if ready {
                // Settle briefly: servers often start the next job right away.
                let settle = tokio::time::Instant::now() + Duration::from_millis(300);
                if tokio::time::timeout_at(settle.min(deadline), notified)
                    .await
                    .is_err()
                {
                    return;
                }
                continue;
            }
            if tokio::time::timeout_at(deadline, notified).await.is_err() {
                return;
            }
        }
    }

    /// Opens or updates a document with its current text from disk.
    async fn sync(&self, path: &Path, language_id: &str) -> anyhow::Result<String> {
        let text = tokio::fs::read_to_string(path).await?;
        let uri = uri(path);
        let mut open = self.open.lock().await;
        match open.get_mut(&uri) {
            Some(version) => {
                *version += 1;
                self.notify(
                    "textDocument/didChange",
                    json!({"textDocument": {"uri": uri, "version": *version}, "contentChanges": [{"text": text}]}),
                )
                .await?;
                self.notify(
                    "textDocument/didSave",
                    json!({"textDocument": {"uri": uri}}),
                )
                .await?;
            }
            None => {
                open.insert(uri.clone(), 1);
                self.notify(
                    "textDocument/didOpen",
                    json!({"textDocument": {"uri": uri, "languageId": language_id, "version": 1, "text": text}}),
                )
                .await?;
                // The file is saved on disk: this starts checks like `cargo check`.
                self.notify(
                    "textDocument/didSave",
                    json!({"textDocument": {"uri": uri}}),
                )
                .await?;
            }
        }
        Ok(uri)
    }
}

async fn read_message(
    reader: &mut BufReader<tokio::process::ChildStdout>,
) -> anyhow::Result<Option<Value>> {
    let mut length = None;
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line).await? == 0 {
            return Ok(None);
        }
        let line = line.trim_end();
        if line.is_empty() {
            break;
        }
        if let Some(value) = line.strip_prefix("Content-Length:") {
            length = value.trim().parse::<usize>().ok();
        }
    }
    let length = length.ok_or_else(|| anyhow::anyhow!("message without Content-Length"))?;
    let mut body = vec![0; length];
    reader.read_exact(&mut body).await?;
    Ok(Some(serde_json::from_slice(&body)?))
}

fn shell_quote(word: &str) -> String {
    if word
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || "-_./=".contains(c))
    {
        word.to_owned()
    } else {
        format!("'{}'", word.replace('\'', r"'\''"))
    }
}

pub fn uri(path: &Path) -> String {
    let mut out = String::from("file://");
    for byte in path.display().to_string().bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'/' | b'-' | b'_' | b'.' | b'~' => {
                out.push(byte as char)
            }
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}

fn path_of(uri: &str) -> PathBuf {
    let raw = uri.strip_prefix("file://").unwrap_or(uri);
    let bytes = raw.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%'
            && i + 2 < bytes.len()
            && let Ok(byte) =
                u8::from_str_radix(std::str::from_utf8(&bytes[i + 1..i + 3]).unwrap_or(""), 16)
        {
            out.push(byte);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    PathBuf::from(String::from_utf8_lossy(&out).into_owned())
}

/// The language servers for one project.
pub struct Hub {
    root: PathBuf,
    sandbox: Option<crate::policy::Sandbox>,
    clients: tokio::sync::Mutex<HashMap<&'static str, Arc<Client>>>,
    status: Mutex<HashMap<&'static str, Status>>,
}

impl Hub {
    /// The hub for a project, created once per process.
    pub fn for_project(cwd: &Path, sandbox: Option<crate::policy::Sandbox>) -> Arc<Hub> {
        static HUBS: OnceLock<Mutex<HashMap<PathBuf, Arc<Hub>>>> = OnceLock::new();
        let root = crate::context::project_root(cwd);
        let mut hubs = HUBS.get_or_init(Default::default).lock().unwrap();
        hubs.entry(root.clone())
            .or_insert_with(|| {
                let hub = Hub {
                    status: Mutex::new(
                        relevant(&root)
                            .into_iter()
                            .map(|s| {
                                (
                                    s.name,
                                    if installed(s) {
                                        Status::Idle
                                    } else {
                                        Status::NotInstalled
                                    },
                                )
                            })
                            .collect(),
                    ),
                    root,
                    sandbox,
                    clients: Default::default(),
                };
                Arc::new(hub)
            })
            .clone()
    }

    /// The servers this project uses, with how they're doing.
    pub fn statuses(&self) -> Vec<(&'static str, Status)> {
        let status = self.status.lock().unwrap();
        let mut list: Vec<_> = relevant(&self.root)
            .into_iter()
            .map(|s| {
                (
                    s.name,
                    status.get(s.name).cloned().unwrap_or(Status::NotInstalled),
                )
            })
            .collect();
        list.sort_by_key(|(name, _)| *name);
        list
    }

    pub fn recheck_installed(&self) {
        let mut status = self.status.lock().unwrap();
        for spec in relevant(&self.root) {
            let entry = status.entry(spec.name).or_insert(Status::NotInstalled);
            if *entry == Status::NotInstalled && installed(spec) {
                *entry = Status::Idle;
            }
        }
    }

    /// The running client for `path`'s language, starting it on first use.
    async fn client_for(&self, path: &Path) -> Option<(Arc<Client>, &'static ServerSpec)> {
        let ext = path.extension()?.to_str()?.to_ascii_lowercase();
        let spec = relevant(&self.root)
            .into_iter()
            .find(|s| s.extensions.contains(&ext.as_str()))?;
        if !installed(spec) {
            return None;
        }
        let mut clients = self.clients.lock().await;
        if let Some(client) = clients.get(spec.name) {
            return Some((client.clone(), spec));
        }
        if matches!(
            self.status.lock().unwrap().get(spec.name),
            Some(Status::Failed(_))
        ) {
            return None;
        }
        match Client::start(spec, &self.root, self.sandbox.as_ref()).await {
            Ok(client) => {
                self.status
                    .lock()
                    .unwrap()
                    .insert(spec.name, Status::Running);
                clients.insert(spec.name, client.clone());
                Some((client, spec))
            }
            Err(error) => {
                eprintln!("crack: {} didn't start: {error:#}", spec.name);
                self.status
                    .lock()
                    .unwrap()
                    .insert(spec.name, Status::Failed(format!("{error:#}")));
                None
            }
        }
    }

    /// Errors (and warnings) for a file just changed, waiting briefly for the server.
    pub async fn diagnostics_after_edit(&self, path: &Path, wait: Duration) -> Option<String> {
        let (client, spec) = self.client_for(path).await?;
        let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("");
        let deadline = tokio::time::Instant::now() + wait;
        let uri = uri(path);
        client.diagnostics.lock().unwrap().remove(&uri);
        client.sync(path, (spec.language_id)(ext)).await.ok()?;
        client.wait_ready(wait).await;
        // Collect this file's diagnostics until they've been quiet for a second:
        // servers often publish their own first and a build's (cargo check) after.
        loop {
            let notified = client.published.notified();
            let seen = client.diagnostics.lock().unwrap().contains_key(&uri);
            let until = if seen {
                (tokio::time::Instant::now() + Duration::from_secs(1)).min(deadline)
            } else {
                deadline
            };
            if tokio::time::timeout_at(until, notified).await.is_err() {
                break;
            }
        }
        let list = client
            .diagnostics
            .lock()
            .unwrap()
            .get(&uri)
            .cloned()
            .unwrap_or_default();
        let shown = path
            .strip_prefix(&self.root)
            .unwrap_or(path)
            .display()
            .to_string();
        format_diagnostics(&shown, &list)
    }

    /// Runs a position request (`definition`, `references`, `hover`, `rename`) at `symbol`
    /// on `line` of `path`.
    pub async fn at_symbol(
        &self,
        method: &str,
        path: &Path,
        line: usize,
        symbol: &str,
        extra: Value,
    ) -> anyhow::Result<Value> {
        let (client, spec) = self.client_for(path).await.ok_or_else(|| {
            anyhow::anyhow!("no language server is running for {}", path.display())
        })?;
        let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("");
        let uri = client.sync(path, (spec.language_id)(ext)).await?;
        // A server still loading the project answers with nothing.
        client.wait_ready(Duration::from_secs(120)).await;
        let text = tokio::fs::read_to_string(path).await?;
        let line_text = text
            .lines()
            .nth(line.saturating_sub(1))
            .ok_or_else(|| anyhow::anyhow!("{} has no line {line}", path.display()))?;
        let byte = line_text.find(symbol).ok_or_else(|| {
            anyhow::anyhow!("`{symbol}` isn't on line {line}: {}", line_text.trim())
        })?;
        // LSP columns count UTF-16 code units.
        let character: usize = line_text[..byte].chars().map(char::len_utf16).sum();
        let mut params = json!({
            "textDocument": {"uri": uri},
            "position": {"line": line - 1, "character": character},
        });
        if let (Some(object), Some(more)) = (params.as_object_mut(), extra.as_object()) {
            object.extend(more.clone());
        }
        let response = client
            .request(
                &format!("textDocument/{method}"),
                params,
                Duration::from_secs(30),
            )
            .await?;
        if let Some(error) = response.get("error") {
            anyhow::bail!(
                "{}",
                error["message"]
                    .as_str()
                    .unwrap_or("the language server returned an error")
            );
        }
        Ok(response["result"].clone())
    }

    pub fn root(&self) -> &Path {
        &self.root
    }
}

fn installed(spec: &ServerSpec) -> bool {
    crate::policy::which(spec.command[0]).is_some()
}

/// The servers for languages this project uses.
pub fn relevant(root: &Path) -> Vec<&'static ServerSpec> {
    SERVERS
        .iter()
        .filter(|s| s.markers.iter().any(|m| root.join(m).exists()))
        .collect()
}

pub fn format_diagnostics(path: &str, list: &[Value]) -> Option<String> {
    let mut errors = Vec::new();
    for d in list {
        let severity = d["severity"].as_u64().unwrap_or(1);
        if severity > 2 {
            continue;
        }
        let line = d["range"]["start"]["line"].as_u64().unwrap_or(0) + 1;
        let column = d["range"]["start"]["character"].as_u64().unwrap_or(0) + 1;
        let label = if severity == 1 { "error" } else { "warning" };
        let message = d["message"]
            .as_str()
            .unwrap_or("")
            .lines()
            .next()
            .unwrap_or("");
        errors.push((
            severity,
            format!("{path}:{line}:{column} {label}: {message}"),
        ));
    }
    if errors.is_empty() {
        return None;
    }
    errors.sort_by_key(|(s, _)| *s);
    let count_errors = errors.iter().filter(|(s, _)| *s == 1).count();
    let count_warnings = errors.len() - count_errors;
    let mut out =
        format!("Language server: {count_errors} error(s), {count_warnings} warning(s) in {path}:");
    for (_, line) in errors.iter().take(10) {
        out.push_str(&format!("\n  {line}"));
    }
    if errors.len() > 10 {
        out.push_str(&format!("\n  … and {} more", errors.len() - 10));
    }
    Some(out)
}

/// Turns locations from `definition` / `references` into `path:line:col` lines.
pub fn format_locations(root: &Path, result: &Value) -> String {
    let items: Vec<Value> = match result {
        Value::Array(list) => list.clone(),
        Value::Null => Vec::new(),
        single => vec![single.clone()],
    };
    let mut lines: Vec<String> = items
        .iter()
        .filter_map(|loc| {
            let uri = loc["uri"].as_str().or_else(|| loc["targetUri"].as_str())?;
            let range = if loc["range"].is_object() {
                &loc["range"]
            } else {
                &loc["targetSelectionRange"]
            };
            let path = path_of(uri);
            let shown = path
                .strip_prefix(root)
                .unwrap_or(&path)
                .display()
                .to_string();
            let line = range["start"]["line"].as_u64()? + 1;
            let text = std::fs::read_to_string(&path)
                .ok()
                .and_then(|t| {
                    t.lines()
                        .nth(line as usize - 1)
                        .map(|l| l.trim().to_owned())
                })
                .unwrap_or_default();
            Some(format!("{shown}:{line}: {text}"))
        })
        .collect();
    lines.dedup();
    if lines.is_empty() {
        "No results.".into()
    } else {
        lines.join("\n")
    }
}

/// Applies a rename's WorkspaceEdit; returns the files changed.
pub fn apply_workspace_edit(edit: &Value) -> anyhow::Result<Vec<PathBuf>> {
    let mut per_file: HashMap<PathBuf, Vec<Value>> = HashMap::new();
    if let Some(changes) = edit["changes"].as_object() {
        for (uri, edits) in changes {
            per_file
                .entry(path_of(uri))
                .or_default()
                .extend(edits.as_array().cloned().unwrap_or_default());
        }
    }
    if let Some(document_changes) = edit["documentChanges"].as_array() {
        for change in document_changes {
            if change.get("kind").is_some() {
                anyhow::bail!(
                    "the rename wants to create, rename or delete files; do that by hand"
                );
            }
            if let Some(uri) = change["textDocument"]["uri"].as_str() {
                per_file
                    .entry(path_of(uri))
                    .or_default()
                    .extend(change["edits"].as_array().cloned().unwrap_or_default());
            }
        }
    }
    let mut changed = Vec::new();
    for (path, mut edits) in per_file {
        let text = std::fs::read_to_string(&path)?;
        // Apply from the end so earlier offsets stay valid.
        edits.sort_by_key(|e| {
            std::cmp::Reverse((
                e["range"]["start"]["line"].as_u64(),
                e["range"]["start"]["character"].as_u64(),
            ))
        });
        let mut text = text;
        for e in edits {
            let start = offset(&text, &e["range"]["start"])
                .ok_or_else(|| anyhow::anyhow!("bad edit range in {}", path.display()))?;
            let end = offset(&text, &e["range"]["end"])
                .ok_or_else(|| anyhow::anyhow!("bad edit range in {}", path.display()))?;
            text.replace_range(start..end, e["newText"].as_str().unwrap_or(""));
        }
        std::fs::write(&path, text)?;
        changed.push(path);
    }
    changed.sort();
    Ok(changed)
}

/// A byte offset from an LSP position (UTF-16 columns).
fn offset(text: &str, position: &Value) -> Option<usize> {
    let line = position["line"].as_u64()? as usize;
    let character = position["character"].as_u64()? as usize;
    let mut start = 0;
    for (i, l) in text.split_inclusive('\n').enumerate() {
        if i == line {
            let mut units = 0;
            for (byte, c) in l.char_indices() {
                if units >= character {
                    return Some(start + byte);
                }
                units += c.len_utf16();
            }
            return Some(start + l.trim_end_matches('\n').len());
        }
        start += l.len();
    }
    (line == text.split_inclusive('\n').count()).then_some(text.len())
}

/// Whether a language server for this project is installed (so the tools are useful).
pub fn available(cwd: &Path) -> bool {
    relevant(&crate::context::project_root(cwd))
        .into_iter()
        .any(installed)
}

pub fn specs() -> Vec<crate::providers::ToolSpec> {
    let spec = |name: &str, description: &str, schema: Value| crate::providers::ToolSpec {
        name: name.into(),
        description: description.into(),
        schema,
    };
    let at = |extra: Value| {
        let mut properties = json!({
            "path": {"type": "string", "description": "The file containing the symbol."},
            "line": {"type": "integer", "description": "1-based line where the symbol appears."},
            "symbol": {"type": "string", "description": "The symbol's name exactly as written on that line."}
        });
        if let (Some(p), Some(e)) = (properties.as_object_mut(), extra.as_object()) {
            p.extend(e.clone());
        }
        properties
    };
    vec![
        spec(
            "definition",
            "Go to the definition of the symbol at a place in a file, using the language server.",
            json!({"type": "object", "properties": at(json!({})), "required": ["path", "line", "symbol"]}),
        ),
        spec(
            "references",
            "List every reference to the symbol at a place in a file, across the project, using the language server.",
            json!({"type": "object", "properties": at(json!({})), "required": ["path", "line", "symbol"]}),
        ),
        spec(
            "hover",
            "Show the type and documentation of the symbol at a place in a file, using the language server.",
            json!({"type": "object", "properties": at(json!({})), "required": ["path", "line", "symbol"]}),
        ),
        spec(
            "rename_symbol",
            "Rename a symbol everywhere it's used, safely, using the language server. Asks the user first.",
            json!({"type": "object", "properties": at(json!({"new_name": {"type": "string"}})), "required": ["path", "line", "symbol", "new_name"]}),
        ),
    ]
}

/// Runs one of the language-server tools; returns (content, title).
pub async fn tool(
    name: &str,
    input: &Value,
    cwd: &Path,
    sandbox: Option<crate::policy::Sandbox>,
) -> anyhow::Result<(String, String)> {
    let hub = Hub::for_project(cwd, sandbox);
    let path_arg = input["path"]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("missing path"))?;
    let path = if Path::new(path_arg).is_absolute() {
        PathBuf::from(path_arg)
    } else {
        cwd.join(path_arg)
    };
    let line = input["line"]
        .as_u64()
        .ok_or_else(|| anyhow::anyhow!("missing line"))? as usize;
    let symbol = input["symbol"]
        .as_str()
        .filter(|s| !s.is_empty())
        .ok_or_else(|| anyhow::anyhow!("missing symbol"))?;
    let shown = format!("{symbol} ({path_arg}:{line})");
    match name {
        "definition" | "references" => {
            let extra = if name == "references" {
                json!({"context": {"includeDeclaration": true}})
            } else {
                json!({})
            };
            let result = hub.at_symbol(name, &path, line, symbol, extra).await?;
            let text = format_locations(hub.root(), &result);
            let count = if text == "No results." {
                0
            } else {
                text.lines().count()
            };
            let verb = if name == "definition" {
                "Definition of"
            } else {
                "References to"
            };
            Ok((text, format!("{verb} {shown}: {count}")))
        }
        "hover" => {
            let result = hub
                .at_symbol("hover", &path, line, symbol, json!({}))
                .await?;
            let contents = &result["contents"];
            let text = match contents {
                Value::String(s) => s.clone(),
                Value::Object(o) => o
                    .get("value")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_owned(),
                Value::Array(items) => items
                    .iter()
                    .map(|i| {
                        i.as_str()
                            .map(str::to_owned)
                            .unwrap_or_else(|| i["value"].as_str().unwrap_or_default().to_owned())
                    })
                    .collect::<Vec<_>>()
                    .join("\n\n"),
                _ => String::new(),
            };
            Ok((
                if text.trim().is_empty() {
                    "No information.".into()
                } else {
                    text
                },
                format!("Hover {shown}"),
            ))
        }
        "rename_symbol" => {
            let new_name = input["new_name"]
                .as_str()
                .filter(|s| !s.is_empty())
                .ok_or_else(|| anyhow::anyhow!("missing new_name"))?;
            let edit = hub
                .at_symbol("rename", &path, line, symbol, json!({"newName": new_name}))
                .await?;
            if edit.is_null() {
                anyhow::bail!("the language server can't rename `{symbol}` there");
            }
            let changed = apply_workspace_edit(&edit)?;
            let files: Vec<String> = changed
                .iter()
                .map(|p| {
                    p.strip_prefix(hub.root())
                        .unwrap_or(p)
                        .display()
                        .to_string()
                })
                .collect();
            // Tell the server about the new text.
            for file in &changed {
                let _ = hub
                    .diagnostics_after_edit(file, Duration::from_millis(1))
                    .await;
            }
            Ok((
                format!(
                    "Renamed `{symbol}` to `{new_name}` in {} file(s):\n{}",
                    files.len(),
                    files.join("\n")
                ),
                format!("Rename {symbol} → {new_name} ({} files)", files.len()),
            ))
        }
        other => anyhow::bail!("unknown language-server tool {other}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uris_and_edits() {
        let dir = std::env::temp_dir().join(format!("crack lsp {}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("a é.rs");
        std::fs::write(&file, "fn old() {}\nfn main() { old(); }\n").unwrap();
        let u = uri(&file);
        assert!(u.contains("%20") && u.contains("%C3%A9"), "{u}");
        assert_eq!(path_of(&u), file);
        let edit = json!({"changes": {u.clone(): [
            {"range": {"start": {"line": 0, "character": 3}, "end": {"line": 0, "character": 6}}, "newText": "new"},
            {"range": {"start": {"line": 1, "character": 12}, "end": {"line": 1, "character": 15}}, "newText": "new"}
        ]}});
        assert_eq!(
            apply_workspace_edit(&edit).unwrap(),
            std::slice::from_ref(&file)
        );
        assert_eq!(
            std::fs::read_to_string(&file).unwrap(),
            "fn new() {}\nfn main() { new(); }\n"
        );
        let diags = [
            json!({"severity": 1, "range": {"start": {"line": 4, "character": 2}}, "message": "mismatched types\nmore"}),
            json!({"severity": 3, "range": {"start": {"line": 1, "character": 0}}, "message": "hint"}),
        ];
        assert_eq!(
            format_diagnostics("src/a.rs", &diags).unwrap(),
            "Language server: 1 error(s), 0 warning(s) in src/a.rs:\n  src/a.rs:5:3 error: mismatched types"
        );
        std::fs::remove_dir_all(dir).unwrap();
    }
}
