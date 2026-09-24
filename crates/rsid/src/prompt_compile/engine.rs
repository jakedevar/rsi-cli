//! Compile engine: streams `/api/generate` output through the bus, applies
//! post-processing, caches results, and supersedes prior in-flight requests
//! from the same caller.

use crate::bus::{DaemonEvent, EventBus};
use crate::config::RuntimeConfig;
use crate::model_control::{
    AdmissionDecision, ModelAdmissionRequest, admit_invocation, classify_error_class,
    completion_with_wall_time, hash_request_fingerprint,
};
use crate::ollama_client::{self, GenerateOptions};
use crate::prompt_compile::post_process::{
    extract_compile_error, parse_contract, strip_think_tags, validate_layers,
};
use crate::prompt_compile::system_prompt::SYSTEM_PROMPT;
use crate::store::Store;
use lru::LruCache;
use parking_lot::Mutex;
use rsi_common::model_control::ModelInvocationPurpose;
use rsi_common::model_control::{InvocationOwner, ModelUsageConfidence};
use rsi_common::prompt_compile::CompileResult;
use rsi_common::rpc::CompilePromptResponse;
use rsi_common::types::SessionProvider;
use std::collections::HashMap;
use std::num::NonZeroUsize;
use std::sync::Arc;
use std::time::Instant;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

/// Identity for supersede-cancellation. Currently the TUI-generated `caller_id`
/// from `CompilePromptParams`.
pub type CallerId = Uuid;

type CacheKey = (blake3::Hash, blake3::Hash, String);

#[derive(Debug, Clone)]
pub struct CompileTarget {
    pub provider: SessionProvider,
    pub model: String,
    pub base_url: Option<String>,
    pub api_key: Option<String>,
}

struct InFlight {
    request_id: Uuid,
    dedup_key: String,
    cancel: CancellationToken,
}

pub struct CompileEngine {
    http: reqwest::Client,
    store: Arc<tokio::sync::Mutex<Store>>,
    runtime_config: Arc<RuntimeConfig>,
    bus: Arc<EventBus>,
    cache: Mutex<LruCache<CacheKey, CompileResult>>,
    in_flight: Mutex<HashMap<CallerId, InFlight>>,
}

const DEFAULT_CACHE_CAPACITY: usize = 128;

impl CompileEngine {
    pub fn new(
        http: reqwest::Client,
        store: Arc<tokio::sync::Mutex<Store>>,
        runtime_config: Arc<RuntimeConfig>,
        bus: Arc<EventBus>,
    ) -> Arc<Self> {
        let cap = NonZeroUsize::new(DEFAULT_CACHE_CAPACITY).expect("nonzero");
        Arc::new(Self {
            http,
            store,
            runtime_config,
            bus,
            cache: Mutex::new(LruCache::new(cap)),
            in_flight: Mutex::new(HashMap::new()),
        })
    }

    fn cache_key(system: &str, input: &str, target: &CompileTarget) -> CacheKey {
        (
            blake3::hash(system.as_bytes()),
            blake3::hash(input.as_bytes()),
            format!(
                "{:?}:{}:{}",
                target.provider,
                target.model,
                target.base_url.as_deref().unwrap_or("")
            ),
        )
    }

    fn resolve_target(
        &self,
        model_override: Option<String>,
        provider_override: Option<SessionProvider>,
        base_url: Option<String>,
        api_key: Option<String>,
    ) -> CompileTarget {
        CompileTarget {
            provider: provider_override
                .unwrap_or_else(|| *self.runtime_config.prompt_compile_model_provider.read()),
            model: model_override.unwrap_or_else(|| {
                self.runtime_config
                    .prompt_compile_model_local
                    .read()
                    .clone()
            }),
            base_url: base_url.or_else(|| {
                self.runtime_config
                    .prompt_compile_model_base_url
                    .read()
                    .clone()
            }),
            api_key: api_key.or_else(|| {
                self.runtime_config
                    .prompt_compile_model_api_key
                    .read()
                    .clone()
            }),
        }
    }

    /// Test helper: expose runtime config so integration tests can look up
    /// the default model without reaching into private state.
    #[doc(hidden)]
    pub fn runtime_config_clone_for_tests(&self) -> Arc<RuntimeConfig> {
        Arc::clone(&self.runtime_config)
    }

    /// Test helper: seed the cache for a given `(input, model)` pair.
    #[doc(hidden)]
    pub fn seed_cache_for_tests(&self, input: &str, model: &str, result: CompileResult) {
        let target = CompileTarget {
            provider: SessionProvider::Local,
            model: model.to_string(),
            base_url: None,
            api_key: None,
        };
        let key = Self::cache_key(SYSTEM_PROMPT, input, &target);
        self.cache.lock().put(key, result);
    }

