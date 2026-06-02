# CC Switch Payload Recording — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Record full request/response bodies for all LLM traffic through CC Switch proxy, with structured field extraction, FTS5 search, and CLI query interface.

**Architecture:** New `proxy_request_payloads` table in the existing SQLite DB (with FK to `proxy_request_logs`). Body capture at two interception points: request body in `forwarder.rs` (before upstream send), response body in `response_processor.rs` (after upstream reply). Async write via `tokio::spawn`, same pattern as existing usage logging. CLI `payload` subcommand for search/get/session/stats/export/prune.

**Tech Stack:** Rust, SQLite (rusqlite), FTS5, tokio, serde_json, axum, wiremock (tests)

**Spec:** `docs/specs/2026-05-27-payload-recording-design.md`

---

## Acceptance Criteria

每条都必须在真机上端到端验证 Pass：

| # | 验收项 | 验证方法 | Pass 标准 |
|---|--------|----------|-----------|
| AC1 | 数据目录迁移到 Data 盘 | `svc restart cc-switch && svc status cc-switch` | cc-switch healthy，DB 文件在 `/Volumes/Data/cc-switch/cc-switch.db` |
| AC2 | 非流式请求 payload 记录 | `curl` 发一条 `/v1/chat/completions` 请求 → `cc-switch-cli payload get --id <id>` | 返回完整 request_body + response_body JSON，extracted_user_message 非空 |
| AC3 | 流式请求 payload 记录 | `curl` 发一条 `stream:true` 请求 → `cc-switch-cli payload get --id <id>` | response_body 包含聚合后的完整 assistant message |
| AC4 | Anthropic 格式 payload 记录 | Claude Code 通过 cc-switch 发一条 `/v1/messages` 请求 → `payload get` | request_body 和 response_body 完整，extracted 字段正确 |
| AC5 | FTS5 全文搜索 | `cc-switch-cli payload search --query "关键词"` | 返回匹配结果，包含 user_message/assistant_message 摘要 |
| AC6 | Session 对话回溯 | `cc-switch-cli payload session --id <session_id>` | 按时序返回该 session 所有请求的 extracted 内容 |
| AC7 | 统计 | `cc-switch-cli payload stats --group-by model` | 按 model 分组显示请求数、token 总量 |
| AC8 | 导出 | `cc-switch-cli payload export --format jsonl --output /tmp/test.jsonl` | JSONL 文件可读，每行一条完整 payload |
| AC9 | 清理 | `cc-switch-cli payload prune --before <yesterday> --dry-run` | 显示将删除的条数，不实际删除 |
| AC10 | 性能无感 | 正常使用 Claude Code / CodeWhale 10 分钟 | 响应延迟无明显增加，`proxy status` 的 success_rate 100% |

---

## File Map

| 操作 | 文件路径 | 职责 |
|------|----------|------|
| Modify | `src-tauri/src/database/schema.rs` | 加 `proxy_request_payloads` 建表 + FTS5 + migration |
| Create | `src-tauri/src/proxy/usage/payload_logger.rs` | PayloadLog struct、字段提取、DB 写入 |
| Modify | `src-tauri/src/proxy/usage/mod.rs` | 声明 `payload_logger` 模块 |
| Modify | `src-tauri/src/proxy/forwarder.rs` | ForwardResult 加 `captured_request_body` |
| Modify | `src-tauri/src/proxy/response_processor.rs` | 非流式 + 流式 body 捕获，调 `log_payload` |
| Modify | `src-tauri/src/bin/cc-switch-cli.rs` | 加 `payload` 子命令路由 |
| Create | `src-tauri/src/services/payload.rs` | payload 查询/搜索/统计/导出/清理的 service 层 |
| Modify | `src-tauri/src/services/mod.rs` | 声明 `payload` 模块 |
| Modify | `src-tauri/src/lib.rs` | 导出 PayloadService |
| Create | `src-tauri/tests/payload_recording.rs` | 集成测试 |
| Modify | `~/.local/lib/local-services/cc-switch.zsh` | svc wrapper 适配 `CC_SWITCH_DATA_DIR` |

---

### Task 1: 迁移数据目录到 Data 盘

**Files:**
- Modify: `~/.local/lib/local-services/cc-switch.zsh`
- Modify: `~/.config/agent-shell/profile.zsh`

- [ ] **Step 1: 创建 Data 盘目录并迁移数据**

```bash
mkdir -p /Volumes/Data/cc-switch
cp -a ~/.cc-switch/cc-switch.db /Volumes/Data/cc-switch/cc-switch.db
cp -a ~/.cc-switch/codex_oauth_auth.json /Volumes/Data/cc-switch/codex_oauth_auth.json 2>/dev/null || true
cp -a ~/.cc-switch/settings.json /Volumes/Data/cc-switch/settings.json 2>/dev/null || true
cp -a ~/.cc-switch/logs /Volumes/Data/cc-switch/logs 2>/dev/null || true
cp -a ~/.cc-switch/backups /Volumes/Data/cc-switch/backups 2>/dev/null || true
ls -la /Volumes/Data/cc-switch/
```

Expected: `cc-switch.db` 和其余文件出现在 `/Volumes/Data/cc-switch/`。

- [ ] **Step 2: 更新 svc wrapper 脚本加 `--config-dir`**

修改 `~/.local/lib/local-services/cc-switch.zsh`，在 `CC_SWITCH_CLI` 定义之后、supervisor loop 之前加入：

```zsh
CC_SWITCH_CLI="/Users/uther/.local/bin/cc-switch-cli"
CC_SWITCH_DATA_DIR="${CC_SWITCH_DATA_DIR:-/Volumes/Data/cc-switch}"
if [ ! -d "$CC_SWITCH_DATA_DIR" ]; then
  CC_SWITCH_DATA_DIR="$HOME/.cc-switch"
fi
mkdir -p "$CC_SWITCH_DATA_DIR"
```

supervisor loop 内的启动命令改为：

```zsh
"$CC_SWITCH_CLI" --config-dir "$CC_SWITCH_DATA_DIR" proxy start
```

