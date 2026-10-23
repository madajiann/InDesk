//! Windows 原生输入注入（鼠标 / 键盘）。
//!
//! 对应原 Electron 主进程里 `@nut-tree-fork/nut-js` 的全部能力。
//! 直接使用 Win32 `SendInput`，不引入额外原生依赖。
//!
//! 按键编号沿用 nut-js 的 `Key` 枚举（0..=133），由 `nut_key_to_vk` 映射到 Windows VK 码，
//! 因此前端代码无需任何改动。

use std::mem::size_of;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;
use std::time::Instant;

use windows_sys::Win32::Foundation::{LPARAM, LRESULT, POINT, RECT, WPARAM};
use windows_sys::Win32::UI::Input::KeyboardAndMouse::{
    GetDoubleClickTime, SendInput, INPUT, INPUT_0, INPUT_KEYBOARD, INPUT_MOUSE, KEYBDINPUT,
    KEYEVENTF_EXTENDEDKEY, KEYEVENTF_KEYUP, KEYEVENTF_UNICODE, MOUSEEVENTF_ABSOLUTE,
    MOUSEEVENTF_HWHEEL, MOUSEEVENTF_LEFTDOWN, MOUSEEVENTF_LEFTUP, MOUSEEVENTF_MOVE,
    MOUSEEVENTF_RIGHTDOWN, MOUSEEVENTF_RIGHTUP, MOUSEEVENTF_WHEEL, MOUSEINPUT,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    GetCursorPos, GetSystemMetrics, SetCursorPos, SM_CXSCREEN, SM_CYSCREEN,
};

// ---------------------------------------------------------------- mouse

/// [20260917 §20] 全屏「输入捕获覆盖层」旁路。
///
/// 根因（本机实测，lab/tools/game_probe7.py）：996 传奇生态在运行时会创建一个
/// 不可见的全屏顶层窗口 `IHWindowClass`（标题 "Input Capture Window"）罩住**整个屏幕**，
/// `WindowFromPoint` 在屏幕任意一点（游戏中心 / 桌面 / 任务栏）命中的都是它 ——
/// 所有注入点击都被它吞掉，表现为「点进游戏后，点什么都没反应」。
/// 该覆盖层按需出现/消失（owning 辅助进程退出后随之消失）。
///
/// 修复：点击类操作前若检测到该覆盖层，`ShowWindow(SW_HIDE)` 临时隐藏，
/// 让点击落到真实目标窗口；抬起后 `SW_SHOWNA` 还原（对覆盖层所有者零破坏，
/// 本机已验证 hide→命中恢复→show 还原链路可行）。
static HIDDEN_OVERLAY: Mutex<Option<isize>> = Mutex::new(None);

/// pub 仅为 overlay_probe（#[path] 行为级取证）可见；产品内只有本模块调用。
pub fn detect_and_hide_overlay() -> Option<isize> {
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        GetClassNameW, GetWindowRect, IsWindowVisible, ShowWindow, SW_HIDE, WindowFromPoint,
    };
    unsafe {
        let mut pt = POINT { x: 0, y: 0 };
        if GetCursorPos(&mut pt) == 0 {
            return None;
        }
        let hit = WindowFromPoint(pt);
        if hit.is_null() || IsWindowVisible(hit) == 0 {
            return None;
        }
        let mut buf = [0u16; 64];
        let n = GetClassNameW(hit, buf.as_mut_ptr(), 64);
        if n <= 0 {
            return None;
        }
        if String::from_utf16_lossy(&buf[..n as usize]) != "IHWindowClass" {
            return None;
        }
        let mut rc = RECT { left: 0, top: 0, right: 0, bottom: 0 };
        if GetWindowRect(hit, &mut rc) == 0 {
            return None;
        }
        let (w, h) = screen_size();
        // 必须罩满整个主屏才判定为输入捕获层（防误伤同名普通窗口）
        if !(rc.left <= 0 && rc.top <= 0 && rc.right >= w && rc.bottom >= h) {
            return None;
        }
        // [20260917 §36] IHWindowClass（"Input Capture Window"）其实是 **mstsc 远程桌面**
        // 的输入捕获窗口 —— 铁证（22:44 黑匣子）：hit 与 fg 同为 pid11072，fg 类名
        // TscShellContainerClass、标题「122.51.143.79 - 远程桌面连接」。此前把它当成
        // 「996 游戏输入捕获层」在每次点击前 SW_HIDE：全屏 RDP 会话窗口被藏掉 →
        // 点击落到它底下的本地桌面 → 用户视角「点什么都没反应」，前台还被强切到
        // 看不见的窗口。修复：同一进程存在 TscShellContainerClass（mstsc 主容器）时
        // 判定为 RDP 的捕获窗，绝不隐藏。
        {
            use windows_sys::Win32::UI::WindowsAndMessaging::{
                FindWindowW, GetWindowThreadProcessId,
            };
            let tsc: Vec<u16> = "TscShellContainerClass\0".encode_utf16().collect();
            let tsc_hwnd = FindWindowW(tsc.as_ptr(), std::ptr::null());
            if !tsc_hwnd.is_null() {
                let mut tsc_pid: u32 = 0;
                let mut hit_pid: u32 = 0;
                GetWindowThreadProcessId(tsc_hwnd, &mut tsc_pid);
                GetWindowThreadProcessId(hit, &mut hit_pid);
                if tsc_pid != 0 && tsc_pid == hit_pid {
                    return None;
                }
            }
        }
        ShowWindow(hit, SW_HIDE);
        Some(hit as isize)
    }
}

fn store_hidden_overlay(h: Option<isize>) {
    *HIDDEN_OVERLAY.lock().unwrap_or_else(|e| e.into_inner()) = h;
}

fn restore_hidden_overlay() {
    use windows_sys::Win32::UI::WindowsAndMessaging::{ShowWindow, SW_SHOWNA};
    if let Some(v) = HIDDEN_OVERLAY.lock().unwrap_or_else(|e| e.into_inner()).take() {
        unsafe {
            ShowWindow(v as *mut core::ffi::c_void, SW_SHOWNA);
        }
    }
}

// ---------------------------------------------------------------- [§21] 注入丢弃检测 + PostMessage 兼容投递

/// [20260917 §21] 攻破「游戏运行时注入全灭」。
///
/// 铁证（本机 attack_battery.py 实验，11:03）：游戏开着时——
///   * 注入**移动**正常（光标精确到位）；
///   * 注入**点击**流经低级钩子链（能看到 injected=true 的 down/up）；
///   * 但点击落在记事本标题栏后**前台纹丝不动**、双击也不最大化
///     → 事件在钩子链下游被保护钩子**整体丢弃**，任何窗口都收不到。
///
/// 破法：钩子只拦「进队列的合成事件」，**拦不住 PostMessage**（直投窗口过程，
/// 不经过钩子链）。因此：
///   1. 用壳自有的**隐形探针窗**做零副作用的丢弃检测（把 4x4 探针窗瞬移到光标下，
///      注入一对点击，看探针窗收没收到 —— 收到 = 注入正常；没收到 = 被丢弃）；
///   2. 检测到丢弃 → 本轮会话切换 **PostMessage 兼容投递**：按下/抬起按当前
///      光标落点直投命中窗口的根窗口，移动仍走 SendInput（移动未被拦，且游戏
///      靠 GetCursorPos/WM_MOUSEMOVE 拿位置）。
/// 兼容态自动翻转：注入恢复（游戏关闭）后 10s 内自动切回 SendInput 路径。
/// 桌面图标双击在兼容态不可用（PostMessage 不触发系统双击合成）——游戏态下无碍。
static PROBE_RECEIVED: AtomicBool = AtomicBool::new(false);
static PROBE_HWND: Mutex<Option<isize>> = Mutex::new(None);
/// (compat_on, checked_at)。ON 时 10s 复查一次（游戏关了自动恢复），OFF 时 30s。
static COMPAT_STATE: Mutex<Option<(bool, Instant)>> = Mutex::new(None);
/// 兼容态下 press 捕获的投递目标（拖拽时按下/抬起必须投给同一窗口）。
static POST_TARGET: Mutex<Option<isize>> = Mutex::new(None);

unsafe extern "system" fn probe_wndproc(
    hwnd: windows_sys::Win32::Foundation::HWND,
    msg: u32,
    wp: WPARAM,
    lp: LPARAM,
) -> LRESULT {
    const WM_LBUTTONDOWN: u32 = 0x0201;
    const WM_LBUTTONUP: u32 = 0x0202;
    if msg == WM_LBUTTONDOWN || msg == WM_LBUTTONUP {
        PROBE_RECEIVED.store(true, Ordering::SeqCst);
        return 0;
    }
    unsafe {
        windows_sys::Win32::UI::WindowsAndMessaging::DefWindowProcW(hwnd, msg, wp, lp)
    }
}

/// 惰性创建 4x4 隐形探针窗（近全透明、不抢焦点、不出任务栏），初始藏在屏外。
fn ensure_probe_window() -> Option<isize> {
    let mut g = PROBE_HWND.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(v) = *g {
        return Some(v);
    }
    use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        CreateWindowExW, RegisterClassW, SetLayeredWindowAttributes, ShowWindow, SW_HIDE,
        WNDCLASSW, WS_EX_LAYERED, WS_EX_NOACTIVATE, WS_EX_TOPMOST, WS_POPUP,
    };
    unsafe {
        let class: Vec<u16> = "IndeskInjectProbe\0".encode_utf16().collect();
        let hinstance = GetModuleHandleW(std::ptr::null());
        let wc = WNDCLASSW {
            style: 0,
            lpfnWndProc: Some(probe_wndproc),
            cbClsExtra: 0,
            cbWndExtra: 0,
            hInstance: hinstance,
            hIcon: std::ptr::null_mut(),
            hCursor: std::ptr::null_mut(),
            hbrBackground: std::ptr::null_mut(),
            lpszMenuName: std::ptr::null(),
            lpszClassName: class.as_ptr(),
        };
        RegisterClassW(&wc);
        let title: Vec<u16> = "IndeskInjectProbe\0".encode_utf16().collect();
        let hwnd = CreateWindowExW(
            WS_EX_TOPMOST | WS_EX_LAYERED | WS_EX_NOACTIVATE,
            class.as_ptr(),
            title.as_ptr(),
            WS_POPUP,
            -2000,
            -2000,
            4,
            4,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            hinstance,
            std::ptr::null(),
        );
        if hwnd.is_null() {
            return None;
        }
        SetLayeredWindowAttributes(hwnd, 0, 2, 0x02); // LWA_ALPHA=2，肉眼不可见但可命中
        ShowWindow(hwnd, SW_HIDE);
        *g = Some(hwnd as isize);
        Some(hwnd as isize)
    }
}