    /// Compile `input` for `caller_id`. Returns immediately with either a
    /// cache hit (`cached: Some`) or a spawned-task receipt (`cached: None`)
    /// whose progress and completion arrive on the bus.
    pub async fn compile(
        self: Arc<Self>,
        caller_id: CallerId,
        input: String,
        model_override: Option<String>,
        provider_override: Option<SessionProvider>,
        base_url: Option<String>,
        api_key: Option<String>,
    ) -> CompilePromptResponse {
        let request_id = Uuid::new_v4();
        let target = self.resolve_target(model_override, provider_override, base_url, api_key);
        let dedup_key = crate::model_control::stable_dedup_key(
            "compile-prompt",
            &[
                &caller_id.to_string(),
                &format!("{:?}", target.provider),
                &target.model,
                target.base_url.as_deref().unwrap_or(""),
                input.as_str(),
            ],
        );

        // Cache lookup.
        let key = Self::cache_key(SYSTEM_PROMPT, &input, &target);
        if let Some(hit) = self.cache.lock().get(&key).cloned() {
            // Emit synthetic stream so subscribers see a consistent shape.
            self.bus.publish(DaemonEvent::CompilePromptChunk {
                request_id,
                delta: hit.compiled.clone(),
            });
            self.bus.publish(DaemonEvent::CompilePromptCompleted {
                request_id,
                result: hit.clone(),
            });
            return CompilePromptResponse {
                request_id,
                cached: Some(hit),
            };
        }

        // Reuse the current in-flight request when the same caller replays the
        // same compile payload. Different payloads still supersede.
        let prior = {
            let mut in_flight = self.in_flight.lock();
            if let Some(existing) = in_flight.get(&caller_id)
                && existing.dedup_key == dedup_key
            {
                return CompilePromptResponse {
                    request_id: existing.request_id,
                    cached: None,
                };
            }
            in_flight.remove(&caller_id)
        };
        if let Some(prior) = prior {
            prior.cancel.cancel();
            self.bus.publish(DaemonEvent::CompilePromptFailed {
                request_id: prior.request_id,
                error: "superseded".to_string(),
            });
        }

        let cancel = CancellationToken::new();
        self.in_flight.lock().insert(
            caller_id,
            InFlight {
                request_id,
                dedup_key: dedup_key.clone(),
                cancel: cancel.clone(),
            },
        );

        let this = Arc::clone(&self);
        let cancel_spawn = cancel.clone();
        tokio::spawn(async move {
            let outcome = this
                .run_compile(request_id, &input, &target, &dedup_key, cancel_spawn)
                .await;

            match outcome {
                Ok(result) => {
                    this.cache.lock().put(key, result.clone());
                    this.bus
                        .publish(DaemonEvent::CompilePromptCompleted { request_id, result });
                }
                Err(err) => {
                    // "superseded" is published by whichever code cancels; if we
                    // were the cancellee (cancel fired during our task), skip
                    // publishing a duplicate failed event here.
                    if !matches!(err, CompileFailure::Superseded) {
                        this.bus.publish(DaemonEvent::CompilePromptFailed {
                            request_id,
                            error: err.to_string(),
                        });
                    }
                }
            }

            // Clean up in_flight only if we still own it (we may have been
            // replaced by a later supersede already).
            let mut flight = this.in_flight.lock();
            if let Some(existing) = flight.get(&caller_id)
                && existing.request_id == request_id
            {
                flight.remove(&caller_id);
            }
        });

        CompilePromptResponse {
            request_id,
            cached: None,
        }
    }

