//! Model providers.
//!
//! Every provider speaks one of two wire protocols:
//!
//! * the Anthropic Messages API: Anthropic itself, Claude in Amazon Bedrock (the
//!   Mantle endpoint), Claude on Google Vertex AI and Microsoft Foundry;
//! * OpenAI chat completions: OpenAI, Azure OpenAI, Google Gemini, OpenRouter, Groq,
//!   Mistral, DeepSeek, xAI, Together, and every self-hosted server that copies the
//!   API (Ollama, LM Studio, vLLM, llama.cpp, LiteLLM, ...).
//!
//! The agent works with the provider-neutral types in this module; each protocol
//! translates them to and from its own JSON.

pub mod anthropic;
pub mod catalog;
pub mod models;
pub mod openai;
pub mod registry;
#[cfg(feature = "cloud")]
mod sigv4;
pub(crate) mod sse;

use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::fmt;

pub use registry::Registry;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    User,
    Assistant,
}

/// One piece of a message.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Part {
    Text {
        text: String,
    },
    /// An image the user attached (base64), for models that read images.
    Image {
        media_type: String,
        data: String,
    },
    /// Reasoning. Anthropic thinking blocks carry a signature and must be passed back
    /// unchanged; other protocols drop them when replaying history.
    Thinking {
        text: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        signature: Option<String>,
        /// Encrypted reasoning (`redacted_thinking`), passed back as-is.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        redacted: Option<String>,
    },
    ToolUse {
        id: String,
        name: String,
        input: Value,
    },
    ToolResult {
        id: String,
        content: String,
        #[serde(default)]
        is_error: bool,
    },
    /// A summary of everything before it, written by Claude's server-side compaction.
    /// Claude takes it back byte-for-byte (signature included); other providers get the
    /// summary as text.
    Compaction {
        content: String,
        signature: String,
    },
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Message {
    pub role: Role,
    pub parts: Vec<Part>,
}

impl Message {
    pub fn user_text(text: impl Into<String>) -> Self {
        Self {
            role: Role::User,
            parts: vec![Part::Text { text: text.into() }],
        }
    }

