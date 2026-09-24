use serde::{Deserialize, Serialize};

use crate::error::{DaemonError, Result};
use crate::memory::embedding::l2_normalize;
use crate::memory::types::EmbeddingProvider;
use crate::model_control::AdmittedEmbeddingExecution;

pub struct OpenAiCompatEmbeddingProvider {
    http: reqwest::Client,
    base_url: String,
    api_key: String,
    model: String,
    max_input_tokens: Option<u32>,
}

impl OpenAiCompatEmbeddingProvider {
    pub fn new(http: reqwest::Client, base_url: String, api_key: String, model: String) -> Self {
        let max_input_tokens = known_max_tokens(&model);
        Self {
            http,
            base_url,
            api_key,
            model,
            max_input_tokens,
        }
    }

    async fn call_api(
        &self,
        texts: &[String],
        execution: AdmittedEmbeddingExecution,
    ) -> Result<Vec<Vec<f32>>> {
        let url = format!("{}/embeddings", self.base_url);
        let body = OpenAiEmbedRequest {
            model: &self.model,
            input: texts,
        };

        let request = self.http.post(&url).bearer_auth(&self.api_key).json(&body);
        let resp = execution
            .bind_http(
                crate::model_control::registry::RuntimeExecutionRoute::OpenAiEmbeddingHttp,
                request,
            )
            .send("OpenAI embeddings")
            .await
            .map_err(|e| DaemonError::Process(format!("OpenAI request failed: {e}")))?;

        if !resp.status().is_success() {
            let status = resp.status();
            let body_text = resp.text().await.unwrap_or_default();
            return Err(DaemonError::Process(format!(
                "OpenAI returned {} — {}",
                status, body_text
            )));
        }

        let result: OpenAiEmbeddingResponse = resp
            .json()
            .await
            .map_err(|e| DaemonError::Process(format!("OpenAI JSON parse failed: {}", e)))?;

        if result.data.len() != texts.len() {
            return Err(DaemonError::Process(format!(
                "OpenAI returned {} embeddings for {} inputs",
                result.data.len(),
                texts.len()
            )));
        }

        // Sort by index to ensure correct order
        let mut data = result.data;
        data.sort_by_key(|d| d.index);

        let mut vecs: Vec<Vec<f32>> = data.into_iter().map(|d| d.embedding).collect();
        for v in &mut vecs {
            l2_normalize(v);
        }
        Ok(vecs)
    }
}

#[async_trait::async_trait]
impl EmbeddingProvider for OpenAiCompatEmbeddingProvider {
    fn id(&self) -> &str {
        "openai"
    }

    fn model(&self) -> &str {
        &self.model
    }

    fn max_input_tokens(&self) -> Option<u32> {
        self.max_input_tokens
    }

    async fn embed_query(
        &self,
        text: &str,
        execution: AdmittedEmbeddingExecution,
    ) -> Result<Vec<f32>> {
        let texts = vec![text.to_string()];
        let mut results = self.call_api(&texts, execution).await?;
        results
            .pop()
            .ok_or_else(|| DaemonError::Process("OpenAI returned no embeddings".to_string()))
    }

    async fn embed_batch(
        &self,
        texts: &[String],
        execution: AdmittedEmbeddingExecution,
    ) -> Result<Vec<Vec<f32>>> {
        self.call_api(texts, execution).await
    }
}

fn known_max_tokens(model: &str) -> Option<u32> {
    match model {
        "text-embedding-3-small" | "text-embedding-3-large" => Some(8192),
        "text-embedding-ada-002" => Some(8191),
        _ => None,
    }
}

// --- Serde types ---

#[derive(Serialize)]
struct OpenAiEmbedRequest<'a> {
    model: &'a str,
    input: &'a [String],
}

#[derive(Deserialize)]
struct OpenAiEmbeddingResponse {
    data: Vec<OpenAiEmbeddingData>,
}

#[derive(Deserialize)]
struct OpenAiEmbeddingData {
    embedding: Vec<f32>,
    index: usize,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn execution() -> AdmittedEmbeddingExecution {
        AdmittedEmbeddingExecution::for_test(
            crate::model_control::registry::RuntimeExecutionRoute::OpenAiEmbeddingHttp,
        )
    }

    fn make_provider(url: &str) -> OpenAiCompatEmbeddingProvider {
        OpenAiCompatEmbeddingProvider::new(
            reqwest::Client::new(),
            url.to_string(),
            "test-key".to_string(),
            "text-embedding-3-small".to_string(),
        )
    }