- [ ] **Step 3: 更新 shell profile 加 `CC_SWITCH_DATA_DIR` 和 CLI alias**

在 `~/.config/agent-shell/profile.zsh` 的 `# ---------- CC Switch / Claude Code proxy ----------` 段加入：

```zsh
export CC_SWITCH_DATA_DIR="${CC_SWITCH_DATA_DIR:-/Volumes/Data/cc-switch}"
```

更新 ccs 相关 alias，让 CLI 默认用 Data 盘：

```zsh
alias ccs='"$CC_SWITCH_CLI" --config-dir "$CC_SWITCH_DATA_DIR"'
alias ccs-model-providers='"$CC_SWITCH_CLI" --config-dir "$CC_SWITCH_DATA_DIR" --app codex provider list'
alias ccs-proxy-start='"$CC_SWITCH_CLI" --config-dir "$CC_SWITCH_DATA_DIR" proxy start'
alias ccs-proxy-status='"$CC_SWITCH_CLI" --config-dir "$CC_SWITCH_DATA_DIR" proxy status'
alias ccs-proxy-stop='"$CC_SWITCH_CLI" --config-dir "$CC_SWITCH_DATA_DIR" proxy stop'
```

- [ ] **Step 4: 重启 cc-switch 并验证**

```bash
svc restart cc-switch
sleep 3
svc status cc-switch
curl -sS http://127.0.0.1:15721/health
# 验证 DB 确实在 Data 盘
ls -la /Volumes/Data/cc-switch/cc-switch.db
# 验证 proxy.pid 也在新目录
cat /Volumes/Data/cc-switch/proxy.pid
```

Expected: cc-switch healthy，proxy.pid 在 `/Volumes/Data/cc-switch/`。

- [ ] **Step 5: 备份旧目录并清理**

```bash
mv ~/.cc-switch ~/.cc-switch.bak.$(date +%Y%m%d)
```

- [ ] **Step 6: Commit**

```bash
cd /Volumes/Data/code/worktrees/cc-switch/cli-headless
git add -A
git commit -m "ops: migrate data directory to /Volumes/Data/cc-switch"
```

---

### Task 2: 建表 + FTS5

**Files:**
- Modify: `src-tauri/src/database/schema.rs`

- [ ] **Step 1: 在 `create_tables_on_conn()` 中加建表 SQL**

在 `proxy_request_logs` 建表语句之后（约 line 196 之后）加入：

```rust
conn.execute(
    "CREATE TABLE IF NOT EXISTS proxy_request_payloads (
        request_id TEXT PRIMARY KEY,
        request_body TEXT NOT NULL,
        response_body TEXT,
        request_headers TEXT,
        response_headers TEXT,
        extracted_user_message TEXT,
        extracted_assistant_message TEXT,
        extracted_tools TEXT,
        extracted_thinking TEXT,
        payload_size_bytes INTEGER DEFAULT 0,
        created_at INTEGER NOT NULL,
        FOREIGN KEY (request_id) REFERENCES proxy_request_logs(request_id)
    )",
    [],
)
.map_err(|e| AppError::Database(format!("创建 proxy_request_payloads 表失败: {e}")))?;

conn.execute(
    "CREATE INDEX IF NOT EXISTS idx_payloads_created ON proxy_request_payloads(created_at)",
    [],
)
.map_err(|e| AppError::Database(format!("创建 idx_payloads_created 索引失败: {e}")))?;
```

- [ ] **Step 2: 在 `apply_schema_migrations()` 中加 FTS5 migration**

找到最后一个 migration 版本号（当前 v10），加 v11：

```rust
if version < 11 {
    conn.execute_batch(
        "CREATE VIRTUAL TABLE IF NOT EXISTS proxy_payload_fts USING fts5(
            extracted_user_message,
            extracted_assistant_message,
            extracted_thinking,
            content=proxy_request_payloads,
            content_rowid=rowid
        );
        PRAGMA user_version = 11;",
    )
    .map_err(|e| AppError::Database(format!("v11 migration 失败: {e}")))?;
}
```

- [ ] **Step 3: 编译验证**

```bash
cd /Volumes/Data/code/worktrees/cc-switch/cli-headless/src-tauri
cargo build 2>&1 | tail -5
```

Expected: 编译成功。

- [ ] **Step 4: Commit**

```bash
git add src-tauri/src/database/schema.rs
git commit -m "feat(db): add proxy_request_payloads table and FTS5 index"
```

---

### Task 3: payload_logger.rs — 写入逻辑 + 结构化提取

**Files:**
- Create: `src-tauri/src/proxy/usage/payload_logger.rs`
- Modify: `src-tauri/src/proxy/usage/mod.rs`

- [ ] **Step 1: 在 `usage/mod.rs` 中声明模块**

在 `pub mod parser;` 之后加：

```rust
pub mod payload_logger;
```

- [ ] **Step 2: 创建 `payload_logger.rs` — struct 和提取逻辑**

