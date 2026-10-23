//! [20260922] Ctrl+Alt+Del（SAS）能力实测探针。
//!
//! ## 为什么要实测而不是推断
//! Microsoft 文档规定 `SendSAS` 仅在满足以下**之一**时才真正触发安全桌面：
//!   1. 进程运行在 SYSTEM 会话（Windows 服务）；
//!   2. 进程持有 `SeTcbPrivilege`；
//!   3. 进程由 `SAS` 屏幕保护策略授权。
//! 否则 `SendSAS` **静默失败**（不报错、不弹任何东西）。
//!
//! InDesk 壳的安装模式是 `installMode: currentUser`（见 tauri.conf.json），
//! 即以普通用户权限运行 —— 理论上发不出 SAS。但"理论上"不算数，
//! 这里**真的调一次**，并把进程权限上下文打印出来，给出可复核的结论。
//!
//! 运行：cargo run --example sas_probe

use std::process::Command;

#[cfg(windows)]
fn check_privilege(name: &str) -> Option<u32> {
    // 用 whoami /priv 查当前进程令牌权限（比写一堆 LookupPrivilegeValue 更快出结论）
    let out = Command::new("whoami").args(["/priv"]).output().ok()?;
    let text = String::from_utf8_lossy(&out.stdout).to_string();
    for line in text.lines() {
        if line.to_lowercase().contains(&name.to_lowercase()) {
            // 返回该行的状态描述长度（用于打印）
            return Some(line.len() as u32);
        }
    }
    None
}

#[cfg(windows)]
fn has_privilege_enabled(name: &str) -> bool {
    let out = match Command::new("whoami").args(["/priv"]).output() {
        Ok(o) => o,
        Err(_) => return false,
    };
    let text = String::from_utf8_lossy(&out.stdout).to_string();
    for line in text.lines() {
        let l = line.to_lowercase();
        if l.contains(&name.to_lowercase()) {
            // whoami /priv 里"已启用/Enabled"才代表真正持有
            return l.contains("enabled") && !l.contains("disabled");
        }
    }
    false
}

fn main() {
    println!("=== InDesk Ctrl+Alt+Del (SAS) 能力实测 ===");
    println!();

    #[cfg(windows)]
    {
        // 1. 进程身份
        let user = std::env::var("USERNAME").unwrap_or_else(|_| "?".into());
        println!("[进程] 当前用户: {user}");

        // 2. 是否管理员（SAS 的一个必要条件线索）
        let admin = Command::new("net")
            .args(["session"])
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false);
        println!("[进程] 提权到管理员: {admin}");

        // 3. SeTcbPrivilege（SAS 密钥权限）
        let tcb = has_privilege_enabled("SeTcbPrivilege");
        println!("[令牌] SeTcbPrivilege 已启用: {tcb}");

        // 4. 看 sas.dll 是否存在 + SendSAS 能否解析出来
        let dll_ok = std::path::Path::new(r"C:\Windows\System32\sas.dll").exists();
        println!("[系统] C:\\Windows\\System32\\sas.dll 存在: {dll_ok}");

        // 5. 真的调用一次（用与 input.rs 完全相同的路径）
        println!();
        println!("[调用] 正在调用 SendSAS(FALSE) ...");
        let r = indesk_desktop_lib_send_sas();
        match r {
            Ok(()) => println!("[结果] SendSAS 调用返回成功（注意：这不等于系统真的弹出了安全桌面）"),
            Err(e) => println!("[结果] SendSAS 调用失败: {e}"),
        }

        println!();
        println!("=== 结论判定 ===");
        if !tcb && !admin {
            println!("判定：**不可用**。进程既非管理员、也无 SeTcbPrivilege，");
            println!("      SendSAS 会被系统静默忽略（不弹安全桌面）。");
            println!("      → 顶部工具栏应**隐藏** Ctrl+Alt+Del 按钮，或改为明确提示不可用。");
        } else {
            println!("判定：**可能可用**（持有提权上下文）。请人工确认安全桌面是否真的弹出。");
        }
    }

    #[cfg(not(windows))]
    println!("非 Windows，跳过。");
}

/// 复制 input.rs 里的 SendSAS 调用（example 不能直接引 lib 的私有模块，
/// 这里用同一份实现保证测的就是上线代码的语义）。
#[cfg(windows)]
fn indesk_desktop_lib_send_sas() -> Result<(), String> {
    use windows_sys::Win32::System::LibraryLoader::{GetProcAddress, LoadLibraryW};
    unsafe {
        let name: Vec<u16> = "sas.dll\0".encode_utf16().collect();
        let h = LoadLibraryW(name.as_ptr());
        if h.is_null() {
            return Err("无法加载 sas.dll".into());
        }
        type SendSasFn = unsafe extern "system" fn(i32);
        let addr = GetProcAddress(h, b"SendSAS\0".as_ptr());
        let f: SendSasFn = match addr {
            Some(p) => unsafe { std::mem::transmute(p) },
            None => return Err("sas.dll 中未找到 SendSAS 导出".into()),
        };
        f(0);
    }
    Ok(())
}
