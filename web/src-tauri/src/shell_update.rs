//! 壳（exe）自更新：**让软件自己把自己换掉**，客户什么都不用做。
//!
//! 为什么必须有这个模块：网页热更只换界面资源（HTML/JS/CSS），而壳层的修复
//! （输入注入、画面采集、单实例、托盘…）都在 `indesk-desktop.exe` 里，
//! 只能"换文件"。以前换文件要靠人到每台机器上双击 bat / 安装包 ——
//! 客户的机器谁来点？所以把"换文件"做成软件自己会做：
//!
//!   ① 前端从 `version.json` 发现 `shellVersion` 比本机新 → 下载新 exe → IPC 交给壳；
//!   ② 壳把新 exe 落到用户数据目录，再把**自己的一份拷贝**当"替换助手"启动；
//!   ③ 壳自己退出；
//!   ④ 助手等旧进程释放 exe → 备份 → 原子替换（目标在别的盘符时先复制到同卷再改名）→ 拉起新版；
//!      替换一旦失败**必须把旧版重新拉起来**，绝不留下"什么都打不开"的机器；
//!   ⑤ 拉起后做 8 秒存活检查，起不来就把备份换回去、重新拉起旧版 —— **不会把机器变砖**。
//!
//! 助手是同一个 exe 的拷贝，靠命令行开关 `--indesk-apply-shell` 进入助手模式，
//! 在 `main()` 里于**单实例守卫之前**分派：它不初始化 Tauri、不建托盘、不显示窗口。
//!
//! 两个设计取舍记在这（别改回去）：
//! * **"旧进程是否退出"用"能不能原子替换掉 dst"来判定**，而不是去查 PID：
//!   运行中的 exe 在 Windows 上是锁着的，`fs::rename` 替换一定失败；一旦成功，
//!   就说明旧进程已经放了文件（映像段关闭）——这既是判据，也正是我们要做的事，
//!   不必再引 OpenProcess/WaitForSingleObject 一套 FFI。
//! * **不用 .bat 助手**：bat 要吃引号/编码/杀软/防闪退的苦（本机安全策略还直接拉黑
//!   cmd），而传参交给 `Command` 是原生 argv，路径带空格中文都不怕。

use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

/// 助手模式开关。
pub const APPLIER_FLAG: &str = "--indesk-apply-shell";

/// 新 exe 体积下限：比这小的肯定不是我们的壳（防止半截下载 / 错误内容覆盖掉好文件）。
const MIN_EXE_LEN: usize = 3 * 1024 * 1024;

/// 替换重试上限与间隔：旧进程退出一般 <1s，给足 30s（60 × 500ms）。
/// 判据是"能不能原子替换掉 dst"——旧壳还在跑时文件被锁着，必然失败，重试到它退出。
const REPLACE_TRIES: u32 = 60;
const REPLACE_GAP_MS: u64 = 500;

/// 拉起新版后观察多久；在此期间退出则判定"起不来"→ 回滚。
const ALIVE_CHECK_MS: u64 = 8000;
const ALIVE_POLL_MS: u64 = 400;

#[cfg(windows)]
const CREATE_NO_WINDOW: u32 = 0x0800_0000;
#[cfg(windows)]
const DETACHED_PROCESS: u32 = 0x0000_0008;
#[cfg(windows)]
const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;

// ---------------------------------------------------------------- 助手参数

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApplyReq {
    /// 已经下载好的新 exe
    pub new_exe: PathBuf,
    /// 要替换掉的目标（= 正在运行的壳自己）
    pub dst_exe: PathBuf,
    /// 旧进程 PID（仅用于日志）
    pub pid: u32,
    /// 新版本号（仅用于日志/备份文件名）
    pub version: String,
}