```rust
use crate::database::Database;
use crate::error::AppError;
use serde_json::Value;
use std::sync::Arc;

pub struct PayloadLog {
    pub request_id: String,
    pub request_body: Value,
    pub response_body: Option<Value>,
    pub request_headers: Option<Value>,
    pub response_headers: Option<Value>,
    pub created_at: i64,
}

struct ExtractedFields {
    user_message: Option<String>,
    assistant_message: Option<String>,
    tools: Option<String>,
    thinking: Option<String>,
}

pub struct PayloadLogger {
    db: Arc<Database>,
}

impl PayloadLogger {
    pub fn new(db: &Arc<Database>) -> Self {
        Self { db: db.clone() }
    }

    pub fn log_payload(&self, log: PayloadLog) -> Result<(), AppError> {
        let extracted = extract_fields(&log.request_body, log.response_body.as_ref());

        let request_body_str = serde_json::to_string(&log.request_body)
            .unwrap_or_else(|_| "{}".to_string());
        let response_body_str = log
            .response_body
            .as_ref()
            .and_then(|v| serde_json::to_string(v).ok());
        let request_headers_str = log
            .request_headers
            .as_ref()
            .and_then(|v| serde_json::to_string(v).ok());
        let response_headers_str = log
            .response_headers
            .as_ref()
            .and_then(|v| serde_json::to_string(v).ok());

        let payload_size = request_body_str.len()
            + response_body_str.as_ref().map(|s| s.len()).unwrap_or(0);

        let conn = crate::database::lock_conn!(self.db.conn);

        conn.execute(
            "INSERT OR REPLACE INTO proxy_request_payloads (
                request_id, request_body, response_body,
                request_headers, response_headers,
                extracted_user_message, extracted_assistant_message,
                extracted_tools, extracted_thinking,
                payload_size_bytes, created_at
            ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
            rusqlite::params![
                log.request_id,
                request_body_str,
                response_body_str,
                request_headers_str,
                response_headers_str,
                extracted.user_message,
                extracted.assistant_message,
                extracted.tools,
                extracted.thinking,
                payload_size as i64,
                log.created_at,
            ],
        )
        .map_err(|e| AppError::Database(format!("payload 写入失败: {e}")))?;

        // Update FTS5 index
        if extracted.user_message.is_some()
            || extracted.assistant_message.is_some()
            || extracted.thinking.is_some()
        {
            let rowid: Option<i64> = conn
                .query_row(
                    "SELECT rowid FROM proxy_request_payloads WHERE request_id = ?1",
                    [&log.request_id],
                    |row| row.get(0),
                )
                .ok();

            if let Some(rowid) = rowid {
                let _ = conn.execute(
                    "INSERT INTO proxy_payload_fts(rowid, extracted_user_message, extracted_assistant_message, extracted_thinking)
                     VALUES (?1, ?2, ?3, ?4)",
                    rusqlite::params![
                        rowid,
                        extracted.user_message,
                        extracted.assistant_message,
                        extracted.thinking,
                    ],
                );
            }
        }

        Ok(())
    }
}

fn extract_fields(request: &Value, response: Option<&Value>) -> ExtractedFields {
    let user_message = extract_last_user_message(request);
    let (assistant_message, tools, thinking) = response
        .map(|r| extract_response_fields(r))
        .unwrap_or((None, None, None));

    ExtractedFields {
        user_message,
        assistant_message,
        tools,
        thinking,
    }
}

fn extract_last_user_message(request: &Value) -> Option<String> {
    let messages = request.get("messages")?.as_array()?;
    let last_user = messages.iter().rev().find(|m| {
        m.get("role").and_then(|r| r.as_str()) == Some("user")
    })?;

    let content = last_user.get("content")?;
    if let Some(text) = content.as_str() {
        return Some(truncate_str(text, 10000));
    }
    // Anthropic array-of-blocks format
    if let Some(blocks) = content.as_array() {
        let texts: Vec<&str> = blocks
            .iter()
            .filter_map(|b| {
                if b.get("type").and_then(|t| t.as_str()) == Some("text") {
                    b.get("text").and_then(|t| t.as_str())
                } else {
                    None
                }
            })
            .collect();
        if !texts.is_empty() {
            return Some(truncate_str(&texts.join("\n"), 10000));
        }
    }
    None
}

fn extract_response_fields(response: &Value) -> (Option<String>, Option<String>, Option<String>) {
    // Try OpenAI format first: choices[0].message
    if let Some(choices) = response.get("choices").and_then(|c| c.as_array()) {
        if let Some(choice) = choices.first() {
            let msg = choice.get("message").unwrap_or(choice);
            let text = msg
                .get("content")
                .and_then(|c| c.as_str())
                .map(|s| truncate_str(s, 10000));
            let thinking = msg
                .get("reasoning_content")
                .and_then(|c| c.as_str())
                .map(|s| truncate_str(s, 5000));
            let tools = msg
                .get("tool_calls")
                .and_then(|t| t.as_array())
                .map(|calls| {
                    let summaries: Vec<Value> = calls
                        .iter()
                        .filter_map(|c| {
                            let name = c
                                .get("function")
                                .and_then(|f| f.get("name"))
                                .and_then(|n| n.as_str())?;
                            Some(serde_json::json!({"name": name}))
                        })
                        .collect();
                    serde_json::to_string(&summaries).unwrap_or_default()
                });
            return (text, tools, thinking);
        }
    }

    // Anthropic format: content[]
    if let Some(content) = response.get("content").and_then(|c| c.as_array()) {
        let mut texts = Vec::new();
        let mut tool_summaries = Vec::new();
        let mut thinking_parts = Vec::new();

        for block in content {
            match block.get("type").and_then(|t| t.as_str()) {
                Some("text") => {
                    if let Some(t) = block.get("text").and_then(|t| t.as_str()) {
                        texts.push(t);
                    }
                }
                Some("tool_use") => {
                    if let Some(name) = block.get("name").and_then(|n| n.as_str()) {
                        tool_summaries.push(serde_json::json!({"name": name}));
                    }
                }
                Some("thinking") => {
                    if let Some(t) = block.get("thinking").and_then(|t| t.as_str()) {
                        thinking_parts.push(t);
                    }
                }
                _ => {}
            }
        }

        let text = if texts.is_empty() {
            None
        } else {
            Some(truncate_str(&texts.join("\n"), 10000))
        };
        let tools = if tool_summaries.is_empty() {
            None
        } else {
            Some(serde_json::to_string(&tool_summaries).unwrap_or_default())
        };
        let thinking = if thinking_parts.is_empty() {
            None
        } else {
            Some(truncate_str(&thinking_parts.join("\n"), 5000))
        };
        return (text, tools, thinking);
    }

    (None, None, None)
}

fn truncate_str(s: &str, max_len: usize) -> String {
    if s.len() <= max_len {
        s.to_string()
    } else {
        let mut end = max_len;
        while end > 0 && !s.is_char_boundary(end) {
            end -= 1;
        }
        format!("{}...[truncated]", &s[..end])
    }
}
```

- [ ] **Step 3: 编译验证**

```bash
cd /Volumes/Data/code/worktrees/cc-switch/cli-headless/src-tauri
cargo build 2>&1 | tail -5
```

