use crate::bus::EventBus;
use crate::error::{DaemonError, Result};
use crate::memory::embedding::cache::{CacheLookupResult, EmbeddingCache};
use crate::memory::store::MemoryStore;
use crate::memory::types::{EmbeddingProvider, MemoryChunk};
use crate::model_control::retry::classify_error_message;
use crate::model_control::{
    AdmissionDecision, InvocationCompletion, ModelAdmissionRequest, admit_invocation,
    classify_error_class, completion_with_wall_time, settle_result, stable_dedup_key,
};
use crate::store::Store;
use rsi_common::model_control::{
    InvocationOwner, ModelInvocationPurpose, ModelInvocationStatus, ModelUsageConfidence,
};
use std::sync::Arc;
use tokio::sync::Mutex;

/// Maximum estimated tokens per batch (chars / 4 approximation).
const BATCH_MAX_TOKENS: usize = 8000;

/// Embedding retries default to zero. If a future policy explicitly enables
/// retries, each attempt must acquire its own admission and lineage row.
const RETRY_MAX_ATTEMPTS: u32 = 1;

/// Safety bound on how many completed-and-superseded dedup generations
/// `embed_batch_with_admission` will walk past before giving up on a single
/// call. Each genuine cache-loss reindex cycle over the same content
/// advances the generation by exactly one, so this exists only to stop a
/// runaway loop if the walk logic is ever broken — not to cap legitimate
/// reindex cycles. Tens of thousands of consecutive cache losses over the
/// same content in one process lifetime is not a realistic scenario.
const MAX_DEDUP_RETRY_GENERATIONS: u32 = 100_000;

/// Query timeout for remote providers (ms).
const QUERY_TIMEOUT_REMOTE_MS: u64 = 60_000;

/// Query timeout for local providers (ms).
const QUERY_TIMEOUT_LOCAL_MS: u64 = 300_000;

/// Batch timeout for remote providers (ms).
const BATCH_TIMEOUT_REMOTE_MS: u64 = 120_000;

/// Batch timeout for local providers (ms).
const BATCH_TIMEOUT_LOCAL_MS: u64 = 600_000;

#[derive(Clone)]
pub struct EmbeddingControl {
    pub store: Arc<Mutex<Store>>,
    pub event_bus: Arc<EventBus>,
    pub owner: InvocationOwner,
    pub provider: String,
    pub backend: String,
    pub model: String,
    pub base_url: Option<String>,
    pub trigger: String,
    pub dedup_namespace: String,
}

/// Embed all chunks in batches, using cache to avoid redundant computation.
///
/// 1. Check cache for existing embeddings
/// 2. Build batches from uncached chunks
/// 3. Embed each batch with retry
/// 4. Store results in cache
/// 5. Prune cache if needed
pub async fn embed_chunks_in_batches(
    provider: &dyn EmbeddingProvider,
    chunks: &[MemoryChunk],
    cache: &EmbeddingCache,
    store: &MemoryStore,
    control: Option<&EmbeddingControl>,
) -> Result<Vec<Vec<f32>>> {
    if chunks.is_empty() {
        return Ok(vec![]);
    }

    // 1. Check cache
    let mut result: CacheLookupResult = cache.get_cached_embeddings(store, chunks)?;
    if result.all_cached() {
        return Ok(result.into_embeddings_lossy());
    }

    // 2. Collect missing chunks and build batches
    let missing_chunks: Vec<&MemoryChunk> =
        result.missing_indices.iter().map(|&i| &chunks[i]).collect();

    let batches = build_batches(&missing_chunks);

    // 3. Embed each batch
    let mut all_computed: Vec<Vec<f32>> = Vec::new();
    let mut to_cache: Vec<(String, Vec<f32>)> = Vec::new();

    for batch_indices in &batches {
        let texts: Vec<String> = batch_indices
            .iter()
            .map(|&i| missing_chunks[i].text.clone())
            .collect();

        let embeddings = embed_batch_with_retry(provider, &texts, control).await?;

        for (j, embedding) in embeddings.into_iter().enumerate() {
            let chunk_idx = batch_indices[j];
            let hash = missing_chunks[chunk_idx].hash.clone();
            if !hash.is_empty() {
                to_cache.push((hash, embedding.clone()));
            }
            all_computed.push(embedding);
        }
    }

    // 4. Store in cache
    cache.store_embeddings(store, &to_cache)?;

    // 5. Merge and return
    result.merge_computed(all_computed);

    // 6. Prune cache
    cache.prune_if_needed(store)?;

    Ok(result.into_embeddings_lossy())
}

