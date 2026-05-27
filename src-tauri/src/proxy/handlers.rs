//! 请求处理器
//!
//! 处理各种API端点的HTTP请求
//!
//! 重构后的结构：
//! - 通用逻辑提取到 `handler_context` 和 `response_processor` 模块
//! - 各 handler 只保留独特的业务逻辑
//! - Claude 的格式转换逻辑保留在此文件（用于 OpenRouter 旧接口回退）

use super::{
    error_mapper::{get_error_message, map_proxy_error_to_status},
    forwarder::ActiveConnectionGuard,
    handler_config::{
        claude_stream_usage_event_filter, CLAUDE_PARSER_CONFIG, CODEX_PARSER_CONFIG,
        GEMINI_PARSER_CONFIG, OPENAI_PARSER_CONFIG,
    },
    handler_context::RequestContext,
    providers::{
        get_adapter, get_claude_api_format, streaming::create_anthropic_sse_stream,
        streaming_gemini::create_anthropic_sse_stream_from_gemini,
        streaming_responses::create_anthropic_sse_stream_from_responses, transform,
        transform_gemini, transform_responses,
    },
    response_processor::{
        aggregate_sse_events, create_logged_passthrough_stream, create_payload_collector,
        process_response, read_decoded_body, strip_entity_headers_for_rebuilt_body,
        strip_hop_by_hop_response_headers, usage_logging_enabled, SseUsageCollector,
    },
    server::ProxyState,
    sse::{strip_sse_field, take_sse_block},
    types::*,
    usage::parser::TokenUsage,
    ProxyError,
};
use crate::app_config::AppType;
use crate::database::PRICING_SOURCE_REQUEST;
use axum::{extract::State, http::StatusCode, response::IntoResponse, Json};
use bytes::Bytes;
use http_body_util::BodyExt;
use serde_json::{json, Value};

// ============================================================================
// 健康检查和状态查询（简单端点）
// ============================================================================

/// 健康检查
pub async fn health_check() -> (StatusCode, Json<Value>) {
    (
        StatusCode::OK,
        Json(json!({
            "status": "healthy",
            "timestamp": chrono::Utc::now().to_rfc3339(),
        })),
    )
}

/// 获取服务状态
pub async fn get_status(State(state): State<ProxyState>) -> Result<Json<ProxyStatus>, ProxyError> {
    let status = state.status.read().await.clone();
    Ok(Json(status))
}

// ============================================================================
// Claude API 处理器（包含格式转换逻辑）
// ============================================================================

/// 处理 /v1/messages 请求（Claude API）
///
/// Claude 处理器包含独特的格式转换逻辑：
/// - 过去用于 OpenRouter 的 OpenAI Chat Completions 兼容接口（Anthropic ↔ OpenAI 转换）
/// - 现在 OpenRouter 已推出 Claude Code 兼容接口，默认不再启用该转换（逻辑保留以备回退）
pub async fn handle_messages(
    State(state): State<ProxyState>,
    request: axum::extract::Request,
) -> Result<axum::response::Response, ProxyError> {
    handle_messages_for_app(state, request, AppType::Claude, "Claude", "claude", None).await
}

pub async fn handle_claude_desktop_messages(
    State(state): State<ProxyState>,
    request: axum::extract::Request,
) -> Result<axum::response::Response, ProxyError> {
    validate_claude_desktop_gateway_auth(&state, request.headers())?;
    handle_messages_for_app(
        state,
        request,
        AppType::ClaudeDesktop,
        "Claude Desktop",
        "claude-desktop",
        Some("/claude-desktop"),
    )
    .await
}

pub async fn handle_claude_desktop_models(
    State(state): State<ProxyState>,
    headers: axum::http::HeaderMap,
) -> Result<Json<Value>, ProxyError> {
    validate_claude_desktop_gateway_auth(&state, &headers)?;
    let providers = state
        .provider_router
        .select_providers("claude-desktop")
        .await
        .map_err(|e| ProxyError::DatabaseError(e.to_string()))?;
    let provider = providers.first().ok_or(ProxyError::NoAvailableProvider)?;
    let response = crate::claude_desktop_config::model_list_response(provider)
        .map_err(|e| ProxyError::ConfigError(e.to_string()))?;
    Ok(Json(response))
}

