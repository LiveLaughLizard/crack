//! OpenAI chat completions, and every server that copies the API.

use super::sse::SseReader;
use super::{
    ChatRequest, Completion, Effort, EventSink, Message, Part, Role, StopReason, StreamEvent, Usage,
};
use anyhow::{Context as _, anyhow};
use serde_json::{Value, json};
use std::collections::BTreeMap;

#[derive(Clone, Debug)]
pub enum Auth {
    /// `Authorization: Bearer <key>`.
    Bearer(String),
    /// A named header, such as Azure's `api-key`.
    Header(String, String),
    #[cfg(feature = "cloud")]
    /// A Google access token (Gemini on Vertex AI).
    Google,
    None,
}

/// How the provider takes reasoning effort.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ReasoningParam {
    /// `reasoning_effort: "low" | "medium" | "high"` (OpenAI, Azure, Gemini, Groq, xAI, vLLM).
    Effort,
    /// `reasoning: {effort: ...}` (OpenRouter).
    Nested,
    /// `thinking: {type: "enabled" | "disabled"}` (Zhipu GLM, Volcengine Doubao, Kimi).
    Thinking,
    /// `enable_thinking: true | false` (Qwen on DashScope, SiliconFlow).
    EnableThinking,
    /// `reasoning_effort` limited to the levels the API accepts; a requested level
    /// snaps up to the nearest one (Kimi: low, high, max). With `off`, the lowest
    /// setting sends `thinking: {type: "disabled"}` instead (DeepSeek).
    Levels { levels: Vec<String>, off: bool },
    /// The server takes no reasoning parameter.
    None,
}

#[derive(Clone, Debug)]
pub struct OpenAiEndpoint {
    pub base_url: String,
    pub auth: Auth,
    pub headers: Vec<(String, String)>,
    pub query: Vec<(String, String)>,
    pub reasoning: ReasoningParam,
    /// Send earlier reasoning back as `reasoning_content` on assistant messages. Some
    /// APIs (DeepSeek, Kimi) require it in thinking mode with tool calls; others reject it.
    pub replay_reasoning: bool,
    /// Extra top-level fields for every request (e.g. MiniMax `reasoning_split`).
    pub extra_body: Vec<(String, Value)>,
    /// Send `max_tokens`, capped at this (some APIs default to very short replies).
    pub max_output_tokens: Option<u32>,
}

impl OpenAiEndpoint {
    fn url(&self, path: &str) -> String {
        let mut url = format!("{}/{path}", self.base_url.trim_end_matches('/'));
        if !self.query.is_empty() {
            let query = self
                .query
                .iter()
                .map(|(k, v)| format!("{k}={v}"))
                .collect::<Vec<_>>()
                .join("&");
            url = format!("{url}?{query}");
        }
        url
    }

    async fn authorize(
        &self,
        mut builder: reqwest::RequestBuilder,
    ) -> anyhow::Result<reqwest::RequestBuilder> {
        for (name, value) in &self.headers {
            builder = builder.header(name, value);
        }
        Ok(match &self.auth {
            Auth::Bearer(key) => builder.bearer_auth(key),
            Auth::Header(name, key) => builder.header(name, key),
            #[cfg(feature = "cloud")]
            Auth::Google => builder.bearer_auth(super::anthropic::google_access_token().await?),
            Auth::None => builder,
        })
    }

    /// The models the server lists at `GET /models`.
    pub async fn list_models(&self, client: &reqwest::Client) -> anyhow::Result<Vec<String>> {
        let request = self.authorize(client.get(self.url("models"))).await?;
        let response = request
            .timeout(std::time::Duration::from_secs(8))
            .send()
            .await?;
        if !response.status().is_success() {
            return Err(super::http_error(response).await);
        }
        let json: Value = response.json().await?;
        let list = json["data"]
            .as_array()
            .or_else(|| json["models"].as_array())
            .context("no model list in the response")?;
        let mut models: Vec<String> = list
            .iter()
            .filter_map(|m| m["id"].as_str().or_else(|| m["name"].as_str()))
            .map(|id| id.trim_start_matches("models/").to_owned())
            .collect();
        models.sort();
        models.dedup();
        Ok(models)
    }

