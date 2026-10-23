//! [1.4.38 隐私屏 v2] 被控端**本机物理屏幕**黑化（原生遮罩窗）。
//!
//! ## 语义（2026-10-02 定稿，取代 1.4.37 的"发布黑化"）
//!
//! 旧实现（1.4.37）把**发布给主控端的帧**整帧换成纯黑——主控端看到黑屏、
//! 被控端显示器毫无变化，语义完全反了，用户实测后判定"功能不对"。
//! 正确的隐私屏 = **被控端本机黑屏**（旁观者看不到内容）+ **主控端照常观看**
//! （抓屏链路零改动、帧照常发布）。
//!
//! ## 实现原理
//!
//! 每台显示器盖一个全屏黑色置顶窗，并对其调用
//! `SetWindowDisplayAffinity(WDA_EXCLUDEFROMCAPTURE)`：
//!
//! - **本机可见**：物理屏幕上就是一层不透明纯黑，旁观者什么都看不到；
//! - **抓屏不可见**：被排除的窗口不会出现在 DXGI/GDI 抓屏结果里，
//!   抓到的是遮罩窗"底下"的真实桌面 ⇒ 主控端画面完全正常；
//! - **鼠标穿透**（WS_EX_TRANSPARENT）：不挡被控端本机的键鼠操作，
//!   远程会话的输入注入不受影响；
//! - **不抢焦点**（WS_EX_NOACTIVATE + SW_SHOWNOACTIVATE）：不打断本机
//!   正在使用的窗口焦点。
//!
//! Windows 版本要求：`WDA_EXCLUDEFROMCAPTURE` 需要 Win10 2004 (build 19041)+。
//! 更老的系统上 `SetWindowDisplayAffinity` 会失败 ⇒ 遮罩窗会把主控端画面也
//! 盖黑 ⇒ 此时**拒绝开启**（返回 false），宁可功能不可用也不能把远端画面搞黑。
//!
//! ## 线程模型
//!
//! Win32 窗口必须在创建它的线程上跑消息泵。这里用一条**常驻 worker 线程**
//! 独占所有遮罩窗：开关指令经 mpsc 送达、带 ack 回执（5s 超时兜底）。
//! worker 每轮 `recv_timeout(200ms)` 醒来泵一次消息（PeekMessage + Dispatch），
//! 保证窗口消息循环不卡死。
//!
//! ## 安全兜底（不回归 1.4.37 的教训）
//!
//! 1. 开启失败（窗口创建/affinity 设置任一步失败）⇒ **立即销毁已建窗口并
//!    返回 false**，绝不留下"半黑"状态；
//! 2. 被控端本地逃生横幅（remote/index.vue `handleLocalPrivacyOff`）走同一
//!    `set_privacy_screen(false)` 通路，本机用户随时可解除；
//! 3. 发布层（`capture::publish/publish_raw`）**不再碰帧内容**——真实画面
//!    照常发布，帧心跳/自愈判据零风险。

#![allow(clippy::upper_case_acronyms)]

#[cfg(windows)]
mod imp {
    use std::sync::mpsc;
    use std::time::Duration;

