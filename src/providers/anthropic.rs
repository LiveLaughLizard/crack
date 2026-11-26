//! The Anthropic Messages API: first-party, Claude in Amazon Bedrock (Mantle),
//! Google Vertex AI and Microsoft Foundry.

use super::sse::SseReader;
use super::{
    ChatRequest, Completion, Effort, EventSink, Message, Part, Role, StopReason, StreamEvent, Usage,
};
use anyhow::{Context as _, anyhow};
use serde_json::{Value, json};
#[cfg(feature = "cloud")]
use std::sync::Mutex;
use std::time::Duration;
#[cfg(feature = "cloud")]
use std::time::Instant;

#[derive(Clone, Debug)]
pub enum Platform {
    /// api.anthropic.com, or any server with the same API (Foundry, proxies).
    Direct { base_url: String },
    #[cfg(feature = "cloud")]
    /// Claude in Amazon Bedrock, through the Messages-API (Mantle) endpoint.
    Bedrock { region: String },
    #[cfg(feature = "cloud")]
    /// Claude on Google Cloud Vertex AI.
    Vertex { project: String, region: String },
}

#[derive(Clone, Debug)]
pub enum Auth {
    /// `x-api-key`.
    ApiKey(String),
    /// `Authorization: Bearer` (OAuth tokens, Bedrock API keys).
    Bearer(String),
    #[cfg(feature = "cloud")]
    /// AWS credentials, signed per request.
    SigV4 {
        credentials: AwsCredentials,
        service: String,
    },
    #[cfg(feature = "cloud")]
    /// A Google access token from `GOOGLE_OAUTH_ACCESS_TOKEN` or `gcloud`.
    Google,
    None,
}

#[cfg(feature = "cloud")]
/// Where AWS keys come from.
#[derive(Clone, Debug)]
pub enum AwsCredentials {
    Static {
        access_key: String,
        secret_key: String,
        session_token: Option<String>,
    },
    /// A named profile in ~/.aws (static keys, SSO, assumed roles...). Resolved with
    /// `aws configure export-credentials`, or read from ~/.aws/credentials.
    Profile(String),
}

#[cfg(feature = "cloud")]
impl AwsCredentials {
    pub async fn resolve(&self) -> anyhow::Result<(String, String, Option<String>)> {
        match self {
            AwsCredentials::Static {
                access_key,
                secret_key,
                session_token,
            } => Ok((
                access_key.clone(),
                secret_key.clone(),
                session_token.clone(),
            )),
            AwsCredentials::Profile(profile) => aws_profile_credentials(profile).await,
        }
    }
}

#[cfg(feature = "cloud")]
type AwsKeys = (String, String, Option<String>);

#[cfg(feature = "cloud")]
async fn aws_profile_credentials(profile: &str) -> anyhow::Result<AwsKeys> {
    static CACHE: Mutex<Vec<(String, AwsKeys, Instant)>> = Mutex::new(Vec::new());
    if let Some((_, keys, at)) = CACHE.lock().unwrap().iter().find(|(p, ..)| p == profile)
        && at.elapsed() < Duration::from_secs(10 * 60)
    {
        return Ok(keys.clone());
    }
    let keys = match tokio::process::Command::new("aws")
        .args(["configure", "export-credentials", "--profile", profile, "--format", "process"])
        .output()
        .await
    {
        Ok(output) if output.status.success() => {
            let json: Value = serde_json::from_slice(&output.stdout)?;
            (
                json["AccessKeyId"].as_str().unwrap_or_default().to_owned(),
                json["SecretAccessKey"].as_str().unwrap_or_default().to_owned(),
                json["SessionToken"].as_str().map(str::to_owned),
            )
        }
        Ok(output) => {
            let message = String::from_utf8_lossy(&output.stderr).trim().to_owned();
            if message.contains("expired") || message.contains("sso") || message.contains("SSO") {
                return Err(anyhow!(
                    "AWS profile '{profile}': {message}. Use \"Sign in\" under Providers (aws sso login)."
                ));
            }
            read_aws_credentials_file(profile).ok_or_else(|| anyhow!("AWS profile '{profile}': {message}"))?
        }
        // No AWS CLI: static keys in ~/.aws/credentials still work.
        Err(_) => read_aws_credentials_file(profile).ok_or_else(|| {
            anyhow!("AWS profile '{profile}' has no keys in ~/.aws/credentials, and the AWS CLI isn't installed")
        })?,
    };
    let mut cache = CACHE.lock().unwrap();
    cache.retain(|(p, ..)| p != profile);
    cache.push((profile.to_owned(), keys.clone(), Instant::now()));
    Ok(keys)
}

