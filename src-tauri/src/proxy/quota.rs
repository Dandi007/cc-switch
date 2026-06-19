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
    let fp = digest.iter().map(|b| format!("{b:02x}")).collect::<String>();
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
pub fn billing_unit(provider: &crate::provider::Provider, app_type: &str) -> Option<BillingUnit> {
    if provider.is_codex_oauth() {
        let account_id = provider
            .meta
            .as_ref()
            .and_then(|m| m.managed_account_id_for("codex_oauth"))
            .unwrap_or_else(|| "default".to_string());
        return Some(BillingUnit {
            id: format!("acct:{account_id}"),
            kind: BillingKind::OauthAccount,
            app_type: app_type.to_string(),
            label: format!("codex · {account_id}"),
            members: vec![provider.id.clone()],
        });
    }
    if provider.has_usage_script_enabled() {
        let base_url = provider_base_url(provider).unwrap_or_default();
        let raw_ak = provider_api_key_raw(provider).unwrap_or_default();
        let fp = sha_fp(&base_url, &raw_ak);
        let host = host_of(&base_url);
        return Some(BillingUnit {
            id: format!("key:{host}:{fp}"),
            kind: BillingKind::ApiKey,
            app_type: app_type.to_string(),
            label: format!("{host} · {}", mask_key(&raw_ak)),
            members: vec![provider.id.clone()],
        });
    }
    None
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
    h.get(k).and_then(|v| v.to_str().ok()).map(|s| s.to_string())
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
        secondary_reset_after_seconds: hdr_i64(h, "x-codex-secondary-reset-after-seconds").unwrap_or(0),
        captured_at: now_unix,
    })
}

use crate::provider::{UsageData, UsageResult};

fn human_secs(secs: i64) -> String {
    if secs <= 0 { return "已重置".into(); }
    let h = secs / 3600;
    if h >= 24 { format!("{}d后重置", h / 24) }
    else if h >= 1 { format!("{}h后重置", h) }
    else { format!("{}m后重置", secs / 60) }
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
            mk(s.primary_used_percent, s.primary_window_minutes, s.primary_reset_after_seconds, "primary"),
            mk(s.secondary_used_percent, s.secondary_window_minutes, s.secondary_reset_after_seconds, "secondary"),
        ]),
        error: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn mk_provider(id: &str, meta: serde_json::Value, settings: serde_json::Value) -> crate::provider::Provider {
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
    fn bearer_same_key_same_id() {
        let s = json!({"base_url":"https://lingzhi.agibot.com/v1","apiKey":"sk-abcdef123456"});
        let a = billing_unit(&mk_provider("lingzhi", json!({"usage_script":{"enabled":true,"language":"js","code":""}}), s.clone()), "codex").unwrap();
        let b = billing_unit(&mk_provider("lingzhi-copy", json!({"usage_script":{"enabled":true,"language":"js","code":""}}), s.clone()), "codex").unwrap();
        assert_eq!(a.id, b.id, "同 baseUrl+AK 必须算出同一个 id");
        assert!(a.id.starts_with("key:lingzhi.agibot.com:"));
        assert_eq!(a.kind, BillingKind::ApiKey);
        assert!(a.label.contains("sk-abc") && a.label.contains("3456"), "label 须打码: {}", a.label);
        assert!(!a.label.contains("abcdef123456"), "label 不得含完整 AK");
    }

    #[test]
    fn bearer_diff_key_diff_id() {
        let s1 = json!({"base_url":"https://lingzhi.agibot.com/v1","apiKey":"sk-aaaa1111"});
        let s2 = json!({"base_url":"https://lingzhi.agibot.com/v1","apiKey":"sk-bbbb2222"});
        let a = billing_unit(&mk_provider("p1", json!({"usage_script":{"enabled":true,"language":"js","code":""}}), s1), "codex").unwrap();
        let b = billing_unit(&mk_provider("p2", json!({"usage_script":{"enabled":true,"language":"js","code":""}}), s2), "codex").unwrap();
        assert_ne!(a.id, b.id);
    }

    #[test]
    fn no_quota_source_returns_none() {
        let s = json!({"base_url":"https://x.com","apiKey":"sk-x"});
        let p = mk_provider("plain", json!({}), s);
        assert!(billing_unit(&p, "claude").is_none(), "无 usage_script 且非 codex_oauth → None");
    }

    #[test]
    fn parse_codex_headers_real_sample() {
        let mut h = http::HeaderMap::new();
        h.insert("x-codex-plan-type", "prolite".parse().unwrap());
        h.insert("x-codex-primary-used-percent", "0".parse().unwrap());
        h.insert("x-codex-primary-window-minutes", "300".parse().unwrap());
        h.insert("x-codex-primary-reset-after-seconds", "18000".parse().unwrap());
        h.insert("x-codex-secondary-used-percent", "2".parse().unwrap());
        h.insert("x-codex-secondary-window-minutes", "10080".parse().unwrap());
        h.insert("x-codex-secondary-reset-after-seconds", "528493".parse().unwrap());
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
