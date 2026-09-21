#!/usr/bin/env bash
# ---------------------------------------------------------------------------
# hook CLI 双版本差分自检：同一输入喂给 JS hook 与 Rust hook，输出必须一致。
#
# 为什么需要它：两版共用同一份 renderer / 同一份 hook 配置格式 / 同一个
# `%TEMP%/pomodoro-hook-cache` 会话缓存，但 **hook 的协议适配和写出的 JSON 是两套代码**。
# 这是双版本方案里最容易出错的地方 —— 只测一边永远发现不了"两边答得不一样"。
#
# 做法：真起 Rust 版 GUI（网关跑起来），然后对每一种输入同时跑：
#     node bin/pomodoro-hook.js <args>
#     target/debug/pomodoro-hook.exe <args>
# 比对 stdout。两处**本来就该不同**的东西会被归一化掉：
#   · `"command"` 里的 hook 路径（Rust 直接跑 exe，JS 用 node 跑脚本）
#   · `sessions` 的 `since` 时间格式（Rust 用 ISO-8601 UTC，见迁移计划「有意保留的差异」）
# 其余一个字符都不能差。
#
# 覆盖：sessions / codex-notify / 8 宿主的 `install --print`（事件+matcher+timeout 结构）
#      / OpenCode 三条子命令的 stdout 契约 / 各宿主真实 stdin 形状（退化路径）
#      / 各宿主**决策输出**：预置缓存作答（第 3b 节）+ 假网关喂决策（第 3c 节）。
#
# ⚠ 数据隔离（缺一个就误判成"差异"）：
#   TEMP/TMP       → 专用缓存目录（且不污染真实 %TEMP%）
#   USERPROFILE    → 专用 home（install 会合并 ~/.claude 等，**别碰用户真配置**）
#   POMODORO_USER_DATA / POMODORO_GATEWAY_FILE → 专用目录
#   环境变量一律用**反斜杠 Windows 形式**：路径会打进 install 的输出，正斜杠会造成
#   `home\.claude` vs `home\.claude` 这类伪差异。
#
# ⚠ 前置：进程表必须干净（残留实例占住单实例锁 + WebView2 用户数据目录，
#   会让后续每次启动都失败，且失败长相不同 —— 自我延续的陷阱）。
#
# 用法（Git Bash）：
#   bash scripts/check-hook-parity.sh
# 环境变量：
#   NODE=...            指定 node（默认 PATH 上的 node）
#   POMODORO_GUI=...    指定 GUI 可执行文件（默认 target/debug/pomodoro.exe）
#   POMODORO_HOOK=...   指定 hook CLI（默认 target/debug/pomodoro-hook.exe）
#   POMODORO_TIMEOUT_S  等待秒数（默认脚本内设 5，别让它真的等 1 小时）
# ---------------------------------------------------------------------------
set -u

REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$REPO" || exit 2

NODE="${NODE:-node}"
GUI="${POMODORO_GUI:-$REPO/target/debug/pomodoro.exe}"
RUST="${POMODORO_HOOK:-$REPO/target/debug/pomodoro-hook.exe}"
JS="bin/pomodoro-hook.js"

command -v "$NODE" >/dev/null 2>&1 || { echo "找不到 node（可用 NODE=/path/to/node.exe 指定）"; exit 2; }
[ -f "$GUI" ]  || { echo "找不到 GUI：$GUI（先 cargo build --workspace）"; exit 2; }
[ -f "$RUST" ] || { echo "找不到 hook CLI：$RUST（先 cargo build --workspace）"; exit 2; }

# ---- 前置：进程表干净 ----
if command -v tasklist >/dev/null 2>&1; then
  if tasklist //FI "IMAGENAME eq pomodoro.exe" 2>/dev/null | grep -qi 'pomodoro.exe'; then
    echo "⚠ 检测到残留的 pomodoro.exe —— 先清掉再跑（PowerShell: Stop-Process -Id <pid> -Force）"
    exit 2
  fi
fi

# ---- 隔离目录：每次用全新路径 ----
# 不用 `rm -rf` 复用同名目录：本仓库的批量删除保护（≥50 项/turn）会拦下删除，
# 上一轮的 home/cache 残留会让 `install --print` 的合并结果凭空多出条目。
ROOT="$(cygpath -u "$(cygpath -w "$TEMP")")/pomodoro-hook-parity-$$"
WIN_ROOT="$(cygpath -w "$ROOT")"
export TEMP="$WIN_ROOT\\cache"; export TMP="$WIN_ROOT\\cache"
export USERPROFILE="$WIN_ROOT\\home"
export POMODORO_USER_DATA="$WIN_ROOT\\ud"
export POMODORO_GATEWAY_FILE="$WIN_ROOT\\gateway.json"
export POMODORO_TIMEOUT_S="${POMODORO_TIMEOUT_S:-5}"
mkdir -p "$ROOT/cache/pomodoro-hook-cache" "$ROOT/home" "$ROOT/ud"

