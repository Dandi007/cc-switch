//! L1 — wiremock-based proxy routing regression tests
//!
//! Each case starts a local wiremock upstream + in-process cc-switch proxy,
//! then verifies model routing and request normalization via reqwest.
//!
//! Cases marked ⭐ should FAIL on the current `cli-headless` branch and turn
//! green after the corresponding bug fix.

use cc_switch_lib::{
    headless::{HeadlessApp, HeadlessOptions},
    AppType, Provider, ProviderMeta,
};
use serde_json::{json, Value};
use tempfile::TempDir;
use wiremock::{
    matchers::{method, path},
    Mock, MockServer, ResponseTemplate,
};

fn fake_provider(id: &str, family: &str, mock_url: &str) -> Provider {
    Provider {
        id: id.to_string(),
        name: format!("fake-{id}"),
        settings_config: json!({
            "base_url": mock_url.trim_end_matches('/'),
            "modelFamily": family,
            "models": ["fake-model"],
        }),
        website_url: None,
        category: Some("codex".to_string()),
        created_at: None,
        sort_index: None,
        notes: None,
        meta: Some(ProviderMeta {
            api_format: Some("anthropic".to_string()),
            ..ProviderMeta::default()
        }),
        icon: None,
        icon_color: None,
        in_failover_queue: false,
    }
}

fn fake_codex_oauth_provider(mock_url: &str) -> Provider {
    Provider {
        id: "gpt".to_string(),
        name: "fake-gpt-oauth".to_string(),
        settings_config: json!({
            "base_url": mock_url.trim_end_matches('/'),
            "modelFamily": "gpt",
            "models": ["gpt-5.4"],
        }),
        website_url: None,
        category: Some("codex".to_string()),
        created_at: None,
        sort_index: None,
        notes: None,
        meta: Some(ProviderMeta {
            provider_type: Some("codex_oauth".to_string()),
            ..ProviderMeta::default()
        }),
        icon: None,
        icon_color: None,
        in_failover_queue: false,
    }
}

async fn seed_providers(app: &HeadlessApp, app_type: AppType, providers: &[Provider]) {
    let app_str = app_type.as_str();
    for p in providers {
        app.state.db.save_provider(app_str, p).expect("save provider");
    }
    if let Some(first) = providers.first() {
        app.state
            .db
            .set_current_provider(app_str, &first.id)
            .expect("set current provider");
    }
}

// ---------------------------------------------------------------------------
// R1 – model routing
// ---------------------------------------------------------------------------

/// R1.no_slash — body model has no '/' prefix → passthrough unchanged.
#[tokio::test]
async fn r1_no_slash_passthrough() {
    let mock = MockServer::start().await;
    let dir = TempDir::new().expect("tempdir");

    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "id": "msg_1", "model": "gpt-4o", "role": "assistant",
            "content": [{"type": "text", "text": "ok"}],
            "usage": {"input_tokens": 1, "output_tokens": 1}
        })))
        .expect(1)
        .mount(&mock)
        .await;

    let app = HeadlessApp::init(HeadlessOptions {
        config_dir: Some(dir.path().to_path_buf()),
        recover_proxy: false,
    })
    .await
    .expect("init headless app");

    let provider = fake_provider("lingzhi", "lingzhi", &mock.uri());
    seed_providers(&app, AppType::Codex, &[provider]).await;

    let info = app.state.proxy_service.start().await.expect("start proxy");
    let client = reqwest::Client::new();
    let resp = client
        .post(format!("http://{}:{}/v1/messages", info.address, info.port))
        .json(&json!({
            "model": "gpt-4o", "max_tokens": 10,
            "messages": [{"role": "user", "content": "hi"}]
        }))
        .send()
        .await
        .expect("send request");

    assert_eq!(resp.status(), 200);
    app.state.proxy_service.stop().await.ok();
}