    // windows-sys 0.59 的 HWND/HMONITOR/HDC/HBRUSH 都是 `*mut c_void`。
    use windows_sys::Win32::Foundation::{HWND, LPARAM, LRESULT, RECT, WPARAM};
    use windows_sys::Win32::Graphics::Gdi::{
        CreateSolidBrush, EnumDisplayMonitors, HMONITOR, MONITORENUMPROC,
    };
    use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW, PeekMessageW,
        RegisterClassW, SetLayeredWindowAttributes, SetWindowDisplayAffinity, SetWindowPos,
        ShowWindow, TranslateMessage, MSG, PM_REMOVE, SET_WINDOW_POS_FLAGS,
        SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOSIZE, SWP_SHOWWINDOW, SW_SHOWNOACTIVATE, WDA_EXCLUDEFROMCAPTURE,
        WINDOW_EX_STYLE, WINDOW_STYLE, WM_CLOSE, WNDCLASSW, WS_EX_LAYERED,
        WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW, WS_EX_TOPMOST, WS_EX_TRANSPARENT, WS_POPUP,
        HWND_TOPMOST, LWA_ALPHA,
    };

    /// 遮罩窗窗口类名（排查时用 FindWindowW 可直接找到）。
    const CLASS_NAME: &[u16] = &[
        b'I' as u16, b'n' as u16, b'D' as u16, b'e' as u16, b's' as u16, b'k' as u16,
        b'P' as u16, b'r' as u16, b'i' as u16, b'v' as u16, b'a' as u16, b'c' as u16,
        b'y' as u16, b'O' as u16, b'v' as u16, b'e' as u16, b'r' as u16, b'l' as u16,
        b'a' as u16, b'y' as u16, 0,
    ];

    enum Cmd {
        /// 开/关。ack 回 true = 遮罩窗已按目标状态就位。
        Set(bool, mpsc::Sender<bool>),
    }

    static TX: std::sync::OnceLock<mpsc::Sender<Cmd>> = std::sync::OnceLock::new();

    /// 对外入口：设置隐私屏遮罩。返回最终状态（true = 已开启）。
    pub fn set(on: bool) -> bool {
        let tx = TX.get_or_init(spawn_worker);
        let (ack, rx) = mpsc::channel();
        if tx.send(Cmd::Set(on, ack)).is_err() {
            // worker 已死（理论上不该发生）——按失败处理。
            return false;
        }
        rx.recv_timeout(Duration::from_secs(5)).unwrap_or(false)
    }

    fn spawn_worker() -> mpsc::Sender<Cmd> {
        let (tx, rx) = mpsc::channel::<Cmd>();
        std::thread::Builder::new()
            .name("privacy-overlay".into())
            .spawn(move || worker_loop(rx))
            .expect("spawn privacy-overlay worker");
        tx
    }

    /// worker 主循环：消息泵 + 指令处理。
    fn worker_loop(rx: mpsc::Receiver<Cmd>) {
        unsafe {
            register_class();
        }
        let mut overlays: Vec<isize> = Vec::new();
        loop {
            match rx.recv_timeout(Duration::from_millis(200)) {
                Ok(Cmd::Set(on, ack)) => {
                    let ok = if on {
                        // 幂等：先清旧再建新，规避显示器拓扑变化。
                        destroy_all(&mut overlays);
                        match create_all() {
                            Ok(list) => {
                                overlays = list;
                                !overlays.is_empty()
                            }
                            Err(()) => {
                                destroy_all(&mut overlays);
                                false
                            }
                        }
                    } else {
                        destroy_all(&mut overlays);
                        true
                    };
                    let _ = ack.send(ok);
                }
                Err(mpsc::RecvTimeoutError::Timeout) => { /* 醒来只为泵消息 */ }
                Err(mpsc::RecvTimeoutError::Disconnected) => return,
            }
            pump_messages();
        }
    }

    unsafe fn register_class() {
        let hinstance = GetModuleHandleW(std::ptr::null());
        let black = CreateSolidBrush(0x0000_0000); // 纯黑背景
        let wc = WNDCLASSW {
            style: 0,
            lpfnWndProc: Some(wnd_proc),
            cbClsExtra: 0,
            cbWndExtra: 0,
            hInstance: hinstance,
            hIcon: std::ptr::null_mut(),
            hCursor: std::ptr::null_mut(),
            hbrBackground: black,
            lpszMenuName: std::ptr::null(),
            lpszClassName: CLASS_NAME.as_ptr(),
        };
        RegisterClassW(&wc);
    }

    unsafe extern "system" fn wnd_proc(
        hwnd: HWND,
        msg: u32,
        wparam: WPARAM,
        lparam: LPARAM,
    ) -> LRESULT {
        if msg == WM_CLOSE {
            DestroyWindow(hwnd);
            return 0;
        }
        DefWindowProcW(hwnd, msg, wparam, lparam)
    }

    fn pump_messages() {
        unsafe {
            let mut msg: MSG = std::mem::zeroed();
            while PeekMessageW(&mut msg, std::ptr::null_mut(), 0, 0, PM_REMOVE) != 0 {
                TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
        }
    }

    /// 枚举所有显示器并逐台创建遮罩窗。任一台失败 ⇒ 全部销毁并报错。
    fn create_all() -> Result<Vec<isize>, ()> {
        unsafe { create_all_impl() }
    }

    unsafe fn create_all_impl() -> Result<Vec<isize>, ()> {
        let mut rects: Vec<RECT> = Vec::new();
        // EnumDisplayMonitors 回调收集每台显示器的完整矩形（含负坐标副屏）。
        unsafe extern "system" fn monitor_cb(
            _hmonitor: HMONITOR,
            _hdc: windows_sys::Win32::Graphics::Gdi::HDC,
            rect: *mut RECT,
            lparam: LPARAM,
        ) -> i32 {
            let out = &mut *(lparam as *mut Vec<RECT>);
            out.push(*rect);
            1 // 继续枚举
        }
        if EnumDisplayMonitors(
            std::ptr::null_mut(),
            std::ptr::null(),
            Some(monitor_cb),
            std::ptr::addr_of_mut!(rects) as isize,
        ) == 0
            || rects.is_empty()
        {
            return Err(());
        }
        let hinstance = GetModuleHandleW(std::ptr::null());
        let mut made: Vec<isize> = Vec::new();
        for r in &rects {
            let hwnd = CreateWindowExW(
                (WS_EX_LAYERED
                    | WS_EX_TRANSPARENT
                    | WS_EX_TOPMOST
                    | WS_EX_TOOLWINDOW
                    | WS_EX_NOACTIVATE) as WINDOW_EX_STYLE,
                CLASS_NAME.as_ptr(),
                std::ptr::null(),
                WS_POPUP as WINDOW_STYLE,
                r.left,
                r.top,
                r.right - r.left,
                r.bottom - r.top,
                std::ptr::null_mut(), // 父窗
                std::ptr::null_mut(), // 菜单
                hinstance,
                std::ptr::null(),
            );
            if hwnd.is_null() {
                destroy_handles(&made);
                return Err(());
            }
            // 不透明（alpha=255；底色已是黑刷子，这步只是确保不透明）。
            SetLayeredWindowAttributes(hwnd, 0, 255, LWA_ALPHA);
            // 关键一步：从抓屏中排除自己（Win10 2004+）。失败 ⇒ 拒绝开启，
            // 否则遮罩会把主控端看到的画面也盖黑（回到 1.4.37 的反面事故）。
            if SetWindowDisplayAffinity(hwnd, WDA_EXCLUDEFROMCAPTURE) == 0 {
                DestroyWindow(hwnd);
                destroy_handles(&made);
                return Err(());
            }
            ShowWindow(hwnd, SW_SHOWNOACTIVATE);
            // 置顶一次（不改变大小位置、不抢焦点）。
            let flags: SET_WINDOW_POS_FLAGS =
                SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE | SWP_SHOWWINDOW;
            SetWindowPos(hwnd, HWND_TOPMOST, 0, 0, 0, 0, flags);
            made.push(hwnd as isize);
        }
        Ok(made)
    }

    fn destroy_all(overlays: &mut Vec<isize>) {
        unsafe {
            for h in overlays.drain(..) {
                DestroyWindow(h as HWND);
            }
        }
    }

    unsafe fn destroy_handles(handles: &[isize]) {
        for h in handles {
            DestroyWindow(*h as HWND);
        }
    }
}

/// 非 Windows 平台：隐私屏不支持（与 capture 的非 Windows stub 对齐）。
#[cfg(not(windows))]
mod imp {
    pub fn set(_on: bool) -> bool {
        false
    }
}

pub use imp::set;
