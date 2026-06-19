use serde::Serialize;
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::RwLock;

pub type CodexQuotaStore = Arc<RwLock<HashMap<String, CodexQuotaSnapshot>>>;

/// 打码：头 6 + … + 尾 4
pub fn mask_key(k: &str) -> String {
    let n = k.chars().count();
    if n <= 10 {
        return "…".to_string();
    }
    let head: String = k.chars().take(6).collect();
    let tail: String = k.chars().skip(n - 4).collect();
    format!("{head}…{tail}")
}

/// 取 base_url 的 host（去协议/路径/端口），失败回退原串
pub fn host_of(base_url: &str) -> String {
    let no_scheme = base_url
        .split_once("://")
        .map(|(_, r)| r)
        .unwrap_or(base_url);
    no_scheme
        .split(['/', ':'])
        .next()
        .unwrap_or(no_scheme)
        .to_string()
}

fn sha_fp(base_url: &str, ak: &str) -> String {
    let mut h = Sha256::new();
    h.update(base_url.as_bytes());
    h.update(b"\n");
    h.update(ak.as_bytes());
    let digest = h.finalize();
    let fp = digest
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect::<String>();
    fp[..12].to_string()
}

/// 读 settings_config 里的 base_url（兼容 top-level 与 env.* 两种放法）
fn provider_base_url(p: &crate::provider::Provider) -> Option<String> {
    let sc = &p.settings_config;
    sc.get("base_url")
        .and_then(|v| v.as_str())
        .or_else(|| {
            sc.get("env")
                .and_then(|e| e.get("ANTHROPIC_BASE_URL"))
                .and_then(|v| v.as_str())
        })
        .map(|s| s.trim_end_matches('/').to_string())
}

/// 读 settings_config 里的 apiKey（兼容 top-level apiKey 与 env.*），返回原始串
fn provider_api_key_raw(p: &crate::provider::Provider) -> Option<String> {
    let sc = &p.settings_config;
    sc.get("apiKey")
        .and_then(|v| v.as_str())
        .or_else(|| {
            sc.get("env")
                .and_then(|e| {
                    e.get("ANTHROPIC_AUTH_TOKEN")
                        .or_else(|| e.get("ANTHROPIC_API_KEY"))
                })
                .and_then(|v| v.as_str())
        })
        .map(|s| s.to_string())
}

/// 派生计费单元身份。无 quota 数据源（既无 usage_script 又非 codex_oauth）→ None。
///
/// 注意：codex_oauth 单元的最终 account_id 依赖托管认证注册表里的 default account，
/// 无法在纯函数里同步确定（详见 `resolve_codex_account_id` / `oauth_billing_unit`）。
/// 这里对 codex_oauth 只产出一个**占位**单元（id 主体可能是绑定的 account_id，
/// 否则暂记 `"default"`），真正与转发写侧对齐的 id 由 `handle_quota` 在异步解析后重建。
/// ApiKey 分支是纯的、可直接采用。
pub fn billing_unit(provider: &crate::provider::Provider, app_type: &str) -> Option<BillingUnit> {
    if provider.is_codex_oauth() {
        // 占位：优先用显式绑定的 account_id，否则先标 "default"，由 handle_quota 异步重算。
        let account_id = provider
            .meta
            .as_ref()
            .and_then(|m| m.managed_account_id_for("codex_oauth"))
            .unwrap_or_else(|| "default".to_string());
        return Some(oauth_billing_unit(provider, app_type, &account_id));
    }
    if provider.has_usage_script_enabled() {
        let base_url = provider_base_url(provider).unwrap_or_default();
        // fp / label 都基于「经 resolve_env_reference 解析后的 AK」（design §2.1），
        // 与转发侧实际送出的凭据一致，避免 {env:..} 占位串污染指纹。
        let raw_ak = provider_api_key_raw(provider).unwrap_or_default();
        let resolved_ak = resolve_env_reference(&raw_ak);
        let fp = sha_fp(&base_url, &resolved_ak);
        let host = host_of(&base_url);
        return Some(BillingUnit {
            id: format!("key:{host}:{fp}"),
            kind: BillingKind::ApiKey,
            app_type: app_type.to_string(),
            label: format!("{host} · {}", mask_key(&resolved_ak)),
            members: vec![provider.id.clone()],
        });
    }
    None
}

