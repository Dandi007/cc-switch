# 退役 cc-switch /quota 的灵智计费单元

development_id: `dev_ccswitch_retire_lingzhi_quota_01`
repo: `/data/code/cc-switch`（Rust，fork Dandi007/cc-switch，当前分支 `feat/quota-endpoint`）
依赖：`dev_lingzhi_analysis_endpoints_03`（D3）已合并——quota-api 已完整覆盖灵智额度查询

## Goal

移除 cc-switch `/quota` 里的灵智计费单元，让灵智额度查询只剩 quota-api 一个出口。

该单元当前不仅坏，而且**会撒谎**：无论上游返回什么，它都报 `success: true` 加一组零值。一个稳定输出零的监控面比没有监控面更危险——它让人以为余额真的是 0，或者以为查询链路是通的。

## Background：为什么是退役而不是修复

`usage_script` 存放在 `providers.meta` 列，内容为：

```js
({ request: { method:'GET', url:'{{baseUrl}}/api/user/self',
              headers:{ 'Authorization':'Bearer {{apiKey}}', 'New-Api-User':'{{userId}}' } },
   extractor: function(d){ var u=d.data||d;
     var rem=(u.quota||0)/500000.0; var used=(u.used_quota||0)/500000.0;
     return { isValid:true, remaining:rem, used:used, total:rem+used,
              unit:'USD', planName:'zhiyuan' }; } })
```

两个独立缺陷：

1. **用错凭证种类**。`{{apiKey}}` 解析自 `settings_config.apiKey` = `{env:LINGZHI_API_KEY}`，即**推理用的 API key**（51 字符）。而 `/api/user/self` 要 **access token**（28 字符）。实测：
   ```
   LINGZHI_API_KEY      → {"message":"无权进行此操作，access token 无效","success":false}
   LINGZHI_ACCESS_TOKEN → success=true, quota=2689033487
   ```
2. **失败被硬编码成成功**。extractor 里 `isValid: true` 与 `planName: 'zhiyuan'` 是常量；`d.data||d` 在错误响应下落到错误对象，`u.quota` 为 undefined，`(undefined||0)` → `0`。

即使修好凭证，cc-switch 也会成为**第二个**灵智额度查询实现，需要第二份 access token 副本注入 proxy 进程——直接违反「灵智查询唯一出口」与「凭证只有一份权威落点」两条不变量。而现存的 `/data/cc-switch/cc-switch.env` 里那份 32 字符 `LINGZHI_ACCESS_TOKEN`（实测已失效）正是上一次这么做留下的残骸。

quota-api 的等价能力已经在跑且能与上游对账（累计消费 $11,180.838 vs 上游 $11,180.81）。

## Required behavior

1. 让 `billing_unit()`（`src-tauri/src/proxy/quota.rs`）不再为 lingzhi provider 产出计费单元。**优先做法**：在 provider 配置侧关掉——把 lingzhi 的 `meta.usage_script.enabled` 置为 `false`，或整个移除 `usage_script`。因为 `has_usage_script_enabled()` 为 false 时 `billing_unit()` 已经自然返回 `None`，无需改判定逻辑。

2. 若选择改配置，需要一个幂等 migration 来更新 `providers.meta`，而不是只在运行时内存里改——否则重启后复现。

3. `/quota` 端点本身保留。codex OAuth 计费单元走 `is_codex_oauth()` 分支，与本改动无关，必须继续工作。

4. 清理 `/data/cc-switch/cc-switch.env` 里的 `LINGZHI_ACCESS_TOKEN`（已失效且无人消费）。**注意该文件用 `export VAR=value` 格式**，逐行 grep 时容易漏掉 `export ` 前缀。`LINGZHI_API_KEY` 必须保留——推理转发要用。

5. 在 `/quota` 的响应或文档里留一条指引，说明灵智额度请查 quota-api `:8101/api/quotas`，避免后来者以为功能丢失。

## Do not change

- **不改任何推理转发逻辑**。`LINGZHI_API_KEY` 与 lingzhi provider 的 `settings_config` 路由部分必须原样保留——灵智仍是主力推理上游（日均数千请求）。
- **不动 codex OAuth 计费单元**。
- **不改 `proxy_request_logs`、`usage_daily_rollups`**。
- **不删 `providers` 里的 lingzhi 行**——只关 `usage_script`。

## 操作注意

生产库 `/data/cc-switch/cc-switch.db` 约 **77GB**，proxy 持写锁常驻。migration 只改一行 `meta`，与库体积无关。开发测试用临时库；若需在生产库验证，先停服务并备份。只读查询用 `sqlite3 -readonly 'file:/data/cc-switch/cc-switch.db?immutable=1'`。

## Verification

- 单元测试：lingzhi provider 在 `usage_script.enabled=false` 时 `billing_unit()` 返回 `None`
- 单元测试：codex OAuth provider 的 `billing_unit()` 行为不变
- migration 幂等：连续执行两次不报错、结果一致
- 跑完整 Rust 测试套件
- 部署后集成验证：
  - `curl -s --noproxy '*' 127.0.0.1:15721/quota | jq '[.billing_units[].members[]]'` **不含** `lingzhi`
  - 同一响应中 codex OAuth 单元仍在且数据正常
  - `curl -s --noproxy '*' 127.0.0.1:15721/health` 仍 200
  - 灵智推理仍正常：`journalctl --user -u cc-switch-proxy --since "5 min ago" | grep -c "lingzhi.agibot.com/v1"` > 0
  - `grep -c LINGZHI_ACCESS_TOKEN /data/cc-switch/cc-switch.env` 为 0，`LINGZHI_API_KEY` 仍在
- 保持 worktree 干净，`git diff --check` 干净

## 参考

- 根因实测：`../findings-verified.md` §3.1（含同 session 内的自我修正记录）
- 退役决策理由：`../spec.md` §4 D-6
- 派发索引与共享上下文：`./README.md`