/// Embed a single query string with a timeout (no retry).
pub async fn embed_query_with_timeout(
    provider: &dyn EmbeddingProvider,
    text: &str,
    control: Option<&EmbeddingControl>,
) -> Result<Vec<f32>> {
    let timeout = resolve_query_timeout(provider);
    match control {
        Some(control) => {
            let dedup_key =
                stable_dedup_key(&control.dedup_namespace, &[&control.model, text, "query"]);
            let request = ModelAdmissionRequest {
                purpose: ModelInvocationPurpose::MemoryEmbeddingIndex,
                provider: Some(control.provider.clone()),
                model: Some(control.model.clone()),
                backend: Some(control.backend.clone()),
                effort: None,
                trigger: control.trigger.clone(),
                owner: control.owner.clone(),
                dedup_key: Some(dedup_key),
                request_fingerprint: Some(crate::model_control::hash_request_fingerprint(&[
                    &control.model,
                    control.base_url.as_deref().unwrap_or(""),
                    text,
                ])),
                parent_invocation_id: None,
                retry_of_invocation_id: None,
                expected_usage: Some(crate::model_control::explicit_expected_usage(
                    ModelInvocationPurpose::MemoryEmbeddingIndex,
                    Some(control.provider.as_str()),
                    Some(control.backend.as_str()),
                    Some(control.model.as_str()),
                )),
                baseline_input_tokens: 0,
                baseline_output_tokens: 0,
                baseline_cache_creation_tokens: 0,
                baseline_cache_read_tokens: 0,
                baseline_reasoning_tokens: 0,
                baseline_embedding_input_count: 0,
                baseline_wall_time_ms: 0,
            };
            let permit = match admit_invocation(&control.store, request, &control.event_bus).await?
            {
                AdmissionDecision::Admitted(permit) => permit,
                AdmissionDecision::Duplicate { invocation_id } => {
                    return Err(DaemonError::PolicyDenied(format!(
                        "duplicate embedding query suppressed ({invocation_id})"
                    )));
                }
            };
            let execution = permit.claim_embedding_execution(if provider.id() == "ollama" {
                crate::model_control::registry::RuntimeExecutionRoute::OllamaEmbeddingQueryHttp
            } else {
                crate::model_control::registry::RuntimeExecutionRoute::OpenAiEmbeddingHttp
            })?;
            let started_at = std::time::Instant::now();
            let result = match tokio::time::timeout(
                std::time::Duration::from_millis(timeout),
                provider.embed_query(text, execution),
            )
            .await
            {
                Ok(result) => result,
                Err(_) => Err(DaemonError::Process(format!(
                    "Embedding query timed out after {}ms",
                    timeout
                ))),
            };
            let completion = embedding_completion(started_at, 1, &result);
            settle_result(
                &control.store,
                &permit,
                completion,
                result,
                "Embedding query",
                &control.event_bus,
            )
            .await
        }
        None => Err(DaemonError::PolicyDenied(format!(
            "embedding query via provider '{}' requires model-control admission",
            provider.id()
        ))),
    }
}

pub async fn embed_text_batch(
    provider: &dyn EmbeddingProvider,
    texts: &[String],
    control: Option<&EmbeddingControl>,
) -> Result<Vec<Vec<f32>>> {
    embed_batch_with_retry(provider, texts, control).await
}

/// Group chunks into batches based on estimated token count.
/// Each batch stays under BATCH_MAX_TOKENS. Oversized chunks get solo batches.
fn build_batches(chunks: &[&MemoryChunk]) -> Vec<Vec<usize>> {
    if chunks.is_empty() {
        return vec![];
    }

    let max_chars = BATCH_MAX_TOKENS * 4; // token estimate
    let mut batches: Vec<Vec<usize>> = Vec::new();
    let mut current_batch: Vec<usize> = Vec::new();
    let mut current_chars: usize = 0;

    for (i, chunk) in chunks.iter().enumerate() {
        let chunk_chars = chunk.text.len();

        // Oversized chunk gets its own batch
        if chunk_chars > max_chars {
            if !current_batch.is_empty() {
                batches.push(std::mem::take(&mut current_batch));
                current_chars = 0;
            }
            batches.push(vec![i]);
            continue;
        }

        if current_chars + chunk_chars > max_chars && !current_batch.is_empty() {
            batches.push(std::mem::take(&mut current_batch));
            current_chars = 0;
        }

        current_batch.push(i);
        current_chars += chunk_chars;
    }

    if !current_batch.is_empty() {
        batches.push(current_batch);
    }

    batches
}

/// Embed a batch of texts with retry on transient errors.
async fn embed_batch_with_retry(
    provider: &dyn EmbeddingProvider,
    texts: &[String],
    control: Option<&EmbeddingControl>,
) -> Result<Vec<Vec<f32>>> {
    if let Some(control) = control {
        return embed_batch_with_admission(provider, texts, control).await;
    }

    embed_batch_without_admission(provider, texts).await
}

async fn embed_batch_without_admission(
    provider: &dyn EmbeddingProvider,
    _texts: &[String],
) -> Result<Vec<Vec<f32>>> {
    Err(DaemonError::PolicyDenied(format!(
        "embedding batch via provider '{}' requires model-control admission",
        provider.id()
    )))
}

