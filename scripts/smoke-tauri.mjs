#!/usr/bin/env node
'use strict';

// ---------------------------------------------------------------------------
// Tauri 版 GUI 冒烟：真起窗口，验证「渲染层 → bridge.js → invoke → Rust」链路是通的
//
// 判定依据是 Rust 侧打的 stderr（见 src-tauri/src/smoke.rs，需 POMODORO_SMOKE=1）：
//   [smoke] 自检模式已启用
//   [smoke] step: setup 进入 / 主窗口已建 / 托盘已建 / setup 返回
//   [smoke] invoke ok: tray_update ...
//   [smoke] invoke ok: pending_get_held
//   [smoke] handshake complete      ← 两个启动期 invoke 都到了，进程随即退出
//
// 为什么这么验，而不是进 webview 里查 DOM：
//
// 「进程没崩 + 窗口起来了 + 托盘有图标」**证明不了**链路通 —— 渲染层的报错只进
// webview 的 console，进程 stderr 里一个字都没有。bridge.js 漏一个方法就是一句
// TypeError，界面整个死掉，而进程照活、托盘照在。
//
// ## 曾经尝试过、已放弃的方案：CDP 进 webview
//
// 本来是走 `WEBVIEW2_ADDITIONAL_BROWSER_ARGUMENTS=--remote-debugging-port=N`，
// 用 CDP 直接读 DOM / 抓 console。放弃原因（都实测过）：
//   · WebView2 的 Browser 进程按用户数据目录共享，被强杀的实例会留下孤儿
//     msedgewebview2.exe，新实例复用它 → 自定义协议（tauri.localhost / ipc.localhost）
//     全都不响应，**IPC 整个不通，但进程活着、窗口也画得出来**；
//     `/json/list` 还会报出上一次的陈旧 target（about:blank），看着特别像"bridge 没注入"；
//   · 就算端口是对的，实测 t+3s 能看到应用页面，t+6s 起 DevTools 的 HTTP 端点
//     整个不再响应（清一色 TimeoutError）—— 断言还没跑完端点就死了。
//
// ## ⚠ 本脚本最容易踩的坑：残留实例（不是 WebView2 的锅）
//
// 第一版把"随机挂死"归咎于 WebView2 运行时退化 —— **错了**。真凶是**残留的应用实例**：
//
//   一旦有一个实例卡住没退，它会同时占住「单实例锁」和「WebView2 用户数据目录」，
//   于是后续每一次启动都必然失败，而且**失败长相与最初成因完全不同**：
//     · 被单实例锁拦下 → 在 setup() 之前就 exit 0，stderr 里只有那一行"自检模式已启用"
//     · 绕过去了但用户数据目录被占 → `CreateCoreWebView2EnvironmentWithOptions` 堵住，
//       表现为 `WebviewWindowBuilder::build()` 永久挂死（埋点停在 `step: setup 进入`）
//   自我延续：初次偶发一次挂起之后，后面全是它的回声。实测通过率会从 100% 掉到 ~10%，
//   看着特别像"代码坏了"，实际只是那一个僵尸没清掉。
//
//   为什么难发现：`taskkill //F //IM pomodoro.exe //T` **从 Git Bash 里发经常静默失败**，
//   `kill -9` 又依赖 bash 的 job pid。要用 PowerShell 的 `Stop-Process -Id <pid> -Force`。
//   ⇒ 所以下面有**前置检查**：开跑前先确认进程表干净，不干净就直接 exit 2 报错，
//     绝不产出误导性的"失败"。想自动清掉设 `TAURI_SMOKE_KILL_EXISTING=1`。
//
//   修完之后实测：干净状态连跑 22 次（10 + 12），**22/22 全通过**。
//
// 因此本脚本默认**重试 3 次**，并把每次失败**分类**打印：
//   build-hang        卡在 build()（多半是用户数据目录被残留实例占着）
//   page-not-loaded   窗口建出来了，但渲染层的 invoke 一直没到
//   single-instance   进程提前退出且没进 setup —— 已有一个实例在跑
//   timeout           以上都不是
// 只要**有一次通过**就算通过（并提示用了几次）。三次全挂则失败。
// 想让它更严格：TAURI_SMOKE_ATTEMPTS=1。
//
// 用法：
//   node scripts/smoke-tauri.mjs                  # 默认 target/debug/pomodoro.exe
//   node scripts/smoke-tauri.mjs <exe路径>
//
// 环境变量：
//   TAURI_SMOKE_ATTEMPTS=N     重试次数（默认 3）
//   TAURI_SMOKE_TIMEOUT_MS=N   单次超时（默认 25000）
//   TAURI_SMOKE_FRESH_PROFILE=1  额外把 WEBVIEW2_USER_DATA_FOLDER 指向新建临时目录
//                                （排障用；实测 Tauri 会用自己的目录，此变量通常不生效）
//
// ⚠ 单实例锁：本应用注册了 tauri-plugin-single-instance。**已经有一个实例在跑时，
//   新实例会立刻退出**，表现为"没等到握手就退了 / 退出码 0"。先从托盘退掉再跑。
// ---------------------------------------------------------------------------