/// 零副作用通道探测：探针窗瞬移到光标下 → 用指定通道注入一对点击 → 看探针窗收到没有。
/// 注入正常时点击被探针窗吞掉（对真实桌面零影响）；被丢弃时同样无副作用。
/// sendinput=true 探测 SendInput 通道；false 探测驱动通道。
fn probe_channel(sendinput: bool) -> bool {
    use windows_sys::Win32::UI::WindowsAndMessaging::MSG;
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        DispatchMessageW, PeekMessageW, SetWindowPos, TranslateMessage,
        HWND_TOPMOST, SWP_NOACTIVATE, SWP_NOSIZE, SWP_SHOWWINDOW,
    };
    let Some(hwnd) = ensure_probe_window() else {
        return true; // 探针都建不出来 → 保守视为通道正常
    };
    unsafe {
        let (cx, cy) = mouse_position();
        SetWindowPos(
            hwnd as *mut core::ffi::c_void,
            -1isize as *mut core::ffi::c_void, // HWND_TOPMOST
            cx - 2,
            cy - 2,
            0,
            0,
            SWP_NOSIZE | SWP_NOACTIVATE | SWP_SHOWWINDOW,
        );
        PROBE_RECEIVED.store(false, Ordering::SeqCst);
        if sendinput {
            send_mouse(MOUSEEVENTF_LEFTDOWN, 0, 0, 0);
            send_mouse(MOUSEEVENTF_LEFTUP, 0, 0, 0);
        } else {
            let ok = crate::interception::mouse_button(true, true);
            std::thread::sleep(std::time::Duration::from_millis(6));
            let _ = crate::interception::mouse_button(true, false);
            if !ok {
                // 驱动投递本身失败 → 该通道不可用
                SetWindowPos(
                    hwnd as *mut core::ffi::c_void,
                    std::ptr::null_mut(),
                    -2000,
                    -2000,
                    0,
                    0,
                    SWP_NOSIZE | SWP_NOACTIVATE,
                );
                return false;
            }
        }
        let deadline = Instant::now() + std::time::Duration::from_millis(40);
        let mut got = false;
        while Instant::now() < deadline {
            let mut msg: MSG = std::mem::zeroed();
            while PeekMessageW(&mut msg, std::ptr::null_mut(), 0, 0, 1) != 0 {
                TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
            if PROBE_RECEIVED.load(Ordering::SeqCst) {
                got = true;
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
        // 探针窗撤回屏外
        SetWindowPos(
            hwnd as *mut core::ffi::c_void,
            std::ptr::null_mut(),
            -2000,
            -2000,
            0,
            0,
            SWP_NOSIZE | SWP_NOACTIVATE,
        );
        got
    }
}

/// [20260917 §36] 点击通道级联探测（结果缓存 10s，恢复后自动回到最优通道）：
///   1 = 驱动注入可达（默认最优：硬件级路径，反作弊无法区分真假鼠标）；
///   0 = 驱动被丢弃但 SendInput 可达（保护钩子只针对驱动设备时）；
///   2 = 两者全被丢弃 → PostMessage 直投窗口过程（钩子链拦不住，但部分自绘
///       UI 不响应窗口消息，故只作最后手段）。
/// 旧 injected_input_dropped() 语义 = 通道 2。
static CHANNEL_STATE: Mutex<Option<(u8, Instant)>> = Mutex::new(None);

pub fn inject_channel() -> u8 {
    // [20260918f no-compat] ⭐ 通道级联探测**整体停用**，恒走 SendInput（通道 0）。
    //
    // 停用理由（用户会话实证 19:57/20:11 黑匣子 + 探针对照）：
    //   级联探测的「SendInput 探针」在有主控实时连接时**系统性误判**——探测窗瞬移到
    //   光标下再注入点击，而真实用户正在远程持续移动光标，点击落点偏离 4x4 探针窗
    //   → 探测失败 → 误判「SendInput 被丢弃」→ 全部注入退化成 PostMessage 直投
    //   （通道 2）。PostMessage 直投对 WebView2/Chrome 内容窗口
    //   （Chrome_RenderWidgetHostHWND，如 NewBody-Up 账号助手）**完全无效**：
    //   ok=true 但消息被浏览器输入管线忽略 → 表现即「有的程序点开以后按钮全点不动」，
    //   且按下走 PostMessage、移动走 SendInput 的错配让**拖窗口全面失效**。
    //   对照：探针（光标静止）期间通道恒为 0，一切正常 —— 与用户体感完全吻合。
    //   该级联本为游戏反作弊环境准备（996 传奇生态吞驱动/SendInput 输入），
    //   用户已明确「游戏不考虑」→ 对齐开源行为，砍掉整个级联，SendInput 常开。
    //   兼容投递/探针代码保留不删（overlay_probe 取证仍引用），仅不再被选中。
    diag_log("inject_channel", "sendinput (no-compat fixed)", true);
    0
}

/// 旧判定（保留语义）：true = 注入点击被系统/保护钩子整体丢弃。
pub fn injected_input_dropped() -> bool {
    inject_channel() == 2
}

fn compat_on() -> bool {
    inject_channel() == 2
}

/// 供行为级取证探针强制清缓存。
pub fn compat_reset() {
    *COMPAT_STATE.lock().unwrap_or_else(|e| e.into_inner()) = None;
    *POST_TARGET.lock().unwrap_or_else(|e| e.into_inner()) = None;
}

/// 兼容投递：按当前光标落点，把左键 down/up 直投命中窗口的根窗口。
/// 返回 true = 投递调用成功（PostMessage 无返回确认，与系统语义一致）。
fn post_left_button(down: bool, remember_target: bool) -> bool {
    use windows_sys::Win32::Graphics::Gdi::ScreenToClient;
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        GetAncestor, PostMessageW, WindowFromPoint, GA_ROOT,
    };
    const WM_LBUTTONDOWN: u32 = 0x0201;
    const WM_LBUTTONUP: u32 = 0x0202;
    const MK_LBUTTON: usize = 0x0001;
    unsafe {
        let mut pt = POINT { x: 0, y: 0 };
        if GetCursorPos(&mut pt) == 0 {
            return false;
        }
        let hit = WindowFromPoint(pt);
        if hit.is_null() {
            return false;
        }
        let root = GetAncestor(hit, GA_ROOT);
        if root.is_null() {
            return false;
        }
        // [20260917 §36] mstsc 主容器不吃窗口消息点击（输入由 RDP 协议层转发，
        // PostMessage 投进去石沉大海）——返回 false 让调用方落回 SendInput。
        {
            use windows_sys::Win32::UI::WindowsAndMessaging::GetClassNameW;
            let mut cn = [0u16; 64];
            let n = GetClassNameW(root, cn.as_mut_ptr(), 64);
            let cls = String::from_utf16_lossy(&cn[..(n.max(0) as usize).min(64)]);
            if cls == "TscShellContainerClass" {
                return false;
            }
        }
        let mut cp = pt;
        ScreenToClient(root, &mut cp);
        let lparam: isize = (((cp.y as u16 as u32) << 16) | (cp.x as u16 as u32)) as isize;
        if down {
            if remember_target {
                *POST_TARGET.lock().unwrap_or_else(|e| e.into_inner()) = Some(root as isize);
            }
            PostMessageW(root, WM_LBUTTONDOWN, MK_LBUTTON, lparam) != 0
        } else {
            // 抬起投给按下时记住的窗口（拖拽语义），没有就投当前命中
            let target = POST_TARGET
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .take()
                .map(|v| v as *mut core::ffi::c_void)
                .unwrap_or(root);
            PostMessageW(target, WM_LBUTTONUP, 0, lparam) != 0
        }
    }
}

fn post_right_button(down: bool) -> bool {
    use windows_sys::Win32::Graphics::Gdi::ScreenToClient;
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        GetAncestor, PostMessageW, WindowFromPoint, GA_ROOT,
    };
    const WM_RBUTTONDOWN: u32 = 0x0204;
    const WM_RBUTTONUP: u32 = 0x0205;
    const MK_RBUTTON: usize = 0x0002;
    unsafe {
        let mut pt = POINT { x: 0, y: 0 };
        if GetCursorPos(&mut pt) == 0 {
            return false;
        }
        let hit = WindowFromPoint(pt);
        if hit.is_null() {
            return false;
        }
        let root = GetAncestor(hit, GA_ROOT);
        if root.is_null() {
            return false;
        }
        // [20260917 §36] 同 post_left_button：mstsc 容器不吃窗口消息点击。
        {
            use windows_sys::Win32::UI::WindowsAndMessaging::GetClassNameW;
            let mut cn = [0u16; 64];
            let n = GetClassNameW(root, cn.as_mut_ptr(), 64);
            let cls = String::from_utf16_lossy(&cn[..(n.max(0) as usize).min(64)]);
            if cls == "TscShellContainerClass" {
                return false;
            }
        }
        let mut cp = pt;
        ScreenToClient(root, &mut cp);
        let lparam: isize = (((cp.y as u16 as u32) << 16) | (cp.x as u16 as u32)) as isize;
        PostMessageW(
            root,
            if down { WM_RBUTTONDOWN } else { WM_RBUTTONUP },
            if down { MK_RBUTTON } else { 0 },
            lparam,
        ) != 0
    }
}

/// [20260917 §19] 双击组装修复：记录上一次左键按下的落点与时刻。
///
/// 根因（本机实测，见 lab/tools/dclick_probe.py）：两次「按下+抬起」之间光标只要
/// 漂移 1~2px，Windows 就不再把它们拼成双击（名义容差 SM_CXDOUBLECLK=4，实际边界更严；
/// 600ms 间隔的负向对照同样失败，证明判别力）。而远程链路里 move 按 rAF 合帧、
/// 坐标经缩放取整，两次点击之间 1~2px 的漂移是常态 → 表现即
/// 「桌面/开始菜单图标怎么点都打不开」（图标靠双击启动），其它单点操作全部正常。
static LAST_LEFT_DOWN: Mutex<Option<(i32, i32, Instant)>> = Mutex::new(None);

fn double_click_window_ms() -> u64 {
    unsafe { GetDoubleClickTime() as u64 }
}

/// 按下前调用：若本次落点与上次左键按下「近在咫尺（≤3px）且间隔在系统双击时限内」，
/// 把光标吸回上次的精确落点再按下 —— 与物理双击零漂移时系统的判定完全一致。
/// 超出 3px 视为正常移动/新操作，绝不吸附（拖动、画图类操作不受影响）。
fn snap_for_double_click() {
    // [20260921 menu-fix] 光标停在菜单等浮层上时不做落点吸附：3px 的吸回足以把高亮
    // 挪到相邻菜单项（菜单项高约 22px、边界很窄），而菜单内也不存在"双击漂移"问题。
    if cursor_over_app_popup() {
        return;
    }
    let last = LAST_LEFT_DOWN
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clone();
    if let Some((lx, ly, t)) = last {
        if t.elapsed().as_millis() as u64 <= double_click_window_ms() {
            let (cx, cy) = mouse_position();
            if (cx - lx).abs() <= 3 && (cy - ly).abs() <= 3 && (cx != lx || cy != ly) {
                mouse_move(lx, ly);
            }
        }
    }
}

