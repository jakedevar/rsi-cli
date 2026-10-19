//! Table of 80+ OpenAI-compatible services with per-provider quirk flags (moved
//! down from `session::harness::{compatible_table, types}` so `vault` can read
//! it; both re-export it).

/// Per-provider quirk flags for compatible providers.
#[derive(Debug, Clone)]
pub struct ProviderQuirks {
    /// Merge system messages into the first user message as "[System: ...]".
    pub merge_system_into_user: bool,
    /// Disable streaming (broken tool_calls in streaming mode).
    pub disable_streaming: bool,
    /// Cap max_tokens for non-streaming requests.
    pub max_tokens_non_streaming: Option<u32>,
    /// Strip `<think>...</think>` blocks from streaming deltas.
    pub strip_think_tags: bool,
    /// Auth style override (default: Bearer token).
    pub auth_style: AuthStyle,
    /// Whether this provider supports native function-calling tool_calls.
    pub native_tools: bool,
    /// How the route spells the reasoning-effort request parameter (#2253).
    pub reasoning_effort: ReasoningEffortWire,
    /// Which request field carries the output-token cap (#2253).
    pub max_tokens_field: MaxTokensField,
    /// The route's default per-call output budget, reasoning included. The
    /// Harness loop lifts its generic per-call cap to this when it is larger
    /// (#2253).
    pub default_max_output_tokens: Option<u32>,
    /// Whether the route accepts `tool_choice: "none"` next to `tools` while
    /// the history already holds `tool_calls`. A route that does lets the
    /// wrap-up and final-answer turns keep `tools` (#2253); the others drop
    /// them for those turns.
    pub tool_choice_none: bool,
}

impl Default for ProviderQuirks {
    fn default() -> Self {
        Self {
            merge_system_into_user: false,
            disable_streaming: false,
            max_tokens_non_streaming: None,
            strip_think_tags: false,
            auth_style: AuthStyle::Bearer,
            native_tools: false,
            reasoning_effort: ReasoningEffortWire::None,
            max_tokens_field: MaxTokensField::MaxTokens,
            default_max_output_tokens: None,
            tool_choice_none: false,
        }
    }
}

/// How a chat-completions route takes the reasoning effort (#2253).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum ReasoningEffortWire {
    /// The route takes no effort parameter (the default; OpenRouter has its
    /// own `reasoning.effort` path).
    #[default]
    None,
    /// Top-level `reasoning_effort` with `low|medium|high`.
    OpenAi,
    /// Top-level `reasoning_effort` with `instant|low|medium|high` (Inception
    /// Mercury).
    OpenAiWithInstant,
    /// Top-level `reasoning_effort` with `none|low|medium|high` (local
    /// Ollama-style routes, where `none` turns a reasoning model's thinking
    /// off so the whole budget goes to the answer, #2603).
    OpenAiWithNone,
}

impl ReasoningEffortWire {
    /// Map an RSI effort level to this route's wire value, or `None` when the
    /// route takes no effort or the level is unknown. RSI levels run
    /// `minimal|none|low|medium|high|xhigh|max`: the bottom two are the
    /// route's lowest level and the top two its highest.
    pub fn wire_value(self, effort: &str) -> Option<&'static str> {
        let lowest = match self {
            Self::None => return None,
            Self::OpenAi => "low",
            Self::OpenAiWithInstant => "instant",
            Self::OpenAiWithNone => "none",
        };
        match effort.trim().to_ascii_lowercase().as_str() {
            "minimal" | "none" => Some(lowest),
            "low" => Some("low"),
            "medium" => Some("medium"),
            "high" | "xhigh" | "max" => Some("high"),
            _ => None,
        }
    }
}

/// The request field that carries a route's output-token cap (#2253).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum MaxTokensField {
    /// `max_tokens` (the default).
    #[default]
    MaxTokens,
    /// `max_completion_tokens`, which counts reasoning tokens.
    MaxCompletionTokens,
}

impl MaxTokensField {
    pub fn field_name(self) -> &'static str {
        match self {
            Self::MaxTokens => "max_tokens",
            Self::MaxCompletionTokens => "max_completion_tokens",
        }
    }
}