import { spawn, execFileSync } from 'node:child_process';
import { existsSync, mkdtempSync, rmSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { basename, dirname, join, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';

const ROOT = resolve(dirname(fileURLToPath(import.meta.url)), '..');
const EXE = resolve(process.argv[2] || join(ROOT, 'target', 'debug', 'pomodoro.exe'));
const TIMEOUT_MS = Number(process.env.TAURI_SMOKE_TIMEOUT_MS || 25000);
const ATTEMPTS = Math.max(1, Number(process.env.TAURI_SMOKE_ATTEMPTS || 3));

const OK_TRAY = '[smoke] invoke ok: tray_update';
const OK_PENDING = '[smoke] invoke ok: pending_get_held';
const OK_DONE = '[smoke] handshake complete';
const ST_SETUP = '[smoke] step: setup 进入';
const ST_WIN = '[smoke] step: 主窗口已建';

if (!existsSync(EXE)) {
  console.error('[smoke] 找不到可执行文件\n        先 cargo build（或传 cargo tauri build 产物的路径）');
  process.exit(1);
}

// ---------------------------------------------------------------------------
// 前置检查：绝不能在"已有实例在跑"的状态下测
//
// ⚠ 这条是踩出来的。本应用注册了 tauri-plugin-single-instance，而**卡死在 build() 里的
// 实例会一直活着**（它是卡主线程，不是崩溃）。这种僵尸实例会占住单实例锁，
// 于是后面每一次启动都被判成"重复实例"、立刻 exit 0、一行 stderr 都不多打。
// 现象极具误导性：**连跑 10 次全失败，看着像代码全烂了**，实际只是那一个僵尸没清掉。
// （`taskkill //F //IM` 从 Git Bash 里发经常不生效；PowerShell 的 Stop-Process 才稳。）
//
// 所以这里在开跑前先确认干净；不干净就直接报错退出，不给出误导性的"失败"。
// ---------------------------------------------------------------------------
const EXE_NAME = basename(EXE);

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
        '        单实例锁会让新实例直接 exit 0，不产生任何握手 —— 这是 100% 的假阴性。\n' +
        '        先在托盘里退出，或设 TAURI_SMOKE_KILL_EXISTING=1 让本脚本强制清掉。\n' +
        '        （卡死在 build() 的实例也会一直活着占锁，见文件头与 docs/rust-migration-plan.md 第 10 节。）',
    );
    process.exit(2);
  }
}

console.log(`[smoke] 目标: ${EXE}`);
console.log(`[smoke] 最多尝试 ${ATTEMPTS} 次，单次超时 ${TIMEOUT_MS}ms`);

// ---------------------------------------------------------------------------
// 单次尝试
// ---------------------------------------------------------------------------

/** @returns {Promise<{ok:boolean, kind:string, lines:string[], exited:any, tail:string}>} */
function runOnce(n) {
  return new Promise((done) => {
    // 数据目录隔离 —— **两个都要**：
    //   · POMODORO_USER_DATA：Rust 侧认它（`pomodoro_core::user_data_dir()`）。
    //     M3 起 GUI 启动会把 `pomodoro-hook.exe` + OpenCode 插件释放进 `<userData>/hook/`，
    //     不隔离就会把**调试版** hook 写进用户真实目录、还会顺手覆盖那份插件。
    //   · WEBVIEW2_USER_DATA_FOLDER：另外给个干净 profile（排障开关）
    const fresh = process.env.TAURI_SMOKE_FRESH_PROFILE === '1';
    const userDataDir = mkdtempSync(join(tmpdir(), 'pomodoro-smoke-'));
    const webviewProfile = fresh ? mkdtempSync(join(tmpdir(), 'pomodoro-webview-')) : null;

    const child = spawn(EXE, [], {
      cwd: ROOT,
      env: {
        ...process.env,
        POMODORO_SMOKE: '1',
        POMODORO_USER_DATA: userDataDir,
        ...(webviewProfile ? { WEBVIEW2_USER_DATA_FOLDER: webviewProfile } : {}),
      },
      stdio: ['ignore', 'pipe', 'pipe'],
    });

    const lines = [];
    let buf = '';
    let exited = null;

    const feed = (chunk) => {
      buf += chunk;
      let i;
      while ((i = buf.indexOf('\n')) >= 0) {
        const t = buf.slice(0, i).trim();
        buf = buf.slice(i + 1);
        if (t.startsWith('[smoke]')) lines.push(t);
      }
    };
    child.stdout.on('data', (d) => feed(String(d)));
    child.stderr.on('data', (d) => feed(String(d)));
    child.on('exit', (code, signal) => { exited = { code, signal }; });
    child.on('error', (e) => { lines.push(`[smoke] spawn 失败: ${e.message}`); exited = { code: -1, signal: null }; });

    const cleanup = () => {
      try { child.kill('SIGKILL'); } catch { /* 已经没了 */ }
      for (const d of [userDataDir, webviewProfile]) {
        if (d) {
          try { rmSync(d, { recursive: true, force: true }); } catch { /* 尽力 */ }
        }
      }
    };

    const txt = () => lines.join('\n');
    const deadline = Date.now() + TIMEOUT_MS;

    const tick = () => {
      const seenAll = txt().includes(OK_TRAY) && txt().includes(OK_PENDING);
      if (txt().includes(OK_DONE) || seenAll) return finish(true);
      if (exited && !txt().includes(ST_SETUP)) return finish(false, 'single-instance');
      if (!exited && Date.now() >= deadline) return finish(false, classify());
      if (exited && Date.now() >= deadline) return finish(false, classify());
      setTimeout(tick, 250);
    };

    // 进程提前退出（比如单实例锁）时也要能尽快判定
    const watchExit = () => {
      if (!exited) return setTimeout(watchExit, 200);
      setTimeout(() => finish(false, classify()), 300);
    };

    const classify = () => {
      const t = txt();
      if (!t.includes(ST_SETUP)) return exited ? 'single-instance' : 'no-setup';
      if (!t.includes(ST_WIN)) return 'build-hang';
      if (!t.includes(OK_TRAY) || !t.includes(OK_PENDING)) return 'page-not-loaded';
      return 'timeout';
    };

    let settled = false;
    const finish = (ok, kind) => {
      if (settled) return;
      settled = true;
      cleanup();
      done({ ok, kind: ok ? 'ok' : kind, lines, exited, tail: lines.slice(-2).join(' | ') });
    };

    console.log(`\n[smoke] 第 ${n}/${ATTEMPTS} 次…`);
    tick();
    watchExit();
  });
}