#[cfg(feature = "cloud")]
fn read_aws_credentials_file(profile: &str) -> Option<AwsKeys> {
    let path = std::env::var("AWS_SHARED_CREDENTIALS_FILE")
        .map(std::path::PathBuf::from)
        .ok()
        .or_else(|| dirs::home_dir().map(|h| h.join(".aws/credentials")))?;
    let text = std::fs::read_to_string(path).ok()?;
    let mut in_section = false;
    let (mut access, mut secret, mut token) = (None, None, None);
    for line in text.lines().map(str::trim) {
        if line.starts_with('[') {
            in_section = line.trim_matches(['[', ']']).trim() == profile;
            continue;
        }
        if !in_section {
            continue;
        }
        if let Some((key, value)) = line.split_once('=') {
            let value = Some(value.trim().to_owned());
            match key.trim() {
                "aws_access_key_id" => access = value,
                "aws_secret_access_key" => secret = value,
                "aws_session_token" => token = value,
                _ => {}
            }
        }
    }
    Some((access?, secret?, token))
}

#[derive(Clone, Debug)]
pub struct AnthropicEndpoint {
    pub platform: Platform,
    pub auth: Auth,
    pub headers: Vec<(String, String)>,
}

/// How a model takes thinking configuration.
enum ThinkingStyle {
    /// Claude 4.6 and later: `thinking: {type: "adaptive"}` + `output_config.effort`.
    Adaptive,
    /// Older models: a fixed `budget_tokens`, no effort parameter.
    Budget,
}

fn thinking_style(model: &str) -> ThinkingStyle {
    const LEGACY: [&str; 8] = [
        "claude-3",
        "-4-5",
        "-4-1",
        "-4-0",
        "opus-4-2",
        "sonnet-4-2",
        "opus-4@",
        "sonnet-4@",
    ];
    if LEGACY.iter().any(|marker| model.contains(marker)) {
        ThinkingStyle::Budget
    } else {
        ThinkingStyle::Adaptive
    }
}

/// Models that refuse some requests through safety classifiers and accept the
/// server-side fallback parameter on the first-party API.
fn supports_fallbacks(model: &str) -> bool {
    [
        "claude-opus-5-5",
        "claude-opus-5",
        "claude-fable-5-1",
        "claude-sonnet-5-5",
    ]
    .contains(&model)
}

impl AnthropicEndpoint {
    /// api.anthropic.com itself. Beta fields (server-side fallbacks, eager input
    /// streaming) are only sent there: proxies and cloud platforms may reject them.
    pub(crate) fn first_party(&self) -> bool {
        #[cfg(feature = "cloud")]
        return matches!(&self.platform, Platform::Direct { base_url } if base_url.contains("api.anthropic.com"));
        // Air-gapped builds only talk to self-hosted servers.
        #[cfg(not(feature = "cloud"))]
        false
    }

    /// Vertex takes the API version in the body instead of a header.
    fn sends_version_header(&self) -> bool {
        #[cfg(feature = "cloud")]
        return !matches!(self.platform, Platform::Vertex { .. });
        #[cfg(not(feature = "cloud"))]
        true
    }

    #[cfg_attr(not(feature = "cloud"), allow(unused_variables))]
    fn url(&self, model: &str) -> String {
        match &self.platform {
            Platform::Direct { base_url } => {
                let base = base_url.trim_end_matches('/');
                if base.ends_with("/v1") {
                    format!("{base}/messages")
                } else {
                    format!("{base}/v1/messages")
                }
            }
            #[cfg(feature = "cloud")]
            Platform::Bedrock { region } => {
                format!("https://bedrock-mantle.{region}.api.aws/anthropic/v1/messages")
            }
            #[cfg(feature = "cloud")]
            Platform::Vertex { project, region } => {
                let host = if region == "global" {
                    "aiplatform.googleapis.com".to_owned()
                } else {
                    format!("{region}-aiplatform.googleapis.com")
                };
                format!(
                    "https://{host}/v1/projects/{project}/locations/{region}/publishers/anthropic/models/{model}:streamRawPredict"
                )
            }
        }
    }

