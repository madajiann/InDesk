// Prevents an additional console window on Windows in release builds.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

// [20260915] Windows 单实例守卫（修"托盘出现两个/三个图标"）：
// 旧壳没有任何单实例保护，双击 exe 就会再起一个进程——每个进程各建一个
// 托盘图标（lib.rs setup() 里每个实例都 TrayIconBuilder），用户看到的
// "有时两个、有时三个"就是多次重复启动累积的多个实例。
// 实现：命名互斥量。第一个实例创建成功并持有（不 CloseHandle，进程存活
// 期间一直占用）；后续实例 CreateMutexW 返回同名的已存在互斥量、
// GetLastError() == ERROR_ALREADY_EXISTS → 直接退出。
// 互斥量创建本身失败（极罕见）时保守放行，不拦正常启动。
#[cfg(windows)]
fn acquire_single_instance() -> bool {
    use windows_sys::Win32::Foundation::{GetLastError, ERROR_ALREADY_EXISTS};
    use windows_sys::Win32::System::Threading::CreateMutexW;

    const MUTEX_NAME: &str = "Global\\InDeskDesktopSingleInstanceMutex\0";
    let name: Vec<u16> = MUTEX_NAME.encode_utf16().collect();
    let handle = unsafe { CreateMutexW(std::ptr::null(), 0, name.as_ptr()) };
    if handle.is_null() {
        // 创建失败：保守放行（最坏情况退回旧行为——可能双实例，但不至于起不来）
        return true;
    }
    // 有意不调用 CloseHandle：让互斥量随进程存活，实现"持有即单实例"
    let last_err: u32 = unsafe { GetLastError() };
    last_err != ERROR_ALREADY_EXISTS
}