async fn handle_messages_for_app(
    state: ProxyState,
    request: axum::extract::Request,
    app_type: AppType,
    tag: &'static str,
    app_type_str: &'static str,
    strip_prefix: Option<&'static str>,
) -> Result<axum::response::Response, ProxyError> {
    let (parts, body) = request.into_parts();
    let method = parts.method.clone();
    let uri = parts.uri;
    let headers = parts.headers;
    let extensions = parts.extensions;
    let body_bytes = body
        .collect()
        .await
        .map_err(|e| ProxyError::Internal(format!("Failed to read request body: {e}")))?
        .to_bytes();
    let mut body: Value = serde_json::from_slice(&body_bytes)
        .map_err(|e| ProxyError::Internal(format!("Failed to parse request body: {e}")))?;

    let registry_app_type = model_provider_registry_app_type(&app_type);
    let provider_override =
        route_provider_model_for_app(&state, &registry_app_type, &mut body).await?;
    let mut ctx = RequestContext::new_with_provider_override(
        &state,
        &body,
        &headers,
        app_type.clone(),
        tag,
        app_type_str,
        provider_override,
    )
    .await?;

    let raw_endpoint = uri
        .path_and_query()
        .map(|path_and_query| path_and_query.as_str())
        .unwrap_or(uri.path());
    let endpoint = strip_prefix
        .and_then(|prefix| raw_endpoint.strip_prefix(prefix))
        .unwrap_or(raw_endpoint);

    let is_stream = body
        .get("stream")
        .and_then(|s| s.as_bool())
        .unwrap_or(false);

    // 转发请求
    let forwarder = ctx.create_forwarder(&state);
    let mut result = match forwarder
        .forward_with_retry(
            &app_type,
            method,
            endpoint,
            body.clone(),
            headers,
            extensions,
            ctx.get_providers(),
        )
        .await
    {
        Ok(result) => result,
        Err(mut err) => {
            if let Some(provider) = err.provider.take() {
                ctx.provider = provider;
            }
            log_forward_error(&state, &ctx, is_stream, &err.error);
            return Err(err.error);
        }
    };

    let connection_guard = result.connection_guard.take();
    ctx.provider = result.provider;
    ctx.captured_request_body = result.captured_request_body.take();
    let api_format = result
        .claude_api_format
        .as_deref()
        .unwrap_or_else(|| get_claude_api_format(&ctx.provider))
        .to_string();
    let response = result.response;

    // 检查是否需要格式转换（OpenRouter 等中转服务）
    let adapter = get_adapter(&app_type);
    let needs_transform = adapter.needs_transform(&ctx.provider);

    // Claude 特有：格式转换处理
    if needs_transform {
        return handle_claude_transform(
            response,
            &ctx,
            &state,
            &body,
            is_stream,
            &api_format,
            connection_guard,
        )
        .await;
    }

    // 通用响应处理（透传模式）
    process_response(
        response,
        &ctx,
        &state,
        &CLAUDE_PARSER_CONFIG,
        connection_guard,
    )
    .await
}

fn validate_claude_desktop_gateway_auth(
    state: &ProxyState,
    headers: &axum::http::HeaderMap,
) -> Result<(), ProxyError> {
    let expected = crate::claude_desktop_config::get_or_create_gateway_token(state.db.as_ref())
        .map_err(|e| ProxyError::AuthError(e.to_string()))?;
    let Some(value) = headers.get(axum::http::header::AUTHORIZATION) else {
        return Err(ProxyError::AuthError(
            "Claude Desktop gateway 缺少 Authorization 头".to_string(),
        ));
    };
    let value = value
        .to_str()
        .map_err(|_| ProxyError::AuthError("Authorization 头格式无效".to_string()))?;
    let token = value
        .strip_prefix("Bearer ")
        .or_else(|| value.strip_prefix("bearer "))
        .unwrap_or("")
        .trim();
    if token != expected {
        return Err(ProxyError::AuthError(
            "Claude Desktop gateway token 无效".to_string(),
        ));
    }
    Ok(())
}

