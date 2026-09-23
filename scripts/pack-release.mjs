#!/usr/bin/env node
'use strict';

// ---------------------------------------------------------------------------
// 打 Windows 安装包（Rust / Tauri 版）
//
// 用法：
//   node scripts/pack-release.mjs                  # 等价 npm run pack
//   node scripts/pack-release.mjs --out <目录>     # 换收集目录（默认 release/）
//
// > 旧 Electron 实现已冻结在 `electron-archive` 分支，本脚本不再出 Electron 包。
//
// ## 版本号：只有一个事实源
//
// **`package.json` 的 `version` 是唯一事实源**。打包装前脚本会把它同步写进这两个文件
// （都只替换版本号那一行，不动其它格式）：
//   - `src-tauri/tauri.conf.json` → 决定安装包文件名与应用版本号
//   - `Cargo.toml`（`[workspace.package]`）→ 决定 `Compiling <crate> vX.Y.Z` 与 **GUI exe 的版本资源**
// 少同步一个就会出现「安装包是 1.1.6、构建日志写 2.0.0」这种对不上的情况。
//
// ⚠ 已知无害缺口：**hook exe 的 PE 版本资源是空的**。GUI 的版本资源由 tauri-build 嵌入，
//   而 `crates/hook` 没有 build.rs，Cargo.toml 的版本号只体现在构建日志里
//   （`(Get-Item pomodoro-hook.exe).VersionInfo.FileVersion` 为空）。
//   要补得引入 winresource / embed-resource 之类的 build 依赖；纯元数据，未做。
//
// 升版本 = 只改 `package.json`（沿用既有的 `chore: bump version to X.Y.Z` 流程），
// 然后跑一次本脚本把版本号带到 `tauri.conf.json` + `Cargo.toml`。别手工去改它们。
//
// ## 产物
//
// 统一落到 `release/`（不分子目录）：
//   Pomodoro-Fluent-Rust-Setup-X.Y.Z.exe
//
// 发布约定见 docs/release.md。
// ---------------------------------------------------------------------------

