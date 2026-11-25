//! A small MCP server for testing crack's MCP client.
//!
//!   cargo run --example mock_mcp                 # stdio
//!   cargo run --example mock_mcp -- --http 8766  # Streamable HTTP on 127.0.0.1:8766/mcp
//!
//! Tools: `echo {text}`, `add {a, b}`, and `fail` (returns an error result). Over HTTP,
//! tool calls are answered as an SSE stream and other requests as plain JSON, and the
//! session id from `initialize` is required afterwards.

use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};

fn handle(message: &Value) -> Option<Value> {
    let id = message.get("id")?.clone();
    let method = message["method"].as_str().unwrap_or_default();
    let result = match method {
        "initialize" => json!({
            "protocolVersion": "2025-06-18",
            "capabilities": {"tools": {}},
            "serverInfo": {"name": "mock-mcp", "version": "1.0"},
        }),
        "ping" => json!({}),
        "tools/list" => json!({"tools": [
            {"name": "echo", "description": "Echo the given text back.",
             "inputSchema": {"type": "object", "properties": {"text": {"type": "string"}}, "required": ["text"]}},
            {"name": "add", "description": "Add two numbers.",
             "inputSchema": {"type": "object", "properties": {"a": {"type": "number"}, "b": {"type": "number"}}, "required": ["a", "b"]}},
            {"name": "fail", "description": "Always fails.", "inputSchema": {"type": "object", "properties": {}}},
        ]}),
        "tools/call" => {
            let args = &message["params"]["arguments"];
            match message["params"]["name"].as_str().unwrap_or_default() {
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
        if let Some(reply) = handle(&message) {
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
            let response = if method != "initialize" && session.as_deref() != Some("mock-session") {
                "HTTP/1.1 400 Bad Request\r\ncontent-length: 18\r\n\r\nmissing session id"
                    .to_owned()
            } else {
                match handle(&message) {
                    None => {
                        "HTTP/1.1 202 Accepted\r\nconnection: close\r\ncontent-length: 0\r\n\r\n"
                            .to_owned()
                    }
                    Some(reply) if method == "tools/call" => {
                        let body = format!("event: message\ndata: {reply}\n\n");
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
