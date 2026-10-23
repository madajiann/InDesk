//! InDesk 桌面客户端 —— Tauri 外壳。
//!
//! 与原 Electron 版保持同一套前端代码与 IPC 协议：
//! 前端通过 `invoke('ipc_dispatch', { channel, payload })` 调用本进程，
//! 本进程通过事件（channel 名与 Electron 的 `response_*` 一致）回推结果。
//!
//! 主窗口在这里用代码创建（没写在 tauri.conf.json 的 `app.windows` 里），原因有两个：
//!  1. 界面热更：必须先判断"存在完整热更包"，才能决定主窗口是加载内置资源还是热更目录，
//!     避免"先用内置资源建窗口、再 navigate 跳转"造成的加载竞态；
//!  2. WebView2 参数合并：tauri.conf 里一旦写了 `additionalBrowserArgs`，WebView2 就会
//!     忽略 `WEBVIEW2_ADDITIONAL_BROWSER_ARGUMENTS` 环境变量，线上就没法用
//!     `--remote-debugging-port` 排查问题了；放在代码里合并则两个来源都能用。

/// [1.4.5 剪贴板同步] 壳侧剪贴板读写（只做纯文本）。
/// 刻意**不引用 crate::** —— `G:\InDesk\lab` 的取证 bin 用 `#[path]` 直接编译
/// 同一份文件并真机跑过（11/11 + 9/9 交叉验证），所以"验证过的"与"发布的"是同一个文件。
pub mod core_bridge;
mod clipboard;
mod commands;
mod gesture;
mod input;
mod ipc;
/// 壳（exe）自更新：软件自己下载新版并把自己替换掉（客户什么都不用做）。
/// 设为 pub 是为了让 `main.rs` 能在单实例守卫之前分派"替换助手"模式。
pub mod shell_update;
// [20260918 §46 回滚] Interception 驱动级注入**恢复编译**。
// 依据：用户实测"睡醒版（c6c621d0）能操作"，而该版二进制内含完整 interception 调用
// （interception_create_context / interception_send / set_filter），即"能操作"建立在
// 驱动通道上；§40 把它当"游戏问题"一起删掉 → 远程输入失效。此处恢复。
mod interception;

/// [1.4.38 privacy v2] 隐私屏原生遮罩窗（本机黑屏、抓屏不可见）。
/// 被 `capture::set_privacy_screen` 委托调用。pub 仅为 examples/privacy_probe.rs 可用。
#[cfg(windows)]
pub mod privacy_overlay;

/// [1.4.39 privacy v3] 隐私屏输入锁（低级钩子：吞被控端物理键鼠、放行注入）。
/// 与 privacy_overlay 配套，被 `capture::set_privacy_screen` 一起调度。
/// pub 仅为 examples/input_probe.rs 取证可用。
#[cfg(windows)]
pub mod input_lock;

/// 无人值守「静默取屏」（Windows 专用，原理见 capture.rs 顶部长注释）。
/// 设为 pub 是为了让 examples/capture_probe.rs 能直接对**真实代码**做链路自检。
#[cfg(windows)]
pub mod capture;
/// DXGI Desktop Duplication 抓屏（GPU 取帧；不可用时由 capture.rs 回退 GDI）。
#[cfg(windows)]
pub mod dxgi;
/// 壳侧 NVENC 硬件 H.264 编码（阶段①「对标 ToDesk」的画面引擎）。
/// 不可用时（无 NVIDIA 卡 / 驱动无 NVENC）capture.rs 自动回退到 JPEG 或直送。
#[cfg(windows)]
pub mod h264;
/// [20261021 七轮] CPU 软件 H.264（OpenH264）：不申请显存、不依赖 NVENC，
/// 用于绕开本机 1GB 显存下 NVENC 的周期性长停顿（对照实验见文件头）。
#[cfg(windows)]
pub mod soft264;
#[cfg(not(windows))]
mod capture {
    #[derive(Clone)]
    pub struct StartOptions {
        pub max_width: u32,
        pub quality: u8,
        pub fps: u32,
        /// "jpeg"（默认）/ "bgra" / "nv12" / "h264"；非 Windows 平台一律不支持，
        /// 字段仅为协议对齐。
        pub mode: String,
        /// H.264 目标码率 kbps（非 Windows 不生效，字段仅为协议对齐）。
        pub bitrate_kbps: u32,
    }
    impl Default for StartOptions {
        fn default() -> Self {
            Self {
                max_width: 1920,
                quality: 70,
                fps: 15,
                mode: "jpeg".into(),
                bitrate_kbps: 4000,
            }
        }
    }
    pub fn start(_opt: StartOptions) -> Result<(u16, u32, u32), String> {
        Err("当前平台不支持静默取屏".into())
    }
    pub fn stop() {}
    pub fn is_running() -> bool {
        false
    }
    pub fn port() -> u16 {
        0
    }
}