Expected: 编译成功。

- [ ] **Step 4: Commit**

```bash
git add src-tauri/src/proxy/usage/payload_logger.rs src-tauri/src/proxy/usage/mod.rs
git commit -m "feat(payload): add PayloadLogger with structured field extraction"
```

---

### Task 4: forwarder.rs — 捕获 request body

**Files:**
- Modify: `src-tauri/src/proxy/forwarder.rs`

- [ ] **Step 1: 在 `ForwardResult` struct 中加字段**

找到 `ForwardResult` 定义（约 line 36），加 `captured_request_body`：

```rust
pub struct ForwardResult {
    pub response: ProxyResponse,
    pub provider: Provider,
    pub claude_api_format: Option<String>,
    pub(crate) connection_guard: Option<ActiveConnectionGuard>,
    pub captured_request_body: Option<Value>,
}
```

- [ ] **Step 2: 在所有 `ForwardResult` 构造处加字段初始化**

搜索所有 `ForwardResult {` 构造点，每处加 `captured_request_body: None`。然后在 `forward_single_attempt` 方法中（`filtered_body` 可用的地方，约 line 1228 之后），在构造 `ForwardResult` 之前 clone body：

在 `let filtered_body = prepare_upstream_request_body(request_body);` 之后加：

```rust
let captured_body = filtered_body.clone();
```

在该方法返回的 `ForwardResult` 构造处，把 `captured_request_body: None` 改为：

```rust
captured_request_body: Some(captured_body),
```

- [ ] **Step 3: 编译验证并修复所有构造点**

```bash
cd /Volumes/Data/code/worktrees/cc-switch/cli-headless/src-tauri
cargo build 2>&1 | grep "error" | head -20
```

编译器会报所有 `ForwardResult` 构造缺少 `captured_request_body` 字段的错误。逐一修复，对非 `forward_single_attempt` 的构造点用 `captured_request_body: None`。

- [ ] **Step 4: 编译通过**

```bash
cargo build 2>&1 | tail -3
```

Expected: 编译成功。

- [ ] **Step 5: Commit**

```bash
git add src-tauri/src/proxy/forwarder.rs
git commit -m "feat(forwarder): capture request body in ForwardResult"
```

---

### Task 5: response_processor.rs — 捕获 response body + 调 log_payload

**Files:**
- Modify: `src-tauri/src/proxy/response_processor.rs`
- Modify: `src-tauri/src/proxy/handlers.rs`

- [ ] **Step 1: 添加 `spawn_log_payload` 函数**

在 `response_processor.rs` 中 `spawn_log_usage` 函数附近（约 line 570）加：

```rust
fn spawn_log_payload(
    state: &ProxyState,
    ctx: &RequestContext,
    request_body: Option<Value>,
    response_body: Option<Value>,
    request_headers: Option<Value>,
    response_headers: Option<Value>,
    request_id: &str,
) {
    let Some(request_body) = request_body else {
        return;
    };

    let db = state.db.clone();
    let request_id = request_id.to_string();
    let created_at = chrono::Utc::now().timestamp();

    tokio::spawn(async move {
        use super::usage::payload_logger::{PayloadLog, PayloadLogger};

        let logger = PayloadLogger::new(&db);
        let log = PayloadLog {
            request_id,
            request_body,
            response_body,
            request_headers,
            response_headers,
            created_at,
        };
        if let Err(e) = logger.log_payload(log) {
            log::warn!("payload 记录失败: {e}");
        }
    });
}
```

- [ ] **Step 2: 在 `handle_non_streaming()` 中捕获 response body 并调用 `spawn_log_payload`**

在 `handle_non_streaming` 中，找到 `json_value` 解析成功的位置（约 line 270-310），在 `spawn_log_usage` 调用之后加：

```rust
spawn_log_payload(
    state,
    ctx,
    ctx.captured_request_body.clone(),
    Some(json_value.clone()),
    None,
    None,
    &usage.dedup_request_id(),
);
```

注意：这需要 `RequestContext` 持有 `captured_request_body`。见 Step 4。

- [ ] **Step 3: 在 `SseUsageCollector` 的 `on_complete` 回调中调用 `spawn_log_payload`**

在 `handle_streaming` 创建 `SseUsageCollector` 的闭包（`on_complete` 回调）中，streaming events 聚合后也调 `spawn_log_payload`。

在 collector 的 `finish` 触发的回调中，把所有 SSE events 的 content 部分合并为一个 response JSON：

```rust
// 在 on_complete 闭包中，events 已经收集完毕
let aggregated_response = aggregate_sse_events(&events);
spawn_log_payload(
    &state_clone,
    &ctx_snapshot,
    captured_request_body_clone,
    Some(aggregated_response),
    None,
    None,
    &request_id,
);
```

在 `response_processor.rs` 底部加 SSE 聚合 helper：

```rust
fn aggregate_sse_events(events: &[Value]) -> Value {
    let mut text_parts = Vec::new();
    let mut reasoning_parts = Vec::new();
    let mut tool_calls = Vec::new();
    let mut model = None;
    let mut usage = None;

    for event in events {
        if model.is_none() {
            model = event.get("model").cloned();
        }
        if let Some(u) = event.get("usage") {
            usage = Some(u.clone());
        }
        if let Some(choices) = event.get("choices").and_then(|c| c.as_array()) {
            for choice in choices {
                let delta = choice.get("delta").unwrap_or(choice);
                if let Some(content) = delta.get("content").and_then(|c| c.as_str()) {
                    text_parts.push(content.to_string());
                }
                if let Some(reasoning) = delta.get("reasoning_content").and_then(|c| c.as_str()) {
                    reasoning_parts.push(reasoning.to_string());
                }
                if let Some(calls) = delta.get("tool_calls").and_then(|t| t.as_array()) {
                    tool_calls.extend(calls.iter().cloned());
                }
            }
        }
        // Anthropic streaming: content_block_delta
        if let Some(delta) = event.get("delta") {
            if let Some(text) = delta.get("text").and_then(|t| t.as_str()) {
                text_parts.push(text.to_string());
            }
            if let Some(thinking) = delta.get("thinking").and_then(|t| t.as_str()) {
                reasoning_parts.push(thinking.to_string());
            }
        }
    }

    let mut result = serde_json::json!({
        "choices": [{
            "message": {
                "role": "assistant",
                "content": text_parts.join(""),
            }
        }]
    });
    if !reasoning_parts.is_empty() {
        result["choices"][0]["message"]["reasoning_content"] =
            Value::String(reasoning_parts.join(""));
    }
    if !tool_calls.is_empty() {
        result["choices"][0]["message"]["tool_calls"] = Value::Array(tool_calls);
    }
    if let Some(m) = model {
        result["model"] = m;
    }
    if let Some(u) = usage {
        result["usage"] = u;
    }
    result
}
```

