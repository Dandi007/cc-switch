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

    /// Extract the API key from provider settings (same priority as ClaudeAdapter::extract_key).
    ///
    /// Used to attach `Authorization: Bearer <key>` to the `/v1/models` probe.
    /// Returns `None` if no key is configured — the probe will still be sent
    /// (some providers don't require auth for `/v1/models`).
    pub(crate) fn extract_api_key(provider: &Provider) -> Option<String> {
        // Helper: resolve ${VAR} references in the key string
        fn resolve_env_ref(value: &str) -> String {
            if let Some(var) = value.strip_prefix("${").and_then(|s| s.strip_suffix('}')) {
                std::env::var(var).unwrap_or_else(|_| value.to_string())
            } else {
                value.to_string()
            }
        }

        // 1. env.ANTHROPIC_AUTH_TOKEN (Bearer token — highest priority)
        if let Some(env) = provider.settings_config.get("env") {
            if let Some(key) = env
                .get("ANTHROPIC_AUTH_TOKEN")
                .and_then(|v| v.as_str())
                .map(str::trim)
                .filter(|s| !s.is_empty())
            {
                return Some(resolve_env_ref(key));
            }
        }
        // 2. env.ANTHROPIC_API_KEY (Anthropic x-api-key — also works as Bearer)
        if let Some(env) = provider.settings_config.get("env") {
            if let Some(key) = env
                .get("ANTHROPIC_API_KEY")
                .and_then(|v| v.as_str())
                .map(str::trim)
                .filter(|s| !s.is_empty())
            {
                return Some(resolve_env_ref(key));
            }
        }
        // 3. env.OPENROUTER_API_KEY
        if let Some(env) = provider.settings_config.get("env") {
            if let Some(key) = env
                .get("OPENROUTER_API_KEY")
                .and_then(|v| v.as_str())
                .map(str::trim)
                .filter(|s| !s.is_empty())
            {
                return Some(resolve_env_ref(key));
            }
        }
        // 4. env.GEMINI_API_KEY
        if let Some(env) = provider.settings_config.get("env") {
            if let Some(key) = env
                .get("GEMINI_API_KEY")
                .and_then(|v| v.as_str())
                .map(str::trim)
                .filter(|s| !s.is_empty())
            {
                return Some(resolve_env_ref(key));
            }
        }
        // 5. settings_config.apiKey / api_key (used by custom proxies like lingzhi)
        if let Some(key) = provider
            .settings_config
            .get("apiKey")
            .or_else(|| provider.settings_config.get("api_key"))
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            return Some(resolve_env_ref(key));
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
        let api_key = Self::extract_api_key(provider);
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
            let result = self.do_fetch(&base_url, api_key.as_deref(), &model_owned).await;

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
    ///
    /// If `api_key` is provided, it is sent as `Authorization: Bearer <key>`.
    /// Some providers (e.g. lingzhi) require auth for `/v1/models`; without it
    /// the endpoint returns 401 and the probe fails-safe → `false`.
    async fn do_fetch(&self, base_url: &Option<String>, api_key: Option<&str>, model: &str) -> bool {
        let base_url = match base_url {
            Some(url) => url.clone(),
            None => {
                log::debug!("[ModelCapability] No base_url available, skip");
                return false;
            }
        };

        let models_url = Self::build_models_url(&base_url);
        log::debug!("[ModelCapability] Fetching models from {models_url}");

        let mut request = self
            .client
            .get(&models_url)
            .timeout(Duration::from_secs(10));

        if let Some(key) = api_key {
            request = request.header("Authorization", format!("Bearer {key}"));
            log::debug!("[ModelCapability] Attaching Authorization: Bearer header");
        }

        let response = match request.send().await {
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

    #[test]
    fn extract_api_key_from_apikey_field() {
        let provider = Provider {
            id: "test".to_string(),
            name: "Test".to_string(),
            settings_config: serde_json::json!({
                "apiKey": "sk-test-key-123",
                "base_url": "https://lingzhi.agibot.com/v1"
            }),
            ..default_provider()
        };
        let key = CachedModelCapabilityResolver::extract_api_key(&provider);
        assert_eq!(key.as_deref(), Some("sk-test-key-123"));
    }

    #[test]
    fn extract_api_key_from_api_key_field() {
        let provider = Provider {
            id: "test".to_string(),
            name: "Test".to_string(),
            settings_config: serde_json::json!({
                "api_key": "sk-underscore-key",
                "base_url": "https://api.example.com"
            }),
            ..default_provider()
        };
        let key = CachedModelCapabilityResolver::extract_api_key(&provider);
        assert_eq!(key.as_deref(), Some("sk-underscore-key"));
    }

    #[test]
    fn extract_api_key_from_env_anthropic_auth_token() {
        let provider = Provider {
            id: "test".to_string(),
            name: "Test".to_string(),
            settings_config: serde_json::json!({
                "env": {
                    "ANTHROPIC_AUTH_TOKEN": "sk-ant-auth-token",
                    "ANTHROPIC_BASE_URL": "https://api.anthropic.com"
                }
            }),
            ..default_provider()
        };
        let key = CachedModelCapabilityResolver::extract_api_key(&provider);
        assert_eq!(key.as_deref(), Some("sk-ant-auth-token"));
    }

    #[test]
    fn extract_api_key_from_env_anthropic_api_key() {
        let provider = Provider {
            id: "test".to_string(),
            name: "Test".to_string(),
            settings_config: serde_json::json!({
                "env": {
                    "ANTHROPIC_API_KEY": "sk-ant-api-key",
                    "ANTHROPIC_BASE_URL": "https://api.anthropic.com"
                }
            }),
            ..default_provider()
        };
        let key = CachedModelCapabilityResolver::extract_api_key(&provider);
        assert_eq!(key.as_deref(), Some("sk-ant-api-key"));
    }

    #[test]
    fn extract_api_key_returns_none_when_no_key() {
        let provider = Provider {
            id: "test".to_string(),
            name: "Test".to_string(),
            settings_config: serde_json::json!({
                "base_url": "https://api.example.com"
            }),
            ..default_provider()
        };
        let key = CachedModelCapabilityResolver::extract_api_key(&provider);
        assert!(key.is_none());
    }

    /// Verify that `do_fetch` attaches the Authorization header when an API key is provided.
    ///
    /// This test uses a local TCP listener to inspect the raw HTTP request without
    /// needing a live provider. It verifies that the `Authorization: Bearer <key>`
    /// header is present in the outgoing request.
    #[tokio::test]
    async fn do_fetch_sends_authorization_header() {
        use std::io::{BufRead, BufReader, Read, Write};
        use std::net::TcpListener;
        use std::thread;

        // Start a tiny HTTP server that captures the first request line + headers
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let port = addr.port();

        let server_handle = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut reader = BufReader::new(stream.try_clone().unwrap());

            // Read request line and headers
            let mut headers = Vec::new();
            for line in reader.by_ref().lines() {
                let line = line.unwrap();
                if line.is_empty() || line == "\r" {
                    break;
                }
                headers.push(line);
            }

            // Send a minimal JSON response
            let response = serde_json::json!({
                "data": [
                    {
                        "id": "claude-opus-4-8",
                        "supported_endpoint_types": ["anthropic", "openai"]
                    }
                ]
            });
            let body = serde_json::to_string(&response).unwrap();
            write!(
                stream,
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{}",
                body.len(),
                body
            )
            .unwrap();

            headers
        });

        let resolver = CachedModelCapabilityResolver::new();
        let base_url = Some(format!("http://127.0.0.1:{port}"));
        let api_key = Some("sk-test-auth-key");

        let result = resolver
            .do_fetch(&base_url, api_key, "claude-opus-4-8")
            .await;

        assert!(result, "should find claude-opus-4-8 with anthropic support");

        let captured_headers = server_handle.join().unwrap();

        // Verify the Authorization header was sent
        let auth_header = captured_headers
            .iter()
            .find(|h| h.to_lowercase().starts_with("authorization:"));
        assert!(
            auth_header.is_some(),
            "Expected Authorization header in request, got headers: {captured_headers:?}"
        );
        assert!(
            auth_header.unwrap().contains("Bearer sk-test-auth-key"),
            "Authorization header should contain 'Bearer sk-test-auth-key', got: {}",
            auth_header.unwrap()
        );
    }

    /// Helper: create a minimal Provider with default fields.
    fn default_provider() -> Provider {
        Provider {
            id: String::new(),
            name: String::new(),
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
        }
    }
}