    async fn run_compile(
        self: &Arc<Self>,
        request_id: Uuid,
        input: &str,
        target: &CompileTarget,
        dedup_key: &str,
        cancel: CancellationToken,
    ) -> Result<CompileResult, CompileFailure> {
        // Apply qwen `/no_think` suffix if relevant.
        let system = if target.model.to_ascii_lowercase().contains("qwen") {
            format!("{SYSTEM_PROMPT}\n/no_think")
        } else {
            SYSTEM_PROMPT.to_string()
        };
        let llm_target = crate::memory::llm::MemoryLlmTarget {
            provider: target.provider,
            model: target.model.clone(),
            base_url: target.base_url.clone(),
            api_key: target.api_key.clone(),
        };
        let prompt = format!("{system}\n\n{input}");
        let provider_label = crate::memory::llm::provider_label(&llm_target)
            .map_err(|e| CompileFailure::Other(e.to_string()))?;
        let backend_label = crate::memory::llm::backend_label(&llm_target)
            .map_err(|e| CompileFailure::Other(e.to_string()))?
            .to_string();
        let admission_request = ModelAdmissionRequest {
            purpose: ModelInvocationPurpose::PromptCompile,
            provider: Some(provider_label.clone()),
            model: Some(llm_target.model.clone()),
            backend: Some(backend_label.clone()),
            effort: None,
            trigger: "CompilePrompt".to_string(),
            owner: InvocationOwner {
                operator: Some(format!("compile_prompt:{request_id}")),
                ..InvocationOwner::default()
            },
            dedup_key: Some(dedup_key.to_string()),
            request_fingerprint: Some(hash_request_fingerprint(&[
                &target.model,
                input,
                &format!("{:?}", target.provider),
                target.base_url.as_deref().unwrap_or(""),
            ])),
            parent_invocation_id: None,
            retry_of_invocation_id: None,
            expected_usage: Some(crate::model_control::explicit_expected_usage(
                ModelInvocationPurpose::PromptCompile,
                Some(provider_label.as_str()),
                Some(backend_label.as_str()),
                Some(llm_target.model.as_str()),
            )),
            baseline_input_tokens: 0,
            baseline_output_tokens: 0,
            baseline_cache_creation_tokens: 0,
            baseline_cache_read_tokens: 0,
            baseline_reasoning_tokens: 0,
            baseline_embedding_input_count: 0,
            baseline_wall_time_ms: 0,
        };
        let permit = match admit_invocation(&self.store, admission_request, &self.bus)
            .await
            .map_err(|e| CompileFailure::Other(e.to_string()))?
        {
            AdmissionDecision::Admitted(permit) => permit,
            AdmissionDecision::Duplicate { invocation_id } => {
                return Err(CompileFailure::Other(format!(
                    "compile prompt already in progress or settled ({invocation_id})"
                )));
            }
        };
        let started_at = Instant::now();

        let opts = GenerateOptions {
            num_predict: None,
            temperature: 0.3,
            think: false,
            keep_alive: Some("60m".to_string()),
        };

        let bus = Arc::clone(&self.bus);
        let request_id_for_cb = request_id;

        let raw_result = if crate::memory::llm::uses_native_ollama(&llm_target)
            .map_err(|e| CompileFailure::Other(e.to_string()))?
        {
            ollama_client::generate_stream(
                &self.http,
                &target.model,
                Some(&system),
                input,
                opts,
                move |delta| {
                    bus.publish(DaemonEvent::CompilePromptChunk {
                        request_id: request_id_for_cb,
                        delta: delta.to_string(),
                    });
                },
                cancel.clone(),
            )
            .await
            .map_err(|e| match e {
                ollama_client::OllamaError::Cancelled => CompileFailure::Superseded,
                other => CompileFailure::Other(other.to_string()),
            })
        } else {
            crate::memory::llm::generate_text(
                &permit,
                &llm_target,
                &prompt,
                4096,
                "Prompt compilation",
            )
            .await
            .map_err(|e| CompileFailure::Other(e.to_string()))
            .map(|raw| {
                bus.publish(DaemonEvent::CompilePromptChunk {
                    request_id,
                    delta: raw.clone(),
                });
                raw
            })
        };
        let final_result = match raw_result {
            Ok(raw) => {
                let cleaned = strip_think_tags(&raw);
                let cleaned = cleaned.trim();
                if cleaned.is_empty() {
                    Err(CompileFailure::Other("empty response".to_string()))
                } else if let Some(desc) = extract_compile_error(cleaned) {
                    Err(CompileFailure::Other(format!("Ambiguous intent: {desc}")))
                } else {
                    let (body_text, contract) = parse_contract(cleaned);
                    let layer_validation = validate_layers(&body_text);
                    Ok(CompileResult {
                        compiled: body_text,
                        contract,
                        layer_validation,
                    })
                }
            }
            Err(error) => Err(error),
        };

        let completion = match &final_result {
            Ok(_) => completion_with_wall_time(started_at, None, ModelUsageConfidence::Partial),
            Err(CompileFailure::Superseded) => completion_with_wall_time(
                started_at,
                Some("superseded".to_string()),
                ModelUsageConfidence::Partial,
            ),
            Err(CompileFailure::Other(message)) => completion_with_wall_time(
                started_at,
                Some(classify_error_class(&crate::error::DaemonError::Process(
                    message.clone(),
                ))),
                ModelUsageConfidence::Partial,
            ),
        };
        if let Err(e) =
            crate::model_control::complete_invocation(&self.store, &permit, completion, &self.bus)
                .await
        {
            return Err(CompileFailure::Other(format!(
                "failed to settle prompt compilation invocation: {e}"
            )));
        }

        final_result
    }
}

#[derive(Debug)]
enum CompileFailure {
    Superseded,
    Other(String),
}

