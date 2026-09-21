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
#      / OpenCode 三条子命令的 stdout 契约 / 各宿主真实 stdin 形状。
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
cleanup() {
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
echo "== 4. install 产物：必须是 .exe、且不含 node =="
printed=$("$RUST" install --print --agent claude </dev/null 2>/dev/null)
echo "$printed" | grep -q 'pomodoro-hook.exe' && ok "含 pomodoro-hook.exe" || bad "缺 pomodoro-hook.exe"
echo "$printed" | grep -q '"node '           && bad "仍含 node 前缀"    || ok "无 node 前缀"
case "$printed" in *"$WIN_ROOT"*) ok "install 落在隔离 home（没碰真配置）" ;;
  *) note "隔离 home 未出现（检查 USERPROFILE 是否生效）" ;; esac

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
