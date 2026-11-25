//! A small MCP server for testing crack's MCP client.
//!
//!   cargo run --example mock_mcp                 # stdio
//!   cargo run --example mock_mcp -- --http 8766  # Streamable HTTP on 127.0.0.1:8766/mcp
//!
//! Tools: `echo {text}`, `add {a, b}`, `fail` (returns an error result), and `grow`
//! (adds a `late` tool and sends `tools/list_changed`). One resource
//! (`file:///notes/readme.md`) and one prompt (`review {focus}`). Over HTTP,
//! tool calls are answered as an SSE stream and other requests as plain JSON, and the
//! session id from `initialize` is required afterwards.

use serde_json::{Value, json};
use std::sync::atomic::{AtomicBool, Ordering};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};

/// Set by the `grow` tool: a `late` tool appears, and clients are told.
static GROWN: AtomicBool = AtomicBool::new(false);

const LIST_CHANGED: &str = r#"{"jsonrpc":"2.0","method":"notifications/tools/list_changed"}"#;

/// The messages to send back: notifications (if any), then the reply.
fn respond(message: &Value) -> Vec<Value> {
    let Some(reply) = handle(message) else {
        return Vec::new();
    };
    if message["method"] == "tools/call" && message["params"]["name"] == "grow" {
        return vec![serde_json::from_str(LIST_CHANGED).unwrap(), reply];
    }
    vec![reply]
}

fn handle(message: &Value) -> Option<Value> {
    let id = message.get("id")?.clone();
    let method = message["method"].as_str().unwrap_or_default();
    let result = match method {
        "initialize" => json!({
            "protocolVersion": "2025-06-18",
            "capabilities": {"tools": {"listChanged": true}, "resources": {}, "prompts": {}},
            "serverInfo": {"name": "mock-mcp", "version": "1.0"},
        }),
        "ping" => json!({}),
        "tools/list" => {
            let mut tools = vec![
                json!({"name": "echo", "description": "Echo the given text back.",
                 "inputSchema": {"type": "object", "properties": {"text": {"type": "string"}}, "required": ["text"]}}),
                json!({"name": "add", "description": "Add two numbers.",
                 "inputSchema": {"type": "object", "properties": {"a": {"type": "number"}, "b": {"type": "number"}}, "required": ["a", "b"]}}),
                json!({"name": "fail", "description": "Always fails.", "inputSchema": {"type": "object", "properties": {}}}),
                json!({"name": "grow", "description": "Adds the `late` tool.", "inputSchema": {"type": "object", "properties": {}}}),
            ];
            if GROWN.load(Ordering::SeqCst) {
                tools.push(json!({"name": "late", "description": "Added at runtime.", "inputSchema": {"type": "object", "properties": {}}}));
            }
            json!({"tools": tools})
        }
        "resources/list" => json!({"resources": [
            {"uri": "file:///notes/readme.md", "name": "Project notes", "mimeType": "text/markdown"},
        ]}),
        "resources/read" => match message["params"]["uri"].as_str() {
            Some(uri @ "file:///notes/readme.md") => json!({"contents": [
                {"uri": uri, "mimeType": "text/markdown", "text": "# Notes\nDeploys happen on Tuesdays."},
            ]}),
            other => {
                return Some(json!({"jsonrpc": "2.0", "id": id,
                    "error": {"code": -32002, "message": format!("no resource {}", other.unwrap_or("?"))}}));
            }
        },
        "prompts/list" => json!({"prompts": [
            {"name": "review", "description": "Review the code for problems.",
             "arguments": [{"name": "focus", "description": "What to look at", "required": false}]},
        ]}),
        "prompts/get" => {
            let focus = message["params"]["arguments"]["focus"]
                .as_str()
                .unwrap_or("correctness");
            json!({"messages": [
                {"role": "user", "content": {"type": "text", "text": format!("Review the code, focusing on {focus}.")}},
            ]})
        }
        "tools/call" => {
            let args = &message["params"]["arguments"];
            match message["params"]["name"].as_str().unwrap_or_default() {
                "grow" => {
                    GROWN.store(true, Ordering::SeqCst);
                    json!({"content": [{"type": "text", "text": "grown"}]})
                }
                "late" => json!({"content": [{"type": "text", "text": "late tool ran"}]}),
                "echo" => {
                    json!({"content": [{"type": "text", "text": format!("echo: {}", args["text"].as_str().unwrap_or(""))}]})
                }
                "add" => {
                    let sum = args["a"].as_f64().unwrap_or(0.0) + args["b"].as_f64().unwrap_or(0.0);
                    json!({"content": [{"type": "text", "text": sum.to_string()}]})
                }
                "fail" => {
                    json!({"content": [{"type": "text", "text": "this tool always fails"}], "isError": true})
                }
                other => {
                    return Some(json!({"jsonrpc": "2.0", "id": id,
                    "error": {"code": -32602, "message": format!("unknown tool {other}")}}));
                }
            }
        }
        other => {
            return Some(json!({"jsonrpc": "2.0", "id": id,
                "error": {"code": -32601, "message": format!("unknown method {other}")}}));
        }
    };
    Some(json!({"jsonrpc": "2.0", "id": id, "result": result}))
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().collect();
    if let Some(port) = args
        .iter()
        .position(|a| a == "--http")
        .and_then(|i| args.get(i + 1))
    {
        return http(port).await;
    }
    eprintln!("mock-mcp: stdio");
    let mut lines = BufReader::new(tokio::io::stdin()).lines();
    let mut stdout = tokio::io::stdout();
    while let Some(line) = lines.next_line().await? {
        let Ok(message) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        for reply in respond(&message) {
            stdout.write_all(format!("{reply}\n").as_bytes()).await?;
            stdout.flush().await?;
        }
    }
    Ok(())
}

