//! Provider/model validation for launch grants and launch preflight.

use std::collections::HashMap;
use std::sync::LazyLock;
use std::time::{Duration, Instant};

use rsi_common::types::SessionProvider;
use tokio::sync::Mutex;

use super::SessionManager;
use crate::error::{DaemonError, Result};

impl SessionManager {
    pub(crate) async fn validate_manager_launch_models(
        &self,
        launches: &[rsi_common::harness_manager_v2::ManagerLaunchChoiceV2],
    ) -> Result<()> {
        for launch in launches {
            validate(self, launch.provider, &launch.model).await?;
        }
        Ok(())
    }

    pub(crate) async fn preflight_manager_action_model(
        &self,
        provider: SessionProvider,
        model: &str,
    ) -> Result<()> {
        match validate(self, provider, model).await {
            Err(error) if error.to_string().contains("provider_model_unknown") => {
                Err(DaemonError::PolicyDenied(error.to_string()))
            }
            result => result,
        }
    }

    async fn discover_provider_models(
        &self,
        provider: SessionProvider,
    ) -> Result<Vec<(String, String)>> {
        match provider {
            SessionProvider::Claude => {
                let client = crate::claude::ClaudeClient::for_discovery()?;
                client.discover_models().await
            }
            SessionProvider::Codex | SessionProvider::CodexAppServer => {
                self.discover_codex_models(
                    crate::provider_capabilities::CatalogRefreshReason::ExplicitDiscovery,
                )
                .await
            }
            SessionProvider::Pioneer => crate::pioneer::discover_pioneer_models()
                .await
                .map_err(|error| DaemonError::Process(error.to_string())),
            SessionProvider::OpenRouter => crate::openrouter::discover_openrouter_models()
                .await
                .map_err(|error| DaemonError::Process(error.to_string())),
            SessionProvider::Bedrock => crate::bedrock::discover_models()
                .await
                .map_err(DaemonError::Process),
            SessionProvider::Local => Ok(crate::openai::OpenAiClient::new_local()?
                .discover_models()
                .await),
            SessionProvider::Antigravity => {
                Ok(crate::agy::AgyClient::new()?.discover_models().await)
            }
            SessionProvider::Harness => Ok(super::harness::models::harness_models()),
            _ => Err(DaemonError::InvalidParam(
                "provider_model_catalog_unsupported".into(),
            )),
        }
    }
}

const CACHE_TTL: Duration = Duration::from_secs(5 * 60);

#[derive(Clone)]
struct CachedValidation {
    checked_at: Instant,
    nearest: Option<String>,
}

#[derive(Clone)]
struct CachedCatalog {
    checked_at: Instant,
    ids: Vec<String>,
}

static VALIDATIONS: LazyLock<Mutex<HashMap<(SessionProvider, String), CachedValidation>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));
static CATALOGS: LazyLock<Mutex<HashMap<SessionProvider, CachedCatalog>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

pub(super) async fn validate(
    manager: &SessionManager,
    provider: SessionProvider,
    model: &str,
) -> Result<()> {
    // The scripted test provider is not in any real catalog; the manager-action
    // fixtures launch it by name (#1506 made that launch path validate models).
    #[cfg(test)]
    if model == "manager-scripted-provider" {
        return Ok(());
    }
    let key = (provider, model.to_owned());
    if let Some(cached) = VALIDATIONS.lock().await.get(&key).cloned()
        && cached.checked_at.elapsed() < CACHE_TTL
    {
        return match cached.nearest {
            None => Ok(()),
            Some(nearest) => Err(unknown_model(provider, model, Some(&nearest))),
        };
    }

    let cached_catalog = CATALOGS.lock().await.get(&provider).cloned();
    let ids = if let Some(cached) =
        cached_catalog.filter(|cached| cached.checked_at.elapsed() < CACHE_TTL)
    {
        cached.ids
    } else {
        let catalog = match manager.discover_provider_models(provider).await {
            Ok(catalog) => catalog,
            Err(error) => {
                // A missing or temporarily unreadable catalog is not evidence
                // that a configured model is invalid. Admit this launch/save,
                // and leave both caches untouched so a later attempt retries.
                tracing::warn!(
                    provider = ?provider,
                    error = %error,
                    "provider model catalog unavailable; allowing manager launch choice"
                );
                return Ok(());
            }
        };
        let ids: Vec<String> = catalog.into_iter().map(|(id, _)| id).collect();
        CATALOGS.lock().await.insert(
            provider,
            CachedCatalog {
                checked_at: Instant::now(),
                ids: ids.clone(),
            },
        );
        ids
    };
    let refs: Vec<&str> = ids.iter().map(String::as_str).collect();
    let nearest = if model_is_in_catalog(provider, model, &refs) {
        None
    } else {
        closest_model(model, &refs).map(str::to_owned)
    };
    VALIDATIONS.lock().await.insert(
        key,
        CachedValidation {
            checked_at: Instant::now(),
            nearest: nearest.clone(),
        },
    );
    match nearest {
        None => Ok(()),
        Some(nearest) => Err(unknown_model(provider, model, Some(&nearest))),
    }
}

