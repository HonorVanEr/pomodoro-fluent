#!/usr/bin/env node
'use strict';

// ---------------------------------------------------------------------------
// 闸门：安装包里到底装了什么？
//
// ## 起因（M5 打包审计，2026-09-21）
//
// `cargo tauri build` 报告成功、安装包也生成了，但 NSIS 里只有一条打包指令 ——
// **只装了 main binary**。而运行时是从 **exe 同级目录**读 `pomodoro-hook.exe`
// （`ensure_hook_exe()`）和 `opencode/pomodoro-opencode.ts`（`resolve_plugin_source()`）的，
// 两者都没进包 ⇒ 用户装完点「一键安装 hook」直接失败。
//
// 这个缺口在开发目录里**永远看不出来**：`target/release/` 下 GUI 与 hook 天然躺在一起。
//
// ## 本脚本做什么
//
// 读 `src-tauri/tauri.conf.json` 的 `bundle.resources`，逐条到 Tauri 生成的 `installer.nsi`
// 里核对「是否真有一条 `File` 指令把它拷进 `$INSTDIR`」，并打印源文件大小/mtime
// （便于肉眼看是不是本次构建的产物，而不是上一次留下的陈旧文件）。
//
// ## 用法
//
//   node scripts/check-installer-contents.mjs
//   node scripts/check-installer-contents.mjs --nsi <别的 installer.nsi>   # 负数用例/排查用
//   node scripts/check-installer-contents.mjs --conf <别的 tauri.conf.json>
//
// ⚠ 加新资源**不用改这里** —— 自动跟随 `bundle.resources`。
//    `pack-release.mjs` 已在 `cargo tauri build` 之后自动调用本脚本。
// ---------------------------------------------------------------------------

import { existsSync, readFileSync, statSync } from 'node:fs';
import { dirname, isAbsolute, join, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';

const ROOT = resolve(dirname(fileURLToPath(import.meta.url)), '..');
const argv = process.argv.slice(2);
const argOf = (name, fallback = '') => {
  const i = argv.indexOf(name);
  return i >= 0 ? (argv[i + 1] ?? fallback) : fallback;
};

const CONF = argOf('--conf', join(ROOT, 'src-tauri', 'tauri.conf.json'));
// Tauri 的 NSIS 打包步骤把它生成在这里（不是最终的 bundle/nsis/ 目录）
const NSI = argOf('--nsi', join(ROOT, 'target', 'release', 'nsis', 'x64', 'installer.nsi'));

const log = (msg) => console.log(`[installer] ${msg}`);
const norm = (p) => p.replace(/\\/g, '/');
const problems = [];

if (!existsSync(NSI)) {
  console.error(`[installer] 失败: 找不到 Tauri 生成的 installer.nsi: ${norm(NSI)}`);
  console.error('           它是 `cargo tauri build` 的 NSIS 副产物。');
  console.error('           若 Tauri 改了布局，请更新本脚本默认的 --nsi 路径后重试。');
  process.exit(1);
}

const conf = JSON.parse(readFileSync(CONF, 'utf8'));
const nsi = readFileSync(NSI, 'utf8');
const confDir = dirname(CONF);
const nsiStat = statSync(NSI);

log(`installer.nsi = ${norm(NSI)}  (${nsiStat.size} B, ${nsiStat.mtime.toISOString()})`);

// ---------------------------------------------------------------------------
// bundle.resources → [{ src, dest }]
//   map 的方向是 **{ 源: 目标 }**（tauri-utils::resources 的反序列化，
//   `let (pattern, dest) = iter.next()`）；数组形式则目标取文件名。
// ---------------------------------------------------------------------------
function parseResources(res) {
  if (!res) return [];
  if (Array.isArray(res)) return res.map((s) => ({ src: s, dest: s.replace(/^.*[\\/]/, '') }));
  return Object.entries(res).map(([src, dest]) => ({ src, dest: String(dest) }));
}

// ---------------------------------------------------------------------------
// ① GUI 本体
// ---------------------------------------------------------------------------
if (/^\s*File\s+"\$\{MAINBINARYSRCPATH\}"/m.test(nsi)) {
  log('✓ GUI main binary 已入包  (File "${MAINBINARYSRCPATH}")');
} else {
  problems.push('installer.nsi 里没有 `File "${MAINBINARYSRCPATH}"` —— 连 GUI 本体都没装进去？');
}

// ---------------------------------------------------------------------------
// ② bundle.resources 逐条核对
//    Tauri 把每条资源展开成：  File /a "/oname=<目标>" "<源绝对路径>"
//    目标里的分隔符是 `\`（如 oname=opencode\pomodoro-opencode.ts），比较前统一成 `/`。
// ---------------------------------------------------------------------------
const entries = parseResources(conf.bundle && conf.bundle.resources);
log(
  entries.length
    ? `bundle.resources: ${entries.length} 项`
    : 'bundle.resources 为空 —— 除 GUI 外没有其它文件要装',
);

for (const { src, dest } of entries) {
  const wantOname = norm(dest);
  let declaredSrc = null;
  for (const m of nsi.matchAll(/^\s*File\s+\/a\s+"\/oname=([^"]*)"\s+"([^"]*)"/gm)) {
    if (norm(m[1]) === wantOname) {
      declaredSrc = m[2];
      break;
    }
  }

  const srcAbs = isAbsolute(src) ? src : resolve(confDir, src);

  if (declaredSrc === null) {
    problems.push(`资源「${dest}」没进包：installer.nsi 里找不到 oname=${dest} 的 File 指令`);
    continue;
  }
  // 源路径核对：.nsi 里是绝对路径且带 `..`，normalize 后比
  if (norm(resolve(norm(declaredSrc))) !== norm(srcAbs)) {
    problems.push(
      `资源「${dest}」的源路径对不上：\n` +
        `           .nsi  声明 ${declaredSrc}\n` +
        `           配置期望 ${srcAbs}`,
    );
  }
  if (!existsSync(srcAbs)) {
    problems.push(
      `资源「${dest}」的源文件不存在: ${norm(srcAbs)}\n` +
        `           （干净 checkout 上多半是漏了预构建步骤，如 ` +
        `cargo build --release -p pomodoro-hook）`,
    );
    continue;
  }
  const st = statSync(srcAbs);
  if (st.size === 0) problems.push(`资源「${dest}」的源文件是 0 字节: ${norm(srcAbs)}`);
  log(`✓ ${dest}  ← ${norm(srcAbs)}  (${st.size} B, ${st.mtime.toISOString()})`);
}

// ---------------------------------------------------------------------------
if (problems.length) {
  console.error('');
  console.error(`[installer] 失败: ${problems.length} 处问题 ——`);
  for (const p of problems) console.error(`  ✗ ${p}`);
  console.error('');
  console.error('  排查建议：手工核对 `grep -n -E \'^\\s*(File|CreateDirectory)\' <installer.nsi>`，');
  console.error('  并对照 `src-tauri/tauri.conf.json` 的 `bundle.resources`（方向是 {源: 目标}）。');
  process.exit(1);
}

log(`通过：GUI + ${entries.length} 项资源都已进 $INSTDIR`);
