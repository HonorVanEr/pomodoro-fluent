# ============================================================
# 番茄钟 · 启用 Win10/11 Acrylic 毛玻璃
# 用法: powershell -ExecutionPolicy Bypass -File apply-acrylic.ps1 -Hwnd <十进制句柄> [-Tint "R,G,B,A"]
# 通过 Win32 SetWindowCompositionAttribute 设置 ACCENT_ENABLE_ACRYLICBLURBEHIND
# 全部互操作在 C# 内完成，PowerShell 只传参数（最稳定）
# ============================================================
param(
  [Parameter(Mandatory=$true)][int64]$Hwnd,
  [string]$Tint = "30,32,42",
  [double]$TintOpacity = 0.35,
  # CSS 圆角半径（px）。>0 时用 SetWindowRgn 把窗口裁成圆角矩形，
  # 避免 DWM 毛玻璃在 CSS 圆角外露出方角
  [double]$CornerRadius = 0
)

$ErrorActionPreference = 'Stop'

if ($Hwnd -eq 0) { Write-Output "INVALID_HWND"; exit 1 }

# 加载 Win32 API（P/Invoke，含应用函数）
Add-Type -TypeDefinition @"
using System;
using System.Runtime.InteropServices;

public static class AcrylicBlurApi {
    [StructLayout(LayoutKind.Sequential)]
    private struct AccentPolicy {
        public int AccentState;
        public int AccentFlags;
        public int GradientColor;
        public int AnimationId;
    }

    [StructLayout(LayoutKind.Sequential)]
    private struct WindowCompositionAttributeData {
        public int Attribute;
        public IntPtr Data;
        public int SizeOfData;
    }

    [DllImport("user32.dll")]
    private static extern int SetWindowCompositionAttribute(IntPtr hwnd, ref WindowCompositionAttributeData data);

        [DllImport("user32.dll")]
        private static extern bool IsWindow(IntPtr hwnd);

        [DllImport("user32.dll")]
        private static extern bool GetWindowRect(IntPtr hwnd, out RECT rect);
        [DllImport("user32.dll")]
        private static extern bool SetWindowRgn(IntPtr hwnd, IntPtr hRgn, bool bRedraw);
        [DllImport("user32.dll")]
        private static extern uint GetDpiForWindow(IntPtr hwnd);
        [DllImport("gdi32.dll")]
        private static extern IntPtr CreateRoundRectRgn(int x1, int y1, int x2, int y2, int cw, int ch);

        [StructLayout(LayoutKind.Sequential)]
        private struct RECT {
            public int Left;
            public int Top;
            public int Right;
            public int Bottom;
        }

    public static string Apply(IntPtr hwnd, int gradientColor) {
        if (!IsWindow(hwnd)) return "NOT_A_WINDOW";
        AccentPolicy accent = new AccentPolicy();
        accent.AccentState = 4;   // ACCENT_ENABLE_ACRYLICBLURBEHIND
        accent.AccentFlags = 2;
        accent.GradientColor = gradientColor;
        accent.AnimationId = 0;

        WindowCompositionAttributeData data = new WindowCompositionAttributeData();
        data.Attribute = 19;      // WCA_ACCENT_POLICY
        data.SizeOfData = Marshal.SizeOf(typeof(AccentPolicy));
        IntPtr dataPtr = Marshal.AllocHGlobal(data.SizeOfData);
        try {
            Marshal.StructureToPtr(accent, dataPtr, false);
            data.Data = dataPtr;
            int result = SetWindowCompositionAttribute(hwnd, ref data);
            return result != 0 ? "OK" : "FAILED";
        } finally {
            Marshal.FreeHGlobal(dataPtr);
        }
    }

    public static string RoundCorners(IntPtr hwnd, double cssRadius) {
        RECT r;
        if (!GetWindowRect(hwnd, out r)) return "NOT_A_WINDOW";
        uint dpi = GetDpiForWindow(hwnd);
        if (dpi == 0) dpi = 96;
        int rad = (int)Math.Round(cssRadius * dpi / 96.0);
        if (rad < 1) return "OK";
        IntPtr hRgn = CreateRoundRectRgn(0, 0, r.Right - r.Left + 1, r.Bottom - r.Top + 1, rad, rad);
        if (hRgn == IntPtr.Zero) return "FAILED";
        // 成功后区域归系统所有，不需要 DeleteObject
        return SetWindowRgn(hwnd, hRgn, true) ? "OK" : "FAILED";
    }
}
"@

$hwndPtr = [IntPtr]$Hwnd

# ACCENT_ENABLE_ACRYLICBLURBEHIND = 4, GradientColor 格式: 0xAABBGGRR
$rgb = $Tint -split ','
$r = [int]$rgb[0]
$g = [int]$rgb[1]
$b = [int]$rgb[2]
$a = [int](255 * $TintOpacity)
$gradient = ($a -shl 24) -bor ($b -shl 16) -bor ($g -shl 8) -bor $r

$result = [AcrylicBlurApi]::Apply($hwndPtr, $gradient)
Write-Output $result
if ($result -ne "OK") { exit 3 }

if ($CornerRadius -gt 0) {
  $round = [AcrylicBlurApi]::RoundCorners($hwndPtr, $CornerRadius)
  if ($round -ne "OK") { Write-Output "ROUND_$round"; exit 4 }
}
