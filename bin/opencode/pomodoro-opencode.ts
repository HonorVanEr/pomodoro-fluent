/**
 * 番茄钟 × OpenCode 插件
 *
 * 安装：
 *   "<hook 目录>/pomodoro-hook.exe" install --agent opencode   (Tauri 版)
 *   node "<hook 目录>/pomodoro-hook.js" install --agent opencode   (Electron 版)
 * 或手动：把本文件放到 ~/.config/opencode/plugins/pomodoro-opencode.ts，
 * 并在 ~/.config/opencode/opencode.json 里加 "plugin": ["file://<绝对路径>"]
 *
 * 行为：
 *   permission.ask  → 调番茄钟网关弹权限窗，用户点允许/拒绝后回写 status
 *   question.asked  → 调番茄钟网关弹提问窗，用户作答后 POST 回 OpenCode
 *   session.idle    → 上报 stop（番茄钟据此建议休息）
 *
 * 所有逻辑都在 hook CLI 里，这里只做转发，方便三端共用一套弹窗与协议解析。
 * 番茄钟没启动时全部静默放行，绝不阻断 OpenCode。
 */

import { spawn } from "node:child_process"
import { existsSync } from "node:fs"
import { homedir } from "node:os"
import { join } from "node:path"

// ---- 定位 hook CLI ----
// Electron 版是 node 脚本（pomodoro-hook.js），Tauri 版是原生 exe（pomodoro-hook.exe）。
// 两个都找，exe 优先：不依赖 node、启动更快。找不到时插件整体静默放行。
function findHookCli() {
  if (process.env.POMODORO_HOOK_PATH) return process.env.POMODORO_HOOK_PATH
  const appData = process.env.APPDATA || join(homedir(), "AppData", "Roaming")
  const dirs = [
    join(appData, "番茄钟", "hook"),
    join(appData, "pomodoro-fluent", "hook"),
  ]
  for (const name of ["pomodoro-hook.exe", "pomodoro-hook.js"])
    for (const dir of dirs) {
      const p = join(dir, name)
      if (existsSync(p)) return p
    }
  return null
}

const HOOK = findHookCli()
const HOOK_IS_EXE = !!HOOK && /\.exe$/i.test(HOOK)

function callHook(subcommand, payload, timeoutMs = 30000) {
  return new Promise((resolve) => {
    if (!HOOK) return resolve(null)
    let child
    try {
      // exe 直接执行；脚本交给当前 node 运行时。
      child = HOOK_IS_EXE
        ? spawn(HOOK, [subcommand], { stdio: ["pipe", "pipe", "ignore"], windowsHide: true })
        : spawn(process.execPath, [HOOK, subcommand], { stdio: ["pipe", "pipe", "ignore"], windowsHide: true })
    } catch {
      return resolve(null)
    }
    let out = ""
    let done = false
    const finish = (value) => {
      if (done) return
      done = true
      try { resolve(value ? JSON.parse(value) : null) } catch { resolve(null) }
    }
    const timer = setTimeout(() => {
      try { child.kill() } catch {}
      finish(out)
    }, timeoutMs)
    child.stdout.setEncoding("utf8")
    child.stdout.on("data", (c) => { out += c })
    child.on("error", () => { clearTimeout(timer); resolve(null) })
    child.on("close", () => { clearTimeout(timer); finish(out) })
    try { child.stdin.end(JSON.stringify(payload || {})) } catch { /* ignore */ }
  })
}

// ---- OpenCode 服务端地址（用于回传提问答案） ----
function serverUrl() {
  return (
    process.env.OPENCODE_SERVER_URL ||
    process.env.OPENCODE_BASE_URL ||
    (process.env.OPENCODE_PORT ? `http://127.0.0.1:${process.env.OPENCODE_PORT}` : "") ||
    "http://127.0.0.1:4096"
  )
}