GUI_PID=""
FAKE_PID=""
cleanup() {
  [ -n "$FAKE_PID" ] && kill "$FAKE_PID" 2>/dev/null
  [ -n "$GUI_PID" ] && kill "$GUI_PID" 2>/dev/null
  wait "$GUI_PID" 2>/dev/null
}
trap cleanup EXIT

pass=0; fail=0
ok()  { pass=$((pass+1)); printf '  PASS  %s\n' "$1"; }
bad() { fail=$((fail+1)); printf '  FAIL  %s\n' "$1"; }
note(){ printf '  ..    %s\n' "$1"; }

# ---- 预置两条 session：不预置的话两边都返回 []，「一致」是假的 ----
NOW=$(( $(date +%s) * 1000 ))
printf '{"source":"zcode","cwd":"E:\\\\proj","sessionId":"seed-a","at":%s,"task":"播种的会话"}' "$NOW" \
  > "$ROOT/cache/pomodoro-hook-cache/session-1111111111111111.json"
printf '{"source":"codex","sessionId":"seed-b","at":%s,"lastTool":"Bash"}' "$NOW" \
  > "$ROOT/cache/pomodoro-hook-cache/session-2222222222222222.json"

# ---- 起 GUI，等网关发现文件 ----
"$GUI" >/dev/null 2>&1 &
GUI_PID=$!
for _ in $(seq 1 60); do [ -f "$ROOT/gateway.json" ] && break; sleep 0.5; done
if [ ! -f "$ROOT/gateway.json" ]; then echo "⚠ 网关未起来（GUI 启动失败？）"; exit 2; fi
sleep 1
printf '网关: %s\n\n' "$(tr -d '\n' < "$ROOT/gateway.json" | head -c 100)"

norm() {
  sed -E \
    -e 's/^([[:space:]]*)"command": ".*"(,?)$/\1"command": "<HOOK>"\2/' \
    -e 's/"since": "[^"]*"/"since": "<TS>"/'
}

diffcase() {
  local name="$1" stdin="$2"; shift 2
  local r j rd jd
  r=$(echo "$stdin" | "$RUST" "$@" 2>/dev/null)
  j=$(echo "$stdin" | "$NODE" "$JS" "$@" 2>/dev/null)
  rd=$(printf '%s' "$r" | norm); jd=$(printf '%s' "$j" | norm)
  if [ "$rd" = "$jd" ]; then ok "$name"
  else
    bad "$name"
    diff <(printf '%s\n' "$rd") <(printf '%s\n' "$jd") | head -12 | sed 's/^/        /'
  fi
}

echo "== 1. 差分：结构与文案必须一致（hook 路径 / since 已归一化）=="
diffcase "sessions（含 2 条预置）" '{}' sessions
diffcase "codex-notify"           '{"type":"agent-turn-complete","thread-id":"t1"}' codex-notify
for a in claude zcode cursor codex vscode trae qwen opencode all; do
  diffcase "install --print $a"   '{}' install --print --agent "$a"
done
echo "  -- OpenCode 插件契约（stdout 直接被插件读）--"
diffcase "opencode-permission"    '{"permission":{"type":"bash","pattern":"npm test"},"sessionID":"s1"}' opencode-permission
diffcase "opencode-question"      '{"questions":[{"question":"Q?","options":[{"label":"Y"}]}],"sessionID":"s1","requestID":"r1"}' opencode-question
diffcase "opencode-event"         '{"event":"session.idle","properties":{"sessionID":"s1"}}' opencode-event

echo
echo "== 2. 已知外观差异：status 的整数被打印成 x.0（只列出，不计失败）=="
r=$(echo '{}' | "$RUST" status 2>/dev/null)
j=$(echo '{}' | "$NODE" "$JS" status 2>/dev/null)
d=$(diff <(printf '%s\n' "$r") <(printf '%s\n' "$j") | grep -c '^[<>]' || true)
if [ "$d" -eq 0 ]; then note "status 也完全一致"
else
  note "status 有 $d 行差异（应全部是 1500000.0 vs 1500000 这类）:"
  diff <(printf '%s\n' "$r") <(printf '%s\n' "$j") | head -8 | sed 's/^/        /'
fi