/// Claude 格式转换处理（独有逻辑）
///
/// 支持 OpenAI Chat Completions 和 Responses API 两种格式的转换
async fn handle_claude_transform(
    response: super::hyper_client::ProxyResponse,
    ctx: &RequestContext,
    state: &ProxyState,
    original_body: &Value,
    is_stream: bool,
    api_format: &str,
    connection_guard: Option<ActiveConnectionGuard>,
) -> Result<axum::response::Response, ProxyError> {
    let status = response.status();
    let is_codex_oauth = ctx
        .provider
        .meta
        .as_ref()
        .and_then(|meta| meta.provider_type.as_deref())
        == Some("codex_oauth");
    // Codex OAuth 会把 openai_responses 响应强制升级为 SSE，即使客户端发的是 stream:false。
    // should_use_claude_transform_streaming 默认会把这个组合路由到流式转换器——虽然能避免
    // JSON parse 报 422，但会让非流客户端收到 text/event-stream，违反 Anthropic 非流语义。
    // 这里为这个特定组合打开 override：把上游 SSE 聚合成 Anthropic JSON 回给客户端，其它
    // 场景（任意上游 is_sse、非 Codex OAuth 等）仍沿用原有流式兜底。
    let aggregate_codex_oauth_responses_sse =
        !is_stream && is_codex_oauth && api_format == "openai_responses";
    let use_streaming = if aggregate_codex_oauth_responses_sse {
        false
    } else {
        should_use_claude_transform_streaming(
            is_stream,
            response.is_sse(),
            api_format,
            is_codex_oauth,
        )
    };
    let tool_schema_hints = transform_gemini::extract_anthropic_tool_schema_hints(original_body);
    let tool_schema_hints = (!tool_schema_hints.is_empty()).then_some(tool_schema_hints);

    if use_streaming {
        // 根据 api_format 选择流式转换器
        let stream = response.bytes_stream();
        let sse_stream: Box<
            dyn futures::Stream<Item = Result<Bytes, std::io::Error>> + Send + Unpin,
        > = if api_format == "openai_responses" {
            Box::new(Box::pin(create_anthropic_sse_stream_from_responses(stream)))
        } else if api_format == "gemini_native" {
            Box::new(Box::pin(create_anthropic_sse_stream_from_gemini(
                stream,
                Some(state.gemini_shadow.clone()),
                Some(ctx.provider.id.clone()),
                Some(ctx.session_id.clone()),
                tool_schema_hints.clone(),
            )))
        } else {
            Box::new(Box::pin(create_anthropic_sse_stream(stream)))
        };

        // 创建使用量 + payload 收集器（合并到同一个回调，确保 request_id 一致）
        let captured_request_body = ctx.captured_request_body.clone();
        let usage_collector = {
            let state = state.clone();
            let provider_id = ctx.provider.id.clone();
            let model = ctx.request_model.clone();
            let status_code = status.as_u16();
            let start_time = ctx.start_time;
            let session_id = ctx.session_id.clone();
            let logging_enabled = usage_logging_enabled(&state);

            Some(SseUsageCollector::new(
                start_time,
                None, // 收集所有 events（payload 需要完整内容）
                move |events, first_token_ms| {
                    let usage = TokenUsage::from_claude_stream_events(&events);
                    let request_id = usage
                        .as_ref()
                        .map(|u| u.dedup_request_id())
                        .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());

                    let latency_ms = start_time.elapsed().as_millis() as u64;
                    let state = state.clone();
                    let provider_id = provider_id.clone();
                    let model = model.clone();
                    let session_id = session_id.clone();
                    let captured_body = captured_request_body.clone();
                    let aggregated = aggregate_sse_events(&events);

                    tokio::spawn(async move {
                        // 记录 usage
                        if logging_enabled {
                            if let Some(usage) = usage {
                                log_usage(
                                    &state,
                                    &provider_id,
                                    "claude",
                                    &model,
                                    &model,
                                    usage,
                                    latency_ms,
                                    first_token_ms,
                                    true,
                                    status_code,
                                    Some(session_id),
                                )
                                .await;
                            }
                        }
                        // 记录 payload（和 usage 共享 request_id）
                        if let Some(req) = captured_body {
                            use crate::proxy::usage::payload_logger::{PayloadLog, PayloadLogger};
                            let logger = PayloadLogger::new(&state.db);
                            let log = PayloadLog {
                                request_id,
                                request_body: req,
                                response_body: Some(aggregated),
                                request_headers: None,
                                response_headers: None,
                                created_at: chrono::Utc::now().timestamp(),
                            };
                            if let Err(e) = logger.log_payload(log) {
                                log::warn!("payload 记录失败 (claude stream): {e}");
                            }
                        }
                    });
                },
            ))
        };

        // 获取流式超时配置
        let timeout_config = ctx.streaming_timeout_config();

        let logged_stream = create_logged_passthrough_stream(
            sse_stream,
            "Claude/OpenRouter",
            usage_collector,
            timeout_config,
            connection_guard,
            None, // payload 已在 usage_collector 回调中处理
        );

        let mut headers = axum::http::HeaderMap::new();
        headers.insert(
            "Content-Type",
            axum::http::HeaderValue::from_static("text/event-stream"),
        );
        headers.insert(
            "Cache-Control",
            axum::http::HeaderValue::from_static("no-cache"),
        );

        let body = axum::body::Body::from_stream(logged_stream);
        return Ok((headers, body).into_response());
    }

    // 非流式响应转换 (OpenAI/Responses → Anthropic)
    let body_timeout =
        if ctx.app_config.auto_failover_enabled && ctx.app_config.non_streaming_timeout > 0 {
            std::time::Duration::from_secs(ctx.app_config.non_streaming_timeout as u64)
        } else {
            std::time::Duration::ZERO
        };
    let (mut response_headers, _status, body_bytes) =
        read_decoded_body(response, ctx.tag, body_timeout).await?;

    let body_str = String::from_utf8_lossy(&body_bytes);

    let upstream_response: Value = if aggregate_codex_oauth_responses_sse {
        responses_sse_to_response_value(&body_str).map_err(|e| {
            log_forward_error(&state, &ctx, is_stream, &e);
            e
        })?
    } else {
        serde_json::from_slice(&body_bytes).map_err(|e| {
            log::error!("[Claude] 解析上游响应失败: {e}, body: {body_str}");
            ProxyError::TransformError(format!("Failed to parse upstream response: {e}"))
        })?
    };

    // 根据 api_format 选择非流式转换器
    let anthropic_response = if api_format == "openai_responses" {
        transform_responses::responses_to_anthropic(upstream_response)
    } else if api_format == "gemini_native" {
        transform_gemini::gemini_to_anthropic_with_shadow_and_hints(
            upstream_response,
            Some(state.gemini_shadow.as_ref()),
            Some(&ctx.provider.id),
            Some(&ctx.session_id),
            tool_schema_hints.as_ref(),
        )
    } else {
        transform::openai_to_anthropic(upstream_response)
    }
    .map_err(|e| {
        log::error!("[Claude] 转换响应失败: {e}");
        e
    })?;

    // 记录使用量 + payload（共享 request_id）
    {
        let usage = TokenUsage::from_claude_response(&anthropic_response);
        let request_id = usage
            .as_ref()
            .map(|u| u.dedup_request_id())
            .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
        let model = anthropic_response
            .get("model")
            .and_then(|m| m.as_str())
            .unwrap_or("unknown")
            .to_string();
        let latency_ms = ctx.latency_ms();
        let request_model = ctx.request_model.clone();
        let request_body = ctx.captured_request_body.clone();
        let response_body = anthropic_response.clone();

        tokio::spawn({
            let state = state.clone();
            let provider_id = ctx.provider.id.clone();
            let session_id = ctx.session_id.clone();
            let request_id = request_id.clone();
            async move {
                if let Some(usage) = usage {
                    use super::usage::logger::UsageLogger;
                    let logger = UsageLogger::new(&state.db);
                    let (multiplier, pricing_model_source) =
                        logger.resolve_pricing_config(&provider_id, "claude").await;
                    let pricing_model = if pricing_model_source
                        == crate::database::PRICING_SOURCE_REQUEST
                    {
                        request_model.clone()
                    } else {
                        model.clone()
                    };
                    if let Err(e) = logger.log_with_calculation(
                        request_id.clone(),
                        provider_id,
                        "claude".to_string(),
                        model,
                        request_model,
                        pricing_model,
                        usage,
                        multiplier,
                        latency_ms,
                        None,
                        status.as_u16(),
                        Some(session_id),
                        None,
                        false,
                    ) {
                        log::warn!("[Claude] usage 记录失败: {e}");
                    }
                }
                if let Some(req) = request_body {
                    use crate::proxy::usage::payload_logger::{PayloadLog, PayloadLogger};
                    let logger = PayloadLogger::new(&state.db);
                    let log = PayloadLog {
                        request_id,
                        request_body: req,
                        response_body: Some(response_body),
                        request_headers: None,
                        response_headers: None,
                        created_at: chrono::Utc::now().timestamp(),
                    };
                    if let Err(e) = logger.log_payload(log) {
                        log::warn!("payload 记录失败 (claude transform): {e}");
                    }
                }
            }
        });
    }

    // 构建响应
    let mut builder = axum::response::Response::builder().status(status);
    strip_entity_headers_for_rebuilt_body(&mut response_headers);
    strip_hop_by_hop_response_headers(&mut response_headers);

    for (key, value) in response_headers.iter() {
        builder = builder.header(key, value);
    }

    builder = builder.header("content-type", "application/json");

    let response_body = serde_json::to_vec(&anthropic_response).map_err(|e| {
        log::error!("[Claude] 序列化响应失败: {e}");
        ProxyError::TransformError(format!("Failed to serialize response: {e}"))
    })?;

    let body = axum::body::Body::from(response_body);
    builder.body(body).map_err(|e| {
        log::error!("[Claude] 构建响应失败: {e}");
        ProxyError::Internal(format!("Failed to build response: {e}"))
    })
}