// [20260927 · 修「双击没反应」] 已有实例在跑时，**不再静默 exit(0)**。
//
// 静默退出就是「下载了新 exe / 双击图标，一点动静都没有」的根因：用户先在设置页
// 点了更新（旧实例还在跑），或者托盘里本来就驻留着一个实例，互斥量被占住，新进程
// 就在 main() 里无声消失 —— 既不建窗口、也不提示、更不会把已有窗口叫出来。
// 表现完全等同于「exe 坏了 / 双击无效」，极易误判成「下载的包有问题」。
//
// 现在的行为：找到已有实例的**主窗口** → 还原 + 置前（+ 任务栏闪烁兜底）；
// 找不到窗口（进程活着却没窗口，或枚举受限）→ 弹一个说明框。两条路都保证
// 「双击」这个动作**一定有可见反馈**。
//
// ⚠️ 挑窗口必须"优先带标题的那个"：实测本机一个 InDesk 进程就有 2 个**可见**顶层窗口
// （主窗口 title=InDesk，另一个无标题 —— WebView2/TAO 的辅助窗口），先碰到就停会挑错，
// 激活无标题窗口用户等于还是"没反应"。所以这里收集完全部候选再挑。
#[cfg(windows)]
fn activate_existing_instance() -> bool {
    use windows_sys::Win32::Foundation::{CloseHandle, INVALID_HANDLE_VALUE};
    use windows_sys::Win32::System::Diagnostics::ToolHelp::{
        CreateToolhelp32Snapshot, Process32FirstW, Process32NextW, PROCESSENTRY32W,
        TH32CS_SNAPPROCESS,
    };
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        BringWindowToTop, EnumWindows, FlashWindowEx, GetWindowTextLengthW,
        GetWindowThreadProcessId, IsWindowVisible, SetForegroundWindow, ShowWindow, FLASHWINFO,
        FLASHW_ALL, FLASHW_TIMERNOFG, SW_RESTORE,
    };

    struct Ctx {
        pids: Vec<u32>,
        /// (窗口句柄, 是否有标题)。有标题的才是主窗口。
        cands: Vec<(*mut core::ffi::c_void, bool)>,
    }

    unsafe extern "system" fn enum_cb(hwnd: *mut core::ffi::c_void, lparam: isize) -> i32 {
        let ctx = &mut *(lparam as *mut Ctx);
        let mut pid: u32 = 0;
        GetWindowThreadProcessId(hwnd, &mut pid);
        if pid != 0 && ctx.pids.contains(&pid) && IsWindowVisible(hwnd) != 0 {
            let has_title = GetWindowTextLengthW(hwnd) > 0;
            ctx.cands.push((hwnd, has_title));
        }
        1 // 继续枚举：要看完所有候选才能挑出主窗口
    }

    // ① 进程快照里挑出同名进程（排除自己）
    let me = std::process::id();
    let mut pids: Vec<u32> = Vec::new();
    unsafe {
        let snap = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0);
        if snap != INVALID_HANDLE_VALUE {
            let mut pe: PROCESSENTRY32W = std::mem::zeroed();
            pe.dwSize = std::mem::size_of::<PROCESSENTRY32W>() as u32;
            let mut more = Process32FirstW(snap, &mut pe) != 0;
            while more {
                let end = pe
                    .szExeFile
                    .iter()
                    .position(|&c| c == 0)
                    .unwrap_or(pe.szExeFile.len());
                let name = String::from_utf16_lossy(&pe.szExeFile[..end]);
                if pe.th32ProcessID != me && name.eq_ignore_ascii_case("indesk-desktop.exe") {
                    pids.push(pe.th32ProcessID);
                }
                more = Process32NextW(snap, &mut pe) != 0;
            }
            CloseHandle(snap);
        }
    }
    if pids.is_empty() {
        return false;
    }

    // ② 枚举这些进程的可见顶层窗口，优先带标题的（真正的主窗口）
    let mut ctx = Ctx {
        pids,
        cands: Vec::new(),
    };
    unsafe {
        EnumWindows(Some(enum_cb), &mut ctx as *mut Ctx as isize);
    }
    let target = ctx
        .cands
        .iter()
        .find(|(_, has_title)| *has_title)
        .or_else(|| ctx.cands.first());
    let Some((hwnd, _)) = target else {
        return false;
    };
    let hwnd = *hwnd;

    unsafe {
        // 最小化的先还原（否则"叫出来"还是个最小化图标）
        ShowWindow(hwnd, SW_RESTORE);
        BringWindowToTop(hwnd);
        // SetForegroundWindow 常被系统的"前台锁"拒绝（调用方不是前台进程时），
        // 失败也无妨 —— 上面两步已让窗口可见、置顶。
        SetForegroundWindow(hwnd);
        // 兜底：任务栏图标持续闪烁直到窗口真的到前台。
        // 万一前面那步被拒，用户至少能看到任务栏在闪，不会"点了像没点"。
        let mut fi: FLASHWINFO = std::mem::zeroed();
        fi.cbSize = std::mem::size_of::<FLASHWINFO>() as u32;
        fi.hwnd = hwnd;
        fi.dwFlags = FLASHW_ALL | FLASHW_TIMERNOFG;
        fi.uCount = 0;
        fi.dwTimeout = 0;
        FlashWindowEx(&fi);
    }
    true
}

/// 找不到已有窗口时的兜底提示 —— 保证「双击」这个动作一定有可见反馈。
#[cfg(windows)]
fn notify_already_running() {
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        MessageBoxW, MB_ICONINFORMATION, MB_OK, MB_SETFOREGROUND, MB_TOPMOST,
    };
    let text: Vec<u16> = concat!(
        "InDesk 已经在运行中。\r\n\r\n",
        "如果桌面上看不到它的窗口，请在屏幕右下角的托盘区找到 InDesk 图标，双击即可打开。\r\n\r\n",
        "若要重新安装或升级：先右键托盘图标 → 退出，再运行安装包。"
    )
    .encode_utf16()
    .collect();
    let caption: Vec<u16> = "InDesk\0".encode_utf16().collect();
    unsafe {
        MessageBoxW(
            std::ptr::null_mut(),
            text.as_ptr(),
            caption.as_ptr(),
            MB_OK | MB_ICONINFORMATION | MB_SETFOREGROUND | MB_TOPMOST,
        );
    }
}