use serde_json::Value;
use tauri::{Emitter, Manager, WebviewUrl, WebviewWindowBuilder};

/// 平台标识，与原 Electron 主进程 `process.platform` 取值一致。
pub fn platform_name() -> &'static str {
    if cfg!(target_os = "windows") {
        "win32"
    } else if cfg!(target_os = "macos") {
        "darwin"
    } else {
        "linux"
    }
}

/// WebView2 浏览器参数默认值。
///
/// ⚠️ 关于"静默取屏"的历史结论（2026-09-13 最终定位，不要再走回头路）：
///
/// * `--auto-select-desktop-capture-source` 是 **Chrome 浏览器层** 的开关（实现在
///   chrome/browser 的 desktop media picker 里）；WebView2 基于 content 层，自带一套
///   由宿主控制的取屏 UI，**根本不读这个参数** —— v1.2.2 / v1.2.3 两次尝试弹窗依旧，
///   根因在此（与参数值里的空格无关）。现已移除。
///
/// * WebView2 的 `ICoreWebView2_27::ScreenCaptureStarting` 事件**同样做不到**自动选源：
///   查官方文档，其事件参数 `ICoreWebView2ScreenCaptureStartingEventArgs` 的成员只有
///   `Cancel`（取消取屏，页面收到 NotAllowedError）与 `Handled`（语义仅为"阻止下游
///   事件处理器被调用"的事件路由开关），**全接口不存在任何指定捕获源的入口** ——
///   所以 v1.2.4 依赖它也是无效的，弹窗照旧。
///
/// * 最终方案：无人值守改为**壳侧自绘抓屏**（见 capture.rs：GDI 抓屏 → JPEG →
///   本机 127.0.0.1 的 MJPEG），前端把帧画到 canvas 后用 `canvas.captureStream()`
///   生成标准视频轨交给 WebRTC。全程**不调用 `getDisplayMedia`**，与浏览器取屏 UI 无关。
///
/// `--ignore-certificate-errors`：私有化部署走自签证书（Caddy + cert.pem），
/// WebView2 对自签证书一律严格拒绝（Failed to fetch），没有浏览器那种"继续前往"；
/// 原 Electron 壳对此是放行的。内网自签部署下必须忽略证书错误，
/// 否则登录 API / wss / 热更下载全部不通。
///
/// `--disable-background-timer-throttling` 等三条 [20260925 断线根治]：
/// 被控端窗口最小化/被遮挡后，WebView2 默认把页面 JS 定时器冻结到分钟级 ——
/// socket.io 心跳随之停发，服务端 45s 判死踢线；重连定时器同样被节流，
/// 表现为「进程活着但零 TCP 连接、服务端查无此设备、怎么都连不上」
/// （10:13 重启 5 分钟后实测复现）。三条参数分别禁掉：隐藏页定时器节流、
/// 被遮挡页渲染进程挂起、渲染进程整体降级。**必须每个 webview 都带上**。
const DEFAULT_BROWSER_ARGS: &str = "--no-proxy-server --disable-features=msWebOOUI,msPdfOOUI,msSmartScreenProtection --ignore-certificate-errors --disable-background-timer-throttling --disable-backgrounding-occluded-windows --disable-renderer-backgrounding";