    pub fn text(&self) -> String {
        self.parts
            .iter()
            .filter_map(|part| match part {
                Part::Text { text } => Some(text.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("")
    }
}

#[derive(Clone, Debug)]
pub struct ToolSpec {
    pub name: String,
    pub description: String,
    pub schema: Value,
}

/// How hard the model should think. `Default` sends nothing and leaves the choice to
/// the provider.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Effort {
    #[default]
    Default,
    Minimal,
    Low,
    Medium,
    High,
    XHigh,
    Max,
}

impl Effort {
    pub const ALL: [Effort; 7] = [
        Effort::Default,
        Effort::Minimal,
        Effort::Low,
        Effort::Medium,
        Effort::High,
        Effort::XHigh,
        Effort::Max,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Effort::Default => "default",
            Effort::Minimal => "minimal",
            Effort::Low => "low",
            Effort::Medium => "medium",
            Effort::High => "high",
            Effort::XHigh => "xhigh",
            Effort::Max => "max",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        Self::ALL
            .into_iter()
            .find(|effort| effort.as_str().eq_ignore_ascii_case(value.trim()))
    }

    pub fn next(self) -> Self {
        let index = Self::ALL.iter().position(|e| *e == self).unwrap_or(0);
        Self::ALL[(index + 1) % Self::ALL.len()]
    }
}

impl fmt::Display for Effort {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

pub struct ChatRequest<'a> {
    pub model: &'a str,
    pub system: &'a str,
    pub messages: &'a [Message],
    pub tools: &'a [ToolSpec],
    pub effort: Effort,
    pub max_tokens: u32,
    /// Ask Claude to summarize the conversation (on-demand server compaction) instead of
    /// answering. Only first-party Anthropic endpoints honor it.
    pub compaction: bool,
    /// Let Claude clear old tool results once the prompt passes this many tokens
    /// (context editing). Only first-party Anthropic endpoints honor it.
    pub clear_tool_results_at: Option<u64>,
}

/// What a provider reports while a response streams in.
#[derive(Clone, Debug)]
pub enum StreamEvent {
    Text(String),
    Thinking(String),
    /// The model started a tool call; its input is still streaming.
    ToolStart {
        id: String,
        name: String,
    },
}

#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize)]
pub struct Usage {
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_read_tokens: u64,
    pub cache_write_tokens: u64,
}

impl Usage {
    pub fn add(&mut self, other: Usage) {
        self.input_tokens += other.input_tokens;
        self.output_tokens += other.output_tokens;
        self.cache_read_tokens += other.cache_read_tokens;
        self.cache_write_tokens += other.cache_write_tokens;
    }

    /// Tokens the last request put in front of the model: its context size.
    pub fn context(&self) -> u64 {
        self.input_tokens + self.cache_read_tokens + self.cache_write_tokens
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum StopReason {
    EndTurn,
    ToolUse,
    MaxTokens,
    Refusal(Option<String>),
    Other(String),
}

pub struct Completion {
    pub message: Message,
    pub stop: StopReason,
    pub usage: Usage,
}

pub type EventSink<'a> = &'a mut (dyn FnMut(StreamEvent) + Send);

/// A configured endpoint that can run a chat request.
#[derive(Clone, Debug)]
pub enum Endpoint {
    Anthropic(anthropic::AnthropicEndpoint),
    OpenAi(openai::OpenAiEndpoint),
}

impl Endpoint {
    /// Whether the endpoint is Anthropic's own API, which offers server-side compaction
    /// and context editing.
    pub fn is_first_party_claude(&self) -> bool {
        match self {
            Endpoint::Anthropic(endpoint) => endpoint.first_party(),
            Endpoint::OpenAi(_) => false,
        }
    }

    /// Whether this endpoint has a "medium" reasoning level. When it does, the
    /// `default` effort means medium; otherwise the provider's own default applies.
    pub fn has_medium_effort(&self) -> bool {
        match self {
            // Adaptive thinking takes `medium`; older models get a medium budget.
            Endpoint::Anthropic(_) => true,
            Endpoint::OpenAi(endpoint) => match &endpoint.reasoning {
                openai::ReasoningParam::Effort | openai::ReasoningParam::Nested => true,
                openai::ReasoningParam::Levels { levels, .. } => {
                    levels.iter().any(|l| l == "medium")
                }
                // On/off switches and servers without a reasoning parameter.
                _ => false,
            },
        }
    }

    /// The effort actually sent: `default` becomes medium where the endpoint has it.
    pub fn effective_effort(&self, effort: Effort) -> Effort {
        if effort == Effort::Default && self.has_medium_effort() {
            Effort::Medium
        } else {
            effort
        }
    }

    pub async fn complete(
        &self,
        client: &reqwest::Client,
        mut request: ChatRequest<'_>,
        sink: EventSink<'_>,
    ) -> anyhow::Result<Completion> {
        request.effort = self.effective_effort(request.effort);
        match self {
            Endpoint::Anthropic(endpoint) => endpoint.complete(client, request, sink).await,
            Endpoint::OpenAi(endpoint) => endpoint.complete(client, request, sink).await,
        }
    }
}

/// Reads an error response body into a short message.
pub(crate) async fn http_error(response: reqwest::Response) -> anyhow::Error {
    let status = response.status();
    let body = response.text().await.unwrap_or_default();
    let detail = serde_json::from_str::<Value>(&body)
        .ok()
        .and_then(|json| {
            json.pointer("/error/message")
                .or_else(|| json.pointer("/message"))
                .or_else(|| json.pointer("/error"))
                .and_then(|v| v.as_str().map(str::to_owned))
        })
        .unwrap_or_else(|| body.chars().take(600).collect());
    anyhow::anyhow!("HTTP {status}: {detail}")
}

/// Sends a request, retrying rate limits, overload and server errors with backoff.
/// `build` makes a fresh request each attempt (signatures and tokens can expire).
pub(crate) async fn send_with_retry<F, Fut>(build: F) -> anyhow::Result<reqwest::Response>
where
    F: Fn() -> Fut,
    Fut: std::future::Future<Output = anyhow::Result<reqwest::RequestBuilder>>,
{
    const ATTEMPTS: u32 = 5;
    let mut delay = std::time::Duration::from_secs(2);
    for attempt in 1..=ATTEMPTS {
        let result = build().await?.send().await;
        let retry = match &result {
            Ok(response) => {
                let status = response.status().as_u16();
                if status == 429 || status == 529 || status >= 500 {
                    // Honour retry-after when the server sends one.
                    if let Some(seconds) = response
                        .headers()
                        .get("retry-after")
                        .and_then(|v| v.to_str().ok())
                        .and_then(|v| v.parse::<u64>().ok())
                    {
                        delay = std::time::Duration::from_secs(seconds.min(60));
                    }
                    true
                } else {
                    false
                }
            }
            Err(error) => error.is_connect() || error.is_timeout(),
        };
        if !retry || attempt == ATTEMPTS {
            let response = result?;
            if !response.status().is_success() {
                return Err(http_error(response).await);
            }
            return Ok(response);
        }
        tracing::warn!(attempt, delay_secs = delay.as_secs(), status = ?result.as_ref().ok().map(|r| r.status().as_u16()), error = ?result.as_ref().err().map(|e| e.to_string()), "model request failed; retrying");
        tokio::time::sleep(delay).await;
        delay = (delay * 2).min(std::time::Duration::from_secs(60));
    }
    unreachable!()
}

/// With `CRACK_DUMP_REQUESTS=<folder>`, saves every request body there as numbered
/// JSON files: the way to see why a provider's prompt cache misses (compare two
/// consecutive requests; everything before the newest message should be identical).
pub fn dump_request(body: &[u8]) {
    static COUNT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
    let Some(dir) = std::env::var_os("CRACK_DUMP_REQUESTS") else {
        return;
    };
    let dir = std::path::PathBuf::from(dir);
    let n = COUNT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let _ = std::fs::create_dir_all(&dir);
    let _ = std::fs::write(
        dir.join(format!("{}-{n:03}.json", std::process::id())),
        body,
    );
}

#[cfg(test)]
mod tests {
    use super::openai::{Auth, OpenAiEndpoint, ReasoningParam};
    use super::*;

    fn openai(reasoning: ReasoningParam) -> Endpoint {
        Endpoint::OpenAi(OpenAiEndpoint {
            base_url: "http://x/v1".into(),
            auth: Auth::None,
            headers: vec![],
            query: vec![],
            reasoning,
            replay_reasoning: false,
            extra_body: vec![],
            max_output_tokens: None,
        })
    }

    #[test]
    fn default_effort_is_medium_where_the_model_has_it() {
        let levels = |l: &[&str]| ReasoningParam::Levels {
            levels: l.iter().map(|s| s.to_string()).collect(),
            off: false,
        };
        // OpenAI-style and GLM (which lists medium) default to medium.
        assert_eq!(
            openai(ReasoningParam::Effort).effective_effort(Effort::Default),
            Effort::Medium
        );
        assert_eq!(
            openai(levels(&["minimal", "low", "medium", "high"])).effective_effort(Effort::Default),
            Effort::Medium
        );
        // Kimi/DeepSeek (low, high, max), Qwen's on/off switch and plain servers keep
        // the provider's own default.
        assert_eq!(
            openai(levels(&["low", "high", "max"])).effective_effort(Effort::Default),
            Effort::Default
        );
        assert_eq!(
            openai(ReasoningParam::EnableThinking).effective_effort(Effort::Default),
            Effort::Default
        );
        assert_eq!(
            openai(ReasoningParam::None).effective_effort(Effort::Default),
            Effort::Default
        );
        // An explicit choice is never changed.
        assert_eq!(
            openai(ReasoningParam::Effort).effective_effort(Effort::High),
            Effort::High
        );
    }
}
