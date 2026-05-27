# CC Switch Payload Recording

全量记录经过 CC Switch proxy 的 LLM 请求和响应，支持历史查询、用量统计和信息挖掘。

## 动机

CC Switch 作为所有 AI Coding Tool（Claude Code、CodeWhale、OpenCode 等）的统一 LLM proxy，是记录完整对话历史的最佳拦截点。当前 `proxy_request_logs` 只存 token 计量和 cost，不存请求/响应内容。完整 body 对以下场景有价值：

- **历史回溯**：「我上周问过 DeepSeek 怎么配置 etcd，它怎么回答的？」
- **对话重放**：按 session 维度还原完整对话流（prompt 演变、tool use、thinking）
- **模式分析**：哪些 tool 最常用、哪些 prompt pattern 效果好、error 分布
- **跨工具统一**：Claude Code 和 CodeWhale 的对话历史集中在一处

## 存储

### 数据目录

整个 cc-switch 数据目录通过 ENV 控制，迁移到 Data 盘：

| 项 | 值 |
|---|---|
| ENV | `CC_SWITCH_DATA_DIR` |
| 默认值 | `/Volumes/Data/cc-switch/` |
| fallback | `~/.cc-switch/`（Data 盘不可用时） |
| 传入方式 | `cc-switch-cli --config-dir "$CC_SWITCH_DATA_DIR" proxy start` |

svc wrapper 脚本设置：

```zsh
export CC_SWITCH_DATA_DIR="/Volumes/Data/cc-switch"
"$CC_SWITCH_CLI" --config-dir "$CC_SWITCH_DATA_DIR" proxy start
```

迁移：`mv ~/.cc-switch/cc-switch.db /Volumes/Data/cc-switch/cc-switch.db`，其余文件（`codex_oauth_auth.json`、`logs/`、`backups/`）同步迁移。

### 数据模型

在主库 `cc-switch.db` 中新建表（与 `proxy_request_logs` 同库、FK 关联、事务一致）：

```sql
CREATE TABLE proxy_request_payloads (
    request_id                  TEXT PRIMARY KEY,
    request_body                TEXT NOT NULL,
    response_body               TEXT,
    request_headers             TEXT,
    response_headers            TEXT,
    extracted_user_message      TEXT,
    extracted_assistant_message TEXT,
    extracted_tools             TEXT,
    extracted_thinking          TEXT,
    payload_size_bytes          INTEGER DEFAULT 0,
    created_at                  INTEGER NOT NULL,
    FOREIGN KEY (request_id) REFERENCES proxy_request_logs(request_id)
);

CREATE INDEX idx_payloads_created ON proxy_request_payloads(created_at);
```

FTS5 全文检索：

```sql
CREATE VIRTUAL TABLE proxy_payload_fts USING fts5(
    extracted_user_message,
    extracted_assistant_message,
    extracted_thinking,
    content=proxy_request_payloads,
    content_rowid=rowid
);
```

### 字段说明

| 字段 | 内容 |
|---|---|
| `request_body` | 发给上游的完整 JSON（`filtered_body`，已去除 private params） |
| `response_body` | 上游返回的完整 JSON；流式请求存聚合后的完整 assistant message |
| `request_headers` | 请求头 JSON（过滤 Authorization） |
| `response_headers` | 响应头 JSON |
| `extracted_user_message` | 从 messages 数组提取的最后一条 user content |
| `extracted_assistant_message` | 从 response 提取的 assistant text content |
| `extracted_tools` | `[{name, input_summary}]` tool_use 调用摘要 |
| `extracted_thinking` | reasoning/thinking 内容 |
| `payload_size_bytes` | request_body + response_body 的总字节数 |

## 代码拦截点

在 `cc_switch_lib` 核心层实现，GUI 和 CLI 都自动受益。

### 数据流

```
Client Request
    │
    ▼
handler (handlers.rs)
    │  body: Value
    │  ctx: RequestContext
    │
    ▼
forwarder.forward_with_retry()
    │  filtered_body: Value  ← 【拦截点 1：request body】
    │
    ▼
upstream API
    │
    ▼ (非流式)                          ▼ (流式)
handle_non_streaming()               create_logged_passthrough_stream()
    │                                    │
    │  body_bytes → json_value          SseUsageCollector.push(event)
    │  ← 【拦截点 2a】                   │
    │                                    collector.finish()
    │                                    ← 【拦截点 2b：聚合后】
    ▼
log_usage_internal()    ← 现有，token counts
log_payload()           ← 新增，完整 body + 结构化提取
```

### 改动文件

