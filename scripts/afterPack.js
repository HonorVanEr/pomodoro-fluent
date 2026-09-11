'use strict';

// electron-builder afterPack 钩子：打包完成后裁剪 Electron 运行时中用不到的文件，
// 减小安装包与安装后体积。仅影响 win-unpacked 产物，不影响运行逻辑。

const fs = require('fs');
const path = require('path');

// 保留的语言包（界面为中文，缺失语言时 Electron 回退到 en-US）
const KEEP_LOCALES = new Set(['en-US.pak', 'zh-CN.pak']);

// 本应用为纯 UI，不使用 WebGPU / Vulkan：
//  - dxcompiler.dll / dxil.dll   Dawn(WebGPU) 着色器编译器，约 26MB
//  - vk_swiftshader.dll          Vulkan 软件渲染回退，约 5MB
//  - vulkan-1.dll                Vulkan 加载器，约 1MB
// 注意：d3dcompiler_47.dll（D3D 着色器编译）与 ffmpeg.dll（启动即加载）必须保留。
const REMOVE_DLLS = ['dxcompiler.dll', 'dxil.dll', 'vk_swiftshader.dll', 'vulkan-1.dll'];

// Chromium 第三方许可文本（约 20MB）。删除可再省体积，但商业分发建议保留以满足许可合规。
const REMOVE_LICENSES = false;

module.exports = async function afterPack(context) {
  const dir = context.appOutDir;

  // 1) 语言包：仅保留中英文
  const locales = path.join(dir, 'locales');
  if (fs.existsSync(locales)) {
    for (const f of fs.readdirSync(locales)) {
      if (!KEEP_LOCALES.has(f)) {
        fs.rmSync(path.join(locales, f), { force: true });
      }
    }
  }

  // 2) 用不到的图形后端 DLL
  for (const f of REMOVE_DLLS) {
    const p = path.join(dir, f);
    if (fs.existsSync(p)) fs.rmSync(p, { force: true });
  }

  // 3) 许可文本（默认保留）
  if (REMOVE_LICENSES) {
    const lic = path.join(dir, 'LICENSES.chromium.html');
    if (fs.existsSync(lic)) fs.rmSync(lic, { force: true });
  }
};
