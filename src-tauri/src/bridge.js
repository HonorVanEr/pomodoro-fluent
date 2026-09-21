// bridge.js —— 把渲染层的 `window.pomodoro.*` 接到 Tauri 的 invoke / event 上。
//
// 注入方式：`main.rs` 里用 `WebviewWindowBuilder::initialization_script(include_str!(...))`。
// Tauri 把自己那几段初始化脚本放在前面（`manager/webview.rs` 里 "Prepend" 那一步），
// 所以这里能直接拿到 `window.__TAURI__`。
//
// 目标：**renderer/ 一行不改**。`preload.js` 暴露过什么方法、什么事件名，这里就补什么
// —— 包括那些看起来没用上、但渲染层启动时会无条件调一次的（如 requestGatewayState）。
//
// ⚠ 改这里的命令名要同步改 `src/commands.rs`：Tauri 命令名只能是合法标识符，
// 所以 `window:close` 这类 Electron 频道名被改成了 `window_close`，
// 映射关系只存在于本文件里。

(function () {
  'use strict';

  var T = window.__TAURI__;
  if (!T || !T.core || !T.event) {
    // withGlobalTauri 被关掉时会走到这里。报清楚原因，否则渲染层只会到处报
    // "undefined 不是函数"，很难查。
    console.error('[bridge] window.__TAURI__ 不可用 —— 检查 tauri.conf.json 的 app.withGlobalTauri');
    return;
  }
  var invoke = T.core.invoke;
  var listen = T.event.listen;

  // -------------------------------------------------------------------------
  // 通道封装
  // -------------------------------------------------------------------------

  // 发后不理：渲染层里这些调用都是"通知主进程"，既不 await 也不看返回值。
  // 失败的 Promise 必须吞掉 —— 不吞的话 devtools 里会刷一屏 unhandled rejection，
  // 而其中大部分只是"M2 还没实现"，不是真问题。
  function send(cmd, payload) {
    try {
      var p = payload === undefined ? invoke(cmd) : invoke(cmd, payload);
      if (p && typeof p.catch === 'function') {
        p.catch(function (err) { console.warn('[bridge] ' + cmd + ' 失败:', err); });
      }
    } catch (err) {
      console.warn('[bridge] ' + cmd + ' 调用异常:', err);
    }
  }

  // 事件订阅：`listen` 是异步的，但渲染层期望**同步**拿到退订函数
  // （`onPinChanged(cb)` 的返回值）。所以先返回一个"可延迟退订"的闭包，
  // 订阅真正建立后再补上真正的 unlisten。订阅失败不能抛 —— 渲染层没有 try 包着。
  function subscribe(event, cb) {
    var unlisten = null;
    var cancelled = false;
    listen(event, function (e) {
      try {
        cb(e.payload);
      } catch (err) {
        console.error('[bridge] ' + event + ' 回调异常:', err);
      }
    }).then(function (fn) {
      if (cancelled) { try { fn(); } catch (err) { /* 已退订 */ } } else { unlisten = fn; }
    }).catch(function (err) {
      console.error('[bridge] 订阅 ' + event + ' 失败:', err);
    });
    return function () {
      cancelled = true;
      if (unlisten) {
        try { unlisten(); } catch (err) { /* ignore */ }
        unlisten = null;
      }
    };
  }

  // -------------------------------------------------------------------------
  // 剪贴板：刻意不走 Rust
  //
  // 渲染层都是在按钮点击（有用户手势）里调的，用浏览器 API 就够了；
  // 为这一次调用去引 tauri-plugin-clipboard-manager（或手写 Win32 剪贴板那
  // 一整套 GlobalAlloc）不划算。WebView2 在 tauri:// 这类安全上下文下允许
  // navigator.clipboard，万一被拒就退回 textarea + execCommand。
  // -------------------------------------------------------------------------

  function legacyCopy(text) {
    try {
      var ta = document.createElement('textarea');
      ta.value = text;
      ta.setAttribute('readonly', '');
      ta.style.position = 'fixed';
      ta.style.top = '-1000px';
      ta.style.opacity = '0';
      document.body.appendChild(ta);
      ta.select();
      document.execCommand('copy');
      document.body.removeChild(ta);
    } catch (err) {
      console.warn('[bridge] 复制失败:', err);
    }
  }

  function copyText(text) {
    var s = text == null ? '' : String(text);
    if (navigator.clipboard && typeof navigator.clipboard.writeText === 'function') {
      navigator.clipboard.writeText(s).catch(function () { legacyCopy(s); });
      return;
    }
    legacyCopy(s);
  }

  // -------------------------------------------------------------------------
  // 通知 / 交互弹窗
  //
  // 参数形状必须与 `preload.js` 逐项对齐（**签名是渲染层的契约，不能改**）：
  //   showNotify(payload)           closeNotify(id)
  //   getNotifyPayload(id) [await]  resizeNotify(width, height)
  //   respondInteraction(payload)   holdInteraction(id)   reopenInteraction(id)
  //   respondConfirm(id, action)
  //
  // ⚠ 两个易错的形状差异：
  // 1. `ipcRenderer.send(频道, 裸值)` → Tauri 的 invoke **只吃对象**，所以这里
  //    一律包成 `{ id }` / `{ payload }` / `{ width, height }`。少包一层就是
  //    `invalid args`，而且只进 console（send 会吞掉拒绝），界面表现为"点了没反应"。
  // 2. `getNotifyPayload` 是**会被 await** 的，必须原样返回 invoke 的 Promise；
  //    不能用 `send`（它是发后不理）。返回 null 由渲染层兜底成"时间到"通知。
  // -------------------------------------------------------------------------

  // -------------------------------------------------------------------------
  // window.pomodoro —— 与 preload.js 逐项对应
  // -------------------------------------------------------------------------

  window.pomodoro = {
    // ---- 窗口控制 ----
    minimizeToTray: function () { send('window_minimize_to_tray'); },
    closeWindow: function () { send('window_close'); },
    togglePin: function () { send('window_toggle_pin'); },
    dragStart: function () { send('window_drag_start'); },
    dragEnd: function () { send('window_drag_end'); },
    dockReveal: function () { send('mini_dock_reveal'); },
    dockHide: function () { send('mini_dock_hide_request'); },

    // ---- 通知 / 交互弹窗 ----
    showNotify: function (payload) { send('notify_show', { payload: payload || {} }); },
    closeNotify: function (id) { send('notify_close', { id: id == null ? '' : String(id) }); },
    getNotifyPayload: function (id) {
      // 会被 await：把调用异常也转成 resolved(null)，免得渲染层多一处 catch
      try {
        return invoke('notify_payload', { id: String(id == null ? '' : id) })
          .catch(function (err) {
            console.warn('[bridge] notify_payload 失败:', err);
            return null;
          });
      } catch (err) {
        console.warn('[bridge] notify_payload 调用异常:', err);
        return Promise.resolve(null);
      }
    },
    resizeNotify: function (width, height) {
      send('notify_resize', { width: Number(width) || 0, height: Number(height) || 0 });
    },
    respondInteraction: function (payload) { send('interaction_respond', { payload: payload || {} }); },
    holdInteraction: function (id) { send('interaction_hold', { id: id == null ? '' : String(id) }); },
    reopenInteraction: function (id) { send('interaction_reopen', { id: id == null ? '' : String(id) }); },
    // 渲染层启动时会**无条件**调一次 requestHeldPending()（app.js 里紧跟着 onPendingHeld 注册）。
    // 漏掉这个方法就是启动即 TypeError，后面的初始化全被带断 —— Rust 侧回推空列表。
    requestHeldPending: function () { send('pending_get_held'); },
    respondConfirm: function (id, action) {
      send('respond_confirm', {
        id: String(id == null ? '' : id),
        action: String(action == null ? '' : action),
      });
    },
    // 已收起的确认（主窗口提示条）：推 + 拉两条路
    onPendingHeld: function (cb) { return subscribe('state:pending-held', cb); },

    // ---- Agent 网关（M2 接真货，M1 诚实地回"已停用"）----
    setGatewayEnabled: function (enabled) { send('gateway_set_enabled', { enabled: !!enabled }); },
    requestGatewayState: function () { send('gateway_get_state'); },
    copyText: copyText,
    installHook: function (agent, clean) {
      return invoke('hook_install', { agent: String(agent || ''), clean: !!clean });
    },

    // ---- 关于 / 检查更新 / 外链 ----
    getAppInfo: function () { return invoke('app_info'); },
    checkUpdate: function () { return invoke('app_check_update'); },
    openExternal: function (url) { send('app_open_external', { url: String(url == null ? '' : url) }); },

    // ---- 托盘状态同步 ----
    updateTray: function (state) { send('tray_update', { patch: state || {} }); },

    // ---- 事件订阅 ----
    onTrayCommand: function (cb) { return subscribe('tray:command', cb); },
    onGatewayState: function (cb) { return subscribe('state:gateway', cb); },
    onAgentActivity: function (cb) { return subscribe('state:agent-activity', cb); },
    onPinChanged: function (cb) { return subscribe('state:pin-changed', cb); },
    onDockChanged: function (cb) { return subscribe('state:dock-changed', cb); },
  };
})();
