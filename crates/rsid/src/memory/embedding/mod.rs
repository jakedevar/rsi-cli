pub mod batch;
pub mod cache;
pub mod ollama;
pub mod openai_compat;

use crate::memory::types::{EmbeddingProvider, MemoryConfig};

/// Result of attempting to create an embedding provider.
/// Wraps `Option<Box<dyn EmbeddingProvider>>` with metadata about the selection process.
///
/// When `provider` is `None`, the memory system operates in FTS-only mode
/// (keyword search only, no vector similarity).
pub struct EmbeddingProviderResult {
    /// The resolved provider, or None for FTS-only mode.
    pub provider: Option<Box<dyn EmbeddingProvider>>,
    /// What was requested ("auto", "ollama", "openai", "none").
    pub requested: String,
    /// Label used in model-control attribution.
    pub provider_label: String,
    /// Backend label used in model-control attribution.
    pub backend: String,
    /// Resolved base URL when applicable.
    pub base_url: Option<String>,
    /// If the resolved provider differs from the requested one, this explains why.
    pub fallback_reason: Option<String>,
    /// If no provider could be created, this explains why.
    pub unavailable_reason: Option<String>,
}

impl EmbeddingProviderResult {
    /// Returns true if a provider is available (not FTS-only mode).
    pub fn is_available(&self) -> bool {
        self.provider.is_some()
    }

    /// Returns the provider ID string, or "none".
    pub fn provider_id(&self) -> &str {
        self.provider.as_ref().map_or("none", |p| p.id())
    }

    /// Returns the model name, or "none".
    pub fn model_name(&self) -> &str {
        self.provider.as_ref().map_or("none", |p| p.model())
    }
}

impl std::fmt::Debug for EmbeddingProviderResult {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EmbeddingProviderResult")
            .field("available", &self.is_available())
            .field("requested", &self.requested)
            .field("provider_label", &self.provider_label)
            .field("backend", &self.backend)
            .field("base_url", &self.base_url)
            .field("fallback_reason", &self.fallback_reason)
            .field("unavailable_reason", &self.unavailable_reason)
            .finish()
    }
}

/// Normalize a vector to unit length (L2 normalization).
///
/// Replaces NaN/Inf values with 0.0 before computing magnitude.
/// If the magnitude is effectively zero (< 1e-10), the vector is left as-is.
pub fn l2_normalize(vec: &mut [f32]) {
    // Replace NaN/Inf with 0.0
    for v in vec.iter_mut() {
        if v.is_nan() || v.is_infinite() {
            *v = 0.0;
        }
    }

    let magnitude: f32 = vec.iter().map(|v| v * v).sum::<f32>().sqrt();
    if magnitude < 1e-10 {
        return;
    }

    for v in vec.iter_mut() {
        *v /= magnitude;
    }
}

/// Parse a JSON string of f32 values into a Vec<f32>.
/// Returns None on empty string or invalid JSON.
pub fn parse_embedding_json(json: &str) -> Option<Vec<f32>> {
    if json.is_empty() {
        return None;
    }
    serde_json::from_str::<Vec<f32>>(json).ok()
}

/// Probe whether Ollama is running and accessible.
async fn probe_ollama(http: &reqwest::Client, base_url: &str) -> std::result::Result<(), String> {
    let url = format!("{}/api/tags", base_url);
    match tokio::time::timeout(std::time::Duration::from_secs(2), http.get(&url).send()).await {
        Ok(Ok(resp)) if resp.status().is_success() => Ok(()),
        Ok(Ok(resp)) => Err(format!("Ollama returned status {}", resp.status())),
        Ok(Err(e)) => Err(format!("Ollama connection failed: {}", e)),
        Err(_) => Err("Ollama probe timed out after 2s".to_string()),
    }
}

fn embedding_attribution(
    base_url: &str,
    local_backend: &str,
    remote_backend: &str,
) -> (String, String) {
    let loopback = reqwest::Url::parse(base_url)
        .ok()
        .and_then(|url| url.host_str().map(ToOwned::to_owned))
        .is_some_and(|host| {
            host.eq_ignore_ascii_case("localhost")
                || host
                    .parse::<std::net::IpAddr>()
                    .is_ok_and(|address| address.is_loopback())
        });
    if loopback {
        ("Local".to_string(), local_backend.to_string())
    } else {
        ("Remote".to_string(), remote_backend.to_string())
    }
}