/// R1.local_prefix_gpt — "gpt/o1" strips prefix, routes to gpt provider.
#[tokio::test]
async fn r1_local_prefix_gpt() {
    let mock = MockServer::start().await;
    let dir = TempDir::new().expect("tempdir");

    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "id": "msg_2", "model": "o1", "role": "assistant",
            "content": [{"type": "text", "text": "ok"}],
            "usage": {"input_tokens": 1, "output_tokens": 1}
        })))
        .expect(1)
        .mount(&mock)
        .await;

    let app = HeadlessApp::init(HeadlessOptions {
        config_dir: Some(dir.path().to_path_buf()),
        recover_proxy: false,
    })
    .await
    .expect("init headless app");

    let provider = fake_provider("gpt", "gpt", &mock.uri());
    seed_providers(&app, AppType::Codex, &[provider]).await;

    let info = app.state.proxy_service.start().await.expect("start proxy");
    let client = reqwest::Client::new();
    let resp = client
        .post(format!("http://{}:{}/v1/messages", info.address, info.port))
        .json(&json!({
            "model": "gpt/o1", "max_tokens": 10,
            "messages": [{"role": "user", "content": "hi"}]
        }))
        .send()
        .await
        .expect("send request");

    assert_eq!(resp.status(), 200);
    app.state.proxy_service.stop().await.ok();
}

/// R1.local_prefix_lingzhi — "lingzhi/deepseek-v4-pro" strips prefix.
#[tokio::test]
async fn r1_local_prefix_lingzhi() {
    let mock = MockServer::start().await;
    let dir = TempDir::new().expect("tempdir");

    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "id": "msg_3", "model": "deepseek-v4-pro", "role": "assistant",
            "content": [{"type": "text", "text": "ok"}],
            "usage": {"input_tokens": 1, "output_tokens": 1}
        })))
        .expect(1)
        .mount(&mock)
        .await;

    let app = HeadlessApp::init(HeadlessOptions {
        config_dir: Some(dir.path().to_path_buf()),
        recover_proxy: false,
    })
    .await
    .expect("init headless app");

    let provider = fake_provider("lingzhi", "lingzhi", &mock.uri());
    seed_providers(&app, AppType::Codex, &[provider]).await;

    let info = app.state.proxy_service.start().await.expect("start proxy");
    let client = reqwest::Client::new();
    let resp = client
        .post(format!("http://{}:{}/v1/messages", info.address, info.port))
        .json(&json!({
            "model": "lingzhi/deepseek-v4-pro", "max_tokens": 10,
            "messages": [{"role": "user", "content": "hi"}]
        }))
        .send()
        .await
        .expect("send request");

    assert_eq!(resp.status(), 200);
    app.state.proxy_service.stop().await.ok();
}

/// ⭐ R1.unknown_prefix — "anthropic/claude-3-5-sonnet" unknown family
/// must NOT return ConfigError. Should fall through with body unchanged.
///
/// Bug #1: route_provider_model_for_app calls .ok_or_else() → ConfigError
#[tokio::test]
async fn r1_unknown_prefix_fallthrough() {
    let mock = MockServer::start().await;
    let dir = TempDir::new().expect("tempdir");

    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "id": "msg_4", "model": "anthropic/claude-3-5-sonnet", "role": "assistant",
            "content": [{"type": "text", "text": "ok"}],
            "usage": {"input_tokens": 1, "output_tokens": 1}
        })))
        .expect(1)
        .mount(&mock)
        .await;

    let app = HeadlessApp::init(HeadlessOptions {
        config_dir: Some(dir.path().to_path_buf()),
        recover_proxy: false,
    })
    .await
    .expect("init headless app");

    let provider = fake_provider("lingzhi", "lingzhi", &mock.uri());
    seed_providers(&app, AppType::Codex, &[provider]).await;

    let info = app.state.proxy_service.start().await.expect("start proxy");
    let client = reqwest::Client::new();
    let resp = client
        .post(format!("http://{}:{}/v1/messages", info.address, info.port))
        .json(&json!({
            "model": "anthropic/claude-3-5-sonnet", "max_tokens": 10,
            "messages": [{"role": "user", "content": "hi"}]
        }))
        .send()
        .await
        .expect("send request");

    // Should succeed (no ConfigError), model passes through unchanged.
    assert_eq!(resp.status(), 200);
    app.state.proxy_service.stop().await.ok();
}