/// 需要线上排查时，可用环境变量追加参数，例如：
/// `set INDESK_BROWSER_ARGS=--remote-debugging-port=9333`
///
/// ⚠️ 必须被**每一个** webview 使用（主窗口 + 所有子窗口都调它）。
/// 原因：wry 对每个 webview 都会调用 `CreateCoreWebView2EnvironmentWithOptions`，
/// 而 WebView2 要求"共享同一个用户数据目录的环境，AdditionalBrowserArguments 必须完全一致"，
/// 否则第二个环境创建失败 -> 子窗口的 webview 建不出来、窗口随即被销毁（表现为"窗口一闪就没"）。
pub fn browser_args() -> String {
    match std::env::var("INDESK_BROWSER_ARGS") {
        Ok(extra) if !extra.trim().is_empty() => format!("{DEFAULT_BROWSER_ARGS} {}", extra.trim()),
        _ => DEFAULT_BROWSER_ARGS.to_string(),
    }
}

use std::sync::atomic::{AtomicBool, Ordering};

/// 无人值守开关（前端走 IPC 通道 `setUnattended` 同步）。
static UNATTENDED: AtomicBool = AtomicBool::new(false);

/// 前端同步无人值守状态。
pub fn set_unattended(enabled: bool) {
    UNATTENDED.store(enabled, Ordering::Relaxed);
}

/// 当前是否处于无人值守模式。
pub fn unattended() -> bool {
    UNATTENDED.load(Ordering::Relaxed)
}

