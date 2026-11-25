//! What the Providers dialog shows for each built-in provider: a description, links
//! to get credentials, the fields to fill in, and an optional browser sign-in.

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Field {
    ApiKey,
    BaseUrl,
    Region,
    Project,
    Profile,
    Models,
    Name,
}

impl Field {
    /// The config key the field is saved under (`api_key` goes to credentials.toml).
    pub fn key(self) -> &'static str {
        match self {
            Field::ApiKey => "api_key",
            Field::BaseUrl => "base_url",
            Field::Region => "region",
            Field::Project => "project",
            Field::Profile => "profile",
            Field::Models => "models",
            Field::Name => "name",
        }
    }
}

#[derive(Clone, Debug)]
pub struct FieldSpec {
    pub field: Field,
    pub label: &'static str,
    pub placeholder: &'static str,
    pub help: &'static str,
}

/// A command that signs in through the browser, e.g. `gcloud auth login`.
#[derive(Clone, Debug)]
pub struct SignIn {
    pub label: &'static str,
    /// The program and its arguments; `{profile}` is replaced by the Profile field.
    pub command: &'static [&'static str],
    pub install_url: &'static str,
}

#[derive(Clone, Debug)]
pub struct Entry {
    pub group: Group,
    pub blurb: &'static str,
    pub links: Vec<(&'static str, &'static str)>,
    pub fields: Vec<FieldSpec>,
    pub sign_in: Option<SignIn>,
    /// Region presets for the base URL, e.g. ("International", "https://…"),
    /// ("China mainland", "https://…"). Shown as toggles above the fields.
    pub regions: Vec<(&'static str, &'static str)>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Group {
    #[cfg(feature = "cloud")]
    Api,
    #[cfg(feature = "china")]
    China,
    #[cfg(feature = "cloud")]
    Cloud,
    Local,
}

impl Group {
    pub fn title(self) -> &'static str {
        match self {
            #[cfg(feature = "cloud")]
            Group::Api => "Model APIs",
            #[cfg(feature = "china")]
            Group::China => "Chinese model clouds",
            #[cfg(feature = "cloud")]
            Group::Cloud => "Cloud platforms",
            Group::Local => "Self-hosted",
        }
    }
}

#[cfg(feature = "cloud")]
fn api_key(help: &'static str) -> FieldSpec {
    FieldSpec {
        field: Field::ApiKey,
        label: "API key",
        placeholder: "Paste your API key",
        help,
    }
}

fn base_url(placeholder: &'static str, help: &'static str) -> FieldSpec {
    FieldSpec {
        field: Field::BaseUrl,
        label: "Base URL",
        placeholder,
        help,
    }
}

fn models(help: &'static str) -> FieldSpec {
    FieldSpec {
        field: Field::Models,
        label: "Models",
        placeholder: "Comma-separated; leave empty to list them from the server",
        help,
    }
}

#[cfg(feature = "cloud")]
fn simple(blurb: &'static str, key_url: &'static str) -> Entry {
    Entry {
        group: Group::Api,
        blurb,
        links: vec![("Create an API key", key_url)],
        fields: vec![api_key(
            "Saved to ~/.config/crack/credentials.toml (readable only by you).",
        )],
        sign_in: None,
        regions: vec![],
    }
}

/// The catalog entry for a provider, or a generic one for user-added providers.
pub fn entry(id: &str) -> Entry {
    #[cfg(feature = "china")]
    if let Some(entry) = china_entry(id) {
        return entry;
    }
    #[cfg(feature = "cloud")]
    if let Some(entry) = cloud_entry(id) {
        return entry;
    }
    local_entry(id)
}

