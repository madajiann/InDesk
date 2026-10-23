//! InDesk 扩展命令：开机自启（注册表 HKCU Run）+ 共享弹窗自动确认（UIA）。

use serde_json::{json, Value};

const RUN_KEY: &str = r"HKCU\Software\Microsoft\Windows\CurrentVersion\Run";
const RUN_VALUE: &str = "InDesk";
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

fn current_exe_path() -> String {
    std::env::current_exe()
        .map(|p| format!("\"{}\"", p.display()))
        .unwrap_or_default()
}

#[tauri::command]
pub async fn set_autostart(enable: bool) -> Result<Value, String> {
    use std::os::windows::process::CommandExt;
    let exe = current_exe_path();
    if exe.is_empty() {
        return Err("无法获取程序路径".into());
    }
    let result = if enable {
        std::process::Command::new("reg")
            .args(["add", RUN_KEY, "/v", RUN_VALUE, "/t", "REG_SZ", "/d", &exe, "/f"])
            .creation_flags(CREATE_NO_WINDOW)
            .output()
    } else {
        std::process::Command::new("reg")
            .args(["delete", RUN_KEY, "/v", RUN_VALUE, "/f"])
            .creation_flags(CREATE_NO_WINDOW)
            .output()
    };
    match result {
        Ok(out) if out.status.success() => Ok(json!({ "ok": true, "enable": enable })),
        Ok(out) => Err(format!(
            "reg 失败: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        )),
        Err(e) => Err(format!("reg 启动失败: {e}")),
    }
}

#[tauri::command]
pub async fn get_autostart() -> Result<bool, String> {
    use std::os::windows::process::CommandExt;
    let result = std::process::Command::new("reg")
        .args(["query", RUN_KEY, "/v", RUN_VALUE])
        .creation_flags(CREATE_NO_WINDOW)
        .output();
    match result {
        Ok(out) => Ok(out.status.success()),
        Err(e) => Err(format!("reg 启动失败: {e}")),
    }
}

/// **[20260922 privacy] 设置隐私屏**：被控端本地画面黑化。
///
/// 语义：开启后，**主控端看到的画面全黑**，但流不断、帧号照常前进、
/// 尺寸格式不变 —— 因此不会触发任何"零帧/冻结"判据（见 capture.rs 的实现注释）。
///
/// 注意：这是**服务端（被控端）本地开关**，与主控端 UI 上的按钮一一对应，
/// 由主控端通过 `deskBehavior` 指令触发被控端调用本命令。
#[tauri::command]
pub async fn set_privacy_screen(on: bool) -> Result<Value, String> {
    let state = crate::capture::set_privacy_screen(on);
    Ok(json!({ "ok": true, "privacy": state }))
}

/// 查询当前隐私屏状态。
#[tauri::command]
pub async fn get_privacy_screen() -> Result<Value, String> {
    Ok(json!({ "privacy": crate::capture::privacy_screen() }))
}

/// **[20260922 文件传输] 把接收到的文件保存到磁盘（被控端）。**
///
/// ⚠️ 注意：这是给 `ipc.rs` 的 `ipc_dispatch` 调用的**普通函数**，不是 Tauri 命令 ——
/// 因为前端统一走 `ipcRendererInvoke → ipc_dispatch(channel)` 这条路，
/// 注册成独立命令反而会与既有 IPC 体系分裂（前端调不到）。
///
/// 落盘位置策略（不做"弹目录选择框"，因为被控端可能是无人值守场景）：
/// 统一存到 `%USERPROFILE%\Downloads\InDesk收件\`，重名自动追加序号。
/// 返回**实际写入的完整路径**，前端据此告知用户"存哪了"——
/// 绝不返回成功却不给出路径（否则用户找不到文件）。
///
/// `data_b64` 传 base64：WebView2 的 IPC 只走 JSON，无法直接传二进制。
/// 因此主控端的分片在**被控端前端**先组装成完整 Blob，再一次性 base64 传进来。
pub fn save_received_file(file_name: &str, data_b64: &str) -> Result<Value, String> {
    use std::io::Write;

    // 1. 先做安全清洗：**绝不允许**文件名里带路径分隔符或 `..`
    //    （否则被控端会被主控端写到任意位置 —— 这是必须堵的越权洞）。
    let safe_name = sanitize_file_name(file_name);
    if safe_name.is_empty() {
        return Err("文件名非法".into());
    }

    // 2. 目标目录：Downloads\InDesk收件
    let home = std::env::var("USERPROFILE").unwrap_or_else(|_| "C:\\".into());
    let dir = std::path::Path::new(&home)
        .join("Downloads")
        .join("InDesk收件");
    std::fs::create_dir_all(&dir).map_err(|e| format!("创建目录失败: {e}"))?;

    // 3. 重名自动追加序号（file.txt → file(1).txt）
    let mut path = dir.join(&safe_name);
    if path.exists() {
        let stem = std::path::Path::new(&safe_name)
            .file_stem()
            .map(|s| s.to_string_lossy().to_string())
            .unwrap_or_else(|| "file".into());
        let ext = std::path::Path::new(&safe_name)
            .extension()
            .map(|s| format!(".{}", s.to_string_lossy()))
            .unwrap_or_default();
        let mut i = 1;
        loop {
            let cand = dir.join(format!("{stem}({i}){ext}"));
            if !cand.exists() {
                path = cand;
                break;
            }
            i += 1;
            if i > 9999 {
                return Err("同名文件过多".into());
            }
        }
    }

    // 4. base64 解码
    let bytes = base64_decode(data_b64).map_err(|e| format!("数据解码失败: {e}"))?;

    // 5. 写盘
    let mut f = std::fs::File::create(&path).map_err(|e| format!("创建文件失败: {e}"))?;
    f.write_all(&bytes).map_err(|e| format!("写入失败: {e}"))?;
    f.flush().map_err(|e| format!("刷新失败: {e}"))?;

    Ok(json!({
        "ok": true,
        "path": path.to_string_lossy(),
        "bytes": bytes.len(),
    }))
}

/// 清洗文件名：只保留纯文件名部分，剥掉任何目录成分。
fn sanitize_file_name(name: &str) -> String {
    // 取最后一段（同时处理 / 与 \），再剔除 Windows 非法字符
    let base = name
        .rsplit(|c| c == '/' || c == '\\')
        .next()
        .unwrap_or("")
        .trim();
    // 去掉 .. 与非法字符
    let cleaned: String = base
        .chars()
        .filter(|c| !matches!(c, '<' | '>' | ':' | '"' | '|' | '?' | '*' | '\0'))
        .collect();
    let cleaned = cleaned.trim_matches('.').to_string();
    cleaned
}

/// 极简 base64 解码（标准字母表，容忍换行与填充）。
fn base64_decode(s: &str) -> Result<Vec<u8>, String> {
    const TABLE: &[u8; 64] =
        b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut lookup = [255u8; 256];
    for (i, &c) in TABLE.iter().enumerate() {
        lookup[c as usize] = i as u8;
    }
    let mut out = Vec::with_capacity(s.len() / 4 * 3);
    let mut buf: u32 = 0;
    let mut bits = 0u32;
    for &c in s.as_bytes() {
        if c == b'\n' || c == b'\r' || c == b' ' || c == b'=' {
            continue;
        }
        let v = lookup[c as usize];
        if v == 255 {
            return Err(format!("非法 base64 字符: {}", c as char));
        }
        buf = (buf << 6) | v as u32;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((buf >> bits) as u8);
        }
    }
    Ok(out)
}

/// 共享弹窗自动确认：被控端调 getDisplayMedia 前 invoke 本命令。
/// 后台起一个 PowerShell UIA 线程，在最长 ~14 秒内轮询 WebView2 的
/// "选择要共享的内容" 弹窗：点"整个屏幕"缩略图 → 点"共享"。
/// 不会弹黑窗（CREATE_NO_WINDOW），找不到弹窗就静默退出（无副作用）。
#[tauri::command]
pub async fn confirm_share_dialog() -> Result<Value, String> {
    use std::os::windows::process::CommandExt;
    let pid = std::process::id();
    let script = SHARED_DIALOG_PS.replace("{PID}", &pid.to_string());
    let spawn = std::thread::spawn(move || {
        let _ = std::process::Command::new("powershell")
            .args(["-NoProfile", "-ExecutionPolicy", "Bypass", "-Command", &script])
            .creation_flags(CREATE_NO_WINDOW)
            .output();
    });
    // 不阻塞前端：立刻返回，自动化在后台线程跑
    let _ = spawn;
    Ok(json!({ "ok": true }))
}

const SHARED_DIALOG_PS: &str = r#"
$ErrorActionPreference = 'SilentlyContinue'
Add-Type -AssemblyName UIAutomationClient
Add-Type -AssemblyName UIAutomationTypes
Add-Type @'
using System;
using System.Runtime.InteropServices;
public class InDeskMouse {
  [DllImport("user32.dll")] public static extern bool SetCursorPos(int x, int y);
  [DllImport("user32.dll")] public static extern void mouse_event(uint f, uint dx, uint dy, uint d, UIntPtr e);
  public static void Click(int x, int y) {
    SetCursorPos(x, y);
    System.Threading.Thread.Sleep(80);
    mouse_event(2, 0, 0, 0, UIntPtr.Zero);
    mouse_event(4, 0, 0, 0, UIntPtr.Zero);
  }
}
'@
$root = [System.Windows.Automation.AutomationElement]::RootElement
$pidCond = New-Object System.Windows.Automation.PropertyCondition([System.Windows.Automation.AutomationElement]::ProcessIdProperty, {PID})
function Find-El($parent, $type, $names) {
  foreach ($n in $names) {
    $c1 = New-Object System.Windows.Automation.PropertyCondition([System.Windows.Automation.AutomationElement]::NameProperty, $n)
    $c2 = New-Object System.Windows.Automation.AndCondition($c1, (New-Object System.Windows.Automation.PropertyCondition([System.Windows.Automation.AutomationElement]::ControlTypeProperty, $type)))
    $e = $parent.FindFirst([System.Windows.Automation.TreeScope]::Descendants, $c2)
    if ($e -ne $null) { return $e }
  }
  return $null
}
$deadline = (Get-Date).AddSeconds(14)
$done = $false
while (-not $done -and (Get-Date) -lt $deadline) {
  $wins = $root.FindAll([System.Windows.Automation.TreeScope]::Children, $pidCond)
  foreach ($w in $wins) {
    $screenBtn = Find-El $w ([System.Windows.Automation.ControlType]::Button) @('整个屏幕','Entire screen','Screen 1')
    if ($screenBtn -ne $null) {
      $r = $screenBtn.Current.BoundingRectangle
      if ($r.Width -gt 0) {
        [InDeskMouse]::Click([int]($r.X + $r.Width/2), [int]($r.Y + $r.Height/2))
        Start-Sleep -Milliseconds 700
        $share = Find-El $w ([System.Windows.Automation.ControlType]::Button) @('共享','Share')
        if ($share -ne $null -and $share.Current.IsEnabled) {
          $r2 = $share.Current.BoundingRectangle
          [InDeskMouse]::Click([int]($r2.X + $r2.Width/2), [int]($r2.Y + $r2.Height/2))
          $done = $true
          break
        }
      }
    }
  }
  if (-not $done) { Start-Sleep -Milliseconds 800 }
}
"#;