/// 从命令行解析助手参数；不是助手模式返回 `None`。
///
/// 形如：`indesk-apply.exe --indesk-apply-shell <new> <dst> <pid> <version>`
pub fn parse_applier_args<I, S>(args: I) -> Option<ApplyReq>
where
    I: IntoIterator<Item = S>,
    S: AsRef<str>,
{
    let v: Vec<String> = args
        .into_iter()
        .map(|s| s.as_ref().to_string())
        .collect();
    let idx = v.iter().position(|a| a == APPLIER_FLAG)?;
    let at = |i: usize| v.get(idx + i).cloned().unwrap_or_default();
    let new_exe = at(1);
    let dst_exe = at(2);
    if new_exe.is_empty() || dst_exe.is_empty() {
        return None;
    }
    Some(ApplyReq {
        new_exe: PathBuf::from(new_exe),
        dst_exe: PathBuf::from(dst_exe),
        pid: at(3).parse().unwrap_or(0),
        version: at(4),
    })
}

// ---------------------------------------------------------------- 校验

/// 像不像一个 Windows PE 可执行文件（MZ + PE\0\0），顺便回报是不是 64 位。
///
/// 这一步是**防砖的第一道闸**：把半截下载 / HTML 错误页 / 空文件写进安装目录，
/// 下次开机就是"点了没反应"。宁可更新失败，也不能覆盖掉能跑的 exe。
pub fn check_pe(bytes: &[u8]) -> Result<bool, String> {
    if bytes.len() < 0x40 {
        return Err(format!("文件太小（{} 字节），不是可执行文件", bytes.len()));
    }
    if &bytes[0..2] != b"MZ" {
        return Err("缺少 MZ 头，不是 Windows 可执行文件".into());
    }
    let off = u32::from_le_bytes([bytes[0x3C], bytes[0x3D], bytes[0x3E], bytes[0x3F]]) as usize;
    if off + 6 > bytes.len() {
        return Err("PE 头偏移越界，文件已损坏".into());
    }
    if &bytes[off..off + 4] != b"PE\0\0" {
        return Err("缺少 PE 签名，文件已损坏".into());
    }
    let machine = u16::from_le_bytes([bytes[off + 4], bytes[off + 5]]);
    Ok(machine == 0x8664) // IMAGE_FILE_MACHINE_AMD64
}

/// 更新包整体校验：体积 + PE + （给了的话）字节数必须与 version.json 声明一致。
pub fn validate_new_exe(bytes: &[u8], expected_size: usize) -> Result<(), String> {
    if bytes.len() < MIN_EXE_LEN {
        return Err(format!(
            "新客户端程序只有 {} 字节，明显不完整（预期 ≥ {} 字节）",
            bytes.len(),
            MIN_EXE_LEN
        ));
    }
    if expected_size > 0 && bytes.len() != expected_size {
        return Err(format!(
            "字节数不符：收到 {}，服务器声明 {}（下载可能被截断）",
            bytes.len(),
            expected_size
        ));
    }
    let x64 = check_pe(bytes)?;
    if !x64 {
        return Err("不是 64 位可执行文件，拒绝替换".into());
    }
    Ok(())
}

/// 安装包（NSIS setup）的整体校验。返回 `true` = 本体恰好是 64 位 PE（仅供日志）。
///
/// ⚠️ **绝不能复用 `validate_new_exe`**（2026-09-28 现场事故根因）：
/// NSIS 安装器本体是 **32 位 PE（实测 machine=0x014C）**，而 `validate_new_exe`
/// 要求 0x8664（x64）—— 复用会把完全合法的安装包判成
/// 「不是 64 位可执行文件，拒绝替换」，于是**安装包自更新从 1.4.31 起一直是坏的**：
/// 客户端点「立即更新」必失败、界面停在旧版（现场反馈原文即该错误串）。
///
/// 真正决定「装进去的是不是 64 位」的是安装器自己（它落盘 x64 的 payload），
/// 外层不必再判一次位数。这里只承担**防砖**：挡半截下载 / HTML 错误页 / 空文件
/// —— 这些一旦写进更新目录，下次开机就是「点了没反应」。
pub fn validate_installer(bytes: &[u8], expected_size: usize) -> Result<bool, String> {
    if bytes.len() < MIN_EXE_LEN {
        return Err(format!(
            "安装包只有 {} 字节，明显不完整（预期 >= {} 字节）",
            bytes.len(),
            MIN_EXE_LEN
        ));
    }
    if expected_size > 0 && bytes.len() != expected_size {
        return Err(format!(
            "字节数不符：收到 {}，服务器声明 {}（下载可能被截断）",
            bytes.len(),
            expected_size
        ));
    }
    check_pe(bytes)
}