- [ ] **Step 4: RequestContext 传递 captured_request_body**

在 `handler_context.rs` 的 `RequestContext` struct 中加：

```rust
pub captured_request_body: Option<Value>,
```

在 handlers.rs 中，`ForwardResult` 返回后、调 `process_response` 前，把 `result.captured_request_body` 存到 `ctx.captured_request_body`：

```rust
ctx.captured_request_body = result.captured_request_body.take();
```

在 `process_response` 中把 `ctx.captured_request_body` 传给 `spawn_log_payload`。

- [ ] **Step 5: 编译验证**

```bash
cd /Volumes/Data/code/worktrees/cc-switch/cli-headless/src-tauri
cargo build 2>&1 | tail -10
```

Expected: 编译成功。

- [ ] **Step 6: Commit**

```bash
git add src-tauri/src/proxy/response_processor.rs src-tauri/src/proxy/handler_context.rs src-tauri/src/proxy/handlers.rs
git commit -m "feat(payload): capture and log full request/response bodies"
```

---

### Task 6: PayloadService — 查询层

**Files:**
- Create: `src-tauri/src/services/payload.rs`
- Modify: `src-tauri/src/services/mod.rs`
- Modify: `src-tauri/src/lib.rs`

- [ ] **Step 1: 在 `services/mod.rs` 中声明模块**

加：

```rust
pub mod payload;
```

- [ ] **Step 2: 在 `lib.rs` 中导出**

在 pub use 区域加：

```rust
pub use services::payload::PayloadService;
```

- [ ] **Step 3: 创建 `services/payload.rs`**