    #[tokio::test]
    async fn test_openai_successful_embed() {
        let mut server = mockito::Server::new_async().await;
        let mock = server
            .mock("POST", "/embeddings")
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(r#"{"data": [{"embedding": [1.0, 0.0], "index": 0}], "model": "text-embedding-3-small"}"#)
            .create_async()
            .await;

        let provider = make_provider(&server.url());
        let result = provider.embed_query("hello", execution()).await.unwrap();
        assert_eq!(result.len(), 2);
        let mag: f32 = result.iter().map(|v| v * v).sum::<f32>().sqrt();
        assert!((mag - 1.0).abs() < 1e-5);
        mock.assert_async().await;
    }

    #[tokio::test]
    async fn test_openai_auth_error() {
        let mut server = mockito::Server::new_async().await;
        let mock = server
            .mock("POST", "/embeddings")
            .with_status(401)
            .with_body(r#"{"error": {"message": "Invalid API key"}}"#)
            .create_async()
            .await;

        let provider = make_provider(&server.url());
        let err = provider
            .embed_query("hello", execution())
            .await
            .unwrap_err();
        assert!(err.to_string().contains("401"));
        mock.assert_async().await;
    }

    #[tokio::test]
    async fn test_openai_batch_success() {
        let mut server = mockito::Server::new_async().await;
        let mock = server
            .mock("POST", "/embeddings")
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(r#"{"data": [{"embedding": [1.0, 0.0], "index": 0}, {"embedding": [0.0, 1.0], "index": 1}], "model": "m"}"#)
            .create_async()
            .await;

        let provider = make_provider(&server.url());
        let texts = vec!["a".to_string(), "b".to_string()];
        let result = provider.embed_batch(&texts, execution()).await.unwrap();
        assert_eq!(result.len(), 2);
        mock.assert_async().await;
    }

    #[tokio::test]
    async fn test_openai_out_of_order_indices() {
        let mut server = mockito::Server::new_async().await;
        let mock = server
            .mock("POST", "/embeddings")
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(r#"{"data": [{"embedding": [0.0, 1.0], "index": 1}, {"embedding": [1.0, 0.0], "index": 0}], "model": "m"}"#)
            .create_async()
            .await;

        let provider = make_provider(&server.url());
        let texts = vec!["first".to_string(), "second".to_string()];
        let result = provider.embed_batch(&texts, execution()).await.unwrap();
        // After sorting by index, [0] should be [1.0, 0.0] and [1] should be [0.0, 1.0]
        assert!((result[0][0] - 1.0).abs() < 1e-5);
        assert!((result[1][1] - 1.0).abs() < 1e-5);
        mock.assert_async().await;
    }

    #[tokio::test]
    async fn test_openai_count_mismatch() {
        let mut server = mockito::Server::new_async().await;
        let mock = server
            .mock("POST", "/embeddings")
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(r#"{"data": [{"embedding": [1.0], "index": 0}], "model": "m"}"#)
            .create_async()
            .await;

        let provider = make_provider(&server.url());
        let texts = vec!["a".to_string(), "b".to_string()];
        let err = provider.embed_batch(&texts, execution()).await.unwrap_err();
        assert!(err.to_string().contains("1 embeddings for 2 inputs"));
        mock.assert_async().await;
    }

    #[tokio::test]
    async fn test_openai_normalization() {
        let mut server = mockito::Server::new_async().await;
        let mock = server
            .mock("POST", "/embeddings")
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(r#"{"data": [{"embedding": [3.0, 4.0], "index": 0}], "model": "m"}"#)
            .create_async()
            .await;

        let provider = make_provider(&server.url());
        let result = provider.embed_query("hello", execution()).await.unwrap();
        let mag: f32 = result.iter().map(|v| v * v).sum::<f32>().sqrt();
        assert!((mag - 1.0).abs() < 1e-5);
        mock.assert_async().await;
    }

    #[test]
    fn test_openai_metadata() {
        let provider = make_provider("http://localhost");
        assert_eq!(provider.id(), "openai");
        assert_eq!(provider.model(), "text-embedding-3-small");
        assert_eq!(provider.max_input_tokens(), Some(8192));
    }

    #[test]
    fn test_openai_known_max_tokens() {
        assert_eq!(known_max_tokens("text-embedding-3-small"), Some(8192));
        assert_eq!(known_max_tokens("text-embedding-3-large"), Some(8192));
        assert_eq!(known_max_tokens("text-embedding-ada-002"), Some(8191));
        assert_eq!(known_max_tokens("custom-model"), None);
    }

    #[tokio::test]
    async fn test_openai_bearer_auth_header() {
        let mut server = mockito::Server::new_async().await;
        let mock = server
            .mock("POST", "/embeddings")
            .match_header("authorization", "Bearer test-key")
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(r#"{"data": [{"embedding": [1.0], "index": 0}], "model": "m"}"#)
            .create_async()
            .await;

        let provider = make_provider(&server.url());
        provider.embed_query("test", execution()).await.unwrap();
        mock.assert_async().await;
    }
}