/// [1.4.34] 任务栏「一点击就变成拖拽换位」护栏 —— 按下落点在任务栏时记下时刻与落点。
///
/// 现场证据（2026-09-28 14:11，被控端 `input_diag.log`，远端点任务栏上最小化的 InDesk）：
///   press_left at(182,1037) → (176,1035) → (174,1029) → (173,1030) → (166,1032)
///   x 坐标**单调左移 16px** —— 这正是 Win11 任务栏按钮被「拖拽换位」的指纹，
///   与用户报的「一点击鼠标就拖拽着最小化的框左右交换」逐字吻合。
/// 机理：Explorer 的任务栏按钮在 WM_LBUTTONDOWN 之后进入「可能拖拽」态，
///   **按钮按下期间任何一条 WM_MOUSEMOVE 都会把它落实成拖拽换位**。
///   而远端客户端是按帧连流 mousemove 的：光标明明没动也在发，坐标还会 ±1 抖动；
///   我们对每一条都无条件注入 MOUSEEVENTF_MOVE ⇒ 按下→抬起之间的那几十毫秒里
///   必然夹着好几条 move ⇒ **每一次"点击"都被 Explorer 判成拖拽**。
/// 处置：任务栏按下后 400ms 内、且落点 8px 死区内的 move 一律**不注入**（见 `mouse_move`）。
///   真拖拽（位移 > 8px）不受影响；任务栏以外的区域完全不受影响。
static TASKBAR_PRESS: Mutex<Option<((i32, i32), Instant)>> = Mutex::new(None);
static TASKBAR_SWALLOW_LOGGED: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);
/// 死区半径（px）：足以吃掉抖动，又远小于一个任务栏按钮的宽度（≈44px）。
const TASKBAR_DEADZONE_PX: i32 = 8;
/// 死区有效期（ms）：按下→抬起通常 60~150ms，400ms 留足余量。
const TASKBAR_DEADZONE_MS: u128 = 400;

fn record_left_down() {
    let pos = mouse_position();
    if cursor_over_taskbar() {
        let mut g = TASKBAR_PRESS.lock().unwrap_or_else(|e| e.into_inner());
        *g = Some((pos, Instant::now()));
        TASKBAR_SWALLOW_LOGGED.store(false, Ordering::SeqCst);
    }
    let mut g = LAST_LEFT_DOWN.lock().unwrap_or_else(|e| e.into_inner());
    *g = Some((pos.0, pos.1, Instant::now()));
}

fn send_mouse(flags: u32, dx: i32, dy: i32, mouse_data: u32) -> bool {
    let input = INPUT {
        r#type: INPUT_MOUSE,
        Anonymous: INPUT_0 {
            mi: MOUSEINPUT {
                dx,
                dy,
                mouseData: mouse_data,
                dwFlags: flags,
                time: 0,
                dwExtraInfo: 0,
            },
        },
    };
    unsafe {
        // [1.4.35] 返回**真实**结果（SendInput 返回成功写入的输入事件数）。
        // 原实现把返回值丢了、调用方一律 `diag_log(..., true)` 写字面量 true ——
        // 日志里的 `ok=true` 因此**从不代表成功**，多次排查被它带偏（2026-09-28 复盘）。
        SendInput(1, &input, size_of::<INPUT>() as i32) != 0
    }
}

/// 主显示器物理分辨率。
pub fn screen_size() -> (i32, i32) {
    unsafe { (GetSystemMetrics(SM_CXSCREEN), GetSystemMetrics(SM_CYSCREEN)) }
}

/// 把物理像素坐标换算成 SendInput 需要的 0..65535 归一化坐标。
fn normalize(x: i32, y: i32) -> (i32, i32) {
    let (w, h) = screen_size();
    let w = if w > 1 { w - 1 } else { 1 };
    let h = if h > 1 { h - 1 } else { 1 };
    let nx = ((x as f64) * 65535.0 / (w as f64)).round() as i32;
    let ny = ((y as f64) * 65535.0 / (h as f64)).round() as i32;
    (nx.clamp(0, 65535), ny.clamp(0, 65535))
}

/// [§33 → 20261019k 输入提速] mouse_move 瘦身（拖拽秒级迟滞的根因）。
///
/// 旧实现每条移动都做「SendInput → sleep 2ms → GetCursorPos 回读校验」，
/// 且连续 5 次回读"没动"就永久切驱动模式（每条 = 3 轮×(move+6ms sleep)
/// + SetCursorPos ≈ 25ms）。所有输入**串行**过单 worker 线程：拖拽时 62 条/秒
/// 的移动一旦涨到 25ms/条，队列每秒净增 ~1.4s —— 表现正是「左键拖拽反应
/// 半天、点击排在移动队列后面越来越卡、右键慢慢也不好使」（20261019 用户实测，
/// 且视频链路 3-16ms/59fps 完全健康，可排除传输）。
///
/// 回读校验与自动降级是游戏反作弊环境的产物；[1.4.34 no-compat] 已定论
/// 「游戏不考虑、SendInput 恒开」，这里把热路径上的 sleep/回读/自动切换
/// 全部砍掉：SendInput 绝对定位注入即返回（~0.1ms），队列永不积压。
/// 高负载下 2ms 内回读到的还是旧坐标，纯属误判（拖拽最忙时最容易连中
/// 5 次误判 → 恰好在拖拽最需要流畅时切进最慢的驱动路径）。
pub fn mouse_move(x: i32, y: i32) {
    // [1.4.34] ① 任务栏点击护栏：刚在任务栏上按下（400ms 内）、且落点在 8px 死区内
    //    → 这条 move 不注入。否则 Explorer 会把一次点击落实成「任务栏按钮拖拽换位」
    //    （机理与现场证据见 `TASKBAR_PRESS` 的注释）。
    {
        let g = TASKBAR_PRESS.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(((px, py), t)) = *g {
            if t.elapsed().as_millis() <= TASKBAR_DEADZONE_MS
                && (x - px).abs() <= TASKBAR_DEADZONE_PX
                && (y - py).abs() <= TASKBAR_DEADZONE_PX
            {
                if !TASKBAR_SWALLOW_LOGGED.swap(true, Ordering::SeqCst) {
                    diag_log(
                        "taskbar_move_swallowed",
                        &format!("press=({},{}) tried=({},{})", px, py, x, y),
                        true,
                    );
                }
                return;
            }
        }
    }
    // [1.4.34] ② 位置没变就不注入。远端是按帧连流的：光标静止时也会不停发 mousemove，
    //    每一条都被我们变成一条真实的 WM_MOUSEMOVE —— 既白耗，又正是上面那个拖拽事故的推手。
    {
        let (cx0, cy0) = mouse_position();
        if cx0 == x && cy0 == y {
            return;
        }
    }
    let (nx, ny) = normalize(x, y);
    send_mouse(MOUSEEVENTF_MOVE | MOUSEEVENTF_ABSOLUTE, nx, ny, 0);
}

/// 现有 `mouseSetPosition` / `getMousePosition` 用的绝对定位（行为同 move）。
pub fn mouse_set_position(x: i32, y: i32) {
    mouse_move(x, y);
}

pub fn mouse_position() -> (i32, i32) {
    let mut p = POINT { x: 0, y: 0 };
    unsafe {
        GetCursorPos(&mut p);
    }
    (p.x, p.y)
}

/// 按住左键拖动到目标点（对应 nut-js `mouse.drag`）。
pub fn mouse_drag(x: i32, y: i32) {
    // [§34] 拖拽同样必须先隐藏覆盖层 + 拉前台：按下那一下若被别的窗口吃掉，
    // 后面整条拖动（含拖标题栏移动窗口）全都不会发生。
    let ctxs = describe_cursor_target();
    store_hidden_overlay(detect_and_hide_overlay());
    ensure_foreground_at_cursor();
    // [§36] 按通道级联：按下/抬起必须走同一通道同一目标（拖拽语义）。
    let compat = inject_channel() == 2;
    let use_driver = !compat && crate::interception::available();
    if compat {
        // [§21] 拖拽：按下直投当前窗口 → 移动仍走 SendInput（移动未被拦）→ 抬起投回原窗口
        let ok = post_left_button(true, true);
        if ok {
            diag_log("drag_press", &format!("compat {}", ctxs), true);
        } else {
            send_mouse(MOUSEEVENTF_LEFTDOWN, 0, 0, 0);
            diag_log("drag_press", &format!("compat_fallback_sendinput {}", ctxs), true);
        }
    } else if use_driver {
        let ok = crate::interception::mouse_button(true, true);
        diag_log("drag_press", &format!("driver {}", ctxs), ok);
        if !ok {
            send_mouse(MOUSEEVENTF_LEFTDOWN, 0, 0, 0);
            diag_log("drag_press", "driver_fail_fallback_sendinput", true);
        }
    } else {
        send_mouse(MOUSEEVENTF_LEFTDOWN, 0, 0, 0);
        diag_log("drag_press", "sendinput", true);
    }
    mouse_move(x, y);
    if compat {
        let ok = post_left_button(false, false);
        if ok {
            diag_log("drag_release", "compat", true);
        } else {
            send_mouse(MOUSEEVENTF_LEFTUP, 0, 0, 0);
            diag_log("drag_release", "compat_fallback_sendinput", true);
        }
    } else if use_driver {
        let ok_up = crate::interception::mouse_button(true, false);
        diag_log("drag_release", "driver", ok_up);
        if !ok_up {
            send_mouse(MOUSEEVENTF_LEFTUP, 0, 0, 0);
            diag_log("drag_release", "driver_fail_fallback_sendinput", true);
        }
    } else {
        send_mouse(MOUSEEVENTF_LEFTUP, 0, 0, 0);
        diag_log("drag_release", "sendinput", true);
    }
    restore_hidden_overlay();
}