async fn http(port: &str) -> anyhow::Result<()> {
    let listener = tokio::net::TcpListener::bind(format!("127.0.0.1:{port}")).await?;
    eprintln!("mock-mcp: http://127.0.0.1:{port}/mcp");
    loop {
        let (mut socket, _) = listener.accept().await?;
        tokio::spawn(async move {
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
            let head = String::from_utf8_lossy(&buffer[..header_end]).to_lowercase();
            let length: usize = head
                .lines()
                .find_map(|l| l.strip_prefix("content-length:"))
                .and_then(|v| v.trim().parse().ok())
                .unwrap_or(0);
            let session = head
                .lines()
                .find_map(|l| l.strip_prefix("mcp-session-id:"))
                .map(|v| v.trim().to_owned());
            while buffer.len() < header_end + length {
                let Ok(n) = socket.read(&mut chunk).await else {
                    return;
                };
                if n == 0 {
                    break;
                }
                buffer.extend_from_slice(&chunk[..n]);
            }
            let message: Value =
                serde_json::from_slice(&buffer[header_end..]).unwrap_or(Value::Null);
            let method = message["method"].as_str().unwrap_or_default().to_owned();
            let verb = head.split_whitespace().next().unwrap_or("").to_owned();
            let response = if verb == "get" {
                // No server-to-client stream outside of POST replies.
                "HTTP/1.1 405 Method Not Allowed\r\nconnection: close\r\ncontent-length: 0\r\n\r\n"
                    .to_owned()
            } else if verb == "delete" {
                "HTTP/1.1 200 OK\r\nconnection: close\r\ncontent-length: 0\r\n\r\n".to_owned()
            } else if method != "initialize" && session.as_deref() != Some("mock-session") {
                "HTTP/1.1 400 Bad Request\r\nconnection: close\r\ncontent-length: 18\r\n\r\nmissing session id"
                    .to_owned()
            } else {
                let mut replies = respond(&message);
                match replies.pop() {
                    None => {
                        "HTTP/1.1 202 Accepted\r\nconnection: close\r\ncontent-length: 0\r\n\r\n"
                            .to_owned()
                    }
                    Some(reply) if method == "tools/call" => {
                        // Notifications ride on the call's stream, before the reply.
                        let mut body: String = replies
                            .iter()
                            .map(|n| format!("event: message\ndata: {n}\n\n"))
                            .collect();
                        body.push_str(&format!("event: message\ndata: {reply}\n\n"));
                        format!(
                            "HTTP/1.1 200 OK\r\nconnection: close\r\ncontent-type: text/event-stream\r\ncontent-length: {}\r\n\r\n{body}",
                            body.len()
                        )
                    }
                    Some(reply) => {
                        let body = reply.to_string();
                        format!(
                            "HTTP/1.1 200 OK\r\nconnection: close\r\ncontent-type: application/json\r\nmcp-session-id: mock-session\r\ncontent-length: {}\r\n\r\n{body}",
                            body.len()
                        )
                    }
                }
            };
            let _ = socket.write_all(response.as_bytes()).await;
        });
    }
}
