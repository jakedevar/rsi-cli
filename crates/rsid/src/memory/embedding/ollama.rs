use serde::{Deserialize, Serialize};

use crate::error::{DaemonError, Result};
use crate::memory::embedding::l2_normalize;
use crate::memory::types::EmbeddingProvider;
use crate::model_control::AdmittedEmbeddingExecution;

pub struct OllamaEmbeddingProvider {
    http: reqwest::Client,
    base_url: String,
    model: String,
}

impl OllamaEmbeddingProvider {
    pub fn new(http: reqwest::Client, base_url: String, model: String) -> Self {
        Self {
            http,
            base_url,
            model,
        }
    }

    async fn embed_single(
        &self,
        text: &str,
        execution: AdmittedEmbeddingExecution,
    ) -> Result<Vec<f32>> {
        let url = format!("{}/api/embed", self.base_url);
        let body = OllamaEmbedRequest {
            model: &self.model,
            input: OllamaInput::Single(text),
        };

        let request = self.http.post(&url).json(&body);
        let resp = execution
            .bind_http(
                crate::model_control::registry::RuntimeExecutionRoute::OllamaEmbeddingQueryHttp,
                request,
            )
            .send("Ollama embeddings")
            .await
            .map_err(|e| DaemonError::Process(format!("Ollama request failed: {e}")))?;

        if !resp.status().is_success() {
            let status = resp.status();
            let body_text = resp.text().await.unwrap_or_default();
            return Err(DaemonError::Process(format!(
                "Ollama returned {} — {}",
                status, body_text
            )));
        }

        let result: OllamaEmbedResponse = resp
            .json()
            .await
            .map_err(|e| DaemonError::Process(format!("Ollama JSON parse failed: {}", e)))?;

        let mut vec =
            result.embeddings.into_iter().next().ok_or_else(|| {
                DaemonError::Process("Ollama returned empty embeddings".to_string())
            })?;

        l2_normalize(&mut vec);
        Ok(vec)
    }

    async fn embed_multiple(
        &self,
        texts: &[String],
        execution: AdmittedEmbeddingExecution,
    ) -> Result<Vec<Vec<f32>>> {
        let url = format!("{}/api/embed", self.base_url);
        let body = OllamaEmbedRequest {
            model: &self.model,
            input: OllamaInput::Batch(texts),
        };

        let request = self.http.post(&url).json(&body);
        let resp = execution
            .bind_http(
                crate::model_control::registry::RuntimeExecutionRoute::OllamaEmbeddingBatchHttp,
                request,
            )
            .send("Ollama embeddings")
            .await
            .map_err(|e| DaemonError::Process(format!("Ollama request failed: {e}")))?;

        if !resp.status().is_success() {
            let status = resp.status();
            let body_text = resp.text().await.unwrap_or_default();
            return Err(DaemonError::Process(format!(
                "Ollama returned {} — {}",
                status, body_text
            )));
        }

        let result: OllamaEmbedResponse = resp
            .json()
            .await
            .map_err(|e| DaemonError::Process(format!("Ollama JSON parse failed: {}", e)))?;

        if result.embeddings.len() != texts.len() {
            return Err(DaemonError::Process(format!(
                "Ollama returned {} embeddings for {} inputs",
                result.embeddings.len(),
                texts.len()
            )));
        }

        let mut vecs = result.embeddings;
        for v in &mut vecs {
            l2_normalize(v);
        }
        Ok(vecs)
    }
}

#[async_trait::async_trait]
impl EmbeddingProvider for OllamaEmbeddingProvider {
    fn id(&self) -> &str {
        "ollama"
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
        execution: AdmittedEmbeddingExecution,
    ) -> Result<Vec<f32>> {
        self.embed_single(text, execution).await
    }

    async fn embed_batch(
        &self,
        texts: &[String],
        execution: AdmittedEmbeddingExecution,
    ) -> Result<Vec<Vec<f32>>> {
        self.embed_multiple(texts, execution).await
    }
}

// --- Serde types ---

#[derive(Serialize)]
struct OllamaEmbedRequest<'a> {
    model: &'a str,
    input: OllamaInput<'a>,
}

