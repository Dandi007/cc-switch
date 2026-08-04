# 修复 provider_health 外键建模错误导致的健康度记录丢失

development_id: `dev_ccswitch_lingzhi_claude_provider_01`
repo: `/data/code/cc-switch`（Rust，fork Dandi007/cc-switch，当前分支 `feat/quota-endpoint`）

## Goal

止住 cc-switch 每天静默丢弃约 370 条灵智健康度记录的问题，让 Claude 侧走灵智的故障转移判断重新有依据。

`provider_health` 的外键把「观测到的上游健康度」错误地建模成了「配置路由条目的子表」。这两者不是一回事，因此凡是解析出的上游在 `providers` 里没有对应 `(id, app_type)` 行的组合，健康度写入必然失败。

## Background：根因（已实测坐实）

日志现象——24 小时内 369 次，首现 2026-07-28，只有 `provider_id=lingzhi`、只有 `[claude]` app：

```
WARN [claude] 异步记录 Provider 成功结果失败: provider_id=lingzhi,
     error=数据库错误: FOREIGN KEY constraint failed
```

表定义：

```sql
CREATE TABLE provider_health (
  provider_id TEXT NOT NULL, app_type TEXT NOT NULL, is_healthy INTEGER NOT NULL DEFAULT 1,
  consecutive_failures INTEGER NOT NULL DEFAULT 0, last_success_at TEXT, last_failure_at TEXT,
  last_error TEXT, updated_at TEXT NOT NULL,
  PRIMARY KEY (provider_id, app_type),
  FOREIGN KEY (provider_id, app_type) REFERENCES providers(id, app_type) ON DELETE CASCADE
)
```

关键证据——`providers` 表在 `app_type='claude'` 下只有两行，且**没有任何一行 `is_current=1`**：

```
default        | claude | default         | is_current=0
claude-official| claude | Claude Official | is_current=0
```

而今日 `proxy_request_logs` 里 `app_type='claude'` 的 411 次请求，`provider_id` **全部是 `lingzhi`**。

即：转发侧写入的 `provider_id` 是**解析后的上游标识**，而 `providers` 表存的是**配置的路由条目**。Claude app 的路由根本不来自 `providers`（无 current 行），来自 env + per-model 配置。因此 `(lingzhi, claude)` 这个组合在 `providers` 中永远不会存在，FK 永远无法满足。

`provider_health` 现存的两行 `(lingzhi, codex)` 和 `(gpt, codex)` 之所以能写入，只是因为它们碰巧在 `providers` 里也被配置了——是巧合，不是设计成立。

## Required behavior

1. 去掉 `provider_health` 上指向 `providers` 的外键约束，保留 `PRIMARY KEY (provider_id, app_type)`。健康度是对上游的**观测记录**，其生命周期不应从属于配置条目。

2. SQLite 无法直接 `DROP CONSTRAINT`，需走表重建：`CREATE TABLE provider_health_new (...)` → `INSERT INTO ... SELECT * FROM provider_health` → `DROP TABLE provider_health` → `ALTER TABLE provider_health_new RENAME TO provider_health`。写成幂等 migration（重复执行不报错、不丢数据）。

3. 原 FK 带 `ON DELETE CASCADE`——删除 provider 配置时会连带清掉健康度。去掉 FK 后该行为消失。这是可接受的：健康度行极少（当前 2 行），且陈旧行会被下一次同 key 的写入覆盖。**不要**为此新增清理逻辑，除非顺手加一条「删除 provider 时一并删同 key 健康度行」的显式调用——两种做法都可以，选一种并在 PR 描述里说明。

4. 健康度写入路径（`src-tauri/src/proxy/forwarder.rs` 中「异步记录 Provider 成功结果」附近）保持 upsert 语义不变，去掉 FK 后应能直接成功。

5. 该写入失败当前只打 `WARN` 就吞掉。保留这个不阻塞转发的行为（健康度记账绝不能影响推理请求），但确认失败时错误信息里带上 `provider_id` 与 `app_type` 两者，便于下次定位。

## Do not change

- **不改任何推理转发逻辑**、provider 路由、退避重试策略。
- **不改 `proxy_request_logs`** 的 schema 或写入路径——它工作正常（今日正常写入 4,825 条）。
- **不往 `providers` 表插入合成行**来绕过 FK。那会把观测数据污染进配置表，且下次配置同步可能被抹掉。
- **不改 `/quota` 端点**（另有单元 `dev_ccswitch_retire_lingzhi_quota_01` 处理它）。

## 操作注意

生产库 `/data/cc-switch/cc-switch.db` 当前约 **77GB**，且 cc-switch proxy 持有写锁常驻运行。

- 开发与测试用小规模临时库，**不要**在 77GB 生产库上做实验
- migration 只重建 `provider_health`（2 行），与库体积无关，实际耗时应在毫秒级
- 若需在生产库验证，先停 `systemctl --user stop cc-switch-proxy`（会打断在跑的推理流量），备份后再执行
- 只读查询生产库必须用 `sqlite3 -readonly 'file:/data/cc-switch/cc-switch.db?immutable=1'`

## Verification

- 新增测试：在临时库上跑 migration，断言重建后 `provider_health` 行数与内容不变、外键已消失（`PRAGMA foreign_key_list(provider_health)` 返回空）
- 新增测试：向 `provider_health` 写入一个在 `providers` 中不存在的 `(provider_id, app_type)` 组合，断言写入成功
- 新增测试：migration 连续执行两次不报错、不丢数据（幂等）
- 跑完整 Rust 测试套件
- 部署后集成验证：
  - `journalctl --user -u cc-switch-proxy --since "1 hour ago" | grep -c "FOREIGN KEY constraint failed"` 应为 `0`
  - `SELECT provider_id, app_type, datetime(updated_at) FROM provider_health` 应出现 `(lingzhi, claude)` 行，且 `updated_at` 在近 1 小时内
- 保持 worktree 干净，`git diff --check` 干净

## 参考

- 实测证据：本 work folder `../findings-verified.md` §3.2
- 完整方案：`../spec.md` §6.1 F-2
- 派发索引与共享上下文：`./README.md`
