//! A scripted model server for testing crack without an API key.
//!
//!   cargo run --example mock_provider            # listens on 127.0.0.1:8765
//!   CRACK_BASE_URL=http://127.0.0.1:8765/v1 crack run -m custom/mock "hi"
//!   ANTHROPIC_BASE_URL=http://127.0.0.1:8765 ANTHROPIC_API_KEY=x crack run -m anthropic/mock "hi"
//!
//! It speaks OpenAI chat completions (`/v1/chat/completions`, `/v1/models`) and the
//! Anthropic Messages API (`/v1/messages`). The first reply in a turn thinks, then
//! calls `bash` with the user's text when it starts with `run:`, or `list` otherwise;
//! once a tool result arrives it answers with a summary of it.

use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let addr = std::env::var("MOCK_ADDR").unwrap_or_else(|_| "127.0.0.1:8765".into());
    let listener = TcpListener::bind(&addr).await?;
    eprintln!("mock provider on http://{addr}");
    loop {
        let (mut socket, _) = listener.accept().await?;
        tokio::spawn(async move {
            let _ = handle(&mut socket).await;
        });
    }
}

async fn read_request(
    socket: &mut tokio::net::TcpStream,
) -> anyhow::Result<(String, String, Vec<u8>)> {
    let mut buffer = Vec::new();
    let mut chunk = [0u8; 8192];
    let header_end = loop {
        let n = socket.read(&mut chunk).await?;
        if n == 0 {
            anyhow::bail!("closed");
        }
        buffer.extend_from_slice(&chunk[..n]);
        if let Some(pos) = buffer.windows(4).position(|w| w == b"\r\n\r\n") {
            break pos + 4;
        }
    };
    let head = String::from_utf8_lossy(&buffer[..header_end]).to_string();
    let mut lines = head.lines();
    let first = lines.next().unwrap_or_default().to_owned();
    let mut parts = first.split_whitespace();
    let method = parts.next().unwrap_or_default().to_owned();
    let path = parts.next().unwrap_or_default().to_owned();
    let length: usize = lines
        .filter_map(|l| l.split_once(':'))
        .find(|(k, _)| k.eq_ignore_ascii_case("content-length"))
        .and_then(|(_, v)| v.trim().parse().ok())
        .unwrap_or(0);
    let mut body = buffer[header_end..].to_vec();
    while body.len() < length {
        let n = socket.read(&mut chunk).await?;
        if n == 0 {
            break;
        }
        body.extend_from_slice(&chunk[..n]);
    }
    Ok((method, path, body))
}

async fn handle(socket: &mut tokio::net::TcpStream) -> anyhow::Result<()> {
    let (method, path, body) = read_request(socket).await?;
    let path = path.split('?').next().unwrap_or_default().to_owned();
    if method == "GET" && path.ends_with("/models") {
        let body = json!({"data": [{"id": "mock"}, {"id": "mock-large"}]}).to_string();
        let response = format!(
            "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
            body.len()
        );
        socket.write_all(response.as_bytes()).await?;
        return Ok(());
    }
    let request: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
    eprintln!(
        "{method} {path} effort={} reasoning={}",
        request["output_config"]["effort"], request["reasoning_effort"]
    );
    let events = if path.ends_with("/messages") {
        anthropic(&request)
    } else {
        openai(&request)
    };
    // MOCK_DELAY_MS: think before answering, to exercise "waiting" states.
    if let Some(ms) = std::env::var("MOCK_DELAY_MS")
        .ok()
        .and_then(|v| v.parse().ok())
    {
        tokio::time::sleep(std::time::Duration::from_millis(ms)).await;
    }
    socket
        .write_all(
            b"HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\nconnection: close\r\n\r\n",
        )
        .await?;
    for event in events {
        socket.write_all(event.as_bytes()).await?;
        socket.flush().await?;
        tokio::time::sleep(std::time::Duration::from_millis(30)).await;
    }
    Ok(())
}

enum Plan {
    Call {
        name: String,
        input: Value,
    },
    /// Several calls in one response (`parallel: N <prompt>` starts N subagents).
    Calls(Vec<(String, Value)>),
    Answer(String),
}