/// Create an embedding provider based on configuration.
///
/// Auto mode: tries Ollama first (local), then falls back to OpenAI-compatible
/// if an API key is configured. Returns None if nothing is available.
pub async fn create_embedding_provider(
    config: &MemoryConfig,
    http: &reqwest::Client,
) -> EmbeddingProviderResult {
    let requested = config.embedding_provider.to_lowercase();

    match requested.as_str() {
        "none" => EmbeddingProviderResult {
            provider: None,
            requested,
            provider_label: "none".to_string(),
            backend: "none".to_string(),
            base_url: None,
            fallback_reason: None,
            unavailable_reason: None,
        },

        "auto" => {
            let base_url = config
                .embedding_url
                .as_deref()
                .unwrap_or("http://localhost:11434");

            match probe_ollama(http, base_url).await {
                Ok(()) => {
                    let (provider_label, backend) =
                        embedding_attribution(base_url, "ollama", "ollama_remote");
                    EmbeddingProviderResult {
                        provider: Some(Box::new(ollama::OllamaEmbeddingProvider::new(
                            http.clone(),
                            base_url.to_string(),
                            config.embedding_model.clone(),
                        ))),
                        requested,
                        provider_label,
                        backend,
                        base_url: Some(base_url.to_string()),
                        fallback_reason: None,
                        unavailable_reason: None,
                    }
                }
                Err(ollama_reason) => {
                    if let Some(ref api_key) = config.embedding_api_key {
                        let openai_url = config
                            .embedding_url
                            .as_deref()
                            .unwrap_or("https://api.openai.com/v1");
                        let (provider_label, backend) = embedding_attribution(
                            openai_url,
                            "openai_compatible_local",
                            "openai_compatible_api",
                        );
                        EmbeddingProviderResult {
                            provider: Some(Box::new(
                                openai_compat::OpenAiCompatEmbeddingProvider::new(
                                    http.clone(),
                                    openai_url.to_string(),
                                    api_key.clone(),
                                    config.embedding_model.clone(),
                                ),
                            )),
                            requested,
                            provider_label,
                            backend,
                            base_url: Some(openai_url.to_string()),
                            fallback_reason: Some(format!(
                                "Ollama unavailable ({}), fell back to OpenAI-compatible API",
                                ollama_reason
                            )),
                            unavailable_reason: None,
                        }
                    } else {
                        EmbeddingProviderResult {
                            provider: None,
                            requested,
                            provider_label: "none".to_string(),
                            backend: "none".to_string(),
                            base_url: None,
                            fallback_reason: None,
                            unavailable_reason: Some(format!(
                                "Ollama: {}; no embedding_api_key configured for fallback",
                                ollama_reason
                            )),
                        }
                    }
                }
            }
        }

        "ollama" => {
            let base_url = config
                .embedding_url
                .as_deref()
                .unwrap_or("http://localhost:11434");

            match probe_ollama(http, base_url).await {
                Ok(()) => {
                    let (provider_label, backend) =
                        embedding_attribution(base_url, "ollama", "ollama_remote");
                    EmbeddingProviderResult {
                        provider: Some(Box::new(ollama::OllamaEmbeddingProvider::new(
                            http.clone(),
                            base_url.to_string(),
                            config.embedding_model.clone(),
                        ))),
                        requested,
                        provider_label,
                        backend,
                        base_url: Some(base_url.to_string()),
                        fallback_reason: None,
                        unavailable_reason: None,
                    }
                }
                Err(reason) => EmbeddingProviderResult {
                    provider: None,
                    requested,
                    provider_label: "none".to_string(),
                    backend: "none".to_string(),
                    base_url: None,
                    fallback_reason: None,
                    unavailable_reason: Some(reason),
                },
            }
        }

        "openai" => {
            if let Some(ref api_key) = config.embedding_api_key {
                let base_url = config
                    .embedding_url
                    .as_deref()
                    .unwrap_or("https://api.openai.com/v1");
                let (provider_label, backend) = embedding_attribution(
                    base_url,
                    "openai_compatible_local",
                    "openai_compatible_api",
                );
                EmbeddingProviderResult {
                    provider: Some(Box::new(openai_compat::OpenAiCompatEmbeddingProvider::new(
                        http.clone(),
                        base_url.to_string(),
                        api_key.clone(),
                        config.embedding_model.clone(),
                    ))),
                    requested,
                    provider_label,
                    backend,
                    base_url: Some(base_url.to_string()),
                    fallback_reason: None,
                    unavailable_reason: None,
                }
            } else {
                EmbeddingProviderResult {
                    provider: None,
                    requested,
                    provider_label: "none".to_string(),
                    backend: "none".to_string(),
                    base_url: None,
                    fallback_reason: None,
                    unavailable_reason: Some(
                        "OpenAI provider requires embedding_api_key".to_string(),
                    ),
                }
            }
        }

        _ => EmbeddingProviderResult {
            provider: None,
            requested: requested.clone(),
            provider_label: "none".to_string(),
            backend: "none".to_string(),
            base_url: None,
            fallback_reason: None,
            unavailable_reason: Some(format!("Unknown embedding provider: {}", requested)),
        },
    }
}