    fn body(&self, request: &ChatRequest<'_>) -> Value {
        let mut messages = Vec::new();
        if !request.system.is_empty() {
            messages.push(json!({"role": "system", "content": request.system}));
        }
        for message in request.messages {
            messages.extend(message_json(message, self.replay_reasoning));
        }
        let mut body = json!({
            "model": request.model,
            "messages": messages,
            "stream": true,
            "stream_options": {"include_usage": true},
        });
        if !request.tools.is_empty() {
            body["tools"] = Value::Array(
                request
                    .tools
                    .iter()
                    .map(|tool| {
                        json!({
                            "type": "function",
                            "function": {
                                "name": tool.name,
                                "description": tool.description,
                                "parameters": tool.schema,
                            }
                        })
                    })
                    .collect(),
            );
        }
        let effort = match request.effort {
            Effort::Default => None,
            Effort::Minimal => Some("minimal"),
            Effort::Low => Some("low"),
            Effort::Medium => Some("medium"),
            Effort::High | Effort::XHigh | Effort::Max => Some("high"),
        };
        if let Some(effort) = effort {
            match &self.reasoning {
                ReasoningParam::Effort => body["reasoning_effort"] = json!(effort),
                ReasoningParam::Nested => body["reasoning"] = json!({"effort": effort}),
                // On/off switches: the lowest setting turns thinking off, anything else on.
                ReasoningParam::Thinking => {
                    let kind = if effort == "minimal" {
                        "disabled"
                    } else {
                        "enabled"
                    };
                    body["thinking"] = json!({"type": kind});
                }
                ReasoningParam::EnableThinking => {
                    body["enable_thinking"] = json!(effort != "minimal")
                }
                ReasoningParam::Levels { levels, off } => {
                    if *off && effort == "minimal" {
                        body["thinking"] = json!({"type": "disabled"});
                    } else if let Some(level) = snap_level(request.effort, levels) {
                        body["reasoning_effort"] = json!(level);
                    }
                }
                ReasoningParam::None => {}
            }
        }
        if let Some(cap) = self.max_output_tokens {
            body["max_tokens"] = json!(request.max_tokens.min(cap));
        }
        for (key, value) in &self.extra_body {
            body[key] = value.clone();
        }
        body
    }

    pub async fn complete(
        &self,
        client: &reqwest::Client,
        request: ChatRequest<'_>,
        sink: EventSink<'_>,
    ) -> anyhow::Result<Completion> {
        let url = self.url("chat/completions");
        let body = serde_json::to_vec(&self.body(&request))?;
        let response = super::send_with_retry(|| async {
            let builder = client
                .post(&url)
                .header("content-type", "application/json")
                .body(body.clone());
            self.authorize(builder).await
        })
        .await?;
        read_stream(response, sink).await
    }
}

fn message_json(message: &Message, replay_reasoning: bool) -> Vec<Value> {
    match message.role {
        Role::User => {
            let mut out = Vec::new();
            let mut text = String::new();
            for part in &message.parts {
                match part {
                    Part::ToolResult { id, content, .. } => out.push(json!({
                        "role": "tool",
                        "tool_call_id": id,
                        "content": if content.is_empty() { "(no output)" } else { content },
                    })),
                    Part::Text { text: t } => {
                        if !text.is_empty() {
                            text.push_str("\n\n");
                        }
                        text.push_str(t);
                    }
                    _ => {}
                }
            }
            if !text.is_empty() {
                out.push(json!({"role": "user", "content": text}));
            }
            out
        }
        Role::Assistant => {
            let text = message.text();
            let calls: Vec<Value> = message
                .parts
                .iter()
                .filter_map(|part| match part {
                    Part::ToolUse { id, name, input } => Some(json!({
                        "id": id,
                        "type": "function",
                        "function": {"name": name, "arguments": input.to_string()},
                    })),
                    _ => None,
                })
                .collect();
            let mut value = json!({"role": "assistant", "content": text});
            if replay_reasoning {
                let reasoning: String = message
                    .parts
                    .iter()
                    .filter_map(|part| match part {
                        Part::Thinking {
                            text,
                            signature: None,
                            redacted: None,
                        } => Some(text.as_str()),
                        _ => None,
                    })
                    .collect();
                if !reasoning.is_empty() {
                    value["reasoning_content"] = json!(reasoning);
                }
            }
            if !calls.is_empty() {
                value["tool_calls"] = Value::Array(calls);
                if text.is_empty() {
                    value["content"] = Value::Null;
                }
            }
            vec![value]
        }
    }
}

#[derive(Default)]
struct PendingCall {
    id: String,
    name: String,
    arguments: String,
    announced: bool,
}