/// Decides the reply from the last message: a tool call for a fresh prompt, an
/// answer after a tool result.
fn plan(last_user_text: Option<&str>, tool_result: Option<&str>) -> Plan {
    if last_user_text.is_some_and(|t| t.contains("Summarize this conversation so that")) {
        return Plan::Answer(
            "## Goal\nThe user is testing crack.\n\n## Next\nAnswer the latest request.".into(),
        );
    }
    if let Some(result) = tool_result {
        let first: String = result.lines().take(3).collect::<Vec<_>>().join(" / ");
        return Plan::Answer(format!(
            "## Done\n\nThe tool returned:\n\n```\n{first}\n```\n\nEverything **worked**. Use `crack` again any time."
        ));
    }
    let text = last_user_text.unwrap_or_default();
    if text.contains("Summarize this conversation so that") {
        return Plan::Answer(
            "## Goal\nThe user is testing crack.\n\n## Next\nAnswer the latest request.".into(),
        );
    }
    if text.starts_with("This conversation was compacted") {
        return Plan::Answer("Picking up from the summary.".into());
    }
    if let Some(i) = text.find("parallel:") {
        let rest = text[i + 9..].trim();
        let (n, prompt) = rest
            .split_once(' ')
            .unwrap_or((rest, "explore the project"));
        let n: usize = n.parse().unwrap_or(2);
        return Plan::Calls(
            (1..=n)
                .map(|k| {
                    let input = json!({"description": format!("area {k}"), "prompt": format!("{prompt} (part {k})"), "subagent_type": "explore"});
                    ("task".to_owned(), input)
                })
                .collect(),
        );
    }
    // "say: <markdown>" answers with that text (`\n` for newlines), no tools.
    if let Some(i) = text.find("say:") {
        let rest = text[i + 4..].lines().next().unwrap_or("").trim();
        return Plan::Answer(rest.replace("\\n", "\n"));
    }
    // "call: <tool> <json args>" calls any tool, e.g. an MCP one.
    if let Some(i) = text.find("call:") {
        // Only the first line: crack may append context (memories) after it.
        let rest = text[i + 5..].lines().next().unwrap_or("").trim();
        let (name, args) = rest.split_once(' ').unwrap_or((rest, "{}"));
        return Plan::Call {
            name: name.to_owned(),
            input: serde_json::from_str(args).unwrap_or(json!({})),
        };
    }
    match text.find("run:") {
        Some(i) => Plan::Call {
            name: "bash".into(),
            input: json!({"command": text[i + 4..].trim(), "description": "Run the requested command"}),
        },
        None => Plan::Call {
            name: "list".into(),
            input: json!({}),
        },
    }
}

fn words(text: &str) -> Vec<String> {
    text.split_inclusive(' ').map(str::to_owned).collect()
}

fn openai(request: &Value) -> Vec<String> {
    let messages = request["messages"].as_array().cloned().unwrap_or_default();
    let last = messages.last().cloned().unwrap_or(Value::Null);
    let plan = if last["role"] == "tool" {
        plan(None, last["content"].as_str())
    } else {
        plan(last["content"].as_str(), None)
    };
    let chunk = |delta: Value, finish: Value| {
        format!(
            "data: {}\n\n",
            json!({"choices": [{"index": 0, "delta": delta, "finish_reason": finish}]})
        )
    };
    let mut out = vec![chunk(
        json!({"reasoning_content": "The user wants me to look at the project first, so I will check the files and the build setup before answering, and keep the reply short and friendly because this looks like a quick request."}),
        Value::Null,
    )];
    match plan {
        Plan::Calls(calls) => {
            out.push(chunk(
                json!({"content": "Splitting this up. "}),
                Value::Null,
            ));
            for (index, (name, input)) in calls.into_iter().enumerate() {
                out.push(chunk(
                    json!({"tool_calls": [{"index": index, "id": format!("call_{index}"), "type": "function",
                        "function": {"name": name, "arguments": input.to_string()}}]}),
                    Value::Null,
                ));
            }
            out.push(chunk(json!({}), json!("tool_calls")));
        }
        Plan::Call { name, input } => {
            out.push(chunk(json!({"content": "I'll check that. "}), Value::Null));
            out.push(chunk(
                json!({"tool_calls": [{"index": 0, "id": "call_1", "type": "function",
                    "function": {"name": name, "arguments": ""}}]}),
                Value::Null,
            ));
            let args = input.to_string();
            let (a, b) = args.split_at(args.len() / 2);
            for piece in [a, b] {
                out.push(chunk(
                    json!({"tool_calls": [{"index": 0, "function": {"arguments": piece}}]}),
                    Value::Null,
                ));
            }
            out.push(chunk(json!({}), json!("tool_calls")));
        }
        Plan::Answer(text) => {
            for word in words(&text) {
                out.push(chunk(json!({"content": word}), Value::Null));
            }
            out.push(chunk(json!({}), json!("stop")));
        }
    }
    out.push(format!(
        "data: {}\n\n",
        json!({"choices": [], "usage": {"prompt_tokens": request.to_string().len() / 4, "completion_tokens": 56}})
    ));
    out.push("data: [DONE]\n\n".into());
    out
}

