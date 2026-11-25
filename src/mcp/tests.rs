//! MCP client tests against an in-process Streamable HTTP server that can also play
//! an OAuth authorization server (metadata, dynamic registration, PKCE, tokens).

use super::*;
use std::sync::atomic::{AtomicBool, Ordering};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

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

#[test]
fn prompt_arguments_by_name_and_position() {
    let prompt = McpPrompt {
        server: "s".into(),
        name: "review".into(),
        description: String::new(),
        arguments: vec![("focus".into(), false), ("depth".into(), false)],
    };
    let args = prompt_arguments(&prompt, "depth=deep error handling");
    assert_eq!(args["depth"], "deep");
    assert_eq!(args["focus"], "error handling");
    assert!(prompt_arguments(&prompt, "").is_empty());
}

/// The test server's state.
struct Mock {
    base: String,
    oauth: bool,
    grown: AtomicBool,
    /// The PKCE challenge sent to /authorize, checked at /token.
    challenge: Mutex<Option<String>>,
}

const TOKEN: &str = "token-1";

async fn serve_mock(oauth: bool) -> Arc<Mock> {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let mock = Arc::new(Mock {
        base: format!("http://127.0.0.1:{}", listener.local_addr().unwrap().port()),
        oauth,
        grown: AtomicBool::new(false),
        challenge: Mutex::new(None),
    });
    let server = mock.clone();
    tokio::spawn(async move {
        loop {
            let Ok((socket, _)) = listener.accept().await else {
                return;
            };
            let server = server.clone();
            tokio::spawn(async move { server.connection(socket).await });
        }
    });
    mock
}

fn reply(status: &str, headers: &[(&str, String)], body: &str) -> String {
    let mut head = format!(
        "HTTP/1.1 {status}\r\nconnection: close\r\ncontent-length: {}\r\n",
        body.len()
    );
    for (name, value) in headers {
        head.push_str(&format!("{name}: {value}\r\n"));
    }
    format!("{head}\r\n{body}")
}

fn form(body: &str) -> HashMap<String, String> {
    url::form_urlencoded::parse(body.as_bytes())
        .into_owned()
        .collect()
}

impl Mock {
    async fn connection(&self, mut socket: tokio::net::TcpStream) {
        let mut buffer = Vec::new();
        let mut chunk = [0u8; 8192];
        let header_end = loop {
            let Ok(n) = socket.read(&mut chunk).await else {
                return;
            };
            if n == 0 {
                return;
            }
            buffer.extend_from_slice(&chunk[..n]);
            if let Some(p) = buffer.windows(4).position(|w| w == b"\r\n\r\n") {
                break p + 4;
            }
        };
        let head = String::from_utf8_lossy(&buffer[..header_end]).into_owned();
        let header = |name: &str| {
            head.lines().find_map(|l| {
                let (k, v) = l.split_once(':')?;
                k.trim()
                    .eq_ignore_ascii_case(name)
                    .then(|| v.trim().to_owned())
            })
        };
        let length: usize = header("content-length")
            .and_then(|v| v.parse().ok())
            .unwrap_or(0);
        while buffer.len() < header_end + length {
            let Ok(n) = socket.read(&mut chunk).await else {
                return;
            };
            if n == 0 {
                break;
            }
            buffer.extend_from_slice(&chunk[..n]);
        }
        let body = String::from_utf8_lossy(&buffer[header_end..]).into_owned();
        let mut request_line = head.lines().next().unwrap_or("").split_whitespace();
        let method = request_line.next().unwrap_or("").to_owned();
        let target = request_line.next().unwrap_or("/").to_owned();
        let (path, query) = target.split_once('?').unwrap_or((&target, ""));
        let response = self
            .route(&method, path, query, &body, header("authorization"))
            .await;
        let _ = socket.write_all(response.as_bytes()).await;
    }