echo
echo "== 3. Rust hook：各宿主协议（真实 stdin 形状，必须 exit 0）=="
proto() {
  local name="$1" stdin="$2"; shift 2
  local out code
  out=$(echo "$stdin" | "$RUST" "$@" 2>/dev/null); code=$?
  if [ $code -eq 0 ]; then ok "$name  (out=${out:0:100})"
  else bad "$name  (exit $code, out=$out)"; fi
}
proto "claude PostToolUse"        '{"hook_event_name":"PostToolUse","tool_name":"Bash","tool_input":{"command":"npm test"},"session_id":"s1","cwd":"E:\\proj"}'
proto "claude PreToolUse(ask)"    '{"hook_event_name":"PreToolUse","tool_name":"AskUserQuestion","tool_input":{"questions":[{"question":"选哪个？","header":"方案","options":[{"label":"A"},{"label":"B"}]}]},"session_id":"s1"}'
proto "claude PermissionRequest"  '{"hook_event_name":"PermissionRequest","tool_name":"Bash","tool_input":{"command":"rm -rf x"},"session_id":"s1"}'
proto "claude Notification"       '{"hook_event_name":"Notification","message":"等待输入","session_id":"s1"}'
proto "claude Stop"               '{"hook_event_name":"Stop","session_id":"s1"}'
proto "claude SessionStart"       '{"hook_event_name":"SessionStart","session_id":"s1","source":"startup"}'
proto "zcode PermissionRequest"   '{"hook_event_name":"PermissionRequest","tool_name":"Bash","tool_input":{"command":"rm -rf x"},"session_id":"s1"}'
proto "vscode askQuestions"       '{"hook_event_name":"PreToolUse","tool_name":"vscode/askQuestions","tool_input":{"questions":[{"question":"Q?","options":[{"label":"Y"}]}]},"session_id":"s1"}'
proto "trae AskUserQuestion"      '{"hook_event_name":"PreToolUse","tool_name":"AskUserQuestion","tool_input":{"questions":[{"question":"Q?","options":[{"label":"Y"}]}]},"session_id":"s1"}'
proto "qwen askQuestions"         '{"hook_event_name":"PreToolUse","tool_name":"askQuestions","tool_input":{"questions":[{"question":"Q?"}]},"session_id":"s1"}'
proto "cursor afterFileEdit"      '{"hook_event_name":"afterFileEdit","file_path":"E:\\a.ts","session_id":"s1"}'
proto "cursor preToolUse(ask)"    '{"hook_event_name":"preToolUse","tool_name":"AskUserQuestion","tool_input":{"questions":[{"question":"Q?","options":[{"label":"Y"}]}]},"session_id":"s1"}'

echo
echo "== 3b. hook 决策输出：预置缓存作答，逐宿主逐动作差分 =="
# 为什么需要：第 3 节只喂了**无人作答**的输入，于是两边都走「不输出、交回宿主原生询问」，
# 恰好把 ancli.rs 里最宿主专属的那段（提交/拒绝 → permissionDecision / decision.behavior /
# updatedInput / vscode 的 deny 模式）**整个跳过了**。而这段正是最容易漂移的地方。
# 手法：handleAsk 命中缓存就直接返回（不发 HTTP、不弹窗），所以只要把缓存预置成
# 「用户已作答」，就能在**无人值守**下拿到那段宿主专属 JSON 并逐字节比对。
# 缓存文件：<TEMP>/pomodoro-hook-cache/<sha1(缓存 id)[:20]>.json，内容 {"at":ms,"result":{...}}
# 缓存 id 取 payload 的 `tool_use_id`（有它就优先用它，与 tool_input 无关，便于脚本构造）。
CACHE_DIR="$ROOT/cache/pomodoro-hook-cache"
seed_answer() { # $1=缓存 id  $2=result JSON
  local key; key=$(printf '%s' "$1" | sha1sum | cut -c1-20)
  printf '{"at":%s,"result":%s}' "$(( $(date +%s) * 1000 ))" "$2" > "$CACHE_DIR/$key.json"
}
# ⚠ $ASK_EXPECT 防「静默假通过」：缓存没命中时两边都会退化成空输出，diff 照样相等，
# 看着 PASS 其实什么都没验到。所以默认要求「必须有决策输出」。
ASK_EXPECT=json
askcase() { # $1=用例名  $2=缓存 id  $3=payload  $4=result JSON
  seed_answer "$2" "$4"
  local r j rd jd
  r=$(printf '%s' "$3" | "$RUST" 2>/dev/null)
  j=$(printf '%s' "$3" | "$NODE" "$JS" 2>/dev/null)
  if [ "$ASK_EXPECT" = json ] && { [ -z "$r" ] || [ -z "$j" ]; }; then
    bad "$1 —— 期望有决策输出，实际 rust=「$r」js=「$j」（缓存未命中？）"; return
  fi
  if [ "$ASK_EXPECT" = empty ] && { [ -n "$r" ] || [ -n "$j" ]; }; then
    bad "$1 —— 期望静默，实际 rust=「$r」js=「$j」"; return
  fi
  rd=$(printf '%s' "$r" | norm); jd=$(printf '%s' "$j" | norm)
  if [ "$rd" = "$jd" ]; then ok "$1"
  else
    bad "$1"
    diff <(printf '%s\n' "$rd") <(printf '%s\n' "$jd") | head -12 | sed 's/^/        /'
  fi
}
P1_ASK='{"source":"claude","hook_event_name":"PreToolUse","tool_name":"AskUserQuestion","tool_use_id":"p1","tool_input":{"questions":[{"question":"选哪个？","header":"方案","options":[{"label":"A"},{"label":"B"}]}]},"session_id":"s1"}'
askcase "claude PreToolUse(ask) 提交→allow+updatedInput" p1 \
  "$P1_ASK" \
  '{"decidedBy":"user","action":"submit","answers":{"q0":["A"]}}'