async function replyQuestion(ctx, sessionID, requestID, answers) {
  // 优先用插件拿到的 SDK client（自带路由与鉴权）
  try {
    if (ctx?.client?.question?.reply) {
      await ctx.client.question.reply({ requestID, answers })
      return true
    }
  } catch {}
  // 退回 HTTP：新旧两种路由都试一遍
  const base = serverUrl().replace(/\/+$/, "")
  const tries = [
    { url: `${base}/session/${encodeURIComponent(sessionID)}/question/reply`, body: { requestID, answers } },
    { url: `${base}/question/${encodeURIComponent(requestID)}/reply`, body: { answers } },
  ]
  for (const t of tries) {
    try {
      const res = await fetch(t.url, {
        method: "POST",
        headers: { "Content-Type": "application/json" },
        body: JSON.stringify(t.body),
      })
      if (res.ok) return true
    } catch {}
  }
  return false
}

async function rejectQuestion(ctx, sessionID, requestID) {
  try {
    if (ctx?.client?.question?.reject) {
      await ctx.client.question.reject({ requestID })
      return
    }
  } catch {}
  const base = serverUrl().replace(/\/+$/, "")
  const tries = [
    { url: `${base}/session/${encodeURIComponent(sessionID)}/question/reject`, body: { requestID } },
    { url: `${base}/question/${encodeURIComponent(requestID)}/reject`, body: {} },
  ]
  for (const t of tries) {
    try {
      const res = await fetch(t.url, {
        method: "POST",
        headers: { "Content-Type": "application/json" },
        body: JSON.stringify(t.body),
      })
      if (res.ok) return
    } catch {}
  }
}

// ---- 会话上下文：让弹窗能显示「哪个任务」 ----
function shortSession(id) {
  return id ? String(id).slice(-6) : ""
}

// 尽力取会话标题（拿不到就算了，弹窗会退化成只显示会话尾号与项目名）
async function sessionInfo(ctx, sessionID) {
  const info = { sessionID, sessionTitle: "", directory: process.cwd() }
  if (!sessionID) return info
  try {
    if (ctx?.client?.session?.get) {
      const res = await ctx.client.session.get({ path: { id: sessionID } })
      const data = res?.data || res
      if (data?.title) info.sessionTitle = String(data.title)
      if (data?.directory) info.directory = String(data.directory)
      return info
    }
  } catch {}
  try {
    const base = serverUrl().replace(/\/+$/, "")
    const res = await fetch(`${base}/session/${encodeURIComponent(sessionID)}`)
    if (res.ok) {
      const data = await res.json()
      if (data?.title) info.sessionTitle = String(data.title)
      if (data?.directory) info.directory = String(data.directory)
    }
  } catch {}
  return info
}

export const PomodoroPlugin = async (ctx) => {
  return {
    // 权限请求：番茄钟弹窗里点允许/拒绝，结果回写成 OpenCode 的 status
    "permission.ask": async (input, output) => {
      const info = await sessionInfo(ctx, input?.sessionID)
      const res = await callHook("opencode-permission", {
        permission: input,
        agent: input?.agent || input?.metadata?.agent || "",
        ...info,
      })
      if (!res || !res.status) return // 番茄钟没运行 / 未决策 → 交给 OpenCode 原生询问
      if (res.status === "allow") output.status = "allow"
      else if (res.status === "deny") output.status = "deny"
    },

    event: async ({ event }) => {
      const type = event?.type || ""
      const props = event?.properties || event?.data || event || {}

      if (type === "question.asked") {
        const questions = props.questions || []
        const sessionID = props.sessionID
        const info = await sessionInfo(ctx, sessionID)
        const res = await callHook("opencode-question", {
          questions,
          agent: props.agent || "",
          ...info,
          sessionID,
          requestID: props.requestID || props.id,
          serverUrl: serverUrl(),
        })
        if (!res) return
        if (res.reject || !Array.isArray(res.answers)) {
          await rejectQuestion(ctx, sessionID, props.requestID || props.id)
          return
        }
        await replyQuestion(ctx, sessionID, props.requestID || props.id, res.answers)
        return
      }

      if (type === "session.idle" || type === "session.error" ||
          type === "session.created" || type === "session.deleted") {
        callHook("opencode-event", { event: type, properties: props })
      }
    },
  }
}

export default PomodoroPlugin