async fn embed_batch_with_admission(
    provider: &dyn EmbeddingProvider,
    texts: &[String],
    control: &EmbeddingControl,
) -> Result<Vec<Vec<f32>>> {
    let joined_texts = texts.join("\u{1f}");
    let base_url = control.base_url.as_deref().unwrap_or("");
    let request_fingerprint =
        crate::model_control::hash_request_fingerprint(&[&control.model, base_url, &joined_texts]);
    let expected_usage = crate::model_control::explicit_expected_usage(
        ModelInvocationPurpose::MemoryEmbeddingIndex,
        Some(control.provider.as_str()),
        Some(control.backend.as_str()),
        Some(control.model.as_str()),
    );
    let make_request = |dedup_key: String| ModelAdmissionRequest {
        purpose: ModelInvocationPurpose::MemoryEmbeddingIndex,
        provider: Some(control.provider.clone()),
        model: Some(control.model.clone()),
        backend: Some(control.backend.clone()),
        effort: None,
        trigger: control.trigger.clone(),
        owner: control.owner.clone(),
        dedup_key: Some(dedup_key),
        request_fingerprint: Some(request_fingerprint.clone()),
        parent_invocation_id: None,
        // Deliberately NOT set, even on the stale-duplicate retry below: a
        // non-null `retry_of_invocation_id` opts into the retry budget
        // scope's A9 no-implicit-retry invariant (`max_retries: 0` by
        // default — see `default_counter_policies` in
        // store/model_control.rs), which exists to stop interactive
        // sessions auto-retrying paid calls. That is a different invariant
        // than the one below: this is a *fresh* admission for content whose
        // previous, already-paid-for result is unrecoverable, not an
        // automatic retry of a failed attempt.
        retry_of_invocation_id: None,
        expected_usage: Some(expected_usage.clone()),
        baseline_input_tokens: 0,
        baseline_output_tokens: 0,
        baseline_cache_creation_tokens: 0,
        baseline_cache_read_tokens: 0,
        baseline_reasoning_tokens: 0,
        baseline_embedding_input_count: 0,
        baseline_wall_time_ms: 0,
    };
    let primary_dedup_key = stable_dedup_key(
        &control.dedup_namespace,
        &[&control.model, base_url, &joined_texts],
    );

    // `embed_batch_with_admission` is only ever called for texts the
    // embedding cache just confirmed it does NOT hold (see the
    // `all_cached()` short-circuit in `embed_chunks_in_batches`), so a
    // `Duplicate` here can never be satisfied by re-reading the cache:
    // whatever a prior invocation produced (if anything) is already gone
    // from the only durable store that holds vectors.
    //
    // The real invariant this admission guard protects is "do not pay twice
    // for a result you still have" — not "never admit this content twice".
    // A `completed` dedup row proves a *specific past result* was paid for
    // and (per 63e18e15) is held as non-retryable forever for audit
    // purposes, but it says nothing about whether that result still exists
    // anywhere. Once the cache has lost it, the row is stale evidence of a
    // vector that is gone, and every subsequent reindex needs its own fresh
    // row: the memory store carries no explicit "cache generation" counter
    // (crates/rsid/src/memory/embedding/cache.rs has no versioning field),
    // so we derive one implicitly by walking forward through the chain of
    // dedup keys for this exact content — generation 0 is the primary
    // content-derived key, generation N is the key used by the Nth retry —
    // until we reach a generation whose row is not `Completed`.
    //
    // That walk is entirely deterministic from durable DB state (never a
    // nonce or timestamp): the same content always produces the same
    // generation chain, so two concurrent callers re-embedding identical
    // content converge on the same next generation and race for the same
    // key, exactly as they would for the primary key today. If a generation
    // in the chain is live (running / cancellation-requested / unknown —
    // not yet `Completed`), stop and suppress: that is the genuine
    // concurrent-duplicate protection this guard exists for, and it applies
    // at every generation, not just the first. Only a `Completed` row lets
    // the walk advance, because only a `Completed` row proves the prior
    // attempt is durably finished (and, per this fix, potentially stale).
    let mut dedup_key = primary_dedup_key;
    let mut generation: u32 = 0;
    let permit = loop {
        let decision = admit_invocation(
            &control.store,
            make_request(dedup_key.clone()),
            &control.event_bus,
        )
        .await?;
        match decision {
            AdmissionDecision::Admitted(permit) => break permit,
            AdmissionDecision::Duplicate { invocation_id } => {
                let prior_status = {
                    let guard = control.store.lock().await;
                    guard
                        .load_model_invocation_record(invocation_id)?
                        .map(|r| r.status)
                };
                if prior_status != Some(ModelInvocationStatus::Completed) {
                    return Err(DaemonError::PolicyDenied(format!(
                        "duplicate embedding batch suppressed ({invocation_id})"
                    )));
                }
                generation += 1;
                if generation > MAX_DEDUP_RETRY_GENERATIONS {
                    return Err(DaemonError::PolicyDenied(format!(
                        "duplicate embedding batch suppressed after {MAX_DEDUP_RETRY_GENERATIONS} \
                         completed retry generations ({invocation_id})"
                    )));
                }
                dedup_key = stable_dedup_key(
                    &control.dedup_namespace,
                    &[
                        &control.model,
                        base_url,
                        &joined_texts,
                        "retry-uncached-generation",
                        generation.to_string().as_str(),
                    ],
                );
            }
        }
    };
    let execution = permit.claim_embedding_execution(if provider.id() == "ollama" {
        crate::model_control::registry::RuntimeExecutionRoute::OllamaEmbeddingBatchHttp
    } else {
        crate::model_control::registry::RuntimeExecutionRoute::OpenAiEmbeddingHttp
    })?;
    let started_at = std::time::Instant::now();
    let result = run_batch_once(provider, texts, execution).await;
    let completion = embedding_completion(started_at, texts.len() as u64, &result);
    settle_result(
        &control.store,
        &permit,
        completion,
        result,
        "Embedding batch",
        &control.event_bus,
    )
    .await
}