    fn body(&self, request: &ChatRequest<'_>) -> Value {
        let direct = self.first_party();
        let mut max_tokens = request.max_tokens;
        let mut body = json!({
            "max_tokens": max_tokens,
            "stream": true,
            "messages": messages_json(request.messages),
        });
        if !request.system.is_empty() {
            body["system"] = json!([{
                "type": "text",
                "text": request.system,
                "cache_control": {"type": "ephemeral"},
            }]);
        }
        if !request.tools.is_empty() || (direct && request.web_search) {
            let mut tools: Vec<Value> = request
                .tools
                .iter()
                .map(|tool| {
                    let mut value = json!({
                        "name": tool.name,
                        "description": tool.description,
                        "input_schema": tool.schema,
                    });
                    // Stream large tool inputs (file contents) as they are written.
                    // Proxies and older cloud deployments reject the field.
                    if direct {
                        value["eager_input_streaming"] = json!(true);
                    }
                    value
                })
                .collect();
            // Claude's own web search, run on Anthropic's servers.
            if direct && request.web_search {
                tools.push(
                    json!({"type": "web_search_20260209", "name": "web_search", "max_uses": 8}),
                );
            }
            if let Some(last) = tools.last_mut() {
                last["cache_control"] = json!({"type": "ephemeral"});
            }
            body["tools"] = Value::Array(tools);
        }

        match thinking_style(request.model) {
            ThinkingStyle::Adaptive => {
                body["thinking"] = json!({"type": "adaptive", "display": "summarized"});
                let effort = match request.effort {
                    Effort::Default => None,
                    Effort::Minimal => Some("low"),
                    other => Some(other.as_str()),
                };
                if let Some(effort) = effort {
                    body["output_config"] = json!({"effort": effort});
                }
            }
            ThinkingStyle::Budget => {
                let budget = match request.effort {
                    Effort::Default | Effort::Minimal => None,
                    Effort::Low => Some(4_096),
                    Effort::Medium => Some(10_000),
                    Effort::High => Some(24_000),
                    Effort::XHigh | Effort::Max => Some(32_000),
                };
                if let Some(budget) = budget {
                    max_tokens = max_tokens.max(budget + 8_192);
                    body["max_tokens"] = json!(max_tokens);
                    body["thinking"] = json!({"type": "enabled", "budget_tokens": budget});
                }
            }
        }

        match &self.platform {
            #[cfg(feature = "cloud")]
            Platform::Vertex { .. } => {
                body["anthropic_version"] = json!("vertex-2023-10-16");
            }
            _ => {
                body["model"] = json!(request.model);
            }
        }
        if direct && request.compaction {
            // On-demand compaction: the reply is one `compaction` block summarizing the
            // conversation. It can't be combined with `context_management`.
            body["compaction"] = json!({"type": "summarize"});
        } else if direct && let Some(at) = request.clear_tool_results_at {
            // Context editing: Claude drops the oldest tool results once the prompt is
            // this big. Clearing invalidates the prompt cache from that point, so clear a
            // worthwhile amount each time.
            body["context_management"] = json!({"edits": [{
                "type": "clear_tool_uses_20250919",
                "trigger": {"type": "input_tokens", "value": at},
                "keep": {"type": "tool_uses", "value": 6},
                "clear_at_least": {"type": "input_tokens", "value": (at / 5).max(10_000)},
            }]});
        }
        if direct && supports_fallbacks(request.model) {
            // Requests a safety classifier declines are retried on a fallback model.
            body["fallbacks"] = json!("default");
        }
        body
    }

    pub async fn complete(
        &self,
        client: &reqwest::Client,
        request: ChatRequest<'_>,
        sink: EventSink<'_>,
    ) -> anyhow::Result<Completion> {
        let url = self.url(request.model);
        let body = serde_json::to_vec(&self.body(&request))?;
        super::dump_request(&body);
        let mut betas: Vec<&str> = Vec::new();
        if self.first_party() {
            if supports_fallbacks(request.model) {
                betas.push("server-side-fallback-2026-07-01");
            }
            // Every request that carries a compaction block needs the beta too.
            if request.compaction
                || request
                    .messages
                    .iter()
                    .any(|m| m.parts.iter().any(|p| matches!(p, Part::Compaction { .. })))
            {
                betas.push("compact-2026-09-04");
            } else if request.clear_tool_results_at.is_some() {
                betas.push("context-management-2025-06-27");
            }
        }
        let betas = betas.join(",");

        let response = super::send_with_retry(|| async {
            let mut builder = client
                .post(&url)
                .header("content-type", "application/json")
                .header("accept", "text/event-stream");
            if self.sends_version_header() {
                builder = builder.header("anthropic-version", "2023-06-01");
            }
            if !betas.is_empty() {
                builder = builder.header("anthropic-beta", &betas);
            }
            for (name, value) in &self.headers {
                builder = builder.header(name, value);
            }
            builder = match &self.auth {
                Auth::ApiKey(key) => builder.header("x-api-key", key),
                Auth::Bearer(token) => builder.bearer_auth(token),
                #[cfg(feature = "cloud")]
                Auth::Google => builder.bearer_auth(google_access_token().await?),
                #[cfg(feature = "cloud")]
                Auth::SigV4 {
                    credentials,
                    service,
                } => {
                    let Platform::Bedrock { region } = &self.platform else {
                        return Err(anyhow!("SigV4 is only used with Bedrock"));
                    };
                    let (access_key, secret_key, session_token) = credentials.resolve().await?;
                    let headers = super::sigv4::sign(&super::sigv4::SignInput {
                        method: "POST",
                        url: &url,
                        body: &body,
                        region,
                        service,
                        access_key: &access_key,
                        secret_key: &secret_key,
                        session_token: session_token.as_deref(),
                    })?;
                    headers.into_iter().fold(builder, |builder, (name, value)| {
                        builder.header(name, value)
                    })
                }
                Auth::None => builder,
            };
            Ok(builder.body(body.clone()))
        })
        .await?;

        read_stream(response, sink).await
    }