// ---------------------------------------------------------------- 落盘 + 启动助手

fn now_ts() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn write_log(log: &Path, msg: &str) {
    if let Some(p) = log.parent() {
        let _ = std::fs::create_dir_all(p);
    }
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(log)
    {
        let _ = writeln!(f, "[{}] {}", now_ts(), msg);
    }
}

/// 带无窗口参数的进程启动器（助手与新版都走它，保证不闪黑框）。
fn spawn_hidden(exe: &Path, args: &[String]) -> std::io::Result<std::process::Child> {
    let mut cmd = std::process::Command::new(exe);
    cmd.args(args);
    cmd.stdin(std::process::Stdio::null());
    cmd.stdout(std::process::Stdio::null());
    cmd.stderr(std::process::Stdio::null());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        // DETACHED_PROCESS：不继承父控制台，父进程退出后不受影响
        cmd.creation_flags(CREATE_NO_WINDOW | DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP);
    }
    cmd.spawn()
}

/// 把助手 exe 准备好（复制自己一份）。返回助手路径。
///
/// 正常情况下固定叫 `indesk-apply.exe`（下次直接覆盖）；万一上次的助手还在跑
/// （文件被占用、删不掉），退化成带时间戳的唯一名字，绝不因此让更新失败。
fn prepare_applier(dir: &Path, current_exe: &Path) -> Result<PathBuf, String> {
    let fixed = dir.join("indesk-apply.exe");
    if fixed.exists() {
        let _ = std::fs::remove_file(&fixed); // 删不掉说明旧助手还活着，走下面的唯一名
    }
    let target = if fixed.exists() {
        dir.join(format!("indesk-apply-{}.exe", now_ts()))
    } else {
        fixed
    };
    std::fs::copy(current_exe, &target)
        .map_err(|e| format!("准备替换助手失败：{e}"))?;
    Ok(target)
}

/// 清掉历史遗留（旧助手副本 / 上一轮的备份 / 残留的新 exe），只留最近的备份。
fn cleanup(dir: &Path, keep: Option<&Path>) {
    let Ok(rd) = std::fs::read_dir(dir) else { return };
    for e in rd.flatten() {
        let p = e.path();
        if let Some(k) = keep {
            if p == k {
                continue;
            }
        }
        let name = e.file_name().to_string_lossy().to_string();
        let is_junk = name.starts_with("indesk-apply-")
            || name.starts_with("indesk-desktop-new")
            || name.starts_with("indesk-desktop-swap")
            || name.starts_with("backup-");
        if is_junk {
            let _ = std::fs::remove_file(&p);
        }
    }
}

/// 两个路径是否在同一个卷上。
///
/// 为什么必须判：替换用的是"原子改名"，而**跨卷改名在 Windows 上恒定失败**
/// （os error 17 / `ERROR_NOT_SAME_DEVICE`）。本机 2026-09-27 实测就踩在这：
/// 新程序下载落到 `C:\Users\<用户>\AppData\Local\com.indesk.app\shell\`，
/// 主程序却在 `D:\InDesk\`，助手重试 60 次 30 秒全部报
/// "系统无法将文件移到不同的磁盘驱动器" → 更新失败，而壳此时已经退出了，
/// 客户机器上什么都不剩（表现为"点更新后窗口消失、再也弹不出来"）。
///
/// 判据只看路径前缀（盘符 / UNC 主机 / 卷 GUID），不解析挂载点 —— 这里只用来决定
/// "要不要先复制到同卷再改名"，判错最多多复制一次 10MB，没有任何副作用。
fn same_volume(a: &Path, b: &Path) -> bool {
    let vol = |p: &Path| -> Option<String> {
        p.components().find_map(|c| match c {
            std::path::Component::Prefix(pfx) => {
                Some(pfx.as_os_str().to_string_lossy().to_uppercase())
            }
            _ => None,
        })
    };
    match (vol(a), vol(b)) {
        (Some(x), Some(y)) => x == y,
        // 没有前缀（相对路径 / 判断不了）时保守当作不同卷：先复制再改名总是安全的。
        _ => false,
    }
}