fn endpoint_with_query(uri: &axum::http::Uri, endpoint: &str) -> String {
    match uri.query() {
        Some(query) => format!("{endpoint}?{query}"),
        None => endpoint.to_string(),
    }
}

// ============================================================================
// Codex API 处理器
// ============================================================================

fn model_family_provider_id(family: &str) -> &str {
    match family {
        "openai" => "gpt",
        other => other,
    }
}

fn model_provider_registry_app_type(app_type: &AppType) -> AppType {
    match app_type {
        AppType::Claude => AppType::Codex,
        _ => app_type.clone(),
    }
}

async fn route_provider_model_for_app(
    state: &ProxyState,
    app_type: &AppType,
    body: &mut Value,
) -> Result<Option<Vec<crate::provider::Provider>>, ProxyError> {
    let Some(model) = body.get("model").and_then(|m| m.as_str()) else {
        return Ok(None);
    };
    let Some((family, upstream_model)) = model.split_once('/') else {
        return Ok(None);
    };
    if family.is_empty() || upstream_model.is_empty() {
        return Ok(None);
    }

    let provider_id = model_family_provider_id(family);
    let Some(provider) = state
        .db
        .get_provider_by_id(provider_id, app_type.as_str())
        .map_err(|e| ProxyError::DatabaseError(e.to_string()))?
    else {
        log::debug!(
            "[route] unknown model prefix family={family}, falling through to default provider"
        );
        return Ok(None);
    };

    body["model"] = Value::String(upstream_model.to_string());
    Ok(Some(vec![provider]))
}


pub(crate) fn normalize_codex_oauth_responses_body(body: &mut Value) {
    const REASONING_MARKER: &str = "reasoning.encrypted_content";
    if !body.is_object() {
        return;
    }

    if let Some(obj) = body.as_object_mut() {
        obj.insert("store".to_string(), json!(false));
        obj.remove("max_output_tokens");
        obj.remove("temperature");
        obj.remove("top_p");
        obj.entry("instructions".to_string()).or_insert(json!(""));
        obj.entry("tools".to_string()).or_insert(json!([]));
        obj.entry("parallel_tool_calls".to_string())
            .or_insert(json!(false));

        // Upstream codex_oauth always responds with SSE regardless of what
        // the client requested. The is_stream flag (captured before this
        // normalize) controls whether process_response aggregates SSE into
        // JSON for non-streaming clients.
        obj.insert("stream".to_string(), json!(true));

        if let Some(input) = obj.get_mut("input") {
            if let Some(text) = input.as_str() {
                *input = json!([{
                    "role": "user",
                    "content": [{
                        "type": "input_text",
                        "text": text,
                    }],
                }]);
            }
        }

        let include = obj.entry("include".to_string()).or_insert(json!([]));
        if !include.is_array() {
            *include = json!([]);
        }
        if let Some(items) = include.as_array_mut() {
            if !items
                .iter()
                .any(|value| value.as_str() == Some(REASONING_MARKER))
            {
                items.push(json!(REASONING_MARKER));
            }
        }
    }
}

pub async fn handle_openai_models(
    State(state): State<ProxyState>,
) -> Result<Json<Value>, ProxyError> {
    let providers = state
        .db
        .get_all_providers(AppType::Codex.as_str())
        .map_err(|e| ProxyError::DatabaseError(e.to_string()))?;

    let mut data = Vec::new();
    for provider in providers.values() {
        let family = provider
            .settings_config
            .get("modelFamily")
            .and_then(|v| v.as_str())
            .unwrap_or(provider.id.as_str());
        let Some(models) = provider
            .settings_config
            .get("models")
            .and_then(|v| v.as_array())
        else {
            continue;
        };

        for model in models {
            let model_id = match model {
                Value::String(id) => id.as_str(),
                Value::Object(obj) => obj.get("id").and_then(|v| v.as_str()).unwrap_or_default(),
                _ => "",
            };
            if model_id.is_empty() {
                continue;
            }
            data.push(json!({
                "id": format!("{family}/{model_id}"),
                "object": "model",
                "created": 0,
                "owned_by": family,
            }));
        }
    }

    data.sort_by(|a, b| {
        a.get("id")
            .and_then(|v| v.as_str())
            .cmp(&b.get("id").and_then(|v| v.as_str()))
    });

    Ok(Json(json!({
        "object": "list",
        "data": data,
    })))
}