/// [20260921 menu-fix] 光标下的窗口是不是「当前前台应用自己的浮层控件」
/// （右键菜单、下拉列表、工具提示……）。
///
/// 为什么必须问这一句（本机实测复现，见 lab 脚本 `menu_repro.py`：记事本右键菜单点「全选」）：
///   右键弹出菜单后，菜单是**独立顶层 popup** —— `WindowFromPoint` 命中 `#32768`，
///   `GetAncestor(GA_ROOT)` 仍是它自己，而 `GetForegroundWindow()` 还是宿主窗口
///   （实测菜单**没有 owner**、也不抢前台）。于是 `ensure_foreground_at_cursor` 判定
///   「光标下窗口 ≠ 前台」→ 把**菜单窗** `BringWindowToTop + SetForegroundWindow`
///   → 菜单当场被销毁（实测 `IsWindow(菜单) = False`）→ 紧接着注入的左键落在菜单
///   **底下**的窗口上 → 用户视角就是「先点右键出菜单，再点左键什么都不发生」
///   （文件夹、程序、编辑框的右键菜单全中；实测同点位选区不变 = 点了没反应）。
///   对照实测：不动前台直接点 → 菜单存活、左键生效。
///
/// 判据（三条同时成立才算，宁可不拦、不可错拦）：
///   1. 光标下顶层窗口不是前台窗口本身；
///   2. 它与前台窗口**同线程** —— 只有「前台应用自己的」浮层才满足；别的应用的窗口
///      （哪怕被压住）线程不同，仍走原前台化路径，「点被压住的窗口」能力不受影响；
///   3. 外形是浮层：非 `WS_CHILD` 且带 `WS_POPUP`，并且是 `#32768`
///      （菜单/下拉的标准窗口类）或带 `WS_EX_TOOLWINDOW` / `WS_EX_NOACTIVATE`。
///      普通对话框（`#32770`，无这些位）不满足 → 保持原行为：对话框被压住照样能点。
fn cursor_over_app_popup() -> bool {
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        GetAncestor, GetClassNameW, GetForegroundWindow, GetWindowLongW,
        GetWindowThreadProcessId, WindowFromPoint, GA_ROOT, GWL_EXSTYLE, GWL_STYLE,
    };
    const WS_CHILD: u32 = 0x4000_0000;
    const WS_POPUP: u32 = 0x8000_0000;
    const WS_EX_TOOLWINDOW: u32 = 0x0000_0080;
    const WS_EX_NOACTIVATE: u32 = 0x0800_0000;
    unsafe {
        let mut pt = POINT { x: 0, y: 0 };
        if GetCursorPos(&mut pt) == 0 {
            return false;
        }
        let hit = WindowFromPoint(pt);
        if hit.is_null() {
            return false;
        }
        let root = GetAncestor(hit, GA_ROOT);
        if root.is_null() {
            return false;
        }
        let fg = GetForegroundWindow();
        if fg.is_null() || fg == root {
            return false; // 命中前台窗口本身：前台化本来就是空操作
        }
        let root_tid = GetWindowThreadProcessId(root, std::ptr::null_mut());
        let fg_tid = GetWindowThreadProcessId(fg, std::ptr::null_mut());
        if root_tid == 0 || root_tid != fg_tid {
            return false; // 不是前台应用自己的浮层 → 不干预
        }
        let style = GetWindowLongW(root, GWL_STYLE) as u32;
        let ex = GetWindowLongW(root, GWL_EXSTYLE) as u32;
        if style & WS_CHILD != 0 || style & WS_POPUP == 0 {
            return false;
        }
        if ex & (WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE) != 0 {
            return true;
        }
        let mut cn = [0u16; 64];
        let n = GetClassNameW(root, cn.as_mut_ptr(), 64);
        let cls = String::from_utf16_lossy(&cn[..(n.max(0) as usize).min(64)]);
        cls == "#32768"
    }
}

/// [20260927 任务栏] 光标下是「系统外壳 UI」（任务栏 / 开始 / 托盘 / 桌面 /
/// 任务栏缩略图预览）时，按下前**绝不能抢前台**。
///
/// 实测证据（2026-09-27 本机对照实验，同一坐标同一台机器）：
///   A 组：光标移到记事本任务栏按钮 → 直接点击 → IsIconic=true（最小化成功）
///   B 组：同坐标，点击前执行抢前台原动作（前台落到 `Shell_TrayWnd`）
///         → IsIconic=false（**零反应**，与用户报的症状逐字一致）
/// 机理：Explorer 的任务栏按钮点击判定是「被点窗口已是前台 → 最小化；否则 → 激活」。
/// 抢前台把 `Shell_TrayWnd` 顶成前台后，被点应用被判为"不在前台"→ 只走激活分支，
/// 而窗口本来就已经激活 → 表现为**再次点击任务栏按钮毫无反应**。
fn cursor_root_class() -> String {
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        GetAncestor, GetClassNameW, WindowFromPoint, GA_ROOT,
    };
    unsafe {
        let mut pt = POINT { x: 0, y: 0 };
        if GetCursorPos(&mut pt) == 0 {
            return String::new();
        }
        let hit = WindowFromPoint(pt);
        if hit.is_null() {
            return String::new();
        }
        let root = GetAncestor(hit, GA_ROOT);
        if root.is_null() {
            return String::new();
        }
        let mut cn = [0u16; 128];
        let n = GetClassNameW(root, cn.as_mut_ptr(), 128);
        String::from_utf16_lossy(&cn[..(n.max(0) as usize).min(128)])
    }
}

fn cursor_over_shell() -> bool {
    matches!(
        cursor_root_class().as_str(),
        "Shell_TrayWnd"
            | "Shell_SecondaryTrayWnd"
            | "Shell_InputSwitchTopLevelWindow"
            | "NotifyIconOverflowWindow"
            | "TaskListThumbnailWnd"
            | "TopLevelWindowForOverflowXamlIsland"
            | "XamlExplorerHostIslandWindow"
            | "Windows.UI.Core.CoreWindow"
            | "Progman"
            | "WorkerW"
    )
}

/// [20260927b] 光标是否正压在**任务栏**上（区别于桌面 Progman/WorkerW）。
fn cursor_over_taskbar() -> bool {
    matches!(
        cursor_root_class().as_str(),
        "Shell_TrayWnd" | "Shell_SecondaryTrayWnd"
    )
}

/// [20260927b] 「点任务栏上已最小化的程序 → 弹不回来」的处置。
///
/// 实测证据（本机 2026-09-27，同一个任务栏按钮 x≈120–126，同一台机器）：
///   13:40:46 press hit=MSTaskListWClass fg=[Shell_TrayWnd] → 下一次按下 fg=Chrome_WidgetWin_1（**还原成功**）
///   16:00:48~16:00:51 press hit=MSTaskListWClass fg=[null] ×5 → fg 恒为 null、窗口纹丝不动（**连续 5 次零反应**）
/// 两次唯一差别 = 点击前的前台状态。机理：Explorer 要还原+激活被点窗口，必须调用
/// `SetForegroundWindow(被点窗口)`；而系统前台锁只允许「自己就是前台进程」的进程这么做。
/// fg=Shell_TrayWnd 时 Explorer 是前台进程 → 允许 → 还原成功；
/// fg=null（最小化掉最后一个窗口后系统留下的状态）时它不被允许 → 还原被静默拒绝。
/// 处置：把前台权交还任务栏（这就是真实用户点任务栏时系统自己会做的事；
/// 我们因走 SendInput 而持有前台权，所以这一次 `SetForegroundWindow` 能成功），
/// Explorer 随之可以自己完成「还原 + 激活」。
///
/// 只在「当前没有活动窗口」（fg 为 null / 桌面 Progman / WorkerW）时做：
/// fg 已是某个应用窗口时，那是「点活动窗口的按钮 = 最小化」场景，
/// 抢先台会把最小化弄坏 —— 那正是 1.4.28 之前的旧 bug（见 cursor_over_shell 注释）。
fn maybe_hand_foreground_back_on_taskbar_click() {
    if !cursor_over_taskbar() {
        return;
    }
    use windows_sys::Win32::System::Threading::{AttachThreadInput, GetCurrentThreadId};
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        GetClassNameW, GetForegroundWindow, GetWindowThreadProcessId, SetForegroundWindow,
    };
    unsafe {
        let fg = GetForegroundWindow();
        if !fg.is_null() {
            let mut cn = [0u16; 64];
            let n = GetClassNameW(fg, cn.as_mut_ptr(), 64);
            let cls = String::from_utf16_lossy(&cn[..(n.max(0) as usize).min(64)]);
            if cls != "Progman" && cls != "WorkerW" {
                return; // 有活动窗口：不干预，交给「点活动窗口按钮=最小化」原路径
            }
        }
        let mut pt = POINT { x: 0, y: 0 };
        if GetCursorPos(&mut pt) == 0 {
            return;
        }
        let root_cls = cursor_root_class();
        if root_cls != "Shell_TrayWnd" && root_cls != "Shell_SecondaryTrayWnd" {
            return;
        }
        // 光标下的顶层窗口就是任务栏本身（前面已确认类名）→ 直接取它
        use windows_sys::Win32::UI::WindowsAndMessaging::{
            GetAncestor, WindowFromPoint, GA_ROOT,
        };
        let hit = WindowFromPoint(pt);
        if hit.is_null() {
            return;
        }
        let tray = GetAncestor(hit, GA_ROOT);
        if tray.is_null() {
            return;
        }
        let cur = GetCurrentThreadId();
        let fg_tid = if fg.is_null() {
            0
        } else {
            GetWindowThreadProcessId(fg, std::ptr::null_mut())
        };
        let tray_tid = GetWindowThreadProcessId(tray, std::ptr::null_mut());
        let mut attached_fg = false;
        if fg_tid != 0 && fg_tid != cur {
            attached_fg = AttachThreadInput(cur, fg_tid, 1) != 0;
        }
        let attached_tray = tray_tid != 0 && tray_tid != cur && AttachThreadInput(cur, tray_tid, 1) != 0;
        // [1.4.35] 交还之后**回读验证**：SetForegroundWindow 返回 true ≠ 真的换成了前台
        // —— 2026-09-28 的判断就是被这一条误导的（"24 次 ok=true"）。这里回读
        // GetForegroundWindow 把**实际结果**写进日志；没换成就保持 AttachThreadInput
        // 再试最多 3 轮（每轮 ~6ms），失败也不阻塞点击流程。
        let mut ok = SetForegroundWindow(tray) != 0;
        let mut after = GetForegroundWindow();
        if after != tray {
            for _ in 0..3 {
                std::thread::sleep(std::time::Duration::from_millis(6));
                if SetForegroundWindow(tray) != 0 {
                    ok = true;
                }
                after = GetForegroundWindow();
                if after == tray {
                    break;
                }
            }
        }
        if attached_tray {
            AttachThreadInput(cur, tray_tid, 0);
        }
        if attached_fg {
            AttachThreadInput(cur, fg_tid, 0);
        }
        let after_desc = if after.is_null() {
            "null".to_string()
        } else {
            let mut acn = [0u16; 64];
            let an = GetClassNameW(after, acn.as_mut_ptr(), 64).max(0) as usize;
            String::from_utf16_lossy(&acn[..an.min(64)])
        };
        // 黑匣子留痕：下次任务栏复测直接在 input_diag.log 里看这一行 ——
        // 现在能区分「交还成功、Explorer 真的成了前台」与「返回 true 但实际没换」。
        diag_log(
            "fg_handback_tray",
            &format!(
                "idle->tray setfg={} after_fg=[{}] is_tray={}",
                ok,
                after_desc,
                after == tray
            ),
            ok,
        );
    }
}

