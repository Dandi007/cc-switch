//! Headless runtime bootstrap for non-Tauri entrypoints.
//!
//! The CLI uses the same database, provider, MCP, prompt, proxy and auth
//! services as the desktop app, but skips window, tray, dialog and deep-link
//! setup.

use crate::app_config::AppType;
use crate::database::Database;
use crate::error::AppError;
use crate::provider::{AuthBinding, AuthBindingSource, Provider, ProviderMeta};
use crate::proxy::providers::codex_oauth_auth::{CodexOAuthManager, CodexOAuthStatus};
use crate::proxy::providers::copilot_auth::{
    CopilotAuthManager, CopilotAuthStatus, GitHubAccount as CopilotAccount,
    GitHubAccount as CodexOAuthAccount, GitHubDeviceCodeResponse,
};
use crate::proxy::server::ManagedAuthRegistry;
use crate::store::AppState;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::BTreeSet;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::RwLock;

#[derive(Debug, Clone, Default)]
pub struct HeadlessOptions {
    pub config_dir: Option<PathBuf>,
    pub recover_proxy: bool,
}

#[derive(Debug, Serialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct HeadlessInitReport {
    pub migrated_from_json: bool,
    pub default_skill_repos: usize,
    pub official_providers: usize,
    pub imported_live_providers: usize,
    pub imported_mcp_servers: usize,
    pub imported_prompts: usize,
}

pub struct HeadlessApp {
    pub state: AppState,
    pub report: HeadlessInitReport,
    codex_oauth: Arc<RwLock<CodexOAuthManager>>,
    copilot: Arc<RwLock<CopilotAuthManager>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ManagedAuthProvider {
    CodexOAuth,
    GitHubCopilot,
}

impl ManagedAuthProvider {
    pub fn parse(raw: &str) -> Result<Self, AppError> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "codex_oauth" | "codex-oauth" | "codex" => Ok(Self::CodexOAuth),
            "github_copilot" | "github-copilot" | "copilot" => Ok(Self::GitHubCopilot),
            other => Err(AppError::Message(format!(
                "unsupported auth provider: {other}. allowed: codex_oauth, github_copilot"
            ))),
        }
    }
}

#[derive(Debug, Serialize)]
#[serde(untagged)]
pub enum ManagedAuthStatus {
    Codex(CodexOAuthStatus),
    Copilot(CopilotAuthStatus),
}

