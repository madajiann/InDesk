//! 注入一次"可信用户手势"（Windows / SendInput）。
//!
//! 背景：`navigator.mediaDevices.getDisplayMedia()` 按规范**必须有 transient user
//! activation**——也就是由真实用户交互（可信事件）产生的"用户手势"。被控机处于
//! 无人值守状态时（开机自启后机器前没人）不存在任何用户交互，取屏会被浏览器直接拒绝，
//! 这正是"重启以后没人点允许、主控连不上"的最后一环。
//!
//! 解决方式：由壳在本程序**自己的窗口**上注入一次真实鼠标点击（`SendInput` 产生的是
//! isTrusted 可信事件），把缺失的"用户手势"补上；前端随即自动重试取屏。
//! 全程不需要任何人在机器前操作。
//!
//! 安全性：
//! - 点击位置固定在本程序窗口顶部标题栏的**正中偏上空白处**（所有按钮都在右上角，避得开），
//!   单击不移动鼠标，不会触发窗口拖动，也不会命中任何按钮；
//! - 点击前确保窗口可见、未最小化，点击后立即把光标移回原位；
//! - 只在「无人值守 + 取屏失败」时才被调用（有人值守时走系统弹窗，根本不需要它）。

use serde_json::{json, Value};

#[cfg(windows)]
pub fn nudge(app: &tauri::AppHandle) -> Result<Value, String> {
    use std::mem::size_of;
    use std::time::Duration;

    use tauri::Manager;
    use windows_sys::Win32::Foundation::{POINT, RECT};
    use windows_sys::Win32::UI::Input::KeyboardAndMouse::{
        SendInput, INPUT, INPUT_MOUSE, MOUSEEVENTF_LEFTDOWN, MOUSEEVENTF_LEFTUP, MOUSEINPUT,
    };
    // ⚠️ windows-sys 0.59 里 SetCursorPos / GetCursorPos 归属 WindowsAndMessaging，
    // **不在** Input::KeyboardAndMouse（这里犯过一次 E0432）
    use windows_sys::Win32::UI::WindowsAndMessaging::{GetCursorPos, GetWindowRect, SetCursorPos};

    let win = app
        .get_webview_window("main")
        .ok_or_else(|| "主窗口不存在".to_string())?;
    // 窗口必须在屏幕上：隐藏或最小化时点击落不到它身上，也就产生不了手势
    let _ = win.show();
    let _ = win.unminimize();
    std::thread::sleep(Duration::from_millis(180));

    let hwnd = win
        .hwnd()
        .map_err(|e| format!("取窗口句柄失败: {e}"))?
        .0;
    let mut rect = RECT {
        left: 0,
        top: 0,
        right: 0,
        bottom: 0,
    };
    unsafe {
        if GetWindowRect(hwnd, &mut rect) == 0 {
            return Err("GetWindowRect 失败".into());
        }
    }

    // 标题栏（自绘，高 50px）正中偏上：距顶部 14px、水平居中 —— 空白区，不会命中按钮
    let cx = rect.left + (rect.right - rect.left) / 2;
    let cy = rect.top + 14;

    let mut old = POINT { x: 0, y: 0 };
    unsafe {
        GetCursorPos(&mut old);
        SetCursorPos(cx, cy);
    }
    std::thread::sleep(Duration::from_millis(60));

    let sent = unsafe {
        let mut inputs: [INPUT; 2] = std::mem::zeroed();
        inputs[0].r#type = INPUT_MOUSE;
        inputs[0].Anonymous.mi = MOUSEINPUT {
            dx: 0,
            dy: 0,
            mouseData: 0,
            dwFlags: MOUSEEVENTF_LEFTDOWN,
            time: 0,
            dwExtraInfo: 0,
        };
        inputs[1].r#type = INPUT_MOUSE;
        inputs[1].Anonymous.mi = MOUSEINPUT {
            dx: 0,
            dy: 0,
            mouseData: 0,
            dwFlags: MOUSEEVENTF_LEFTUP,
            time: 0,
            dwExtraInfo: 0,
        };
        SendInput(2, inputs.as_ptr(), size_of::<INPUT>() as i32)
    };

    std::thread::sleep(Duration::from_millis(60));
    unsafe {
        SetCursorPos(old.x, old.y);
    }

    if sent == 0 {
        return Err("SendInput 被系统拦截（可能被安全软件限制）".into());
    }
    Ok(json!({ "ok": true, "sent": sent }))
}

#[cfg(not(windows))]
pub fn nudge(_app: &tauri::AppHandle) -> Result<Value, String> {
    Err("当前平台不支持手势兜底".into())
}
