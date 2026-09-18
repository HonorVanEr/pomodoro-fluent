'use strict';

const { contextBridge, ipcRenderer } = require('electron');

// 渲染进程（主界面 + 通知弹窗）通过该桥接调用主进程能力
contextBridge.exposeInMainWorld('pomodoro', {
  // 窗口控制
  minimizeToTray: () => ipcRenderer.send('window:minimize-to-tray'),
  closeWindow: () => ipcRenderer.send('window:close'),
  togglePin: () => ipcRenderer.send('window:toggle-pin'),
  dragStart: () => ipcRenderer.send('window:drag-start'),
  dragEnd: () => ipcRenderer.send('window:drag-end'),
  dockReveal: () => ipcRenderer.send('mini:dock-reveal'),
  dockHide: () => ipcRenderer.send('mini:dock-hide-request'),

  // 通知 / 交互弹窗
  showNotify: (payload) => ipcRenderer.send('notify:show', payload),
  closeNotify: (id) => ipcRenderer.send('notify:close', id),
  // 弹窗页按 id 取回完整 payload（提问内容可能很长，不走 URL query）
  getNotifyPayload: (id) => ipcRenderer.invoke('notify:payload', id),
  // 弹窗页实测内容高度后回传，主进程据此调整窗口尺寸
  resizeNotify: (width, height) => ipcRenderer.send('notify:resize', { width, height }),

  // 交互弹窗（ask / permission / custom）用户决策结果
  respondInteraction: (payload) => ipcRenderer.send('interaction:respond', payload),
  // 「暂时收起」：不结束这次交互，只收起窗口，之后可从托盘唤回
  holdInteraction: (id) => ipcRenderer.send('interaction:hold', { id }),
  // 旧接口：只有 action 的确认弹窗
  respondConfirm: (id, action) => ipcRenderer.send('confirm:respond', { id, action }),

  // Agent 网关
  setGatewayEnabled: (enabled) => ipcRenderer.send('gateway:set-enabled', enabled),
  requestGatewayState: () => ipcRenderer.send('gateway:get-state'),
  copyText: (text) => ipcRenderer.send('clipboard:write', text),
  // 一键安装 hook：主进程直接跑 CLI 写配置，返回 { ok, message, files, command, log }
  installHook: (agent, clean) => ipcRenderer.invoke('hook:install', { agent, clean }),

  // 关于 / 检查更新（请求由主进程代发：渲染层有 CSP，直接取不到 GitHub）
  getAppInfo: () => ipcRenderer.invoke('app:info'),
  checkUpdate: () => ipcRenderer.invoke('app:check-update'),
  openExternal: (url) => ipcRenderer.send('app:open-external', url),

  // 托盘状态同步
  updateTray: (state) => ipcRenderer.send('tray:update', state),

  // 事件订阅
  onTrayCommand: (cb) => {
    const handler = (_e, cmd) => cb(cmd);
    ipcRenderer.on('tray:command', handler);
    return () => ipcRenderer.removeListener('tray:command', handler);
  },
  onGatewayState: (cb) => {
    const handler = (_e, val) => cb(val);
    ipcRenderer.on('state:gateway', handler);
    return () => ipcRenderer.removeListener('state:gateway', handler);
  },
  onAgentActivity: (cb) => {
    const handler = (_e, val) => cb(val);
    ipcRenderer.on('state:agent-activity', handler);
    return () => ipcRenderer.removeListener('state:agent-activity', handler);
  },
  onPinChanged: (cb) => {
    const handler = (_e, val) => cb(val);
    ipcRenderer.on('state:pin-changed', handler);
    return () => ipcRenderer.removeListener('state:pin-changed', handler);
  },
  onDockChanged: (cb) => {
    const handler = (_e, val) => cb(val);
    ipcRenderer.on('state:dock-changed', handler);
    return () => ipcRenderer.removeListener('state:dock-changed', handler);
  },
});
