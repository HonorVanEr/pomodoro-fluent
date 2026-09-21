// 参考值生成器：用真实 Node 实现算出缓存键 / 会话键，供 Rust 侧对齐测试。
// 用法：node scripts/keygen-ref.mjs
//
// ⚠ 为什么要有这个脚本：`cacheKeyFor` / `sessionKey` 决定的是**两版共用的
// 临时目录里那些文件名**（`%TEMP%/pomodoro-hook-cache`）。算法一旦分叉，
// Electron 版写下的去重记录 Rust 版读不到 —— 症状是「一次提问弹两次窗」
// 这种极难归因的毛病。所以参考值必须能一键重算，而不是靠记忆。
import { createHash } from 'node:crypto';

function cacheKeyFor(payload) {
  const id = payload.tool_use_id || payload.tool_useId || payload.toolUseId
    || `${payload.tool_name || ''}:${JSON.stringify(payload.tool_input || {})}`;
  return createHash('sha1').update(String(id)).digest('hex').slice(0, 20);
}
function sessionKey(sessionId) {
  return createHash('sha1').update(String(sessionId || 'unknown')).digest('hex').slice(0, 16);
}

const cases = [
  { tool_use_id: 'toolu_01ABC', tool_name: 'AskUserQuestion' },
  { tool_name: 'Bash', tool_input: { command: 'npm test' } },
  { tool_name: 'Bash', tool_input: { command: 'ls', description: '列出文件' } },
  { tool_name: '', tool_input: {} },
  { tool_name: 'Write', tool_input: { file_path: 'C:\\项目\\a b.ts', content: 'x=1' } },
  { tool_name: 'AskUserQuestion', tool_input: { questions: [{ question: '继续吗？', options: ['是', '否'] }] } },
];
for (const c of cases) console.log(cacheKeyFor(c));
for (const s of ['', 'unknown', 'abcdefghij', '1a2b3c4d-5e6f-7890-abcd-ef1234567890']) console.log(sessionKey(s));