/// 处理 /v1/chat/completions 请求（OpenAI Chat Completions API - Codex CLI）
pub async fn handle_chat_completions(
    State(state): State<ProxyState>,
    request: axum::extract::Request,
) -> Result<axum::response::Response, ProxyError> {
    let (parts, req_body) = request.into_parts();
    let method = parts.method.clone();
    let uri = parts.uri;
    let headers = parts.headers;
    let extensions = parts.extensions;
    let body_bytes = req_body
        .collect()
        .await
        .map_err(|e| ProxyError::Internal(format!("Failed to read request body: {e}")))?
        .to_bytes();
    let mut body: Value = serde_json::from_slice(&body_bytes)
        .map_err(|e| ProxyError::Internal(format!("Failed to parse request body: {e}")))?;

    let provider_override =
        route_provider_model_for_app(&state, &AppType::Codex, &mut body).await?;
    let mut ctx = RequestContext::new_with_provider_override(
        &state,
        &body,
        &headers,
        AppType::Codex,
        "Codex",
        "codex",
        provider_override,
    )
    .await?;
    let endpoint = endpoint_with_query(&uri, "/chat/completions");

    let is_stream = body
        .get("stream")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);

    let forwarder = ctx.create_forwarder(&state);
    let mut result = match forwarder
        .forward_with_retry(
            &AppType::Codex,
            method,
            &endpoint,
            body,
            headers,
            extensions,
            ctx.get_providers(),
        )
        .await
    {
        Ok(result) => result,
        Err(mut err) => {
            if let Some(provider) = err.provider.take() {
                ctx.provider = provider;
            }
            log_forward_error(&state, &ctx, is_stream, &err.error);
            return Err(err.error);
        }
    };

    let connection_guard = result.connection_guard.take();
    ctx.provider = result.provider;
    ctx.captured_request_body = result.captured_request_body.take();
    let response = result.response;

    process_response(
        response,
        &ctx,
        &state,
        &OPENAI_PARSER_CONFIG,
        connection_guard,
    )
    .await
}

/// 处理 /v1/responses 请求（OpenAI Responses API - Codex CLI 透传）
pub async fn handle_responses(
    State(state): State<ProxyState>,
    request: axum::extract::Request,
) -> Result<axum::response::Response, ProxyError> {
    let (parts, req_body) = request.into_parts();
    let method = parts.method.clone();
    let uri = parts.uri;
    let headers = parts.headers;
    let extensions = parts.extensions;
    let body_bytes = req_body
        .collect()
        .await
        .map_err(|e| ProxyError::Internal(format!("Failed to read request body: {e}")))?
        .to_bytes();
    let mut body: Value = serde_json::from_slice(&body_bytes)
        .map_err(|e| ProxyError::Internal(format!("Failed to parse request body: {e}")))?;

    // Capture client stream intent BEFORE normalize mutates the body.
    let client_wants_stream = body
        .get("stream")
        .and_then(|v| v.as_bool())
        .unwrap_or(false); // OpenAI Responses default is non-streaming

    let provider_override =
        route_provider_model_for_app(&state, &AppType::Codex, &mut body).await?;

    let mut ctx = RequestContext::new_with_provider_override(
        &state,
        &body,
        &headers,
        AppType::Codex,
        "Codex",
        "codex",
        provider_override,
    )
    .await?;

    // Use the resolved provider (not just provider_override) to decide
    // whether to normalize. This covers the case where the current/default
    // provider is codex_oauth but the model has no prefix slug → route
    // returns None, yet the downstream backend is still the ChatGPT OAuth
    // endpoint and needs the normalized request shape.
    if ctx.provider.is_codex_oauth() {
        normalize_codex_oauth_responses_body(&mut body);
    }

    let endpoint = endpoint_with_query(&uri, "/responses");

    let is_stream = client_wants_stream;

    let forwarder = ctx.create_forwarder(&state);
    let mut result = match forwarder
        .forward_with_retry(
            &AppType::Codex,
            method,
            &endpoint,
            body,
            headers,
            extensions,
            ctx.get_providers(),
        )
        .await
    {
        Ok(result) => result,
        Err(mut err) => {
            if let Some(provider) = err.provider.take() {
                ctx.provider = provider;
            }
            log_forward_error(&state, &ctx, is_stream, &err.error);
            return Err(err.error);
        }
    };

    let connection_guard = result.connection_guard.take();
    ctx.provider = result.provider;
    ctx.captured_request_body = result.captured_request_body.take();
    let response = result.response;

    process_response(
        response,
        &ctx,
        &state,
        &CODEX_PARSER_CONFIG,
        connection_guard,
    )
    .await
}

