#!/usr/bin/env node
'use strict';

// ---------------------------------------------------------------------------
// Rust 版网关冒烟：真起应用，让 Rust 自己打自己一遍 HTTP 接口
//
// 与 scripts/smoke-tauri.mjs 的区别（**不要合并**，两者的生命周期完全不同）：
//   · smoke-tauri.mjs  只验「渲染层 → bridge → invoke → Rust」链路。它设
//     `POMODORO_SMOKE=1`，而那条路在两个握手点到齐后会**立刻 app.exit(0)**
//     （见 src/smoke.rs 的 maybe_finish）—— 网关自检要跑 ~20 秒，会被它腰斩。
//   · 本脚本设 `POMODORO_GATEWAY_SMOKE=1`（**不设** POMODORO_SMOKE），跑完自己
//     收尾，由脚本把进程杀掉。
//
// 判定依据是 Rust 侧 stderr 的两行：
//   [gateway-smoke] 7 项，0 失败
//     PASS health / PASS status / ...
//   [gateway-smoke] done
// 只要「失败数 == 0」且跑完了，就算通过。任何一项 FAIL 都会原样打印出来。
//
// 覆盖的用例（见 src-tauri/src/gateway.rs 的 smoke）：
//   health / status / host 头拒绝(403) / 无 token 拒绝(401)
//   /api/event 计数 / permission 超时→deny / ask 超时→cancel / confirm 超时
//   加 POMODORO_GATEWAY_POPUP=1（本脚本的 TAURI_SMOKE_POPUP=1）再验一条全链路：
//   弹窗页 → bridge → 命令 → 网关 resolve → HTTP 侧拿到 decidedBy=user
//
// ## ⚠ 与 smoke-tauri.mjs 相同的头号坑：残留实例
//
// 残留实例会同时占住单实例锁与 WebView2 用户数据目录，让每次启动都失败，
// 且失败长相与成因完全不同（自我延续）。所以下面有一样的前置检查。
// 清进程**必须用 PowerShell `Stop-Process -Id <pid> -Force`** —— `taskkill //F //IM`
// 从 Git Bash 里发经常静默失败。
//
// ## 数据目录隔离
//
// 本脚本把 `POMODORO_USER_DATA` 指到一个临时目录。Rust 侧的 `pomodoro_core::user_data_dir()`
// 认这个变量（Electron 的 main.js 不认），于是 config.json / gateway.json 都落在
// 临时目录里，**不会碰到你日常那份 `%APPDATA%/pomodoro-fluent/gateway.json`**。
// 用 `TAURI_SMOKE_REAL_USERDATA=1` 可以关掉隔离（排障用）。
//
// 用法：
//   node scripts/smoke-gateway.mjs                    # 默认 target/debug/pomodoro.exe
//   node scripts/smoke-gateway.mjs <exe路径>
//
// 环境变量：
//   TAURI_SMOKE_POPUP=1        额外验「弹窗内真作答」那条全链路（会多弹一个窗）
//   TAURI_SMOKE_TIMEOUT_MS=N   总超时（默认 90000；加 POPUP 时自动再放宽）
//   TAURI_SMOKE_KILL_EXISTING=1  开跑前强制清掉残留实例
//   TAURI_SMOKE_REAL_USERDATA=1  不隔离用户数据目录
// ---------------------------------------------------------------------------