async fn read_stream(
    response: reqwest::Response,
    sink: EventSink<'_>,
) -> anyhow::Result<Completion> {
    let mut reader = SseReader::new(response);
    let mut text = String::new();
    let mut thinking = String::new();
    let mut calls: BTreeMap<u64, PendingCall> = BTreeMap::new();
    let mut usage = Usage::default();
    let mut finish: Option<String> = None;

    while let Some(event) = reader.next().await? {
        let data = event.data.trim();
        if data.is_empty() || data == "[DONE]" {
            continue;
        }
        let json: Value =
            serde_json::from_str(data).with_context(|| format!("bad stream event: {data}"))?;
        if let Some(error) = json.get("error") {
            let message = error["message"].as_str().unwrap_or("stream error");
            return Err(anyhow!("{message}"));
        }
        if let Some(u) = json.get("usage").filter(|u| u.is_object()) {
            let cached = u["prompt_tokens_details"]["cached_tokens"]
                .as_u64()
                .unwrap_or(0);
            usage.input_tokens = u["prompt_tokens"]
                .as_u64()
                .unwrap_or(0)
                .saturating_sub(cached);
            usage.cache_read_tokens = cached;
            usage.output_tokens = u["completion_tokens"].as_u64().unwrap_or(0);
        }
        let Some(choice) = json["choices"].get(0) else {
            continue;
        };
        let delta = &choice["delta"];
        if let Some(piece) = delta["content"].as_str().filter(|s| !s.is_empty()) {
            text.push_str(piece);
            sink(StreamEvent::Text(piece.to_owned()));
        }
        // DeepSeek, vLLM, Ollama: `reasoning_content`; OpenRouter, LM Studio: `reasoning`.
        if let Some(piece) = delta["reasoning_content"]
            .as_str()
            .or_else(|| delta["reasoning"].as_str())
            .filter(|s| !s.is_empty())
        {
            thinking.push_str(piece);
            sink(StreamEvent::Thinking(piece.to_owned()));
        }
        if let Some(tool_calls) = delta["tool_calls"].as_array() {
            for (position, call) in tool_calls.iter().enumerate() {
                let index = call["index"].as_u64().unwrap_or(position as u64);
                let pending = calls.entry(index).or_default();
                if let Some(id) = call["id"].as_str().filter(|s| !s.is_empty()) {
                    pending.id = id.to_owned();
                }
                if let Some(name) = call["function"]["name"].as_str() {
                    pending.name.push_str(name);
                }
                if let Some(arguments) = call["function"]["arguments"].as_str() {
                    pending.arguments.push_str(arguments);
                } else if call["function"]["arguments"].is_object() {
                    // Some servers send the arguments as an object.
                    pending.arguments = call["function"]["arguments"].to_string();
                }
                if !pending.announced && !pending.name.is_empty() {
                    pending.announced = true;
                    sink(StreamEvent::ToolStart {
                        id: pending.id.clone(),
                        name: pending.name.clone(),
                    });
                }
            }
        }
        if let Some(reason) = choice["finish_reason"].as_str() {
            finish = Some(reason.to_owned());
        }
    }

    let mut parts = Vec::new();
    if !thinking.is_empty() {
        parts.push(Part::Thinking {
            text: thinking,
            signature: None,
            redacted: None,
        });
    }
    if !text.is_empty() {
        parts.push(Part::Text { text });
    }
    let has_calls = !calls.is_empty();
    for (index, call) in calls {
        let id = if call.id.is_empty() {
            format!("call_{index}_{}", uuid::Uuid::new_v4().simple())
        } else {
            call.id
        };
        let input = if call.arguments.trim().is_empty() {
            json!({})
        } else {
            serde_json::from_str(&call.arguments)
                .unwrap_or_else(|_| json!({ "__invalid_json": call.arguments }))
        };
        parts.push(Part::ToolUse {
            id,
            name: call.name,
            input,
        });
    }

    let stop = match finish.as_deref() {
        _ if has_calls => StopReason::ToolUse,
        Some("length") => StopReason::MaxTokens,
        Some("content_filter") => StopReason::Refusal(None),
        None | Some("stop") => StopReason::EndTurn,
        Some(other) => StopReason::Other(other.to_owned()),
    };
    Ok(Completion {
        message: Message {
            role: Role::Assistant,
            parts,
        },
        stop,
        usage,
    })
}