    async fn route(
        &self,
        method: &str,
        path: &str,
        query: &str,
        body: &str,
        auth: Option<String>,
    ) -> String {
        let base = &self.base;
        let json_reply = |value: Value| {
            reply(
                "200 OK",
                &[("content-type", "application/json".into())],
                &value.to_string(),
            )
        };
        if path.starts_with("/.well-known/oauth-protected-resource") && self.oauth {
            return json_reply(json!({
                "resource": format!("{base}/mcp"),
                "authorization_servers": [base],
                "bearer_methods_supported": ["header"],
            }));
        }
        if (path.starts_with("/.well-known/oauth-authorization-server")
            || path.starts_with("/.well-known/openid-configuration"))
            && self.oauth
        {
            return json_reply(json!({
                "issuer": base,
                "authorization_endpoint": format!("{base}/authorize"),
                "token_endpoint": format!("{base}/token"),
                "registration_endpoint": format!("{base}/register"),
                "response_types_supported": ["code"],
                "grant_types_supported": ["authorization_code", "refresh_token"],
                "code_challenge_methods_supported": ["S256"],
                "token_endpoint_auth_methods_supported": ["none"],
            }));
        }
        match (method, path) {
            ("POST", "/register") => {
                let request: Value = serde_json::from_str(body).unwrap_or_default();
                let answer = json!({
                    "client_id": "client-1",
                    "redirect_uris": request["redirect_uris"],
                    "token_endpoint_auth_method": "none",
                    "grant_types": ["authorization_code", "refresh_token"],
                    "response_types": ["code"],
                });
                reply(
                    "201 Created",
                    &[("content-type", "application/json".into())],
                    &answer.to_string(),
                )
            }
            ("GET", "/authorize") => {
                let params = form(query);
                assert_eq!(
                    params.get("code_challenge_method").map(String::as_str),
                    Some("S256")
                );
                *self.challenge.lock().unwrap() = params.get("code_challenge").cloned();
                let location = format!(
                    "{}?code=code-1&state={}",
                    params["redirect_uri"],
                    params.get("state").cloned().unwrap_or_default()
                );
                reply("302 Found", &[("location", location)], "")
            }
            ("POST", "/token") => {
                let params = form(body);
                let verifier_ok = match (
                    params.get("code_verifier"),
                    self.challenge.lock().unwrap().clone(),
                ) {
                    (Some(verifier), Some(challenge)) => {
                        use base64::Engine;
                        use sha2::Digest;
                        let digest = sha2::Sha256::digest(verifier.as_bytes());
                        base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(digest) == challenge
                    }
                    _ => false,
                };
                if params.get("code").map(String::as_str) != Some("code-1") || !verifier_ok {
                    return reply(
                        "400 Bad Request",
                        &[("content-type", "application/json".into())],
                        r#"{"error":"invalid_grant"}"#,
                    );
                }
                json_reply(
                    json!({"access_token": TOKEN, "token_type": "Bearer", "expires_in": 3600, "refresh_token": "refresh-1"}),
                )
            }
            (_, "/mcp") => {
                if self.oauth && auth.as_deref() != Some(&format!("Bearer {TOKEN}")) {
                    return reply(
                        "401 Unauthorized",
                        &[(
                            "www-authenticate",
                            format!(
                                "Bearer resource_metadata=\"{base}/.well-known/oauth-protected-resource/mcp\""
                            ),
                        )],
                        "",
                    );
                }
                match method {
                    "GET" => reply("405 Method Not Allowed", &[], ""),
                    "DELETE" => reply("200 OK", &[], ""),
                    _ => self.rpc(body),
                }
            }
            _ => reply("404 Not Found", &[], ""),
        }
    }

    fn rpc(&self, body: &str) -> String {
        let message: Value = serde_json::from_str(body).unwrap_or_default();
        let Some(id) = message.get("id").cloned() else {
            return reply("202 Accepted", &[], "");
        };
        let params = &message["params"];
        let mut notifications = Vec::new();
        let result = match message["method"].as_str().unwrap_or_default() {
            "initialize" => json!({
                "protocolVersion": "2025-06-18",
                "capabilities": {"tools": {"listChanged": true}, "resources": {}, "prompts": {}},
                "serverInfo": {"name": "test", "version": "1"},
            }),
            "tools/list" => {
                let mut tools = vec![
                    json!({"name": "echo", "inputSchema": {"type": "object", "properties": {"text": {"type": "string"}}}}),
                    json!({"name": "grow", "inputSchema": {"type": "object"}}),
                ];
                if self.grown.load(Ordering::SeqCst) {
                    tools.push(json!({"name": "late", "inputSchema": {"type": "object"}}));
                }
                json!({"tools": tools})
            }
            "tools/call" => match params["name"].as_str() {
                Some("echo") => {
                    json!({"content": [{"type": "text", "text": format!("echo: {}", params["arguments"]["text"].as_str().unwrap_or(""))}]})
                }
                Some("grow") => {
                    self.grown.store(true, Ordering::SeqCst);
                    notifications.push(
                        json!({"jsonrpc": "2.0", "method": "notifications/tools/list_changed"}),
                    );
                    json!({"content": [{"type": "text", "text": "grown"}]})
                }
                _ => {
                    json!({"content": [{"type": "text", "text": "no such tool"}], "isError": true})
                }
            },
            "resources/list" => json!({"resources": [{"uri": "memo://plan", "name": "Plan"}]}),
            "resources/read" => {
                json!({"contents": [{"uri": params["uri"], "text": "Ship on Friday."}]})
            }
            "prompts/list" => {
                json!({"prompts": [{"name": "review", "arguments": [{"name": "focus"}]}]})
            }
            "prompts/get" => json!({"messages": [{"role": "user", "content": {"type": "text",
                "text": format!("Review for {}.", params["arguments"]["focus"].as_str().unwrap_or("bugs"))}}]}),
            "ping" => json!({}),
            other => {
                let error = json!({"jsonrpc": "2.0", "id": id, "error": {"code": -32601, "message": format!("no {other}")}});
                return reply(
                    "200 OK",
                    &[("content-type", "application/json".into())],
                    &error.to_string(),
                );
            }
        };
        let answer = json!({"jsonrpc": "2.0", "id": id, "result": result});
        if notifications.is_empty() {
            reply(
                "200 OK",
                &[
                    ("content-type", "application/json".into()),
                    ("mcp-session-id", "s1".into()),
                ],
                &answer.to_string(),
            )
        } else {
            // Notifications ride on the call's event stream, before the reply.
            let mut events: String = notifications
                .iter()
                .map(|n| format!("event: message\ndata: {n}\n\n"))
                .collect();
            events.push_str(&format!("event: message\ndata: {answer}\n\n"));
            reply(
                "200 OK",
                &[("content-type", "text/event-stream".into())],
                &events,
            )
        }
    }
}