// ---------------------------------------------------------------------------
// 重试循环
// ---------------------------------------------------------------------------

const HINT = {
  'build-hang':
    '卡在 WebviewWindowBuilder::build() —— 本机已知的 WebView2 环境性挂死' +
    '（同一份二进制会话初期 100% 通过过，见 docs/rust-migration-plan.md 第 10 节）。' +
    '建议先重启机器再跑；重启后仍复现才是真 bug。',
  'page-not-loaded':
    '窗口建出来了但渲染层的 invoke 没到 —— 真回归的可能性最大。' +
    '先跑 `node scripts/check-bridge-parity.mjs`，再确认 renderer/index.html 的 CSP 没挡住 IPC。',
  'single-instance':
    '进程提前退出且没进 setup —— 十有八九**已经有一个实例在跑**，单实例锁把新实例结束了。' +
    '先从托盘退掉在跑的那个再试。',
  'no-setup': '进程起来了但没进 setup，且没退出 —— 情况不明确，看下面的完整输出。',
  timeout: '单次超时。',
};

let passed = 0;
let lastFail = null;

for (let n = 1; n <= ATTEMPTS; n++) {
  const r = await runOnce(n);
  for (const l of r.lines) console.log(`  ${l}`);

  if (r.ok) {
    passed = n;
    console.log(`  ✓ 第 ${n} 次通过`);
    break;
  }

  lastFail = r;
  console.log(`  ✗ 第 ${n} 次失败：${r.kind}`);
  console.log(`    最后两行: ${r.tail || '(无 [smoke] 输出)'}`);
  if (HINT[r.kind]) console.log(`    提示: ${HINT[r.kind]}`);
  if (r.exited) console.log(`    进程退出: code=${r.exited.code} signal=${r.exited.signal}`);

  if (n < ATTEMPTS) await new Promise((res) => setTimeout(res, 1500));
}

console.log('\n--- 判定 ---');
if (passed) {
  if (passed > 1) {
    console.log(`[smoke] 通过（第 ${passed}/${ATTEMPTS} 次）`);
    console.log('       前几次失败疑似本机 WebView2 环境抖动，不是代码回归 —— 详见文件头。');
  } else {
    console.log('[smoke] 通过（一次成功）：渲染层已初始化，bridge 两个启动期 invoke 都到达了 Rust');
  }
  process.exit(0);
}

console.error(`\n[smoke] 失败：${ATTEMPTS} 次都没通过（最后一次类型：${lastFail ? lastFail.kind : '未知'}）`);
if (lastFail) {
  console.error('--- 最后一次的完整输出 ---');
  console.error(lastFail.lines.join('\n') || '(无 [smoke] 输出)');
  console.error('--- 排查顺序 ---');
  console.error('  ① 是否已有一个实例在跑（单实例锁会让新实例直接退出）');
  console.error('  ② node scripts/check-bridge-parity.mjs（bridge 与 renderer 的方法名/命令名）');
  console.error('  ③ 若是 build-hang：重启机器；仍复现则查 wry 的 WebView2 环境创建');
}
process.exit(1);
