use anyhow::{bail, Context, Result};
use cc_switch_lib::headless::{HeadlessApp, HeadlessOptions, ManagedAuthProvider};
use cc_switch_lib::{
    get_settings, update_settings, AppSettings, AppType, LogFilters, McpServer, Prompt,
    PromptService, Provider, ProviderService, ProxyConfig,
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::path::PathBuf;
use std::str::FromStr;
use std::time::Duration;

#[derive(Debug)]
struct Cli {
    pretty: bool,
    app: Option<AppType>,
    config_dir: Option<PathBuf>,
    args: Vec<String>,
}

fn take_value(args: &mut Vec<String>, flag: &str) -> Result<Option<String>> {
    if let Some(index) = args.iter().position(|arg| arg == flag) {
        args.remove(index);
        if index >= args.len() {
            bail!("{flag} requires a value");
        }
        return Ok(Some(args.remove(index)));
    }

    let prefix = format!("{flag}=");
    if let Some(index) = args.iter().position(|arg| arg.starts_with(&prefix)) {
        let value = args.remove(index)[prefix.len()..].to_string();
        return Ok(Some(value));
    }

    Ok(None)
}

fn take_bool(args: &mut Vec<String>, flag: &str) -> bool {
    if let Some(index) = args.iter().position(|arg| arg == flag) {
        args.remove(index);
        return true;
    }
    false
}

fn parse_cli() -> Result<Cli> {
    let mut args: Vec<String> = std::env::args().skip(1).collect();
    let pretty = take_bool(&mut args, "--pretty");
    let _json = take_bool(&mut args, "--json");
    let app = take_value(&mut args, "--app")?
        .map(|raw| AppType::from_str(&raw))
        .transpose()?;
    let config_dir = take_value(&mut args, "--config-dir")?.map(PathBuf::from);
    Ok(Cli {
        pretty,
        app,
        config_dir,
        args,
    })
}

fn require_app(cli: &Cli) -> Result<AppType> {
    cli.app
        .clone()
        .context("--app <claude|claude-desktop|codex|gemini|opencode|openclaw|hermes> is required")
}

fn required_value(args: &mut Vec<String>, flag: &str) -> Result<String> {
    take_value(args, flag)?.with_context(|| format!("{flag} is required"))
}

fn optional_bool(args: &mut Vec<String>, flag: &str, default: bool) -> Result<bool> {
    match take_value(args, flag)? {
        Some(raw) => match raw.trim().to_ascii_lowercase().as_str() {
            "true" | "1" | "yes" | "on" => Ok(true),
            "false" | "0" | "no" | "off" => Ok(false),
            _ => bail!("{flag} must be true or false"),
        },
        None => Ok(default),
    }
}

fn read_json_file<T: serde::de::DeserializeOwned>(path: &str) -> Result<T> {
    let content =
        std::fs::read_to_string(path).with_context(|| format!("failed to read {path}"))?;
    serde_json::from_str(&content).with_context(|| format!("failed to parse JSON from {path}"))
}

fn print_json<T: Serialize>(value: &T, pretty: bool) -> Result<()> {
    if pretty {
        println!("{}", serde_json::to_string_pretty(value)?);
    } else {
        println!("{}", serde_json::to_string(value)?);
    }
    Ok(())
}

fn ok() -> Value {
    json!({ "ok": true })
}

// ============================================================================
// Cross-process proxy state (PID file)
// ============================================================================
//
// Each `cc-switch-cli proxy start` invocation lives in its own process with its
// own ProxyService state. Without IPC, sibling `proxy status` / `proxy stop`
// invocations see an empty in-process state and report "not running" (and stop
// becomes a no-op). The running instance writes a JSON record at
// `<config-dir>/proxy.pid` on start and removes it on clean stop so siblings
// can detect and signal it.

#[derive(Serialize, Deserialize)]
struct ProxyPidRecord {
    pid: u32,
    address: String,
    port: u16,
    started_at: String,
}

fn pid_file_path(cli: &Cli) -> PathBuf {
    let base = cli.config_dir.clone().unwrap_or_else(|| {
        dirs::home_dir()
            .expect("无法定位用户 home 目录")
            .join(".cc-switch")
    });
    base.join("proxy.pid")
}

fn write_pid_file(cli: &Cli, address: &str, port: u16, started_at: &str) -> Result<()> {
    let path = pid_file_path(cli);
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let rec = ProxyPidRecord {
        pid: std::process::id(),
        address: address.to_string(),
        port,
        started_at: started_at.to_string(),
    };
    let bytes = serde_json::to_vec(&rec)?;
    std::fs::write(&path, bytes).with_context(|| format!("写入 PID 文件失败: {}", path.display()))
}

fn read_pid_file(cli: &Cli) -> Option<ProxyPidRecord> {
    let bytes = std::fs::read(pid_file_path(cli)).ok()?;
    serde_json::from_slice(&bytes).ok()
}

fn remove_pid_file(cli: &Cli) {
    let _ = std::fs::remove_file(pid_file_path(cli));
}

#[cfg(unix)]
fn pid_is_alive(pid: u32) -> bool {
    std::process::Command::new("kill")
        .arg("-0")
        .arg(pid.to_string())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

#[cfg(not(unix))]
fn pid_is_alive(_pid: u32) -> bool {
    // Non-Unix: conservatively return false so the caller falls back to the
    // legacy in-process behavior. PID-file cross-process control is a Unix-only
    // feature in this CLI.
    false
}

#[cfg(unix)]
fn send_sigterm(pid: u32) -> bool {
    std::process::Command::new("kill")
        .arg(pid.to_string())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

#[cfg(not(unix))]
fn send_sigterm(_pid: u32) -> bool {
    false
}

fn port_is_listening(address: &str, port: u16) -> bool {
    let host = if address.is_empty() || address == "0.0.0.0" {
        "127.0.0.1"
    } else {
        address
    };
    let target = format!("{host}:{port}");
    target
        .parse()
        .ok()
        .and_then(|addr| {
            std::net::TcpStream::connect_timeout(&addr, Duration::from_millis(200)).ok()
        })
        .is_some()
}

/// Returns `Some(rec)` iff PID file exists, the recorded PID is alive, and the
/// recorded port accepts a TCP connection. Stale records (dead PID) are removed
/// as a side effect.
fn external_running(cli: &Cli) -> Option<ProxyPidRecord> {
    let rec = read_pid_file(cli)?;
    if !pid_is_alive(rec.pid) {
        remove_pid_file(cli);
        return None;
    }
    if !port_is_listening(&rec.address, rec.port) {
        return None;
    }
    Some(rec)
}

async fn cmd_provider(app: &HeadlessApp, cli: &Cli, mut args: Vec<String>) -> Result<Value> {
    let sub = if args.is_empty() {
        bail!("provider subcommand is required");
    } else {
        args.remove(0)
    };
    match sub.as_str() {
        "import-opencode" => {
            let id = required_value(&mut args, "--id")?;
            Ok(serde_json::to_value(app.import_opencode_provider(&id)?)?)
        }
        "list" => {
            let app_type = require_app(cli)?;
            Ok(serde_json::to_value(ProviderService::list(
                &app.state, app_type,
            )?)?)
        }
        "current" => {
            let app_type = require_app(cli)?;
            Ok(json!({ "current": ProviderService::current(&app.state, app_type)? }))
        }
        "add" => {
            let app_type = require_app(cli)?;
            let file = required_value(&mut args, "--file")?;
            let add_to_live = optional_bool(&mut args, "--add-to-live", false)?;
            let provider: Provider = read_json_file(&file)?;
            Ok(
                json!({ "changed": ProviderService::add(&app.state, app_type, provider, add_to_live)? }),
            )
        }
        "update" => {
            let app_type = require_app(cli)?;
            let file = required_value(&mut args, "--file")?;
            let id = take_value(&mut args, "--id")?;
            let provider: Provider = read_json_file(&file)?;
            Ok(
                json!({ "changed": ProviderService::update(&app.state, app_type, id.as_deref(), provider)? }),
            )
        }
        "delete" => {
            let app_type = require_app(cli)?;
            let id = required_value(&mut args, "--id")?;
            ProviderService::delete(&app.state, app_type, &id)?;
            Ok(ok())
        }
        "switch" => {
            let app_type = require_app(cli)?;
            let id = required_value(&mut args, "--id")?;
            Ok(serde_json::to_value(ProviderService::switch(
                &app.state, app_type, &id,
            )?)?)
        }
        "remove-live" => {
            let app_type = require_app(cli)?;
            let id = required_value(&mut args, "--id")?;
            ProviderService::remove_from_live_config(&app.state, app_type, &id)?;
            Ok(ok())
        }
        "import-default" => {
            let app_type = require_app(cli)?;
            Ok(json!({
                "imported": ProviderService::import_default_config(&app.state, app_type)?
            }))
        }
        "import-live" => {
            let app_type = require_app(cli)?;
            Ok(json!({
                "settings": ProviderService::read_live_settings(app_type)?
            }))
        }
        "sync-live" => {
            let app_type = require_app(cli)?;
            ProviderService::sync_current_provider_for_app(&app.state, app_type)?;
            Ok(ok())
        }
        "failover" => {
            // Manage the per-app failover queue (providers.in_failover_queue).
            // When auto_failover_enabled is on, only providers in this queue are
            // eligible — without a CLI for this, users had to write raw SQL.
            let action = if args.is_empty() {
                bail!("provider failover action is required (list | add | remove | clear)");
            } else {
                args.remove(0)
            };
            let app_type = require_app(cli)?;
            let app_str = app_type.as_str();
            match action.as_str() {
                "list" => Ok(serde_json::to_value(
                    app.state
                        .db
                        .get_failover_queue(app_str)
                        .map_err(|e| anyhow::anyhow!(e.to_string()))?,
                )?),
                "add" => {
                    let id = required_value(&mut args, "--id")?;
                    app.state
                        .db
                        .add_to_failover_queue(app_str, &id)
                        .map_err(|e| anyhow::anyhow!(e.to_string()))?;
                    Ok(ok())
                }
                "remove" => {
                    let id = required_value(&mut args, "--id")?;
                    app.state
                        .db
                        .remove_from_failover_queue(app_str, &id)
                        .map_err(|e| anyhow::anyhow!(e.to_string()))?;
                    Ok(ok())
                }
                "clear" => {
                    app.state
                        .db
                        .clear_failover_queue(app_str)
                        .map_err(|e| anyhow::anyhow!(e.to_string()))?;
                    Ok(ok())
                }
                other => bail!("unsupported provider failover action: {other}"),
            }
        }
        other => bail!("unsupported provider subcommand: {other}"),
    }
}

async fn cmd_proxy(app: &HeadlessApp, cli: &Cli, mut args: Vec<String>) -> Result<Value> {
    let sub = if args.is_empty() {
        bail!("proxy subcommand is required");
    } else {
        args.remove(0)
    };

    match sub.as_str() {
        "start" => {
            let info = app
                .state
                .proxy_service
                .start()
                .await
                .map_err(anyhow::Error::msg)?;
            if let Err(e) = write_pid_file(cli, &info.address, info.port, &info.started_at) {
                log::warn!("PID 文件写入失败（不影响 proxy 运行）: {e:#}");
            }
            // Wait for either Ctrl+C or SIGTERM. On SIGTERM the process exits
            // the select! and runs the cleanup path (stop_with_restore +
            // remove_pid_file) before returning.
            tokio::select! {
                _ = tokio::signal::ctrl_c() => {}
                _ = async {
                    #[cfg(unix)]
                    {
                        use tokio::signal::unix::{signal, SignalKind};
                        let mut term = signal(SignalKind::terminate())
                            .expect("install SIGTERM handler");
                        term.recv().await;
                    }
                    #[cfg(not(unix))]
                    std::future::pending::<()>().await;
                } => {
                    log::info!("收到 SIGTERM，正在优雅关闭...");
                }
            }
            let stop_result = app.state.proxy_service.stop_with_restore().await;
            remove_pid_file(cli);
            stop_result.map_err(anyhow::Error::msg)?;
            Ok(serde_json::to_value(info)?)
        }
        "stop" => match app.state.proxy_service.stop().await {
            Ok(()) => {
                remove_pid_file(cli);
                Ok(ok())
            }
            Err(e) => {
                // In-process state empty: try cross-process via PID file.
                if let Some(rec) = external_running(cli) {
                    if send_sigterm(rec.pid) {
                        remove_pid_file(cli);
                        return Ok(json!({
                            "ok": true,
                            "source": "pid_file",
                            "pid": rec.pid,
                        }));
                    }
                }
                Err(anyhow::Error::msg(e))
            }
        },
        "stop-restore" => match app.state.proxy_service.stop_with_restore().await {
            Ok(()) => {
                remove_pid_file(cli);
                Ok(ok())
            }
            Err(e) => {
                // If an external instance owns the running proxy, do NOT send
                // SIGTERM — that would cause the external instance to restore
                // live config (which it legitimately modified), potentially
                // wiping the user's proxy settings.
                if external_running(cli).is_some() {
                    bail!(
                        "proxy is running in another process (pid file detected). \
                         stop-restore is only allowed from the process that owns the proxy. \
                         Use 'proxy stop' to send a clean shutdown signal instead."
                    );
                }
                Err(anyhow::Error::msg(e))
            }
        },
        "status" | "health" => {
            let status = app
                .state
                .proxy_service
                .get_status()
                .await
                .map_err(anyhow::Error::msg)?;
            if status.running {
                return Ok(serde_json::to_value(status)?);
            }
            // Fall back to PID file if a sibling process owns the running proxy.
            if let Some(rec) = external_running(cli) {
                return Ok(json!({
                    "running": true,
                    "address": rec.address,
                    "port": rec.port,
                    "started_at": rec.started_at,
                    "pid": rec.pid,
                    "source": "pid_file",
                    "note": "owned by another process; runtime metrics unavailable from this invocation",
                }));
            }
            Ok(serde_json::to_value(status)?)
        }
        "config" => {
            let action = if args.is_empty() {
                bail!("proxy config action is required");
            } else {
                args.remove(0)
            };
            match action.as_str() {
                "get" => Ok(serde_json::to_value(
                    app.state
                        .proxy_service
                        .get_config()
                        .await
                        .map_err(anyhow::Error::msg)?,
                )?),
                "set" => {
                    let file = required_value(&mut args, "--file")?;
                    let config: ProxyConfig = read_json_file(&file)?;
                    app.state
                        .proxy_service
                        .update_config(&config)
                        .await
                        .map_err(anyhow::Error::msg)?;
                    Ok(ok())
                }
                other => bail!("unsupported proxy config action: {other}"),
            }
        }
        "takeover" => {
            let action = if args.is_empty() {
                bail!("proxy takeover action is required");
            } else {
                args.remove(0)
            };
            match action.as_str() {
                "get" => Ok(serde_json::to_value(
                    app.state
                        .proxy_service
                        .get_takeover_status()
                        .await
                        .map_err(anyhow::Error::msg)?,
                )?),
                "set" => {
                    let app_type = require_app(cli)?;
                    if matches!(app_type, AppType::OpenCode) {
                        bail!(
                            "unsupported: opencode proxy takeover is intentionally not implemented"
                        );
                    }
                    // Require a running proxy before modifying takeover state.
                    // A short-lived CLI that sets takeover and exits leaves
                    // dead-pointer live configs.
                    let running_in_process = app
                        .state
                        .proxy_service
                        .is_running()
                        .await;
                    let external = external_running(cli);

                    if !running_in_process && external.is_none() {
                        bail!(
                            "proxy is not running. Start the proxy first with 'proxy start' \
                             before enabling takeover."
                        );
                    }
                    if !running_in_process && external.is_some() {
                        bail!(
                            "proxy is running in another process (pid file detected). \
                             Takeover state can only be modified from the owning process. \
                             Stop it with 'proxy stop' first, or run takeover from the foreground CLI."
                        );
                    }
                    let enabled = optional_bool(&mut args, "--enabled", true)?;
                    app.state
                        .proxy_service
                        .set_takeover_for_app(app_type.as_str(), enabled)
                        .await
                        .map_err(anyhow::Error::msg)?;
                    Ok(ok())
                }
                other => bail!("unsupported proxy takeover action: {other}"),
            }
        }
        "switch-provider" => {
            let app_type = require_app(cli)?;
            let provider_id = required_value(&mut args, "--id")?;
            app.state
                .proxy_service
                .switch_proxy_target(app_type.as_str(), &provider_id)
                .await
                .map_err(anyhow::Error::msg)?;
            Ok(ok())
        }
        "circuit-breaker" => Ok(json!({
            "unsupported": "CLI circuit-breaker mutation is not exposed yet; use proxy status for runtime health."
        })),
        "app" => {
            // Per-app proxy_config mutations: set enabled / auto_failover_enabled
            // without forcing users to drop down to raw SQL.
            let action = if args.is_empty() {
                bail!("proxy app action is required (get | set-enabled | set-failover)");
            } else {
                args.remove(0)
            };
            let app_type = require_app(cli)?;
            let app_str = app_type.as_str();
            match action.as_str() {
                "get" => Ok(serde_json::to_value(
                    app.state
                        .db
                        .get_proxy_config_for_app(app_str)
                        .await
                        .map_err(|e| anyhow::anyhow!(e.to_string()))?,
                )?),
                "set-enabled" => {
                    let value = optional_bool(&mut args, "--value", true)?;
                    let mut config = app
                        .state
                        .db
                        .get_proxy_config_for_app(app_str)
                        .await
                        .map_err(|e| anyhow::anyhow!(e.to_string()))?;
                    config.enabled = value;
                    app.state
                        .db
                        .update_proxy_config_for_app(config)
                        .await
                        .map_err(|e| anyhow::anyhow!(e.to_string()))?;
                    Ok(ok())
                }
                "set-failover" => {
                    let value = optional_bool(&mut args, "--value", true)?;
                    let mut config = app
                        .state
                        .db
                        .get_proxy_config_for_app(app_str)
                        .await
                        .map_err(|e| anyhow::anyhow!(e.to_string()))?;
                    if value && !config.enabled {
                        bail!(
                            "must enable the app first: proxy app set-enabled --app {app_str} --value true"
                        );
                    }
                    config.auto_failover_enabled = value;
                    app.state
                        .db
                        .update_proxy_config_for_app(config)
                        .await
                        .map_err(|e| anyhow::anyhow!(e.to_string()))?;
                    Ok(ok())
                }
                other => bail!("unsupported proxy app action: {other}"),
            }
        }
        other => bail!("unsupported proxy subcommand: {other}"),
    }
}

async fn cmd_auth(app: &HeadlessApp, mut args: Vec<String>) -> Result<Value> {
    let sub = if args.is_empty() {
        bail!("auth subcommand is required");
    } else {
        args.remove(0)
    };
    let provider_raw = required_value(&mut args, "--provider")?;

    if sub == "import-opencode" {
        if provider_raw.trim().eq_ignore_ascii_case("openai") {
            return Ok(serde_json::to_value(
                app.import_opencode_openai_oauth()
                    .await
                    .map_err(anyhow::Error::msg)?,
            )?);
        }
        bail!("unsupported OpenCode auth provider: {provider_raw}");
    }

    let provider = ManagedAuthProvider::parse(&provider_raw)?;

    match sub.as_str() {
        "start-login" => Ok(serde_json::to_value(
            app.auth_start_login(provider)
                .await
                .map_err(anyhow::Error::msg)?,
        )?),
        "poll" => {
            let code = required_value(&mut args, "--device-code")?;
            Ok(
                json!({ "account": app.auth_poll(provider, &code).await.map_err(anyhow::Error::msg)? }),
            )
        }
        "status" => Ok(serde_json::to_value(
            app.auth_status(provider)
                .await
                .map_err(anyhow::Error::msg)?,
        )?),
        "list" => Ok(serde_json::to_value(
            app.auth_list(provider).await.map_err(anyhow::Error::msg)?,
        )?),
        "set-default" => {
            let account_id = required_value(&mut args, "--account-id")?;
            app.auth_set_default(provider, &account_id)
                .await
                .map_err(anyhow::Error::msg)?;
            Ok(ok())
        }
        "remove" => {
            let account_id = required_value(&mut args, "--account-id")?;
            app.auth_remove(provider, &account_id)
                .await
                .map_err(anyhow::Error::msg)?;
            Ok(ok())
        }
        "logout" => {
            app.auth_logout(provider)
                .await
                .map_err(anyhow::Error::msg)?;
            Ok(ok())
        }
        other => bail!("unsupported auth subcommand: {other}"),
    }
}

fn cmd_mcp(app: &HeadlessApp, cli: &Cli, mut args: Vec<String>) -> Result<Value> {
    let sub = if args.is_empty() {
        bail!("mcp subcommand is required");
    } else {
        args.remove(0)
    };

    match sub.as_str() {
        "list" => Ok(serde_json::to_value(
            cc_switch_lib::McpService::get_all_servers(&app.state)?,
        )?),
        "upsert" => {
            let file = required_value(&mut args, "--file")?;
            let server: McpServer = read_json_file(&file)?;
            cc_switch_lib::McpService::upsert_server(&app.state, server)?;
            Ok(ok())
        }
        "delete" => {
            let id = required_value(&mut args, "--id")?;
            Ok(json!({ "deleted": cc_switch_lib::McpService::delete_server(&app.state, &id)? }))
        }
        "toggle" => {
            let id = required_value(&mut args, "--id")?;
            let enabled = optional_bool(&mut args, "--enabled", true)?;
            cc_switch_lib::McpService::toggle_app(&app.state, &id, require_app(cli)?, enabled)?;
            Ok(ok())
        }
        "import" => {
            let app_type = require_app(cli)?;
            let count = match app_type {
                AppType::Claude => cc_switch_lib::McpService::import_from_claude(&app.state)?,
                AppType::Codex => cc_switch_lib::McpService::import_from_codex(&app.state)?,
                AppType::Gemini => cc_switch_lib::McpService::import_from_gemini(&app.state)?,
                AppType::OpenCode => cc_switch_lib::McpService::import_from_opencode(&app.state)?,
                AppType::Hermes => cc_switch_lib::McpService::import_from_hermes(&app.state)?,
                AppType::OpenClaw | AppType::ClaudeDesktop => 0,
            };
            Ok(json!({ "imported": count }))
        }
        "sync" => {
            cc_switch_lib::McpService::sync_all_enabled(&app.state)?;
            Ok(ok())
        }
        other => bail!("unsupported mcp subcommand: {other}"),
    }
}

fn cmd_prompt(app: &HeadlessApp, cli: &Cli, mut args: Vec<String>) -> Result<Value> {
    let sub = if args.is_empty() {
        bail!("prompt subcommand is required");
    } else {
        args.remove(0)
    };
    let app_type = require_app(cli)?;

    match sub.as_str() {
        "list" => Ok(serde_json::to_value(PromptService::get_prompts(
            &app.state, app_type,
        )?)?),
        "upsert" => {
            let file = required_value(&mut args, "--file")?;
            let prompt: Prompt = read_json_file(&file)?;
            let id = prompt.id.clone();
            PromptService::upsert_prompt(&app.state, app_type, &id, prompt)?;
            Ok(ok())
        }
        "delete" => {
            let id = required_value(&mut args, "--id")?;
            PromptService::delete_prompt(&app.state, app_type, &id)?;
            Ok(ok())
        }
        "enable" => {
            let id = required_value(&mut args, "--id")?;
            PromptService::enable_prompt(&app.state, app_type, &id)?;
            Ok(ok())
        }
        "import-current" => {
            Ok(json!({ "id": PromptService::import_from_file(&app.state, app_type)? }))
        }
        other => bail!("unsupported prompt subcommand: {other}"),
    }
}

fn cmd_usage(app: &HeadlessApp, cli: &Cli, mut args: Vec<String>) -> Result<Value> {
    let sub = if args.is_empty() {
        bail!("usage subcommand is required");
    } else {
        args.remove(0)
    };
    let start = take_value(&mut args, "--start")?
        .map(|v| v.parse())
        .transpose()?;
    let end = take_value(&mut args, "--end")?
        .map(|v| v.parse())
        .transpose()?;
    let app_filter = cli.app.as_ref().map(|a| a.as_str().to_string());

    match sub.as_str() {
        "summary" => Ok(serde_json::to_value(app.state.db.get_usage_summary(
            start,
            end,
            app_filter.as_deref(),
        )?)?),
        "trends" => Ok(serde_json::to_value(app.state.db.get_daily_trends(
            start,
            end,
            app_filter.as_deref(),
        )?)?),
        "provider-stats" => Ok(serde_json::to_value(app.state.db.get_provider_stats(
            start,
            end,
            app_filter.as_deref(),
        )?)?),
        "model-stats" => Ok(serde_json::to_value(app.state.db.get_model_stats(
            start,
            end,
            app_filter.as_deref(),
        )?)?),
        "logs" => {
            let page = take_value(&mut args, "--page")?
                .unwrap_or_else(|| "1".to_string())
                .parse()?;
            let page_size = take_value(&mut args, "--page-size")?
                .unwrap_or_else(|| "50".to_string())
                .parse()?;
            let filters = LogFilters {
                app_type: app_filter,
                provider_name: take_value(&mut args, "--provider-name")?,
                model: take_value(&mut args, "--model")?,
                status_code: take_value(&mut args, "--status-code")?
                    .map(|v| v.parse())
                    .transpose()?,
                start_date: start,
                end_date: end,
            };
            Ok(serde_json::to_value(
                app.state.db.get_request_logs(&filters, page, page_size)?,
            )?)
        }
        "pricing" => Ok(json!({
            "unsupported": "pricing table management is not exposed through this CLI yet"
        })),
        other => bail!("unsupported usage subcommand: {other}"),
    }
}

fn cmd_config(app: &HeadlessApp, mut args: Vec<String>) -> Result<Value> {
    let sub = if args.is_empty() {
        bail!("config subcommand is required");
    } else {
        args.remove(0)
    };

    match sub.as_str() {
        "path" => Ok(json!({
            "appConfigDir": cc_switch_lib::config::get_app_config_dir(),
        })),
        "status" => Ok(json!({
            "init": &app.report,
            "settings": get_settings(),
        })),
        "settings" => {
            let action = if args.is_empty() {
                bail!("config settings action is required");
            } else {
                args.remove(0)
            };
            match action.as_str() {
                "get" => Ok(serde_json::to_value(get_settings())?),
                "set" => {
                    let file = required_value(&mut args, "--file")?;
                    let settings: AppSettings = read_json_file(&file)?;
                    update_settings(settings)?;
                    Ok(ok())
                }
                other => bail!("unsupported config settings action: {other}"),
            }
        }
        other => bail!("unsupported config subcommand: {other}"),
    }
}

fn cmd_backup(app: &HeadlessApp, mut args: Vec<String>) -> Result<Value> {
    let sub = if args.is_empty() {
        bail!("backup subcommand is required");
    } else {
        args.remove(0)
    };

    match sub.as_str() {
        "export" => {
            let file = required_value(&mut args, "--file")?;
            app.state.db.export_sql(PathBuf::from(&file).as_path())?;
            Ok(json!({ "path": file }))
        }
        "import" => {
            let file = required_value(&mut args, "--file")?;
            Ok(json!({ "message": app.state.db.import_sql(PathBuf::from(&file).as_path())? }))
        }
        "create" => Ok(json!({ "path": app.state.db.backup_database_file()? })),
        "list" => Ok(serde_json::to_value(
            cc_switch_lib::Database::list_backups()?,
        )?),
        "restore" => {
            let filename = required_value(&mut args, "--filename")?;
            Ok(json!({ "message": app.state.db.restore_from_backup(&filename)? }))
        }
        other => bail!("unsupported backup subcommand: {other}"),
    }
}

async fn cmd_payload(app: &HeadlessApp, mut args: Vec<String>) -> Result<Value> {
    let sub = if args.is_empty() {
        bail!("payload subcommand is required (search | get | session | stats | export | prune)");
    } else {
        args.remove(0)
    };

    match sub.as_str() {
        "search" => {
            let query = required_value(&mut args, "--query")?;
            let start = take_value(&mut args, "--start")?
                .map(|v| v.parse::<i64>())
                .transpose()?;
            let end = take_value(&mut args, "--end")?
                .map(|v| v.parse::<i64>())
                .transpose()?;
            let app_filter = take_value(&mut args, "--app")?;
            Ok(serde_json::to_value(
                cc_switch_lib::PayloadService::search(
                    &app.state,
                    &query,
                    start,
                    end,
                    app_filter.as_deref(),
                )?,
            )?)
        }
        "get" => {
            let id = required_value(&mut args, "--id")?;
            Ok(serde_json::to_value(
                cc_switch_lib::PayloadService::get(&app.state, &id)?,
            )?)
        }
        "session" => {
            let id = required_value(&mut args, "--id")?;
            Ok(serde_json::to_value(
                cc_switch_lib::PayloadService::session(&app.state, &id)?,
            )?)
        }
        "stats" => {
            let start = take_value(&mut args, "--start")?
                .map(|v| v.parse::<i64>())
                .transpose()?;
            let end = take_value(&mut args, "--end")?
                .map(|v| v.parse::<i64>())
                .transpose()?;
            let group_by = take_value(&mut args, "--group-by")?
                .unwrap_or_else(|| "model".to_string());
            Ok(serde_json::to_value(
                cc_switch_lib::PayloadService::stats(
                    &app.state, start, end, &group_by,
                )?,
            )?)
        }
        "export" => {
            let output = required_value(&mut args, "--output")?;
            let start = take_value(&mut args, "--start")?
                .map(|v| v.parse::<i64>())
                .transpose()?;
            let end = take_value(&mut args, "--end")?
                .map(|v| v.parse::<i64>())
                .transpose()?;
            let count =
                cc_switch_lib::PayloadService::export(&app.state, start, end, &output)?;
            Ok(serde_json::json!({ "exported": count, "path": output }))
        }
        "prune" => {
            let before = required_value(&mut args, "--before")?;
            let before_ts: i64 = before.parse()?;
            let dry_run = take_bool(&mut args, "--dry-run");
            let count = cc_switch_lib::PayloadService::prune(
                &app.state, before_ts, dry_run,
            )?;
            Ok(serde_json::json!({
                "would_delete": count,
                "dry_run": dry_run,
            }))
        }
        other => bail!("unsupported payload subcommand: {other}"),
    }
}

async fn run() -> Result<()> {
    let cli = parse_cli()?;
    if cli.args.is_empty() {
        bail!("command is required");
    }

    // Only recover proxy crash state when the explicit intent is to start the
    // proxy. Read-only sibling commands (status, provider list, config, etc.)
    // must not trigger recover_from_crash, which would tear down an already
    // running proxy.
    let first_arg = cli.args.first().map(|s| s.as_str()).unwrap_or("");
    let is_proxy_start = first_arg == "proxy"
        && cli.args.get(1).map(|s| s.as_str()) == Some("start");

    // HeadlessApp::init(recover_proxy=true) runs recover_from_crash, which
    // mutates live config & deletes backup files. If another proxy process
    // already owns the PID file, we must refuse before init to avoid tearing
    // down the active proxy's takeover state.
    if is_proxy_start {
        if let Some(rec) = external_running(&cli) {
            bail!(
                "another proxy is already running (pid={}, port={}).                  Stop it with 'proxy stop' first, or check with 'proxy status'.",
                rec.pid, rec.port
            );
        }
    }

    let app = HeadlessApp::init(HeadlessOptions {
        config_dir: cli.config_dir.clone(),
        recover_proxy: is_proxy_start,
    })
    .await?;

    let mut args = cli.args.clone();
    let command = args.remove(0);
    let value = match command.as_str() {
        "provider" => cmd_provider(&app, &cli, args).await?,
        "proxy" => cmd_proxy(&app, &cli, args).await?,
        "auth" => cmd_auth(&app, args).await?,
        "mcp" => cmd_mcp(&app, &cli, args)?,
        "prompt" => cmd_prompt(&app, &cli, args)?,
        "usage" => cmd_usage(&app, &cli, args)?,
        "config" => cmd_config(&app, args)?,
        "backup" => cmd_backup(&app, args)?,
        "payload" => cmd_payload(&app, args).await?,
        other => bail!("unsupported command: {other}"),
    };

    print_json(&value, cli.pretty)
}

#[tokio::main]
async fn main() {
    if let Err(err) = run().await {
        eprintln!("{err:#}");
        std::process::exit(1);
    }
}
