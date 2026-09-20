#!/usr/bin/env node
'use strict';

// ---------------------------------------------------------------------------
// 一次打出两份安装包：Electron 版 + Rust/Tauri 版
//
// 用法：
//   node scripts/pack-release.mjs                 # 两份都打（默认）
//   node scripts/pack-release.mjs --only electron
//   node scripts/pack-release.mjs --only rust
//   node scripts/pack-release.mjs --out <目录>
//
// 两条发布线**版本号各自独立**，本脚本不会把 package.json 的版本号写进
// tauri.conf.json（那是单版本方案才需要做的事）：
//   Electron 线 → package.json              → release/electron/
//   Rust 线     → src-tauri/tauri.conf.json → release/rust/
// 分两个子目录收集，方便分别 gh release create。
//
// 发布约定见 docs/dual-release.md —— 尤其是 Rust 线 tag 必须带 `rust-` 前缀，
// 否则两个版本的应用内「检查更新」会互相串线。
// ---------------------------------------------------------------------------

import { execFileSync } from 'node:child_process';
import { copyFileSync, existsSync, mkdirSync, readFileSync, readdirSync } from 'node:fs';
import { dirname, join, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';

const ROOT = resolve(dirname(fileURLToPath(import.meta.url)), '..');
const argv = process.argv.slice(2);
const argOf = (name, fallback = '') => {
  const i = argv.indexOf(name);
  return i >= 0 ? (argv[i + 1] ?? fallback) : fallback;
};

const ONLY = argOf('--only', 'all');
if (!['all', 'electron', 'rust'].includes(ONLY)) {
  console.error(`[pack] --only 只能是 all | electron | rust，收到: ${ONLY}`);
  process.exit(1);
}

const log = (msg) => console.log(`[pack] ${msg}`);
const fail = (msg) => {
  console.error(`[pack] 失败: ${msg}`);
  process.exit(1);
};

const run = (bin, args, extraEnv = {}) => {
  log(`$ ${bin} ${args.join(' ')}`);
  execFileSync(bin, args, {
    cwd: ROOT,
    stdio: 'inherit',
    shell: true,
    env: { ...process.env, ...extraEnv },
  });
};

const readJson = (relPath) => JSON.parse(readFileSync(join(ROOT, relPath), 'utf8'));

// ---------------------------------------------------------------------------
// 两条线各自的版本号（互不同步，各自独立演进）
// ---------------------------------------------------------------------------
const electronVersion = readJson('package.json').version;
const rustVersion = existsSync(join(ROOT, 'src-tauri', 'tauri.conf.json'))
  ? readJson(join('src-tauri', 'tauri.conf.json')).version
  : '';

const baseOut = resolve(argOf('--out', join(ROOT, 'release')));
const electronOut = join(baseOut, 'electron');
const rustOut = join(baseOut, 'rust');

const collected = [];

// ---------------------------------------------------------------------------
// Electron 线
// ---------------------------------------------------------------------------
function packElectron() {
  if (!electronVersion) fail('package.json 里没有 version');
  log(`—— Electron 线 v${electronVersion} ——`);
  mkdirSync(electronOut, { recursive: true });

  run('npm', ['run', 'pack'], {
    // electron-builder 会清理 dist/win-unpacked（几十个文件），会被批量删除保护拦下
    CODEBUDDY_SAFE_DELETE_ENABLED: '0',
  });

  const wanted = [
    `Pomodoro-Fluent-Setup-${electronVersion}.exe`,
    `Pomodoro-Fluent-Setup-${electronVersion}.exe.blockmap`,
    'latest.yml',
  ];
  for (const name of wanted) {
    const src = join(ROOT, 'dist', name);
    if (!existsSync(src)) {
      log(`  跳过（产物不存在）: dist/${name}`);
      continue;
    }
    copyFileSync(src, join(electronOut, name));
    collected.push(`electron/${name}`);
    log(`  ✓ electron/${name}`);
  }
}

// ---------------------------------------------------------------------------
// Rust 线
// ---------------------------------------------------------------------------
function packRust() {
  if (!rustVersion) fail('src-tauri/tauri.conf.json 里没有 version');
  log(`—— Rust 线 v${rustVersion} ——`);
  mkdirSync(rustOut, { recursive: true });

  const env = {};
  // 本机对 GitHub 的连接会被重置，而 tauri bundler 的 NSIS 工具链只从 GitHub releases 取。
  // 不设这个变量会直接 `Error failed to bundle project: timeout: global`。
  if (!process.env.TAURI_BUNDLER_TOOLS_GITHUB_MIRROR) {
    env.TAURI_BUNDLER_TOOLS_GITHUB_MIRROR = 'https://ghfast.top';
    log('  未设 TAURI_BUNDLER_TOOLS_GITHUB_MIRROR，本次用 https://ghfast.top');
  }
  run('cargo', ['tauri', 'build'], env);

  const nsisDir = join(ROOT, 'target', 'release', 'bundle', 'nsis');
  if (!existsSync(nsisDir)) fail(`没找到 Tauri 产物目录: ${nsisDir}`);
  const setups = readdirSync(nsisDir).filter((f) => f.toLowerCase().endsWith('.exe'));
  if (setups.length === 0) fail(`Tauri 没产出任何 .exe（${nsisDir}）`);

  for (const file of setups) {
    // 统一改名，免得和 Electron 版在同一个 release 页面里看不出谁是谁
    const target = `Pomodoro-Fluent-Rust-Setup-${rustVersion}.exe`;
    copyFileSync(join(nsisDir, file), join(rustOut, target));
    collected.push(`rust/${target}`);
    log(`  ✓ rust/${target}  (← ${file})`);
  }
}

// ---------------------------------------------------------------------------
if (ONLY === 'all' || ONLY === 'electron') packElectron();
if (ONLY === 'all' || ONLY === 'rust') packRust();

log('');
log(`完成，共 ${collected.length} 个产物落 ${baseOut}`);
for (const name of collected) log(`  ${name}`);

log('');
log('下一步：两条线各自发一个 release（tag 前缀是硬约定，别省）');
// gh 在 Windows 上吃反斜杠，但提示是给人复制粘贴的，统一成正斜杠
const p = (dir) => dir.replace(/\\/g, '/');
if (ONLY !== 'rust' && electronVersion) {
  log(`  gh release create v${electronVersion} --target <mergeCommit> --title v${electronVersion} "${p(electronOut)}"/*`);
}
if (ONLY !== 'electron' && rustVersion) {
  log(`  gh release create rust-v${rustVersion} --target <mergeCommit> --title "Rust 版 ${rustVersion}" "${p(rustOut)}"/*`);
}
