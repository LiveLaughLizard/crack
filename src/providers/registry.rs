//! Built-in providers, user-configured providers, and model discovery.

use super::Endpoint;
use super::anthropic::{self, AnthropicEndpoint, Platform};
use super::openai::{self, OpenAiEndpoint, ReasoningParam};
use crate::config::{Config, ProviderConfig};
use std::collections::BTreeMap;

#[derive(Clone, Debug)]
pub struct ModelInfo {
    pub id: String,
    /// Context window, when the server reported it.
    pub context: Option<u64>,
}

#[derive(Clone, Debug)]
pub struct ProviderInfo {
    pub id: String,
    pub name: String,
    pub endpoint: Endpoint,
    pub models: Vec<ModelInfo>,
    /// Credentials are present (or the local server answered).
    pub available: bool,
    pub discover: bool,
    pub local: bool,
    /// Why the provider is unavailable, or what discovery reported.
    pub note: Option<String>,
    /// Where its credentials come from, for display ("saved in crack", "$OPENAI_API_KEY").
    pub source: Option<String>,
}

fn missing_key(config: &ProviderConfig) -> String {
    if config.api_key_env.is_empty() {
        "add an API key".into()
    } else {
        format!(
            "add an API key (or set {})",
            config.api_key_env.join(" or ")
        )
    }
}

/// Where a provider's credentials come from.
fn key_source(id: &str, config: &ProviderConfig) -> Option<String> {
    if config.api_key.as_ref().is_some_and(|k| !k.is_empty()) {
        return Some(if crate::config::has_saved_key(id) {
            "key saved in crack".into()
        } else {
            "key in config.toml".into()
        });
    }
    if let Some(name) = config
        .api_key_env
        .iter()
        .find(|name| std::env::var(name).is_ok_and(|v| !v.trim().is_empty()))
    {
        return Some(format!("${name}"));
    }
    match config.kind.as_deref() {
        Some("bedrock") => {
            if env(&["AWS_BEARER_TOKEN_BEDROCK"]).is_some() {
                Some("$AWS_BEARER_TOKEN_BEDROCK".into())
            } else if let Some(profile) = config.profile.as_ref().filter(|p| !p.is_empty()) {
                Some(format!("AWS profile `{profile}`"))
            } else if env(&["AWS_ACCESS_KEY_ID"]).is_some() {
                Some("$AWS_ACCESS_KEY_ID".into())
            } else {
                env(&["AWS_PROFILE"]).map(|p| format!("AWS profile `{p}`"))
            }
        }
        Some("vertex") => Some("Google sign-in (gcloud)".into()),
        _ => None,
    }
}

/// The effective settings of provider `id` (built-in merged with config), or the
/// default for a new OpenAI-compatible server.
pub fn provider_config(config: &Config, id: &str) -> ProviderConfig {
    let builtin = builtins()
        .into_iter()
        .find(|(b, _)| b == id)
        .map(|(_, c)| c)
        .unwrap_or_else(|| ProviderConfig {
            kind: Some("openai".into()),
            discover: Some(true),
            ..Default::default()
        });
    match config.provider.get(id) {
        Some(user) => user.clone().merged_over(&builtin),
        None => builtin,
    }
}

/// Checks that a provider is reachable with the given settings, without saving them.
/// Lists models where the API can; otherwise sends a tiny request to `model`.
pub async fn test_connection(
    id: &str,
    config: &ProviderConfig,
    model: Option<&str>,
    client: &reqwest::Client,
) -> anyhow::Result<String> {
    let endpoint = build(id, config).map_err(|e| anyhow::anyhow!("{e}"))?;
    let mut list_error = None;
    match &endpoint {
        Endpoint::OpenAi(e) => match e.list_models(client).await {
            Ok(models) => {
                let models: Vec<String> = models.into_iter().filter(|m| is_chat_model(m)).collect();
                return Ok(format!("Connected: {} models available.", models.len()));
            }
            Err(error) => list_error = Some(error),
        },
        Endpoint::Anthropic(e) => {
            if let Some(models) = e.list_models(client).await? {
                return Ok(format!("Connected: {} models available.", models.len()));
            }
        }
    }
    let Some(model) = model
        .map(str::to_owned)
        .or_else(|| config.models.first().cloned())
    else {
        // Nothing to send a test request to: the listing failure is the answer.
        return Err(match list_error {
            Some(error) => anyhow::anyhow!("Couldn't reach the server: {}", describe(&error)),
            None => anyhow::anyhow!("add a model to test with"),
        });
    };
    let messages = [super::Message::user_text("Reply with the word OK.")];
    let mut sink = |_| {};
    let request = super::ChatRequest {
        model: &model,
        system: "",
        messages: &messages,
        tools: &[],
        effort: super::Effort::Low,
        max_tokens: 64,
        compaction: false,
        clear_tool_results_at: None,
        web_search: false,
    };
    tokio::time::timeout(
        std::time::Duration::from_secs(60),
        endpoint.complete(client, request, &mut sink),
    )
    .await
    .map_err(|_| anyhow::anyhow!("no answer within 60s"))??;
    Ok(format!("Connected: {model} answered."))
}