import { execFileSync } from 'node:child_process';
import {
  copyFileSync,
  existsSync,
  mkdirSync,
  readFileSync,
  readdirSync,
  statSync,
  writeFileSync,
} from 'node:fs';
import { dirname, join, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';

const ROOT = resolve(dirname(fileURLToPath(import.meta.url)), '..');
const argv = process.argv.slice(2);
const argOf = (name, fallback = '') => {
  const i = argv.indexOf(name);
  return i >= 0 ? (argv[i + 1] ?? fallback) : fallback;
};

const log = (msg) => console.log(`[pack] ${msg}`);
const fail = (msg) => {
  console.error(`[pack] 失败: ${msg}`);
  process.exit(1);
};

// 旧用法直接报错退出，别静默忽略 —— 以为出了 Electron 包却没出，比直接失败更难查
if (argv.includes('--only')) {
  fail(
    '--only 已取消：Electron 实现冻结在 electron-archive 分支，本仓库只出 Rust 安装包。\n' +
      '       直接 `node scripts/pack-release.mjs` 即可。',
  );
}

const run = (bin, args, extraEnv = {}) => {
  log(`$ ${bin} ${args.join(' ')}`);
  execFileSync(bin, args, {
    cwd: ROOT,
    stdio: 'inherit',
    shell: true,
    env: { ...process.env, ...extraEnv },
  });
};

const readJson = (absPath) => JSON.parse(readFileSync(absPath, 'utf8'));
const toPosix = (p) => p.replace(/\\/g, '/');

const TAURI_CONF = 'src-tauri/tauri.conf.json';
// 版本号在 workspace 根部，见文件头注释
const CARGO_TOML = 'Cargo.toml';
// 顶层 "version" 那一行；锚在行首（带缩进），避免误伤嵌套字段
const TAURI_VERSION_RE = /^(\s*"version"\s*:\s*")([^"]*)(")/m;
// [workspace.package] 段里的 version = "x.y.z"。
// 注意：成员 crate 里是 `version.workspace = true`（等号后没有字符串），本正则匹不到 —— 这是对的，
// 匹到了反而说明有人在成员里写死了版本号，那正是要避免的。
const CARGO_VERSION_RE = /(^\[workspace\.package\][\s\S]*?^version\s*=\s*")([^"]*)(")/m;
// 成员 crate 路径，用于兜底检查「没人写死 version」
const MEMBER_MANIFESTS = [
  'src-tauri/Cargo.toml',
  'crates/core/Cargo.toml',
  'crates/hook/Cargo.toml',
];

// ---------------------------------------------------------------------------
// 版本号：package.json → tauri.conf.json + Cargo.toml
// ---------------------------------------------------------------------------
const version = readJson(join(ROOT, 'package.json')).version;
if (!/^\d+\.\d+\.\d+/.test(String(version || ''))) {
  fail(`package.json 里的 version 不合法: ${JSON.stringify(version)}`);
}

// 把版本号写进某个文件（只替换那一行的值，不动其它格式，git diff 干净）
function syncVersionIn(relPath, re, what) {
  const abs = join(ROOT, relPath);
  if (!existsSync(abs)) fail(`找不到 ${relPath}`);
  const src = readFileSync(abs, 'utf8');
  const m = src.match(re);
  if (!m) fail(`${relPath} 里没找到${what}`);
  const old = m[2];
  const next = src.replace(re, `$1${version}$3`);
  if (next === src) {
    log(`版本号同步: ${relPath} 已是 ${version}`);
    return;
  }
  writeFileSync(abs, next);
  log(`版本号同步: ${relPath} ${old} → ${version}`);
}

// 兜底：成员 crate 里出现字面量 `version = "x.y.z"` 就会盖掉 workspace 的值，
// 而本脚本同步不到它 —— 那种情况下构建日志/版本资源会和安装包对不上，必须在这里掐掉。
function assertNoHardcodedMemberVersion() {
  for (const rel of MEMBER_MANIFESTS) {
    const abs = join(ROOT, rel);
    if (!existsSync(abs)) continue;
    const m = readFileSync(abs, 'utf8').match(/^\[package\][\s\S]*?^version\s*=\s*"/m);
    if (m) {
      fail(
        `${rel} 的 [package] 里写死了 version（${m[0].split('\n').pop().trim()}）。\n` +
          `       版本号只在根 Cargo.toml 的 [workspace.package] 里维护，` +
          `成员一律写 version.workspace = true。`,
      );
    }
  }
}

// 两处都要同步，漏一个就会出现「安装包 1.1.6 / 构建日志 2.0.0」这种对不上
function syncVersions() {
  assertNoHardcodedMemberVersion();
  syncVersionIn(TAURI_CONF, TAURI_VERSION_RE, '顶层 "version" 字段');
  syncVersionIn(CARGO_TOML, CARGO_VERSION_RE, '[workspace.package] 段的 version');
}

// ---------------------------------------------------------------------------
// 打包
// ---------------------------------------------------------------------------
function packRust(outDir) {
  log(`—— Rust / Tauri 版 v${version} ——`);

  const env = {};
  // 本机对 GitHub 的连接会被重置，而 tauri bundler 的 NSIS 工具链只从 GitHub releases 取。
  // 不设这个变量会直接 `Error failed to bundle project: timeout: global`。
  if (!process.env.TAURI_BUNDLER_TOOLS_GITHUB_MIRROR) {
    env.TAURI_BUNDLER_TOOLS_GITHUB_MIRROR = 'https://ghfast.top';
    log('  未设 TAURI_BUNDLER_TOOLS_GITHUB_MIRROR，本次用 https://ghfast.top');
  }

  // ⚠ 必须**先**构建 hook exe，再 `cargo tauri build`。
  // hook exe 是 sidecar（`tauri.conf.json` 的 `bundle.resources` 把它打进 $INSTDIR，
  // 与 GUI 同级），而 `cargo tauri build` **只构建 GUI 那一个 bin**，不会顺带构建
  // workspace 里的 `pomodoro-hook`。漏了这一步的两种失败长相：
  //   ① 干净 checkout 上 resources 的源文件不存在 → 打包直接失败；
  //   ② 本地有上一次的产物 → 打包"成功"，但装上去的是**上一版**的 hook exe
  //      （版本资源陈旧，且新改的 hook 逻辑根本没进包）——更隐蔽。
  run('cargo', ['build', '--release', '-p', 'pomodoro-hook'], env);

  run('cargo', ['tauri', 'build'], env);

  // ⚠ 闸门：`cargo tauri build` 抛异常才算失败，它"成功"并不代表安装包装齐了运行时需要的文件。
  // NSIS 模板默认**只装 main binary** —— `bundle.resources` 漏一项，装完的用户就拿不到 hook CLI /
  // OpenCode 插件（M5 实打实踩过：开发目录里 GUI 与 hook 天然同级，永远看不出来）。
  run('node', ['scripts/check-installer-contents.mjs'], env);

  const nsisDir = join(ROOT, 'target', 'release', 'bundle', 'nsis');
  if (!existsSync(nsisDir)) fail(`没找到 Tauri 产物目录: ${nsisDir}`);
  const all = readdirSync(nsisDir).filter((f) => f.toLowerCase().endsWith('.exe'));
  if (all.length === 0) fail(`Tauri 没产出任何 .exe（${nsisDir}）`);

  // 只认当前版本号的产物。target/ 里会留着上一次打包的旧安装包（改过版本号后文件名不同），
  // 全部照抄的话旧包会顶掉新包 —— 实测踩过（旧 2.0.0 覆盖了新 1.1.6，且日志里看着像成功）。
  const setups = all.filter((f) => f.includes(version));
  const stale = all.filter((f) => !f.includes(version));
  if (stale.length) {
    log(`  忽略 ${stale.length} 个旧版本产物（可在 ${toPosix(nsisDir)} 里删掉）: ${stale.join(', ')}`);
  }
  if (setups.length === 0) {
    fail(`没找到 v${version} 的安装包；${nsisDir} 里只有: ${all.join(', ')}`);
  }
  if (setups.length > 1) {
    // 将来若同时出 x64 / arm64 两个包，得先把文件名区分开再进来改这里，别默认覆盖
    fail(`v${version} 匹配到多个安装包，无法确定发哪个: ${setups.join(', ')}`);
  }

  const collected = [];
  for (const file of setups) {
    const target = `Pomodoro-Fluent-Rust-Setup-${version}.exe`;
    copyFileSync(join(nsisDir, file), join(outDir, target));
    collected.push(target);
    const mb = (statSync(join(outDir, target)).size / 1048576).toFixed(2);
    log(`  ✓ ${target}  ${mb} MB  (← ${file})`);
  }
  return collected;
}

// ---------------------------------------------------------------------------
// 先同步版本号，再打包
// ---------------------------------------------------------------------------
const outDir = resolve(argOf('--out', join(ROOT, 'release')));
mkdirSync(outDir, { recursive: true });

syncVersions();
const collected = packRust(outDir);

log('');
log(`完成，共 ${collected.length} 个产物落 ${outDir}`);
for (const name of collected) log(`  ${name}`);

log('');
log('下一步：一个 tag 一个 release');
log(`  gh release create v${version} --target <mergeCommit> --title v${version} ${toPosix(outDir)}/*`);
log('');
log(`（tag 是 v${version}，不带任何前缀 —— 别加 rust- 之类的前缀，`);
log('  否则 app:check-update 的 /releases/latest 会挑不到它。）');