/// [20260916] 点击前强制前台化。
///
/// 背景（本机实测，996 传奇客户端 `我的天真.ex_`，保护壳）：注入的点击**能到达**
/// 窗口（标题栏右键能弹出系统菜单，说明消息进了窗口过程），但窗口**拒绝被激活**——
/// 客户区连点两次前台始终不变；而这类客户端在前台时不激活状态会忽略游戏内输入，
/// 表现为「远程点游戏完全没反应」。实测 `SetForegroundWindow` 强制激活后点击正常。
///
/// 做法与 ToDesk/向日葵同类：注入**按下**事件前，把光标下的顶层窗口拉到前台。
/// 用 `AttachThreadInput` 挂到前台线程与目标线程上再激活，绕开系统对后台进程
/// 抢前台的锁定；对普通窗口（桌面、记事本等）无副作用——它们本来就会响应点击激活。
///
/// 注意：只在**按下**时调用（move 不调用，保持高频移动零额外开销）。
fn ensure_foreground_at_cursor() {
    // [20260927 任务栏] 光标下是任务栏/开始/托盘/桌面时**绝不抢前台**：抢了会把任务栏
    // 顶成前台，Explorer 便不再执行「再次点击=最小化」（实测对照见 cursor_over_shell 注释）。
    if cursor_over_shell() {
        // [1.4.35] **重新打开「交还前台」** —— 1.4.34 把它关掉，那正是本轮的回归。
        //
        // 1.4.34 关掉的理由：2026-09-28 黑匣子实测它 24 次 `ok=true` 却仍以 `fg=null` 收场，
        //   于是判定"没达成目标、纯干扰"。但那个读数只看了**下一次点击开始时的前台快照**
        //   （`describe_cursor_target` 在注入**前**拍），而 fg=null 本身就极不稳定 ——
        //   这中间任何一次窗口显隐都能把它打回 null ⇒ 该读数**不足以否证**它起过作用。
        // 反向证据（用户口径，权重更高）：
        //   09-27 开着它 ⇒ 用户确认「点任务栏能弹回来」；
        //   09-28 关掉它 ⇒ 用户报「连接上以后，鼠标点啥都不管用」。回归窗口与它完全重合。
        //   09-27 的现场对照也清楚：fg=[Shell_TrayWnd] → 还原成功；
        //   fg=[null] ×5 → 窗口纹丝不动（连续零反应，见
        //   maybe_hand_foreground_back_on_taskbar_click 的实测记录）。
        // 爆炸半径可控：该函数**只在 fg 为 null / Progman / WorkerW 时才真正动手**
        //   （内部早退）。「点活动窗口的按钮 = 最小化」场景 fg 是那个窗口 ⇒ 走不到这里，
        //   不会把最小化弄坏。
        const TASKBAR_FG_HANDBACK: bool = true;
        if TASKBAR_FG_HANDBACK {
            maybe_hand_foreground_back_on_taskbar_click();
        }
        return;
    }
    // [20260921 menu-fix] 光标下若是「当前前台应用自己的浮层控件」（右键菜单/下拉/提示），
    // 绝不能拉前台 —— 会把菜单当场打掉，见 cursor_over_app_popup 的实测记录。
    if cursor_over_app_popup() {
        return;
    }
    use windows_sys::Win32::System::Threading::{AttachThreadInput, GetCurrentThreadId};
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        BringWindowToTop, GetAncestor, GetForegroundWindow,
        GetWindowThreadProcessId, IsIconic, SetForegroundWindow, WindowFromPoint, GA_ROOT,
    };
    unsafe {
        let mut pt = POINT { x: 0, y: 0 };
        if GetCursorPos(&mut pt) == 0 {
            return;
        }
        let hit = WindowFromPoint(pt);
        if hit.is_null() {
            return;
        }
        let target = GetAncestor(hit, GA_ROOT);
        if target.is_null() || IsIconic(target) != 0 {
            // 最小化的窗口不抢前台，交给系统自身的点击还原逻辑
            return;
        }
        let fg = GetForegroundWindow();
        if fg == target {
            return;
        }
        let cur = GetCurrentThreadId();
        // [20260918f anti-wedge] 挂起窗口（≥5s 未响应）不做前台化：
        // AttachThreadInput 会与目标线程的输入队列同步，目标一忙注入线程就被拖死
        // （与 describe_cursor_target 的 GetWindowTextW 同一批次线上事故的另一卡点）。
        // 挂起目标直接放弃，交给 SendInput 自行落点。
        {
            use windows_sys::Win32::UI::WindowsAndMessaging::IsHungAppWindow;
            if IsHungAppWindow(target) != 0 {
                return;
            }
            if !fg.is_null() && IsHungAppWindow(fg) != 0 {
                return;
            }
        }
        let fg_tid = if fg.is_null() {
            0
        } else {
            GetWindowThreadProcessId(fg, std::ptr::null_mut())
        };
        let tgt_tid = GetWindowThreadProcessId(target, std::ptr::null_mut());
        let mut attached_fg = false;
        if fg_tid != 0 && fg_tid != cur {
            attached_fg = AttachThreadInput(cur, fg_tid, 1) != 0;
        }
        let attached_tgt = tgt_tid != 0 && tgt_tid != cur && AttachThreadInput(cur, tgt_tid, 1) != 0;
        BringWindowToTop(target);
        SetForegroundWindow(target);
        if attached_tgt {
            AttachThreadInput(cur, tgt_tid, 0);
        }
        if attached_fg {
            AttachThreadInput(cur, fg_tid, 0);
        }
    }
}

// ============ [20260918f anti-wedge] 注入线程隔离 + 看门狗 ============
// 背景（2026-09-18 线上事故）：18:26:23 起注入线程被一次点击的窗口取证
// （GetWindowTextW 同步 SendMessage 打到繁忙的 WebView2 主窗）永久卡死，
// 此后所有注入 IPC 静默排队、黑匣子零输出 —— 症状「画面正常、鼠标键盘全灭」。
// 防御：所有注入走**专职 worker 线程**；调用侧最多等 800ms，超时即判定 worker
// 楔死并**换新线程**接管（旧线程连同卡死的调用一起遗弃）。
// 这样无论未来再出现什么未知的卡点，最坏损失 = 单次注入被丢弃，
// 注入管线本身永远不会再整体瘫痪。
struct InputJob {
    run: Box<dyn FnOnce() + Send>,
    ack: std::sync::mpsc::Sender<()>,
}

static INPUT_TX: Mutex<Option<std::sync::mpsc::Sender<InputJob>>> = Mutex::new(None);

static LAST_INPUT_MS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// [20260928b] 本次进程运行内是否已经「自动让位」过一次（护栏，见 `dispatch_input`）。
/// 只让位一次 = 之后窗口的最小化/恢复完全由用户掌控（任务栏/托盘都可靠）。
static TUCK_DONE: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// 主窗口 HWND：lib.rs 启动时注册（子窗口标题同名，不能按标题找）。
static MAIN_HWND: Mutex<Option<isize>> = Mutex::new(None);

pub fn set_main_hwnd(hwnd: isize) {
    *MAIN_HWND.lock().unwrap_or_else(|e| e.into_inner()) = Some(hwnd);
}

/// [20260920 auto-tuck] 主窗口还摆在桌面上（可见且未最小化）就把它最小化。
/// 只收起、从不恢复 —— 恢复只由用户自己做；已最小化/已隐藏（托盘）时不动。
/// [20260928b] 追加护栏②：主窗口若是**当前前台窗口**（用户正开着它）则不动 ——
/// 那时的它并没有「挡桌面」，替用户做主只会制造「窗口自己消失」的观感。
#[cfg(windows)]
fn auto_tuck_main_window() {
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        GetForegroundWindow, IsIconic, IsWindowVisible, ShowWindowAsync, SW_MINIMIZE,
    };
    let hwnd = match *MAIN_HWND.lock().unwrap_or_else(|e| e.into_inner()) {
        Some(v) => v as *mut core::ffi::c_void,
        None => return,
    };
    unsafe {
        if IsWindowVisible(hwnd) == 0 || IsIconic(hwnd) != 0 {
            return;
        }
        if GetForegroundWindow() == hwnd {
            return;
        }
        ShowWindowAsync(hwnd, SW_MINIMIZE);
    }
}

#[cfg(not(windows))]
fn auto_tuck_main_window() {}

fn spawn_input_worker() -> std::sync::mpsc::Sender<InputJob> {
    let (tx, rx) = std::sync::mpsc::channel::<InputJob>();
    let _ = std::thread::Builder::new()
        .name("indesk-input".into())
        .spawn(move || {
            while let Ok(job) = rx.recv() {
                (job.run)();
                let _ = job.ack.send(());
            }
        });
    tx
}

/// 所有注入类操作必须经此派发（ipc.rs 的 mouse*/keyboard* 处理器统一走这里）。
pub fn dispatch_input(job: impl FnOnce() + Send + 'static) {
    // [20260920 auto-tuck / 20260928b 修「任务栏上的程序点不回来」]
    // 原型行为：距上次远端输入 ≥15s 的**第一条**输入 → 把主窗口最小化（让位）。
    //   初衷：被控时主窗罩在被控桌面上，用户抱怨"窗口关不掉、挡桌面"。
    //
    // 事故（用户 09-28 报「点底部程序栏里最小化的 InDesk，窗口不弹出来」）：
    //   这条规则和"用户自己点任务栏"正面冲突 —— 用户点任务栏按钮那一下**本身
    //   就是一条远端输入**，于是"把窗口唤回来"的动作先被这条规则最小化掉，
    //   表现就是「点了没反应 / 弹不出来」。间隔 <15s 时碰不到，所以早期连续操作
    //   测试"看着是好的"，一旦中途停顿 15s 再点就必现。
    //
    // 两条护栏（见 auto_tuck_main_window 的护栏②）：
    //   ① **每次进程运行只让位一次**：首次远端输入让开之后不再自动收起，
    //      之后的最小化/恢复完全听用户的（任务栏、托盘都可靠）；
    //   ② 主窗口是当前前台窗口时不让位（用户正开着它，不是"挡桌面"）。
    // 恢复方式仍是用户自己（任务栏按钮 / 托盘左键），本函数从不主动恢复。
    const TUCK_IDLE_MS: u64 = 15_000;
    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0);
    let last = LAST_INPUT_MS.swap(now_ms, std::sync::atomic::Ordering::Relaxed);
    if last != 0
        && now_ms.saturating_sub(last) >= TUCK_IDLE_MS
        && !TUCK_DONE.swap(true, std::sync::atomic::Ordering::Relaxed)
    {
        auto_tuck_main_window();
    }
    const WEDGE_TIMEOUT_MS: u64 = 800;
    let (ack_tx, ack_rx) = std::sync::mpsc::channel::<()>();
    let tx = {
        let mut g = INPUT_TX.lock().unwrap_or_else(|e| e.into_inner());
        if g.is_none() {
            *g = Some(spawn_input_worker());
        }
        g.as_ref().unwrap().clone()
    };
    if tx.send(InputJob { run: Box::new(job), ack: ack_tx }).is_err() {
        // worker 线程异常消亡（正常不应发生）：换新线程，本条瞬时事件放弃
        let mut g = INPUT_TX.lock().unwrap_or_else(|e| e.into_inner());
        *g = Some(spawn_input_worker());
        return;
    }
    if ack_rx
        .recv_timeout(std::time::Duration::from_millis(WEDGE_TIMEOUT_MS))
        .is_err()
    {
        // worker 楔死：遗弃旧线程（连同卡在里面的调用），新线程接管后续注入
        diag_log("dispatch_input", "wedge_detected_worker_respawned", false);
        let mut g = INPUT_TX.lock().unwrap_or_else(|e| e.into_inner());
        *g = Some(spawn_input_worker());
    }
}

/// [§26] 输入诊断日志：每个鼠标操作走哪条注入路径、成败如何，追加写到 exe 同目录
/// input_diag.log。首次写入时记录驱动可用性快照 —— 现场排障不再靠猜。
static DIAG_ONCE: std::sync::Once = std::sync::Once::new();
static DIAG_PATH: Mutex<Option<std::path::PathBuf>> = Mutex::new(None);

