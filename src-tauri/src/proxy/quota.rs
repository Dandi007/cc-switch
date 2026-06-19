use serde::Serialize;
use sha2::{Digest, Sha256};

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
}
