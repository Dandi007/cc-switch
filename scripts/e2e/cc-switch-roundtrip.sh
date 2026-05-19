#!/usr/bin/env bash
# scripts/e2e/cc-switch-roundtrip.sh
# 真实 e2e 烟测：cc CLI 经 cc-switch 跑通两个用户日常 setup。
# 前置：cc-switch proxy 在跑（127.0.0.1:15721），LINGZHI_API_KEY 已配，
#       gpt provider OAuth 有效。
set -euo pipefail

DB="$HOME/.cc-switch/cc-switch.db"

assert_logs() {
  local label=$1 since=$2 min=$3 providers=$4
  local count seen
  count=$(sqlite3 "$DB" "SELECT count(*) FROM proxy_request_logs WHERE created_at >= $since AND status_code = 200;")
  seen=$(sqlite3 "$DB" "SELECT DISTINCT provider_id FROM proxy_request_logs WHERE created_at >= $since AND status_code = 200;" | sort | tr '\n' ' ')
  echo "[$label] count=$count providers=[$seen]"
  [[ "$count" -ge "$min" ]] || { echo "FAIL: $label count<$min"; exit 1; }
  for p in $providers; do
    grep -q "$p" <<< "$seen" || { echo "FAIL: $label provider $p missing"; exit 1; }
  done
  echo "PASS: $label"
}

# 加载用户的 zsh 函数
source ~/.config/agent-shell/profile.zsh

# ========== Setup 1: cc + gpt ==========
echo "--- Setup 1: cc + gpt ---"
set_claude_ccswitch_gpt
SINCE1=$(date +%s)
sleep 1

# T1.basic
echo "[T1.basic]"
claude -p "1+1 等于几？只回答数字。"

# T1.subagent (Explore subagent → fast/haiku model)
echo "[T1.subagent]"
claude -p "用 Explore subagent 帮我搜一下当前目录有几个 .md 文件，给我数字。"

assert_logs "Setup-1 cc+gpt" "$SINCE1" 2 "gpt"

# ========== Setup 2: cc + ds + gpt ==========
echo "--- Setup 2: cc + ds + gpt ---"
set_claude_ccswitch_ds_gptmini
SINCE2=$(date +%s)
sleep 1

# T2.basic (走 lingzhi/deepseek 主模型)
echo "[T2.basic]"
claude -p "1+1 等于几？只回答数字。"

# T2.subagent (主 agent: lingzhi/deepseek; Explore subagent: gpt/gpt-5.4-mini)
echo "[T2.subagent]"
claude -p "用 Explore subagent 帮我搜一下当前目录有几个 .md 文件，给我数字。"

assert_logs "Setup-2 cc+ds+gpt" "$SINCE2" 2 "lingzhi gpt"

echo "ALL PASS"