/// [20260918f anti-wedge] 带 80ms 超时的窗口标题读取。
/// 背景（2026-09-18 线上事故，18:26:23 起「画面正常、输入全灭」）：
/// 旧实现用 `GetWindowTextW` —— 它是**同步 SendMessage**，目标窗口线程一忙
/// （实测：WebView2 主窗正在渲染重活）就**永久阻塞**，把注入线程整体卡死，
/// 此后所有注入 IPC 静默排队，input_diag.log 一行都不再出现。
/// 改用 `SendMessageTimeoutW(WM_GETTEXT)`：最多等 80ms，挂起窗口直接放弃。
unsafe fn get_text_timeout(h: *mut core::ffi::c_void, buf: *mut u16, max: usize) -> usize {
    use windows_sys::Win32::UI::WindowsAndMessaging::{SendMessageTimeoutW, SMTO_ABORTIFHUNG};
    const WM_GETTEXT: u32 = 0x000D;
    let mut res: usize = 0;
    let ok = SendMessageTimeoutW(
        h,
        WM_GETTEXT,
        max,
        buf as isize,
        SMTO_ABORTIFHUNG,
        80,
        &mut res,
    );
    if ok == 0 {
        return usize::MAX; // 超时/挂起：调用方记 <hung>
    }
    res
}

/// [§34] 现场取证：记录光标屏幕坐标、该点**最顶层窗口**（类名|标题|pid）与当前前台窗口。
/// 「点了没反应」时黑匣子能直接指出这次点击被哪个窗口吃掉了 —— 不用再靠猜。
/// [20261019r 点击提速] 完整标题取证开关（环境变量 INDESK_INPUT_FORENSICS=1）。
///
/// 为什么点击热路径默认**不读窗口标题**：标题读取走 SendMessageTimeoutW
/// (WM_GETTEXT, 80ms 超时)，这是**跨线程同步消息**——前台是忙碌的
/// Chrome/WebView2（渲染中）时单次可吃 10~80ms。2026-10-19 20:31 用户回环实测
/// input_diag.log：press_left 单条处理 4~101ms，尖峰与 `<hung>` 完全重合；
/// 且 press/release/移动**串行过单 worker**，慢的 press 直接把 release 和
/// 后续移动堵在队列里 —— 用户体感即「左右键点击反应迟钝」。
/// 这是 §34/§36 游戏反作弊时代的黑匣子遗留（游戏已定论不考虑，billd 原版
/// 点击就是纯 SendInput ≈1ms）。类名/坐标取证保留（GetClassNameW 本地读、
/// 零跨线程消息）；真要抓「点了没反应」的完整标题时再开环境变量。
fn input_forensics_full() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var("INDESK_INPUT_FORENSICS").ok().as_deref() == Some("1"))
}

/// [20260918f anti-wedge] 标题改走超时读取（见 get_text_timeout）；
/// [20261019r] 默认热路径跳过标题读取（见 input_forensics_full）。
fn window_title_opt(h: *mut core::ffi::c_void) -> String {
    if !input_forensics_full() {
        return String::new();
    }
    let mut tt = [0u16; 64];
    // SAFETY: 调用方均处于 unsafe 上下文且 h 来自 GetForegroundWindow/WindowFromPoint。
    let m = unsafe { get_text_timeout(h, tt.as_mut_ptr(), 64) };
    if m == usize::MAX {
        "<hung>".to_string()
    } else {
        String::from_utf16_lossy(&tt[..m.min(64)])
    }
}

fn describe_cursor_target() -> String {
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        GetClassNameW, GetForegroundWindow, GetWindowThreadProcessId, WindowFromPoint,
    };
    unsafe {
        let mut pt = POINT { x: 0, y: 0 };
        if GetCursorPos(&mut pt) == 0 {
            return "cursor=?".to_string();
        }
        let info = |h: *mut core::ffi::c_void| -> String {
            if h.is_null() {
                return "null".to_string();
            }
            let mut cn = [0u16; 64];
            let n = GetClassNameW(h, cn.as_mut_ptr(), 64).max(0) as usize;
            let class = String::from_utf16_lossy(&cn[..n.min(64)]);
            let title = window_title_opt(h);
            let mut pid: u32 = 0;
            GetWindowThreadProcessId(h, &mut pid);
            format!("{}|{}|pid{}", class, title, pid)
        };
        format!(
            "at({},{}) hit=[{}] fg=[{}]",
            pt.x,
            pt.y,
            info(WindowFromPoint(pt)),
            info(GetForegroundWindow())
        )
    }
}

/// [1.4.35] 只取「当前前台窗口」的简短描述（类名|标题|pid，或 null）。
///
/// 用途：在**注入之后**再读一次前台。`describe_cursor_target` 只在注入**前**拍一张，
/// 所以「任务栏按钮到底有没有还原并激活被点窗口」在旧日志里根本看不出来 ——
/// 2026-09-28 的 `fg=[null] ×5 / 零反应` 就是这么漏过去的。
fn describe_fg_now() -> String {
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        GetClassNameW, GetForegroundWindow, GetWindowThreadProcessId,
    };
    unsafe {
        let h = GetForegroundWindow();
        if h.is_null() {
            return "null".to_string();
        }
        let mut cn = [0u16; 64];
        let n = GetClassNameW(h, cn.as_mut_ptr(), 64).max(0) as usize;
        let class = String::from_utf16_lossy(&cn[..n.min(64)]);
        let title = window_title_opt(h);
        let mut pid: u32 = 0;
        GetWindowThreadProcessId(h, &mut pid);
        format!("{}|{}|pid{}", class, title, pid)
    }
}

fn diag_log(kind: &str, path_taken: &str, ok: bool) {
    use std::io::Write;
    DIAG_ONCE.call_once(|| {
        let p = std::env::current_exe()
            .ok()
            .and_then(|e| e.parent().map(|d| d.join("input_diag.log")));
        if let Some(p) = &p {
            let ts = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0);
            let line = format!(
                "=== input diag start ts={} driver_available={} create_fails={}\r\n",
                ts,
                crate::interception::available(),
                crate::interception::CREATE_FAILS.load(Ordering::SeqCst)
            );
            let _ = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(p)
                .and_then(|mut f| f.write_all(line.as_bytes()));
        }
        *DIAG_PATH.lock().unwrap_or_else(|e| e.into_inner()) = p;
    });
    if let Some(p) = DIAG_PATH.lock().unwrap_or_else(|e| e.into_inner()).clone() {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis())
            .unwrap_or(0);
        let line = format!("{} {} {} ok={}\r\n", now, kind, path_taken, ok);
        let _ = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(p)
            .and_then(|mut f| f.write_all(line.as_bytes()));
    }
}

pub fn mouse_press_left() {
    // [§34] 注入点击按**屏幕坐标**投给该点最顶层的窗口 —— 桌面上只要有别的窗口压在
    // 目标上，点击就全被吃掉。先隐藏（真正的）输入覆盖层，再把光标下的顶层窗口拉到前台。
    let ctxs = describe_cursor_target();
    store_hidden_overlay(detect_and_hide_overlay());
    ensure_foreground_at_cursor();
    snap_for_double_click();
    // [§36] 通道级联：1=驱动（默认，硬件级路径） 0=SendInput 2=PostMessage。
    // 通道由探针实测选定，被保护钩子丢弃时自动降级，不再"驱动点击静默蒸发"。
    match inject_channel() {
        2 => {
            // [§21] 注入被保护钩子整体丢弃 → PostMessage 直投（按下，记住目标供抬起配对）。
            // 目标是 mstsc 容器时 post 返回 false（窗口消息进不了 RDP）→ 落回 SendInput。
            let ok = post_left_button(true, true);
            if ok {
                diag_log("press_left", &format!("compat {}", ctxs), true);
            } else {
                send_mouse(MOUSEEVENTF_LEFTDOWN, 0, 0, 0);
                diag_log("press_left", &format!("compat_fallback_sendinput {}", ctxs), true);
            }
        }
        0 => {
            let ok = send_mouse(MOUSEEVENTF_LEFTDOWN, 0, 0, 0);
            // [1.4.35] ok 改为 SendInput 的真实返回值；并追加 fg_after —— 任务栏按钮
            // 是否真的还原并激活被点窗口，只有注入**之后**的前台能说明问题。
            diag_log(
                "press_left",
                &format!("sendinput {} fg_after=[{}]", ctxs, describe_fg_now()),
                ok,
            );
        }
        _ => {
            if crate::interception::available() {
                let ok = crate::interception::mouse_button(true, true);
                diag_log("press_left", &format!("driver {}", ctxs), ok);
                if !ok {
                    send_mouse(MOUSEEVENTF_LEFTDOWN, 0, 0, 0);
                    diag_log("press_left", "driver_fail_fallback_sendinput", true);
                }
            } else {
                send_mouse(MOUSEEVENTF_LEFTDOWN, 0, 0, 0);
                diag_log("press_left", "sendinput", true);
            }
        }
    }
    record_left_down();
}

pub fn mouse_release_left() {
    match inject_channel() {
        2 => {
            let ok = post_left_button(false, false);
            if ok {
                diag_log("release_left", "compat", true);
            } else {
                send_mouse(MOUSEEVENTF_LEFTUP, 0, 0, 0);
                diag_log("release_left", "compat_fallback_sendinput", true);
            }
        }
        0 => {
            let ok = send_mouse(MOUSEEVENTF_LEFTUP, 0, 0, 0);
            // [1.4.35] 同上：真实 ok + 抬起后的前台（任务栏的还原/激活就落在抬起这一下）。
            diag_log(
                "release_left",
                &format!("sendinput fg_after=[{}]", describe_fg_now()),
                ok,
            );
        }
        _ => {
            if crate::interception::available() {
                let ok = crate::interception::mouse_button(true, false);
                diag_log("release_left", "driver", ok);
                if !ok {
                    send_mouse(MOUSEEVENTF_LEFTUP, 0, 0, 0);
                    diag_log("release_left", "driver_fail_fallback_sendinput", true);
                }
            } else {
                send_mouse(MOUSEEVENTF_LEFTUP, 0, 0, 0);
                diag_log("release_left", "sendinput", true);
            }
        }
    }
    // [§34] 与按下时的 hide 配对，还原输入覆盖层
    restore_hidden_overlay();
}