#[derive(Clone, Debug, Default)]
pub struct Registry {
    pub providers: Vec<ProviderInfo>,
    /// `[models."provider/model"]` from the config.
    pub model_overrides: BTreeMap<String, super::models::ModelCaps>,
}

fn env(names: &[&str]) -> Option<String> {
    names
        .iter()
        .find_map(|name| std::env::var(name).ok().filter(|v| !v.trim().is_empty()))
}

fn strings(items: &[&str]) -> Vec<String> {
    items.iter().map(|s| s.to_string()).collect()
}

#[cfg(feature = "cloud")]
const CLAUDE_MODELS: [&str; 5] = [
    "claude-opus-5-5",
    "claude-sonnet-5-5",
    "claude-haiku-5-5",
    "claude-fable-5-1",
    "claude-opus-5",
];

/// The providers crack knows without configuration, in default-selection order.
/// Which exist depends on the build: `cloud` adds hosted providers, `china` adds
/// the Chinese model clouds; an air-gapped build (no features) has only local ones.
fn builtins() -> Vec<(String, ProviderConfig)> {
    let mut list = Vec::new();
    #[cfg(feature = "cloud")]
    cloud_builtins(&mut list);
    #[cfg(feature = "china")]
    china_builtins(&mut list);
    local_builtins(&mut list);
    list
}

type Builtins = Vec<(String, ProviderConfig)>;