/// 下载好的新 exe 落盘，并把替换助手启动起来。
///
/// `dir` 由调用方给（`<app_local_data_dir>/shell`），本模块刻意不依赖 Tauri，
/// 便于单测与离线复现。
pub fn stage_and_spawn(
    dir: &Path,
    current_exe: &Path,
    bytes: &[u8],
    version: &str,
    expected_size: usize,
) -> Result<PathBuf, String> {
    validate_new_exe(bytes, expected_size)?;
    std::fs::create_dir_all(dir).map_err(|e| format!("创建更新目录失败：{e}"))?;

    let log = dir.join("update.log");
    let new_exe = dir.join("indesk-desktop-new.exe");
    std::fs::write(&new_exe, bytes).map_err(|e| format!("写入新客户端程序失败：{e}"))?;

    let staged = new_exe
        .metadata()
        .map(|m| m.len())
        .unwrap_or_default();
    if staged != bytes.len() as u64 {
        return Err(format!("落盘后大小不符：{} != {}", staged, bytes.len()));
    }

    let applier = prepare_applier(dir, current_exe)?;

    let args: Vec<String> = vec![
        APPLIER_FLAG.to_string(),
        new_exe.to_string_lossy().to_string(),
        current_exe.to_string_lossy().to_string(),
        std::process::id().to_string(),
        version.to_string(),
    ];
    let child = spawn_hidden(&applier, &args)
        .map_err(|e| format!("启动替换助手失败：{e}"))?;

    write_log(
        &log,
        &format!(
            "staged v{version}: {} bytes -> {} ; applier pid={} ; shell pid={}",
            bytes.len(),
            new_exe.display(),
            child.id(),
            std::process::id()
        ),
    );
    Ok(new_exe)
}

// ---------------------------------------------------------------- 助手本体