/// 处理 /v1/responses/compact 请求（OpenAI Responses Compact API - Codex CLI 透传）
pub async fn handle_responses_compact(
    State(state): State<ProxyState>,
    request: axum::extract::Request,
) -> Result<axum::response::Response, ProxyError> {
    let (parts, req_body) = request.into_parts();
    let method = parts.method.clone();
    let uri = parts.uri;
    let headers = parts.headers;
    let extensions = parts.extensions;
    let body_bytes = req_body
        .collect()
        .await
        .map_err(|e| ProxyError::Internal(format!("Failed to read request body: {e}")))?
        .to_bytes();
    let mut body: Value = serde_json::from_slice(&body_bytes)
        .map_err(|e| ProxyError::Internal(format!("Failed to parse request body: {e}")))?;

    let provider_override =
        route_provider_model_for_app(&state, &AppType::Codex, &mut body).await?;
    let mut ctx = RequestContext::new_with_provider_override(
        &state,
        &body,
        &headers,
        AppType::Codex,
        "Codex",
        "codex",
        provider_override,
    )
    .await?;
    let endpoint = endpoint_with_query(&uri, "/responses/compact");

    let is_stream = body
        .get("stream")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);

    let forwarder = ctx.create_forwarder(&state);
    let mut result = match forwarder
        .forward_with_retry(
            &AppType::Codex,
            method,
            &endpoint,
            body,
            headers,
            extensions,
            ctx.get_providers(),
        )
        .await
    {
        Ok(result) => result,
        Err(mut err) => {
            if let Some(provider) = err.provider.take() {
                ctx.provider = provider;
            }
            log_forward_error(&state, &ctx, is_stream, &err.error);
            return Err(err.error);
        }
    };

    let connection_guard = result.connection_guard.take();
    ctx.provider = result.provider;
    ctx.captured_request_body = result.captured_request_body.take();
    let response = result.response;

    process_response(
        response,
        &ctx,
        &state,
        &CODEX_PARSER_CONFIG,
        connection_guard,
    )
    .await
}

// ============================================================================
// Gemini API 处理器
// ============================================================================

/// 处理 Gemini API 请求（透传，包括查询参数）
pub async fn handle_gemini(
    State(state): State<ProxyState>,
    uri: axum::http::Uri,
    request: axum::extract::Request,
) -> Result<axum::response::Response, ProxyError> {
    let (parts, req_body) = request.into_parts();
    let method = parts.method.clone();
    let headers = parts.headers;
    let extensions = parts.extensions;
    let body_bytes = req_body
        .collect()
        .await
        .map_err(|e| ProxyError::Internal(format!("Failed to read request body: {e}")))?
        .to_bytes();
    // GET 类只读端点（/v1beta/models、/v1beta/models/<model> 等）没有请求体，
    // 不能强制 parse 为 JSON —— 否则空 body 会被拒绝。
    let body: Value = if body_bytes.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&body_bytes)
            .map_err(|e| ProxyError::Internal(format!("Failed to parse request body: {e}")))?
    };

    // Gemini 的模型名称在 URI 中
    let mut ctx = RequestContext::new(&state, &body, &headers, AppType::Gemini, "Gemini", "gemini")
        .await?
        .with_model_from_uri(&uri);

    // 提取完整的路径和查询参数
    let endpoint = uri
        .path_and_query()
        .map(|pq| pq.as_str())
        .unwrap_or(uri.path());

    let is_stream = body
        .get("stream")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);

    let forwarder = ctx.create_forwarder(&state);
    let mut result = match forwarder
        .forward_with_retry(
            &AppType::Gemini,
            method,
            endpoint,
            body,
            headers,
            extensions,
            ctx.get_providers(),
        )
        .await
    {
        Ok(result) => result,
        Err(mut err) => {
            if let Some(provider) = err.provider.take() {
                ctx.provider = provider;
            }
            log_forward_error(&state, &ctx, is_stream, &err.error);
            return Err(err.error);
        }
    };

    let connection_guard = result.connection_guard.take();
    ctx.provider = result.provider;
    ctx.captured_request_body = result.captured_request_body.take();
    let response = result.response;

    process_response(
        response,
        &ctx,
        &state,
        &GEMINI_PARSER_CONFIG,
        connection_guard,
    )
    .await
}

fn should_use_claude_transform_streaming(
    requested_streaming: bool,
    upstream_is_sse: bool,
    api_format: &str,
    is_codex_oauth: bool,
) -> bool {
    requested_streaming || upstream_is_sse || (is_codex_oauth && api_format == "openai_responses")
}

/// 把 OpenAI Responses SSE 流聚合成一个完整的 Responses JSON 对象，供下游转成 Anthropic
/// 非流响应。仅在 Codex OAuth 把 `stream:false` 强制升级为 SSE 的场景下调用。
///
/// 复用 `proxy::sse` 的 `take_sse_block`/`strip_sse_field`：`take_sse_block` 同时支持
/// `\n\n` 与 `\r\n\r\n` 两种分隔符，`strip_sse_field` 兼容带/不带空格的字段写法。
/// 识别上游"context window 超限"类错误。
///
/// Why: Claude Code 客户端按 `claude-opus-4-7` 的窗口（~1M）预估上下文，
/// 但路由到 `gpt/gpt-5.5` 的 ChatGPT Codex backend 实际窗口远小于此。
/// 命中后改返回 400 invalid_request_error，让客户端识别为请求级问题
/// 而非"代理转换错误"（422），便于触发 /compact 或人工裁剪后重试。
fn is_context_window_exceeded_message(message: &str) -> bool {
    let lower = message.to_lowercase();
    const MARKERS: &[&str] = &[
        "exceeds the context window",
        "exceeds the model's context",
        "exceed the context window",
        "exceed the model's context",
        "context_length_exceeded",
        "context length exceeded",
        "maximum context length",
        "context window of this model",
        "input is too long",
        "request too large",
        "prompt is too long",
        "tokens exceeds",
        "上下文窗口",
        "上下文长度",
    ];
    MARKERS.iter().any(|m| lower.contains(m))
}

