//! Table of 80+ OpenAI-compatible services with per-provider quirk flags.
//!
//! Lookup table mapping service names to base URLs, auth styles, and quirk flags.

use crate::harness::types::{AuthStyle, ProviderQuirks};

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
};

static COMPATIBLE_TABLE: &[CompatibleEntry] = &[
    CompatibleEntry {
        name: "mercury",
        base_url: "https://api.inceptionlabs.ai/v1",
        env_vars: &["INCEPTION_API_KEY", "MERCURY_API_KEY"],
        quirks: ProviderQuirks {
            native_tools: true,
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_lookup_by_name() {
        let entry = lookup("groq").unwrap();
        assert_eq!(entry.name, "groq");
        assert!(entry.quirks.native_tools);
    }

    #[test]
    fn test_lookup_by_model_prefix() {
        // "deepseek-chat" should match "deepseek"
        let entry = lookup("deepseek-chat").unwrap();
        assert_eq!(entry.name, "deepseek");
        assert!(entry.quirks.strip_think_tags);
    }

    #[test]
    fn test_lookup_mercury_routes_to_inception() {
        let entry = lookup("mercury-2").unwrap();
        assert_eq!(entry.name, "mercury");
        assert_eq!(entry.base_url, "https://api.inceptionlabs.ai/v1");
        assert_eq!(entry.env_vars, &["INCEPTION_API_KEY", "MERCURY_API_KEY"]);
        assert!(entry.quirks.native_tools);
    }

    #[test]
    fn test_lookup_case_insensitive() {
        assert!(lookup("Groq").is_some());
        assert!(lookup("DEEPSEEK").is_some());
    }

    #[test]
    fn test_lookup_unknown_returns_none() {
        assert!(lookup("unknown-provider-xyz").is_none());
    }

    #[test]
    fn test_perplexity_no_streaming() {
        let entry = lookup("perplexity").unwrap();
        assert!(entry.quirks.disable_streaming);
    }

    #[test]
    fn test_local_providers_no_auth() {
        for name in &["lmstudio", "llamacpp", "vllm", "ollama"] {
            let entry = lookup(name).unwrap();
            assert!(
                matches!(entry.quirks.auth_style, AuthStyle::None),
                "{name} should use AuthStyle::None"
            );
        }
    }
}
