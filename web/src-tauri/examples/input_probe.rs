//! [1.4.39 输入锁探针] 直接驱动 input_lock，供真机验证三件事：
//! 1. set(true)/set(false) 全部成功（钩子装得上、卸得下）；
//! 2. 锁定期**物理**键鼠被吞：窗口期敲键盘/晃鼠标，结束后
//!    BLOCKED_KEY / BLOCKED_MOUSE 计数应 > 0；
//! 3. 锁定期**注入**输入放行：探针在锁定期 SendInput 一个 F15
//!    （注入事件带 LLKHF_INJECTED，钩子必须放行——否则远程键鼠会被误杀）。
//!
//! 用法：input_probe on [hold_secs]   锁 N 秒（默认 8）后自动解锁
//!       input_probe off              立即解锁
//!       input_probe state            只查状态（退出码 0=锁着）
//!
//! 退出码：0=成功；1=set(true) 失败；2=set(false) 失败。
//! 安全性：探针异常退出（含被强杀）时钩子由 OS 随进程自动拆除，锁不死人。

#![cfg(windows)]

/// 结束前停住：双击运行时窗口不再闪退，用户能看到全部输出。
/// （与服务器 bat 的 `pause` 同理——没停住的探针就是不合格探针。）
fn pause_before_exit() {
    println!();
    println!("[probe] ==== 按回车键退出 ====");
    use std::io::Read;
    let _ = std::io::stdin().read(&mut [0u8]);
}

fn main() {
    use indesk_desktop_lib::input_lock;
    use std::time::Duration;

    let arg = std::env::args().nth(1).unwrap_or_default();

    // 双击直接运行（无参数）= 默认锁定 10 秒，方便体感验收。
    let arg = if arg.is_empty() { "on".to_string() } else { arg };

    let code = match arg.as_str() {
        "on" => {
            let hold: u64 = std::env::args()
                .nth(2)
                .and_then(|s| s.parse().ok())
                .unwrap_or(10);
            let ok = input_lock::set(true);
            println!("[probe] set(true) -> {} (locked={})", ok, input_lock::locked());
            if !ok {
                pause_before_exit();
                std::process::exit(1);
            }
            println!("[probe] locked {}s —— 现在请敲键盘/晃鼠标（应无反应）...", hold);
            // 注入放行验证：锁定期发一个 F15（VK_F15=0x7E，注入事件不受本锁影响）。
            unsafe {
                // keybd_event(bVk=0x7E, bScan=0, dwFlags=0=按下, 0)
                windows_sys::Win32::UI::Input::KeyboardAndMouse::keybd_event(0x7E, 0, 0, 0);
                windows_sys::Win32::UI::Input::KeyboardAndMouse::keybd_event(
                    0x7E, 0, 2, 0, // KEYEVENTF_KEYUP = 2
                );
            }
            println!("[probe] 注入 F15 已发送（若远程/前台无异常 = 注入路径放行）");
            std::thread::sleep(Duration::from_secs(hold));
            let unlocked = input_lock::set(false);
            println!("[probe] set(false) -> {} (locked={})", unlocked, input_lock::locked());
            println!(
                "[probe] 计数取证：吞物理键 {} 次 / 吞物理鼠标 {} 次",
                input_lock::blocked_keys(),
                input_lock::blocked_mice()
            );
            if input_lock::blocked_keys() + input_lock::blocked_mice() > 0 {
                println!("[probe] ✅ 计数 > 0 = 物理输入确实被吞过，输入锁生效");
            } else {
                println!("[probe] ⚠️ 计数为 0：窗口期没检测到物理输入（敲了吗？）");
            }
            if !unlocked {
                pause_before_exit();
                2
            } else {
                0
            }
        }
        "off" => {
            println!("[probe] set(false) -> {}", input_lock::set(false));
            pause_before_exit();
            0
        }
        "state" => {
            let on = input_lock::locked();
            println!("[probe] locked = {}", on);
            pause_before_exit();
            if on {
                0
            } else {
                3
            }
        }
        _ => {
            println!("usage: input_probe on [secs] | off | state（无参数 = on 10）");
            pause_before_exit();
            0
        }
    };
    std::process::exit(code);
}