**1. `proxy/forwarder.rs` — 捕获 request body**

`ForwardResult` 新增字段：

```rust
pub struct ForwardResult {
    pub response: ProxyResponse,
    pub provider: Provider,
    pub claude_api_format: Option<String>,
    pub(crate) connection_guard: Option<ActiveConnectionGuard>,
    pub captured_request_body: Option<Value>,  // 新增
}
```

在 `filtered_body` 序列化前（line ~1576）clone 存入。

**2. `proxy/response_processor.rs` — 捕获 response body**

- 非流式 `handle_non_streaming()`：`json_value` 解析后 clone
- 流式 `SseUsageCollector::finish()`：把累积的 SSE events 合并为完整 assistant message JSON

两个路径最终都调用新增的 `log_payload()`。

**3. 新增 `proxy/usage/payload_logger.rs` — 写入和提取**

```rust
pub struct PayloadLog {
    pub request_id: String,
    pub request_body: Value,
    pub response_body: Option<Value>,
    pub request_headers: Value,
    pub response_headers: Option<Value>,
    pub created_at: i64,
}

pub async fn log_payload(db: &Database, log: PayloadLog) {
    // 1. 提取结构化字段
    // 2. INSERT INTO proxy_request_payloads
    // 3. 更新 FTS5 索引
}
```

提取逻辑按 API 格式分两路：

- **Anthropic** (`/v1/messages`): `request.messages[-1]` (role=user), `response.content[].text`, `response.content[].type=="tool_use"`, `response.content[].type=="thinking"`
- **OpenAI** (`/v1/chat/completions`): `request.messages[-1]` (role=user), `response.choices[0].message.content`, `response.choices[0].message.tool_calls`, `response.choices[0].message.reasoning_content`

**4. `database/schema.rs` — DDL**

在 `init_tables()` 中加建表和 FTS5 的 SQL。

### 性能

- body clone 和 DB 写入在 `tokio::spawn` 里异步执行，不阻塞请求响应路径
- `Value::clone()` 成本：几十 KB 到几 MB，单次 ~1ms，可接受
- FTS5 索引更新在同一个异步 task 里
- 可选：通过 setting `payloadRecordingEnabled` 控制开关（默认 on）

## CLI 接口

`cc-switch-cli` 新增 `payload` 子命令：

```bash
# 全文搜索
cc-switch-cli payload search --query "etcd 配置" [--start 2026-05-01] [--end 2026-05-27] [--app codex]

# 查看完整 payload
cc-switch-cli payload get --id <request_id> [--pretty]

# 按 session 回溯对话
cc-switch-cli payload session --id <session_id> [--pretty]

# 统计
cc-switch-cli payload stats [--start ...] [--end ...] [--group-by model|tool|app]

# 导出
cc-switch-cli payload export --start 2026-05-01 --end 2026-05-27 --format jsonl --output /tmp/payloads.jsonl

# 清理
cc-switch-cli payload prune --before 2026-04-01 [--dry-run]
```

### 搜索结果格式

```json
{
  "request_id": "session:msg_abc123",
  "created_at": "2026-05-27T13:51:53",
  "app_type": "codex",
  "model": "deepseek-v4-pro",
  "session_id": "sess_xyz",
  "user_message": "怎么部署 cc-switch...",
  "assistant_message": "你可以用 svc...",
  "tools_used": ["Bash", "Read"],
  "tokens": {"input": 1234, "output": 567},
  "cost_usd": "0.0123"
}
```

搜索只返回 extracted 字段（轻量）。`payload get` 返回完整 body。

## Retention

- `payload prune --before <date>` 只删 `proxy_request_payloads` 行，`proxy_request_logs` 统计保留
- ENV `CC_SWITCH_PAYLOAD_RETENTION_DAYS`（可选，默认不限）
- prune 后自动 `VACUUM`

## 不做

- Web UI（CLI + 现有 usage-dashboard 够用）
- 实时 tail/stream
- 跨机同步（Data 盘有 Time Machine）
- 请求/响应加密（本地 DB，Data 盘已是加密 APFS 卷）

## 实现顺序

1. 迁移数据目录到 Data 盘 + svc wrapper 适配 `CC_SWITCH_DATA_DIR`
2. 建表 + FTS5（`database/schema.rs`）
3. `payload_logger.rs`：写入逻辑 + 结构化提取
4. `forwarder.rs`：`ForwardResult` 加 `captured_request_body`
5. `response_processor.rs`：非流式 + 流式 body 捕获，调 `log_payload()`
6. `cc-switch-cli payload` 子命令（search / get / session / stats / export / prune）
7. 端到端验证
