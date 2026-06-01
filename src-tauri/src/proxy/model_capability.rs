//! Per-model capability resolver
//!
//! Queries a provider's `/v1/models` (or `/models`) endpoint to determine whether
//! a specific model supports the Anthropic-native format (`supported_endpoint_types`
//! contains `"anthropic"`).
//!
//! Results are cached with a ~5 min TTL to avoid adding a fixed HTTP round-trip
//! to every request. On cache miss or cache expiry, one HTTP request is issued per
//! (provider, model) pair — subsequent lookups for the same pair within the TTL
//! are served from cache.

use crate::provider::Provider;
use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::RwLock;

/// Trait for querying whether a model supports the Anthropic-native format.
///
/// This is injectable so that tests can provide pre-configured answers without
/// hitting the network. The production implementation is `CachedModelCapabilityResolver`.
pub trait ModelCapabilityResolver: Send + Sync {
    fn supports_anthropic(
        &self,
        provider: &Provider,
        model: &str,
    ) -> Pin<Box<dyn Future<Output = bool> + Send + '_>>;
}

/// Simple mock resolver for tests — returns a fixed answer for every model.
pub struct FixedModelCapabilityResolver {
    /// Map from model name → supports_anthropic.
    pub answers: HashMap<String, bool>,
    /// Default answer when model is not in the map.
    pub default: bool,
}

impl ModelCapabilityResolver for FixedModelCapabilityResolver {
    fn supports_anthropic(
        &self,
        _provider: &Provider,
        model: &str,
    ) -> Pin<Box<dyn Future<Output = bool> + Send + '_>> {
        let result = self.answers.get(model).copied().unwrap_or(self.default);
        Box::pin(std::future::ready(result))
    }
}

/// Production resolver: fetches `/v1/models` (or `/models`) from the provider,
/// parses `supported_endpoint_types`, and caches with a TTL.
///
/// **Fail-safe**: any error (network, timeout, parse, missing field) → returns
/// `false`, so the caller falls back to the provider-level base format.
///
/// Wrap in `Arc<CachedModelCapabilityResolver>` and pass as `Arc<dyn ModelCapabilityResolver>`
/// for shared access across multiple forwarders.
pub struct CachedModelCapabilityResolver {
    /// Cache key: `"{provider_id}:{model}"` → `(inserted_at, supports_anthropic)`.
    cache: RwLock<HashMap<String, (Instant, bool)>>,
    client: reqwest::Client,
}

impl CachedModelCapabilityResolver {
    /// 5-minute TTL, matching the copilot live-model cache semantics.
    const DEFAULT_TTL: Duration = Duration::from_secs(300);

    pub fn new() -> Self {
        Self {
            cache: RwLock::new(HashMap::new()),
            client: reqwest::Client::new(),
        }
    }

    /// Build the models URL from the provider's base URL.
    ///
    /// Tries the standard OpenAI-compatible `/v1/models` path. If the base URL
    /// already ends with `/v1`, appends `/models` instead to avoid double `/v1`.
    fn build_models_url(base_url: &str) -> String {
        let base = base_url.trim_end_matches('/');
        if base.ends_with("/v1") {
            format!("{base}/models")
        } else {
            format!("{base}/v1/models")
        }
    }

    /// Extract the base URL from provider settings (same priority as ClaudeAdapter).
    pub(crate) fn extract_base_url(provider: &Provider) -> Option<String> {
        // 1. env.ANTHROPIC_BASE_URL
        if let Some(env) = provider.settings_config.get("env") {
            if let Some(url) = env.get("ANTHROPIC_BASE_URL").and_then(|v| v.as_str()) {
                return Some(url.trim_end_matches('/').to_string());
            }
        }
        // 2. base_url
        if let Some(url) = provider
            .settings_config
            .get("base_url")
            .and_then(|v| v.as_str())
        {
            return Some(url.trim_end_matches('/').to_string());
        }
        // 3. baseURL
        if let Some(url) = provider
            .settings_config
            .get("baseURL")
            .and_then(|v| v.as_str())
        {
            return Some(url.trim_end_matches('/').to_string());
        }
        // 4. apiEndpoint
        if let Some(url) = provider
            .settings_config
            .get("apiEndpoint")
            .and_then(|v| v.as_str())
        {
            return Some(url.trim_end_matches('/').to_string());
        }
        None
    }
}