fn responses_sse_to_response_value(body: &str) -> Result<Value, ProxyError> {
    let mut buffer = body.to_string();
    let mut completed_response: Option<Value> = None;
    let mut output_items = Vec::new();

    while let Some(block) = take_sse_block(&mut buffer) {
        let mut event_name = "";
        let mut data_lines: Vec<&str> = Vec::new();

        for line in block.lines() {
            if let Some(evt) = strip_sse_field(line, "event") {
                event_name = evt.trim();
            } else if let Some(d) = strip_sse_field(line, "data") {
                data_lines.push(d);
            }
        }

        if data_lines.is_empty() {
            continue;
        }

        let data_str = data_lines.join("\n");
        if data_str.trim() == "[DONE]" {
            continue;
        }

        let data: Value = serde_json::from_str(&data_str).map_err(|e| {
            ProxyError::TransformError(format!("Failed to parse upstream SSE event: {e}"))
        })?;

        match event_name {
            "response.output_item.done" => {
                if let Some(item) = data.get("item") {
                    output_items.push(item.clone());
                }
            }
            "response.completed" => {
                completed_response = Some(data.get("response").cloned().unwrap_or(data));
            }
            "response.failed" => {
                let message = data
                    .pointer("/response/error/message")
                    .and_then(|v| v.as_str())
                    .unwrap_or("response.failed event received");
                if is_context_window_exceeded_message(message) {
                    return Err(ProxyError::InvalidRequest(message.to_string()));
                }
                return Err(ProxyError::TransformError(message.to_string()));
            }
            _ => {}
        }
    }

    let mut response = completed_response.ok_or_else(|| {
        ProxyError::TransformError("No response.completed event in upstream SSE".to_string())
    })?;

    if !output_items.is_empty() {
        if let Some(obj) = response.as_object_mut() {
            obj.insert("output".to_string(), Value::Array(output_items));
        } else {
            return Err(ProxyError::TransformError(
                "response.completed payload is not an object".to_string(),
            ));
        }
    }

    Ok(response)
}

// ============================================================================
// 使用量记录（保留用于 Claude 转换逻辑）
// ============================================================================

fn log_forward_error(
    state: &ProxyState,
    ctx: &RequestContext,
    is_streaming: bool,
    error: &ProxyError,
) {
    use super::usage::logger::UsageLogger;

    let logger = UsageLogger::new(&state.db);
    let status_code = map_proxy_error_to_status(error);
    let error_message = get_error_message(error);
    let request_id = uuid::Uuid::new_v4().to_string();

    if let Err(e) = logger.log_error_with_context(
        request_id,
        ctx.provider.id.clone(),
        ctx.app_type_str.to_string(),
        ctx.request_model.clone(),
        status_code,
        error_message,
        ctx.latency_ms(),
        is_streaming,
        Some(ctx.session_id.clone()),
        None,
    ) {
        log::warn!("记录失败请求日志失败: {e}");
    }
}

/// 记录请求使用量
#[allow(clippy::too_many_arguments)]
async fn log_usage(
    state: &ProxyState,
    provider_id: &str,
    app_type: &str,
    model: &str,
    request_model: &str,
    usage: TokenUsage,
    latency_ms: u64,
    first_token_ms: Option<u64>,
    is_streaming: bool,
    status_code: u16,
    session_id: Option<String>,
) {
    use super::usage::logger::UsageLogger;

    if !usage_logging_enabled(state) {
        return;
    }

    let logger = UsageLogger::new(&state.db);

    let (multiplier, pricing_model_source) =
        logger.resolve_pricing_config(provider_id, app_type).await;
    let pricing_model = if pricing_model_source == PRICING_SOURCE_REQUEST {
        request_model
    } else {
        model
    };

    let request_id = usage.dedup_request_id();

    if let Err(e) = logger.log_with_calculation(
        request_id,
        provider_id.to_string(),
        app_type.to_string(),
        model.to_string(),
        request_model.to_string(),
        pricing_model.to_string(),
        usage,
        multiplier,
        latency_ms,
        first_token_ms,
        status_code,
        session_id,
        None, // provider_type
        is_streaming,
    ) {
        log::warn!("[USG-001] 记录使用量失败: {e}");
    }
}

#[cfg(test)]
mod tests {
    use super::{
        is_context_window_exceeded_message, responses_sse_to_response_value,
        should_use_claude_transform_streaming,
    };
    use crate::proxy::ProxyError;

    #[test]
    fn codex_oauth_responses_force_streaming_even_if_client_sent_false() {
        assert!(should_use_claude_transform_streaming(
            false,
            false,
            "openai_responses",
            true,
        ));
    }

    #[test]
    fn upstream_sse_response_always_uses_streaming_path() {
        assert!(should_use_claude_transform_streaming(
            false,
            true,
            "openai_chat",
            false,
        ));
    }

    #[test]
    fn non_streaming_response_stays_non_streaming_for_regular_openai_responses() {
        assert!(!should_use_claude_transform_streaming(
            false,
            false,
            "openai_responses",
            false,
        ));
    }

    #[test]
    fn responses_sse_to_response_value_collects_output_items() {
        let sse = r#"event: response.output_item.done
data: {"type":"response.output_item.done","item":{"type":"message","role":"assistant","content":[{"type":"output_text","text":"hello"}]}}

event: response.completed
data: {"type":"response.completed","response":{"id":"resp_1","status":"completed","model":"gpt-5.4","output":[],"usage":{"input_tokens":10,"output_tokens":2}}}

"#;

        let response = responses_sse_to_response_value(sse).unwrap();

        assert_eq!(response["id"], "resp_1");
        assert_eq!(response["output"][0]["type"], "message");
        assert_eq!(response["output"][0]["content"][0]["text"], "hello");
    }