    /// The models a first-party-style endpoint lists at `GET /v1/models`. Bedrock and
    /// Vertex don't serve that list; they return `None`.
    pub async fn list_models(
        &self,
        client: &reqwest::Client,
    ) -> anyhow::Result<Option<Vec<String>>> {
        #[allow(irrefutable_let_patterns)]
        let Platform::Direct { base_url } = &self.platform else {
            return Ok(None);
        };
        let base = base_url.trim_end_matches('/');
        let url = if base.ends_with("/v1") {
            format!("{base}/models?limit=100")
        } else {
            format!("{base}/v1/models?limit=100")
        };
        let mut builder = client
            .get(url)
            .header("anthropic-version", "2023-06-01")
            .timeout(Duration::from_secs(15));
        for (name, value) in &self.headers {
            builder = builder.header(name, value);
        }
        builder = match &self.auth {
            Auth::ApiKey(key) => builder.header("x-api-key", key),
            Auth::Bearer(token) => builder.bearer_auth(token),
            _ => builder,
        };
        let response = builder.send().await?;
        if response.status() == reqwest::StatusCode::NOT_FOUND {
            return Ok(None);
        }
        if !response.status().is_success() {
            return Err(super::http_error(response).await);
        }
        let json: Value = response.json().await?;
        Ok(Some(
            json["data"]
                .as_array()
                .map(|list| {
                    list.iter()
                        .filter_map(|m| m["id"].as_str().map(str::to_owned))
                        .collect()
                })
                .unwrap_or_default(),
        ))
    }
}

/// Translates history into Messages API JSON, merging consecutive turns of one role.
fn messages_json(messages: &[Message]) -> Vec<Value> {
    let mut out: Vec<Value> = Vec::new();
    for message in messages {
        let role = match message.role {
            Role::User => "user",
            Role::Assistant => "assistant",
        };
        let blocks: Vec<Value> = message.parts.iter().filter_map(part_json).collect();
        if blocks.is_empty() {
            continue;
        }
        match out.last_mut() {
            Some(last) if last["role"] == role => {
                last["content"]
                    .as_array_mut()
                    .expect("content is an array")
                    .extend(blocks);
            }
            _ => out.push(json!({"role": role, "content": blocks})),
        }
    }
    // Cache the conversation so far: the next request reads it back.
    if let Some(block) = out
        .last_mut()
        .and_then(|m| m["content"].as_array_mut())
        .and_then(|c| c.last_mut())
    {
        block["cache_control"] = json!({"type": "ephemeral"});
    }
    out
}

fn part_json(part: &Part) -> Option<Value> {
    Some(match part {
        Part::Text { text } if text.is_empty() => return None,
        Part::Text { text } => json!({"type": "text", "text": text}),
        Part::Image { media_type, data } => json!({
            "type": "image",
            "source": {"type": "base64", "media_type": media_type, "data": data},
        }),
        Part::Thinking {
            redacted: Some(data),
            ..
        } => json!({"type": "redacted_thinking", "data": data}),
        // Thinking blocks go back unchanged, signature and all; reasoning from other
        // providers has no signature and is dropped.
        Part::Thinking {
            text,
            signature: Some(signature),
            ..
        } => json!({"type": "thinking", "thinking": text, "signature": signature}),
        Part::Thinking { .. } => return None,
        Part::ToolUse { id, name, input } => {
            json!({"type": "tool_use", "id": id, "name": name, "input": input})
        }
        Part::ToolResult {
            id,
            content,
            is_error,
        } => json!({
            "type": "tool_result",
            "tool_use_id": id,
            "content": if content.is_empty() { "(no output)" } else { content },
            "is_error": is_error,
        }),
        Part::Compaction { content, signature } => json!({
            "type": "compaction",
            "content": content,
            "signature": signature,
        }),
        Part::Server { block } => block.clone(),
    })
}

enum Block {
    Text(String),
    Thinking {
        text: String,
        signature: String,
    },
    Redacted(String),
    Compaction {
        content: String,
        signature: String,
    },
    /// A server tool's block, kept as sent (its input may stream in as JSON).
    Server {
        block: Value,
        json: String,
    },
    Tool {
        id: String,
        name: String,
        json: String,
    },
    Other,
}