```rust
use crate::database::Database;
use crate::error::AppError;
use crate::store::AppState;
use serde::Serialize;
use serde_json::Value;
use std::io::Write;

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PayloadSearchResult {
    pub request_id: String,
    pub created_at: i64,
    pub app_type: String,
    pub model: String,
    pub session_id: Option<String>,
    pub user_message: Option<String>,
    pub assistant_message: Option<String>,
    pub tools_used: Option<Value>,
    pub input_tokens: u32,
    pub output_tokens: u32,
    pub total_cost_usd: String,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PayloadDetail {
    pub request_id: String,
    pub request_body: Value,
    pub response_body: Option<Value>,
    pub request_headers: Option<Value>,
    pub response_headers: Option<Value>,
    pub extracted_user_message: Option<String>,
    pub extracted_assistant_message: Option<String>,
    pub extracted_tools: Option<String>,
    pub extracted_thinking: Option<String>,
    pub payload_size_bytes: i64,
    pub created_at: i64,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PayloadStatGroup {
    pub key: String,
    pub request_count: u32,
    pub total_input_tokens: u64,
    pub total_output_tokens: u64,
    pub total_cost_usd: String,
    pub total_payload_bytes: u64,
}

pub struct PayloadService;

impl PayloadService {
    pub fn search(
        state: &AppState,
        query: &str,
        start: Option<i64>,
        end: Option<i64>,
        app_filter: Option<&str>,
    ) -> Result<Vec<PayloadSearchResult>, AppError> {
        let conn = crate::database::lock_conn!(state.db.conn);

        let mut sql = String::from(
            "SELECT p.request_id, p.created_at,
                    COALESCE(l.app_type, '') as app_type,
                    COALESCE(l.model, '') as model,
                    l.session_id,
                    p.extracted_user_message,
                    p.extracted_assistant_message,
                    p.extracted_tools,
                    COALESCE(l.input_tokens, 0),
                    COALESCE(l.output_tokens, 0),
                    COALESCE(l.total_cost_usd, '0')
             FROM proxy_payload_fts f
             JOIN proxy_request_payloads p ON p.rowid = f.rowid
             LEFT JOIN proxy_request_logs l ON l.request_id = p.request_id
             WHERE proxy_payload_fts MATCH ?1",
        );

        let mut params: Vec<Box<dyn rusqlite::types::ToSql>> = vec![Box::new(query.to_string())];
        let mut param_idx = 2;

        if let Some(s) = start {
            sql.push_str(&format!(" AND p.created_at >= ?{param_idx}"));
            params.push(Box::new(s));
            param_idx += 1;
        }
        if let Some(e) = end {
            sql.push_str(&format!(" AND p.created_at <= ?{param_idx}"));
            params.push(Box::new(e));
            param_idx += 1;
        }
        if let Some(app) = app_filter {
            sql.push_str(&format!(" AND l.app_type = ?{param_idx}"));
            params.push(Box::new(app.to_string()));
        }

        sql.push_str(" ORDER BY p.created_at DESC LIMIT 50");

        let param_refs: Vec<&dyn rusqlite::types::ToSql> = params.iter().map(|p| p.as_ref()).collect();
        let mut stmt = conn
            .prepare(&sql)
            .map_err(|e| AppError::Database(e.to_string()))?;

        let rows = stmt
            .query_map(param_refs.as_slice(), |row| {
                Ok(PayloadSearchResult {
                    request_id: row.get(0)?,
                    created_at: row.get(1)?,
                    app_type: row.get(2)?,
                    model: row.get(3)?,
                    session_id: row.get(4)?,
                    user_message: row.get(5)?,
                    assistant_message: row.get(6)?,
                    tools_used: row
                        .get::<_, Option<String>>(7)?
                        .and_then(|s| serde_json::from_str(&s).ok()),
                    input_tokens: row.get(8)?,
                    output_tokens: row.get(9)?,
                    total_cost_usd: row.get(10)?,
                })
            })
            .map_err(|e| AppError::Database(e.to_string()))?;

        rows.collect::<Result<Vec<_>, _>>()
            .map_err(|e| AppError::Database(e.to_string()))
    }

    pub fn get(state: &AppState, request_id: &str) -> Result<PayloadDetail, AppError> {
        let conn = crate::database::lock_conn!(state.db.conn);

        conn.query_row(
            "SELECT request_id, request_body, response_body,
                    request_headers, response_headers,
                    extracted_user_message, extracted_assistant_message,
                    extracted_tools, extracted_thinking,
                    payload_size_bytes, created_at
             FROM proxy_request_payloads WHERE request_id = ?1",
            [request_id],
            |row| {
                let request_body_str: String = row.get(1)?;
                let response_body_str: Option<String> = row.get(2)?;
                let request_headers_str: Option<String> = row.get(3)?;
                let response_headers_str: Option<String> = row.get(4)?;

                Ok(PayloadDetail {
                    request_id: row.get(0)?,
                    request_body: serde_json::from_str(&request_body_str).unwrap_or(Value::Null),
                    response_body: response_body_str
                        .and_then(|s| serde_json::from_str(&s).ok()),
                    request_headers: request_headers_str
                        .and_then(|s| serde_json::from_str(&s).ok()),
                    response_headers: response_headers_str
                        .and_then(|s| serde_json::from_str(&s).ok()),
                    extracted_user_message: row.get(5)?,
                    extracted_assistant_message: row.get(6)?,
                    extracted_tools: row.get(7)?,
                    extracted_thinking: row.get(8)?,
                    payload_size_bytes: row.get(9)?,
                    created_at: row.get(10)?,
                })
            },
        )
        .map_err(|e| AppError::Database(format!("payload not found: {e}")))
    }

    pub fn session(state: &AppState, session_id: &str) -> Result<Vec<PayloadSearchResult>, AppError> {
        let conn = crate::database::lock_conn!(state.db.conn);

        let mut stmt = conn
            .prepare(
                "SELECT p.request_id, p.created_at,
                        COALESCE(l.app_type, ''), COALESCE(l.model, ''),
                        l.session_id,
                        p.extracted_user_message, p.extracted_assistant_message,
                        p.extracted_tools,
                        COALESCE(l.input_tokens, 0), COALESCE(l.output_tokens, 0),
                        COALESCE(l.total_cost_usd, '0')
                 FROM proxy_request_payloads p
                 LEFT JOIN proxy_request_logs l ON l.request_id = p.request_id
                 WHERE l.session_id = ?1
                 ORDER BY p.created_at ASC",
            )
            .map_err(|e| AppError::Database(e.to_string()))?;

        let rows = stmt
            .query_map([session_id], |row| {
                Ok(PayloadSearchResult {
                    request_id: row.get(0)?,
                    created_at: row.get(1)?,
                    app_type: row.get(2)?,
                    model: row.get(3)?,
                    session_id: row.get(4)?,
                    user_message: row.get(5)?,
                    assistant_message: row.get(6)?,
                    tools_used: row
                        .get::<_, Option<String>>(7)?
                        .and_then(|s| serde_json::from_str(&s).ok()),
                    input_tokens: row.get(8)?,
                    output_tokens: row.get(9)?,
                    total_cost_usd: row.get(10)?,
                })
            })
            .map_err(|e| AppError::Database(e.to_string()))?;

        rows.collect::<Result<Vec<_>, _>>()
            .map_err(|e| AppError::Database(e.to_string()))
    }

    pub fn stats(
        state: &AppState,
        start: Option<i64>,
        end: Option<i64>,
        group_by: &str,
    ) -> Result<Vec<PayloadStatGroup>, AppError> {
        let conn = crate::database::lock_conn!(state.db.conn);

        let group_col = match group_by {
            "tool" => "p.extracted_tools",
            "app" => "l.app_type",
            _ => "l.model",
        };

        let mut sql = format!(
            "SELECT COALESCE({group_col}, 'unknown'),
                    COUNT(*),
                    SUM(COALESCE(l.input_tokens, 0)),
                    SUM(COALESCE(l.output_tokens, 0)),
                    PRINTF('%.6f', SUM(CAST(COALESCE(l.total_cost_usd, '0') AS REAL))),
                    SUM(COALESCE(p.payload_size_bytes, 0))
             FROM proxy_request_payloads p
             LEFT JOIN proxy_request_logs l ON l.request_id = p.request_id
             WHERE 1=1"
        );

        let mut params: Vec<Box<dyn rusqlite::types::ToSql>> = Vec::new();
        let mut idx = 1;

        if let Some(s) = start {
            sql.push_str(&format!(" AND p.created_at >= ?{idx}"));
            params.push(Box::new(s));
            idx += 1;
        }
        if let Some(e) = end {
            sql.push_str(&format!(" AND p.created_at <= ?{idx}"));
            params.push(Box::new(e));
        }

        sql.push_str(&format!(" GROUP BY {group_col} ORDER BY COUNT(*) DESC"));

        let param_refs: Vec<&dyn rusqlite::types::ToSql> = params.iter().map(|p| p.as_ref()).collect();
        let mut stmt = conn
            .prepare(&sql)
            .map_err(|e| AppError::Database(e.to_string()))?;

        let rows = stmt
            .query_map(param_refs.as_slice(), |row| {
                Ok(PayloadStatGroup {
                    key: row.get(0)?,
                    request_count: row.get(1)?,
                    total_input_tokens: row.get(2)?,
                    total_output_tokens: row.get(3)?,
                    total_cost_usd: row.get(4)?,
                    total_payload_bytes: row.get(5)?,
                })
            })
            .map_err(|e| AppError::Database(e.to_string()))?;

        rows.collect::<Result<Vec<_>, _>>()
            .map_err(|e| AppError::Database(e.to_string()))
    }

    pub fn export(
        state: &AppState,
        start: Option<i64>,
        end: Option<i64>,
        output_path: &str,
    ) -> Result<u64, AppError> {
        let conn = crate::database::lock_conn!(state.db.conn);

        let mut sql = String::from(
            "SELECT p.request_id, p.request_body, p.response_body,
                    p.extracted_user_message, p.extracted_assistant_message,
                    p.extracted_tools, p.extracted_thinking,
                    p.payload_size_bytes, p.created_at,
                    l.app_type, l.model, l.session_id,
                    l.input_tokens, l.output_tokens, l.total_cost_usd
             FROM proxy_request_payloads p
             LEFT JOIN proxy_request_logs l ON l.request_id = p.request_id
             WHERE 1=1",
        );

        let mut params: Vec<Box<dyn rusqlite::types::ToSql>> = Vec::new();
        let mut idx = 1;

        if let Some(s) = start {
            sql.push_str(&format!(" AND p.created_at >= ?{idx}"));
            params.push(Box::new(s));
            idx += 1;
        }
        if let Some(e) = end {
            sql.push_str(&format!(" AND p.created_at <= ?{idx}"));
            params.push(Box::new(e));
        }

        sql.push_str(" ORDER BY p.created_at ASC");

        let param_refs: Vec<&dyn rusqlite::types::ToSql> = params.iter().map(|p| p.as_ref()).collect();
        let mut stmt = conn
            .prepare(&sql)
            .map_err(|e| AppError::Database(e.to_string()))?;

        let mut file = std::fs::File::create(output_path)
            .map_err(|e| AppError::Message(format!("创建导出文件失败: {e}")))?;

        let mut count: u64 = 0;
        let mut rows = stmt
            .query(param_refs.as_slice())
            .map_err(|e| AppError::Database(e.to_string()))?;

        while let Some(row) = rows.next().map_err(|e| AppError::Database(e.to_string()))? {
            let entry = serde_json::json!({
                "request_id": row.get::<_, String>(0).unwrap_or_default(),
                "request_body": row.get::<_, String>(1).ok().and_then(|s| serde_json::from_str::<Value>(&s).ok()),
                "response_body": row.get::<_, Option<String>>(2).ok().flatten().and_then(|s| serde_json::from_str::<Value>(&s).ok()),
                "extracted_user_message": row.get::<_, Option<String>>(3).unwrap_or(None),
                "extracted_assistant_message": row.get::<_, Option<String>>(4).unwrap_or(None),
                "extracted_tools": row.get::<_, Option<String>>(5).unwrap_or(None),
                "extracted_thinking": row.get::<_, Option<String>>(6).unwrap_or(None),
                "payload_size_bytes": row.get::<_, i64>(7).unwrap_or(0),
                "created_at": row.get::<_, i64>(8).unwrap_or(0),
                "app_type": row.get::<_, Option<String>>(9).unwrap_or(None),
                "model": row.get::<_, Option<String>>(10).unwrap_or(None),
                "session_id": row.get::<_, Option<String>>(11).unwrap_or(None),
                "input_tokens": row.get::<_, Option<u32>>(12).unwrap_or(None),
                "output_tokens": row.get::<_, Option<u32>>(13).unwrap_or(None),
                "total_cost_usd": row.get::<_, Option<String>>(14).unwrap_or(None),
            });
            serde_json::to_writer(&mut file, &entry)
                .map_err(|e| AppError::Message(format!("写入失败: {e}")))?;
            writeln!(file).map_err(|e| AppError::Message(format!("写入换行失败: {e}")))?;
            count += 1;
        }

        Ok(count)
    }

    pub fn prune(
        state: &AppState,
        before_timestamp: i64,
        dry_run: bool,
    ) -> Result<u64, AppError> {
        let conn = crate::database::lock_conn!(state.db.conn);

        let count: u64 = conn
            .query_row(
                "SELECT COUNT(*) FROM proxy_request_payloads WHERE created_at < ?1",
                [before_timestamp],
                |row| row.get(0),
            )
            .map_err(|e| AppError::Database(e.to_string()))?;

        if dry_run || count == 0 {
            return Ok(count);
        }

        // Delete FTS entries first
        conn.execute(
            "DELETE FROM proxy_payload_fts WHERE rowid IN (
                SELECT rowid FROM proxy_request_payloads WHERE created_at < ?1
            )",
            [before_timestamp],
        )
        .map_err(|e| AppError::Database(format!("FTS 清理失败: {e}")))?;

        conn.execute(
            "DELETE FROM proxy_request_payloads WHERE created_at < ?1",
            [before_timestamp],
        )
        .map_err(|e| AppError::Database(format!("payload 清理失败: {e}")))?;

        Ok(count)
    }
}
```

