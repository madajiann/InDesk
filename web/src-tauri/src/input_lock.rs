//! [1.4.39 隐私屏 v3] 被控端**本机物理键鼠锁**（低级钩子输入锁）。
//!
//! ## 语义表（2026-10-03 定稿，串端红线 §41 要求先写死再动工）
//!
//! | 维度 | 行为 |
//! |---|---|
//! | 动作发起 | 主控端点击"隐私屏"按钮（或被控端本机逃生横幅关闭） |
//! | 被控端显示器 | 黑化（`privacy_overlay` 负责，本模块不碰） |
//! | 被控端**物理**键鼠 | **全部被忽略**：坐在被控端电脑前的人敲键盘/动鼠标无效，无法干扰远程会话、无法偷窥时顺手操作 |
//! | 被控端**注入**输入 | **照常放行**：主控端发来的远程键鼠（SendInput 注入，`LLKHF/LLMHF_INJECTED` 置位）不受影响，主控端照常操作被控端 |
//! | 主控端 | 画面照常、键鼠照常（本模块对主控端零感知） |
//! | 关闭 | 隐私屏关 = 锁自动解；壳进程退出 = 钩子由 OS 自动拆除（进程死锁不死人） |
//!
//! ## 实现原理
//!
//! `SetWindowsHookExW(WH_KEYBOARD_LL / WH_MOUSE_LL)` 低级钩子。钩子回调里：
//! - 注入事件（flags 含 INJECTED 0x10）→ `CallNextHookEx` 放行；
//! - 物理事件且锁开启 → 返回 1 吞掉，并累加计数器（供探针取证）。
//!
//! ## 安全兜底
//!
//! 1. **全有或全无**：键盘/鼠标任一钩子装失败 ⇒ 立即卸掉另一个并返回 false，
//!    绝不留"半锁"状态（与 privacy_overlay 同哲学）；
//! 2. 回调路径极轻：只读两个原子量 + 一次比较，无分配无系统调用，
//!    规避 Windows LowLevelHooksTimeout 静默摘钩；
//! 3. Ctrl+Alt+Del / Win+L 属 OS 安全序列，任何用户态手段都拦不住——
//!   **已知限制**，测试矩阵里单列；
//! 4. 关闭时先原子清 `LOCKED` 再卸钩（哪怕卸钩失败，锁也已经失效）；
//! 5. 非 Windows 平台不支持（与 privacy_overlay 的 stub 对齐）。

#[cfg(windows)]
mod imp {
    use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
    use std::sync::mpsc;
    use std::time::Duration;