#[cfg(feature = "cloud")]
fn cloud_builtins(list: &mut Builtins) {
    let mut add = |id: &str, config: ProviderConfig| list.push((id.to_owned(), config));

    add(
        "anthropic",
        ProviderConfig {
            kind: Some("anthropic".into()),
            name: Some("Anthropic".into()),
            base_url: env(&["ANTHROPIC_BASE_URL"]).or(Some("https://api.anthropic.com".into())),
            api_key_env: strings(&["ANTHROPIC_API_KEY", "ANTHROPIC_AUTH_TOKEN"]),
            models: strings(&CLAUDE_MODELS),
            ..Default::default()
        },
    );
    add(
        "openai",
        ProviderConfig {
            kind: Some("openai".into()),
            name: Some("OpenAI".into()),
            base_url: env(&["OPENAI_BASE_URL"]).or(Some("https://api.openai.com/v1".into())),
            api_key_env: strings(&["OPENAI_API_KEY"]),
            discover: Some(true),
            ..Default::default()
        },
    );
    add(
        "google",
        ProviderConfig {
            kind: Some("openai".into()),
            name: Some("Google Gemini".into()),
            base_url: Some("https://generativelanguage.googleapis.com/v1beta/openai".into()),
            api_key_env: strings(&["GEMINI_API_KEY", "GOOGLE_API_KEY"]),
            discover: Some(true),
            ..Default::default()
        },
    );
    add(
        "bedrock",
        ProviderConfig {
            kind: Some("bedrock".into()),
            name: Some("Amazon Bedrock".into()),
            region: env(&["AWS_REGION", "AWS_DEFAULT_REGION"]).or(Some("us-east-1".into())),
            models: CLAUDE_MODELS
                .iter()
                .map(|m| format!("anthropic.{m}"))
                .collect(),
            ..Default::default()
        },
    );
    add(
        "vertex",
        ProviderConfig {
            kind: Some("vertex".into()),
            name: Some("Google Vertex AI".into()),
            project: env(&[
                "ANTHROPIC_VERTEX_PROJECT_ID",
                "GOOGLE_CLOUD_PROJECT",
                "GCLOUD_PROJECT",
            ]),
            region: env(&["CLOUD_ML_REGION"]).or(Some("global".into())),
            models: strings(&CLAUDE_MODELS),
            ..Default::default()
        },
    );
    add(
        "azure",
        ProviderConfig {
            kind: Some("azure".into()),
            name: Some("Azure OpenAI".into()),
            base_url: env(&["AZURE_OPENAI_ENDPOINT"]),
            api_key_env: strings(&["AZURE_OPENAI_API_KEY"]),
            models: env(&["AZURE_OPENAI_DEPLOYMENTS", "AZURE_OPENAI_DEPLOYMENT"])
                .map(|v| v.split(',').map(|s| s.trim().to_owned()).collect())
                .unwrap_or_default(),
            ..Default::default()
        },
    );
    add(
        "foundry",
        ProviderConfig {
            kind: Some("anthropic".into()),
            name: Some("Microsoft Foundry".into()),
            base_url: env(&["ANTHROPIC_FOUNDRY_BASE_URL"]).or_else(|| {
                env(&["ANTHROPIC_FOUNDRY_RESOURCE"])
                    .map(|r| format!("https://{r}.services.ai.azure.com/anthropic"))
            }),
            api_key_env: strings(&["ANTHROPIC_FOUNDRY_API_KEY"]),
            models: strings(&CLAUDE_MODELS),
            ..Default::default()
        },
    );
    for (id, name, url, key, reasoning) in [
        (
            "openrouter",
            "OpenRouter",
            "https://openrouter.ai/api/v1",
            "OPENROUTER_API_KEY",
            "nested",
        ),
        (
            "groq",
            "Groq",
            "https://api.groq.com/openai/v1",
            "GROQ_API_KEY",
            "effort",
        ),
        ("xai", "xAI", "https://api.x.ai/v1", "XAI_API_KEY", "effort"),
        (
            "mistral",
            "Mistral",
            "https://api.mistral.ai/v1",
            "MISTRAL_API_KEY",
            "none",
        ),
        (
            "together",
            "Together AI",
            "https://api.together.xyz/v1",
            "TOGETHER_API_KEY",
            "none",
        ),
        (
            "fireworks",
            "Fireworks",
            "https://api.fireworks.ai/inference/v1",
            "FIREWORKS_API_KEY",
            "effort",
        ),
    ] {
        add(
            id,
            ProviderConfig {
                kind: Some("openai".into()),
                name: Some(name.into()),
                base_url: Some(url.into()),
                api_key_env: vec![key.into()],
                reasoning: Some(reasoning.into()),
                discover: Some(true),
                ..Default::default()
            },
        );
    }
}

