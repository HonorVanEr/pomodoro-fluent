'use strict';

// electron-builder afterPack 钩子：打包完成后裁剪 Electron 运行时中用不到的文件，
// 减小安装包与安装后体积。仅影响 win-unpacked 产物，不影响运行逻辑。
//
// 注意：裁剪只是"顺带优化"，不是构建的必要条件。所以每个删除都单独兜住异常：
//   - 文件被占用、只读、被上层安全策略（批量删除保护之类）拦下……
//     都不该让整个打包失败 —— 最坏情况只是安装包大一点。
//   - 被跳过的数量会打印出来，便于判断产物是否如预期地瘦过身。

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

function safeRemove(file, skipped) {
  try {
    fs.rmSync(file, { force: true });
    return true;
  } catch (e) {
    skipped.push(`${path.basename(file)}（${(e && e.message) || e}）`);
    return false;
  }
}

module.exports = async function afterPack(context) {
  const dir = context.appOutDir;
  const skipped = [];
  let removed = 0;

  // 1) 语言包：仅保留中英文
  const locales = path.join(dir, 'locales');
  if (fs.existsSync(locales)) {
    for (const f of fs.readdirSync(locales)) {
      if (KEEP_LOCALES.has(f)) continue;
      if (safeRemove(path.join(locales, f), skipped)) removed += 1;
    }
  }

  // 2) 用不到的图形后端 DLL
  for (const f of REMOVE_DLLS) {
    const p = path.join(dir, f);
    if (fs.existsSync(p) && safeRemove(p, skipped)) removed += 1;
  }

  // 3) 许可文本（默认保留）
  if (REMOVE_LICENSES) {
    const lic = path.join(dir, 'LICENSES.chromium.html');
    if (fs.existsSync(lic) && safeRemove(lic, skipped)) removed += 1;
  }

  console.log(`[afterPack] 已裁剪 ${removed} 个文件`);
  if (skipped.length) {
    console.warn(`[afterPack] 跳过 ${skipped.length} 个文件（打包仍会继续，安装包会偏大）：`);
    skipped.slice(0, 5).forEach((s) => console.warn(`  - ${s}`));
    if (skipped.length > 5) console.warn(`  … 其余 ${skipped.length - 5} 个同类`);
  }
};