    #[test]
    fn responses_sse_to_response_value_handles_crlf_delimiters() {
        // 真实 HTTP SSE 按规范使用 \r\n\r\n 分隔事件；take_sse_block 必须同时处理两种分隔符，
        // 否则此路径在任何标准上游（含 Codex OAuth HTTPS 后端）下都会 TransformError。
        let sse = "event: response.output_item.done\r\n\
data: {\"type\":\"response.output_item.done\",\"item\":{\"type\":\"message\",\"role\":\"assistant\",\"content\":[{\"type\":\"output_text\",\"text\":\"hi\"}]}}\r\n\
\r\n\
event: response.completed\r\n\
data: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp_crlf\",\"status\":\"completed\",\"model\":\"gpt-5.4\",\"output\":[],\"usage\":{\"input_tokens\":5,\"output_tokens\":1}}}\r\n\
\r\n";

        let response = responses_sse_to_response_value(sse).unwrap();

        assert_eq!(response["id"], "resp_crlf");
        assert_eq!(response["output"][0]["type"], "message");
        assert_eq!(response["output"][0]["content"][0]["text"], "hi");
    }

    #[test]
    fn responses_sse_to_response_value_returns_err_on_response_failed() {
        let sse = "event: response.failed\n\
data: {\"type\":\"response.failed\",\"response\":{\"error\":{\"message\":\"upstream blew up\"}}}\n\n";

        let err = responses_sse_to_response_value(sse).unwrap_err();
        match err {
            ProxyError::TransformError(msg) => assert!(msg.contains("upstream blew up")),
            other => panic!("expected TransformError, got {other:?}"),
        }
    }

    #[test]
    fn context_window_marker_detection_positive_cases() {
        let cases = [
            "Your input exceeds the context window of this model. Please adjust your input and try again.",
            "This model's maximum context length is 400000 tokens.",
            "context_length_exceeded: please reduce the input.",
            "Request too large for model gpt-5.5",
            "The prompt is too long for this model",
            "请求 token 数已超过模型上下文窗口限制",
        ];
        for msg in cases {
            assert!(
                is_context_window_exceeded_message(msg),
                "should match context-window marker: {msg}"
            );
        }
    }

    #[test]
    fn context_window_marker_detection_negative_cases() {
        let cases = [
            "upstream blew up",
            "Internal server error",
            "Refresh Token 失效或已过期",
            "rate limit reached",
            "model not found",
        ];
        for msg in cases {
            assert!(
                !is_context_window_exceeded_message(msg),
                "should not match context-window marker: {msg}"
            );
        }
    }

    #[test]
    fn responses_sse_response_failed_with_context_window_maps_to_invalid_request() {
        let sse = "event: response.failed\n\
data: {\"type\":\"response.failed\",\"response\":{\"error\":{\"message\":\"Your input exceeds the context window of this model. Please adjust your input and try again.\"}}}\n\n";

        let err = responses_sse_to_response_value(sse).unwrap_err();
        match err {
            ProxyError::InvalidRequest(msg) => {
                assert!(msg.contains("context window"), "unexpected message: {msg}")
            }
            other => panic!("expected InvalidRequest, got {other:?}"),
        }
    }

    #[test]
    fn responses_sse_to_response_value_errors_when_no_completed_event() {
        let sse = "event: response.output_item.done\n\
data: {\"type\":\"response.output_item.done\",\"item\":{\"type\":\"message\"}}\n\n";

        assert!(responses_sse_to_response_value(sse).is_err());
    }

    // ===================================================================
    // normalize_codex_oauth_responses_body unit tests
    // ===================================================================

    #[test]
    fn normalize_stream_false_stays_true() {
        let mut body = serde_json::json!({
            "model": "gpt-5.4",
            "input": "hello",
            "stream": false,
        });
        super::normalize_codex_oauth_responses_body(&mut body);
        // Upstream always SSE; body must carry stream:true for ChatGPT backend.
        assert_eq!(body["stream"], serde_json::json!(true));
        assert_eq!(body["store"], serde_json::json!(false));
        assert!(body["include"]
            .as_array()
            .unwrap()
            .contains(&serde_json::json!("reasoning.encrypted_content")));
    }

    #[test]
    fn normalize_stream_omit_defaults_true() {
        let mut body = serde_json::json!({
            "model": "gpt-5.4",
            "input": "hello",
        });
        super::normalize_codex_oauth_responses_body(&mut body);
        assert_eq!(body["stream"], serde_json::json!(true));
    }

    #[test]
    fn normalize_stream_true_preserves() {
        let mut body = serde_json::json!({
            "model": "gpt-5.4",
            "input": "hello",
            "stream": true,
        });
        super::normalize_codex_oauth_responses_body(&mut body);
        assert_eq!(body["stream"], serde_json::json!(true));
    }

    #[test]
    fn normalize_wraps_string_input() {
        let mut body = serde_json::json!({
            "model": "gpt-5.4",
            "input": "plain text input",
        });
        super::normalize_codex_oauth_responses_body(&mut body);
        assert_eq!(
            body["input"],
            serde_json::json!([{
                "role": "user",
                "content": [{"type": "input_text", "text": "plain text input"}],
            }])
        );
    }

    #[test]
    fn normalize_skips_non_object() {
        let mut body = serde_json::json!("not an object");
        super::normalize_codex_oauth_responses_body(&mut body);
        assert_eq!(body, serde_json::json!("not an object"));
    }
}