#[derive(Debug, Serialize)]
#[serde(untagged)]
pub enum ManagedAuthAccount {
    Codex(CodexOAuthAccount),
    Copilot(CopilotAccount),
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct OpenCodeImportResult {
    pub provider_id: String,
    pub family: String,
    pub app: String,
    pub models: Vec<String>,
    pub changed: bool,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct OpenCodeOAuthImportResult {
    pub account_id: String,
    pub provider_id: String,
    pub family: String,
    pub models: Vec<String>,
}

#[derive(Debug, Deserialize)]
struct OpenCodeConfig {
    #[serde(default)]
    provider: std::collections::HashMap<String, OpenCodeProviderConfig>,
}

#[derive(Debug, Deserialize)]
struct OpenCodeProviderConfig {
    #[serde(default)]
    npm: Option<String>,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    options: OpenCodeProviderOptions,
    #[serde(default)]
    models: std::collections::HashMap<String, Value>,
}

#[derive(Debug, Default, Deserialize)]
struct OpenCodeProviderOptions {
    #[serde(default)]
    #[serde(rename = "baseURL", alias = "base_url")]
    base_url: Option<String>,
    #[serde(default)]
    #[serde(rename = "apiKey", alias = "api_key")]
    api_key: Option<String>,
}

#[derive(Debug, Deserialize)]
struct OpenCodeAuthFile {
    #[serde(flatten)]
    entries: std::collections::HashMap<String, OpenCodeAuthEntry>,
}

#[derive(Debug, Deserialize)]
struct OpenCodeAuthEntry {
    #[serde(rename = "type")]
    kind: String,
    #[serde(default)]
    refresh: Option<String>,
    #[serde(default, rename = "accountId")]
    account_id: Option<String>,
    #[serde(default)]
    email: Option<String>,
}

fn opencode_config_path() -> Result<PathBuf, AppError> {
    if let Ok(xdg) = std::env::var("XDG_CONFIG_HOME") {
        return Ok(PathBuf::from(xdg).join("opencode/opencode.json"));
    }
    let home =
        dirs::home_dir().ok_or_else(|| AppError::Message("无法定位用户 home 目录".to_string()))?;
    Ok(home.join(".config/opencode/opencode.json"))
}

fn opencode_auth_path() -> Result<PathBuf, AppError> {
    if let Ok(xdg) = std::env::var("XDG_DATA_HOME") {
        return Ok(PathBuf::from(xdg).join("opencode/auth.json"));
    }
    let home =
        dirs::home_dir().ok_or_else(|| AppError::Message("无法定位用户 home 目录".to_string()))?;
    Ok(home.join(".local/share/opencode/auth.json"))
}

fn read_json_path<T: for<'de> Deserialize<'de>>(path: PathBuf) -> Result<T, AppError> {
    let content = std::fs::read_to_string(&path)
        .map_err(|e| AppError::Message(format!("读取 {} 失败: {e}", path.display())))?;
    serde_json::from_str(&content)
        .map_err(|e| AppError::Message(format!("解析 {} 失败: {e}", path.display())))
}

fn fallback_gpt_models() -> Vec<String> {
    vec![
        "gpt-5.5".to_string(),
        "gpt-5.4".to_string(),
        "gpt-5.4-mini".to_string(),
        "gpt-5.4-mini-fast".to_string(),
        "gpt-5.3-codex".to_string(),
    ]
}

fn init_global_proxy_client(db: &Database) {
    let proxy_url = db.get_global_proxy_url().ok().flatten();
    if let Err(e) = crate::proxy::http_client::init(proxy_url.as_deref()) {
        log::error!("[GlobalProxy] failed to initialize saved config: {e}");
        if proxy_url.is_some() {
            if let Err(clear_err) = db.set_global_proxy_url(None) {
                log::error!("[GlobalProxy] failed to clear invalid config: {clear_err}");
            }
        }
        if let Err(fallback_err) = crate::proxy::http_client::init(None) {
            log::error!("[GlobalProxy] failed to initialize direct mode: {fallback_err}");
        }
    }
}

fn migrate_legacy_json_if_needed(
    db: &Database,
    had_db_before_init: bool,
    had_json_before_init: bool,
) -> Result<bool, AppError> {
    let app_config_dir = crate::config::get_app_config_dir();
    let json_path = app_config_dir.join("config.json");

    if had_db_before_init || !had_json_before_init {
        return Ok(false);
    }

    let config = crate::app_config::MultiAppConfig::load()?;
    db.migrate_from_json(&config)?;
    let archive_path = json_path.with_extension("json.migrated");
    if let Err(e) = std::fs::rename(&json_path, &archive_path) {
        log::warn!("failed to archive legacy config.json: {e}");
    }
    Ok(true)
}

fn init_providers(state: &AppState, report: &mut HeadlessInitReport) {
    match state.db.init_default_skill_repos() {
        Ok(count) => report.default_skill_repos = count,
        Err(e) => log::warn!("failed to initialize default skill repos: {e}"),
    }

    match state.db.get_setting("skills_ssot_migration_pending") {
        Ok(Some(flag)) if flag == "true" || flag == "1" => {
            let has_existing = state
                .db
                .get_all_installed_skills()
                .map(|skills| !skills.is_empty())
                .unwrap_or(false);
            if has_existing {
                let _ = state
                    .db
                    .set_setting("skills_ssot_migration_pending", "false");
            } else {
                match crate::services::skill::migrate_skills_to_ssot(&state.db) {
                    Ok(_) => {
                        let _ = state
                            .db
                            .set_setting("skills_ssot_migration_pending", "false");
                    }
                    Err(e) => log::warn!("failed to migrate legacy skills: {e}"),
                }
            }
        }
        Ok(_) => {}
        Err(e) => log::warn!("failed to read skills migration flag: {e}"),
    }

    for app_type in AppType::all().filter(|t| !t.is_additive_mode()) {
        let should_import =
            crate::services::provider::should_import_default_config_on_startup(state, &app_type)
                .unwrap_or(false);
        if should_import {
            match crate::services::provider::import_default_config(state, app_type) {
                Ok(true) => report.imported_live_providers += 1,
                Ok(false) => {}
                Err(e) => log::debug!("no live provider imported: {e}"),
            }
        }
    }

    match state.db.init_default_official_providers() {
        Ok(count) => report.official_providers = count,
        Err(e) => log::warn!("failed to seed official providers: {e}"),
    }

    for importer in [
        crate::services::provider::import_opencode_providers_from_live,
        crate::services::provider::import_openclaw_providers_from_live,
        crate::services::provider::import_hermes_providers_from_live,
    ] {
        match importer(state) {
            Ok(count) => report.imported_live_providers += count,
            Err(e) => log::warn!("failed to import additive live providers: {e}"),
        }
    }

    for variant in [&crate::services::omo::STANDARD, &crate::services::omo::SLIM] {
        let has_provider = state
            .db
            .get_all_providers("opencode")
            .map(|providers| {
                providers
                    .values()
                    .any(|p| p.category.as_deref() == Some(variant.category))
            })
            .unwrap_or(false);
        if !has_provider {
            match crate::services::OmoService::import_from_local(state, variant) {
                Ok(_) => report.imported_live_providers += 1,
                Err(AppError::OmoConfigNotFound) => {}
                Err(e) => log::warn!("failed to import {} config: {e}", variant.label),
            }
        }
    }
}

fn init_mcp_and_prompts(state: &AppState, report: &mut HeadlessInitReport) {
    if state.db.is_mcp_table_empty().unwrap_or(false) {
        for importer in [
            crate::services::mcp::McpService::import_from_claude,
            crate::services::mcp::McpService::import_from_codex,
            crate::services::mcp::McpService::import_from_gemini,
            crate::services::mcp::McpService::import_from_opencode,
            crate::services::mcp::McpService::import_from_hermes,
        ] {
            match importer(state) {
                Ok(count) => report.imported_mcp_servers += count,
                Err(e) => log::warn!("failed to import MCP servers: {e}"),
            }
        }
    }

    if state.db.is_prompts_table_empty().unwrap_or(false) {
        for app in [
            AppType::Claude,
            AppType::Codex,
            AppType::Gemini,
            AppType::OpenCode,
            AppType::OpenClaw,
            AppType::Hermes,
        ] {
            match crate::services::prompt::PromptService::import_from_file_on_first_launch(
                state, app,
            ) {
                Ok(count) => report.imported_prompts += count,
                Err(e) => log::warn!("failed to import prompt: {e}"),
            }
        }
    }
}

impl HeadlessApp {
    pub async fn init(options: HeadlessOptions) -> Result<Self, AppError> {
        let _ = rustls::crypto::ring::default_provider().install_default();
        crate::panic_hook::setup_panic_hook();

        if let Some(path) = options.config_dir {
            crate::app_store::set_app_config_dir_override_for_process(Some(path));
        }
        crate::panic_hook::init_app_config_dir(crate::config::get_app_config_dir());

        let app_config_dir = crate::config::get_app_config_dir();
        let had_db_before_init = app_config_dir.join("cc-switch.db").exists();
        let had_json_before_init = app_config_dir.join("config.json").exists();

        let db = Arc::new(Database::init()?);
        let mut report = HeadlessInitReport {
            migrated_from_json: migrate_legacy_json_if_needed(
                &db,
                had_db_before_init,
                had_json_before_init,
            )?,
            ..Default::default()
        };
        let state = AppState::new(db);

        init_providers(&state, &mut report);
        init_mcp_and_prompts(&state, &mut report);
        init_global_proxy_client(&state.db);

        if options.recover_proxy {
            let has_backups = state.db.has_any_live_backup().await.unwrap_or(false);
            let live_taken_over = state.proxy_service.detect_takeover_in_live_configs();
            if has_backups || live_taken_over {
                if let Err(e) = state.proxy_service.recover_from_crash().await {
                    log::warn!("failed to recover live proxy takeover state: {e}");
                }
            }
        }

        let app_config_dir = crate::config::get_app_config_dir();
        let codex_oauth = Arc::new(RwLock::new(CodexOAuthManager::new(app_config_dir.clone())));
        let copilot = Arc::new(RwLock::new(CopilotAuthManager::new(app_config_dir)));
        state
            .proxy_service
            .set_managed_auth_registry(ManagedAuthRegistry {
                codex_oauth: Some(codex_oauth.clone()),
                copilot: Some(copilot.clone()),
            });

        Ok(Self {
            state,
            report,
            codex_oauth,
            copilot,
        })
    }

    pub async fn auth_start_login(
        &self,
        provider: ManagedAuthProvider,
    ) -> Result<GitHubDeviceCodeResponse, String> {
        match provider {
            ManagedAuthProvider::CodexOAuth => self
                .codex_oauth
                .read()
                .await
                .start_device_flow()
                .await
                .map_err(|e| e.to_string()),
            ManagedAuthProvider::GitHubCopilot => self
                .copilot
                .read()
                .await
                .start_device_flow(None)
                .await
                .map_err(|e| e.to_string()),
        }
    }

    pub async fn auth_poll(
        &self,
        provider: ManagedAuthProvider,
        device_code: &str,
    ) -> Result<Option<ManagedAuthAccount>, String> {
        match provider {
            ManagedAuthProvider::CodexOAuth => {
                match self
                    .codex_oauth
                    .write()
                    .await
                    .poll_for_token(device_code)
                    .await
                {
                    Ok(Some(account)) => Ok(Some(ManagedAuthAccount::Codex(account))),
                    Ok(None) => Ok(None),
                    Err(e) if e.to_string().contains("authorization_pending") => Ok(None),
                    Err(e) => Err(e.to_string()),
                }
            }
            ManagedAuthProvider::GitHubCopilot => {
                match self
                    .copilot
                    .write()
                    .await
                    .poll_for_token(device_code, None)
                    .await
                {
                    Ok(Some(account)) => Ok(Some(ManagedAuthAccount::Copilot(account))),
                    Ok(None) => Ok(None),
                    Err(e) if e.to_string().contains("authorization_pending") => Ok(None),
                    Err(e) => Err(e.to_string()),
                }
            }
        }
    }

    pub async fn auth_status(
        &self,
        provider: ManagedAuthProvider,
    ) -> Result<ManagedAuthStatus, String> {
        Ok(match provider {
            ManagedAuthProvider::CodexOAuth => {
                ManagedAuthStatus::Codex(self.codex_oauth.read().await.get_status().await)
            }
            ManagedAuthProvider::GitHubCopilot => {
                ManagedAuthStatus::Copilot(self.copilot.read().await.get_status().await)
            }
        })
    }

    pub async fn auth_list(
        &self,
        provider: ManagedAuthProvider,
    ) -> Result<Vec<ManagedAuthAccount>, String> {
        Ok(match provider {
            ManagedAuthProvider::CodexOAuth => self
                .codex_oauth
                .read()
                .await
                .list_accounts()
                .await
                .into_iter()
                .map(ManagedAuthAccount::Codex)
                .collect(),
            ManagedAuthProvider::GitHubCopilot => self
                .copilot
                .read()
                .await
                .list_accounts()
                .await
                .into_iter()
                .map(ManagedAuthAccount::Copilot)
                .collect(),
        })
    }

    pub async fn auth_set_default(
        &self,
        provider: ManagedAuthProvider,
        account_id: &str,
    ) -> Result<(), String> {
        match provider {
            ManagedAuthProvider::CodexOAuth => self
                .codex_oauth
                .write()
                .await
                .set_default_account(account_id)
                .await
                .map_err(|e| e.to_string()),
            ManagedAuthProvider::GitHubCopilot => self
                .copilot
                .write()
                .await
                .set_default_account(account_id)
                .await
                .map_err(|e| e.to_string()),
        }
    }

    pub async fn auth_remove(
        &self,
        provider: ManagedAuthProvider,
        account_id: &str,
    ) -> Result<(), String> {
        match provider {
            ManagedAuthProvider::CodexOAuth => self
                .codex_oauth
                .write()
                .await
                .remove_account(account_id)
                .await
                .map_err(|e| e.to_string()),
            ManagedAuthProvider::GitHubCopilot => self
                .copilot
                .write()
                .await
                .remove_account(account_id)
                .await
                .map_err(|e| e.to_string()),
        }
    }

    pub async fn auth_logout(&self, provider: ManagedAuthProvider) -> Result<(), String> {
        match provider {
            ManagedAuthProvider::CodexOAuth => self
                .codex_oauth
                .write()
                .await
                .clear_auth()
                .await
                .map_err(|e| e.to_string()),
            ManagedAuthProvider::GitHubCopilot => self
                .copilot
                .write()
                .await
                .clear_auth()
                .await
                .map_err(|e| e.to_string()),
        }
    }

    pub fn import_opencode_provider(
        &self,
        provider_id: &str,
    ) -> Result<OpenCodeImportResult, AppError> {
        let config: OpenCodeConfig = read_json_path(opencode_config_path()?)?;
        let source = config
            .provider
            .get(provider_id)
            .ok_or_else(|| AppError::Message(format!("OpenCode provider 不存在: {provider_id}")))?;

        if source.npm.as_deref() != Some("@ai-sdk/openai-compatible") {
            return Err(AppError::Message(format!(
                "OpenCode provider {provider_id} 不是 openai-compatible provider"
            )));
        }

        let base_url = source.options.base_url.clone().ok_or_else(|| {
            AppError::Message(format!("OpenCode provider {provider_id} 缺少 baseURL"))
        })?;
        let api_key = source.options.api_key.clone().ok_or_else(|| {
            AppError::Message(format!("OpenCode provider {provider_id} 缺少 apiKey"))
        })?;
        let models: Vec<String> = source.models.keys().cloned().collect();
        if models.is_empty() {
            return Err(AppError::Message(format!(
                "OpenCode provider {provider_id} 没有可导入模型"
            )));
        }

        let provider = Provider {
            id: provider_id.to_string(),
            name: source
                .name
                .clone()
                .unwrap_or_else(|| provider_id.to_string()),
            settings_config: json!({
                "base_url": base_url,
                "apiKey": api_key,
                "auth_mode": "bearer_only",
                "modelFamily": provider_id,
                "models": models.clone(),
            }),
            website_url: None,
            category: Some("codex".to_string()),
            created_at: Some(chrono::Utc::now().timestamp_millis()),
            sort_index: None,
            notes: Some("Imported from OpenCode provider config".to_string()),
            meta: Some(ProviderMeta {
                api_format: Some("openai_chat".to_string()),
                ..ProviderMeta::default()
            }),
            icon: Some("openai".to_string()),
            icon_color: Some("#111827".to_string()),
            in_failover_queue: false,
        };

        self.state
            .db
            .save_provider(AppType::Codex.as_str(), &provider)?;
        self.state
            .db
            .save_provider(AppType::Claude.as_str(), &provider)?;
        if self
            .state
            .db
            .get_current_provider(AppType::Codex.as_str())?
            .is_none()
        {
            self.state
                .db
                .set_current_provider(AppType::Codex.as_str(), &provider.id)?;
        }

        Ok(OpenCodeImportResult {
            provider_id: provider.id,
            family: provider_id.to_string(),
            app: AppType::Codex.as_str().to_string(),
            models,
            changed: true,
        })
    }

    pub async fn import_opencode_openai_oauth(&self) -> Result<OpenCodeOAuthImportResult, String> {
        let auth: OpenCodeAuthFile =
            read_json_path(opencode_auth_path()?).map_err(|e| e.to_string())?;
        let entry = auth
            .entries
            .get("openai")
            .ok_or_else(|| "OpenCode auth.json 中没有 openai auth".to_string())?;
        if entry.kind != "oauth" {
            return Err("OpenCode openai auth 不是 oauth 类型".to_string());
        }
        let account_id = entry
            .account_id
            .clone()
            .ok_or_else(|| "OpenCode openai auth 缺少 accountId".to_string())?;
        let refresh = entry
            .refresh
            .clone()
            .ok_or_else(|| "OpenCode openai auth 缺少 refresh token".to_string())?;

        let account = self
            .codex_oauth
            .write()
            .await
            .import_refresh_token_account(
                account_id.clone(),
                refresh,
                entry.email.clone(),
                None,
                true,
            )
            .await
            .map_err(|e| e.to_string())?;

        let mut model_set: BTreeSet<String> = fallback_gpt_models().into_iter().collect();
        if let Ok(token) = self
            .codex_oauth
            .read()
            .await
            .get_valid_token_for_account(&account.id)
            .await
        {
            if let Ok(fetched) =
                crate::services::codex_oauth_models::fetch_models_with_token(&token, &account.id)
                    .await
            {
                for model in fetched {
                    if model.id.starts_with("gpt-") {
                        model_set.insert(model.id);
                    }
                }
            }
        }
        let models: Vec<String> = model_set.into_iter().collect();
        let provider = Provider {
            id: "gpt".to_string(),
            name: "GPT OAuth".to_string(),
            settings_config: json!({
                "base_url": "https://chatgpt.com/backend-api/codex",
                "modelFamily": "gpt",
                "models": models.clone(),
            }),
            website_url: None,
            category: Some("codex".to_string()),
            created_at: Some(chrono::Utc::now().timestamp_millis()),
            sort_index: None,
            notes: Some("Imported from OpenCode openai OAuth auth".to_string()),
            meta: Some(ProviderMeta {
                provider_type: Some("codex_oauth".to_string()),
                auth_binding: Some(AuthBinding {
                    source: AuthBindingSource::ManagedAccount,
                    auth_provider: Some("codex_oauth".to_string()),
                    account_id: Some(account.id.clone()),
                }),
                ..ProviderMeta::default()
            }),
            icon: Some("openai".to_string()),
            icon_color: Some("#10A37F".to_string()),
            in_failover_queue: false,
        };
        self.state
            .db
            .save_provider(AppType::Codex.as_str(), &provider)
            .map_err(|e| e.to_string())?;
        self.state
            .db
            .save_provider(AppType::Claude.as_str(), &provider)
            .map_err(|e| e.to_string())?;
        if self
            .state
            .db
            .get_current_provider(AppType::Codex.as_str())
            .map_err(|e| e.to_string())?
            .is_none()
        {
            self.state
                .db
                .set_current_provider(AppType::Codex.as_str(), &provider.id)
                .map_err(|e| e.to_string())?;
        }

        Ok(OpenCodeOAuthImportResult {
            account_id: account.id,
            provider_id: provider.id,
            family: "gpt".to_string(),
            models,
        })
    }
}
