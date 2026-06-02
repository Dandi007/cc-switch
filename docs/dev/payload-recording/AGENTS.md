# Dev Task: CC Switch Payload Recording

你正在实现 CC Switch proxy 的请求/响应全量记录功能。请严格按照以下文档执行。

## 任务文档

| 文档 | 路径 | 说明 |
|------|------|------|
| Spec | `docs/dev/payload-recording/spec.md` | 需求、技术方案、约束、验收标准 |
| Plan | `docs/dev/payload-recording/plan.md` | 分步实现计划（Task 2-7，Task 1 已完成） |

## 工作规则

1. **先读 spec，再读 plan**，确保理解全貌后再动手
2. **从 Task 2 开始**，Task 1（数据目录迁移）已完成
3. 严格按 plan.md 的步骤顺序执行 Task 2 → Task 3 → Task 4 → Task 5 → Task 6 → Task 7
4. **跳过 Task 8**（端到端真机测试），这个由人工验证
5. 每完成一步，运行该步骤的验证命令（主要是 `cargo build`），确认通过再继续
6. 不要偏离 spec.md 的范围，不要添加 spec 没有提到的功能
7. 遵守 repo 现有的代码风格和约定（中文注释、rusqlite params 模式、`lock_conn!` 宏）
8. 完成后做原子化 commit，每个 Task 一个 commit
9. 如果 plan 中某步不可行或发现遗漏，在该步骤下方注释说明原因，调整后继续
10. **最后执行 `cargo build --release`** 生成 release binary

## 关键文件速查

| 文件 | 职责 |
|------|------|
| `src-tauri/src/database/schema.rs` | DDL 建表 + migration（`create_tables_on_conn` + `apply_schema_migrations`） |
| `src-tauri/src/proxy/usage/mod.rs` | usage 模块声明（加 `payload_logger`） |
| `src-tauri/src/proxy/usage/logger.rs` | 现有 RequestLog 写入模式（参考） |
| `src-tauri/src/proxy/usage/parser.rs` | TokenUsage struct 和 `dedup_request_id`（参考） |
| `src-tauri/src/proxy/forwarder.rs` | ForwardResult struct（加 `captured_request_body`），`filtered_body` 在 ~line 1228 |
| `src-tauri/src/proxy/response_processor.rs` | 非流式 `handle_non_streaming` (~line 243)，流式 `SseUsageCollector` (~line 368)，`spawn_log_usage` (~line 570) |
| `src-tauri/src/proxy/handler_context.rs` | RequestContext struct（加 `captured_request_body`） |
| `src-tauri/src/proxy/handlers.rs` | `handle_chat_completions` (~line 635)，`handle_messages_for_app` (~line 111) |
| `src-tauri/src/services/mod.rs` | service 模块声明（加 `payload`） |
| `src-tauri/src/bin/cc-switch-cli.rs` | CLI 命令路由（加 `payload` 子命令） |
| `src-tauri/src/lib.rs` | pub use 导出（加 `PayloadService`） |

## 技术栈

- 语言: Rust (edition 2021, MSRV 1.85.0)
- 框架: Tauri 2.x (shared lib `cc_switch_lib`)
- 构建: `cargo build` (在 `src-tauri/` 目录下)
- 测试: `cargo test` (rusqlite in-memory DB + wiremock)
- 数据库: SQLite via rusqlite, FTS5
- HTTP: axum (proxy server), reqwest (upstream client)
- 异步: tokio
- CLI: 自定义 arg parser（不用 clap）

## 构建命令

```bash
cd src-tauri
cargo build          # debug build（验证编译）
cargo build --release  # release build（最终产物）
cargo test           # 运行测试
```

## 注意事项

- `lock_conn!` 宏用于获取 DB 连接：`let conn = crate::database::lock_conn!(self.db.conn);`
- `AppError::Database(String)` 用于 DB 错误
- 异步日志用 `tokio::spawn` + clone state，参考 `spawn_log_usage` 模式
- DB struct 是 `Arc<Database>`，通过 `state.db.clone()` 传入 spawn
- plan 中的代码是参考实现，你可以根据编译器反馈调整细节（如类型适配），但保持 API 和行为不变