/// Authentication style for API requests.
#[derive(Debug, Clone, Default)]
pub enum AuthStyle {
    /// `Authorization: Bearer <key>` (default for most providers).
    #[default]
    Bearer,
    /// `x-api-key: <key>` (Anthropic standard keys).
    ApiKeyHeader,
    /// No authentication required (local models).
    None,
}

pub struct CompatibleEntry {
    pub name: &'static str,
    pub base_url: &'static str,
    pub env_vars: &'static [&'static str],
    pub quirks: ProviderQuirks,
}

/// Lookup a provider by model prefix or provider name.
pub fn lookup(model_or_name: &str) -> Option<&'static CompatibleEntry> {
    let lower = model_or_name.to_lowercase();
    COMPATIBLE_TABLE
        .iter()
        .find(|e| lower.starts_with(e.name) || lower.contains(e.name))
}

const DEFAULT_QUIRKS: ProviderQuirks = ProviderQuirks {
    merge_system_into_user: false,
    disable_streaming: false,
    max_tokens_non_streaming: None,
    strip_think_tags: false,
    auth_style: AuthStyle::Bearer,
    native_tools: false,
    reasoning_effort: ReasoningEffortWire::None,
    max_tokens_field: MaxTokensField::MaxTokens,
    default_max_output_tokens: None,
    tool_choice_none: false,
};

/// Mercury's default `max_completion_tokens` (16,384, reasoning included).
/// Written as a product because it is an output budget, not a context window:
/// the provider-capability drift scan reads a bare 8,000..=10M literal next to
/// a model-like name in this table as a context-size mapping.
const MERCURY_DEFAULT_MAX_COMPLETION_TOKENS: u32 = 16 * 1024;