/// 助手主流程（在 main() 里被调用，之后进程直接退出，不初始化 Tauri）。
pub fn run_applier(req: &ApplyReq) -> i32 {
    let dir = req
        .new_exe
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("."));
    let log = dir.join("update.log");
    write_log(
        &log,
        &format!(
            "applier start: v{} new={} dst={} shell_pid={}",
            req.version,
            req.new_exe.display(),
            req.dst_exe.display(),
            req.pid
        ),
    );

    if !req.new_exe.is_file() {
        write_log(&log, "applier abort: 新程序不存在");
        return 2;
    }

    // ① 备份旧 exe（best effort：备份失败也继续，但有备份就能回滚）
    let backup = dir.join(format!("backup-{}.exe", req.version));
    let backed_up = match std::fs::copy(&req.dst_exe, &backup) {
        Ok(n) => {
            write_log(&log, &format!("backup -> {} ({n} bytes)", backup.display()));
            true
        }
        Err(e) => {
            write_log(&log, &format!("backup 失败（继续）: {e}"));
            false
        }
    };

    // ② 先把"待替换源"弄到与目标同卷 —— 跨卷改名恒定失败（os error 17），
    //    这是本机实测的失败点（C: AppData → D: 安装目录）。
    let mut source = req.new_exe.clone();
    let new_len = req.new_exe.metadata().map(|m| m.len()).unwrap_or(0);
    if !same_volume(&source, &req.dst_exe) {
        let swap = req
            .dst_exe
            .parent()
            .map(|p| p.join("indesk-desktop-swap.exe"))
            .unwrap_or_else(|| PathBuf::from("indesk-desktop-swap.exe"));
        write_log(
            &log,
            &format!(
                "跨卷（{} -> {}）：先复制到同卷 {} 再改名",
                req.new_exe.display(),
                req.dst_exe.display(),
                swap.display()
            ),
        );
        match std::fs::copy(&source, &swap) {
            Ok(n) if n == new_len && n > 0 => {
                write_log(&log, &format!("已落到同卷（{n} bytes）"));
                source = swap;
            }
            Ok(n) => {
                write_log(&log, &format!("复制后大小不符：{n} != {new_len} —— 放弃本次更新"));
                let _ = spawn_hidden(&req.dst_exe, &[]);
                return 3;
            }
            Err(e) => {
                write_log(
                    &log,
                    &format!("跨卷落盘失败：{e} —— 放弃本次更新，恢复旧版运行"),
                );
                let _ = spawn_hidden(&req.dst_exe, &[]);
                return 3;
            }
        }
    }

    // ③ 等旧进程释放文件，然后原子替换。
    //    判据 = fs::rename 能不能成功（运行中的 exe 锁着，必然失败）。
    let deadline = Instant::now() + Duration::from_millis(REPLACE_TRIES as u64 * REPLACE_GAP_MS);
    let mut replaced = false;
    let mut last_err = String::new();
    let mut tries = 0u32;
    while Instant::now() < deadline {
        tries += 1;
        match std::fs::rename(&source, &req.dst_exe) {
            Ok(_) => {
                replaced = true;
                break;
            }
            Err(e) => {
                last_err = e.to_string();
                std::thread::sleep(Duration::from_millis(REPLACE_GAP_MS));
            }
        }
    }
    if !replaced {
        write_log(
            &log,
            &format!("替换失败（重试 {tries} 次）: {last_err}"),
        );
        // 【不许变砖】前端点了"更新完成、客户端正在重启"之后就催壳退进程了
        // （见前端 applyShellUpdateFlow），而这里的 return 一旦直接走人，
        // 客户机器上就什么都不剩 —— 2026-09-27 实机现象正是如此：点更新 →
        // 窗口消失再也不出来 → 再双击图标还是旧版、又提示更新。
        // 此时 dst 文件**没有被改动过**，把它拉起来就能恢复可用状态。
        match spawn_hidden(&req.dst_exe, &[]) {
            Ok(_) => write_log(&log, "已重新拉起旧版，客户端保持可用（可稍后再试更新）"),
            Err(e) => write_log(
                &log,
                &format!("拉起旧版也失败：{e} —— 需要人工双击主程序"),
            ),
        }
        return 3;
    }

    // ④ 校验替换结果
    let dst_len = req.dst_exe.metadata().map(|m| m.len()).unwrap_or(0);
    if dst_len != new_len || dst_len == 0 {
        write_log(
            &log,
            &format!("替换后大小异常：dst={dst_len} new={new_len}，回滚"),
        );
        if backed_up {
            let _ = std::fs::rename(&backup, &req.dst_exe);
            let _ = spawn_hidden(&req.dst_exe, &[]);
        }
        return 4;
    }
    write_log(&log, &format!("replaced ok ({dst_len} bytes, {tries} 次尝试)"));

    // ⑤ 拉起新版，并观察是否真的活着（起不来就回滚，别让客户机器变砖）
    let mut child = match spawn_hidden(&req.dst_exe, &[]) {
        Ok(c) => c,
        Err(e) => {
            write_log(&log, &format!("拉起新程序失败：{e}，回滚"));
            if backed_up {
                let _ = std::fs::rename(&backup, &req.dst_exe);
                let _ = spawn_hidden(&req.dst_exe, &[]);
            }
            return 5;
        }
    };
    let until = Instant::now() + Duration::from_millis(ALIVE_CHECK_MS);
    while Instant::now() < until {
        std::thread::sleep(Duration::from_millis(ALIVE_POLL_MS));
        match child.try_wait() {
            Ok(Some(status)) => {
                let why = format!("新程序启动后立即退出（{status}），回滚到旧版");
                write_log(&log, &why);
                if backed_up {
                    let _ = std::fs::rename(&backup, &req.dst_exe);
                    let _ = spawn_hidden(&req.dst_exe, &[]);
                    write_log(&log, "已回滚并重新拉起旧版");
                }
                return 6;
            }
            Ok(None) => {}
            Err(e) => {
                write_log(&log, &format!("观察新进程出错（当成功处理）: {e}"));
                break;
            }
        }
    }

    write_log(&log, &format!("done: 已升级到 v{} 并重新启动", req.version));
    cleanup(&dir, Some(&backup));
    0
}