/// 由「已解析的 codex account_id」构造 OAuth 计费单元（id/label/members 统一出处）。
/// 写侧（forwarder 写快照的 key）与读侧（handle_quota 查快照的 key）都经此函数派生，
/// 保证 write-key ≡ read-key，不会漂移。
pub fn oauth_billing_unit(
    provider: &crate::provider::Provider,
    app_type: &str,
    account_id: &str,
) -> BillingUnit {
    BillingUnit {
        id: format!("acct:{account_id}"),
        kind: BillingKind::OauthAccount,
        app_type: app_type.to_string(),
        label: format!("codex · {account_id}"),
        members: vec![provider.id.clone()],
    }
}

/// 解析一个 codex_oauth provider **实际所用**的 account_id，逻辑与 forwarder 写侧
/// (`get_codex_oauth_token` → `codex_oauth_account_id`) 完全一致：
///   1. provider.meta 显式绑定 → 用该 account_id；
///   2. 否则取托管认证注册表里 codex 的 default account（即 forwarder `None` 分支
///      `auth.default_account_id().await` 的结果，落到真实账号 UUID）。
/// 都拿不到时回退占位 `"default"`（此时通常也没有快照，读出来是"暂无快照"）。
pub async fn resolve_codex_account_id(
    provider: &crate::provider::Provider,
    managed_auth: &crate::proxy::server::ManagedAuthRegistry,
) -> String {
    if let Some(id) = provider
        .meta
        .as_ref()
        .and_then(|m| m.managed_account_id_for("codex_oauth"))
    {
        return id;
    }
    if let Some(manager) = &managed_auth.codex_oauth {
        if let Some(id) = manager.read().await.default_account_id().await {
            return id;
        }
    }
    "default".to_string()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum BillingKind {
    ApiKey,
    OauthAccount,
}

#[derive(Debug, Clone, Serialize)]
pub struct BillingUnit {
    pub id: String,
    pub kind: BillingKind,
    pub app_type: String,
    pub label: String,
    pub members: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct CodexQuotaSnapshot {
    pub plan_type: String,
    pub primary_used_percent: f64,
    pub primary_window_minutes: u32,
    pub primary_reset_after_seconds: i64,
    pub secondary_used_percent: f64,
    pub secondary_window_minutes: u32,
    pub secondary_reset_after_seconds: i64,
    pub captured_at: i64,
}

fn hdr_str(h: &http::HeaderMap, k: &str) -> Option<String> {
    h.get(k)
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string())
}
fn hdr_f64(h: &http::HeaderMap, k: &str) -> Option<f64> {
    hdr_str(h, k).and_then(|s| s.parse().ok())
}
fn hdr_u32(h: &http::HeaderMap, k: &str) -> Option<u32> {
    hdr_str(h, k).and_then(|s| s.parse().ok())
}
fn hdr_i64(h: &http::HeaderMap, k: &str) -> Option<i64> {
    hdr_str(h, k).and_then(|s| s.parse().ok())
}

/// 解析 codex 限流头。无任何 x-codex-primary-* → None。
pub fn parse_codex_headers(h: &http::HeaderMap, now_unix: i64) -> Option<CodexQuotaSnapshot> {
    let primary_used_percent = hdr_f64(h, "x-codex-primary-used-percent")?;
    Some(CodexQuotaSnapshot {
        plan_type: hdr_str(h, "x-codex-plan-type").unwrap_or_default(),
        primary_used_percent,
        primary_window_minutes: hdr_u32(h, "x-codex-primary-window-minutes").unwrap_or(0),
        primary_reset_after_seconds: hdr_i64(h, "x-codex-primary-reset-after-seconds").unwrap_or(0),
        secondary_used_percent: hdr_f64(h, "x-codex-secondary-used-percent").unwrap_or(0.0),
        secondary_window_minutes: hdr_u32(h, "x-codex-secondary-window-minutes").unwrap_or(0),
        secondary_reset_after_seconds: hdr_i64(h, "x-codex-secondary-reset-after-seconds")
            .unwrap_or(0),
        captured_at: now_unix,
    })
}

use crate::provider::{UsageData, UsageResult};

/// 解析 {env:NAME} 占位符为环境变量值；非占位符原样返回
pub fn resolve_env_reference(raw: &str) -> String {
    let t = raw.trim();
    if let Some(name) = t.strip_prefix("{env:").and_then(|v| v.strip_suffix('}')) {
        return std::env::var(name).unwrap_or_else(|_| raw.to_string());
    }
    raw.to_string()
}

fn human_secs(secs: i64) -> String {
    if secs <= 0 {
        return "已重置".into();
    }
    let h = secs / 3600;
    if h >= 24 {
        format!("{}d后重置", h / 24)
    } else if h >= 1 {
        format!("{}h后重置", h)
    } else {
        format!("{}m后重置", secs / 60)
    }
}

pub fn snapshot_to_usage_result(s: &CodexQuotaSnapshot) -> UsageResult {
    let mk = |used: f64, win_min: u32, reset: i64, tag: &str| UsageData {
        plan_name: Some(s.plan_type.clone()),
        extra: Some(format!("{tag} {}min窗 · {}", win_min, human_secs(reset))),
        is_valid: Some(true),
        invalid_message: None,
        total: Some(100.0),
        used: Some(used),
        remaining: Some(100.0 - used),
        unit: Some("%".to_string()),
    };
    UsageResult {
        success: true,
        data: Some(vec![
            mk(
                s.primary_used_percent,
                s.primary_window_minutes,
                s.primary_reset_after_seconds,
                "primary",
            ),
            mk(
                s.secondary_used_percent,
                s.secondary_window_minutes,
                s.secondary_reset_after_seconds,
                "secondary",
            ),
        ]),
        error: None,
    }
}

// ============================================================================
// HTTP handler — GET /quota
// ============================================================================

use axum::{
    extract::{Query, State},
    response::IntoResponse,
    Json,
};
use serde::Deserialize;

/// 查询参数：可选按 unit id 或 provider id 过滤
#[derive(Deserialize, Default)]
pub struct QuotaParams {
    pub provider: Option<String>,
    pub unit: Option<String>,
}

#[derive(Serialize)]
struct QuotaEntry {
    #[serde(flatten)]
    unit: BillingUnit,
    quota: crate::provider::UsageResult,
}

#[derive(Serialize)]
struct QuotaResponse {
    billing_units: Vec<QuotaEntry>,
}

/// GET /quota — 枚举所有 app_type 的 providers，去重为计费单元，返回各单元 quota。
pub async fn handle_quota(
    State(state): State<crate::proxy::server::ProxyState>,
    Query(params): Query<QuotaParams>,
) -> impl IntoResponse {
    use crate::app_config::AppType;

    // 1. 枚举所有 app_type 下的 providers，克隆为 owned Vec（避免借用跨 await）
    let mut owned: Vec<(String, Vec<crate::provider::Provider>)> = Vec::new();
    for app in AppType::all() {
        if let Ok(map) = state.db.get_all_providers(app.as_str()) {
            owned.push((app.as_str().to_string(), map.into_values().collect()));
        }
    }

    // 2. 派生计费单元并去重。codex_oauth 单元的 account_id 必须经托管认证注册表异步解析，
    //    与 forwarder 写快照的 key 用同一套逻辑（resolve_codex_account_id），保证读写 key 一致。
    let mut order: Vec<String> = Vec::new();
    let mut map: std::collections::HashMap<String, BillingUnit> = std::collections::HashMap::new();
    for (app_type, providers) in &owned {
        for p in providers {
            let Some(unit) = billing_unit(p, app_type) else {
                continue;
            };
            let unit = match unit.kind {
                BillingKind::OauthAccount => {
                    // 重算与转发写侧对齐的真实 account_id（占位 "default" 会被替换为 UUID）
                    let acct = resolve_codex_account_id(p, &state.managed_auth).await;
                    oauth_billing_unit(p, app_type, &acct)
                }
                BillingKind::ApiKey => unit,
            };
            if let Some(existing) = map.get_mut(&unit.id) {
                // 同 id 合并 members；跨 app_type 同 AK 的单元保留首次迭代到的 app_type（design §2.2）。
                for m in &unit.members {
                    if !existing.members.contains(m) {
                        existing.members.push(m.clone());
                    }
                }
            } else {
                order.push(unit.id.clone());
                map.insert(unit.id.clone(), unit);
            }
        }
    }
    let mut units: Vec<BillingUnit> = order.into_iter().filter_map(|id| map.remove(&id)).collect();

    // 3. 可选过滤
    if let Some(uid) = &params.unit {
        units.retain(|u| &u.id == uid);
    }
    if let Some(pid) = &params.provider {
        units.retain(|u| u.members.contains(pid));
    }
    if (params.unit.is_some() || params.provider.is_some()) && units.is_empty() {
        // design §5：按实际给的筛选参数区分错误信息（?unit → unknown unit，?provider → unknown provider）。
        let error = if params.unit.is_some() {
            "unknown unit"
        } else {
            "unknown provider"
        };
        return (
            axum::http::StatusCode::NOT_FOUND,
            Json(serde_json::json!({ "error": error })),
        )
            .into_response();
    }

    // 4. 逐单元查询 quota
    let mut entries: Vec<QuotaEntry> = Vec::with_capacity(units.len());
    for u in units {
        let quota = match u.kind {
            BillingKind::OauthAccount => {
                // acct:<account_id> → 读内存快照
                let acct = u.id.strip_prefix("acct:").unwrap_or(&u.id).to_string();
                match state.codex_quota.read().await.get(&acct) {
                    Some(snap) => snapshot_to_usage_result(snap),
                    None => crate::provider::UsageResult {
                        success: false,
                        data: None,
                        error: Some("暂无快照，先发一次 Codex 请求".to_string()),
                    },
                }
            }
            BillingKind::ApiKey => {
                // 用第一个 member provider 执行 usage_script（同单元凭据相同）
                let pid = u.members.first().cloned().unwrap_or_default();
                // 找对应的 AppType
                let app = AppType::all()
                    .find(|a| a.as_str() == u.app_type)
                    .unwrap_or(AppType::Codex);
                match crate::services::provider::usage::query_usage_with_db(&state.db, app, &pid)
                    .await
                {
                    Ok(r) => r,
                    Err(e) => crate::provider::UsageResult {
                        success: false,
                        data: None,
                        error: Some(e.to_string()),
                    },
                }
            }
        };
        entries.push(QuotaEntry { unit: u, quota });
    }

    Json(QuotaResponse {
        billing_units: entries,
    })
    .into_response()
}

// ============================================================================
// collect_units — 纯函数，枚举去重
// ============================================================================

/// 枚举多个 app_type 下的 providers，合并同一计费单元（相同 id 的去重并合并 members）。
/// 纯函数，便于单测。
pub fn collect_units(by_app: &[(String, Vec<&crate::provider::Provider>)]) -> Vec<BillingUnit> {
    let mut order: Vec<String> = Vec::new();
    let mut map: std::collections::HashMap<String, BillingUnit> = std::collections::HashMap::new();
    for (app_type, providers) in by_app {
        for p in providers {
            if let Some(u) = billing_unit(p, app_type) {
                if let Some(existing) = map.get_mut(&u.id) {
                    // 同 id：合并 members（追加去重）。注意：跨 app_type 但同一把 AK 的单元会落到
                    // 同一个 id，此时**保留首次迭代到的 app_type**（design §2.2：合并的是计费视角，
                    // 不是 provider，首个 app_type 即代表该单元）。
                    for m in &u.members {
                        if !existing.members.contains(m) {
                            existing.members.push(m.clone());
                        }
                    }
                } else {
                    order.push(u.id.clone());
                    map.insert(u.id.clone(), u);
                }
            }
        }
    }
    order.into_iter().filter_map(|id| map.remove(&id)).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn mk_provider(
        id: &str,
        meta: serde_json::Value,
        settings: serde_json::Value,
    ) -> crate::provider::Provider {
        crate::provider::Provider {
            id: id.to_string(),
            name: id.to_string(),
            settings_config: settings,
            website_url: None,
            category: None,
            created_at: None,
            sort_index: None,
            notes: None,
            meta: serde_json::from_value(meta).ok(),
            icon: None,
            icon_color: None,
            in_failover_queue: false,
        }
    }

    #[test]
    fn dedup_same_ak_merges_members() {
        let s = json!({"base_url":"https://lingzhi.agibot.com/v1","apiKey":"sk-same000111"});
        let p1 = mk_provider(
            "lingzhi",
            json!({"usage_script":{"enabled":true,"language":"js","code":""}}),
            s.clone(),
        );
        let p2 = mk_provider(
            "lingzhi-copy",
            json!({"usage_script":{"enabled":true,"language":"js","code":""}}),
            s.clone(),
        );
        let units = collect_units(&[("codex".to_string(), vec![&p1, &p2])]);
        assert_eq!(units.len(), 1, "同 AK 去重为一个单元");
        assert_eq!(units[0].members.len(), 2);
        assert!(units[0].members.contains(&"lingzhi".to_string()));
        assert!(units[0].members.contains(&"lingzhi-copy".to_string()));
    }

    #[test]
    fn bearer_same_key_same_id() {
        let s = json!({"base_url":"https://lingzhi.agibot.com/v1","apiKey":"sk-abcdef123456"});
        let a = billing_unit(
            &mk_provider(
                "lingzhi",
                json!({"usage_script":{"enabled":true,"language":"js","code":""}}),
                s.clone(),
            ),
            "codex",
        )
        .unwrap();
        let b = billing_unit(
            &mk_provider(
                "lingzhi-copy",
                json!({"usage_script":{"enabled":true,"language":"js","code":""}}),
                s.clone(),
            ),
            "codex",
        )
        .unwrap();
        assert_eq!(a.id, b.id, "同 baseUrl+AK 必须算出同一个 id");
        assert!(a.id.starts_with("key:lingzhi.agibot.com:"));
        assert_eq!(a.kind, BillingKind::ApiKey);
        assert!(
            a.label.contains("sk-abc") && a.label.contains("3456"),
            "label 须打码: {}",
            a.label
        );
        assert!(!a.label.contains("abcdef123456"), "label 不得含完整 AK");
    }

    #[test]
    fn bearer_diff_key_diff_id() {
        let s1 = json!({"base_url":"https://lingzhi.agibot.com/v1","apiKey":"sk-aaaa1111"});
        let s2 = json!({"base_url":"https://lingzhi.agibot.com/v1","apiKey":"sk-bbbb2222"});
        let a = billing_unit(
            &mk_provider(
                "p1",
                json!({"usage_script":{"enabled":true,"language":"js","code":""}}),
                s1,
            ),
            "codex",
        )
        .unwrap();
        let b = billing_unit(
            &mk_provider(
                "p2",
                json!({"usage_script":{"enabled":true,"language":"js","code":""}}),
                s2,
            ),
            "codex",
        )
        .unwrap();
        assert_ne!(a.id, b.id);
    }

    #[test]
    fn no_quota_source_returns_none() {
        let s = json!({"base_url":"https://x.com","apiKey":"sk-x"});
        let p = mk_provider("plain", json!({}), s);
        assert!(
            billing_unit(&p, "claude").is_none(),
            "无 usage_script 且非 codex_oauth → None"
        );
    }

    #[test]
    fn parse_codex_headers_real_sample() {
        let mut h = http::HeaderMap::new();
        h.insert("x-codex-plan-type", "prolite".parse().unwrap());
        h.insert("x-codex-primary-used-percent", "0".parse().unwrap());
        h.insert("x-codex-primary-window-minutes", "300".parse().unwrap());
        h.insert(
            "x-codex-primary-reset-after-seconds",
            "18000".parse().unwrap(),
        );
        h.insert("x-codex-secondary-used-percent", "2".parse().unwrap());
        h.insert("x-codex-secondary-window-minutes", "10080".parse().unwrap());
        h.insert(
            "x-codex-secondary-reset-after-seconds",
            "528493".parse().unwrap(),
        );
        let s = parse_codex_headers(&h, 1_700_000_000).expect("应解析出快照");
        assert_eq!(s.plan_type, "prolite");
        assert_eq!(s.primary_used_percent, 0.0);
        assert_eq!(s.primary_window_minutes, 300);
        assert_eq!(s.secondary_used_percent, 2.0);
        assert_eq!(s.secondary_window_minutes, 10080);
        assert_eq!(s.captured_at, 1_700_000_000);
    }

    #[test]
    fn parse_codex_headers_absent_returns_none() {
        let h = http::HeaderMap::new();
        assert!(parse_codex_headers(&h, 0).is_none());
    }

    /// C1 回归：codex_oauth provider 无显式绑定时，读侧解析出的 account_id 必须等于托管认证
    /// 注册表的 default account（真实 UUID），而不是字面量 "default"——否则读 key ≠ 写 key，
    /// default 账号的 codex 快照永远查不到。
    ///
    /// forwarder 写侧（forwarder.rs ~1300）逻辑：
    ///   account_id = meta.managed_account_id_for("codex_oauth")  // None
    ///   used_account = (None 分支) auth.default_account_id().await // → 真实 UUID
    ///   snapshot 以 used_account 为 key 写入。
    /// 本测试断言 resolve_codex_account_id 走出完全相同的结果。
    #[tokio::test]
    async fn codex_read_key_matches_managed_default_uuid() {
        use crate::proxy::providers::codex_oauth_auth::CodexOAuthManager;
        use crate::proxy::server::ManagedAuthRegistry;
        use std::sync::Arc;
        use tokio::sync::RwLock;

        let uuid = "8f4be7fc-1234-4abc-9def-aaaabbbbcccc";
        let dir = tempfile::tempdir().unwrap();
        // 写一个含单账号 + 显式 default 的存储文件，让 manager 加载出真实 UUID 作为 default。
        let store = json!({
            "version": 1,
            "accounts": {
                uuid: {
                    "account_id": uuid,
                    "email": "archerus007@gmail.com",
                    "refresh_token": "rt-test",
                    "authenticated_at": 1_700_000_000i64
                }
            },
            "default_account_id": uuid
        });
        std::fs::write(
            dir.path().join("codex_oauth_auth.json"),
            serde_json::to_string(&store).unwrap(),
        )
        .unwrap();

        let manager = CodexOAuthManager::new(dir.path().to_path_buf());
        // sanity：manager 自身解析的 default == UUID（即 forwarder None 分支拿到的值）
        assert_eq!(manager.default_account_id().await.as_deref(), Some(uuid));

        let registry = ManagedAuthRegistry {
            codex_oauth: Some(Arc::new(RwLock::new(manager))),
            copilot: None,
        };

        // codex_oauth provider，无显式 authBinding（走 default 解析）
        let provider = mk_provider("gpt", json!({"providerType": "codex_oauth"}), json!({}));
        assert!(provider.is_codex_oauth());

        let resolved = resolve_codex_account_id(&provider, &registry).await;
        assert_eq!(resolved, uuid, "读侧必须解析到真实 UUID，而非 'default'");
        assert_ne!(resolved, "default");

        // 读侧据此构造的单元 id == 写侧快照 key（acct:<UUID>）
        let unit = oauth_billing_unit(&provider, "codex", &resolved);
        assert_eq!(unit.id, format!("acct:{uuid}"));
        // 快照存储 key 即去掉 "acct:" 前缀，应回到 UUID（与 forwarder 写入 key 一致）
        assert_eq!(unit.id.strip_prefix("acct:"), Some(uuid));
    }

    #[test]
    fn snapshot_maps_to_two_windows() {
        let s = CodexQuotaSnapshot {
            plan_type: "prolite".into(),
            primary_used_percent: 0.0,
            primary_window_minutes: 300,
            primary_reset_after_seconds: 18000,
            secondary_used_percent: 2.0,
            secondary_window_minutes: 10080,
            secondary_reset_after_seconds: 528493,
            captured_at: 0,
        };
        let r = snapshot_to_usage_result(&s);
        assert!(r.success);
        let data = r.data.expect("有 data");
        assert_eq!(data.len(), 2, "主+次两个窗口");
        assert_eq!(data[0].remaining, Some(100.0));
        assert_eq!(data[0].unit.as_deref(), Some("%"));
        assert_eq!(data[1].remaining, Some(98.0));
    }
}