askcase "claude PermissionRequest(ask) 提交→decision.allow" p2 \
  '{"source":"claude","hook_event_name":"PermissionRequest","tool_name":"AskUserQuestion","tool_use_id":"p2","tool_input":{"questions":[{"question":"选哪个？","options":[{"label":"A"},{"label":"B"}]}]},"session_id":"s1"}' \
  '{"decidedBy":"user","action":"submit","answers":{"q0":["B"]}}'
askcase "claude PreToolUse(ask) 拒绝(带文本)→deny" p3 \
  '{"source":"claude","hook_event_name":"PreToolUse","tool_name":"AskUserQuestion","tool_use_id":"p3","tool_input":{"questions":[{"question":"选哪个？","options":[{"label":"A"}]}]},"session_id":"s1"}' \
  '{"decidedBy":"user","action":"deny","text":"不要这样"}'
askcase "claude PermissionRequest(ask) 取消→decision.deny" p4 \
  '{"source":"claude","hook_event_name":"PermissionRequest","tool_name":"AskUserQuestion","tool_use_id":"p4","tool_input":{"questions":[{"question":"选哪个？","options":[{"label":"A"}]}]},"session_id":"s1"}' \
  '{"decidedBy":"user","action":"cancel"}'
askcase "vscode askQuestions 提交→deny 模式（默认）" p5 \
  '{"source":"vscode","hook_event_name":"PreToolUse","tool_name":"vscode/askQuestions","tool_use_id":"p5","tool_input":{"questions":[{"question":"选哪个？","options":[{"label":"A"}]}]},"session_id":"s1"}' \
  '{"decidedBy":"user","action":"submit","answers":{"q0":["A"]}}'
# 同名两次、只差环境变量：vscode 默认走 deny，POMODORO_ASK_MODE=answers 时应改回 updatedInput
export POMODORO_ASK_MODE=answers
askcase "vscode askQuestions 提交 + ASK_MODE=answers→allow" p6 \
  '{"source":"vscode","hook_event_name":"PreToolUse","tool_name":"vscode/askQuestions","tool_use_id":"p6","tool_input":{"questions":[{"question":"选哪个？","options":[{"label":"A"}]}]},"session_id":"s1"}' \
  '{"decidedBy":"user","action":"submit","answers":{"q0":["A"]}}'
unset POMODORO_ASK_MODE
askcase "zcode ask 提交→allow" p7 \
  '{"source":"zcode","hook_event_name":"PreToolUse","tool_name":"AskUserQuestion","tool_use_id":"p7","tool_input":{"questions":[{"question":"选哪个？","options":[{"label":"A"}]}]},"session_id":"s1"}' \
  '{"decidedBy":"user","action":"submit","answers":{"q0":["A"]}}'
askcase "trae AskUserQuestion 提交→allow" p8 \
  '{"source":"trae","hook_event_name":"PreToolUse","tool_name":"AskUserQuestion","tool_use_id":"p8","tool_input":{"questions":[{"question":"选哪个？","options":[{"label":"A"}]}]},"session_id":"s1"}' \
  '{"decidedBy":"user","action":"submit","answers":{"q0":["A"]}}'
askcase "qwen askQuestions 提交→allow" p9 \
  '{"source":"qwen","hook_event_name":"PreToolUse","tool_name":"askQuestions","tool_use_id":"p9","tool_input":{"questions":[{"question":"选哪个？","options":[{"label":"A"}]}]},"session_id":"s1"}' \
  '{"decidedBy":"user","action":"submit","answers":{"q0":["A"]}}'
# 多问题 + 一个无 options（强制自定义输入）：答案映射必须一致
askcase "多问题+自定义输入 提交→answers 映射" p10 \
  '{"source":"claude","hook_event_name":"PreToolUse","tool_name":"AskUserQuestion","tool_use_id":"p10","tool_input":{"questions":[{"question":"A 选哪个？","options":[{"label":"X"},{"label":"Y"}]},{"question":"B 呢？"}]},"session_id":"s1"}' \
  '{"decidedBy":"user","action":"submit","answers":{"q0":["X"],"q1":["手写的"]}}'
# multiSelect：答案必须保持数组（不能被当成单选取 [0]）
askcase "multiSelect 提交→数组原样保留" p12 \
  '{"source":"claude","hook_event_name":"PreToolUse","tool_name":"AskUserQuestion","tool_use_id":"p12","tool_input":{"questions":[{"question":"多选？","multiSelect":true,"options":[{"label":"A"},{"label":"B"}]}]},"session_id":"s1"}' \
  '{"decidedBy":"user","action":"submit","answers":{"q0":["A","B"]}}'
