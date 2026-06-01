# CC Switch

AI Coding Tool 的统一 LLM proxy，支持 Anthropic 和 OpenAI 格式请求的转发、provider routing、failover、usage tracking。

## 配置与运维约束（agent 必读）

- **provider / 配置变更必须用 CLI，禁止直接改 `cc-switch.db`。** 用 `cc-switch-cli provider add|update|delete|list|switch --app <app_type> [--file <json>] [--id <id>]`（走 `ProviderService`，含校验与正确序列化）。手写 SQL / `sqlite3` 改 `providers.settings_config`/`meta` 是反模式（绕过校验、易写坏 BLOB、与 live config 不同步）。
  - 只读查询（排障）可直接 `sqlite3 ... SELECT`，但任何**写**都走 CLI。
  - 已知 CLI gap：import 来的 provider（无 `auth` 字段）跑 `provider update` 会因校验失败 → 用 `provider add` 传完整 JSON（含 `auth`），或补 CLI 能力，**不要回退到改 DB**。
- 代码改动走 PR（fork `Dandi007/cc-switch`）。
- **proxy 行为改动必须真机 E2E**：mock/单测全绿 ≠ live 通。本机 proxy 在 `127.0.0.1:15721`（svc `cc-switch`），换二进制 → `svc restart cc-switch` → 真实打 API 验证（调用 + 缓存 `cache_read`）。

## 已知问题 / 设计要点

- **per-model api_format（已修，PR #4）**：lingzhi 是聚合网关，Claude 支持 anthropic 端点、deepseek/gpt/qwen 仅 openai。`resolve_claude_api_format`（`proxy/forwarder.rs`）非 copilot 分支按 model 的 live `/v1/models` `supported_endpoint_types` 选协议（含 anthropic → passthrough 保 `cache_control`），**请求与响应两侧都用 resolved 格式**（`handlers.rs` 的 `needs_transform` 也是），否则 anthropic 请求的响应被当 openai 转换会报 `No choices in response`(422)。provider 的 secret 形如 `{env:NAME}`，取 key 用 `providers::claude::resolve_env_reference` 解析，勿自写。

## Current Dev Task

当前有一个活跃的开发任务，请阅读任务文档：

→ **`docs/dev/payload-recording/AGENTS.md`**

任务内容：实现请求/响应全量记录功能（payload recording）。