#[cfg(feature = "cloud")]
fn cloud_entry(id: &str) -> Option<Entry> {
    Some(match id {
        "anthropic" => Entry {
            group: Group::Api,
            blurb: "Claude models directly from Anthropic.",
            links: vec![
                (
                    "Create an API key",
                    "https://platform.claude.com/settings/keys",
                ),
                ("Billing", "https://platform.claude.com/settings/billing"),
            ],
            fields: vec![
                api_key("Saved to ~/.config/crack/credentials.toml (readable only by you)."),
                base_url(
                    "https://api.anthropic.com",
                    "Only change this for a proxy or gateway.",
                ),
            ],
            sign_in: None,
            regions: vec![],
        },
        "openai" => Entry {
            group: Group::Api,
            blurb: "GPT and o-series models from OpenAI.",
            links: vec![
                ("Create an API key", "https://platform.openai.com/api-keys"),
                (
                    "Usage and billing",
                    "https://platform.openai.com/settings/organization/billing",
                ),
            ],
            fields: vec![
                api_key("Saved to ~/.config/crack/credentials.toml (readable only by you)."),
                base_url(
                    "https://api.openai.com/v1",
                    "Only change this for a proxy or gateway.",
                ),
            ],
            sign_in: None,
            regions: vec![],
        },
        "google" => simple(
            "Gemini models through Google AI Studio.",
            "https://aistudio.google.com/apikey",
        ),
        "openrouter" => simple(
            "Hundreds of models from many labs behind one key.",
            "https://openrouter.ai/settings/keys",
        ),
        "groq" => simple(
            "Fast inference of open models.",
            "https://console.groq.com/keys",
        ),
        "xai" => simple("Grok models from xAI.", "https://console.x.ai"),
        "mistral" => simple(
            "Mistral and Codestral models.",
            "https://console.mistral.ai/api-keys",
        ),
        "together" => simple(
            "Open models hosted by Together AI.",
            "https://api.together.ai/settings/api-keys",
        ),
        "fireworks" => simple(
            "Open models hosted by Fireworks.",
            "https://fireworks.ai/account/api-keys",
        ),
        "bedrock" => Entry {
            group: Group::Cloud,
            blurb: "Claude in Amazon Bedrock. Use a Bedrock API key, or an AWS profile \
                    (access keys or IAM Identity Center / SSO). Model access must be enabled \
                    in the Bedrock console for your region.",
            links: vec![
                (
                    "Create a Bedrock API key",
                    "https://console.aws.amazon.com/bedrock/home#/api-keys",
                ),
                (
                    "Enable model access",
                    "https://console.aws.amazon.com/bedrock/home#/modelaccess",
                ),
                (
                    "Set up AWS SSO",
                    "https://docs.aws.amazon.com/cli/latest/userguide/sso-configure-profile-token.html",
                ),
            ],
            fields: vec![
                FieldSpec {
                    field: Field::ApiKey,
                    label: "Bedrock API key",
                    placeholder: "Optional if you use an AWS profile",
                    help: "Long-term or short-term Bedrock API key.",
                },
                FieldSpec {
                    field: Field::Profile,
                    label: "AWS profile",
                    placeholder: "default",
                    help: "A profile from ~/.aws/config. SSO profiles sign in with the button below.",
                },
                FieldSpec {
                    field: Field::Region,
                    label: "Region",
                    placeholder: "us-east-1",
                    help: "A region where Claude is enabled for your account.",
                },
            ],
            sign_in: Some(SignIn {
                label: "Sign in with AWS SSO",
                command: &["aws", "sso", "login", "--profile", "{profile}"],
                install_url: "https://docs.aws.amazon.com/cli/latest/userguide/getting-started-install.html",
            }),
            regions: vec![],
        },
        "vertex" => Entry {
            group: Group::Cloud,
            blurb: "Claude on Google Cloud Vertex AI. Signs in with your Google account \
                    through the gcloud CLI. Enable the Claude models in Model Garden first.",
            links: vec![
                (
                    "Enable Claude in Model Garden",
                    "https://console.cloud.google.com/vertex-ai/model-garden",
                ),
                (
                    "Find your project ID",
                    "https://console.cloud.google.com/home/dashboard",
                ),
            ],
            fields: vec![
                FieldSpec {
                    field: Field::Project,
                    label: "Project ID",
                    placeholder: "my-gcp-project",
                    help: "The Google Cloud project with Vertex AI enabled.",
                },
                FieldSpec {
                    field: Field::Region,
                    label: "Region",
                    placeholder: "global",
                    help: "`global`, `us`, `eu`, or a region such as us-east5.",
                },
            ],
            sign_in: Some(SignIn {
                label: "Sign in with Google",
                command: &["gcloud", "auth", "login"],
                install_url: "https://cloud.google.com/sdk/docs/install",
            }),
            regions: vec![],
        },
        "azure" => Entry {
            group: Group::Cloud,
            blurb: "OpenAI models deployed in your Azure OpenAI / AI Foundry resource. \
                    Models are your deployment names.",
            links: vec![
                ("Open Azure AI Foundry", "https://ai.azure.com"),
                (
                    "Find keys and endpoint",
                    "https://portal.azure.com/#browse/Microsoft.CognitiveServices%2Faccounts",
                ),
            ],
            fields: vec![
                FieldSpec {
                    field: Field::BaseUrl,
                    label: "Endpoint",
                    placeholder: "https://YOUR-RESOURCE.openai.azure.com",
                    help: "From the resource's Keys and Endpoint page.",
                },
                api_key("Key 1 or Key 2 from the same page."),
                FieldSpec {
                    field: Field::Models,
                    label: "Deployments",
                    placeholder: "gpt-5, gpt-5-mini",
                    help: "Comma-separated deployment names.",
                },
            ],
            sign_in: None,
            regions: vec![],
        },
        "foundry" => Entry {
            group: Group::Cloud,
            blurb: "Claude in Microsoft Foundry.",
            links: vec![("Open Azure AI Foundry", "https://ai.azure.com")],
            fields: vec![
                FieldSpec {
                    field: Field::BaseUrl,
                    label: "Endpoint",
                    placeholder: "https://YOUR-RESOURCE.services.ai.azure.com/anthropic",
                    help: "The Anthropic endpoint of your Foundry resource.",
                },
                api_key("The resource's API key."),
            ],
            sign_in: None,
            regions: vec![],
        },
        _ => return None,
    })
}