# 缓存里读到的是**上次超时**留下的空 result → 必须静默（不输出决策），不能当成作答
ASK_EXPECT=empty
askcase "缓存命中空 result（上次超时）→静默" p11 \
  '{"source":"claude","hook_event_name":"PreToolUse","tool_name":"AskUserQuestion","tool_use_id":"p11","tool_input":{"questions":[{"question":"选哪个？","options":[{"label":"A"}]}]},"session_id":"s1"}' \
  '{}'
# 人眼抽样：把 p1 的真实 stdout 打出来，让人一眼看到这节确实在验「决策内容」而不只是 exit code
note "抽样（p1，Rust 实际 stdout）：$(printf '%s' "$P1_ASK" | "$RUST" 2>/dev/null)"

echo
echo "== 3c. hook 决策输出：假网关作答（permission / cursor / codex / opencode 的已作答路径）=="
# 3b 用「预置缓存」只够覆盖 ask（只有 handleAsk 读缓存）。handle_permission / cursor /
# codex / opencode 都是直接 POST /api/interaction 拿决策，而真网关的决策只能来自弹窗
# —— 无人值守时它们永远走「超时→静默」，最宿主专属的输出分支一直没被验过。
# 解法：起一个只实现 hook 会打的那几个端点的**假网关**（scripts/fake-gateway.mjs），
# 答复从文件每次重读，于是逐用例换答复、不用反复起停。纯测试侧，不碰生产代码。
REPLY_FILE="$ROOT/fake-reply.json"
printf '{}' > "$REPLY_FILE"
# ⚠ 必须给 Windows 反斜杠路径：Node 读不了 MSYS 的 /tmp/... 形式，读不到就静默退回 {}
POMODORO_FAKE_REPLY="$(cygpath -w "$REPLY_FILE")" "$NODE" scripts/fake-gateway.mjs > "$ROOT/fake-gw.log" 2>&1 &
FAKE_PID=$!
for _ in $(seq 1 40); do grep -q '^PORT=' "$ROOT/fake-gw.log" 2>/dev/null && break; sleep 0.25; done
FAKE_PORT=$(sed -n 's/^PORT=\([0-9][0-9]*\).*/\1/p' "$ROOT/fake-gw.log" | head -1)
if [ -z "${FAKE_PORT:-}" ]; then
  echo "⚠ 假网关未起来（端口没解析到）："
  sed 's/^/    /' "$ROOT/fake-gw.log"
  exit 2
fi
FAKE_TOKEN=parity-fake-token

gwcase() { # $1=用例名  $2=假网关答复 JSON  $3=payload  [额外 args...]
  local name="$1" reply="$2" payload="$3"; shift 3
  local r j rd jd
  printf '%s' "$reply" > "$REPLY_FILE"
  # ⚠ 两次运行之间必须清掉「本地始终允许」规则：若本轮走的是 allow-always，
  # 第一次跑（Rust）会落一条规则，第二次跑（JS）就提前命中规则、**根本不发 HTTP** ——
  # 输出文案随之变成「命中本地规则」，看着像两版行为不一致，其实是自造的共享状态。
  # （踩过：codex 的 allow-always 用例就这样假失败过。claude 不在 LOCAL_RULE_SOURCES，
  #  所以同一个用例在 claude 上反而是 PASS —— 假失败只在部分宿主显形。）
  rm -f "$CACHE_DIR/always-allow.json"
  r=$(printf '%s' "$payload" | POMODORO_PORT="$FAKE_PORT" POMODORO_TOKEN="$FAKE_TOKEN" "$RUST" "$@" 2>/dev/null)
  rm -f "$CACHE_DIR/always-allow.json"
  j=$(printf '%s' "$payload" | POMODORO_PORT="$FAKE_PORT" POMODORO_TOKEN="$FAKE_TOKEN" "$NODE" "$JS" "$@" 2>/dev/null)
  if [ "$GW_EXPECT" = json ] && { [ -z "$r" ] || [ -z "$j" ]; }; then
    bad "$name —— 期望有决策输出，实际 rust=「$r」js=「$j」"; return
  fi
  if [ "$GW_EXPECT" = empty ] && { [ -n "$r" ] || [ -n "$j" ]; }; then
    bad "$name —— 期望静默，实际 rust=「$r」js=「$j」"; return
  fi
  rd=$(printf '%s' "$r" | norm); jd=$(printf '%s' "$j" | norm)
  if [ "$rd" = "$jd" ]; then ok "$name"
  else
    bad "$name"
    diff <(printf '%s\n' "$rd") <(printf '%s\n' "$jd") | head -12 | sed 's/^/        /'
  fi
}
GW_EXPECT=json