impl ModelCapabilityResolver for CachedModelCapabilityResolver {
    fn supports_anthropic(
        &self,
        provider: &Provider,
        model: &str,
    ) -> Pin<Box<dyn Future<Output = bool> + Send + '_>> {
        let cache_key = format!("{}:{}", provider.id, model);
        let base_url = Self::extract_base_url(provider);
        let model_owned = model.to_string();

        Box::pin(async move {
            // Check cache
            {
                let cache = self.cache.read().await;
                if let Some((ts, result)) = cache.get(&cache_key) {
                    if ts.elapsed() < Self::DEFAULT_TTL {
                        return *result;
                    }
                }
            }

            // Cache miss — fetch from network
            let result = self.do_fetch(&base_url, &model_owned).await;

            // Update cache
            {
                let mut cache = self.cache.write().await;
                cache.insert(cache_key, (Instant::now(), result));
            }

            result
        })
    }
}

impl CachedModelCapabilityResolver {
    /// Fetch models from the provider's `/v1/models` endpoint and check if the
    /// given model has `"anthropic"` in its `supported_endpoint_types`.
    async fn do_fetch(&self, base_url: &Option<String>, model: &str) -> bool {
        let base_url = match base_url {
            Some(url) => url.clone(),
            None => {
                log::debug!("[ModelCapability] No base_url available, skip");
                return false;
            }
        };

        let models_url = Self::build_models_url(&base_url);
        log::debug!("[ModelCapability] Fetching models from {models_url}");

        let response = match self
            .client
            .get(&models_url)
            .timeout(Duration::from_secs(10))
            .send()
            .await
        {
            Ok(r) => r,
            Err(e) => {
                log::debug!("[ModelCapability] Failed to fetch models from {models_url}: {e}");
                return false;
            }
        };

        if !response.status().is_success() {
            log::debug!(
                "[ModelCapability] Models endpoint returned {} for {models_url}",
                response.status()
            );
            return false;
        }

        let body: serde_json::Value = match response.json().await {
            Ok(v) => v,
            Err(e) => {
                log::debug!("[ModelCapability] Failed to parse models response: {e}");
                return false;
            }
        };

        // Supports both OpenAI-compatible `{"data": [...]}` and bare `[...]` formats.
        let models = body
            .get("data")
            .and_then(|d| d.as_array())
            .or_else(|| body.as_array());

        let models = match models {
            Some(arr) => arr,
            None => {
                log::debug!("[ModelCapability] Unexpected models response shape");
                return false;
            }
        };

        for entry in models {
            let entry_id = entry.get("id").and_then(|v| v.as_str()).unwrap_or("");
            if entry_id == model {
                let has_anthropic = entry
                    .get("supported_endpoint_types")
                    .and_then(|v| v.as_array())
                    .map(|arr| {
                        arr.iter().any(|t| {
                            t.as_str()
                                .is_some_and(|s| s.eq_ignore_ascii_case("anthropic"))
                        })
                    })
                    .unwrap_or(false);

                log::debug!("[ModelCapability] Model {model} supports_anthropic={has_anthropic}");
                return has_anthropic;
            }
        }

        log::debug!("[ModelCapability] Model {model} not found in {models_url} response");
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Test that FixedModelCapabilityResolver returns configured answers.
    #[tokio::test]
    async fn fixed_resolver_returns_configured_answers() {
        let resolver = FixedModelCapabilityResolver {
            answers: HashMap::from([
                ("claude-opus-4-8".to_string(), true),
                ("deepseek-v4-pro".to_string(), false),
            ]),
            default: false,
        };

        let provider = Provider {
            id: "test".to_string(),
            name: "Test".to_string(),
            settings_config: serde_json::json!({}),
            website_url: None,
            category: None,
            created_at: None,
            sort_index: None,
            notes: None,
            meta: None,
            icon: None,
            icon_color: None,
            in_failover_queue: false,
        };

        assert!(resolver.supports_anthropic(&provider, "claude-opus-4-8").await);
        assert!(!resolver.supports_anthropic(&provider, "deepseek-v4-pro").await);
        // Unknown model falls back to default
        assert!(!resolver.supports_anthropic(&provider, "unknown-model").await);
    }

    #[test]
    fn build_models_url_handles_v1_suffix() {
        assert_eq!(
            CachedModelCapabilityResolver::build_models_url("https://api.example.com"),
            "https://api.example.com/v1/models"
        );
        assert_eq!(
            CachedModelCapabilityResolver::build_models_url("https://api.example.com/v1"),
            "https://api.example.com/v1/models"
        );
        assert_eq!(
            CachedModelCapabilityResolver::build_models_url("https://api.example.com/v1/"),
            "https://api.example.com/v1/models"
        );
    }
}