async fn read_stream(
    response: reqwest::Response,
    sink: EventSink<'_>,
) -> anyhow::Result<Completion> {
    let mut reader = SseReader::new(response);
    let mut blocks: Vec<Block> = Vec::new();
    let mut usage = Usage::default();
    let mut stop = StopReason::EndTurn;

    while let Some(event) = reader.next().await? {
        if event.data.is_empty() || event.data == "[DONE]" {
            continue;
        }
        let data: Value = serde_json::from_str(&event.data)
            .with_context(|| format!("bad stream event: {}", event.data))?;
        match data["type"].as_str().unwrap_or_default() {
            "message_start" => {
                let u = &data["message"]["usage"];
                usage.input_tokens = u["input_tokens"].as_u64().unwrap_or(0);
                usage.cache_read_tokens = u["cache_read_input_tokens"].as_u64().unwrap_or(0);
                usage.cache_write_tokens = u["cache_creation_input_tokens"].as_u64().unwrap_or(0);
                usage.output_tokens = u["output_tokens"].as_u64().unwrap_or(0);
            }
            "content_block_start" => {
                let index = data["index"].as_u64().unwrap_or(0) as usize;
                let block = &data["content_block"];
                let new = match block["type"].as_str().unwrap_or_default() {
                    "text" => Block::Text(block["text"].as_str().unwrap_or_default().to_owned()),
                    "thinking" => Block::Thinking {
                        text: block["thinking"].as_str().unwrap_or_default().to_owned(),
                        signature: block["signature"].as_str().unwrap_or_default().to_owned(),
                    },
                    "redacted_thinking" => {
                        Block::Redacted(block["data"].as_str().unwrap_or_default().to_owned())
                    }
                    // Arrives whole in `content_block_start` (no deltas).
                    "compaction" => Block::Compaction {
                        content: block["content"].as_str().unwrap_or_default().to_owned(),
                        signature: block["signature"].as_str().unwrap_or_default().to_owned(),
                    },
                    "tool_use" => {
                        let id = block["id"].as_str().unwrap_or_default().to_owned();
                        let name = block["name"].as_str().unwrap_or_default().to_owned();
                        sink(StreamEvent::ToolStart {
                            id: id.clone(),
                            name: name.clone(),
                        });
                        Block::Tool {
                            id,
                            name,
                            json: String::new(),
                        }
                    }
                    // Server tools (web search and the code that filters its results).
                    kind if !kind.is_empty() => Block::Server {
                        block: block.clone(),
                        json: String::new(),
                    },
                    _ => Block::Other,
                };
                while blocks.len() <= index {
                    blocks.push(Block::Other);
                }
                blocks[index] = new;
            }
            "content_block_delta" => {
                let index = data["index"].as_u64().unwrap_or(0) as usize;
                let delta = &data["delta"];
                let Some(block) = blocks.get_mut(index) else {
                    continue;
                };
                match (delta["type"].as_str().unwrap_or_default(), block) {
                    ("text_delta", Block::Text(text)) => {
                        let piece = delta["text"].as_str().unwrap_or_default();
                        text.push_str(piece);
                        sink(StreamEvent::Text(piece.to_owned()));
                    }
                    ("thinking_delta", Block::Thinking { text, .. }) => {
                        let piece = delta["thinking"].as_str().unwrap_or_default();
                        text.push_str(piece);
                        sink(StreamEvent::Thinking(piece.to_owned()));
                    }
                    ("signature_delta", Block::Thinking { signature, .. }) => {
                        signature.push_str(delta["signature"].as_str().unwrap_or_default());
                    }
                    ("input_json_delta", Block::Tool { json, .. }) => {
                        json.push_str(delta["partial_json"].as_str().unwrap_or_default());
                    }
                    ("input_json_delta", Block::Server { json, .. }) => {
                        json.push_str(delta["partial_json"].as_str().unwrap_or_default());
                    }
                    // In case the summary is ever streamed in pieces.
                    ("compaction_delta", Block::Compaction { content, .. }) => {
                        content.push_str(delta["content"].as_str().unwrap_or_default());
                    }
                    _ => {}
                }
            }
            "message_delta" => {
                if let Some(output) = data["usage"]["output_tokens"].as_u64() {
                    usage.output_tokens = output;
                }
                // Compaction reports its tokens per iteration, with zeros at the top level.
                if let Some(iterations) = data["usage"]["iterations"].as_array() {
                    let sum = |key: &str| -> u64 {
                        iterations.iter().filter_map(|i| i[key].as_u64()).sum()
                    };
                    usage.input_tokens = usage.input_tokens.max(sum("input_tokens"));
                    usage.output_tokens = usage.output_tokens.max(sum("output_tokens"));
                }
                if let Some(reason) = data["delta"]["stop_reason"].as_str() {
                    stop = match reason {
                        "end_turn" | "stop_sequence" => StopReason::EndTurn,
                        "tool_use" => StopReason::ToolUse,
                        "max_tokens" => StopReason::MaxTokens,
                        "refusal" => StopReason::Refusal(
                            data["delta"]["stop_details"]["explanation"]
                                .as_str()
                                .map(str::to_owned),
                        ),
                        other => StopReason::Other(other.to_owned()),
                    };
                }
            }
            "error" => {
                let message = data["error"]["message"].as_str().unwrap_or("stream error");
                return Err(anyhow!("{message}"));
            }
            _ => {}
        }
    }

    let parts = blocks
        .into_iter()
        .filter_map(|block| match block {
            Block::Text(text) => Some(Part::Text { text }),
            Block::Thinking { text, signature } => Some(Part::Thinking {
                text,
                signature: (!signature.is_empty()).then_some(signature),
                redacted: None,
            }),
            Block::Redacted(data) => Some(Part::Thinking {
                text: String::new(),
                signature: None,
                redacted: Some(data),
            }),
            Block::Compaction { content, signature } => {
                (!content.is_empty()).then_some(Part::Compaction { content, signature })
            }
            Block::Server { mut block, json } => {
                if !json.trim().is_empty()
                    && let Ok(input) = serde_json::from_str::<Value>(&json)
                {
                    block["input"] = input;
                }
                Some(Part::Server { block })
            }
            Block::Tool { id, name, json } => {
                // With eager input streaming the input is not validated server-side;
                // invalid JSON reaches the tool runner as an error marker.
                let input = if json.trim().is_empty() {
                    json!({})
                } else {
                    serde_json::from_str(&json)
                        .unwrap_or_else(|_| json!({ "__invalid_json": json }))
                };
                Some(Part::ToolUse { id, name, input })
            }
            Block::Other => None,
        })
        .collect();

    Ok(Completion {
        message: Message {
            role: Role::Assistant,
            parts,
        },
        stop,
        usage,
    })
}