pub fn mouse_click_left() {
    let ctxs = describe_cursor_target();
    store_hidden_overlay(detect_and_hide_overlay());
    ensure_foreground_at_cursor();
    snap_for_double_click();
    // [§36] 同 press_left：通道级联 + 覆盖层隐藏 + 前台化，缺一不可（本机实测）。
    match inject_channel() {
        2 => {
            let ok = post_left_button(true, false);
            std::thread::sleep(std::time::Duration::from_millis(12));
            post_left_button(false, false);
            if ok {
                diag_log("click_left", &format!("compat {}", ctxs), true);
            } else {
                send_mouse(MOUSEEVENTF_LEFTDOWN, 0, 0, 0);
                record_left_down();
                send_mouse(MOUSEEVENTF_LEFTUP, 0, 0, 0);
                diag_log("click_left", &format!("compat_fallback_sendinput {}", ctxs), true);
            }
        }
        0 => {
            let ok = send_mouse(MOUSEEVENTF_LEFTDOWN, 0, 0, 0);
            record_left_down();
            send_mouse(MOUSEEVENTF_LEFTUP, 0, 0, 0);
            // [1.4.35] 真实 ok + 单击后的前台。
            diag_log(
                "click_left",
                &format!("sendinput {} fg_after=[{}]", ctxs, describe_fg_now()),
                ok,
            );
        }
        _ => {
            if crate::interception::available() {
                let ok = crate::interception::mouse_button(true, true);
                diag_log("click_left", &format!("driver {}", ctxs), ok);
                if ok {
                    std::thread::sleep(std::time::Duration::from_millis(12));
                    crate::interception::mouse_button(true, false);
                } else {
                    send_mouse(MOUSEEVENTF_LEFTDOWN, 0, 0, 0);
                    record_left_down();
                    send_mouse(MOUSEEVENTF_LEFTUP, 0, 0, 0);
                    diag_log("click_left", "driver_fail_fallback_sendinput", true);
                }
            } else {
                send_mouse(MOUSEEVENTF_LEFTDOWN, 0, 0, 0);
                record_left_down();
                send_mouse(MOUSEEVENTF_LEFTUP, 0, 0, 0);
                diag_log("click_left", "sendinput", true);
            }
        }
    }
    restore_hidden_overlay();
}

pub fn mouse_click_right() {
    let ctxs = describe_cursor_target();
    store_hidden_overlay(detect_and_hide_overlay());
    ensure_foreground_at_cursor();
    match inject_channel() {
        2 => {
            let ok = post_right_button(true);
            std::thread::sleep(std::time::Duration::from_millis(12));
            post_right_button(false);
            if ok {
                diag_log("click_right", &format!("compat {}", ctxs), true);
            } else {
                send_mouse(MOUSEEVENTF_RIGHTDOWN, 0, 0, 0);
                send_mouse(MOUSEEVENTF_RIGHTUP, 0, 0, 0);
                diag_log("click_right", &format!("compat_fallback_sendinput {}", ctxs), true);
            }
        }
        0 => {
            let ok = send_mouse(MOUSEEVENTF_RIGHTDOWN, 0, 0, 0)
                && send_mouse(MOUSEEVENTF_RIGHTUP, 0, 0, 0);
            // [1.4.35] 真实 ok + 右键后的前台。
            diag_log(
                "click_right",
                &format!("sendinput {} fg_after=[{}]", ctxs, describe_fg_now()),
                ok,
            );
        }
        _ => {
            if crate::interception::available() {
                let ok = crate::interception::mouse_button(false, true);
                diag_log("click_right", &format!("driver {}", ctxs), ok);
                if ok {
                    std::thread::sleep(std::time::Duration::from_millis(12));
                    crate::interception::mouse_button(false, false);
                } else {
                    send_mouse(MOUSEEVENTF_RIGHTDOWN, 0, 0, 0);
                    send_mouse(MOUSEEVENTF_RIGHTUP, 0, 0, 0);
                    diag_log("click_right", "driver_fail_fallback_sendinput", true);
                }
            } else {
                send_mouse(MOUSEEVENTF_RIGHTDOWN, 0, 0, 0);
                send_mouse(MOUSEEVENTF_RIGHTUP, 0, 0, 0);
                diag_log("click_right", "sendinput", true);
            }
        }
    }
    restore_hidden_overlay();
}

pub fn mouse_double_click() {
    mouse_click_left();
    std::thread::sleep(std::time::Duration::from_millis(60));
    mouse_click_left();
}

/// 滚轮：垂直为正/负，水平用 HWHEEL。
///
/// [20260919 wheel-fix] `amount` 语义是「滚轮格数」（每格 = WHEEL_DELTA 120）。
/// 旧前端曾把 deltaY **像素值**（一格 ≈100px）直接当 amount 传上来 →
/// `amount×120` 后一格滚动 = 100 齿，滚动条直接从顶跳到底。前端已改为
/// 像素→格换算（每 100px 一格、单次 ≤10 格）；这里再加 ±16 格钳制做双保险，
/// 兼容老前端时最坏也是"一次滚 16 格"而不是"直接滚到头"。
pub fn mouse_scroll(amount: i32, horizontal: bool) {
    let amount = amount.clamp(-16, 16);
    // [§22] 驱动级滚轮（GameGuard 对滚轮的拦截行为未单测，统一走驱动最稳）
    if crate::interception::available() && crate::interception::mouse_wheel(amount, horizontal) {
        return;
    }
    // 一次滚轮点击 = WHEEL_DELTA(120)
    let delta = (amount * 120) as u32;
    let flags = if horizontal {
        MOUSEEVENTF_HWHEEL
    } else {
        MOUSEEVENTF_WHEEL
    };
    send_mouse(flags, 0, 0, delta);
}

pub fn mouse_scroll_up(amount: i32) {
    mouse_scroll(amount, false);
}

pub fn mouse_scroll_down(amount: i32) {
    mouse_scroll(-amount, false);
}

pub fn mouse_scroll_left(amount: i32) {
    mouse_scroll(-amount, true);
}

pub fn mouse_scroll_right(amount: i32) {
    mouse_scroll(amount, true);
}

// ---------------------------------------------------------------- keyboard

/// [20260919 ime] 输入法切换代按。
///
/// 为什么必须由壳代按：主控端的系统级热键（Win+Space / Ctrl+Space）在主控
/// 操作系统/输入法层就被吃掉，键盘事件根本到不了主控网页 —— 远端永远收不到
/// 「切换输入法」的意图。主控 UI 的「输入法」按钮发显式指令（switchIme），
/// 这里在**被控端**代按对应组合键。
///
/// method：0=Win+Space（切换输入语言） 1=Ctrl+Space（中/英，微软拼音等）
///         2=单击 Shift（中/英，搜狗等） 3=Alt+Shift（切换输入语言）
///
/// 全部走 SendInput（VK 级），**不**走驱动：这些是系统全局热键，
/// SendInput 注入即可触发，且驱动扫描码路径对 Win 键组合的行为没验证过。
/// 总耗时 ≤ ~300ms，dispatch_input 的 800ms 看门狗兜得住。
pub fn ime_switch(method: u32) {
    const LWIN: u16 = 0x5B;
    const LALT: u16 = 0xA4;
    const SHIFT: u16 = 0xA0;
    const SPACE: u16 = 0x20;
    let sleep_ms = |ms: u64| std::thread::sleep(std::time::Duration::from_millis(ms));
    match method {
        0 => {
            send_key(LWIN, 0, 0);
            sleep_ms(50);
            send_key(SPACE, 0, 0);
            sleep_ms(80);
            send_key(SPACE, 0, KEYEVENTF_KEYUP);
            sleep_ms(50);
            send_key(LWIN, 0, KEYEVENTF_KEYUP);
        }
        1 => {
            // [20260925 ime6] Ctrl+空格 按钮 = 切换**当前输入法自己的**中/英模式。
            // 实现是模拟一次 Shift 单击（down→up、中间无别的键）——这正是搜狗/
            // 微软拼音默认的「中英切换」热键，注入后 IME 自己翻转模式。
            // 本机行为级实测（probe-web/_t42）：conv 0x0001(中文)→0x0000(英文) PASS，
            // IMM 直改转换状态同样 PASS；两者取键盘路径（对 TSF 应用零句柄依赖）。
            // ⚠️ 绝不做的事（用户硬性要求）：① 不切键盘布局（旧 ime_toggle_layout
            //    的 WM_INPUTLANGCHANGEREQUEST 路径）；② 不 LoadKeyboardLayout 装载
            //    「英语(美国)」——旧逻辑曾因此往会话里塞了一个英语布局（用户在切换器
            //    里看到了，已用 UnloadKeyboardLayout 清掉）；③ 不装/不切任何输入法。
            send_key(SHIFT, 0, 0);
            sleep_ms(80);
            send_key(SHIFT, 0, KEYEVENTF_KEYUP);
            diag_log("ime_switch", "method=1 shift_tap(CN/EN toggle)", true);
        }
        2 => {
            send_key(SHIFT, 0, 0);
            sleep_ms(80);
            send_key(SHIFT, 0, KEYEVENTF_KEYUP);
        }
        3 => {
            send_key(LALT, 0, 0);
            sleep_ms(40);
            send_key(SHIFT, 0, 0);
            sleep_ms(80);
            send_key(SHIFT, 0, KEYEVENTF_KEYUP);
            sleep_ms(40);
            send_key(LALT, 0, KEYEVENTF_KEYUP);
        }
        _ => {}
    }
}

fn send_key(vk: u16, scan: u16, flags: u32) {
    let input = INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 {
            ki: KEYBDINPUT {
                wVk: vk,
                wScan: scan,
                dwFlags: flags,
                time: 0,
                dwExtraInfo: 0,
            },
        },
    };
    unsafe {
        SendInput(1, &input, size_of::<INPUT>() as i32);
    }
}

/// 直接输入一段文本（含中文等 Unicode 字符）。
pub fn keyboard_text(text: &str) {
    // [§22] 驱动级注入优先：整串字符都能映射扫描码才走驱动（否则整串退回 SendInput，
    // 避免半截字符串被拆到两条通道造成重复/丢字）
    if crate::interception::available() && text.chars().all(text_char_driver_mappable) {
        let all_ok = text
            .chars()
            .all(|ch| text_char_via_driver(ch));
        if all_ok {
            return;
        }
    }
    for ch in text.chars() {
        let code = ch as u16;
        send_key(0, code, KEYEVENTF_UNICODE);
        send_key(0, code, KEYEVENTF_UNICODE | KEYEVENTF_KEYUP);
    }
}

/// [§22] 字符能否映射成扫描码走驱动（ASCII 字母/数字/常用标点；中文等返回 false）。
fn text_char_driver_mappable(ch: char) -> bool {
    use windows_sys::Win32::UI::Input::KeyboardAndMouse::VkKeyScanW;
    if !ch.is_ascii() {
        return false;
    }
    let r = unsafe { VkKeyScanW(ch as u16) };
    if (r as i16) == -1 {
        return false;
    }
    let shift = ((r >> 8) & 0xFF) as u16;
    shift == 0 || shift == 1
}

/// [§22] 经驱动注入一个字符（含必要的 Shift 组合）。
fn text_char_via_driver(ch: char) -> bool {
    use windows_sys::Win32::UI::Input::KeyboardAndMouse::VkKeyScanW;
    let r = unsafe { VkKeyScanW(ch as u16) };
    if (r as i16) == -1 {
        return false;
    }
    let vk = (r & 0xFF) as u16;
    let shift = ((r >> 8) & 0xFF) as u16;
    let Some((scan, ext)) = crate::interception::vk_to_scancode(vk) else {
        return false;
    };
    let press = || -> bool {
        let down = crate::interception::key_scancode(scan, true, ext);
        let up = crate::interception::key_scancode(scan, false, ext);
        down && up
    };
    if shift == 1 {
        // 需要 Shift 组合（大写字母 / 符号）
        let Some((ssh, sext)) = crate::interception::vk_to_scancode(0xA0) else {
            return false;
        };
        if !crate::interception::key_scancode(ssh, true, sext) {
            return false;
        }
        let ok = press();
        crate::interception::key_scancode(ssh, false, sext);
        ok
    } else {
        press()
    }
}