/// Chinese model clouds. All of them want earlier reasoning sent back
/// (`reasoning_content`) when tools are used, and their keys only work in the region
/// that issued them; the international endpoints are the defaults.
#[cfg(feature = "china")]
fn china_builtins(list: &mut Builtins) {
    let mut add = |id: &str, config: ProviderConfig| list.push((id.to_owned(), config));
    let base = |var: &str, default: &str| env(&[var]).or(Some(default.to_owned()));
    add(
        "qwen",
        ProviderConfig {
            kind: Some("openai".into()),
            name: Some("Qwen (Alibaba Cloud)".into()),
            base_url: base(
                "DASHSCOPE_BASE_URL",
                "https://dashscope-intl.aliyuncs.com/compatible-mode/v1",
            ),
            api_key_env: strings(&["DASHSCOPE_API_KEY"]),
            // No documented model listing on the compatible-mode API.
            models: strings(&[
                "qwen4-max",
                "qwen3.8-max",
                "qwen3.7-plus",
                "qwen3.8-flash",
                "qwen3-coder-flash",
            ]),
            discover: Some(false),
            reasoning: Some("enable_thinking".into()),
            replay_reasoning: Some(true),
            ..Default::default()
        },
    );
    add(
        "kimi",
        ProviderConfig {
            kind: Some("openai".into()),
            name: Some("Kimi (Moonshot AI)".into()),
            base_url: base("MOONSHOT_BASE_URL", "https://api.moonshot.ai/v1"),
            api_key_env: strings(&["MOONSHOT_API_KEY"]),
            models: strings(&["kimi-k3.1-pro", "kimi-k3.1-pro-highspeed", "kimi-k3"]),
            discover: Some(true),
            // Always thinks; takes reasoning_effort low|high|max and no `thinking` field.
            reasoning: Some("levels:low,high,max".into()),
            replay_reasoning: Some(true),
            // Kimi asks for max_tokens of at least 16000 with its thinking models.
            max_output_tokens: Some(32_768),
            ..Default::default()
        },
    );
    add(
        "zai",
        ProviderConfig {
            kind: Some("openai".into()),
            name: Some("Z.ai (GLM)".into()),
            base_url: base("ZAI_BASE_URL", "https://api.z.ai/api/paas/v4"),
            api_key_env: strings(&["ZAI_API_KEY", "ZHIPUAI_API_KEY"]),
            models: strings(&["glm-5.3", "glm-5.2", "glm-5.1", "glm-4.7"]),
            discover: Some(false),
            reasoning: Some("levels:minimal,low,medium,high,xhigh,max".into()),
            replay_reasoning: Some(true),
            ..Default::default()
        },
    );
    add(
        "minimax",
        ProviderConfig {
            kind: Some("openai".into()),
            name: Some("MiniMax".into()),
            base_url: base("MINIMAX_BASE_URL", "https://api.minimax.io/v1"),
            api_key_env: strings(&["MINIMAX_API_KEY"]),
            models: strings(&["MiniMax-M3", "MiniMax-M3.1-Flash"]),
            discover: Some(false),
            reasoning: Some("none".into()),
            replay_reasoning: Some(true),
            // Reasoning in `reasoning_content` rather than inline <think> tags.
            extra_body: [("reasoning_split".to_owned(), serde_json::json!(true))].into(),
            ..Default::default()
        },
    );
    add(
        "deepseek",
        ProviderConfig {
            kind: Some("openai".into()),
            name: Some("DeepSeek".into()),
            base_url: base("DEEPSEEK_BASE_URL", "https://api.deepseek.com/v1"),
            api_key_env: strings(&["DEEPSEEK_API_KEY"]),
            models: strings(&["deepseek-v4-pro", "deepseek-flash"]),
            discover: Some(true),
            // Thinking on by default; minimal turns it off, the rest set its effort.
            reasoning: Some("levels+off:low,high,max".into()),
            replay_reasoning: Some(true),
            ..Default::default()
        },
    );
}

fn local_builtins(list: &mut Builtins) {
    let mut add = |id: &str, config: ProviderConfig| list.push((id.to_owned(), config));
    // Self-hosted servers on their default ports. They show up when they answer.
    let ollama = env(&["OLLAMA_HOST"])
        .map(|h| {
            if h.starts_with("http") {
                h
            } else {
                format!("http://{h}")
            }
        })
        .unwrap_or_else(|| "http://localhost:11434".into());
    add(
        "ollama",
        ProviderConfig {
            kind: Some("openai".into()),
            name: Some("Ollama".into()),
            base_url: Some(format!("{}/v1", ollama.trim_end_matches('/'))),
            auth: Some("none".into()),
            discover: Some(true),
            local: Some(true),
            reasoning: Some("effort".into()),
            ..Default::default()
        },
    );
    add(
        "lmstudio",
        ProviderConfig {
            kind: Some("openai".into()),
            name: Some("LM Studio".into()),
            base_url: Some("http://localhost:1234/v1".into()),
            auth: Some("none".into()),
            discover: Some(true),
            local: Some(true),
            reasoning: Some("none".into()),
            ..Default::default()
        },
    );
    // Any OpenAI-compatible server (vLLM, llama.cpp, LiteLLM, TGI...) from the environment.
    if let Some(url) = env(&["CRACK_BASE_URL"]) {
        add(
            "custom",
            ProviderConfig {
                kind: Some("openai".into()),
                name: Some("OpenAI-compatible".into()),
                base_url: Some(url),
                api_key_env: strings(&["CRACK_API_KEY"]),
                auth: env(&["CRACK_API_KEY"]).map_or(Some("none".into()), |_| None),
                discover: Some(true),
                ..Default::default()
            },
        );
    }
}