#[cfg(feature = "cloud")]
/// A Google access token for Vertex AI, cached for 45 minutes.
pub async fn google_access_token() -> anyhow::Result<String> {
    static CACHE: Mutex<Option<(String, Instant)>> = Mutex::new(None);
    if let Ok(token) = std::env::var("GOOGLE_OAUTH_ACCESS_TOKEN") {
        return Ok(token);
    }
    if let Some((token, at)) = CACHE.lock().unwrap().clone()
        && at.elapsed() < Duration::from_secs(45 * 60)
    {
        return Ok(token);
    }
    let output = tokio::process::Command::new("gcloud")
        .args(["auth", "print-access-token"])
        .output()
        .await
        .context("Vertex AI needs GOOGLE_OAUTH_ACCESS_TOKEN or the gcloud CLI")?;
    if !output.status.success() {
        return Err(anyhow!(
            "gcloud auth print-access-token failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    let token = String::from_utf8_lossy(&output.stdout).trim().to_owned();
    *CACHE.lock().unwrap() = Some((token.clone(), Instant::now()));
    Ok(token)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::providers::ToolSpec;

    #[test]
    fn images_are_base64_blocks() {
        let part = Part::Image {
            media_type: "image/jpeg".into(),
            data: "BBBB".into(),
        };
        assert_eq!(
            part_json(&part).unwrap(),
            json!({"type": "image", "source": {"type": "base64", "media_type": "image/jpeg", "data": "BBBB"}})
        );
    }

    fn request<'a>(
        model: &'a str,
        messages: &'a [Message],
        tools: &'a [ToolSpec],
        effort: Effort,
    ) -> ChatRequest<'a> {
        ChatRequest {
            model,
            system: "sys",
            messages,
            tools,
            effort,
            max_tokens: 1000,
            compaction: false,
            clear_tool_results_at: None,
            web_search: false,
        }
    }

    fn history() -> Vec<Message> {
        vec![
            Message::user_text("hi"),
            Message {
                role: Role::Assistant,
                parts: vec![
                    Part::Thinking {
                        text: "hmm".into(),
                        signature: Some("sig".into()),
                        redacted: None,
                    },
                    Part::Thinking {
                        text: "other provider".into(),
                        signature: None,
                        redacted: None,
                    },
                    Part::ToolUse {
                        id: "t1".into(),
                        name: "read".into(),
                        input: json!({"path": "a"}),
                    },
                ],
            },
            Message {
                role: Role::User,
                parts: vec![Part::ToolResult {
                    id: "t1".into(),
                    content: "ok".into(),
                    is_error: false,
                }],
            },
        ]
    }

    #[cfg(feature = "cloud")]
    #[test]
    fn first_party_body_replays_signed_thinking_and_sets_effort() {
        let endpoint = AnthropicEndpoint {
            platform: Platform::Direct {
                base_url: "https://api.anthropic.com".into(),
            },
            auth: Auth::None,
            headers: vec![],
        };
        let messages = history();
        let tools = [ToolSpec {
            name: "read".into(),
            description: "d".into(),
            schema: json!({"type": "object"}),
        }];
        let body = endpoint.body(&request(
            "claude-opus-5-5",
            &messages,
            &tools,
            Effort::XHigh,
        ));
        assert_eq!(body["model"], "claude-opus-5-5");
        assert_eq!(body["thinking"]["type"], "adaptive");
        assert_eq!(body["output_config"]["effort"], "xhigh");
        assert_eq!(body["fallbacks"], "default");
        assert_eq!(body["tools"][0]["eager_input_streaming"], true);
        let assistant = &body["messages"][1]["content"];
        // The signed block goes back; unsigned reasoning from elsewhere is dropped.
        assert_eq!(assistant.as_array().unwrap().len(), 2);
        assert_eq!(assistant[0]["signature"], "sig");
        assert_eq!(body["messages"][2]["content"][0]["tool_use_id"], "t1");
        assert!(body["messages"][2]["content"][0]["cache_control"].is_object());
    }

    #[cfg(feature = "cloud")]
    #[test]
    fn vertex_and_bedrock_shapes() {
        let messages = history();
        let vertex = AnthropicEndpoint {
            platform: Platform::Vertex {
                project: "p".into(),
                region: "global".into(),
            },
            auth: Auth::None,
            headers: vec![],
        };
        let body = vertex.body(&request(
            "claude-sonnet-5-5",
            &messages,
            &[],
            Effort::Default,
        ));
        assert!(body.get("model").is_none());
        assert_eq!(body["anthropic_version"], "vertex-2023-10-16");
        assert!(body.get("output_config").is_none());
        assert!(body.get("fallbacks").is_none());
        assert_eq!(
            vertex.url("claude-sonnet-5-5"),
            "https://aiplatform.googleapis.com/v1/projects/p/locations/global/publishers/anthropic/models/claude-sonnet-5-5:streamRawPredict"
        );
        let bedrock = AnthropicEndpoint {
            platform: Platform::Bedrock {
                region: "us-west-2".into(),
            },
            auth: Auth::None,
            headers: vec![],
        };
        assert_eq!(
            bedrock.url("x"),
            "https://bedrock-mantle.us-west-2.api.aws/anthropic/v1/messages"
        );
        let body = bedrock.body(&request(
            "anthropic.claude-opus-5-5",
            &messages,
            &[],
            Effort::Low,
        ));
        assert_eq!(body["model"], "anthropic.claude-opus-5-5");
        assert!(body.get("fallbacks").is_none());
    }

    #[test]
    fn legacy_models_use_budget_tokens() {
        let endpoint = AnthropicEndpoint {
            platform: Platform::Direct {
                base_url: "https://api.anthropic.com".into(),
            },
            auth: Auth::None,
            headers: vec![],
        };
        let messages = history();
        let body = endpoint.body(&request("claude-haiku-4-5", &messages, &[], Effort::Medium));
        assert_eq!(body["thinking"]["budget_tokens"], 10_000);
        assert!(body["max_tokens"].as_u64().unwrap() > 10_000);
        assert!(body.get("output_config").is_none());
    }
}

#[cfg(all(test, feature = "cloud"))]
mod compaction_tests {
    use super::*;

    fn endpoint(base_url: &str) -> AnthropicEndpoint {
        AnthropicEndpoint {
            platform: Platform::Direct {
                base_url: base_url.into(),
            },
            auth: Auth::None,
            headers: vec![],
        }
    }

    fn request(messages: &[Message], compaction: bool, clear: Option<u64>) -> ChatRequest<'_> {
        ChatRequest {
            model: "claude-opus-5-5",
            system: "sys",
            messages,
            tools: &[],
            effort: Effort::Default,
            max_tokens: 1000,
            compaction,
            clear_tool_results_at: clear,
            web_search: false,
        }
    }

    #[test]
    fn asks_for_compaction_or_clearing_only_on_anthropics_api() {
        let messages = vec![Message::user_text("hi")];
        let direct = endpoint("https://api.anthropic.com");
        let body = direct.body(&request(&messages, true, Some(100_000)));
        assert_eq!(body["compaction"], json!({"type": "summarize"}));
        // The API refuses compaction and context_management together.
        assert!(body.get("context_management").is_none());

        let body = direct.body(&request(&messages, false, Some(100_000)));
        let edit = &body["context_management"]["edits"][0];
        assert_eq!(edit["type"], "clear_tool_uses_20250919");
        assert_eq!(edit["trigger"]["value"], 100_000);
        assert!(body.get("compaction").is_none());

        // Proxies and other Anthropic-compatible servers don't get either.
        let proxy = endpoint("http://localhost:8080");
        let body = proxy.body(&request(&messages, true, Some(100_000)));
        assert!(body.get("compaction").is_none());
        assert!(body.get("context_management").is_none());
    }

    #[test]
    fn compaction_block_goes_back_first_and_unchanged() {
        let messages = vec![
            Message {
                role: Role::Assistant,
                parts: vec![Part::Compaction {
                    content: "Summary".into(),
                    signature: "EuYB".into(),
                }],
            },
            Message::user_text("go on"),
        ];
        let json = messages_json(&messages);
        assert_eq!(json[0]["role"], "assistant");
        assert_eq!(
            json[0]["content"][0],
            json!({"type": "compaction", "content": "Summary", "signature": "EuYB"})
        );
        assert_eq!(json[1]["role"], "user");
    }

    #[tokio::test]
    async fn web_search_blocks_are_kept_and_replayed_unchanged() {
        let events = [
            json!({"type": "message_start", "message": {"usage": {"input_tokens": 10, "output_tokens": 0}}}),
            json!({"type": "content_block_start", "index": 0, "content_block": {"type": "server_tool_use", "id": "srvtoolu_1", "name": "web_search", "input": {}}}),
            json!({"type": "content_block_delta", "index": 0, "delta": {"type": "input_json_delta", "partial_json": "{\"query\": \"rust 1.100\"}"}}),
            json!({"type": "content_block_stop", "index": 0}),
            json!({"type": "content_block_start", "index": 1, "content_block": {"type": "web_search_tool_result", "tool_use_id": "srvtoolu_1", "content": [{"type": "web_search_result", "url": "https://blog.rust-lang.org", "title": "Rust 1.100", "encrypted_content": "abc"}]}}),
            json!({"type": "content_block_stop", "index": 1}),
            json!({"type": "content_block_start", "index": 2, "content_block": {"type": "text", "text": ""}}),
            json!({"type": "content_block_delta", "index": 2, "delta": {"type": "text_delta", "text": "It shipped."}}),
            json!({"type": "message_delta", "delta": {"stop_reason": "end_turn"}, "usage": {"output_tokens": 5}}),
        ];
        let body: String = events
            .iter()
            .map(|e| format!("event: {}\ndata: {e}\n\n", e["type"].as_str().unwrap()))
            .collect();
        let response = reqwest::Response::from(http::Response::new(body));
        let mut sink = |_| {};
        let completion = read_stream(response, &mut sink).await.unwrap();
        let parts = &completion.message.parts;
        assert!(
            matches!(&parts[0], Part::Server { block } if block["input"]["query"] == "rust 1.100")
        );
        assert!(
            matches!(&parts[1], Part::Server { block } if block["type"] == "web_search_tool_result")
        );
        assert!(matches!(&parts[2], Part::Text { text } if text == "It shipped."));
        // Sent back as they came.
        let json = messages_json(std::slice::from_ref(&completion.message));
        assert_eq!(json[0]["content"][0]["type"], "server_tool_use");
        assert_eq!(
            json[0]["content"][1]["content"][0]["encrypted_content"],
            "abc"
        );
        // The tool goes only to Anthropic's own API, and only when asked for.
        let messages = vec![Message::user_text("hi")];
        let mut with_search = request(&messages, false, None);
        with_search.web_search = true;
        let body = endpoint("https://api.anthropic.com").body(&with_search);
        assert_eq!(body["tools"][0]["type"], "web_search_20260209");
        assert!(
            endpoint("http://localhost:1")
                .body(&with_search)
                .get("tools")
                .is_none()
        );
    }

    #[tokio::test]
    async fn reads_a_compaction_reply() {
        let events = [
            json!({"type": "message_start", "message": {"usage": {"input_tokens": 0, "output_tokens": 0}}}),
            json!({"type": "content_block_start", "index": 0, "content_block": {"type": "compaction", "content": "Summary of the conversation", "signature": "EuYB"}}),
            json!({"type": "content_block_stop", "index": 0}),
            json!({"type": "message_delta", "delta": {"stop_reason": "compaction"}, "usage": {"input_tokens": 0, "output_tokens": 0, "iterations": [{"type": "compaction", "input_tokens": 144, "output_tokens": 276}]}}),
            json!({"type": "message_stop"}),
        ];
        let body: String = events
            .iter()
            .map(|e| format!("event: {}\ndata: {e}\n\n", e["type"].as_str().unwrap()))
            .collect();
        let response = reqwest::Response::from(http::Response::new(body));
        let mut sink = |_| {};
        let completion = read_stream(response, &mut sink).await.unwrap();
        assert!(matches!(
            completion.message.parts.as_slice(),
            [Part::Compaction { content, signature }] if content == "Summary of the conversation" && signature == "EuYB"
        ));
        assert_eq!(completion.usage.input_tokens, 144);
        assert_eq!(completion.usage.output_tokens, 276);
    }
}