fn local_entry(id: &str) -> Entry {
    match id {
        "ollama" => Entry {
            group: Group::Local,
            blurb: "Models running on your machine with Ollama. Pull one first, \
                    e.g. `ollama pull qwen3-coder`.",
            links: vec![
                ("Download Ollama", "https://ollama.com/download"),
                ("Browse models", "https://ollama.com/search"),
            ],
            fields: vec![base_url(
                "http://localhost:11434/v1",
                "Where Ollama listens.",
            )],
            sign_in: None,
            regions: vec![],
        },
        "lmstudio" => Entry {
            group: Group::Local,
            blurb: "Models served by LM Studio's local server (Developer tab → Start server).",
            links: vec![("Download LM Studio", "https://lmstudio.ai")],
            fields: vec![base_url(
                "http://localhost:1234/v1",
                "Where LM Studio's server listens.",
            )],
            sign_in: None,
            regions: vec![],
        },
        _ => Entry {
            group: Group::Local,
            blurb: "An OpenAI-compatible server: vLLM, llama.cpp, LiteLLM, TGI, a gateway…",
            links: vec![],
            fields: custom_fields(),
            sign_in: None,
            regions: vec![],
        },
    }
}

/// Fields for adding an OpenAI-compatible server.
pub fn custom_fields() -> Vec<FieldSpec> {
    vec![
        FieldSpec {
            field: Field::Name,
            label: "Name",
            placeholder: "GPU box",
            help: "Shown in the model picker.",
        },
        base_url(
            "http://localhost:8000/v1",
            "The server's OpenAI-compatible base URL (ends in /v1).",
        ),
        FieldSpec {
            field: Field::ApiKey,
            label: "API key",
            placeholder: "Leave empty if the server needs none",
            help: "Saved to ~/.config/crack/credentials.toml (readable only by you).",
        },
        models("Only needed if the server can't list its models."),
    ]
}

