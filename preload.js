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

  // 通知
  showNotify: (payload) => ipcRenderer.send('notify:show', payload),
  closeNotify: () => ipcRenderer.send('notify:close'),

  // 托盘状态同步
  updateTray: (state) => ipcRenderer.send('tray:update', state),

  // 事件订阅
  onTrayCommand: (cb) => {
    const handler = (_e, cmd) => cb(cmd);
    ipcRenderer.on('tray:command', handler);
    return () => ipcRenderer.removeListener('tray:command', handler);
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