fn anthropic(request: &Value) -> Vec<String> {
    let messages = request["messages"].as_array().cloned().unwrap_or_default();
    let last = messages.last().cloned().unwrap_or(Value::Null);
    let blocks = last["content"].as_array().cloned().unwrap_or_default();
    let tool_result = blocks
        .iter()
        .find(|b| b["type"] == "tool_result")
        .and_then(|b| b["content"].as_str());
    let text = blocks
        .iter()
        .rev()
        .find(|b| b["type"] == "text")
        .and_then(|b| b["text"].as_str());
    let plan = plan(text, tool_result);
    let event = |name: &str, data: Value| format!("event: {name}\ndata: {data}\n\n");
    let mut out = vec![
        event(
            "message_start",
            json!({"type": "message_start", "message": {"usage": {"input_tokens": 900, "cache_read_input_tokens": 300, "output_tokens": 1}}}),
        ),
        event(
            "content_block_start",
            json!({"type": "content_block_start", "index": 0, "content_block": {"type": "thinking", "thinking": ""}}),
        ),
        event(
            "content_block_delta",
            json!({"type": "content_block_delta", "index": 0, "delta": {"type": "thinking_delta", "thinking": "**Inspecting** the request."}}),
        ),
        event(
            "content_block_delta",
            json!({"type": "content_block_delta", "index": 0, "delta": {"type": "signature_delta", "signature": "sig-abc"}}),
        ),
        event(
            "content_block_stop",
            json!({"type": "content_block_stop", "index": 0}),
        ),
    ];
    let stop = match plan {
        Plan::Calls(calls) => {
            for (k, (name, input)) in calls.into_iter().enumerate() {
                let index = k + 1;
                out.push(event("content_block_start", json!({"type": "content_block_start", "index": index, "content_block": {"type": "tool_use", "id": format!("toolu_{index}"), "name": name, "input": {}}})));
                out.push(event("content_block_delta", json!({"type": "content_block_delta", "index": index, "delta": {"type": "input_json_delta", "partial_json": input.to_string()}})));
                out.push(event(
                    "content_block_stop",
                    json!({"type": "content_block_stop", "index": index}),
                ));
            }
            "tool_use"
        }
        Plan::Call { name, input } => {
            out.push(event("content_block_start", json!({"type": "content_block_start", "index": 1, "content_block": {"type": "tool_use", "id": "toolu_1", "name": name, "input": {}}})));
            let args = input.to_string();
            let (a, b) = args.split_at(args.len() / 2);
            for piece in [a, b] {
                out.push(event("content_block_delta", json!({"type": "content_block_delta", "index": 1, "delta": {"type": "input_json_delta", "partial_json": piece}})));
            }
            out.push(event(
                "content_block_stop",
                json!({"type": "content_block_stop", "index": 1}),
            ));
            "tool_use"
        }
        Plan::Answer(text) => {
            out.push(event("content_block_start", json!({"type": "content_block_start", "index": 1, "content_block": {"type": "text", "text": ""}})));
            for word in words(&text) {
                out.push(event("content_block_delta", json!({"type": "content_block_delta", "index": 1, "delta": {"type": "text_delta", "text": word}})));
            }
            out.push(event(
                "content_block_stop",
                json!({"type": "content_block_stop", "index": 1}),
            ));
            "end_turn"
        }
    };
    out.push(event("message_delta", json!({"type": "message_delta", "delta": {"stop_reason": stop}, "usage": {"output_tokens": 42}})));
    out.push(event("message_stop", json!({"type": "message_stop"})));
    out
}