fn key_from(config: &ProviderConfig) -> Option<String> {
    config
        .api_key
        .clone()
        .filter(|k| !k.is_empty())
        .or_else(|| {
            let names: Vec<&str> = config.api_key_env.iter().map(String::as_str).collect();
            env(&names)
        })
}

/// Builds one provider. `Err` carries why it can't be used.
fn build(id: &str, config: &ProviderConfig) -> Result<Endpoint, String> {
    let kind = config.kind.as_deref().unwrap_or("openai");
    let key = key_from(config);
    let headers: Vec<(String, String)> = config.headers.clone().into_iter().collect();
    match kind {
        "anthropic" => {
            let base_url = config.base_url.clone().ok_or("add the endpoint URL")?;
            let auth = match (config.auth.as_deref(), key) {
                (Some("none"), _) => anthropic::Auth::None,
                #[cfg(feature = "cloud")]
                (Some("google"), _) => anthropic::Auth::Google,
                (Some("bearer"), Some(key)) => anthropic::Auth::Bearer(key),
                (_, Some(key)) if id == "anthropic" && env(&["ANTHROPIC_API_KEY"]).is_none() => {
                    // Only ANTHROPIC_AUTH_TOKEN is set: an OAuth bearer token.
                    anthropic::Auth::Bearer(key)
                }
                (_, Some(key)) => anthropic::Auth::ApiKey(key),
                (_, None) => {
                    return Err(missing_key(config));
                }
            };
            Ok(Endpoint::Anthropic(AnthropicEndpoint {
                platform: Platform::Direct { base_url },
                auth,
                headers,
            }))
        }
        #[cfg(feature = "cloud")]
        "bedrock" => {
            let region = config.region.clone().unwrap_or_else(|| "us-east-1".into());
            let service = config
                .sigv4_service
                .clone()
                .unwrap_or_else(|| "bedrock-mantle".into());
            // A profile chosen in crack wins over keys that happen to be in the environment.
            let profile = config.profile.clone().filter(|p| !p.is_empty());
            let auth = if let Some(token) = key.or_else(|| env(&["AWS_BEARER_TOKEN_BEDROCK"])) {
                anthropic::Auth::Bearer(token)
            } else if let Some(profile) = profile {
                anthropic::Auth::SigV4 {
                    credentials: anthropic::AwsCredentials::Profile(profile),
                    service,
                }
            } else if let (Some(access_key), Some(secret_key)) =
                (env(&["AWS_ACCESS_KEY_ID"]), env(&["AWS_SECRET_ACCESS_KEY"]))
            {
                anthropic::Auth::SigV4 {
                    credentials: anthropic::AwsCredentials::Static {
                        access_key,
                        secret_key,
                        session_token: env(&["AWS_SESSION_TOKEN"]),
                    },
                    service,
                }
            } else if let Some(profile) = env(&["AWS_PROFILE"]) {
                anthropic::Auth::SigV4 {
                    credentials: anthropic::AwsCredentials::Profile(profile),
                    service,
                }
            } else {
                return Err("add a Bedrock API key or an AWS profile".into());
            };
            Ok(Endpoint::Anthropic(AnthropicEndpoint {
                platform: Platform::Bedrock { region },
                auth,
                headers,
            }))
        }
        #[cfg(feature = "cloud")]
        "vertex" => {
            let project = config
                .project
                .clone()
                .ok_or("add your Google Cloud project ID")?;
            Ok(Endpoint::Anthropic(AnthropicEndpoint {
                platform: Platform::Vertex {
                    project,
                    region: config.region.clone().unwrap_or_else(|| "global".into()),
                },
                auth: anthropic::Auth::Google,
                headers,
            }))
        }
        "openai" | "azure" if kind == "openai" || cfg!(feature = "cloud") => {
            let mut base_url = config.base_url.clone().ok_or("add the endpoint URL")?;
            if kind == "azure" && !base_url.contains("/openai") {
                base_url = format!("{}/openai/v1", base_url.trim_end_matches('/'));
            }
            let auth = match (config.auth.as_deref(), key) {
                (Some("none"), _) => openai::Auth::None,
                #[cfg(feature = "cloud")]
                (Some("google"), _) => openai::Auth::Google,
                (Some("api-key"), Some(key)) => openai::Auth::Header("api-key".into(), key),
                (Some("x-api-key"), Some(key)) => openai::Auth::Header("x-api-key".into(), key),
                (_, Some(key)) if kind == "azure" => openai::Auth::Header("api-key".into(), key),
                (_, Some(key)) => openai::Auth::Bearer(key),
                (_, None) if config.local == Some(true) => openai::Auth::None,
                (_, None) => {
                    return Err(missing_key(config));
                }
            };
            let reasoning = match config.reasoning.as_deref() {
                Some("none") => ReasoningParam::None,
                Some("nested") => ReasoningParam::Nested,
                Some("thinking") => ReasoningParam::Thinking,
                Some("enable_thinking") => ReasoningParam::EnableThinking,
                Some(spec) if spec.starts_with("levels") => {
                    // "levels:low,high,max", or "levels+off:..." to switch thinking off at minimal.
                    let (head, list) = spec.split_once(':').unwrap_or((spec, ""));
                    ReasoningParam::Levels {
                        levels: list
                            .split(',')
                            .map(|s| s.trim().to_owned())
                            .filter(|s| !s.is_empty())
                            .collect(),
                        off: head == "levels+off",
                    }
                }
                _ => ReasoningParam::Effort,
            };
            Ok(Endpoint::OpenAi(OpenAiEndpoint {
                base_url,
                auth,
                headers,
                query: config.query.clone().into_iter().collect(),
                reasoning,
                replay_reasoning: config.replay_reasoning.unwrap_or(false),
                extra_body: config.extra_body.clone().into_iter().collect(),
                max_output_tokens: config.max_output_tokens,
            }))
        }
        #[cfg(not(feature = "cloud"))]
        "bedrock" | "vertex" | "azure" => Err(format!(
            "{kind} isn't available in this build ({})",
            build_flavor()
        )),
        other => Err(format!("unknown provider kind '{other}'")),
    }
}