ALLOW='{"kind":"permission","decidedBy":"user","action":"allow","answers":{},"text":""}'
ALLOW_TXT='{"kind":"permission","decidedBy":"user","action":"allow","answers":{},"text":"请小心"}'
ALWAYS='{"kind":"permission","decidedBy":"user","action":"allow-always","answers":{},"text":""}'
DENY_TXT='{"kind":"permission","decidedBy":"user","action":"deny","answers":{},"text":"太危险"}'
TIMEOUT_DENY='{"kind":"permission","decidedBy":"timeout","action":"deny","answers":{},"text":""}'
CLAUDE_PERM='{"source":"claude","hook_event_name":"PermissionRequest","tool_name":"Bash","tool_input":{"command":"npm test"},"session_id":"s1"}'
gwcase "claude 审批 allow→behavior.allow" "$ALLOW" "$CLAUDE_PERM"
gwcase "claude 审批 allow+备注→message 取备注" "$ALLOW_TXT" "$CLAUDE_PERM"
gwcase "claude 审批 始终允许→带 updatedPermissions" "$ALWAYS" "$CLAUDE_PERM"
gwcase "claude 审批 deny+备注→behavior.deny" "$DENY_TXT" "$CLAUDE_PERM"
# 兜底值不算用户决定：靠 decidedBy 挡住「超时默认 deny 被当成用户拒绝」
GW_EXPECT=empty
gwcase "claude 审批 decidedBy=timeout→静默（兜底不算用户决定）" "$TIMEOUT_DENY" "$CLAUDE_PERM"
GW_EXPECT=json

# Codex 遇不支持字段 fail closed → 「始终允许」绝不能带 updatedPermissions
CODEX_PERM='{"source":"codex","hook_event_name":"PermissionRequest","tool_name":"Bash","tool_input":{"command":"rm -rf x"},"turn_id":"t1"}'
gwcase "codex 审批 始终允许→不带 updatedPermissions（fail closed）" "$ALWAYS" "$CODEX_PERM"
gwcase "codex 审批 deny→behavior.deny" "$DENY_TXT" "$CODEX_PERM"

# Cursor 没有 PermissionRequest，审批走 preToolUse 族；beforeReadFile 只认 permission 一个字段
CURSOR_SH='{"source":"cursor","hook_event_name":"beforeShellExecution","command":"npm test","cwd":"E:\\proj"}'
CURSOR_READ='{"source":"cursor","hook_event_name":"beforeReadFile","file_path":"E:\\a.ts","cwd":"E:\\proj"}'
CURSOR_DENY='{"kind":"permission","decidedBy":"user","action":"deny","answers":{},"text":""}'
gwcase "cursor beforeShellExecution allow→permission=allow" "$ALLOW" "$CURSOR_SH"
gwcase "cursor beforeShellExecution deny→permission=deny + 双 message" "$CURSOR_DENY" "$CURSOR_SH"
gwcase "cursor beforeReadFile allow→只回 permission（多字段会被判非法）" "$ALLOW" "$CURSOR_READ"
gwcase "cursor beforeReadFile deny→只回 permission" "$CURSOR_DENY" "$CURSOR_READ"

# OpenCode：协议自带 ask，所以「没拿到决策」也要**明确**回 status=ask（不能假装放行）
gwcase "opencode-permission allow→status=allow" "$ALLOW" '{"permission":{"type":"bash","pattern":"npm test"},"sessionID":"s1"}'
gwcase "opencode-permission deny→status=deny" "$DENY_TXT" '{"permission":{"type":"bash","pattern":"npm test"},"sessionID":"s1"}'
gwcase "opencode-permission 未决策→status=ask" '{"decidedBy":"dismissed","action":null,"answers":{},"text":""}' '{"permission":{"type":"bash","pattern":"npm test"},"sessionID":"s1"}'
gwcase "opencode-question submit→answers 为 string[][]" \
  '{"decidedBy":"user","action":"submit","answers":{"q0":["A"]}}' \
  '{"questions":[{"question":"Q?","options":[{"label":"A"}]}],"sessionID":"s1","requestID":"r1"}'
gwcase "opencode-question 取消→reject" \
  '{"decidedBy":"user","action":"cancel","answers":{},"text":""}' \
  '{"questions":[{"question":"Q?","options":[{"label":"A"}]}],"sessionID":"s1","requestID":"r1"}'
# cursor 的提问走 preToolUse（它没有 PermissionRequest）：答复提交 → permission=allow + updated_input.answers
gwcase "cursor preToolUse 提问 提交→permission=allow+updated_input" \
  '{"decidedBy":"user","action":"submit","answers":{"q0":["A"]}}' \
  '{"source":"cursor","hook_event_name":"preToolUse","tool_name":"AskUserQuestion","tool_input":{"questions":[{"question":"选哪个？","options":[{"label":"A"}]}]},"conversation_id":"c1"}'