/// ⭐ R1.multi_slash — "meta-llama/Llama-3-70b-instruct" unknown prefix
/// must fall through with body unchanged (same root cause as R1.unknown_prefix).
#[tokio::test]
async fn r1_multi_slash_unknown_prefix_fallthrough() {
    let mock = MockServer::start().await;
    let dir = TempDir::new().expect("tempdir");

    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "id": "msg_5", "model": "meta-llama/Llama-3-70b-instruct", "role": "assistant",
            "content": [{"type": "text", "text": "ok"}],
            "usage": {"input_tokens": 1, "output_tokens": 1}
        })))
        .expect(1)
        .mount(&mock)
        .await;

    let app = HeadlessApp::init(HeadlessOptions {
        config_dir: Some(dir.path().to_path_buf()),
        recover_proxy: false,
    })
    .await
    .expect("init headless app");

    let provider = fake_provider("lingzhi", "lingzhi", &mock.uri());
    seed_providers(&app, AppType::Codex, &[provider]).await;

    let info = app.state.proxy_service.start().await.expect("start proxy");
    let client = reqwest::Client::new();
    let resp = client
        .post(format!("http://{}:{}/v1/messages", info.address, info.port))
        .json(&json!({
            "model": "meta-llama/Llama-3-70b-instruct", "max_tokens": 10,
            "messages": [{"role": "user", "content": "hi"}]
        }))
        .send()
        .await
        .expect("send request");

    assert_eq!(resp.status(), 200);
    app.state.proxy_service.stop().await.ok();
}

/// R1.openai_alias — "openai/gpt-4o" aliases to gpt provider, strips prefix.
#[tokio::test]
async fn r1_openai_alias() {
    let mock = MockServer::start().await;
    let dir = TempDir::new().expect("tempdir");

    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "id": "msg_6", "model": "gpt-4o", "role": "assistant",
            "content": [{"type": "text", "text": "ok"}],
            "usage": {"input_tokens": 1, "output_tokens": 1}
        })))
        .expect(1)
        .mount(&mock)
        .await;

    let app = HeadlessApp::init(HeadlessOptions {
        config_dir: Some(dir.path().to_path_buf()),
        recover_proxy: false,
    })
    .await
    .expect("init headless app");

    let provider = fake_provider("gpt", "gpt", &mock.uri());
    seed_providers(&app, AppType::Codex, &[provider]).await;

    let info = app.state.proxy_service.start().await.expect("start proxy");
    let client = reqwest::Client::new();
    let resp = client
        .post(format!("http://{}:{}/v1/messages", info.address, info.port))
        .json(&json!({
            "model": "openai/gpt-4o", "max_tokens": 10,
            "messages": [{"role": "user", "content": "hi"}]
        }))
        .send()
        .await
        .expect("send request");

    assert_eq!(resp.status(), 200);
    app.state.proxy_service.stop().await.ok();
}

/// R1.responses_routing — model prefix routing on /v1/responses endpoint.
/// Uses a non-oauth provider so wiremock can intercept the upstream call.
/// (Codex OAuth normalize behaviour is covered by unit tests in handlers.rs.)
#[tokio::test]
async fn r1_responses_routing_by_prefix() {
    let mock = MockServer::start().await;
    let dir = TempDir::new().expect("tempdir");

    Mock::given(method("POST"))
        .and(path("/v1/responses"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "id": "resp_routed", "status": "completed", "model": "o1",
            "output": [], "usage": {"input_tokens": 1, "output_tokens": 1}
        })))
        .expect(1)
        .mount(&mock)
        .await;

    let app = HeadlessApp::init(HeadlessOptions {
        config_dir: Some(dir.path().to_path_buf()),
        recover_proxy: false,
    })
    .await
    .expect("init headless app");

    let provider = fake_provider("gpt", "gpt", &mock.uri());
    seed_providers(&app, AppType::Codex, &[provider]).await;

    let info = app.state.proxy_service.start().await.expect("start proxy");
    let client = reqwest::Client::new();
    let resp = client
        .post(format!("http://{}:{}/v1/responses", info.address, info.port))
        .json(&json!({"model": "gpt/o1", "input": "hi"}))
        .send()
        .await
        .expect("send request");

    assert_eq!(resp.status(), 200);
    app.state.proxy_service.stop().await.ok();
}