/// Chinese model clouds: one key per region, so each has region toggles that set
/// the base URL, and a key link per region.
#[cfg(feature = "china")]
fn china_entry(id: &str) -> Option<Entry> {
    let fields = |key_help: &'static str| {
        vec![
            api_key(key_help),
            base_url(
                "Set by the Region buttons above",
                "Pick a region above, or paste a workspace-specific URL.",
            ),
            models("Built-in list; edit it if your account has other models."),
        ]
    };
    Some(match id {
        "qwen" => Entry {
            group: Group::China,
            blurb: "Qwen models on Alibaba Cloud Model Studio (DashScope). Keys only work in \
                    the region that created them.",
            links: vec![
                (
                    "Create an API key",
                    "https://modelstudio.console.alibabacloud.com/ap-southeast-1/settings/api-key",
                ),
                (
                    "Models",
                    "https://www.alibabacloud.com/help/en/model-studio/models",
                ),
            ],
            fields: fields(
                "For a China (Beijing) key, switch the console's region to China (Beijing) first.",
            ),
            sign_in: None,
            regions: vec![
                (
                    "International (Singapore)",
                    "https://dashscope-intl.aliyuncs.com/compatible-mode/v1",
                ),
                (
                    "China (Beijing)",
                    "https://dashscope.aliyuncs.com/compatible-mode/v1",
                ),
            ],
        },
        "kimi" => Entry {
            group: Group::China,
            blurb: "Kimi models from Moonshot AI. The international (.ai) and China (.cn) \
                    platforms have separate accounts and keys.",
            links: vec![
                (
                    "API keys (international)",
                    "https://platform.kimi.ai/console/api-keys",
                ),
                (
                    "API keys (China)",
                    "https://platform.kimi.com/console/api-keys",
                ),
            ],
            fields: fields("A key from the platform matching the region above."),
            sign_in: None,
            regions: vec![
                ("International", "https://api.moonshot.ai/v1"),
                ("China", "https://api.moonshot.cn/v1"),
            ],
        },
        "zai" => Entry {
            group: Group::China,
            blurb: "GLM models from Z.ai (Zhipu AI). Use the Coding Plan endpoint if you \
                    subscribe to the GLM Coding Plan.",
            links: vec![
                ("API keys (Z.ai)", "https://z.ai/manage-apikey/apikey-list"),
                (
                    "API keys (BigModel, China)",
                    "https://bigmodel.cn/usercenter/proj-mgmt/apikeys",
                ),
            ],
            fields: fields("A Z.ai key for International / Coding Plan, a BigModel key for China."),
            sign_in: None,
            regions: vec![
                ("International", "https://api.z.ai/api/paas/v4"),
                ("China (BigModel)", "https://open.bigmodel.cn/api/paas/v4"),
                ("Coding Plan", "https://api.z.ai/api/coding/paas/v4"),
            ],
        },
        "minimax" => Entry {
            group: Group::China,
            blurb: "MiniMax M-series models. The international (.io) and China (.cn) \
                    platforms have separate keys.",
            links: vec![
                (
                    "API keys (international)",
                    "https://platform.minimax.io/user-center/basic-information/interface-key",
                ),
                (
                    "API keys (China)",
                    "https://platform.minimax.cn/user-center/basic-information/interface-key",
                ),
            ],
            fields: fields("A key from the platform matching the region above."),
            sign_in: None,
            regions: vec![
                ("International", "https://api.minimax.io/v1"),
                ("China", "https://api.minimax.cn/v1"),
            ],
        },
        "deepseek" => Entry {
            group: Group::China,
            blurb: "DeepSeek chat and reasoning models.",
            links: vec![(
                "Create an API key",
                "https://platform.deepseek.com/api_keys",
            )],
            fields: vec![api_key(
                "Saved to ~/.config/crack/credentials.toml (readable only by you).",
            )],
            sign_in: None,
            regions: vec![],
        },
        _ => return None,
    })
}