import { spawn, execFileSync } from 'node:child_process';
import { existsSync, mkdtempSync, rmSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { basename, dirname, join, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';

const ROOT = resolve(dirname(fileURLToPath(import.meta.url)), '..');
const EXE = resolve(process.argv[2] || join(ROOT, 'target', 'debug', 'pomodoro.exe'));
const EXE_NAME = basename(EXE);
const WITH_POPUP = process.env.TAURI_SMOKE_POPUP === '1';
const TIMEOUT_MS = Number(
  process.env.TAURI_SMOKE_TIMEOUT_MS || (WITH_POPUP ? 120000 : 90000),
);

const DONE = '[gateway-smoke] done';
// `[gateway-smoke] 7 项，0 失败`
const SUMMARY_RE = /\[gateway-smoke\]\s*(\d+)\s*项[，,]\s*(\d+)\s*失败/;

if (!existsSync(EXE)) {
  console.error('[smoke] 找不到可执行文件\n        先 cargo build（或传 cargo tauri build 产物的路径）');
  process.exit(1);
}

// ---------------------------------------------------------------------------
// 前置检查：绝不能在"已有实例在跑"的状态下测（理由见文件头）
// ---------------------------------------------------------------------------
function runningCount() {
  try {
    const out = execFileSync('tasklist', ['/fi', `IMAGENAME eq ${EXE_NAME}`, '/fo', 'csv', '/nh'], {
      encoding: 'utf8',
      windowsHide: true,
    });
    return (out.match(new RegExp(EXE_NAME.replace('.', '\\.'), 'gi')) || []).length;
  } catch {
    return 0; // tasklist 不可用时不阻断
  }
}

const alive = runningCount();
if (alive > 0) {
  if (process.env.TAURI_SMOKE_KILL_EXISTING === '1') {
    console.log(`[smoke] 检测到 ${alive} 个已在运行的 ${EXE_NAME}，按 TAURI_SMOKE_KILL_EXISTING=1 强制结束`);
    try {
      execFileSync('taskkill', ['/F', '/IM', EXE_NAME, '/T'], { stdio: 'ignore', windowsHide: true });
    } catch { /* 尽力 */ }
    await new Promise((r) => setTimeout(r, 1500));
  } else {
    console.error(
      `[smoke] 中止：检测到 ${alive} 个已在运行的 ${EXE_NAME}。\n` +
        '        单实例锁会让新实例直接 exit 0，网关根本不会起来 —— 这是 100% 的假阴性。\n' +
        '        先在托盘里退出，或设 TAURI_SMOKE_KILL_EXISTING=1 让本脚本强制清掉。',
    );
    process.exit(2);
  }
}

// ---------------------------------------------------------------------------
// 用户数据目录隔离
// ---------------------------------------------------------------------------
const isolated = process.env.TAURI_SMOKE_REAL_USERDATA !== '1';
const userDataDir = isolated ? mkdtempSync(join(tmpdir(), 'pomodoro-gw-smoke-')) : null;

console.log(`[smoke] 目标: ${EXE}`);
console.log(`[smoke] 用例: ${WITH_POPUP ? '含弹窗内作答（TAURI_SMOKE_POPUP=1）' : '不含弹窗内作答'}`);
console.log(`[smoke] 用户数据: ${userDataDir || '%APPDATA%（未隔离）'}`);
console.log(`[smoke] 总超时: ${TIMEOUT_MS}ms\n`);

const child = spawn(EXE, [], {
  cwd: ROOT,
  env: {
    ...process.env,
    // ⚠ 不要设 POMODORO_SMOKE：那条路握手完就 app.exit(0)，会把网关自检腰斩
    POMODORO_GATEWAY_SMOKE: '1',
    ...(WITH_POPUP ? { POMODORO_GATEWAY_POPUP: '1' } : {}),
    ...(userDataDir ? { POMODORO_USER_DATA: userDataDir } : {}),
  },
  stdio: ['ignore', 'pipe', 'pipe'],
});

const lines = [];
let buf = '';
let exited = null;

function feed(chunk) {
  buf += chunk;
  let i;
  while ((i = buf.indexOf('\n')) >= 0) {
    const t = buf.slice(0, i);
    buf = buf.slice(i + 1);
    if (t.trim()) lines.push(t.replace(/\r$/, ''));
  }
}

child.stdout.on('data', (d) => feed(String(d)));
child.stderr.on('data', (d) => feed(String(d)));
child.on('exit', (code, signal) => { exited = { code, signal }; });
child.on('error', (e) => { lines.push(`spawn 失败: ${e.message}`); exited = { code: -1, signal: null }; });

const joined = () => lines.join('\n');

let settled = false;
function finish(ok, note) {
  if (settled) return;
  settled = true;
  // 收尾：GUI 进程不会自己退，必须显式杀
  try { child.kill(); } catch { /* 已经没了 */ }
  setTimeout(() => {
    try { child.kill('SIGKILL'); } catch { /* 已经没了 */ }
    if (userDataDir) {
      try { rmSync(userDataDir, { recursive: true, force: true }); } catch { /* 尽力 */ }
    }
    report(ok, note);
  }, 800);
}

function report(ok, note) {
  console.log('--- 应用输出（含网关自检）---');
  for (const l of lines) if (l.includes('[gateway') || l.includes('PASS ') || l.includes('FAIL ')) {
    console.log(`  ${l}`);
  }
  console.log('--- 判定 ---');
  if (ok) {
    console.log(`[smoke] 通过：${note}`);
    process.exit(0);
  }
  console.error(`[smoke] 失败：${note}`);
  if (exited) console.error(`  进程退出: code=${exited.code} signal=${exited.signal}`);
  console.error('--- 应用完整输出 ---');
  console.error(lines.join('\n') || '(无输出)');
  console.error('--- 排查顺序 ---');
  console.error('  ① 是否已有一个实例在跑（单实例锁会让新实例直接退出）');
  console.error('  ② 网关是否真的启起来了（看有没有 [gateway] 已启动，端口 N）');
  console.error('  ③ 端口是否被别的进程占住（网关会在 20 个候选端口里找）');
  process.exit(1);
}

const deadline = Date.now() + TIMEOUT_MS;
const tick = () => {
  if (settled) return;
  const text = joined();
  if (text.includes(DONE)) {
    const m = SUMMARY_RE.exec(text);
    if (!m) return finish(false, '跑完了但没能解析出「N 项，M 失败」摘要');
    const total = Number(m[1]);
    const failed = Number(m[2]);
    if (failed !== 0) return finish(false, `${total} 项里有 ${failed} 项失败（见上面的 FAIL 行）`);
    if (total < 7) return finish(false, `只跑了 ${total} 项，疑似有用例没执行到`);
    return finish(true, `网关 ${total} 项全部通过`);
  }
  // 进程提前退出且没有 done —— 多半是被单实例锁结束了，或 setup 里就炸了
  if (exited) {
    return finish(false, exited.code === 0
      ? '进程提前退出（code=0）且没跑完自检 —— 十有八九是单实例锁把新实例结束了'
      : `进程提前退出（code=${exited.code}）且没跑完自检`);
  }
  if (Date.now() >= deadline) {
    return finish(false, `${TIMEOUT_MS}ms 内没等到 ${DONE}`);
  }
  setTimeout(tick, 250);
};

tick();