impl Registry {
    pub fn load(config: &Config) -> Registry {
        let mut configs: Vec<(String, ProviderConfig)> = builtins();
        let mut user: BTreeMap<String, ProviderConfig> = config.provider.clone();
        for (id, provider) in configs.iter_mut() {
            if let Some(over) = user.remove(id) {
                *provider = over.merged_over(provider);
            }
        }
        configs.extend(user);

        let providers = configs
            .into_iter()
            .filter(|(id, _)| !config.disabled_providers.contains(id))
            .map(|(id, provider)| {
                let name = provider.name.clone().unwrap_or_else(|| id.clone());
                let local = provider.local == Some(true);
                let models: Vec<ModelInfo> = provider
                    .models
                    .iter()
                    .map(|m| ModelInfo {
                        id: m.clone(),
                        context: None,
                    })
                    .collect();
                // OpenAI-compatible servers list their own models unless told otherwise.
                let discover = provider
                    .discover
                    .unwrap_or(provider.kind.as_deref().unwrap_or("openai") == "openai");
                let source = key_source(&id, &provider);
                match build(&id, &provider) {
                    Ok(endpoint) => ProviderInfo {
                        available: !local,
                        id,
                        name,
                        endpoint,
                        models,
                        discover,
                        local,
                        note: None,
                        source,
                    },
                    // Unconfigured providers stay listed so the picker can say what's missing.
                    Err(note) => ProviderInfo {
                        available: false,
                        id,
                        name,
                        endpoint: Endpoint::OpenAi(OpenAiEndpoint {
                            base_url: String::new(),
                            auth: openai::Auth::None,
                            headers: vec![],
                            query: vec![],
                            reasoning: ReasoningParam::None,
                            replay_reasoning: false,
                            extra_body: vec![],
                            max_output_tokens: None,
                        }),
                        models,
                        discover: false,
                        local,
                        note: Some(note),
                        source,
                    },
                }
            })
            .collect();
        Registry {
            providers,
            model_overrides: config.models.clone(),
        }
    }