- [ ] **Step 4: 编译验证**

```bash
cd /Volumes/Data/code/worktrees/cc-switch/cli-headless/src-tauri
cargo build 2>&1 | tail -5
```

Expected: 编译成功。

- [ ] **Step 5: Commit**

```bash
git add src-tauri/src/services/payload.rs src-tauri/src/services/mod.rs src-tauri/src/lib.rs
git commit -m "feat(payload): add PayloadService with search/get/session/stats/export/prune"
```

---

### Task 7: CLI payload 子命令

**Files:**
- Modify: `src-tauri/src/bin/cc-switch-cli.rs`

- [ ] **Step 1: 添加 `cmd_payload` 函数**

在 `cmd_backup` 函数之后加：

```rust
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
```

- [ ] **Step 2: 在 `run()` 的 command dispatch 中加 `"payload"` 分支**

找到 `match command.as_str()` 块（约 line 919），加：

```rust
"payload" => cmd_payload(&app, args).await?,
```

- [ ] **Step 3: 编译验证**

```bash
cd /Volumes/Data/code/worktrees/cc-switch/cli-headless/src-tauri
cargo build 2>&1 | tail -5
```

Expected: 编译成功。

- [ ] **Step 4: 重新 build release binary**

```bash
cd /Volumes/Data/code/worktrees/cc-switch/cli-headless/src-tauri
cargo build --release 2>&1 | tail -5
```