// ---------------------------------------------------------------- 安装包模式（1.4.31）

/// [1.4.31] 安装包模式的自更新：把下载好的 NSIS 安装包落盘 → 以 `/S /R` 静默启动。
///
/// 为什么改用安装包（取代 1.4.26~1.4.30 的"跨盘原子改名"）：
/// 旧做法要自己处理跨卷复制、备份、回滚、失败拉起旧版 —— 任何一步漏掉就会把
/// 客户机器变成"点了更新什么都没有"（2026-09-27 实机事故，红线 §39）。安装包是
/// 成熟的安装器，这些它全包了，还能顺带补齐快捷方式 / 卸载项 / 注册表。
///
/// 三个关键行为已按 Tauri NSIS 模板逐条核对（从 CLI 内嵌的 `utils.nsh` 原文确认，
/// 不是推测）：
/// * `/S` 静默：安装程序发现 `indesk-desktop.exe` 还在跑会**直接结束它**
///   （`CheckIfAppIsRunning` 宏里的 `IfSilent kill` 分支），不会弹窗卡住；
/// * `/R` 装完自动重启（`installer.nsi` 的 `.onInstSuccess` 读到 `/R` 后 `RunAsUser`）；
/// * 就地覆盖：`RestorePreviousInstallLocation` 读 `HKCU\Software\InDesk\InDesk`，
///   老机器的 `D:\InDesk` 会被原样沿用，不会装出第二份。
pub fn stage_and_run_installer(
    dir: &Path,
    bytes: &[u8],
    version: &str,
    expected_size: usize,
) -> Result<PathBuf, String> {
    // [1.4.36] 安装包本体是 **32 位 PE**，必须走安装包专用校验。
    // 原来这里调 validate_new_exe（要求 x64）→ 合法安装包被判「不是 64 位可执行文件」，
    // 安装包自更新从 1.4.31 起一直失败。详见 validate_installer 的注释。
    let installer_x64 = validate_installer(bytes, expected_size)?;

    std::fs::create_dir_all(dir).map_err(|e| format!("创建更新目录失败：{e}"))?;

    let log = dir.join("update.log");
    let setup = dir.join(format!("InDesk-setup-{version}.exe"));
    std::fs::write(&setup, bytes).map_err(|e| format!("写入安装包失败：{e}"))?;

    let staged = setup.metadata().map(|m| m.len()).unwrap_or_default();
    if staged != bytes.len() as u64 {
        return Err(format!("落盘后大小不符：{} != {}", staged, bytes.len()));
    }

    // 清掉上一轮留下的安装包副本（旧安装程序可能还占着文件，删不掉就跳过）
    if let Ok(rd) = std::fs::read_dir(dir) {
        for e in rd.flatten() {
            let p = e.path();
            if p == setup {
                continue;
            }
            let name = e.file_name().to_string_lossy().to_string();
            if name.starts_with("InDesk-setup-") && name.ends_with(".exe") {
                let _ = std::fs::remove_file(&p);
            }
        }
    }

    // 静默安装 + 装完自动拉起：安装器自己会结束旧进程、就地覆盖、重建快捷方式
    let child = spawn_hidden(&setup, &["/S".to_string(), "/R".to_string()])
        .map_err(|e| format!("启动安装程序失败：{e}"))?;

    write_log(
        &log,
        &format!(
            "installer staged v{version}: {} bytes -> {} ; setup pid={} ; shell pid={} ; pe_x64={}",
            bytes.len(),
            setup.display(),
            child.id(),
            std::process::id(),
            installer_x64
        ),
    );
    Ok(setup)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fake_pe(machine: u16, len: usize) -> Vec<u8> {
        let mut b = vec![0u8; len.max(0x80)];
        b[0] = b'M';
        b[1] = b'Z';
        b[0x3C] = 0x40;
        b[0x40] = b'P';
        b[0x41] = b'E';
        b[0x42] = 0;
        b[0x43] = 0;
        b[0x44] = (machine & 0xFF) as u8;
        b[0x45] = (machine >> 8) as u8;
        b
    }

    #[test]
    fn parse_args_ok() {
        let a = vec![
            "indesk-apply.exe",
            APPLIER_FLAG,
            r"C:\x\new.exe",
            r"D:\InDesk\indesk-desktop.exe",
            "24064",
            "1.4.26",
        ];
        let r = parse_applier_args(a).expect("应能解析");
        assert_eq!(r.new_exe, PathBuf::from(r"C:\x\new.exe"));
        assert_eq!(r.dst_exe, PathBuf::from(r"D:\InDesk\indesk-desktop.exe"));
        assert_eq!(r.pid, 24064);
        assert_eq!(r.version, "1.4.26");
    }

    #[test]
    fn parse_args_none_when_not_applier() {
        assert!(parse_applier_args(vec!["indesk-desktop.exe"]).is_none());
        // 缺路径也不算助手模式
        assert!(parse_applier_args(vec!["x.exe", APPLIER_FLAG]).is_none());
    }

    #[test]
    fn pe_checks() {
        assert_eq!(check_pe(&fake_pe(0x8664, 0x100)).unwrap(), true);
        assert_eq!(check_pe(&fake_pe(0x014C, 0x100)).unwrap(), false); // 32 位
        assert!(check_pe(b"hello").is_err());
        let mut bad = fake_pe(0x8664, 0x100);
        bad[0] = b'X';
        assert!(check_pe(&bad).is_err());
    }

    #[test]
    fn validate_rejects_short_and_truncated() {
        // 体积不足
        assert!(validate_new_exe(&fake_pe(0x8664, 0x100), 0).is_err());
        // 体积够但服务器声明更大 → 判定截断
        let big = fake_pe(0x8664, MIN_EXE_LEN + 10);
        assert!(validate_new_exe(&big, MIN_EXE_LEN + 999).is_err());
        assert!(validate_new_exe(&big, big.len()).is_ok());
        // 32 位拒收
        let x86 = fake_pe(0x014C, MIN_EXE_LEN + 10);
        assert!(validate_new_exe(&x86, x86.len()).is_err());
    }

    /// 回归测试：**安装包本体是 32 位 PE，必须放行**。
    /// 这条测试的意义 = 焊死 2026-09-28 的事故：只要有人把 validate_installer
    /// 换回 validate_new_exe（或给它加位数检查），这里就会红。
    #[test]
    fn installer_accepts_32bit_pe_but_still_blocks_junk() {
        let x86 = fake_pe(0x014C, MIN_EXE_LEN + 10);
        assert_eq!(validate_installer(&x86, x86.len()).unwrap(), false);
        assert_eq!(validate_installer(&x86, 0).unwrap(), false);

        let x64 = fake_pe(0x8664, MIN_EXE_LEN + 10);
        assert_eq!(validate_installer(&x64, x64.len()).unwrap(), true);

        // 体积不足 / 截断 / 非 PE 仍然要挡住（防砖职责不变）
        assert!(validate_installer(&fake_pe(0x014C, 0x100), 0).is_err());
        assert!(validate_installer(&x86, x86.len() + 999).is_err());
        assert!(validate_installer(b"not a pe at all", 0).is_err());
    }
}