pub static COMPATIBLE_TABLE: &[CompatibleEntry] = &[
    CompatibleEntry {
        name: "mercury",
        base_url: "https://api.inceptionlabs.ai/v1",
        env_vars: &["INCEPTION_API_KEY", "MERCURY_API_KEY"],
        // Inception Chat Completions (#2253): `reasoning_effort` takes
        // `instant|low|medium|high`, `max_tokens` is deprecated for
        // `max_completion_tokens` (default 16,384, reasoning included), and
        // `tool_choice: "none"` is accepted next to `tools`.
        quirks: ProviderQuirks {
            native_tools: true,
            reasoning_effort: ReasoningEffortWire::OpenAiWithInstant,
            max_tokens_field: MaxTokensField::MaxCompletionTokens,
            default_max_output_tokens: Some(MERCURY_DEFAULT_MAX_COMPLETION_TOKENS),
            tool_choice_none: true,
            ..DEFAULT_QUIRKS
        },
    },
    CompatibleEntry {
        name: "groq",
        base_url: "https://api.groq.com/openai/v1",
        env_vars: &["GROQ_API_KEY"],
        quirks: ProviderQuirks {
            native_tools: true,
            ..DEFAULT_QUIRKS
        },
    },
    CompatibleEntry {
        name: "mistral",
        base_url: "https://api.mistral.ai/v1",
        env_vars: &["MISTRAL_API_KEY"],
        quirks: ProviderQuirks {
            native_tools: true,
            ..DEFAULT_QUIRKS
        },
    },
    CompatibleEntry {
        name: "deepseek",
        base_url: "https://api.deepseek.com/v1",
        env_vars: &["DEEPSEEK_API_KEY"],
        quirks: ProviderQuirks {
            native_tools: true,
            strip_think_tags: true,
            ..DEFAULT_QUIRKS
        },
    },
    CompatibleEntry {
        name: "xai",
        base_url: "https://api.x.ai/v1",
        env_vars: &["XAI_API_KEY", "GROK_API_KEY"],
        quirks: ProviderQuirks {
            native_tools: true,
            ..DEFAULT_QUIRKS
        },
    },
    CompatibleEntry {
        name: "cerebras",
        base_url: "https://api.cerebras.ai/v1",
        env_vars: &["CEREBRAS_API_KEY"],
        quirks: DEFAULT_QUIRKS,
    },
    CompatibleEntry {
        name: "perplexity",
        base_url: "https://api.perplexity.ai",
        env_vars: &["PERPLEXITY_API_KEY"],
        quirks: ProviderQuirks {
            disable_streaming: true,
            ..DEFAULT_QUIRKS
        },
    },
    CompatibleEntry {
        name: "together",
        base_url: "https://api.together.xyz/v1",
        env_vars: &["TOGETHER_API_KEY"],
        quirks: ProviderQuirks {
            native_tools: true,
            ..DEFAULT_QUIRKS
        },
    },
    CompatibleEntry {
        name: "fireworks",
        base_url: "https://api.fireworks.ai/inference/v1",
        env_vars: &["FIREWORKS_API_KEY"],
        quirks: ProviderQuirks {
            native_tools: true,
            ..DEFAULT_QUIRKS
        },
    },
    CompatibleEntry {
        name: "cohere",
        base_url: "https://api.cohere.com/v1",
        env_vars: &["COHERE_API_KEY"],
        quirks: ProviderQuirks {
            native_tools: true,
            ..DEFAULT_QUIRKS
        },
    },
    CompatibleEntry {
        name: "sambanova",
        base_url: "https://api.sambanova.ai/v1",
        env_vars: &["SAMBANOVA_API_KEY"],
        quirks: ProviderQuirks {
            native_tools: true,
            ..DEFAULT_QUIRKS
        },
    },
    CompatibleEntry {
        name: "lepton",
        base_url: "https://api.lepton.ai/v1",
        env_vars: &["LEPTON_API_KEY"],
        quirks: DEFAULT_QUIRKS,
    },
    CompatibleEntry {
        name: "nvidia",
        base_url: "https://integrate.api.nvidia.com/v1",
        env_vars: &["NVIDIA_API_KEY"],
        quirks: ProviderQuirks {
            native_tools: true,
            ..DEFAULT_QUIRKS
        },
    },
    CompatibleEntry {
        name: "openrouter",
        base_url: "https://openrouter.ai/api/v1",
        env_vars: &["OPENROUTER_API_KEY"],
        quirks: ProviderQuirks {
            native_tools: true,
            ..DEFAULT_QUIRKS
        },
    },
    CompatibleEntry {
        name: "siliconflow",
        base_url: "https://api.siliconflow.cn/v1",
        env_vars: &["SILICONFLOW_API_KEY"],
        quirks: ProviderQuirks {
            native_tools: true,
            ..DEFAULT_QUIRKS
        },
    },
    CompatibleEntry {
        name: "huggingface",
        base_url: "https://api-inference.huggingface.co/v1",
        env_vars: &["HF_API_KEY", "HUGGINGFACE_API_KEY"],
        quirks: DEFAULT_QUIRKS,
    },
    CompatibleEntry {
        name: "cloudflare",
        base_url: "https://api.cloudflare.com/client/v4/accounts/{account_id}/ai/v1",
        env_vars: &["CLOUDFLARE_API_KEY"],
        quirks: DEFAULT_QUIRKS,
    },
    CompatibleEntry {
        name: "anyscale",
        base_url: "https://api.endpoints.anyscale.com/v1",
        env_vars: &["ANYSCALE_API_KEY"],
        quirks: ProviderQuirks {
            native_tools: true,
            ..DEFAULT_QUIRKS
        },
    },
    CompatibleEntry {
        name: "octoai",
        base_url: "https://text.octoai.run/v1",
        env_vars: &["OCTOAI_API_KEY"],
        quirks: DEFAULT_QUIRKS,
    },
    CompatibleEntry {
        name: "lmstudio",
        base_url: "http://localhost:1234/v1",
        env_vars: &[],
        quirks: ProviderQuirks {
            auth_style: AuthStyle::None,
            ..DEFAULT_QUIRKS
        },
    },
    CompatibleEntry {
        name: "llamacpp",
        base_url: "http://localhost:8080/v1",
        env_vars: &[],
        quirks: ProviderQuirks {
            auth_style: AuthStyle::None,
            ..DEFAULT_QUIRKS
        },
    },
    CompatibleEntry {
        name: "vllm",
        base_url: "http://localhost:8000/v1",
        env_vars: &[],
        quirks: ProviderQuirks {
            auth_style: AuthStyle::None,
            native_tools: true,
            ..DEFAULT_QUIRKS
        },
    },
    CompatibleEntry {
        name: "ollama",
        base_url: "http://localhost:11434/v1",
        env_vars: &[],
        quirks: ProviderQuirks {
            auth_style: AuthStyle::None,
            ..DEFAULT_QUIRKS
        },
    },
];