Expected: release 编译成功，`target/release/cc-switch-cli` 更新。

- [ ] **Step 5: Commit**

```bash
git add src-tauri/src/bin/cc-switch-cli.rs
git commit -m "feat(cli): add payload subcommand (search/get/session/stats/export/prune)"
```

---

### Task 8: 端到端真机测试

**Files:** 无代码改动，纯验证。

- [ ] **Step 1: 重启 cc-switch 使新 binary 生效**

```bash
svc restart cc-switch
sleep 3
svc status cc-switch
```

Expected: cc-switch running + healthy。

- [ ] **Step 2: AC1 — 验证 Data 盘数据目录**

```bash
ls -la /Volumes/Data/cc-switch/cc-switch.db
cat /Volumes/Data/cc-switch/proxy.pid
curl -sS http://127.0.0.1:15721/health
```

Expected: DB 文件存在，proxy.pid 指向正确 PID，health 返回 healthy。

- [ ] **Step 3: AC2 — 非流式 payload 记录**

```bash
# 发一条非流式请求
curl -sS http://127.0.0.1:15721/v1/chat/completions \
  -H "Content-Type: application/json" \
  -H "Authorization: Bearer cc-switch" \
  -d '{"model":"lingzhi/deepseek-v4-pro","messages":[{"role":"user","content":"Say hello"}],"max_tokens":10,"stream":false}'

# 查最近的 request_id
REQUEST_ID=$(sqlite3 /Volumes/Data/cc-switch/cc-switch.db "SELECT request_id FROM proxy_request_payloads ORDER BY created_at DESC LIMIT 1;")
echo "Request ID: $REQUEST_ID"

# 用 CLI 查看完整 payload
cc-switch-cli --config-dir /Volumes/Data/cc-switch payload get --id "$REQUEST_ID" --pretty
```

Expected: 返回完整 JSON，`request_body` 包含 messages 数组，`response_body` 包含 choices，`extracted_user_message` = "Say hello"。

- [ ] **Step 4: AC3 — 流式 payload 记录**

```bash
curl -sS http://127.0.0.1:15721/v1/chat/completions \
  -H "Content-Type: application/json" \
  -H "Authorization: Bearer cc-switch" \
  -d '{"model":"lingzhi/deepseek-v4-pro","messages":[{"role":"user","content":"Count to 3"}],"max_tokens":20,"stream":true}'

sleep 2
REQUEST_ID=$(sqlite3 /Volumes/Data/cc-switch/cc-switch.db "SELECT request_id FROM proxy_request_payloads ORDER BY created_at DESC LIMIT 1;")
cc-switch-cli --config-dir /Volumes/Data/cc-switch payload get --id "$REQUEST_ID" --pretty
```

Expected: `response_body` 包含聚合后的完整 assistant message（不是 SSE 碎片）。

- [ ] **Step 5: AC5 — FTS5 搜索**

```bash
cc-switch-cli --config-dir /Volumes/Data/cc-switch payload search --query "hello" --pretty
```

Expected: 返回至少 1 条结果，包含之前发的 "Say hello" 请求。

- [ ] **Step 6: AC6 — Session 对话回溯**

```bash
# 查一个 session_id
SESSION_ID=$(sqlite3 /Volumes/Data/cc-switch/cc-switch.db "SELECT session_id FROM proxy_request_logs WHERE session_id IS NOT NULL ORDER BY created_at DESC LIMIT 1;")
cc-switch-cli --config-dir /Volumes/Data/cc-switch payload session --id "$SESSION_ID" --pretty
```

Expected: 按时序返回该 session 的所有请求。

- [ ] **Step 7: AC7 — 统计**

```bash
cc-switch-cli --config-dir /Volumes/Data/cc-switch payload stats --group-by model --pretty
```

Expected: 按 model 分组显示 request_count、token 总量。

- [ ] **Step 8: AC8 — 导出**

```bash
cc-switch-cli --config-dir /Volumes/Data/cc-switch payload export --output /tmp/payload-test.jsonl --pretty
head -1 /tmp/payload-test.jsonl | python3 -m json.tool | head -10
wc -l /tmp/payload-test.jsonl
```

Expected: JSONL 文件行数 >= 2，每行可解析为 JSON。

- [ ] **Step 9: AC9 — 清理 dry-run**

```bash
YESTERDAY=$(date -v-1d +%s)
cc-switch-cli --config-dir /Volumes/Data/cc-switch payload prune --before "$YESTERDAY" --dry-run --pretty
```

Expected: 显示 `would_delete` 数量，`dry_run: true`。

- [ ] **Step 10: AC4 — Anthropic 格式（Claude Code 真实请求）**

```bash
# 用 Claude Code 通过 cc-switch 发一条请求（需要 set_claude_ccswitch_lingzhi 或 set_claude_ccswitch_gpt）
# 然后检查 payload 是否记录
sleep 5
REQUEST_ID=$(sqlite3 /Volumes/Data/cc-switch/cc-switch.db "SELECT request_id FROM proxy_request_payloads ORDER BY created_at DESC LIMIT 1;")
cc-switch-cli --config-dir /Volumes/Data/cc-switch payload get --id "$REQUEST_ID" --pretty | head -30
```

Expected: Anthropic 格式的 request_body（有 `messages` 数组、`system` 字段），extracted 字段正确。

- [ ] **Step 11: AC10 — 性能验证**

```bash
curl -sS http://127.0.0.1:15721/status | python3 -c "import json,sys; d=json.load(sys.stdin); print(f'requests={d[\"total_requests\"]} success_rate={d[\"success_rate\"]}%')"
```

Expected: success_rate = 100.0。

- [ ] **Step 12: 全部 AC Pass → Commit test evidence**

如果以上全部 Pass，在 spec 目录记录验证结果：

```bash
cd /Volumes/Data/code/worktrees/cc-switch/cli-headless
echo "All AC1-AC10 passed on $(date '+%Y-%m-%d %H:%M')" >> docs/specs/2026-05-27-payload-recording-design.md
git add -A
git commit -m "feat(payload): all acceptance criteria verified on real machine"
```