#[tauri::command]
async fn ipc_dispatch(
    app: tauri::AppHandle,
    channel: String,
    payload: Value,
) -> Result<Value, String> {
    ipc::dispatch(&app, &channel, payload).await
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    // [20261015 混合壳] core 模式：`--core` 由 Electron 壳传入。
    // 无窗口运行（不创建任何 WebviewWindow → 不加载 WebView2），
    // 本地 axum 服务接管 web UI 与 IPC（见 core_bridge.rs）。
    let core_mode = std::env::args().any(|a| a == "--core");
    let core_port: u16 = std::env::args()
        .position(|a| a == "--core-port")
        .and_then(|i| std::env::args().nth(i + 1))
        .and_then(|s| s.parse().ok())
        .unwrap_or(23654);

    tauri::Builder::default()
        // 界面热更协议：Windows 下访问地址为 http://indesk.localhost/，
        // 只有在用户数据目录里存在完整热更包时才会被主窗口使用。
        .register_uri_scheme_protocol("indesk", |ctx, request| {
            // query 也要带上：取帧通道用 `?after=<seq>` 判断"这帧是不是已经拿过了"
            // （直送模式下单帧 4MB，帧号没变就不该重传），`?meta=1` 查询发布格式。
            ipc::serve_web_update(
                ctx.app_handle(),
                request.uri().path(),
                request.uri().query(),
            )
        })
        .invoke_handler(tauri::generate_handler![
            ipc_dispatch,
            commands::set_autostart,
            commands::get_autostart,
            commands::confirm_share_dialog,
            commands::set_privacy_screen,
            commands::get_privacy_screen
        ])
        .setup(move |app| {
            // 闭包 'static 约束：move 拿走所有权（u16/bool 均 Copy）
            let core_mode = core_mode;
            let core_port = core_port;
            // [20260918 §46 回滚] 恢复启动探测驱动可用性（并落盘诊断）。
            // 输入层 internal：`interception::available()` 懒加载 + 永久缓存，
            // 这里提前探一次是为了让首个输入事件不必承担 DLL 加载延迟。
            #[cfg(windows)]
            {
                crate::interception::startup_probe_log();
            }
            // 过期热更包清理副作用保留：比当前壳旧的热更包会被删除，
            // 之后 serve_web_update 自然回落到壳内置资源。
            let _ = ipc::web_update_ready(app.handle());

            // [20261015 混合壳] core 模式：启动本地桥服务（ws IPC + web UI），
            // 跳过主窗口与托盘（这两样归 Electron 壳）。
            if core_mode {
                core_bridge::start(app.handle().clone(), core_port);
                return Ok(());
            }

            // 主窗口地址：**永远**走 indesk 协议（20260913f 起）。
            // 该协议处理器自带两级回落（热更包 → 壳内置资源），所以无热更包时
            // 也走这里 —— 之前"无热更包回落 tauri.localhost"会让
            // /__capture/frame 帧通道失效（那条通道不经过 indesk 协议处理器）。
            // [20261017] `--hidden` 静默启动（主控壳拉起被控）：不创建托盘图标，
            // 全机只保留主控壳那一个托盘（用户反馈"托盘出现两个图标，一新一旧"）。
            // 直接双击本模块（无 --hidden）时保留旧界面+托盘作调试入口。
            let silent_launch = std::env::args().any(|a| a == "--hidden");
            let url = WebviewUrl::External(
                "http://indesk.localhost/index.html"
                    .parse()
                    .map_err(|e| format!("热更地址解析失败: {e}"))?,
            );

            let win = WebviewWindowBuilder::new(app, "main", url)
                .title("InDesk")
                // [20260928b] 环境标记：**在页面脚本之前**注入，前端据此判定"有无壳"。
                // 事故背景：前端 `isWebEnv` 依赖 `window.__TAURI__.core.invoke` 在模块
                // 求值那一刻已存在；任一环节偏差（开关/协议/时序）都会让电脑端被误判成
                // 网页版，于是网页版那条**竖版数据栏**跟着上电脑端 —— 用户 09-28 报障。
                // 初始化脚本由 WebView2 在每个文档创建时先于任何页面脚本执行，时序上
                // 不可能被抢跑；前端见 utils/ipc-adapter.ts 的 isShellEnv。
                // [1.4.34] 除了标记环境，再注入一条**版本无关的硬否决**：
                //   网页版那条竖版数据栏（`.pc-databar` / `.pc-databar-fold`）在壳里
                //   一律 `display:none !important`。
                //
                // 为什么必须放在壳侧而不是前端：2026-09-28 用户报「连接动画百分比旁边
                // 还有数据栏」时，前端的闸门（`isWebEnv` 判据 + `pc-db-off`）本身是对的
                // （实测 `pcConnShow === isWebEnv && !pcVideoReady`，与 `pc-db-off` 同源同步），
                // 但它**只对新前端生效**；只要客户端跑的是旧前端、或环境判据被误判成网页版，
                // 竖栏就会冒出来。壳侧注入的 CSS 作用于**任何**前端版本，判据错也拦得住。
                .initialization_script(
                    r#"(function(){
  window.__INDESK_SHELL__ = 'desktop';
  function veto(){
    try{
      if (document.getElementById('indesk-shell-veto')) return true;
      var h = document.head || document.documentElement;
      if (!h) return false;
      var s = document.createElement('style');
      s.id = 'indesk-shell-veto';
      s.textContent = '.pc-databar,.pc-databar-fold{display:none !important}';
      h.appendChild(s);
      document.documentElement.setAttribute('data-indesk-shell','1');
      return true;
    }catch(e){ return false; }
  }
  if (!veto()) {
    var mo = new MutationObserver(function(){ if (veto()) mo.disconnect(); });
    mo.observe(document, {childList:true, subtree:true});
    document.addEventListener('readystatechange', veto, true);
  }
})();"#,
                )
                // [20260914r 第 12 条] 窗口尺寸对齐 ToDesk 主窗口。
                // 实测依据（_verify/_recon_todesk_place.py / _recon_todesk_disambig.py）：
                //   ToDesk 主窗口 = class H-SMILE-FRAME、IsWindowVisible=True、
                //   WS_EX_APPWINDOW、owner/parent 均为 0 的那只；
                //   GetWindowPlacement().rcNormalPosition = 737,170 900x568，
                //   系统 DPI=96（缩放 1.0）→ 逻辑尺寸同为 900x568。
                //   另一只 747x610 的 H-SMILE-FRAME 是 IsWindowVisible=False 的隐藏窗，排除。
                // 本壳 decorations(false)，内外尺寸一致，故 inner_size 直接取 900x568。
                .inner_size(900.0, 568.0)
                .min_inner_size(800.0, 500.0)
                // 必须 resizable(true)：resizable(false) 会去掉 WS_MAXIMIZEBOX，
                // 程序化最大化（windowToggleMaximize）在部分系统上会被静默忽略
                .resizable(true)
                .decorations(false)
                .center()
                // [20261016] `--hidden` 静默启动：主控壳勾「无人值守」时后台拉起本模块，
                // 不弹窗口（对齐 ToDesk：被控常驻后台，统一入口是主控壳界面/托盘）
                .visible(!silent_launch)
                .additional_browser_args(&browser_args())
                .build()?;

            // [20260920 auto-tuck] 注册主窗口 HWND：远端输入恢复时 input::auto_tuck
            // 用它精准收起主窗口（子窗口标题同为 "InDesk"，不能按标题找）。
            if let Ok(h) = win.hwnd() {
                crate::input::set_main_hwnd(h.0 as isize);
            }

            // 主窗口关闭（点右上角×）：不退出，缩到托盘；只有托盘右键"退出"才真正结束进程
            let handle = app.handle().clone();
            win.on_window_event(move |event| match event {
                tauri::WindowEvent::CloseRequested { api, .. } => {
                    api.prevent_close();
                    if let Some(w) = handle.get_webview_window("main") {
                        let _ = w.hide();
                    }
                    // 子窗口（远程控制窗）跟着收起
                    for (label, w) in handle.webview_windows() {
                        if label != "main" {
                            let _ = w.hide();
                        }
                    }
                }
                tauri::WindowEvent::Destroyed => {
                    let _ = handle.emit(
                        "response_closeWindowed",
                        serde_json::json!({
                            "requestId": "",
                            "data": { "windowId": 1 },
                            "code": 0
                        }),
                    );
                }
                _ => {}
            });

            // 托盘：左键单击显示主界面，右键菜单（打开主界面 / 退出）。
            // [20261017] 静默启动（主控壳拉起）时不创建 —— 否则托盘出现
            // 「新版壳 + 旧被控」两个图标；被控统一从主控壳界面/托盘进出。
            if !silent_launch {
            use tauri::{
                menu::{Menu, MenuItem},
                tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent},
            };
            let open_item =
                MenuItem::with_id(app, "tray_open", "打开主界面", true, None::<&str>)?;
            let quit_item = MenuItem::with_id(app, "tray_quit", "退出", true, None::<&str>)?;
            let tray_menu = Menu::with_items(app, &[&open_item, &quit_item])?;
            let tray = TrayIconBuilder::new()
                .icon(app.default_window_icon().unwrap().clone())
                .tooltip("InDesk")
                .menu(&tray_menu)
                .show_menu_on_left_click(false)
                .on_menu_event(|app, event| match event.id.as_ref() {
                    "tray_open" => {
                        if let Some(w) = app.get_webview_window("main") {
                            let _ = w.show();
                            let _ = w.unminimize();
                            let _ = w.set_focus();
                        }
                    }
                    "tray_quit" => {
                        // 用户显式退出：先停掉静默取屏，再关窗口结束进程
                        crate::capture::stop();
                        for (_, w) in app.webview_windows() {
                            let _ = w.close();
                        }
                        app.exit(0);
                    }
                    _ => {}
                })
                .on_tray_icon_event(|tray, event| {
                    if let TrayIconEvent::Click {
                        button: MouseButton::Left,
                        button_state: MouseButtonState::Up,
                        ..
                    } = event
                    {
                        let app = tray.app_handle();
                        if let Some(w) = app.get_webview_window("main") {
                            let _ = w.show();
                            let _ = w.unminimize();
                            let _ = w.set_focus();
                        }
                    }
                })
                .build(app)?;
            let _ = tray.set_visible(true);
            }

            Ok(())
        })
        .run(tauri::generate_context!())
        .expect("InDesk 启动失败");
}
