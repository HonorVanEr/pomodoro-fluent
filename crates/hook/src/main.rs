//! pomodoro-hook —— 宿主 hook CLI。
//!
//! **本 crate 绝对不能加 `windows_subsystem = "windows"`**：hook 的输出是决策
//! JSON，必须可靠写进 stdout。GUI subsystem 的进程在部分宿主/终端组合下拿不到
//! console，写不出 stdout 就等于弹窗白等一整轮超时。
//!
//! M0 只占位：不连网关、直接放行，保证「应用没跑时绝不阻断 agent」这条底线不破。

fn main() {
    let _ = pomodoro_core::gateway_file();
    // 宿主读 stdout 拿决策；M3 接入真实网关长轮询后这里改成真实结果。
    println!("{{\"continue\":true}}");
}