fn model_is_in_catalog(provider: SessionProvider, model: &str, catalog: &[&str]) -> bool {
    if catalog.contains(&model) {
        return true;
    }
    match provider {
        SessionProvider::Claude => {
            let base_model = model.strip_suffix("[1m]").unwrap_or(model);
            catalog.contains(&base_model)
                || rsi_common::claude_catalog::CLAUDE_ONE_MILLION_VARIANT_LAUNCH_MODELS
                    .iter()
                    .any(|alias| *alias == base_model)
        }
        _ => false,
    }
}

fn unknown_model(provider: SessionProvider, model: &str, nearest: Option<&str>) -> DaemonError {
    let suggestion = nearest
        .map(|model| format!("; nearest match: {model}"))
        .unwrap_or_default();
    DaemonError::InvalidParam(format!(
        "provider_model_unknown: {provider:?}/{model}{suggestion}"
    ))
}

fn closest_model<'a>(model: &str, catalog: &[&'a str]) -> Option<&'a str> {
    catalog
        .iter()
        .copied()
        .map(|candidate| {
            (
                edit_distance(
                    &without_numeric_catalog_suffix(model),
                    &without_numeric_catalog_suffix(candidate),
                ),
                candidate,
            )
        })
        .min_by_key(|(distance, candidate)| (*distance, candidate.len()))
        .map(|(_, candidate)| candidate)
}

fn without_numeric_catalog_suffix(model: &str) -> String {
    let Some((prefix, suffix)) = model.rsplit_once('-') else {
        return model.to_owned();
    };
    if suffix.len() >= 6 && suffix.bytes().all(|byte| byte.is_ascii_digit()) {
        prefix.to_owned()
    } else {
        model.to_owned()
    }
}

fn edit_distance(left: &str, right: &str) -> usize {
    let mut row: Vec<usize> = (0..=right.chars().count()).collect();
    for (left_index, left_char) in left.chars().enumerate() {
        let mut diagonal = row[0];
        row[0] = left_index + 1;
        for (right_index, right_char) in right.chars().enumerate() {
            let above = row[right_index + 1];
            row[right_index + 1] = (row[right_index + 1] + 1)
                .min(row[right_index] + 1)
                .min(diagonal + usize::from(left_char != right_char));
            diagonal = above;
        }
    }
    *row.last().unwrap_or(&usize::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-01"))]
    #[test]
    fn closest_model_suggests_nearby_catalog_id() {
        assert_eq!(
            closest_model(
                "claude-haiku-5-5",
                &["claude-haiku-4-5-20251001", "claude-opus-5-5"]
            ),
            Some("claude-haiku-4-5-20251001")
        );
        assert_eq!(
            closest_model("totally-unrelated", &["gpt-6-sol"]),
            Some("gpt-6-sol")
        );
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-01"))]
    #[test]
    fn real_provider_catalogs_accept_current_manager_models_and_suggest_unknowns() {
        let claude_catalog: Vec<&str> = rsi_common::claude_catalog::CLAUDE_MODEL_MENU
            .iter()
            .map(|(id, _)| *id)
            .collect();
        assert!(model_is_in_catalog(
            SessionProvider::Claude,
            "claude-opus-5-5",
            &claude_catalog
        ));
        // Sonnet 5.5 is a CLI-supported [1m] launch alias, although the
        // compiled picker catalog does not list it as a separate SKU.
        assert!(model_is_in_catalog(
            SessionProvider::Claude,
            "claude-sonnet-5-5",
            &claude_catalog
        ));

        let codex_fixture = crate::codex::parse_codex_model_catalog(
            std::str::from_utf8(include_bytes!(
                "../../tests/fixtures/codex-models-0.155.1.json"
            ))
            .expect("Codex fixture is UTF-8"),
        )
        .expect("Codex fixture parses");
        let mut codex_ids: Vec<String> = crate::codex::codex_fallback_models()
            .into_iter()
            .map(|(id, _)| id)
            .chain(codex_fixture.into_iter().map(|(id, _)| id))
            .collect();
        codex_ids.sort();
        codex_ids.dedup();
        let codex_catalog: Vec<&str> = codex_ids.iter().map(String::as_str).collect();
        for model in ["gpt-6.1-sol", "gpt-6-luna", "gpt-6-astra"] {
            assert!(
                model_is_in_catalog(SessionProvider::Codex, model, &codex_catalog),
                "{model} missing from {codex_catalog:?}"
            );
        }

        let bad = "claude-haiku-5-5";
        let suggestion = closest_model(bad, &claude_catalog).expect("nearest Claude model");
        assert_eq!(suggestion, "claude-haiku-4-5-20251001");
        assert!(!model_is_in_catalog(
            SessionProvider::Claude,
            bad,
            &claude_catalog
        ));
        assert!(
            unknown_model(SessionProvider::Claude, bad, Some(suggestion))
                .to_string()
                .contains("nearest match: claude-haiku-4-5-20251001")
        );
    }
}