/// 按下 / 抬起一个（或多个）nut-js Key。
pub fn keyboard_key(keys: &[u16], down: bool) {
    let via_driver = crate::interception::available();
    for &k in keys {
        if let Some((vk, extended)) = nut_key_to_vk(k) {
            if via_driver {
                if let Some((scan, ext)) = crate::interception::vk_to_scancode(vk) {
                    if crate::interception::key_scancode(scan, down, ext || extended) {
                        continue;
                    }
                }
                // 驱动不可映射（极少数 VK）→ 落回 SendInput
            }
            let mut flags = 0u32;
            if extended {
                flags |= KEYEVENTF_EXTENDEDKEY;
            }
            if !down {
                flags |= KEYEVENTF_KEYUP;
            }
            send_key(vk, 0, flags);
            // [20260925 ime6] 键盘注入留痕：组合键时序排查全靠它
            // （Ctrl+V「全序注入」的验证就是用它看 Ctrl/V 的落地顺序）。
            diag_log(
                if down { "key_press" } else { "key_release" },
                &format!("vk=0x{:04X}", vk),
                true,
            );
        }
    }
}

/// **[20260922 松开系统键]** 释放被控端上所有按下的修饰键。
///
/// 为什么需要：主控端在**浏览器**里操作时，用户按住 Ctrl/Alt 后若焦点切走
/// （切标签页、点到浏览器 UI、窗口失焦），浏览器**不会**把 Ctrl 的 keyup
/// 发给我们 → 被控端那边 Ctrl 一直处于"按下"状态 → 后续所有点击都变成
/// Ctrl+点击（多选）、打字变成快捷键。这在网页版主控端是高频问题。
///
/// 做法：把四个修饰键（含左右两套）各发一次 keyup。**只发 keyup、不发按下**，
/// 因此对"本来没按下"的键是无害空操作 —— 可以放心重复点。
///
/// 覆盖：General Ctrl/Alt/Shift、左右 Ctrl/Alt/Shift、左右 Win。
pub fn release_all_modifiers() {
    // VK 码：Ctrl=0x11 Shift=0x10 Alt=0x12 Win=0x5B/0x5C；左右专用见注释
    const KEYS: &[(u16, bool)] = &[
        (0x11, false), // VK_CONTROL（General）
        (0xA2, false), // VK_LCONTROL
        (0xA3, true),  // VK_RCONTROL（extended）
        (0x10, false), // VK_SHIFT
        (0xA0, false), // VK_LSHIFT
        (0xA1, false), // VK_RSHIFT
        (0x12, false), // VK_MENU（General Alt）
        (0xA4, false), // VK_LMENU
        (0xA5, true),  // VK_RMENU（extended）
        (0x5B, true),  // VK_LWIN（extended）
        (0x5C, true),  // VK_RWIN（extended）
    ];
    for &(vk, extended) in KEYS {
        let mut flags = KEYEVENTF_KEYUP;
        if extended {
            flags |= KEYEVENTF_EXTENDEDKEY;
        }
        send_key(vk, 0, flags);
    }
    eprintln!("[indesk] 松开系统键：已释放全部修饰键");
}

/// **[20260922 Ctrl+Alt+Del]** 触发系统级 SAS（Secure Attention Sequence）。
///
/// 返回 `Ok(())` 仅代表"注入调用成功"，**不代表**系统一定弹出安全桌面 ——
/// SAS 是否真正生效取决于被控端进程的权限：
/// - Microsoft 规定 `SendSAS` 仅在「进程运行于 SYSTEM 会话」或「进程持有
///   SeTcbPrivilege（作为服务运行）」时才真正触发安全桌面；普通用户态进程
///   调用会**静默失败**（返回成功但操作系统忽略）。
/// - InDesk 壳当前以当前用户权限安装运行（nsis installMode=currentUser），
///   因此**很可能发不出 SAS**。这里如实返回结果，由主控端 UI 决定如何提示，
///   **不做假成功**。
///
/// 实现走 `sas.dll` 的 `SendSAS(FALSE)`（FALSE = 不在后台弹 UAC）。
#[cfg(windows)]
pub fn send_ctrl_alt_del() -> Result<(), String> {
    use windows_sys::Win32::System::LibraryLoader::{GetProcAddress, LoadLibraryW};
    unsafe {
        let name: Vec<u16> = "sas.dll\0".encode_utf16().collect();
        let h = LoadLibraryW(name.as_ptr());
        if h.is_null() {
            return Err("无法加载 sas.dll（系统缺少该组件）".into());
        }
        // SendSAS 签名：void WINAPI SendSAS(BOOL AsUser)
        type SendSasFn = unsafe extern "system" fn(i32);
        let addr = GetProcAddress(h, b"SendSAS\0".as_ptr());
        let f: SendSasFn = match addr {
            Some(p) => unsafe { std::mem::transmute(p) },
            None => return Err("sas.dll 中未找到 SendSAS 导出".into()),
        };
        f(0); // FALSE = 以当前桌面上下文触发
    }
    Ok(())
}

#[cfg(not(windows))]
pub fn send_ctrl_alt_del() -> Result<(), String> {
    Err("仅 Windows 支持".into())
}

/// nut-js `Key` 枚举 → Windows 虚拟键码（VK）。
/// 返回 `(vk, is_extended)`；`None` 表示该键无需注入（如 Fn）。
fn nut_key_to_vk(key: u16) -> Option<(u16, bool)> {
    let r = match key {
        0 => (0x1B, false),   // Escape
        1..=24 => (0x70 + (key - 1), false), // F1..F24
        25 => (0x2C, true),   // Print
        26 => (0x91, false),  // ScrollLock
        27 => (0x13, false),  // Pause
        28 => (0xC0, false),  // Grave
        29..=37 => (0x31 + (key - 29), false), // Num1..Num9
        38 => (0x30, false),  // Num0
        39 => (0xBD, false),  // Minus
        40 => (0xBB, false),  // Equal
        41 => (0x08, false),  // Backspace
        42 => (0x2D, true),   // Insert
        43 => (0x24, true),   // Home
        44 => (0x21, true),   // PageUp
        45 => (0x90, false),  // NumLock
        46 => (0x6F, true),   // Divide
        47 => (0x6A, false),  // Multiply
        48 => (0x6D, false),  // Subtract
        49 => (0x09, false),  // Tab
        50 => (0x51, false),  // Q
        51 => (0x57, false),  // W
        52 => (0x45, false),  // E
        53 => (0x52, false),  // R
        54 => (0x54, false),  // T
        55 => (0x59, false),  // Y
        56 => (0x55, false),  // U
        57 => (0x49, false),  // I
        58 => (0x4F, false),  // O
        59 => (0x50, false),  // P
        60 => (0xDB, false),  // LeftBracket
        61 => (0xDD, false),  // RightBracket
        62 => (0xDC, false),  // Backslash
        63 => (0x2E, true),   // Delete
        64 => (0x23, true),   // End
        65 => (0x22, true),   // PageDown
        66..=68 => (0x67 + (key - 66), false), // NumPad7..9
        69 => (0x6B, false),  // Add
        70 => (0x14, false),  // CapsLock
        71 => (0x41, false),  // A
        72 => (0x53, false),  // S
        73 => (0x44, false),  // D
        74 => (0x46, false),  // F
        75 => (0x47, false),  // G
        76 => (0x48, false),  // H
        77 => (0x4A, false),  // J
        78 => (0x4B, false),  // K
        79 => (0x4C, false),  // L
        80 => (0xBA, false),  // Semicolon
        81 => (0xDE, false),  // Quote
        82 => (0x0D, false),  // Return
        83..=85 => (0x64 + (key - 83), false), // NumPad4..6
        86 => (0xA0, false),  // LeftShift
        87 => (0x5A, false),  // Z
        88 => (0x58, false),  // X
        89 => (0x43, false),  // C
        90 => (0x56, false),  // V
        91 => (0x42, false),  // B
        92 => (0x4E, false),  // N
        93 => (0x4D, false),  // M
        94 => (0xBC, false),  // Comma
        95 => (0xBE, false),  // Period
        96 => (0xBF, false),  // Slash
        97 => (0xA1, false),  // RightShift
        98 => (0x26, true),   // Up
        99..=101 => (0x61 + (key - 99), false), // NumPad1..3
        102 => (0x0D, false), // Enter
        103 => (0xA2, false), // LeftControl
        104 | 105 | 106 => (0x5B, true), // LeftSuper / LeftWin / LeftCmd
        107 => (0xA4, false), // LeftAlt
        108 => (0x20, false), // Space
        109 => (0xA5, true),  // RightAlt
        110 | 111 | 112 => (0x5C, true), // RightSuper / RightWin / RightCmd
        113 => (0x5D, true),  // Menu
        114 => (0xA3, true),  // RightControl
        115 => return None,   // Fn
        116 => (0x25, true),  // Left
        117 => (0x28, true),  // Down
        118 => (0x27, true),  // Right
        119 => (0x60, false), // NumPad0
        120 => (0x6E, false), // Decimal
        121 => (0x0C, false), // Clear
        122 => (0xAD, true),  // AudioMute
        123 => (0xAE, true),  // AudioVolDown
        124 => (0xAF, true),  // AudioVolUp
        125 => (0xB3, true),  // AudioPlay
        126 => (0xB2, true),  // AudioStop
        127 => (0xB3, true),  // AudioPause
        128 => (0xB1, true),  // AudioPrev
        129 => (0xB0, true),  // AudioNext
        130 => (0xB1, true),  // AudioRewind
        131 => (0xB0, true),  // AudioForward
        132 => (0xB3, true),  // AudioRepeat
        133 => (0xB3, true),  // AudioRandom
        _ => return None,
    };
    Some(r)
}

/// 主显示器工作区尺寸（物理像素，已排除任务栏）。取不到返回 None。
pub fn primary_work_area() -> Option<(i32, i32)> {
    use windows_sys::Win32::Foundation::RECT;
    use windows_sys::Win32::UI::WindowsAndMessaging::{SystemParametersInfoW, SPI_GETWORKAREA};
    let mut rect = RECT {
        left: 0,
        top: 0,
        right: 0,
        bottom: 0,
    };
    let ok = unsafe { SystemParametersInfoW(SPI_GETWORKAREA, 0, (&mut rect as *mut RECT).cast(), 0) };
    if ok == 0 {
        None
    } else {
        Some((rect.right - rect.left, rect.bottom - rect.top))
    }
}

/// 让显示器保持唤醒（对应 Electron 的 `powerSaveBlocker.start('prevent-display-sleep')`）。
pub fn prevent_display_sleep() {
    use windows_sys::Win32::System::Power::{
        SetThreadExecutionState, ES_CONTINUOUS, ES_DISPLAY_REQUIRED, ES_SYSTEM_REQUIRED,
    };
    unsafe {
        SetThreadExecutionState(ES_CONTINUOUS | ES_DISPLAY_REQUIRED | ES_SYSTEM_REQUIRED);
    }
}