    /// Asks OpenAI-compatible servers for their models. Local servers that don't
    /// answer stay unavailable.
    pub async fn discover(&mut self, client: &reqwest::Client) {
        let jobs = self
            .providers
            .iter()
            .enumerate()
            .filter(|(_, p)| p.discover && (p.available || p.local))
            .filter_map(|(i, p)| match &p.endpoint {
                Endpoint::OpenAi(endpoint) => Some((i, endpoint.clone())),
                _ => None,
            })
            .map(|(i, endpoint)| async move { (i, endpoint.list_models_detailed(client).await) });
        for (index, result) in futures::future::join_all(jobs).await {
            let provider = &mut self.providers[index];
            match result {
                Ok(models) => {
                    for (id, context) in models.into_iter().filter(|(m, _)| is_chat_model(m)) {
                        match provider.models.iter_mut().find(|m| m.id == id) {
                            Some(known) => known.context = known.context.or(context),
                            None => provider.models.push(ModelInfo { id, context }),
                        }
                    }
                    if provider.local {
                        provider.available = !provider.models.is_empty();
                    }
                }
                Err(error) => {
                    if !provider.local {
                        provider.note = Some(format!("model list: {error}"));
                    }
                }
            }
        }
    }

    pub fn find(&self, provider: &str) -> Option<&ProviderInfo> {
        self.providers.iter().find(|p| p.id == provider)
    }

    /// Splits `provider/model` and checks the provider is usable.
    pub fn resolve(&self, model_ref: &str) -> anyhow::Result<(&ProviderInfo, String)> {
        let (provider_id, model) = model_ref.split_once('/').ok_or_else(|| {
            anyhow::anyhow!("model must be written provider/model, got '{model_ref}'")
        })?;
        let provider = self
            .find(provider_id)
            .ok_or_else(|| anyhow::anyhow!("unknown provider '{provider_id}'"))?;
        if let Some(note) = provider.note.as_ref().filter(|_| !provider.available) {
            anyhow::bail!("provider '{provider_id}' is not configured: {note}");
        }
        Ok((provider, model.to_owned()))
    }

    /// The first model of the first usable provider.
    pub fn default_model(&self) -> Option<String> {
        self.providers
            .iter()
            .filter(|p| p.available)
            .find_map(|p| p.models.first().map(|m| format!("{}/{}", p.id, m.id)))
    }

    pub fn available(&self) -> impl Iterator<Item = &ProviderInfo> {
        self.providers.iter().filter(|p| p.available)
    }
}

/// Drops embedding, speech and image models from discovered lists.
fn is_chat_model(id: &str) -> bool {
    const SKIP: [&str; 12] = [
        "embed",
        "whisper",
        "tts",
        "dall-e",
        "moderation",
        "image",
        "audio",
        "realtime",
        "transcribe",
        "rerank",
        "search-",
        "davinci",
    ];
    let lower = id.to_ascii_lowercase();
    !SKIP.iter().any(|s| lower.contains(s))
}

/// A readable one-line reason for a connection failure.
fn describe(error: &anyhow::Error) -> String {
    if let Some(e) = error.downcast_ref::<reqwest::Error>() {
        if e.is_builder() {
            return "that isn't a valid URL".into();
        }
        if e.is_connect() {
            return "nothing is listening there (is the server running?)".into();
        }
        if e.is_timeout() {
            return "it didn't answer in time".into();
        }
    }
    format!("{error:#}")
}

/// Which services this binary was built with, for messages and the interface.
pub fn build_flavor() -> &'static str {
    if cfg!(feature = "china") {
        "all providers"
    } else if cfg!(feature = "cloud") {
        "no Chinese providers"
    } else {
        "air-gapped: local servers only"
    }
}

impl Registry {
    /// What's known about `provider/model`: the config's override, then what the
    /// server reported, then the built-in table.
    pub fn caps(&self, model_ref: &str) -> super::models::ModelCaps {
        let (provider, model) = model_ref.split_once('/').unwrap_or(("", model_ref));
        let discovered = self
            .find(provider)
            .and_then(|p| p.models.iter().find(|m| m.id == model))
            .and_then(|m| m.context);
        let reported = super::models::ModelCaps {
            context: discovered,
            ..Default::default()
        };
        self.model_overrides
            .get(model_ref)
            .copied()
            .unwrap_or_default()
            .or(reported)
            .or(super::models::builtin(model))
    }
}