impl std::fmt::Display for CompileFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CompileFailure::Superseded => write!(f, "superseded"),
            CompileFailure::Other(s) => write!(f, "{s}"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rsi_common::prompt_compile::{LayerValidation, OutputContract};

    fn make_engine() -> Arc<CompileEngine> {
        let cfg = flywheeld_test_runtime_config();
        let bus = Arc::new(EventBus::new(256));
        let store = Arc::new(tokio::sync::Mutex::new(
            crate::store::Store::open_in_memory().expect("store"),
        ));
        CompileEngine::new(reqwest::Client::new(), store, cfg, bus)
    }

    fn flywheeld_test_runtime_config() -> Arc<RuntimeConfig> {
        // RuntimeConfig::from_config needs a Config — build a minimal one.
        // Use defaults from the actual Config initializer.
        let config = crate::config::Config::from_env();
        RuntimeConfig::from_config(&config)
    }

    #[test]
    fn cache_hit_roundtrip() {
        let engine = make_engine();
        let target = CompileTarget {
            provider: SessionProvider::Local,
            model: "qwen3:14b".to_string(),
            base_url: None,
            api_key: None,
        };
        let key = CompileEngine::cache_key("sys", "input", &target);
        let result = CompileResult {
            compiled: "Compiled output.".to_string(),
            contract: OutputContract::Complete,
            layer_validation: LayerValidation {
                semantic: true,
                syntactic: true,
                deictic: true,
                discourse: true,
                pragmatic: true,
            },
        };
        engine.cache.lock().put(key.clone(), result.clone());
        let fetched = engine.cache.lock().get(&key).cloned();
        assert!(fetched.is_some());
        let f = fetched.unwrap();
        assert_eq!(f.compiled, "Compiled output.");
        assert_eq!(f.contract, OutputContract::Complete);
    }

    #[test]
    fn cache_evicts_beyond_capacity() {
        let bus = Arc::new(EventBus::new(16));
        let cfg = flywheeld_test_runtime_config();
        let engine = Arc::new(CompileEngine {
            http: reqwest::Client::new(),
            store: Arc::new(tokio::sync::Mutex::new(
                crate::store::Store::open_in_memory().expect("store"),
            )),
            runtime_config: cfg,
            bus,
            cache: Mutex::new(LruCache::new(NonZeroUsize::new(2).unwrap())),
            in_flight: Mutex::new(HashMap::new()),
        });
        let r = CompileResult {
            compiled: "x".into(),
            contract: OutputContract::Complete,
            layer_validation: LayerValidation {
                semantic: true,
                syntactic: true,
                deictic: true,
                discourse: true,
                pragmatic: true,
            },
        };
        let target = CompileTarget {
            provider: SessionProvider::Local,
            model: "m".to_string(),
            base_url: None,
            api_key: None,
        };
        let k1 = CompileEngine::cache_key("a", "1", &target);
        let k2 = CompileEngine::cache_key("a", "2", &target);
        let k3 = CompileEngine::cache_key("a", "3", &target);
        engine.cache.lock().put(k1.clone(), r.clone());
        engine.cache.lock().put(k2.clone(), r.clone());
        engine.cache.lock().put(k3, r);
        assert!(engine.cache.lock().get(&k1).is_none());
        assert!(engine.cache.lock().get(&k2).is_some());
    }

    #[tokio::test]
    async fn cache_hit_emits_synthetic_events() {
        let engine = make_engine();
        let caller = Uuid::new_v4();
        let input = "TEST-input-cache".to_string();
        let model = engine
            .runtime_config
            .prompt_compile_model_local
            .read()
            .clone();

        // Seed the cache manually.
        let target = CompileTarget {
            provider: SessionProvider::Local,
            model,
            base_url: None,
            api_key: None,
        };
        let key = CompileEngine::cache_key(SYSTEM_PROMPT, &input, &target);
        let result = CompileResult {
            compiled: "Cached compiled.".to_string(),
            contract: OutputContract::Complete,
            layer_validation: LayerValidation {
                semantic: true,
                syntactic: true,
                deictic: true,
                discourse: true,
                pragmatic: true,
            },
        };
        engine.cache.lock().put(key, result.clone());

        let mut rx = engine.bus.subscribe();
        let resp = Arc::clone(&engine)
            .compile(caller, input, None, None, None, None)
            .await;
        assert!(resp.cached.is_some());

        // Should have received chunk + completed.
        let first = rx.try_recv().unwrap();
        let second = rx.try_recv().unwrap();
        matches!(*first, DaemonEvent::CompilePromptChunk { .. });
        matches!(*second, DaemonEvent::CompilePromptCompleted { .. });
    }
}
