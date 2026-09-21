#!/usr/bin/env node
'use strict';

// ---------------------------------------------------------------------------
// 桥接对齐检查：renderer/ ←→ bridge.js ←→ commands.rs
//
// Rust 版不许改 `renderer/`，所以渲染层对主进程的所有期待都必须由
// `src-tauri/src/bridge.js` 一个人补齐。这条链上有三个点会**静默**断掉：
//
//   1. preload.js 暴露过、但 bridge.js 忘了补的方法
//      → 渲染层调到就是 `undefined is not a function`，且往往死在初始化那一段，
//        整个界面直接不动。（真踩过：漏了 `requestHeldPending`，而渲染层是
//        在启动时**无条件**调一次的。）
//   2. bridge.js 调了、commands.rs 里没有的命令
//      → invoke 的 Promise 被 bridge 吞掉（只留一条 console 警告），
//        表现是"点了没反应"，最难查。
//   3. commands.rs 有、bridge.js 从没调过的命令
//      → 只是死代码，不致命，报出来当提示。
//
// 用法：node scripts/check-bridge-parity.mjs
// 退出码：0 = 对齐；1 = 有缺项（会列出缺什么）。
// ---------------------------------------------------------------------------

import { readFileSync, existsSync } from 'node:fs';
import { dirname, join, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';

const ROOT = resolve(dirname(fileURLToPath(import.meta.url)), '..');
const read = (rel) => readFileSync(join(ROOT, rel), 'utf8');

const PRELOAD = 'preload.js';
const BRIDGE = 'src-tauri/src/bridge.js';
const COMMANDS = 'src-tauri/src/commands.rs';
const RENDERER_FILES = ['renderer/app.js', 'renderer/notify.js'];

for (const rel of [PRELOAD, BRIDGE, COMMANDS, ...RENDERER_FILES]) {
  if (!existsSync(join(ROOT, rel))) {
    console.error(`[bridge] 找不到 ${rel}`);
    process.exit(1);
  }
}

// 取出某个对象字面量里缩进为 N 个空格的顶层键名。
// 靠缩进区分"对象自己的键"和"键值里的函数体"——两边文件都是这个排版风格。
function objectKeys(src, startMarker, indent) {
  const lines = src.split(/\r?\n/);
  const start = lines.findIndex((l) => l.includes(startMarker));
  if (start < 0) return null;
  const re = new RegExp(`^ {${indent}}([A-Za-z_$][\\w$]*)\\s*:`);
  const keys = [];
  for (let i = start + 1; i < lines.length; i += 1) {
    const line = lines[i];
    // 回到同级的 `};` 就是这个对象结束了
    if (new RegExp(`^ {${indent - 2}}\\}`).test(line)) break;
    const m = line.match(re);
    if (m) keys.push(m[1]);
  }
  return keys;
}

const preloadKeys = objectKeys(read(PRELOAD), "exposeInMainWorld('pomodoro'", 2);
const bridgeKeys = objectKeys(read(BRIDGE), 'window.pomodoro = {', 4);
if (!preloadKeys || !bridgeKeys) {
  console.error('[bridge] 没能在 preload.js / bridge.js 里定位到 window.pomodoro 对象');
  process.exit(1);
}

const bridgeSrc = read(BRIDGE);
const commandsSrc = read(COMMANDS);

// bridge 里所有 invoke('x') / send('x') 的目标命令名
const called = new Set(
  [...bridgeSrc.matchAll(/\b(?:invoke|send)\('([a-z0-9_]+)'/g)].map((m) => m[1]),
);
// commands.rs 里所有 #[tauri::command] 注册的函数名
const defined = new Set(
  [...commandsSrc.matchAll(/#\[tauri::command\][\s\S]*?pub\s+(?:async\s+)?fn\s+([a-z0-9_]+)/g)].map(
    (m) => m[1],
  ),
);

// 渲染层实际用到的 window.pomodoro.<方法>
const used = new Set();
for (const rel of RENDERER_FILES) {
  for (const m of read(rel).matchAll(/pomodoro\.([A-Za-z_$][\w$]*)/g)) used.add(m[1]);
}

const missingInBridge = [...new Set([...preloadKeys, ...used])].filter(
  (k) => !bridgeKeys.includes(k),
);
const calledButUndefined = [...called].filter((c) => !defined.has(c));
const definedButNeverCalled = [...defined].filter((d) => !called.has(d));
const extraInBridge = bridgeKeys.filter((k) => !preloadKeys.includes(k));

let failed = false;
const head = (t) => console.log(`\n${t}`);
const ok = (t) => console.log(`  ✓ ${t}`);

head(`preload.js 暴露 ${preloadKeys.length} 个方法 / bridge.js 实现 ${bridgeKeys.length} 个`);

if (missingInBridge.length) {
  failed = true;
  head('✗ preload.js（或渲染层）要的、bridge.js 没补的方法：');
  for (const k of missingInBridge) console.log(`    ${k}`);
  console.log('  → 渲染层调用时会 TypeError。补进 bridge.js 的 window.pomodoro。');
} else {
  ok('preload.js 暴露的方法 bridge.js 全都补上了');
}

if (calledButUndefined.length) {
  failed = true;
  head('✗ bridge.js 调了、commands.rs 里没有的命令：');
  for (const c of calledButUndefined) console.log(`    ${c}`);
  console.log('  → invoke 的 Promise 会被 bridge 吞掉，表现成"点了没反应"。');
} else {
  ok(`bridge.js 调用的 ${called.size} 个命令在 commands.rs 里都有定义`);
}

if (extraInBridge.length) {
  // 不算失败：bridge 故意多给几个 M2 占位方法，方便渲染层提前调用
  head('提示：bridge.js 有、preload.js 没有的键（M2 占位属正常）：');
  console.log(`    ${extraInBridge.join(', ')}`);
}

if (definedButNeverCalled.length) {
  // 也不算失败：可能只是留着给将来用
  head('提示：commands.rs 定义但 bridge.js 从没调过的命令：');
  console.log(`    ${definedButNeverCalled.join(', ')}`);
}

console.log('');
if (failed) {
  console.error('[bridge] 对齐检查失败');
  process.exit(1);
}
console.log('[bridge] 对齐检查通过');
