#!/usr/bin/env node
'use strict';

// ---------------------------------------------------------------------------
// 假网关（只给 scripts/check-hook-parity.sh 用）
//
// 为什么需要：hook CLI 的「已作答」输出（permission 的 allow / allow-always / deny、
// cursor 的 permission 字段、codex 的 fail-closed、opencode 的 status）只有在**真的
// 拿到一个决策**时才会产生。真网关的决策只能来自弹窗，而弹窗要人来点 —— 于是这段
// 逻辑在无人值守的差分自检里一直是个洞（第 3 节只喂了无人作答的输入，两边都静默）。
//
// 做法：起一个只实现 hook 会用到的端点的最小 HTTP 服务，`/api/interaction` 的答复
// 从 `POMODORO_FAKE_REPLY` 指定的文件**每次请求重读** —— 于是同一进程就能逐用例
// 换答复，不用反复起停。
//
// ⚠ 它**不**校验 token / Host（真网关才校验）；只为差分比对提供一个确定性对端，
// 不参与任何生产路径，也不该被别处 require。
//
// 用法：POMODORO_FAKE_REPLY=<file> node scripts/fake-gateway.mjs
//   启动后在 stdout 打一行 `PORT=<n>`（监听 127.0.0.1 的临时端口），随后一直服务到被 kill。
// ---------------------------------------------------------------------------

import fs from 'node:fs';
import http from 'node:http';

const REPLY_FILE = process.env.POMODORO_FAKE_REPLY || '';

function replyBody() {
  try {
    const t = fs.readFileSync(REPLY_FILE, 'utf8').trim();
    if (t) return t;
  } catch {
    /* 文件还没写好 / 被删了：退回空对象 */
  }
  return '{}';
}

const server = http.createServer((req, res) => {
  let body = '';
  req.on('data', (c) => {
    body += c;
  });
  req.on('end', () => {
    const path = String(req.url || '').split('?')[0];
    let out = '{"ok":true}';
    if (path === '/health') out = '{"ok":true}';
    else if (path === '/api/status') out = '{"ok":true,"host":"fake-gateway"}';
    else if (path === '/api/interaction') out = replyBody();
    res.writeHead(200, {
      'Content-Type': 'application/json',
      'Content-Length': Buffer.byteLength(out),
    });
    res.end(out);
  });
});

server.listen(0, '127.0.0.1', () => {
  console.log(`PORT=${server.address().port}`);
});

function shutdown() {
  server.close(() => process.exit(0));
  // 有 keep-alive 长连接时 close 不会立刻回调，兜一个硬退出
  setTimeout(() => process.exit(0), 500).unref();
}
process.on('SIGTERM', shutdown);
process.on('SIGINT', shutdown);