fn embedding_completion<T>(
    started_at: std::time::Instant,
    item_count: u64,
    result: &Result<T>,
) -> InvocationCompletion {
    match result {
        Ok(_) => InvocationCompletion {
            embedding_input_count: Some(item_count),
            wall_time_ms: completion_with_wall_time(
                started_at,
                None,
                ModelUsageConfidence::Partial,
            )
            .wall_time_ms,
            confidence: Some(ModelUsageConfidence::Partial),
            ..InvocationCompletion::default()
        },
        Err(error) => InvocationCompletion {
            embedding_input_count: Some(item_count),
            wall_time_ms: completion_with_wall_time(
                started_at,
                Some(classify_error_class(error)),
                ModelUsageConfidence::Partial,
            )
            .wall_time_ms,
            error_class: Some(classify_error_class(error)),
            confidence: Some(ModelUsageConfidence::Partial),
            ..InvocationCompletion::default()
        },
    }
}

async fn run_batch_once(
    provider: &dyn EmbeddingProvider,
    texts: &[String],
    execution: crate::model_control::AdmittedEmbeddingExecution,
) -> Result<Vec<Vec<f32>>> {
    let timeout = resolve_batch_timeout(provider);
    match tokio::time::timeout(
        std::time::Duration::from_millis(timeout),
        provider.embed_batch(texts, execution),
    )
    .await
    {
        Ok(Ok(result)) => Ok(result),
        Ok(Err(error)) => {
            let _classification = classify_error_message(&error.to_string());
            Err(error)
        }
        Err(_) => Err(DaemonError::Process(format!(
            "Embedding batch timed out after {}ms ({} attempt)",
            timeout, RETRY_MAX_ATTEMPTS
        ))),
    }
}

fn resolve_query_timeout(provider: &dyn EmbeddingProvider) -> u64 {
    if provider.id() == "ollama" {
        QUERY_TIMEOUT_LOCAL_MS
    } else {
        QUERY_TIMEOUT_REMOTE_MS
    }
}