#[derive(Serialize)]
#[serde(untagged)]
enum OllamaInput<'a> {
    Single(&'a str),
    Batch(&'a [String]),
}

#[derive(Deserialize)]
struct OllamaEmbedResponse {
    embeddings: Vec<Vec<f32>>,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn query_execution() -> AdmittedEmbeddingExecution {
        AdmittedEmbeddingExecution::for_test(
            crate::model_control::registry::RuntimeExecutionRoute::OllamaEmbeddingQueryHttp,
        )
    }

    fn batch_execution() -> AdmittedEmbeddingExecution {
        AdmittedEmbeddingExecution::for_test(
            crate::model_control::registry::RuntimeExecutionRoute::OllamaEmbeddingBatchHttp,
        )
    }

    #[tokio::test]
    async fn test_ollama_successful_single_embed() {
        let mut server = mockito::Server::new_async().await;
        let mock = server
            .mock("POST", "/api/embed")
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(r#"{"embeddings": [[1.0, 0.0, 0.0]]}"#)
            .create_async()
            .await;

        let provider = OllamaEmbeddingProvider::new(
            reqwest::Client::new(),
            server.url(),
            "nomic-embed-text".to_string(),
        );

        let result = provider
            .embed_query("hello", query_execution())
            .await
            .unwrap();
        assert_eq!(result.len(), 3);
        // Should be L2 normalized
        let mag: f32 = result.iter().map(|v| v * v).sum::<f32>().sqrt();
        assert!((mag - 1.0).abs() < 1e-5);
        mock.assert_async().await;
    }

    #[tokio::test]
    async fn test_ollama_successful_batch_embed() {
        let mut server = mockito::Server::new_async().await;
        let mock = server
            .mock("POST", "/api/embed")
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(r#"{"embeddings": [[1.0, 0.0], [0.0, 1.0]]}"#)
            .create_async()
            .await;

        let provider = OllamaEmbeddingProvider::new(
            reqwest::Client::new(),
            server.url(),
            "nomic-embed-text".to_string(),
        );

        let texts = vec!["hello".to_string(), "world".to_string()];
        let result = provider
            .embed_batch(&texts, batch_execution())
            .await
            .unwrap();
        assert_eq!(result.len(), 2);
        mock.assert_async().await;
    }

    #[tokio::test]
    async fn test_ollama_server_error() {
        let mut server = mockito::Server::new_async().await;
        let mock = server
            .mock("POST", "/api/embed")
            .with_status(500)
            .with_body("internal error")
            .create_async()
            .await;

        let provider = OllamaEmbeddingProvider::new(
            reqwest::Client::new(),
            server.url(),
            "nomic-embed-text".to_string(),
        );

        let err = provider
            .embed_query("hello", query_execution())
            .await
            .unwrap_err();
        assert!(err.to_string().contains("500"));
        mock.assert_async().await;
    }

    #[tokio::test]
    async fn test_ollama_empty_response() {
        let mut server = mockito::Server::new_async().await;
        let mock = server
            .mock("POST", "/api/embed")
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(r#"{"embeddings": []}"#)
            .create_async()
            .await;

        let provider = OllamaEmbeddingProvider::new(
            reqwest::Client::new(),
            server.url(),
            "nomic-embed-text".to_string(),
        );

        let err = provider
            .embed_query("hello", query_execution())
            .await
            .unwrap_err();
        assert!(err.to_string().contains("empty"));
        mock.assert_async().await;
    }

    #[tokio::test]
    async fn test_ollama_malformed_json() {
        let mut server = mockito::Server::new_async().await;
        let mock = server
            .mock("POST", "/api/embed")
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body("not json at all")
            .create_async()
            .await;

        let provider = OllamaEmbeddingProvider::new(
            reqwest::Client::new(),
            server.url(),
            "nomic-embed-text".to_string(),
        );

        let err = provider
            .embed_query("hello", query_execution())
            .await
            .unwrap_err();
        assert!(err.to_string().contains("parse"));
        mock.assert_async().await;
    }

    #[tokio::test]
    async fn test_ollama_count_mismatch() {
        let mut server = mockito::Server::new_async().await;
        let mock = server
            .mock("POST", "/api/embed")
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(r#"{"embeddings": [[1.0]]}"#)
            .create_async()
            .await;

        let provider = OllamaEmbeddingProvider::new(
            reqwest::Client::new(),
            server.url(),
            "nomic-embed-text".to_string(),
        );

        let texts = vec!["a".to_string(), "b".to_string()];
        let err = provider
            .embed_batch(&texts, batch_execution())
            .await
            .unwrap_err();
        assert!(err.to_string().contains("1 embeddings for 2 inputs"));
        mock.assert_async().await;
    }

    #[tokio::test]
    async fn test_ollama_normalization_check() {
        let mut server = mockito::Server::new_async().await;
        let mock = server
            .mock("POST", "/api/embed")
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(r#"{"embeddings": [[3.0, 4.0]]}"#)
            .create_async()
            .await;

        let provider = OllamaEmbeddingProvider::new(
            reqwest::Client::new(),
            server.url(),
            "nomic-embed-text".to_string(),
        );

        let result = provider
            .embed_query("hello", query_execution())
            .await
            .unwrap();
        let mag: f32 = result.iter().map(|v| v * v).sum::<f32>().sqrt();
        assert!((mag - 1.0).abs() < 1e-5);
        mock.assert_async().await;
    }

    #[test]
    fn test_ollama_id_model() {
        let provider = OllamaEmbeddingProvider::new(
            reqwest::Client::new(),
            "http://localhost:11434".to_string(),
            "nomic-embed-text".to_string(),
        );
        assert_eq!(provider.id(), "ollama");
        assert_eq!(provider.model(), "nomic-embed-text");
        assert_eq!(provider.max_input_tokens(), None);
    }

    #[tokio::test]
    async fn test_ollama_connection_refused() {
        let provider = OllamaEmbeddingProvider::new(
            reqwest::Client::new(),
            "http://127.0.0.1:1".to_string(), // guaranteed-unreachable
            "nomic-embed-text".to_string(),
        );
        let err = provider
            .embed_query("hello", query_execution())
            .await
            .unwrap_err();
        assert!(err.to_string().contains("request failed"));
    }
}