fn hub_for(mock: &Mock) -> Arc<McpHub> {
    let mut configs = BTreeMap::new();
    configs.insert(
        "t".to_owned(),
        McpServerConfig {
            url: Some(format!("{}/mcp", mock.base)),
            ..Default::default()
        },
    );
    McpHub::new(configs, reqwest::Client::new())
}

fn status(hub: &McpHub) -> Status {
    hub.snapshot()[0].status.clone()
}

#[tokio::test]
async fn tools_resources_prompts_and_live_tool_changes() {
    let mock = serve_mock(false).await;
    let hub = hub_for(&mock);
    hub.connect_all().await;
    assert_eq!(status(&hub), Status::Connected);
    let names: Vec<String> = hub.tools().into_iter().map(|t| t.exposed).collect();
    assert_eq!(names, ["mcp__t__echo", "mcp__t__grow"]);
    assert_eq!(
        hub.call("mcp__t__echo", json!({"text": "hi"}))
            .await
            .unwrap(),
        ("echo: hi".into(), false)
    );

    assert_eq!(hub.resources()[0].uri, "memo://plan");
    assert_eq!(
        hub.read_resource("t", "memo://plan").await.unwrap(),
        "Ship on Friday."
    );
    assert_eq!(hub.prompts()[0].command(), "t:review");
    let blocks = hub
        .expand_prompt("/t:review security, then check @t:memo://plan.")
        .await;
    assert_eq!(blocks.len(), 2, "{blocks:?}");
    assert!(
        blocks[0].ends_with("Review for security, then check @t:memo://plan.."),
        "{}",
        blocks[0]
    );
    assert!(blocks[1].ends_with("Ship on Friday."));
    assert!(
        hub.expand_prompt("no references here @nobody:x")
            .await
            .is_empty()
    );

    // `grow` adds a tool and tells the client; the list refreshes by itself.
    assert_eq!(
        hub.call("mcp__t__grow", json!({})).await.unwrap().0,
        "grown"
    );
    for _ in 0..50 {
        if hub.tools().iter().any(|t| t.name == "late") {
            return;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!(
        "tools/list_changed didn't refresh the tool list: {:?}",
        hub.tools()
    );
}

#[tokio::test]
async fn oauth_sign_in_stores_tokens_and_reconnects() {
    let dir = std::env::temp_dir().join(format!("crack-mcp-oauth-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&dir).unwrap();
    let credentials = dir.join("credentials.toml");
    *TEST_CREDENTIALS.lock().unwrap() = Some(credentials.clone());

    let mock = serve_mock(true).await;
    let hub = hub_for(&mock);
    hub.connect_all().await;
    assert_eq!(status(&hub), Status::NeedsAuth);

    // The "browser": follows the authorization redirect back to crack's callback.
    let browser = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .unwrap();
    hub.sign_in("t", move |url| {
        tokio::spawn(async move {
            let answer = browser.get(&url).send().await.unwrap();
            let location = answer.headers()["location"].to_str().unwrap().to_owned();
            let page = browser
                .get(&location)
                .send()
                .await
                .unwrap()
                .text()
                .await
                .unwrap();
            assert!(page.contains("Signed in to t"));
        });
    })
    .await
    .unwrap();
    assert_eq!(status(&hub), Status::Connected);
    assert_eq!(
        hub.call("mcp__t__echo", json!({"text": "authorized"}))
            .await
            .unwrap()
            .0,
        "echo: authorized"
    );

    // The tokens were saved privately, and a fresh start uses them without signing in.
    let saved = std::fs::read_to_string(&credentials).unwrap();
    assert!(saved.contains("[mcp_oauth]") && saved.contains(TOKEN));
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            std::fs::metadata(&credentials)
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
    }
    let fresh = hub_for(&mock);
    fresh.connect_all().await;
    assert_eq!(status(&fresh), Status::Connected);

    // Signing out forgets them.
    fresh.sign_out("t").unwrap();
    let again = hub_for(&mock);
    again.connect_all().await;
    assert_eq!(status(&again), Status::NeedsAuth);
    *TEST_CREDENTIALS.lock().unwrap() = None;
    std::fs::remove_dir_all(dir).unwrap();
}