fn resolve_batch_timeout(provider: &dyn EmbeddingProvider) -> u64 {
    if provider.id() == "ollama" {
        BATCH_TIMEOUT_LOCAL_MS
    } else {
        BATCH_TIMEOUT_REMOTE_MS
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::memory::embedding::mock::MockEmbeddingProvider;
    use crate::memory::files::hash_text;
    use crate::store::Store;
    use rsi_common::model_control::InvocationOwner;
    use rusqlite::params;
    use std::sync::atomic::AtomicU32;
    use std::sync::atomic::Ordering;

    fn make_chunk(text: &str) -> MemoryChunk {
        MemoryChunk {
            start_line: 1,
            end_line: 1,
            text: text.to_string(),
            hash: hash_text(text),
        }
    }

    fn make_cache() -> EmbeddingCache {
        EmbeddingCache {
            provider_id: "mock".to_string(),
            model: "mock-embed".to_string(),
            provider_key: "key".to_string(),
            max_entries: 1000,
            enabled: true,
        }
    }

    struct CountingProvider {
        id: &'static str,
        query_calls: AtomicU32,
        batch_calls: AtomicU32,
        batch_error: Option<&'static str>,
    }

    impl CountingProvider {
        fn new(id: &'static str) -> Self {
            Self {
                id,
                query_calls: AtomicU32::new(0),
                batch_calls: AtomicU32::new(0),
                batch_error: None,
            }
        }

        fn with_batch_error(id: &'static str, batch_error: &'static str) -> Self {
            Self {
                id,
                query_calls: AtomicU32::new(0),
                batch_calls: AtomicU32::new(0),
                batch_error: Some(batch_error),
            }
        }
    }

    #[async_trait::async_trait]
    impl EmbeddingProvider for CountingProvider {
        fn id(&self) -> &str {
            self.id
        }

        fn model(&self) -> &str {
            "counting-embed"
        }

        fn max_input_tokens(&self) -> Option<u32> {
            None
        }

        async fn embed_query(
            &self,
            _text: &str,
            _execution: crate::model_control::AdmittedEmbeddingExecution,
        ) -> Result<Vec<f32>> {
            self.query_calls.fetch_add(1, Ordering::Relaxed);
            Ok(vec![0.1, 0.2, 0.3, 0.4])
        }

        async fn embed_batch(
            &self,
            texts: &[String],
            _execution: crate::model_control::AdmittedEmbeddingExecution,
        ) -> Result<Vec<Vec<f32>>> {
            self.batch_calls.fetch_add(1, Ordering::Relaxed);
            if let Some(message) = self.batch_error {
                return Err(DaemonError::Process(message.to_string()));
            }
            Ok(texts.iter().map(|_| vec![0.1, 0.2, 0.3, 0.4]).collect())
        }
    }

    fn make_control(
        store: Arc<Mutex<Store>>,
        provider: &str,
        backend: &str,
        model: &str,
        base_url: Option<&str>,
    ) -> EmbeddingControl {
        EmbeddingControl {
            store,
            event_bus: Arc::new(EventBus::new(32)),
            owner: InvocationOwner::default(),
            provider: provider.to_string(),
            backend: backend.to_string(),
            model: model.to_string(),
            base_url: base_url.map(str::to_string),
            trigger: "test".to_string(),
            dedup_namespace: "embedding:test".to_string(),
        }
    }

    fn make_local_test_control() -> EmbeddingControl {
        make_control(
            Arc::new(Mutex::new(
                Store::open_in_memory().expect("model-control store"),
            )),
            "Local",
            "mock",
            "mock-model",
            Some("http://localhost"),
        )
    }

    async fn invocation_row_count(store: &Arc<Mutex<Store>>) -> usize {
        let guard = store.lock().await;
        guard
            .conn
            .query_row("SELECT COUNT(*) FROM model_invocations", params![], |row| {
                row.get::<_, i64>(0)
            })
            .expect("count model invocations") as usize
    }

    async fn latest_invocation_state(store: &Arc<Mutex<Store>>) -> (String, Option<String>) {
        let guard = store.lock().await;
        guard
            .conn
            .query_row(
                "SELECT status, error_class FROM model_invocations ORDER BY created_at DESC LIMIT 1",
                params![],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .expect("latest model invocation")
    }

    // --- build_batches tests ---

    #[test]
    fn test_build_batches_empty() {
        let batches = build_batches(&[]);
        assert!(batches.is_empty());
    }

    #[test]
    fn test_build_batches_single_small() {
        let chunk = make_chunk("hello");
        let batches = build_batches(&[&chunk]);
        assert_eq!(batches.len(), 1);
        assert_eq!(batches[0], vec![0]);
    }

    #[test]
    fn test_build_batches_multiple_fit_one() {
        let c1 = make_chunk("hello");
        let c2 = make_chunk("world");
        let batches = build_batches(&[&c1, &c2]);
        assert_eq!(batches.len(), 1);
        assert_eq!(batches[0], vec![0, 1]);
    }

    #[test]
    fn test_build_batches_oversized_solo() {
        let big_text = "x".repeat(BATCH_MAX_TOKENS * 4 + 1);
        let big_chunk = MemoryChunk {
            start_line: 1,
            end_line: 1,
            text: big_text,
            hash: "big".to_string(),
        };
        let small = make_chunk("small");
        let batches = build_batches(&[&small, &big_chunk, &small]);
        // big chunk should be alone in its batch
        assert!(batches.len() >= 2);
        let big_batch = batches.iter().find(|b| b.contains(&1)).unwrap();
        assert_eq!(big_batch.len(), 1);
    }

    #[test]
    fn test_build_batches_split_by_budget() {
        // Create chunks that together exceed the budget
        let text = "a".repeat(BATCH_MAX_TOKENS * 2); // half the char budget per chunk
        let c1 = MemoryChunk {
            start_line: 1,
            end_line: 1,
            text: text.clone(),
            hash: "c1".to_string(),
        };
        let c2 = MemoryChunk {
            start_line: 1,
            end_line: 1,
            text: text.clone(),
            hash: "c2".to_string(),
        };
        let c3 = MemoryChunk {
            start_line: 1,
            end_line: 1,
            text: text,
            hash: "c3".to_string(),
        };
        let batches = build_batches(&[&c1, &c2, &c3]);
        assert!(batches.len() >= 2);
    }

    // --- embed_chunks_in_batches tests ---

    #[tokio::test]
    async fn test_embed_chunks_empty() {
        let provider = MockEmbeddingProvider::new(4);
        let cache = make_cache();
        let store = MemoryStore::open_in_memory().unwrap();
        let result = embed_chunks_in_batches(&provider, &[], &cache, &store, None)
            .await
            .unwrap();
        assert!(result.is_empty());
    }

    #[tokio::test]
    async fn test_embed_chunks_basic() {
        let provider = MockEmbeddingProvider::new(4);
        let cache = make_cache();
        let store = MemoryStore::open_in_memory().unwrap();
        let chunks = vec![make_chunk("hello"), make_chunk("world")];
        let control = make_local_test_control();
        let result = embed_chunks_in_batches(&provider, &chunks, &cache, &store, Some(&control))
            .await
            .unwrap();
        assert_eq!(result.len(), 2);
        assert_eq!(result[0].len(), 4);
    }

    #[tokio::test]
    async fn test_embed_chunks_uses_cache() {
        let provider = MockEmbeddingProvider::new(4);
        let cache = make_cache();
        let store = MemoryStore::open_in_memory().unwrap();
        let chunks = vec![make_chunk("hello")];
        let control = make_local_test_control();

        // First call computes
        let result1 = embed_chunks_in_batches(&provider, &chunks, &cache, &store, Some(&control))
            .await
            .unwrap();
        let calls1 = provider.batch_call_count.load(Ordering::Relaxed);
        assert_eq!(calls1, 1);

        // Second call should hit cache
        let result2 = embed_chunks_in_batches(&provider, &chunks, &cache, &store, Some(&control))
            .await
            .unwrap();
        let calls2 = provider.batch_call_count.load(Ordering::Relaxed);
        assert_eq!(calls2, 1); // no additional batch calls
        assert_eq!(result1, result2);
    }

    #[tokio::test]
    async fn test_embed_chunks_partial_cache() {
        let provider = MockEmbeddingProvider::new(4);
        let cache = make_cache();
        let store = MemoryStore::open_in_memory().unwrap();
        let control = make_local_test_control();

        // Embed first chunk and cache it
        let chunks1 = vec![make_chunk("cached")];
        embed_chunks_in_batches(&provider, &chunks1, &cache, &store, Some(&control))
            .await
            .unwrap();
        assert_eq!(provider.batch_call_count.load(Ordering::Relaxed), 1);

        // Now embed both; only second should need computation
        let chunks2 = vec![make_chunk("cached"), make_chunk("uncached")];
        embed_chunks_in_batches(&provider, &chunks2, &cache, &store, Some(&control))
            .await
            .unwrap();
        assert_eq!(provider.batch_call_count.load(Ordering::Relaxed), 2);
    }

    // --- is_retryable_error tests ---

    #[test]
    fn test_retryable_rate_limit() {
        assert!(classify_error_message("rate limit exceeded").retryable());
        assert!(classify_error_message("rate_limit_exceeded").retryable());
        assert!(classify_error_message("too many requests").retryable());
    }

    #[test]
    fn test_retryable_server_errors() {
        assert!(classify_error_message("server returned 500").retryable());
        assert!(classify_error_message("502 bad gateway").retryable());
        assert!(classify_error_message("503 service unavailable").retryable());
        assert!(classify_error_message("504 gateway timeout").retryable());
    }

    #[test]
    fn test_retryable_cloudflare() {
        assert!(classify_error_message("cloudflare error").retryable());
    }

    #[test]
    fn test_not_retryable() {
        assert!(!classify_error_message("invalid api key").retryable());
        assert!(!classify_error_message("model not found").retryable());
        assert!(!classify_error_message("bad request").retryable());
    }

    // --- timeout resolver tests ---

    #[test]
    fn test_query_timeout_ollama() {
        let provider = MockEmbeddingProvider::new(4);
        assert_eq!(resolve_query_timeout(&provider), QUERY_TIMEOUT_REMOTE_MS);
    }

    #[test]
    fn test_query_timeout_remote() {
        let provider = MockEmbeddingProvider::new(4);
        // Mock has id "mock", not "ollama"
        assert_eq!(resolve_query_timeout(&provider), QUERY_TIMEOUT_REMOTE_MS);
    }

    #[test]
    fn test_batch_timeout_remote() {
        let provider = MockEmbeddingProvider::new(4);
        assert_eq!(resolve_batch_timeout(&provider), BATCH_TIMEOUT_REMOTE_MS);
    }

    // --- embed_query_with_timeout test ---

    #[tokio::test]
    async fn test_embed_query_with_timeout_success() {
        let provider = MockEmbeddingProvider::new(4);
        let control = make_local_test_control();
        let result = embed_query_with_timeout(&provider, "hello", Some(&control))
            .await
            .unwrap();
        assert_eq!(result.len(), 4);
    }

    #[tokio::test]
    async fn denied_embedding_query_does_not_execute_provider_future() {
        let provider = CountingProvider::new("remote-test");
        let store = Arc::new(Mutex::new(Store::open_in_memory().expect("store")));
        let control = make_control(
            Arc::clone(&store),
            "OpenAI",
            "openai_compatible_api",
            "text-embedding-3-small",
            Some("https://api.openai.com/v1"),
        );

        let error = embed_query_with_timeout(&provider, "blocked", Some(&control))
            .await
            .expect_err("paid-capable background query must be denied by default");

        assert!(matches!(error, DaemonError::PolicyDenied(_)));
        assert_eq!(provider.query_calls.load(Ordering::Relaxed), 0);
        assert_eq!(invocation_row_count(&store).await, 1);
    }

    #[tokio::test]
    async fn successful_embedding_batch_settles_one_completed_row() {
        let provider = CountingProvider::new("ollama");
        let store = Arc::new(Mutex::new(Store::open_in_memory().expect("store")));
        let control = make_control(
            Arc::clone(&store),
            "Local",
            "ollama",
            "nomic-embed-text",
            Some("http://localhost:11434"),
        );
        let texts = vec!["alpha".to_string(), "beta".to_string()];

        let embeddings = embed_text_batch(&provider, &texts, Some(&control))
            .await
            .expect("admitted embedding batch");

        assert_eq!(embeddings.len(), 2);
        assert_eq!(provider.batch_calls.load(Ordering::Relaxed), 1);
        assert_eq!(
            latest_invocation_state(&store).await,
            ("completed".to_string(), None)
        );
    }

    #[tokio::test]
    async fn identical_content_embeds_twice_after_completed_duplicate() {
        // Regression test for the Duplicate-as-fatal bug: a second admission
        // request for the exact same (model, base_url, content) triple, after
        // the first has already reached `completed`, must NOT hard-fail. The
        // model_control dedup key for a completed row is held forever
        // (63e18e15), so without the fix every subsequent identical request
        // is permanently denied.
        let provider = CountingProvider::new("ollama");
        let store = Arc::new(Mutex::new(Store::open_in_memory().expect("store")));
        let control = make_control(
            Arc::clone(&store),
            "Local",
            "ollama",
            "nomic-embed-text",
            Some("http://localhost:11434"),
        );
        let texts = vec!["alpha".to_string(), "beta".to_string()];

        let first = embed_text_batch(&provider, &texts, Some(&control))
            .await
            .expect("first embedding batch succeeds");
        assert_eq!(first.len(), 2);
        assert_eq!(provider.batch_calls.load(Ordering::Relaxed), 1);

        let second = embed_text_batch(&provider, &texts, Some(&control))
            .await
            .expect("re-embedding identical content after a completed duplicate must succeed");
        assert_eq!(second.len(), 2);
        // The stale completed duplicate is not retrievable from anywhere, so
        // the retry legitimately re-invokes the provider exactly once more.
        assert_eq!(provider.batch_calls.load(Ordering::Relaxed), 2);
        // Two rows: the original completed invocation, plus the salted retry.
        assert_eq!(invocation_row_count(&store).await, 2);
        assert_eq!(
            latest_invocation_state(&store).await,
            ("completed".to_string(), None)
        );
    }

    #[tokio::test]
    async fn two_consecutive_cache_loss_cycles_both_reembed_successfully() {
        // Regression test for the bug in c15b0450's fix: that fix salted the
        // retry dedup key with the *stale invocation id*, which is a pure
        // function of the original row and therefore identical on every
        // subsequent pass. A second cache-loss cycle over the same content
        // re-admits under that same salted key, finds the FIRST retry's now
        // -completed row sitting there, and is permanently suppressed again
        // — i.e. the fix survives exactly one cache-loss cycle, not
        // arbitrarily many. This test drives THREE embeddings of identical
        // content back to back (initial index, then two consecutive
        // cache-loss reindexes) and requires all three to succeed.
        let provider = CountingProvider::new("ollama");
        let store = Arc::new(Mutex::new(Store::open_in_memory().expect("store")));
        let control = make_control(
            Arc::clone(&store),
            "Local",
            "ollama",
            "nomic-embed-text",
            Some("http://localhost:11434"),
        );
        let texts = vec!["alpha".to_string(), "beta".to_string()];

        // Initial index.
        let first = embed_text_batch(&provider, &texts, Some(&control))
            .await
            .expect("initial embedding batch succeeds");
        assert_eq!(first.len(), 2);
        assert_eq!(provider.batch_calls.load(Ordering::Relaxed), 1);

        // Cache-loss cycle #1: must re-embed.
        let second = embed_text_batch(&provider, &texts, Some(&control))
            .await
            .expect("first cache-loss reindex must re-embed successfully");
        assert_eq!(second.len(), 2);
        assert_eq!(provider.batch_calls.load(Ordering::Relaxed), 2);

        // Cache-loss cycle #2: the exact case c15b0450 does not cover. Under
        // the old salted-by-invocation-id retry key, this call hits
        // `Duplicate` against cycle #1's completed retry row and is denied.
        let third = embed_text_batch(&provider, &texts, Some(&control))
            .await
            .expect("second consecutive cache-loss reindex must also re-embed successfully");
        assert_eq!(third.len(), 2);
        assert_eq!(provider.batch_calls.load(Ordering::Relaxed), 3);

        // Three rows: the original completed invocation plus one per
        // cache-loss retry generation.
        assert_eq!(invocation_row_count(&store).await, 3);
        assert_eq!(
            latest_invocation_state(&store).await,
            ("completed".to_string(), None)
        );
    }

    #[tokio::test]
    async fn duplicate_while_prior_attempt_still_running_stays_suppressed() {
        // A Duplicate against a non-terminal prior attempt (running /
        // cancellation-requested / unknown) must keep failing hard — that is
        // the genuine concurrent-duplicate protection this admission guard
        // exists for, and retrying it would double-spend on a request that
        // may still be in flight.
        let store = Arc::new(Mutex::new(Store::open_in_memory().expect("store")));
        let control = make_control(
            Arc::clone(&store),
            "Local",
            "ollama",
            "nomic-embed-text",
            Some("http://localhost:11434"),
        );
        let texts = vec!["alpha".to_string(), "beta".to_string()];
        let dedup_key = stable_dedup_key(
            &control.dedup_namespace,
            &[
                &control.model,
                control.base_url.as_deref().unwrap_or(""),
                &texts.join("\u{1f}"),
            ],
        );

        // Admit directly and leave the invocation running (never settled),
        // simulating a genuinely concurrent in-flight duplicate.
        let request = ModelAdmissionRequest {
            purpose: ModelInvocationPurpose::MemoryEmbeddingIndex,
            provider: Some(control.provider.clone()),
            model: Some(control.model.clone()),
            backend: Some(control.backend.clone()),
            effort: None,
            trigger: control.trigger.clone(),
            owner: control.owner.clone(),
            dedup_key: Some(dedup_key),
            request_fingerprint: Some(crate::model_control::hash_request_fingerprint(&[
                &control.model,
                control.base_url.as_deref().unwrap_or(""),
                &texts.join("\u{1f}"),
            ])),
            parent_invocation_id: None,
            retry_of_invocation_id: None,
            expected_usage: Some(crate::model_control::explicit_expected_usage(
                ModelInvocationPurpose::MemoryEmbeddingIndex,
                Some(control.provider.as_str()),
                Some(control.backend.as_str()),
                Some(control.model.as_str()),
            )),
            baseline_input_tokens: 0,
            baseline_output_tokens: 0,
            baseline_cache_creation_tokens: 0,
            baseline_cache_read_tokens: 0,
            baseline_reasoning_tokens: 0,
            baseline_embedding_input_count: 0,
            baseline_wall_time_ms: 0,
        };
        let decision = admit_invocation(&control.store, request, &control.event_bus)
            .await
            .expect("first admission succeeds");
        assert!(matches!(decision, AdmissionDecision::Admitted(_)));
        assert_eq!(
            latest_invocation_state(&store).await,
            ("running".to_string(), None)
        );

        let provider = CountingProvider::new("ollama");
        let error = embed_text_batch(&provider, &texts, Some(&control))
            .await
            .expect_err("duplicate against a still-running invocation must stay suppressed");

        assert!(matches!(error, DaemonError::PolicyDenied(_)));
        assert_eq!(provider.batch_calls.load(Ordering::Relaxed), 0);
        assert_eq!(invocation_row_count(&store).await, 1);
    }

    #[tokio::test]
    async fn full_reindex_over_already_embedded_content_indexes_nonzero() {
        // Simulates a full reindex: the memory index (and its embedding
        // cache) is rebuilt from scratch, but rsi.db's model_invocations
        // still holds `completed` rows for identical content from before the
        // rebuild. Every chunk's admission comes back `Duplicate`, and
        // without the fix every session in the reindex fails, exactly as
        // observed live (sessions_indexed=36 vs sessions_failed=1278).
        let provider = MockEmbeddingProvider::new(4);
        let control_store = Arc::new(Mutex::new(Store::open_in_memory().expect("store")));
        let control = make_control(
            Arc::clone(&control_store),
            "Local",
            "mock",
            "mock-model",
            Some("http://localhost"),
        );

        let chunks = vec![
            make_chunk("session content one"),
            make_chunk("session content two"),
        ];

        // First index: populates both the cache and rsi.db's completed rows.
        let first_cache = make_cache();
        let first_memory_store = MemoryStore::open_in_memory().unwrap();
        let first_result = embed_chunks_in_batches(
            &provider,
            &chunks,
            &first_cache,
            &first_memory_store,
            Some(&control),
        )
        .await
        .expect("initial index succeeds");
        assert_eq!(first_result.len(), 2);

        // Full reindex: fresh memory store/cache (as a rebuilt index would
        // have), but the SAME rsi.db model_invocations rows from above.
        let second_cache = make_cache();
        let second_memory_store = MemoryStore::open_in_memory().unwrap();
        let second_result = embed_chunks_in_batches(
            &provider,
            &chunks,
            &second_cache,
            &second_memory_store,
            Some(&control),
        )
        .await
        .expect("reindex over already-embedded content must index a non-zero count");

        assert_eq!(second_result.len(), 2);
        assert!(
            second_result.iter().all(|embedding| !embedding.is_empty()),
            "reindex must not silently return empty vectors for duplicated content"
        );
    }

    #[tokio::test]
    async fn embedding_retry_budget_defaults_to_single_attempt() {
        let provider = CountingProvider::with_batch_error("ollama", "server returned 500");
        let store = Arc::new(Mutex::new(Store::open_in_memory().expect("store")));
        let control = make_control(
            Arc::clone(&store),
            "Local",
            "ollama",
            "nomic-embed-text",
            Some("http://localhost:11434"),
        );

        let texts = vec!["alpha".to_string(), "beta".to_string()];
        let error = embed_text_batch(&provider, &texts, Some(&control))
            .await
            .expect_err("current retry budget should allow only one embedding attempt");

        assert!(matches!(error, DaemonError::Process(_)));
        assert_eq!(provider.batch_calls.load(Ordering::Relaxed), 1);
        assert_eq!(invocation_row_count(&store).await, 1);
    }
}