    use windows_sys::Win32::Foundation::{LPARAM, LRESULT, WPARAM};
    use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        CallNextHookEx, DispatchMessageW, PeekMessageW, SetWindowsHookExW, TranslateMessage,
        UnhookWindowsHookEx, HHOOK, KBDLLHOOKSTRUCT, LLKHF_INJECTED, LLMHF_INJECTED,
        MSG, PM_REMOVE, WH_KEYBOARD_LL, WH_MOUSE_LL,
    };

    /// 锁开关（钩子回调读取；开/关顺序保证见 set 流程）。
    static LOCKED: AtomicBool = AtomicBool::new(false);
    /// 取证计数：被吞掉的物理键盘/鼠标事件数。
    pub static BLOCKED_KEY: AtomicU32 = AtomicU32::new(0);
    pub static BLOCKED_MOUSE: AtomicU32 = AtomicU32::new(0);

    /// 键盘注入标记（与 LLKHF_INJECTED 同值；显式定义避免特性门控差异）。
    const KF_INJECTED: u32 = LLKHF_INJECTED;
    /// 鼠标注入标记（与 LLMHF_INJECTED 同值 = 0x10）。
    const MF_INJECTED: u32 = LLMHF_INJECTED;

    enum Cmd {
        /// 开/关。ack 回 true = 钩子已按目标状态就位。
        Set(bool, mpsc::Sender<bool>),
    }

    static TX: std::sync::OnceLock<mpsc::Sender<Cmd>> = std::sync::OnceLock::new();

    /// 对外入口：设置输入锁。返回最终状态（true = 已就位/已解除）。
    pub fn set(on: bool) -> bool {
        let tx = TX.get_or_init(spawn_worker);
        let (ack, rx) = mpsc::channel();
        if tx.send(Cmd::Set(on, ack)).is_err() {
            return false;
        }
        rx.recv_timeout(Duration::from_secs(5)).unwrap_or(false)
    }

    /// 只读探针：当前是否处于锁死状态（供 examples 探针取证）。
    pub fn locked() -> bool {
        LOCKED.load(Ordering::Acquire)
    }

    /// 只读探针：本次进程生命周期内被吞掉的物理键盘事件总数。
    pub fn blocked_keys() -> u32 {
        BLOCKED_KEY.load(Ordering::Relaxed)
    }

    /// 只读探针：本次进程生命周期内被吞掉的物理鼠标事件总数。
    pub fn blocked_mice() -> u32 {
        BLOCKED_MOUSE.load(Ordering::Relaxed)
    }

    fn spawn_worker() -> mpsc::Sender<Cmd> {
        let (tx, rx) = mpsc::channel::<Cmd>();
        std::thread::Builder::new()
            .name("indesk-input-lock".into())
            .spawn(move || worker_loop(rx))
            .expect("spawn indesk-input-lock worker");
        tx
    }

    fn worker_loop(rx: mpsc::Receiver<Cmd>) {
        let mut kb_hook: HHOOK = std::ptr::null_mut();
        let mut ms_hook: HHOOK = std::ptr::null_mut();
        loop {
            match rx.recv_timeout(Duration::from_millis(200)) {
                Ok(Cmd::Set(on, ack)) => {
                    let ok = if on {
                        if !kb_hook.is_null() {
                            // 幂等：已开着就算成功。
                            true
                        } else {
                            LOCKED.store(true, Ordering::Release); // 先置位再挂钩，防漏吞
                            unsafe {
                                let hmod = GetModuleHandleW(std::ptr::null());
                                kb_hook = SetWindowsHookExW(
                                    WH_KEYBOARD_LL,
                                    Some(kb_hook_proc),
                                    hmod,
                                    0,
                                );
                                ms_hook =
                                    SetWindowsHookExW(WH_MOUSE_LL, Some(ms_hook_proc), hmod, 0);
                                if kb_hook.is_null() || ms_hook.is_null() {
                                    // 全有或全无：卸掉已装的，回到无锁态。
                                    LOCKED.store(false, Ordering::Release);
                                    if !kb_hook.is_null() {
                                        UnhookWindowsHookEx(kb_hook);
                                        kb_hook = std::ptr::null_mut();
                                    }
                                    if !ms_hook.is_null() {
                                        UnhookWindowsHookEx(ms_hook);
                                        ms_hook = std::ptr::null_mut();
                                    }
                                    false
                                } else {
                                    true
                                }
                            }
                        }
                    } else {
                        // 关锁：先原子清位（立即失效），再卸钩（失败也不影响解锁语义）。
                        LOCKED.store(false, Ordering::Release);
                        unsafe {
                            if !kb_hook.is_null() {
                                UnhookWindowsHookEx(kb_hook);
                                kb_hook = std::ptr::null_mut();
                            }
                            if !ms_hook.is_null() {
                                UnhookWindowsHookEx(ms_hook);
                                ms_hook = std::ptr::null_mut();
                            }
                        }
                        true
                    };
                    let _ = ack.send(ok);
                }
                Err(mpsc::RecvTimeoutError::Timeout) => { /* 醒来只为泵消息 */ }
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    // 主进程在收摊：清锁、卸钩、退出（线程退出时 OS 兜底拆钩）。
                    LOCKED.store(false, Ordering::Release);
                    unsafe {
                        if !kb_hook.is_null() {
                            UnhookWindowsHookEx(kb_hook);
                        }
                        if !ms_hook.is_null() {
                            UnhookWindowsHookEx(ms_hook);
                        }
                    }
                    return;
                }
            }
            pump_messages();
        }
    }

    /// 键盘钩子：锁开启且非注入 → 吞掉并计数；否则放行。
    unsafe extern "system" fn kb_hook_proc(ncode: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
        if ncode >= 0 && LOCKED.load(Ordering::Acquire) {
            let info = &*(lparam as *const KBDLLHOOKSTRUCT);
            if info.flags & KF_INJECTED == 0 {
                BLOCKED_KEY.fetch_add(1, Ordering::Relaxed);
                return 1; // 吞掉物理键
            }
        }
        CallNextHookEx(std::ptr::null_mut(), ncode, wparam, lparam)
    }

    /// 鼠标钩子：同上。注意仅吞"有破坏力"的动作即可，但 move 也一并吞，
    /// 否则旁观者晃鼠标会与主控端光标打架（语义表"无法干扰"）。
    unsafe extern "system" fn ms_hook_proc(ncode: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
        if ncode >= 0 && LOCKED.load(Ordering::Acquire) {
            let info = &*(lparam as *const windows_sys::Win32::UI::WindowsAndMessaging::MSLLHOOKSTRUCT);
            if info.flags & MF_INJECTED == 0 {
                BLOCKED_MOUSE.fetch_add(1, Ordering::Relaxed);
                return 1; // 吞掉物理鼠标（含移动）
            }
        }
        CallNextHookEx(std::ptr::null_mut(), ncode, wparam, lparam)
    }

    fn pump_messages() {
        // 必须 PeekMessage（非阻塞）：阻塞式 GetMessage 会让 worker 卡在等消息，
        // mpsc 的开/关指令就再也收不到了（ack 5s 超时 => 功能假死）。
        unsafe {
            let mut msg: MSG = std::mem::zeroed();
            while PeekMessageW(&mut msg, std::ptr::null_mut(), 0, 0, PM_REMOVE) != 0 {
                TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
        }
    }
}

/// 非 Windows 平台：输入锁不支持（stub 与 privacy_overlay 对齐）。
#[cfg(not(windows))]
mod imp {
    pub fn set(_on: bool) -> bool {
        false
    }
    pub fn locked() -> bool {
        false
    }
}

pub use imp::blocked_keys;
pub use imp::blocked_mice;
pub use imp::locked;
pub use imp::set;