# cursor 的非决策事件：这一类「有输出」和「必须静默」的边界最容易漂移
gwcase "cursor beforeSubmitPrompt→{continue:true}" "$ALLOW" \
  '{"source":"cursor","hook_event_name":"beforeSubmitPrompt","prompt":"hi","conversation_id":"c1"}'
gwcase "cursor stop→{}（不回决策、不触发 followup）" "$ALLOW" \
  '{"source":"cursor","hook_event_name":"stop","conversation_id":"c1"}'
GW_EXPECT=empty
gwcase "codex PreToolUse→静默（审批只走 PermissionRequest）" "$ALLOW" \
  '{"source":"codex","hook_event_name":"PreToolUse","tool_name":"Bash","tool_input":{"command":"ls"},"turn_id":"t1"}'
GW_EXPECT=json

# 本地「始终允许」规则命中：必须直接放行、**不打扰网关**。
# 手法：预置规则 + 把假网关答复设成 deny —— 若真发了 HTTP，输出就会变成 deny。
printf '%s' "$DENY_TXT" > "$REPLY_FILE"
seed_local_rule() {
  printf '[{"source":"codex","tool":"Bash","rule":"rm -rf x","at":%s}]' "$(( $(date +%s) * 1000 ))" > "$CACHE_DIR/always-allow.json"
}
seed_local_rule
r=$(printf '%s' "$CODEX_PERM" | POMODORO_PORT="$FAKE_PORT" POMODORO_TOKEN="$FAKE_TOKEN" "$RUST" 2>/dev/null)
seed_local_rule
j=$(printf '%s' "$CODEX_PERM" | POMODORO_PORT="$FAKE_PORT" POMODORO_TOKEN="$FAKE_TOKEN" "$NODE" "$JS" 2>/dev/null)
rm -f "$CACHE_DIR/always-allow.json"
if [ "$r" = "$j" ] && printf '%s' "$r" | grep -q '命中本地'; then
  ok "本地「始终允许」规则命中→直接放行（答复设成 deny 仍回 allow ⇒ 没发 HTTP）"
else
  bad "本地规则命中：rust=「$r」js=「$j」（两边都该是「命中本地…allow」）"
fi
kill "$FAKE_PID" 2>/dev/null; FAKE_PID=""

echo
echo "== 4. install 产物：必须是 .exe、且不含 node =="
printed=$("$RUST" install --print --agent claude </dev/null 2>/dev/null)
echo "$printed" | grep -q 'pomodoro-hook.exe' && ok "含 pomodoro-hook.exe" || bad "缺 pomodoro-hook.exe"
echo "$printed" | grep -q '"node '           && bad "仍含 node 前缀"    || ok "无 node 前缀"
case "$printed" in *"$WIN_ROOT"*) ok "install 落在隔离 home（没碰真配置）" ;;
  *) note "隔离 home 未出现（检查 USERPROFILE 是否生效）" ;; esac

echo
echo "== 4b. install 未知宿主：必须非零退出（前端据此判定失败）=="
"$RUST" install --agent nosuchhost </dev/null >/dev/null 2>&1; rc_r=$?
"$NODE" "$JS" install --agent nosuchhost </dev/null >/dev/null 2>&1; rc_j=$?
if [ "$rc_r" -ne 0 ] && [ "$rc_j" -ne 0 ]; then
  ok "未知宿主都非零退出（rust=$rc_r js=$rc_j）"
else
  bad "未知宿主退出码 rust=$rc_r js=$rc_j（两边都要非零）"
fi

echo
echo "== 5. install（真写盘，隔离 home）：8 宿主 + all 都该 exit 0 =="
for a in claude zcode vscode trae cursor codex qwen opencode all; do
  out=$("$RUST" install --agent "$a" </dev/null 2>&1); code=$?
  if [ $code -eq 0 ]; then ok "install $a (exit 0)"
  else bad "install $a (exit $code)"; printf '%s\n' "$out" | head -3 | sed 's/^/        /'; fi
done
echo "  隔离 home 下写出的配置文件："
find "$ROOT/home" -type f \( -name '*.json' -o -name '*.toml' -o -name '*.ts' \) 2>/dev/null \
  | grep -v EBWebView | sed "s|$ROOT/home|<home>|" | sort | sed 's/^/    /'

echo
echo "== 5b. codex config.toml 与失败路径（隔离 home，退出码 / 落盘内容比对）=="
# notify 是比 hooks 更老的通道，用户很可能已经指向别的工具 → 默认绝不能覆盖。
# 这几条都在「隔离 home」里做，跑完只动 $ROOT/home，不碰真实 ~/.codex。
CODEX_TOML="$ROOT/home/.codex/config.toml"
mkdir -p "$ROOT/home/.codex"
NOTIFY_ORIGIN='notify = ["some-other-tool.exe", "turn-ended"]'
seed_codex_toml() { printf 'model = "gpt-5.3-codex"\n%s\n' "$NOTIFY_ORIGIN" > "$CODEX_TOML"; }

