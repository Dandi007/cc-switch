//! Headless runtime bootstrap for non-Tauri entrypoints.
//!
//! The CLI uses the same database, provider, MCP, prompt, proxy and auth
//! services as the desktop app, but skips window, tray, dialog and deep-link
//! setup.

use crate::app_config::AppType;
use crate::database::Database;
use crate::error::AppError;
use crate::proxy::providers::codex_oauth_auth::{CodexOAuthManager, CodexOAuthStatus};
use crate::proxy::providers::copilot_auth::{
    CopilotAuthManager, CopilotAuthStatus, GitHubAccount as CopilotAccount,
    GitHubAccount as CodexOAuthAccount, GitHubDeviceCodeResponse,
};
use crate::store::AppState;
use serde::Serialize;
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
}