fn main() {
    // [20261021 七轮] **实时抓屏进程不允许被调度饿死**。
    //
    // 现场实测：本进程由计划任务 `InDeskGuard` 拉起（任务未显式设 Priority，默认 7），
    // 于是壳与核都继承 **BelowNormal（basePriority=6）**；同机的 ToDesk/billd-desk/
    // 浏览器全是 Normal，4 核 i5-4590 上抓屏线程被反复饿死，直接证据：
    //   `slow soft-encode 4181ms idr=false bytes=100400`（纯 CPU 编码，CPU 频率 100%、
    //   系统负载仅 22%，不是在算，而是在**等调度**）。
    // 这不是"硬件不行"，是优先级设置错了 —— 远程桌面被控端必须高于普通后台程序。
    #[cfg(windows)]
    {
        use windows::Win32::System::Threading::{
            GetCurrentProcess, SetPriorityClass, ABOVE_NORMAL_PRIORITY_CLASS,
        };
        let ok = unsafe { SetPriorityClass(GetCurrentProcess(), ABOVE_NORMAL_PRIORITY_CLASS) };
        indesk_desktop_lib::capture::capture_diag_pub(&format!(
            "进程优先级提升为 ABOVE_NORMAL（原继承自计划任务 BelowNormal）：{}",
            if ok.is_ok() { "成功" } else { "失败" }
        ));
    }
    // [20261020] panic 取证网：任何线程 panic 都落盘 capture_diag.log 再走默认 hook。
    // 背景：抓屏线程 stage=0（grab 外）心跳停 15s 判死，怀疑线程 panic 静默蒸发
    // —— panic 消息走 stderr，被壳（不读/丢弃）吞掉，不落盘永远查不到。
    #[cfg(windows)]
    {
        let default_hook = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            let loc = info
                .location()
                .map(|l| format!("{}:{}:{}", l.file(), l.line(), l.column()))
                .unwrap_or_default();
            let msg = info
                .payload()
                .downcast_ref::<String>()
                .cloned()
                .or_else(|| info.payload().downcast_ref::<&str>().map(|s| s.to_string()))
                .unwrap_or_else(|| "unknown payload".to_string());
            indesk_desktop_lib::capture::capture_diag_pub(&format!(
                "PANIC: {msg} @ {loc}"
            ));
            default_hook(info);
        }));
    }
    // [20261015 代理豁免] WebView2(Chromium) 默认读系统代理；用户机 Clash 等代理
    // 挂掉后 wss 信令会跟着"积极拒绝"全灭。自家域直连天经地义 → 强制 no-proxy。
    // 保留用户已设的调试参数（如 --remote-debugging-port），只追加不覆盖。
    #[cfg(windows)]
    {
        const KEY: &str = "WEBVIEW2_ADDITIONAL_BROWSER_ARGUMENTS";
        let merged = match std::env::var(KEY) {
            Ok(v) if !v.is_empty() => {
                if v.contains("--no-proxy-server") { v } else { format!("{} --no-proxy-server", v) }
            }
            _ => "--no-proxy-server".to_string(),
        };
        std::env::set_var(KEY, merged);
    }

    // [1.4.26] 壳自更新的「替换助手」模式：这个 exe 是壳自己的拷贝，被旧版本壳带着
    // `--indesk-apply-shell <新exe> <目标exe> <pid> <版本>` 启动。
    // 它只做一件事：等旧进程释放文件 → 备份 → 替换 → 拉起新版（失败回滚）。
    // **必须抢在单实例守卫之前返回**：助手不是"再开一个 InDesk"，
    // 一旦走后面的流程就会去抢互斥量、建托盘，既没必要也会让替换逻辑跑不起来。
    if let Some(req) = indesk_desktop_lib::shell_update::parse_applier_args(std::env::args()) {
        std::process::exit(indesk_desktop_lib::shell_update::run_applier(&req));
    }

    #[cfg(windows)]
    {
        // [20261015 混合壳] core 模式（--core）是 Electron 壳拉起的子进程，
        // 生命周期归壳管：不抢单实例互斥量（那是 GUI 壳的事）。
        let core_mode = std::env::args().any(|a| a == "--core");
        if !core_mode && !acquire_single_instance() {
            // 已有实例在跑：不再静默退出（那正是"双击没反应"的根因），
            // 改为把已有窗口拉到前台；实在拉不到就明确弹一个提示。
            if !activate_existing_instance() {
                notify_already_running();
            }
            std::process::exit(0);
        }
    }
    indesk_desktop_lib::run()
}