seed_codex_toml
"$RUST" install --agent codex </dev/null >/dev/null 2>&1
kept_r=$(grep -c 'some-other-tool.exe' "$CODEX_TOML" || true)
seed_codex_toml
"$NODE" "$JS" install --agent codex </dev/null >/dev/null 2>&1
kept_j=$(grep -c 'some-other-tool.exe' "$CODEX_TOML" || true)
if [ "$kept_r" -ge 1 ] && [ "$kept_j" -ge 1 ]; then
  ok "默认不动 config.toml 的 notify（既有通知工具没被覆盖）"
else
  bad "notify 被覆盖了（rust 残留 $kept_r 处 / js 残留 $kept_j 处）"
fi

# --with-notify 才改写，且要留备份（用户的 notify 指向丢了会很烦）
seed_codex_toml; rm -f "$CODEX_TOML.pomodoro.bak"
"$RUST" install --agent codex --with-notify </dev/null >/dev/null 2>&1
wr=$(grep -c 'pomodoro-hook' "$CODEX_TOML" || true); br=$([ -f "$CODEX_TOML.pomodoro.bak" ] && echo yes || echo no)
seed_codex_toml; rm -f "$CODEX_TOML.pomodoro.bak"
"$NODE" "$JS" install --agent codex --with-notify </dev/null >/dev/null 2>&1
wj=$(grep -c 'pomodoro-hook' "$CODEX_TOML" || true); bj=$([ -f "$CODEX_TOML.pomodoro.bak" ] && echo yes || echo no)
if [ "$wr" -ge 1 ] && [ "$wj" -ge 1 ] && [ "$br" = yes ] && [ "$bj" = yes ]; then
  ok "--with-notify 才改写 notify（两版都留 .pomodoro.bak 备份）"
else
  bad "--with-notify：rust(改写=$wr 备份=$br) js(改写=$wj 备份=$bj)"
fi

# hooks 被关掉时必须明确告警 —— 否则「装好了却不弹窗」根本查不出来
# ⚠ 这条告警走的是 **stdout**（install 的正常输出流），不是 stderr
printf '[features]\nhooks = false\n' > "$CODEX_TOML"
warn_r=$("$RUST" install --agent codex </dev/null 2>/dev/null)
printf '[features]\nhooks = false\n' > "$CODEX_TOML"
warn_j=$("$NODE" "$JS" install --agent codex </dev/null 2>/dev/null)
if printf '%s' "$warn_r" | grep -q 'hooks 被关了' && printf '%s' "$warn_j" | grep -q 'hooks 被关了'; then
  ok "config.toml 里 hooks 被关掉→安装时明确告警（两版都告警）"
else
  bad "hooks 关闭告警缺失：rust=「$warn_r」js=「$warn_j」"
fi

# 写盘失败也要非零退出：把 ~/.trae-cn 做成**文件**，mkdir 必然失败
BAD_HOME="$ROOT/badhome"
mkdir -p "$BAD_HOME/.claude"
printf '{}' > "$BAD_HOME/.claude/settings.json"
printf 'not a directory' > "$BAD_HOME/.trae-cn"
BAD_HOME_W="$(cygpath -w "$BAD_HOME")"
rc_r=0; rc_j=0
USERPROFILE="$BAD_HOME_W" HOME="$BAD_HOME_W" "$RUST" install --agent trae </dev/null >/dev/null 2>&1 || rc_r=$?
USERPROFILE="$BAD_HOME_W" HOME="$BAD_HOME_W" "$NODE" "$JS" install --agent trae </dev/null >/dev/null 2>&1 || rc_j=$?
if [ "$rc_r" -ne 0 ] && [ "$rc_j" -ne 0 ]; then
  ok "写盘失败→非零退出（rust=$rc_r js=$rc_j）"
else
  bad "写盘失败退出码 rust=$rc_r js=$rc_j（两边都要非零）"
fi

echo
echo "== 6. install --clean：清掉指向「另一个版本 hook」的旧条目 =="
mkdir -p "$ROOT/home/.claude"
printf '{ "hooks": { "Stop": [\n  { "hooks": [{ "type": "command", "command": "node \\"C:\\\\other\\\\hook\\\\pomodoro-hook.js\\"" }] }\n] } }\n' \
  > "$ROOT/home/.claude/settings.json"
"$RUST" install --agent claude --clean </dev/null >/dev/null 2>&1
if grep -q 'C:\\\\other' "$ROOT/home/.claude/settings.json"; then
  bad "--clean 没清掉旧条目"
  grep -o 'C:\\\\other[^"]*' "$ROOT/home/.claude/settings.json" | head -2 | sed 's/^/        /'
else
  ok "--clean 清掉了旧条目"
fi

echo
echo "==== 合计: $pass passed, $fail failed ===="
exit $([ $fail -eq 0 ] && echo 0 || echo 1)