// ---------------------------------------------------------------------------
// CI gating for live service tests
// ---------------------------------------------------------------------------

/// Returns true when live Ollama tests should run.
/// Set `RSI_TEST_OLLAMA=1` to enable (legacy `MOTHERSHIP_TEST_OLLAMA` /
/// `FLYWHEEL_TEST_OLLAMA` still honored).
#[doc(hidden)]
pub fn ollama_available() -> bool {
    rsi_common::identity::env_with_legacy(
        "RSI_TEST_OLLAMA",
        &["MOTHERSHIP_TEST_OLLAMA", "FLYWHEEL_TEST_OLLAMA"],
    )
    .map(|v| v == "1")
    .unwrap_or(false)
}

// ---------------------------------------------------------------------------
// Mock provider for tests
// ---------------------------------------------------------------------------

/// Mock embedding provider for testing. Available for integration tests.
#[doc(hidden)]
pub mod mock {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicU32, Ordering};

    use crate::error::{DaemonError, Result};
    use crate::memory::types::EmbeddingProvider;

    pub struct MockEmbeddingProvider {
        pub id: String,
        pub model: String,
        pub dims: usize,
        pub fail_next: Mutex<Option<String>>,
        pub batch_call_count: AtomicU32,
    }

    impl MockEmbeddingProvider {
        pub fn new(dims: usize) -> Self {
            Self {
                id: "mock".to_string(),
                model: "mock-embed".to_string(),
                dims,
                fail_next: Mutex::new(None),
                batch_call_count: AtomicU32::new(0),
            }
        }

        pub fn set_fail_next(&self, msg: &str) {
            *self.fail_next.lock().unwrap() = Some(msg.to_string());
        }

        fn deterministic_embedding(&self, text: &str) -> Vec<f32> {
            let mut hasher = DefaultHasher::new();
            text.hash(&mut hasher);
            let mut seed = hasher.finish();

            let mut vec = Vec::with_capacity(self.dims);
            for _ in 0..self.dims {
                // LCG: seed = seed * 6364136223846793005 + 1442695040888963407
                seed = seed
                    .wrapping_mul(6364136223846793005)
                    .wrapping_add(1442695040888963407);
                vec.push((seed as f32) / (u64::MAX as f32));
            }

            // L2 normalize
            let mag: f32 = vec.iter().map(|v| v * v).sum::<f32>().sqrt();
            if mag > 1e-10 {
                for v in &mut vec {
                    *v /= mag;
                }
            }

            vec
        }
    }

    #[async_trait::async_trait]
    impl EmbeddingProvider for MockEmbeddingProvider {
        fn id(&self) -> &str {
            &self.id
        }

        fn model(&self) -> &str {
            &self.model
        }

        fn max_input_tokens(&self) -> Option<u32> {
            None
        }

        async fn embed_query(
            &self,
            text: &str,
            _execution: crate::model_control::AdmittedEmbeddingExecution,
        ) -> Result<Vec<f32>> {
            if let Some(msg) = self.fail_next.lock().unwrap().take() {
                return Err(DaemonError::Process(msg));
            }
            Ok(self.deterministic_embedding(text))
        }

        async fn embed_batch(
            &self,
            texts: &[String],
            _execution: crate::model_control::AdmittedEmbeddingExecution,
        ) -> Result<Vec<Vec<f32>>> {
            self.batch_call_count.fetch_add(1, Ordering::Relaxed);
            if let Some(msg) = self.fail_next.lock().unwrap().take() {
                return Err(DaemonError::Process(msg));
            }
            Ok(texts
                .iter()
                .map(|t| self.deterministic_embedding(t))
                .collect())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bus::EventBus;
    use crate::error::DaemonError;
    use crate::memory::embedding::batch::{EmbeddingControl, embed_query_with_timeout};
    use crate::store::Store;
    use rsi_common::model_control::{InvocationOwner, ModelControlMode};
    use std::sync::Arc;
    use tokio::sync::Mutex;

    // --- l2_normalize tests ---

    #[test]
    fn test_l2_normalize_unit_vector() {
        let mut v = vec![1.0, 0.0, 0.0];
        l2_normalize(&mut v);
        assert!((v[0] - 1.0).abs() < 1e-6);
        assert!(v[1].abs() < 1e-6);
        assert!(v[2].abs() < 1e-6);
    }

    #[test]
    fn test_l2_normalize_unnormalized() {
        let mut v = vec![3.0, 4.0];
        l2_normalize(&mut v);
        let mag: f32 = v.iter().map(|x| x * x).sum::<f32>().sqrt();
        assert!((mag - 1.0).abs() < 1e-6);
        assert!((v[0] - 0.6).abs() < 1e-6);
        assert!((v[1] - 0.8).abs() < 1e-6);
    }

    #[test]
    fn test_l2_normalize_zero_vector() {
        let mut v = vec![0.0, 0.0, 0.0];
        l2_normalize(&mut v);
        assert_eq!(v, vec![0.0, 0.0, 0.0]);
    }

    #[test]
    fn test_l2_normalize_near_zero() {
        let mut v = vec![1e-12, 1e-12];
        l2_normalize(&mut v);
        // Should be no-op since magnitude < 1e-10
        assert!(v[0].abs() < 1e-10);
    }

    #[test]
    fn test_l2_normalize_nan() {
        let mut v = vec![f32::NAN, 3.0, 4.0];
        l2_normalize(&mut v);
        // NaN replaced with 0.0, then normalized [0, 3, 4] -> [0, 0.6, 0.8]
        assert!(v[0].abs() < 1e-6);
        assert!((v[1] - 0.6).abs() < 1e-6);
        assert!((v[2] - 0.8).abs() < 1e-6);
    }

    #[test]
    fn test_l2_normalize_inf() {
        let mut v = vec![f32::INFINITY, 3.0, 4.0];
        l2_normalize(&mut v);
        assert!(v[0].abs() < 1e-6);
        assert!((v[1] - 0.6).abs() < 1e-6);
    }

    #[test]
    fn test_l2_normalize_single_element() {
        let mut v = vec![5.0];
        l2_normalize(&mut v);
        assert!((v[0] - 1.0).abs() < 1e-6);
    }

    #[test]
    fn test_l2_normalize_negative() {
        let mut v = vec![-3.0, -4.0];
        l2_normalize(&mut v);
        let mag: f32 = v.iter().map(|x| x * x).sum::<f32>().sqrt();
        assert!((mag - 1.0).abs() < 1e-6);
    }

    #[test]
    fn test_l2_normalize_empty() {
        let mut v: Vec<f32> = vec![];
        l2_normalize(&mut v);
        assert!(v.is_empty());
    }

    #[test]
    fn test_l2_normalize_magnitude_check() {
        let mut v = vec![1.0, 2.0, 3.0, 4.0, 5.0];
        l2_normalize(&mut v);
        let mag: f32 = v.iter().map(|x| x * x).sum::<f32>().sqrt();
        assert!((mag - 1.0).abs() < 1e-5);
    }

    // --- parse_embedding_json tests ---

    #[test]
    fn test_parse_embedding_json_valid() {
        let v = parse_embedding_json("[1.0, 2.0, 3.0]").unwrap();
        assert_eq!(v, vec![1.0, 2.0, 3.0]);
    }

    #[test]
    fn test_parse_embedding_json_empty_string() {
        assert!(parse_embedding_json("").is_none());
    }

    #[test]
    fn test_parse_embedding_json_invalid() {
        assert!(parse_embedding_json("not json").is_none());
    }

    #[test]
    fn test_parse_embedding_json_empty_array() {
        let v = parse_embedding_json("[]").unwrap();
        assert!(v.is_empty());
    }

    // --- EmbeddingProviderResult tests ---

    #[test]
    fn test_result_none_provider() {
        let r = EmbeddingProviderResult {
            provider: None,
            requested: "none".to_string(),
            provider_label: "none".to_string(),
            backend: "none".to_string(),
            base_url: None,
            fallback_reason: None,
            unavailable_reason: None,
        };
        assert!(!r.is_available());
        assert_eq!(r.provider_id(), "none");
        assert_eq!(r.model_name(), "none");
    }

    // --- Factory tests ---

    #[tokio::test]
    async fn test_factory_none_mode() {
        let config = MemoryConfig {
            embedding_provider: "none".to_string(),
            ..MemoryConfig::default()
        };
        let http = reqwest::Client::new();
        let result = create_embedding_provider(&config, &http).await;
        assert!(!result.is_available());
        assert!(result.unavailable_reason.is_none());
    }

    #[tokio::test]
    async fn test_factory_auto_no_services() {
        let config = MemoryConfig {
            embedding_provider: "auto".to_string(),
            embedding_url: Some("http://127.0.0.1:1".to_string()), // guaranteed-unreachable port
            embedding_api_key: None,
            ..MemoryConfig::default()
        };
        let http = reqwest::Client::new();
        let result = create_embedding_provider(&config, &http).await;
        assert!(!result.is_available());
        assert!(result.unavailable_reason.is_some());
    }

    #[tokio::test]
    async fn test_factory_unknown_provider() {
        let config = MemoryConfig {
            embedding_provider: "foobar".to_string(),
            ..MemoryConfig::default()
        };
        let http = reqwest::Client::new();
        let result = create_embedding_provider(&config, &http).await;
        assert!(!result.is_available());
        assert!(
            result
                .unavailable_reason
                .as_ref()
                .unwrap()
                .contains("Unknown")
        );
    }

    async fn assert_remote_metadata_denied(
        provider: &dyn EmbeddingProvider,
        provider_label: &str,
        backend: &str,
        base_url: &str,
    ) {
        for mode in [ModelControlMode::DenyPaid, ModelControlMode::LocalOnly] {
            let store = Store::open_in_memory().expect("model-control store");
            store.set_model_control_mode(mode).expect("control mode");
            let control = EmbeddingControl {
                store: Arc::new(Mutex::new(store)),
                event_bus: Arc::new(EventBus::new(8)),
                owner: InvocationOwner::default(),
                provider: provider_label.to_string(),
                backend: backend.to_string(),
                model: provider.model().to_string(),
                base_url: Some(base_url.to_string()),
                trigger: "factory_policy_test".to_string(),
                dedup_namespace: format!("factory-policy-{mode:?}-{}", uuid::Uuid::new_v4()),
            };
            let error = embed_query_with_timeout(provider, "no network", Some(&control))
                .await
                .expect_err("remote factory metadata must be denied before transport");
            assert!(matches!(error, DaemonError::PolicyDenied(_)));
        }
    }

    #[tokio::test]
    async fn explicit_remote_openai_factory_metadata_is_policy_denied() {
        let config = MemoryConfig {
            embedding_provider: "openai".to_string(),
            embedding_url: Some("https://api.openai.com/v1".to_string()),
            embedding_api_key: Some("test-only".to_string()),
            ..MemoryConfig::default()
        };
        let result = create_embedding_provider(&config, &reqwest::Client::new()).await;
        assert_eq!(result.provider_label, "Remote");
        assert_eq!(result.backend, "openai_compatible_api");
        assert_remote_metadata_denied(
            result.provider.as_deref().expect("OpenAI provider"),
            &result.provider_label,
            &result.backend,
            result.base_url.as_deref().expect("resolved URL"),
        )
        .await;
    }

    #[tokio::test]
    async fn remote_ollama_factory_attribution_is_policy_denied() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("loopback probe server");
        let address = listener.local_addr().expect("probe address");
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.expect("probe request");
            let mut request = [0_u8; 4096];
            let bytes = socket.read(&mut request).await.expect("probe bytes");
            assert!(
                std::str::from_utf8(&request[..bytes])
                    .expect("UTF-8 probe")
                    .starts_with("GET /api/tags HTTP/1.1")
            );
            socket
                .write_all(b"HTTP/1.1 200 OK\r\ncontent-length: 2\r\nconnection: close\r\n\r\n{}")
                .await
                .expect("probe response");
        });
        let http = reqwest::Client::builder()
            .no_proxy()
            .resolve("ollama.remote.test", address)
            .build()
            .expect("resolved HTTP client");
        let base_url = format!("http://ollama.remote.test:{}", address.port());
        let config = MemoryConfig {
            embedding_provider: "ollama".to_string(),
            embedding_url: Some(base_url.clone()),
            ..MemoryConfig::default()
        };

        let result = create_embedding_provider(&config, &http).await;
        server.await.expect("probe server");
        assert_eq!(result.provider_label, "Remote");
        assert_eq!(result.backend, "ollama_remote");
        assert_remote_metadata_denied(
            result.provider.as_deref().expect("Ollama provider"),
            &result.provider_label,
            &result.backend,
            result.base_url.as_deref().expect("resolved URL"),
        )
        .await;
    }

    // --- MockEmbeddingProvider tests ---

    #[tokio::test]
    async fn test_mock_deterministic() {
        let mock = mock::MockEmbeddingProvider::new(8);
        let a = mock
            .embed_query(
                "hello",
                crate::model_control::AdmittedEmbeddingExecution::for_test(
                    crate::model_control::registry::RuntimeExecutionRoute::OpenAiEmbeddingHttp,
                ),
            )
            .await
            .unwrap();
        let b = mock
            .embed_query(
                "hello",
                crate::model_control::AdmittedEmbeddingExecution::for_test(
                    crate::model_control::registry::RuntimeExecutionRoute::OpenAiEmbeddingHttp,
                ),
            )
            .await
            .unwrap();
        assert_eq!(a, b);
    }

    #[tokio::test]
    async fn test_mock_different_inputs() {
        let mock = mock::MockEmbeddingProvider::new(8);
        let a = mock
            .embed_query(
                "hello",
                crate::model_control::AdmittedEmbeddingExecution::for_test(
                    crate::model_control::registry::RuntimeExecutionRoute::OpenAiEmbeddingHttp,
                ),
            )
            .await
            .unwrap();
        let b = mock
            .embed_query(
                "world",
                crate::model_control::AdmittedEmbeddingExecution::for_test(
                    crate::model_control::registry::RuntimeExecutionRoute::OpenAiEmbeddingHttp,
                ),
            )
            .await
            .unwrap();
        assert_ne!(a, b);
    }

    #[tokio::test]
    async fn test_mock_fail_next() {
        let mock = mock::MockEmbeddingProvider::new(8);
        mock.set_fail_next("test error");
        assert!(
            mock.embed_query(
                "hello",
                crate::model_control::AdmittedEmbeddingExecution::for_test(
                    crate::model_control::registry::RuntimeExecutionRoute::OpenAiEmbeddingHttp,
                ),
            )
            .await
            .is_err()
        );
        // Second call should succeed
        assert!(
            mock.embed_query(
                "hello",
                crate::model_control::AdmittedEmbeddingExecution::for_test(
                    crate::model_control::registry::RuntimeExecutionRoute::OpenAiEmbeddingHttp,
                ),
            )
            .await
            .is_ok()
        );
    }
}