/// The lowest allowed level at or above the requested effort (or the highest
/// allowed one). `None` for the provider default.
fn snap_level(effort: Effort, levels: &[String]) -> Option<String> {
    if effort == Effort::Default || levels.is_empty() {
        return None;
    }
    let rank = |name: &str| Effort::parse(name).map(|e| e as usize);
    let wanted = effort as usize;
    levels
        .iter()
        .filter(|l| rank(l).is_some_and(|r| r >= wanted))
        .min_by_key(|l| rank(l))
        .or_else(|| levels.iter().max_by_key(|l| rank(l)))
        .cloned()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn endpoint(reasoning: ReasoningParam, replay: bool) -> OpenAiEndpoint {
        OpenAiEndpoint {
            base_url: "http://x/v1".into(),
            auth: Auth::None,
            headers: vec![],
            query: vec![],
            reasoning,
            replay_reasoning: replay,
            extra_body: vec![],
            max_output_tokens: None,
        }
    }

    #[test]
    fn effort_snaps_to_provider_levels() {
        let kimi: Vec<String> = ["low", "high", "max"].map(String::from).into();
        assert_eq!(snap_level(Effort::Minimal, &kimi).as_deref(), Some("low"));
        assert_eq!(snap_level(Effort::Medium, &kimi).as_deref(), Some("high"));
        assert_eq!(snap_level(Effort::XHigh, &kimi).as_deref(), Some("max"));
        assert_eq!(snap_level(Effort::Default, &kimi), None);

        let history = vec![Message::user_text("hi")];
        let request = |effort| ChatRequest {
            model: "m",
            system: "",
            messages: &history,
            tools: &[],
            effort,
            max_tokens: 64_000,
        };
        // DeepSeek: minimal switches thinking off, anything else sets the effort.
        let mut deepseek = endpoint(
            ReasoningParam::Levels {
                levels: kimi.clone(),
                off: true,
            },
            true,
        );
        let body = deepseek.body(&request(Effort::Minimal));
        assert_eq!(body["thinking"]["type"], "disabled");
        assert!(body.get("reasoning_effort").is_none());
        assert_eq!(
            deepseek.body(&request(Effort::High))["reasoning_effort"],
            "high"
        );
        // Output cap and extra fields (Kimi max_tokens, MiniMax reasoning_split).
        deepseek.max_output_tokens = Some(32_768);
        deepseek.extra_body = vec![("reasoning_split".into(), json!(true))];
        let body = deepseek.body(&request(Effort::Default));
        assert_eq!(body["max_tokens"], 32_768);
        assert_eq!(body["reasoning_split"], true);
    }

    #[test]
    fn thinking_switches_and_reasoning_replay() {
        let history = vec![
            Message::user_text("hi"),
            Message {
                role: Role::Assistant,
                parts: vec![
                    Part::Thinking {
                        text: "plan".into(),
                        signature: None,
                        redacted: None,
                    },
                    Part::ToolUse {
                        id: "c1".into(),
                        name: "read".into(),
                        input: json!({}),
                    },
                ],
            },
        ];
        let request = |effort| ChatRequest {
            model: "m",
            system: "",
            messages: &history,
            tools: &[],
            effort,
            max_tokens: 100,
        };
        let body = endpoint(ReasoningParam::Thinking, false).body(&request(Effort::High));
        assert_eq!(body["thinking"]["type"], "enabled");
        assert!(body["messages"][1].get("reasoning_content").is_none());
        let body = endpoint(ReasoningParam::Thinking, false).body(&request(Effort::Minimal));
        assert_eq!(body["thinking"]["type"], "disabled");
        let body = endpoint(ReasoningParam::EnableThinking, true).body(&request(Effort::Low));
        assert_eq!(body["enable_thinking"], true);
        assert_eq!(body["messages"][1]["reasoning_content"], "plan");
        let body = endpoint(ReasoningParam::EnableThinking, true).body(&request(Effort::Default));
        assert!(body.get("enable_thinking").is_none());
    }

    #[test]
    fn tool_calls_and_results_translate() {
        let assistant = Message {
            role: Role::Assistant,
            parts: vec![
                Part::Thinking {
                    text: "x".into(),
                    signature: Some("s".into()),
                    redacted: None,
                },
                Part::ToolUse {
                    id: "c1".into(),
                    name: "bash".into(),
                    input: json!({"command": "ls"}),
                },
            ],
        };
        let out = message_json(&assistant, false);
        assert_eq!(out.len(), 1);
        assert!(out[0]["content"].is_null());
        assert_eq!(
            out[0]["tool_calls"][0]["function"]["arguments"],
            "{\"command\":\"ls\"}"
        );

        let user = Message {
            role: Role::User,
            parts: vec![
                Part::ToolResult {
                    id: "c1".into(),
                    content: "a\nb".into(),
                    is_error: false,
                },
                Part::Text {
                    text: "and then".into(),
                },
            ],
        };
        let out = message_json(&user, false);
        assert_eq!(out[0]["role"], "tool");
        assert_eq!(out[0]["tool_call_id"], "c1");
        assert_eq!(out[1]["role"], "user");
    }
}
