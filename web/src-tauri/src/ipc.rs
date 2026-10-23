//! IPC 通道分发：把原 Electron 主进程 `ipcMain.on / ipcMain.handle` 的全部通道，
//! 收敛到一个 `ipc_dispatch` 命令。前端只需把 `ipcRenderer.send/invoke` 换成
//! `invoke('ipc_dispatch', {channel, payload})`，业务代码零改动。

use std::path::PathBuf;

use serde_json::{json, Value};
use tauri::{AppHandle, Emitter, Manager, WebviewUrl, WebviewWindow, WebviewWindowBuilder};

use crate::input;

/// 窗口 id 与 Tauri 窗口 label 的映射（主窗口沿用 "main"）。
pub fn label_of(window_id: i64) -> String {
    if window_id == 1 {
        "main".to_string()
    } else {
        format!("win-{window_id}")
    }
}

fn win_of(app: &AppHandle, window_id: i64) -> Option<WebviewWindow> {
    app.get_webview_window(&label_of(window_id))
}

/// 向前端推送事件（等价 Electron 的 `win.webContents.send`）。
fn reply(app: &AppHandle, window_id: i64, channel: &str, payload: Value) {
    // [20261015 混合壳] core 模式（无窗口）下事件走 ws 广播给 Electron 壳；
    // 非 core 模式是 no-op，Tauri 事件照走原路。
    crate::core_bridge::broadcast(channel, &payload);
    match win_of(app, window_id) {
        Some(win) => {
            let _ = win.emit(channel, payload);
        }
        None => {
            let _ = app.emit(channel, payload);
        }
    }
}

fn ok(request_id: &str, data: Value) -> Value {
    json!({ "requestId": request_id, "data": data, "code": 0 })
}

fn fail(request_id: &str, data: Value, msg: String) -> Value {
    json!({ "requestId": request_id, "data": data, "code": 1, "msg": msg })
}

fn num(v: &Value, key: &str) -> Option<i64> {
    v.get(key).and_then(|x| x.as_i64())
}

fn fnum(v: &Value, key: &str) -> Option<f64> {
    v.get(key)
        .and_then(|x| x.as_f64().or_else(|| x.as_i64().map(|i| i as f64)))
}

/// 用户数据目录下的热更目录（前端资源可覆盖升级）。
fn web_update_dir(app: &AppHandle) -> PathBuf {
    app.path()
        .app_local_data_dir()
        .unwrap_or_else(|_| PathBuf::from("."))
        .join("web")
}

/// 被控真实身份文件（uuid + password）：放 %APPDATA%\InDesk\ ——
/// 每机独立、普通权限一定可写；壳读它显示设备代码/临时密码。
fn host_identity_path() -> PathBuf {
    let base = std::env::var("APPDATA")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("."));
    base.join("InDesk").join("host_identity.json")
}

/// 壳自更新工作目录（暂存新 exe、替换助手、备份、日志都在这里）。
/// 放在用户数据目录而不是安装目录：安装目录可能只读，而且这里不参与版本回滚判定。
fn shell_update_dir(app: &AppHandle) -> PathBuf {
    app.path()
        .app_local_data_dir()
        .unwrap_or_else(|_| PathBuf::from("."))
        .join("shell")
}

pub async fn dispatch(app: &AppHandle, channel: &str, payload: Value) -> Result<Value, String> {
    let request_id = payload
        .get("requestId")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let data = payload.get("data").cloned().unwrap_or_else(|| json!({}));
    let wid = num(&data, "windowId").unwrap_or(1);

    // 排查开关：设了 INDESK_IPC_LOG=1 才会写日志（含通道名与截断后的参数）
    ipc_log(&format!(
        "dispatch channel={channel} windowId={wid} data={}",
        truncate(&data.to_string(), 400)
    ));

    match channel {
        // ------------------------------------------------ 基础信息
        "getPlatform" => Ok(ok(
            &request_id,
            json!({ "platform": crate::platform_name() }),
        )),

        // [20260914r] 顶栏/设置页"当前版本"取真实**壳版本**。
        // 起因：界面一直显示 v1.3.1 —— 因为 appStore.version 来自前端构建期常量
        // APP_BUILD_INFO.pkgVersion（= web/package.json 的 version），
        // 而那份 package.json 自 1.3.1 起就没跟着壳一起升，于是永远滞后。
        // 这里直接回报编译期壳版本（env!("CARGO_PKG_VERSION") = Cargo.toml 的 version），
        // 前端拿到后覆盖显示 —— 从此不会再和安装包版本对不上。
        "getShellVersion" => Ok(ok(
            &request_id,
            json!({ "version": env!("CARGO_PKG_VERSION") }),
        )),

        // [20261016 统一壳] 被控身份落盘：被控前端 initDeskUser 拿到
        // deskUserUuid/password 后调用，写到 exe 同目录 host_identity.json。
        // 主控壳读它来显示「本机设备代码/临时密码」—— 主控壳自己的
        // controller_identity 是 /desk_user/create 的另一条记录，
        // 拿它当被控代码显示，对方连那个 code 永远无人应答（连接不上的根因）。
        "writeHostIdentity" => {
            let uuid = data.get("uuid").and_then(|v| v.as_str()).unwrap_or("");
            let password = data.get("password").and_then(|v| v.as_str()).unwrap_or("");
            if uuid.is_empty() {
                return Ok(fail(&request_id, json!({}), "uuid 为空".into()));
            }
            // [20261016] 存 %APPDATA%\InDesk\（每机独立且一定可写）。
            // 旧版写 exe 同目录：Program Files 下普通权限写不进，且安装包会
            // 把开发机的文件带出去 → 两台机器显示同一个设备代码。
            let path = host_identity_path();
            if let Some(dir) = path.parent() {
                let _ = std::fs::create_dir_all(dir);
            }
            let body = json!({ "uuid": uuid, "password": password });
            match std::fs::write(&path, body.to_string()) {
                Ok(_) => Ok(ok(&request_id, json!({ "path": path.to_string_lossy() }))),
                Err(e) => Ok(fail(&request_id, json!({}), format!("写入失败: {e}"))),
            }
        }

        // [20261016] 被控启动 login(本地缓存密码) 失败时，读壳改过的新密码重试，
        // 避免密码一改设备代码就变。
        "readHostIdentity" => {
            let path = host_identity_path();
            match std::fs::read_to_string(&path) {
                Ok(s) => match serde_json::from_str::<serde_json::Value>(&s) {
                    Ok(v) => Ok(ok(&request_id, v)),
                    Err(e) => Ok(fail(&request_id, json!({}), format!("解析失败: {e}"))),
                },
                Err(_) => Ok(ok(&request_id, serde_json::Value::Null)),
            }
        }

        "shellOpenExternal" => {
            let url = data.get("url").and_then(|v| v.as_str()).unwrap_or("");
            if url.is_empty() {
                return Ok(fail(&request_id, json!({}), "url 为空".into()));
            }
            match open_external(url) {
                Ok(_) => Ok(ok(&request_id, json!({}))),
                Err(e) => Ok(fail(&request_id, json!({}), e)),
            }
        }

        "powerSaveBlockerStart" => {
            input::prevent_display_sleep();
            reply(
                app,
                wid,
                "response_powerSaveBlockerStart",
                ok(&request_id, json!({})),
            );
            Ok(ok(&request_id, json!({})))
        }

        // 无人值守开关：壳据此决定「点关闭」时是彻底隐藏还是保持渲染（保画面不卡）
        "setUnattended" => {
            let enabled = data.get("enabled").and_then(|v| v.as_bool()).unwrap_or(false);
            crate::set_unattended(enabled);
            Ok(ok(
                &request_id,
                json!({ "enabled": enabled, "silentCapture": cfg!(windows) }),
            ))
        }

        // ------------------------------------------------ 静默取屏（无人值守核心）
        // 壳侧抓屏 → 本地只读取帧服务；前端 fetch 该流后画到 canvas，
        // 再由 canvas.captureStream() 生成视频轨交给 WebRTC。
        // 全程不调用 getDisplayMedia，因此不会有任何系统共享选择窗口。
        //
        // mode（1.4.0 换引擎）：
        //   "jpeg"（默认）→ 壳侧 JPEG 编码，前端 createImageBitmap 解码（老前端行为）
        //   "bgra"/"nv12" → 壳侧**不编码**直送像素，前端 new VideoFrame 零解码上屏
        // 实际生效的格式以抓屏线程的判定为准（GPU 不可用会退回 jpeg），
        // 前端用 /__capture/frame 返回的 X-Indesk-Fmt 或 meta 查询确认。
        "captureStart" => {
            let opt = crate::capture::StartOptions {
                max_width: num(&data, "maxWidth").unwrap_or(1920).max(0) as u32,
                quality: num(&data, "quality").unwrap_or(70).clamp(20, 95) as u8,
                fps: num(&data, "fps").unwrap_or(15).clamp(1, 60) as u32,
                mode: data
                    .get("mode")
                    .and_then(|v| v.as_str())
                    .unwrap_or("jpeg")
                    .to_string(),
                // [1.4.5 阶段①] H.264 目标码率（kbps）。0/缺省 = 壳侧默认 4000。
                // 前端应把用户选的档位传进来，否则壳侧编码码率与 WebRTC 上限不一致。
                bitrate_kbps: num(&data, "bitrate").unwrap_or(0).max(0) as u32,
                // [20261021 七轮] 编码后端：默认 CPU 软编；传 "nvenc" 走硬编（A/B 对照）。
                enc: data
                    .get("enc")
                    .and_then(|v| v.as_str())
                    .unwrap_or("nvenc")
                    .to_string(),
            };
            // [20260919 h264probe] 取证：每次会话重建都落盘（谁、什么格式、多大）。
            // 排查「h264 会话被谁打回 jpegn」用 —— 排查完可撤。
            crate::capture::capture_diag_pub(&format!(
                "captureStart mode={} fps={} w={} from={}",
                opt.mode,
                opt.fps,
                opt.max_width,
                data.get("from").and_then(|v| v.as_str()).unwrap_or("?")
            ));
            match crate::capture::start(opt) {
                Ok((port, width, height)) => {
                    // 1.4.1：应答直接带上真实发布格式与分条布局。
                    // JPEG 家族的判断（单张 / 分条）只依赖输出高度与核数，`start()`
                    // 里已经定下来，所以这里拿到的就是最终格式 —— 前端不必再去
                    // 轮询 meta 等最多 2 秒才有画面。
                    // 只有直送格式（bgra/nv12）取决于 GPU 缩放链能否建成，那时仍是
                    // "starting"，前端会退回轮询兜底。
                    let (strips, rows_per) = crate::capture::strip_layout();
                    Ok(ok(
                        &request_id,
                        json!({
                            "port": port,
                            "width": width,
                            "height": height,
                            "mode": crate::capture::mode(),
                            "strips": strips,
                            "rowsPer": rows_per,
                        }),
                    ))
                }
                Err(e) => Ok(fail(&request_id, json!({}), e)),
            }
        }

        // [20260918 capfix] 同步停：`captureStop` 必须等抓屏线程真正退出
        // （其 DXGI 复制对象已释放）。复制对象是**独占**资源，异步停会让紧接着的
        // `captureStart` 建链失败（0x80070057）→ OUT_MODE 卡在 starting → 前端零帧。
        "captureStop" => {
            crate::capture::stop_and_join();
            Ok(ok(&request_id, json!({})))
        }

        // [20260918c idlekill] 前端把「当前主控数」同步到壳侧。
        //
        // 需要它的原因见 `capture::set_viewer_count` 的说明：前端 `remoteDesk`
        // Map 的写入点在 `acceptIncoming()`，**主控正在连的过程中它还是 0**，
        // 看门狗据此判"无人值守空闲"就会把刚拉起的取屏掐死（用户侧="连上就死"）。
        // 由前端"一变就报"、看门狗"回头查壳"，判据就不再受前端时序影响。
        //
        // 之所以让前端上报而不是壳自己数 RTC 对端：RTC 连接活在 WebView2 里，
        // 壳拿不到它的生命周期；前端至少知道"我建了几个 PeerConnection"。
        // 壳只是把这个数**存下来**，供看门狗以"壳侧真话"的形式读回。
        "setRemoteViewerCount" => {
            let n = num(&data, "count").unwrap_or(0).max(0) as u32;
            crate::capture::set_viewer_count(n);
            Ok(ok(&request_id, json!({ "count": n })))
        }

        // [20260918c idlekill] 读回上一条写入的主控数。
        // 看门狗用它替代 `appStore.remoteDesk.size` 做空闲判定。
        "remoteViewerCount" => Ok(ok(
            &request_id,
            json!({ "count": crate::capture::viewer_count() }),
        )),

        // [20260918e idleslow] 空闲降速开关：没人连 90s 后前端把它置 true，
        // 抓屏/编码降到 2fps/q50（管线不停，恢复零延迟 —— 与 v52 的"停管线"
        // 有本质区别，那个有"连上就死"的误杀窗口，这个没有）。
        "setCaptureIdle" => {
            let on = data.get("on").and_then(|v| v.as_bool()).unwrap_or(false);
            crate::capture::set_idle(on);
            Ok(ok(&request_id, json!({ "idle": on })))
        }

        "captureStatus" => {
            let (w, h) = crate::capture::out_size();
            Ok(ok(
                &request_id,
                json!({
                    "running": crate::capture::is_running(),
                    "port": crate::capture::port(),
                    "mode": crate::capture::mode(),
                    "width": w,
                    "height": h,
                    // [20260918 capfix] 抓屏失败原因（空 = 正常）。
                    // 壳由 explorer 拉起时 stderr 无处可去，"连上就死画面"曾因此
                    // 完全没有线索；现在前端/取证脚本一句话就能读到根因。
                    "error": crate::capture::last_error(),
                }),
            ))
        }

        // [1.4.5 阶段①] 主控端请求一个 IDR。
        //
        // 为什么必须有这条通道：壳侧 NVENC 走的是**无限 GOP**（`idr_period = fps*2`，
        // 刻意不靠 P 帧数量触发 IDR，让码率全部花在画质上）。代价是**中途接入/丢帧后
        // 无法自动恢复** —— 主控端解码器一旦参考链断裂（缺一帧、或解码器重建），
        // 后续 P 帧全部解不出，画面会一直花到下一个定时 IDR（最坏 2 秒）。
        // 有了这条通道，主控端发现断帧可以**立刻**要一个 IDR，恢复时间从"最坏 2s"
        // 压到"一个 RTT"。`capture::request_keyframe()` 只是置一个 AtomicBool，
        // 抓屏线程下一帧消费，本身无锁无阻塞，可以在热路径上随便调。
        "captureKeyframe" => {
            crate::capture::request_keyframe();
            Ok(ok(&request_id, json!({})))
        }

        // 无人值守兜底：浏览器规范要求 getDisplayMedia 必须有"真实用户手势"，
        // 无人机器前没人操作，由壳在本程序窗口标题栏注入一次真实点击把手势补上
        "nudgeUserGesture" => match crate::gesture::nudge(app) {
            Ok(v) => Ok(ok(&request_id, v)),
            Err(e) => Ok(fail(&request_id, json!({}), e)),
        },

        "commonTest" => {
            if let (Some(_x), Some(_y)) = (fnum(&data, "x"), fnum(&data, "y")) {
                input::dispatch_input(move || input::mouse_move(_x as i32, _y as i32));
            }
            reply(app, wid, "commonTest", ok(&request_id, json!({})));
            Ok(ok(&request_id, json!({})))
        }

        // ------------------------------------------------ 窗口管理
        // 主界面右上角 ×：主窗缩到托盘（不退出），子窗口一并隐藏
        "closeAllWindow" => {
            for (_, w) in app.webview_windows() {
                let _ = w.hide();
            }
            Ok(ok(&request_id, json!({})))
        }

        // [1.4.26] 壳自更新收尾：替换助手已在后台等这个进程退出。
        // 这里**必须无条件退出进程**，不能走"缩到托盘"那套 —— 被控端可能正开着
        // 主界面/远控窗，只要 exe 还被占用，助手就会在 30 秒后超时失败。
        // 停取屏 → 让响应先回到前端（延迟 300ms）→ exit(0)。
        "appQuit" => {
            crate::capture::stop();
            let handle = app.clone();
            std::thread::spawn(move || {
                std::thread::sleep(std::time::Duration::from_millis(300));
                handle.exit(0);
                // 兜底硬退：`exit()` 是把退出请求投给事件循环，万一某个窗口把消息循环
                // 卡住（历史上出现过"页面冻结"），请求就落不了地 —— 而替换助手正在等
                // 这个进程释放 exe，不退就等于更新失败。3 秒后还在跑就无条件退出。
                std::thread::sleep(std::time::Duration::from_secs(3));
                std::process::exit(0);
            });
            Ok(ok(&request_id, json!({})))
        }

        "closeWindow" => {
            if let Some(w) = win_of(app, wid) {
                let _ = w.close();
            }
            Ok(ok(&request_id, json!({})))
        }

        "windowMinimize" => {
            if let Some(w) = win_of(app, wid) {
                let _ = w.minimize();
                // [20260920] frameless 窗在部分机器上 w.minimize() 静默无效（用户实测
                // "点了最小化不管用"）：250ms 后仍未最小化 → Win32 ShowWindowAsync 兜底。
                #[cfg(windows)]
                if let Ok(h) = w.hwnd() {
                    let hwnd = h.0 as isize;
                    std::thread::spawn(move || {
                        std::thread::sleep(std::time::Duration::from_millis(250));
                        use windows_sys::Win32::UI::WindowsAndMessaging::{
                            IsIconic, ShowWindowAsync, SW_MINIMIZE,
                        };
                        if unsafe { IsIconic(hwnd as *mut core::ffi::c_void) } == 0 {
                            unsafe { ShowWindowAsync(hwnd as *mut core::ffi::c_void, SW_MINIMIZE) };
                        }
                    });
                }
            }
            Ok(ok(&request_id, json!({})))
        }

        "windowMaximize" => {
            if let Some(w) = win_of(app, wid) {
                let _ = w.maximize();
            }
            Ok(ok(&request_id, json!({})))
        }

        "windowToggleMaximize" => {
            if let Some(w) = win_of(app, wid) {
                if w.is_maximized().unwrap_or(false) {
                    let _ = w.unmaximize();
                } else {
                    let _ = w.maximize();
                }
            }
            Ok(ok(&request_id, json!({})))
        }

        "handleOpenDevTools" => {
            if let Some(w) = win_of(app, wid) {
                w.open_devtools();
            }
            reply(
                app,
                wid,
                "response_handleOpenDevTools",
                ok(&request_id, json!({})),
            );
            Ok(ok(&request_id, json!({})))
        }

        "handleMoveScreenRightBottom" => {
            if let Some(w) = win_of(app, wid) {
                if let Ok(Some(monitor)) = w.primary_monitor() {
                    let ms = monitor.size();
                    let scale = monitor.scale_factor();
                    let mp = monitor.position();
                    if let Ok(size) = w.outer_size() {
                        // 任务栏高度按 48 物理像素估算，避免压住任务栏
                        let new_x = mp.x + ms.width as i32 - size.width as i32;
                        let new_y =
                            mp.y + ms.height as i32 - size.height as i32 - (48.0 * scale) as i32;
                        let _ = w.set_position(tauri::PhysicalPosition::new(new_x, new_y));
                    }
                }
            }
            reply(
                app,
                wid,
                "response_handleMoveScreenRightBottom",
                ok(&request_id, json!({})),
            );
            Ok(ok(&request_id, json!({})))
        }

        "setWindowBounds" => {
            let width = fnum(&data, "width").unwrap_or(0.0);
            let height = fnum(&data, "height").unwrap_or(0.0);
            if let Some(w) = win_of(app, wid) {
                if width > 0.0 && height > 0.0 {
                    let _ = w.set_size(tauri::LogicalSize::new(width, height));
                }
            }
            reply(
                app,
                wid,
                "response_setWindowBounds",
                ok(&request_id, json!({})),
            );
            Ok(ok(&request_id, json!({})))
        }

        "getWindowTitlebarHeight" => Ok(json!({
            "requestId": request_id,
            "data": { "height": 0 },
            "code": 0
        })),

        "setWindowPosition" => {
            let x = fnum(&data, "x").unwrap_or(0.0);
            let y = fnum(&data, "y").unwrap_or(0.0);
            if let Some(w) = win_of(app, wid) {
                let _ = w.set_position(tauri::LogicalPosition::new(x, y));
            }
            reply(
                app,
                wid,
                "response_setWindowPosition",
                ok(&request_id, json!({})),
            );
            Ok(ok(&request_id, json!({})))
        }

        "getWindowPosition" => {
            let pos = win_of(app, wid)
                .and_then(|w| w.outer_position().ok())
                .map(|p| json!({ "x": p.x, "y": p.y }))
                .unwrap_or_else(|| json!({ "x": 0, "y": 0 }));
            reply(app, wid, "response_getWindowPosition", ok(&request_id, json!({ "position": pos })));
            Ok(ok(&request_id, json!({})))
        }

        "setAlwaysOnTop" => {
            let flag = data
                .get("flag")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            if let Some(w) = win_of(app, wid) {
                let _ = w.set_always_on_top(flag);
            }
            reply(app, wid, "response_setAlwaysOnTop", ok(&request_id, json!({})));
            Ok(ok(&request_id, json!({})))
        }

        "createWindow" => {
            match create_window(app, &data).await {
                Ok(_) => Ok(ok(&request_id, json!({}))),
                Err(e) => Ok(fail(&request_id, json!({}), e)),
            }
        }

        // ------------------------------------------------ 屏幕 / 显示器
        "scaleFactor" => {
            let (scale, platform) = win_of(app, wid)
                .and_then(|w| w.scale_factor().ok())
                .map(|s| (s, crate::platform_name()))
                .unwrap_or((1.0, crate::platform_name()));
            Ok(json!({
                "requestId": request_id,
                "data": { "scaleFactor": scale, "platform": platform },
                "code": 0
            }))
        }

        "workAreaSize" => {
            let (w_, h_) = work_area_size(app, wid);
            reply(
                app,
                wid,
                "response_workAreaSize",
                ok(&request_id, json!({ "width": w_, "height": h_ })),
            );
            Ok(ok(&request_id, json!({})))
        }

        "getPrimaryDisplaySize" => {
            let (w_, h_) = monitor_size(app, wid);
            Ok(json!({
                "requestId": request_id,
                "data": { "width": w_, "height": h_ },
                "code": 0
            }))
        }

        // Tauri/WebView2 没有 desktopCapturer：前端改为 getDisplayMedia，
        // 这里返回一个占位 source，让前端走到统一的采集分支。
        "getScreenStream" => {
            reply(
                app,
                wid,
                "response_getScreenStream",
                ok(
                    &request_id,
                    json!({ "stream": { "id": "primary-screen", "name": "Entire screen" } }),
                ),
            );
            Ok(ok(&request_id, json!({})))
        }

        // ------------------------------------------------ 鼠标
        "mouseMove" => {
            if let (Some(x), Some(y)) = (fnum(&data, "x"), fnum(&data, "y")) {
                input::dispatch_input(move || input::mouse_move(x as i32, y as i32));
            }
            reply(app, wid, "response_mouseMove", ok(&request_id, json!({})));
            Ok(ok(&request_id, json!({})))
        }
        "mouseSetPosition" => {
            if let (Some(x), Some(y)) = (fnum(&data, "x"), fnum(&data, "y")) {
                input::dispatch_input(move || input::mouse_set_position(x as i32, y as i32));
            }
            reply(
                app,
                wid,
                "response_mouseSetPosition",
                ok(&request_id, json!({})),
            );
            Ok(ok(&request_id, json!({})))
        }
        "mouseDrag" => {
            if let (Some(x), Some(y)) = (fnum(&data, "x"), fnum(&data, "y")) {
                input::dispatch_input(move || input::mouse_drag(x as i32, y as i32));
            }
            reply(app, wid, "response_mouseDrag", ok(&request_id, json!({})));
            Ok(ok(&request_id, json!({})))
        }
        "mousePressButtonLeft" => {
            input::dispatch_input(input::mouse_press_left);
            reply(
                app,
                wid,
                "response_mousePressButtonLeft",
                ok(&request_id, json!({})),
            );
            Ok(ok(&request_id, json!({})))
        }
        "mouseReleaseButtonLeft" => {
            input::dispatch_input(input::mouse_release_left);
            reply(
                app,
                wid,
                "response_mouseReleaseButtonLeft",
                ok(&request_id, json!({})),
            );
            Ok(ok(&request_id, json!({})))
        }
        "mouseLeftClick" => {
            input::dispatch_input(input::mouse_click_left);
            reply(
                app,
                wid,
                "response_mouseLeftClick",
                ok(&request_id, json!({})),
            );
            Ok(ok(&request_id, json!({})))
        }
        "mouseRightClick" => {
            input::dispatch_input(input::mouse_click_right);
            reply(
                app,
                wid,
                "response_mouseRightClick",
                ok(&request_id, json!({})),
            );
            Ok(ok(&request_id, json!({})))
        }
        "mouseDoubleClick" => {
            input::dispatch_input(input::mouse_double_click);
            reply(
                app,
                wid,
                "response_mouseDoubleClick",
                ok(&request_id, json!({})),
            );
            Ok(ok(&request_id, json!({})))
        }
        "mouseScrollUp" => {
            let amt = num(&data, "amount").unwrap_or(1) as i32;
            input::dispatch_input(move || input::mouse_scroll_up(amt));
            reply(
                app,
                wid,
                "response_mouseScrollUp",
                ok(&request_id, json!({})),
            );
            Ok(ok(&request_id, json!({})))
        }
        "mouseScrollDown" => {
            let amt = num(&data, "amount").unwrap_or(1) as i32;
            input::dispatch_input(move || input::mouse_scroll_down(amt));
            reply(
                app,
                wid,
                "response_mouseScrollDown",
                ok(&request_id, json!({})),
            );
            Ok(ok(&request_id, json!({})))
        }
        "mouseScrollLeft" => {
            let amt = num(&data, "amount").unwrap_or(1) as i32;
            input::dispatch_input(move || input::mouse_scroll_left(amt));
            reply(
                app,
                wid,
                "response_mouseScrollLeft",
                ok(&request_id, json!({})),
            );
            Ok(ok(&request_id, json!({})))
        }
        "mouseScrollRight" => {
            let amt = num(&data, "amount").unwrap_or(1) as i32;
            input::dispatch_input(move || input::mouse_scroll_right(amt));
            reply(
                app,
                wid,
                "response_mouseScrollRight",
                ok(&request_id, json!({})),
            );
            Ok(ok(&request_id, json!({})))
        }
        "getMousePosition" => {
            let (x, y) = input::mouse_position();
            let payload = json!({ "point": { "x": x, "y": y } });
            reply(
                app,
                wid,
                "response_getMousePosition",
                ok(&request_id, payload.clone()),
            );
            // send 走事件、invoke 走返回值，两种调用方式都给到完整数据
            Ok(ok(&request_id, payload))
        }

        // ------------------------------------------------ 键盘
        "keyboardType" => {
            if let Some(k) = data.get("key").and_then(|v| v.as_str()) {
                let k = k.to_string();
                input::dispatch_input(move || input::keyboard_text(&k));
            }
            reply(
                app,
                wid,
                "response_keyboardType",
                ok(&request_id, json!({})),
            );
            Ok(ok(&request_id, json!({})))
        }
        "keyboardPressKey" => {
            {
                let keys = key_list(&data);
                input::dispatch_input(move || input::keyboard_key(&keys, true));
            }
            reply(
                app,
                wid,
                "response_keyboardPressKey",
                ok(&request_id, json!({})),
            );
            Ok(ok(&request_id, json!({})))
        }
        "keyboardReleaseKey" => {
            {
                let keys = key_list(&data);
                input::dispatch_input(move || input::keyboard_key(&keys, false));
            }
            reply(
                app,
                wid,
                "response_keyboardReleaseKey",
                ok(&request_id, json!({})),
            );
            Ok(ok(&request_id, json!({})))
        }
        // [20260919 ime] 输入法切换代按（method：0=Win+空格 1=Ctrl+空格 2=Shift 3=Alt+Shift）。
        // 为什么走独立通道而不是 keyboardPressKey/ReleaseKey 拆开发：组合键的
        // 按下/抬起之间需要几十毫秒的间隔（系统热键识别靠时序），拆成两条
        // IPC 消息的间隔不可控。见 input::ime_switch 的说明。
        "switchIme" => {
            let method = num(&data, "method").unwrap_or(0) as u32;
            input::dispatch_input(move || input::ime_switch(method));
            reply(app, wid, "response_switchIme", ok(&request_id, json!({})));
            Ok(ok(&request_id, json!({})))
        }

        // ------------------------------- [20260922 网页版工具栏] 三项新能力
        //
        // 复用同一套「被控端代执行 + reply」模式。三条都**只在被控端本机生效**，
        // 主控端拿到的只是回执；其中 Ctrl+Alt+Del 的回执**如实反映成败**
        // （普通用户态极可能失败），绝不上报假成功。

        // 松开系统键：释放被控端上所有按下的修饰键。
        // 解决「主控端在浏览器里按住 Ctrl 后切走焦点 → Ctrl 的 keyup 送不到 →
        // 被控端 Ctrl 卡住」的高频问题。只发 keyup，重复调用无害。
        "releaseAllModifiers" => {
            input::dispatch_input(input::release_all_modifiers);
            reply(
                app,
                wid,
                "response_releaseAllModifiers",
                ok(&request_id, json!({ "ok": true })),
            );
            Ok(ok(&request_id, json!({ "ok": true })))
        }

        // 隐私屏开/关：只改被控端 capture 层的发布内容（黑化），
        // 不停流、不改尺寸 —— 因此不会触发主控端的"零帧/冻结"自愈判据。
        "privacyScreen" => {
            let on = data
                .get("on")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            let state = crate::capture::set_privacy_screen(on);
            reply(
                app,
                wid,
                "response_privacyScreen",
                ok(&request_id, json!({ "privacy": state })),
            );
            Ok(ok(&request_id, json!({ "privacy": state })))
        }

        // Ctrl+Alt+Del：触发系统级 SAS。
        // ⚠️ 回执如实：壳以当前用户权限运行时 SendSAS 通常被系统忽略，
        // 这里把结果（成功/失败原因）原样回给主控端，由 UI 决定如何提示。
        "sendCtrlAltDel" => {
            let r = input::send_ctrl_alt_del();
            let (sent, err) = match &r {
                Ok(()) => (true, String::new()),
                Err(e) => (false, e.clone()),
            };
            crate::capture::capture_diag_pub(&format!(
                "[cad] SendSAS 调用{}",
                if sent { "完成" } else { "失败" }
            ));
            reply(
                app,
                wid,
                "response_sendCtrlAltDel",
                ok(&request_id, json!({ "sent": sent, "error": err })),
            );
            Ok(ok(&request_id, json!({ "sent": sent, "error": err })))
        }

        // [20261010 批次014 S1'] 会话结束锁屏：调 user32 LockWorkStation，与 Win+L
        // 完全等价 —— 动鼠标出系统登录界面，输密码回来，本机一切照常。
        // ⚠️ 绝不是"黑屏+锁物理键鼠"（那会把用户锁在自己电脑外面，
        // 20261010.1 已翻车回滚，教训见 11_优化记录 批次 013 复盘）。
        "lock_workstation" => lock_workstation_impl(&request_id),

        // [20261010 批次015 工具栏] 打开任务管理器（独立 GUI 进程）。
        "openTaskManager" => open_task_manager_impl(&request_id),

        // [20260922 文件传输] 把组装好的文件落盘到 Downloads\InDesk收件\。
        // 前端走 ipcRendererInvoke → 本 channel，返回真实落盘路径。
        "save_received_file" => {
            let file_name = data
                .get("fileName")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let data_b64 = data
                .get("dataB64")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            match crate::commands::save_received_file(&file_name, &data_b64) {
                Ok(v) => Ok(ok(&request_id, v)),
                Err(e) => Ok(json!({
                    "requestId": request_id,
                    "code": 1,
                    "msg": e,
                    "data": {},
                })),
            }
        }

        // ------------------------------------------------ [1.4.5] 剪贴板同步（纯文本）
        // 实现与四个 Win32 陷阱见 clipboard.rs 顶部长注释。三条通道分工是刻意的：
        //   clipboardSeq —— 廉价轮询：GetClipboardSequenceNumber **不打开剪贴板**，
        //                   前端 700ms 打一次，绝大多数轮次到这里就返回了；
        //   clipboardGet —— 只有序列号变了才调，真去读内容；
        //   clipboardSet —— 把对端内容写进本机剪贴板。
        // 三条都同时"返回 + reply"（与 getMousePosition 同款）：invoke 拿返回值、
        // send 拿事件，两种调用方式都不缺数据。
        "clipboardSeq" => {
            let seq = crate::clipboard::sequence();
            let payload = json!({ "seq": seq });
            reply(app, wid, "response_clipboardSeq", ok(&request_id, payload.clone()));
            Ok(ok(&request_id, payload))
        }
        "clipboardGet" => {
            // 显式标类型而不是 `let snap = ...`：一是让读的人一眼看到返回的是
            // clipboard.rs 里那个结构，二是避免该类型因为"只被函数返回值间接引用"
            // 而被 dead_code 分析报成未使用（那种警告会淹没真正有用的警告）。
            let snap: crate::clipboard::ClipSnapshot = crate::clipboard::read_snapshot();
            let len = snap.text.as_ref().map(|s| s.chars().count()).unwrap_or(0);
            // ⚠️ 顺序要紧：`snap.text` / `snap.error` 在这里被移进 json!，
            // 之后再碰 `snap.text` 就是编译错误（这是好事，不是坏事）。
            let payload = json!({
                "text": snap.text,
                "hasText": len > 0,
                "seq": snap.seq,
                "len": len,
                // rawLen：截断前的原始码元数。前端据此能把"内容太长"报成
                // **具体多长**（"原始 130000 字符 > 上限 20000"），而不是一句
                // 无从下手的"太长了"。shellLimit 是壳侧自己的读上限，
                // 与前端单条同步上限（2 万字符）不是一个数，别混用。
                "rawLen": snap.raw_len,
                "shellLimit": crate::clipboard::READ_MAX_CHARS,
                "truncated": snap.truncated,
                "error": snap.error,
            });
            reply(app, wid, "response_clipboardGet", ok(&request_id, payload.clone()));
            Ok(ok(&request_id, payload))
        }
        "clipboardSet" => {
            let text = data.get("text").and_then(|v| v.as_str()).unwrap_or("");
            match crate::clipboard::write_text(text) {
                Ok(seq) => {
                    let payload =
                        json!({ "ok": true, "seq": seq, "len": text.chars().count() });
                    reply(
                        app,
                        wid,
                        "response_clipboardSet",
                        ok(&request_id, payload.clone()),
                    );
                    Ok(ok(&request_id, payload))
                }
                // 写失败（多为剪贴板被别的进程占住）：**不能静默** ——
                // 前端要据此把它记成一次 write-fail 并显示出来，
                // 否则用户会看到"复制了但对面粘不出来"而没有任何线索。
                Err(e) => {
                    reply(
                        app,
                        wid,
                        "response_clipboardSet",
                        fail(&request_id, json!({}), e.clone()),
                    );
                    Ok(fail(&request_id, json!({}), e))
                }
            }
        }

        // [20261022 剪贴板文件同步] CF_HDROP 读写。为什么不放在 Dart 里做：
        // Flutter 的 Clipboard API 只有纯文本，文件列表（CF_HDROP）必须由 core
        // 用 Win32 原语取/写，所以这两个命令是 Dart 侧唯一入口。
        // 与上面三条一样"同时返回 + reply"。
        "clipboardGetFiles" => {
            let snap: crate::clipboard::FileSnapshot = crate::clipboard::read_files();
            let count = snap.files.len();
            let payload = json!({
                "files": snap.files,
                "count": count,
                "seq": snap.seq,
                "limit": crate::clipboard::FILES_MAX,
                "truncated": snap.truncated,
                "error": snap.error,
            });
            reply(
                app,
                wid,
                "response_clipboardGetFiles",
                ok(&request_id, payload.clone()),
            );
            Ok(ok(&request_id, payload))
        }
        "clipboardSetFiles" => {
            let paths: Vec<String> = data
                .get("files")
                .and_then(|v| v.as_array())
                .map(|a| {
                    a.iter()
                        .filter_map(|x| x.as_str().map(|s| s.to_string()))
                        .collect()
                })
                .unwrap_or_default();
            match crate::clipboard::write_files(&paths) {
                Ok(seq) => {
                    let payload = json!({ "ok": true, "seq": seq, "count": paths.len() });
                    reply(
                        app,
                        wid,
                        "response_clipboardSetFiles",
                        ok(&request_id, payload.clone()),
                    );
                    Ok(ok(&request_id, payload))
                }
                // 写失败必须如实回传：用户"对面粘不出来"时，这是唯一线索。
                Err(e) => {
                    reply(
                        app,
                        wid,
                        "response_clipboardSetFiles",
                        fail(&request_id, json!({}), e.clone()),
                    );
                    Ok(fail(&request_id, json!({}), e))
                }
            }
        }

        // ------------------------------------------------ 热更
        "applyWebUpdate" => {
            let zip_b64 = data
                .get("zipBase64")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let version = data
                .get("version")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            match apply_web_update(app, &zip_b64, &version) {
                Ok(dest) => Ok(json!({
                    "requestId": request_id,
                    "code": 0,
                    "data": { "ok": true, "dir": dest }
                })),
                Err(e) => Ok(json!({
                    "requestId": request_id,
                    "code": 1,
                    "data": {},
                    "msg": e
                })),
            }
        }

        // ------------------------------------------------ 壳自更新（换 exe）
        // 前端把下载好的新 `indesk-desktop.exe` 字节发过来，这里落盘 + 启动"替换助手"，
        // 之后由前端调用 app.exit 让旧进程退出（详见 shell_update.rs 顶部长注释）。
        // 注意：**不在这里退出进程** —— 前端还要把"正在更新"的界面画完。
        "applyShellUpdate" => {
            let exe_b64 = data
                .get("exeBase64")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let version = data
                .get("version")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let expected = num(&data, "size").unwrap_or(0).max(0) as usize;
            if exe_b64.is_empty() {
                return Ok(fail(&request_id, json!({}), "更新数据为空".into()));
            }
            let bytes = match base64_decode::decode(&exe_b64) {
                Ok(b) => b,
                Err(e) => {
                    return Ok(fail(&request_id, json!({}), format!("解码失败: {e}")))
                }
            };
            let current = std::env::current_exe().unwrap_or_default();
            match crate::shell_update::stage_and_spawn(
                &shell_update_dir(app),
                &current,
                &bytes,
                &version,
                expected,
            ) {
                Ok(staged) => Ok(ok(
                    &request_id,
                    json!({ "ok": true, "staged": staged.to_string_lossy() }),
                )),
                Err(e) => Ok(fail(&request_id, json!({}), e)),
            }
        }

        // ------------------------------------------------ 壳自更新（安装包模式，1.4.31）
        // 与上面的 applyShellUpdate（换 exe + 替换助手）并列：前端按 version.json 里
        // 有没有 `shellInstallerUrl` 决定走哪条。这里把安装包落盘并以 `/S /R` 静默启动，
        // 剩下的"结束旧进程 → 就地覆盖 → 重新拉起"由安装程序自己做（见 shell_update.rs）。
        // 同样**不在这里退出进程** —— 前端要把"正在更新"的界面画完再调 appQuit。
        "applyShellInstaller" => {
            let b64 = data
                .get("installerBase64")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let version = data
                .get("version")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let expected = num(&data, "size").unwrap_or(0).max(0) as usize;
            if b64.is_empty() {
                return Ok(fail(&request_id, json!({}), "更新数据为空".into()));
            }
            let bytes = match base64_decode::decode(&b64) {
                Ok(b) => b,
                Err(e) => {
                    return Ok(fail(&request_id, json!({}), format!("解码失败: {e}")))
                }
            };
            match crate::shell_update::stage_and_run_installer(
                &shell_update_dir(app),
                &bytes,
                &version,
                expected,
            ) {
                Ok(setup) => Ok(ok(
                    &request_id,
                    json!({ "ok": true, "staged": setup.to_string_lossy() }),
                )),
                Err(e) => Ok(fail(&request_id, json!({}), e)),
            }
        }

        other => Err(format!("未实现的 IPC 通道: {other}")),
    }
}

/// 解析前端传来的按键（可能是数字、数字数组或单值）。
fn key_list(data: &Value) -> Vec<u16> {
    match data.get("key") {
        Some(Value::Array(arr)) => arr
            .iter()
            .filter_map(|v| v.as_u64().map(|n| n as u16))
            .collect(),
        Some(v) => v.as_u64().map(|n| vec![n as u16]).unwrap_or_default(),
        None => Vec::new(),
    }
}

/// 主显示器"逻辑尺寸"（DIP）——与 Electron `display.size` 语义一致。
///
/// 注意：Tauri 的 `monitor.size()` 返回**物理像素**，必须除以缩放比再返回。
/// 前端换算公式是 `primaryDisplaySize.width * scaleFactor * (x / 1000)`，
/// 语义要求 primaryDisplaySize 是逻辑值；若直接给物理值，坐标会被整体放大
/// （125% 缩放的机器上鼠标就点不准了）。
fn monitor_size(app: &AppHandle, wid: i64) -> (i32, i32) {
    let win = win_of(app, wid);
    if let Some(w) = &win {
        if let Ok(Some(m)) = w.primary_monitor() {
            let s = m.size();
            let sc = scale_of(m.scale_factor(), w);
            return (
                (s.width as f64 / sc).round() as i32,
                (s.height as f64 / sc).round() as i32,
            );
        }
    }
    let sc = win
        .as_ref()
        .and_then(|w| w.scale_factor().ok())
        .filter(|s| *s > 0.0)
        .unwrap_or(1.0);
    let (pw, ph) = input::screen_size();
    ((pw as f64 / sc).round() as i32, (ph as f64 / sc).round() as i32)
}

fn scale_of(monitor_scale: f64, win: &WebviewWindow) -> f64 {
    if monitor_scale > 0.0 {
        monitor_scale
    } else {
        win.scale_factor()
            .ok()
            .filter(|s| *s > 0.0)
            .unwrap_or(1.0)
    }
}

/// 主显示器工作区（不含任务栏）的逻辑尺寸，语义同 Electron `display.workAreaSize`。
fn work_area_size(app: &AppHandle, wid: i64) -> (i32, i32) {
    let sc = win_of(app, wid)
        .and_then(|w| w.scale_factor().ok())
        .filter(|s| *s > 0.0)
        .unwrap_or(1.0);
    match input::primary_work_area() {
        Some((pw, ph)) => (
            (pw as f64 / sc).round() as i32,
            (ph as f64 / sc).round() as i32,
        ),
        // 取不到工作区就退化成整屏逻辑尺寸
        None => monitor_size(app, wid),
    }
}

fn open_external(url: &str) -> Result<(), String> {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        // CREATE_NO_WINDOW：本进程是 GUI 应用，不加这个 flag 会闪一个 cmd 黑框。
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        // 避免引入额外插件：直接交给系统 shell 处理
        std::process::Command::new("cmd")
            .args(["/C", "start", "", url])
            .creation_flags(CREATE_NO_WINDOW)
            .spawn()
            .map(|_| ())
            .map_err(|e| e.to_string())
    }
    #[cfg(not(windows))]
    {
        let _ = url;
        Err("当前平台未实现".into())
    }
}

/// 创建子窗口（对应原 `createWindow` IPC）。
async fn create_window(app: &AppHandle, data: &Value) -> Result<(), String> {
    let window_id = num(data, "windowId").unwrap_or(2);
    let label = label_of(window_id);
    if app.get_webview_window(&label).is_some() {
        return Ok(());
    }

    let width = fnum(data, "width").unwrap_or(800.0);
    let height = fnum(data, "height").unwrap_or(500.0);
    let min_w = fnum(data, "minWidth").unwrap_or(width);
    let min_h = fnum(data, "minHeight").unwrap_or(height);
    let frame = data.get("frame").and_then(|v| v.as_bool()).unwrap_or(true);
    let use_work_area = data
        .get("useWorkAreaSize")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);

    let (mut w, mut h) = (width, height);
    if use_work_area {
        let (mw, mh) = monitor_size(app, window_id);
        w = mw as f64;
        h = mh as f64;
    }

    let (mut x, mut y) = (fnum(data, "x"), fnum(data, "y"));
    if x.is_none() && y.is_none() {
        let (mw, mh) = monitor_size(app, window_id);
        x = Some(((mw as f64 - w) / 2.0).round());
        y = Some(((mh as f64 - h) / 2.0).round());
    }

    let route = data.get("route").and_then(|v| v.as_str()).unwrap_or("");
    let query = data.get("query").cloned().unwrap_or_else(|| json!({}));
    let mut params = format!("windowId={window_id}");
    if let Some(obj) = query.as_object() {
        for (k, v) in obj {
            let val = match v {
                Value::String(s) => s.clone(),
                other => other.to_string(),
            };
            params.push('&');
            params.push_str(k);
            params.push('=');
            params.push_str(&val);
        }
    }
    // 基址取"主窗口当前地址"（去掉 hash）：打包态是 http://tauri.localhost/index.html，
    // 主窗口做过界面热更后会变成 indesk 协议地址 —— 子窗口自动跟随，不用改代码。
    let base = win_of(app, 1)
        .and_then(|w| w.url().ok())
        .map(|u| {
            let s = u.to_string();
            s.split('#').next().unwrap_or("").to_string()
        })
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| {
            // [20261015 混合壳] core 模式无主窗口可取 base → 用本地桥地址
            if crate::core_bridge::is_core_mode() {
                format!(
                    "http://127.0.0.1:{}/index.html",
                    crate::core_bridge::core_port().unwrap_or(23654)
                )
            } else {
                "http://tauri.localhost/index.html".to_string()
            }
        });
    // Electron 生产态是 index.html#/route?windowId=x，这里保持一致（vue-router hash 模式）。
    let url_str = format!("{base}#/{route}?{params}");
    let url = url_str
        .parse()
        .map_err(|e| format!("URL 解析失败: {e}"))?;

    if crate::core_bridge::is_core_mode() {
        // [20261015 混合壳] core 模式没有 WebView2：子窗口由 Electron 壳创建。
        // 把算好的 url 与尺寸/位置原样转交；close/closeWindowed 事件由 Electron 侧
        // 直接回给渲染进程（与原版 Electron 壳行为一致）。
        let payload = json!({
            "label": label,
            "windowId": window_id,
            "url": url_str,
            "width": width,
            "height": height,
            "minWidth": min_w,
            "minHeight": min_h,
            "frame": frame,
            "x": x,
            "y": y,
        });
        crate::core_bridge::broadcast("__core_createWindow", &payload);
        return Ok(());
    }

    let mut builder = WebviewWindowBuilder::new(app, &label, WebviewUrl::External(url))
        .title("InDesk")
        .inner_size(w, h)
        .min_inner_size(min_w, min_h)
        .decorations(frame)
        .resizable(true)
        // 必须显式写 visible(true)：Tauri 程序化创建窗口的默认值是"隐藏"（与 Electron 相反），
        // 漏掉这行窗口会建出来但不显示、webview 也不会加载页面。
        .visible(true)
        // 关键：必须和主窗口用完全相同的 WebView2 参数，否则第二个环境创建失败（详见 lib.rs 注释）
        .additional_browser_args(&crate::browser_args());

    if let (Some(px), Some(py)) = (x, y) {
        builder = builder.position(px, py);
    }

    ipc_log(&format!("createWindow label={label} url={url_str}"));
    let win = builder.build().map_err(|e| {
        ipc_log(&format!("createWindow 失败 label={label}: {e}"));
        e.to_string()
    })?;
    ipc_log(&format!(
        "createWindow ok label={label} visible={:?} size={:?}",
        win.is_visible(),
        win.outer_size().map(|s| (s.width, s.height))
    ));

    // 窗口关闭事件回传前端（对应 Electron 的 close / closed）
    let app_handle = app.clone();
    let label_clone = label.clone();
    win.on_window_event(move |event| match event {
        tauri::WindowEvent::CloseRequested { .. } => {
            let _ = app_handle.emit(
                "response_closeWindow",
                json!({ "requestId": "", "data": { "windowId": window_id }, "code": 0 }),
            );
        }
        tauri::WindowEvent::Destroyed => {
            let _ = app_handle.emit(
                "response_closeWindowed",
                json!({ "requestId": "", "data": { "windowId": window_id }, "code": 0 }),
            );
        }
        _ => {}
    });
    let _ = label_clone;
    Ok(())
}

/// 应用前端热更包：解压到用户数据目录，下次启动优先从这里加载。
fn apply_web_update(app: &AppHandle, zip_b64: &str, version: &str) -> Result<String, String> {
    use base64_decode::decode;

    let bytes = decode(zip_b64).map_err(|e| format!("base64 解码失败: {e}"))?;
    let dir = web_update_dir(app);
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;

    let tmp = dir.join(format!("update-{version}.zip"));
    std::fs::write(&tmp, &bytes).map_err(|e| e.to_string())?;

    let target = dir.join("dist");
    if target.exists() {
        let _ = std::fs::remove_dir_all(&target);
    }
    std::fs::create_dir_all(&target).map_err(|e| e.to_string())?;

    extract_zip(&tmp, &target)?;
    let _ = std::fs::remove_file(&tmp);

    // 记录应用时间（秒）。启动时与壳构建时间比较：
    // 比当前壳旧的热更包会在 web_update_ready() 里被删除并回落内置资源，
    // 防止"过期热更包锁死新壳"（旧代码接口相对地址全 404 的死循环）。
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let _ = std::fs::write(dir.join("applied.txt"), now.to_string());

    Ok(target.to_string_lossy().to_string())
}

fn extract_zip(zip_path: &std::path::Path, dest: &std::path::Path) -> Result<(), String> {
    let file = std::fs::File::open(zip_path).map_err(|e| e.to_string())?;
    let mut archive = zip::ZipArchive::new(file).map_err(|e| e.to_string())?;
    for i in 0..archive.len() {
        let mut entry = archive.by_index(i).map_err(|e| e.to_string())?;
        let Some(rel) = entry.enclosed_name() else {
            continue;
        };
        let out = dest.join(rel);
        if entry.is_dir() {
            std::fs::create_dir_all(&out).map_err(|e| e.to_string())?;
        } else {
            if let Some(parent) = out.parent() {
                std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
            }
            let mut f = std::fs::File::create(&out).map_err(|e| e.to_string())?;
            std::io::copy(&mut entry, &mut f).map_err(|e| e.to_string())?;
        }
    }
    Ok(())
}

/// 界面热更是否可用：必须 index.html 与 assets 目录都在（说明上次解压是完整的），
/// 且应用时间不早于当前壳的构建时间（过期热更包直接删除并回落内置资源）。
pub fn web_update_ready(app: &AppHandle) -> bool {
    let dir = web_update_dir(app).join("dist");
    if !(dir.join("index.html").is_file() && dir.join("assets").is_dir()) {
        return false;
    }
    let shell_ts: u64 = env!("INDESK_BUILD_TS").parse().unwrap_or(0);
    match std::fs::read_to_string(web_update_dir(app).join("applied.txt"))
        .ok()
        .and_then(|s| s.trim().parse::<u64>().ok())
    {
        Some(applied_ts) if applied_ts >= shell_ts => true,
        _ => {
            // 过期热更包（旧壳时代应用 / 无时间戳）：删除，让壳回落到内置资源。
            let _ = std::fs::remove_dir_all(&dir);
            false
        }
    }
}

/// 自定义协议（Windows 下访问地址为 http://indesk.localhost/...）的静态文件服务。
///
/// ⚠️ 主窗口**永远**走这个协议（lib.rs）：热更包有效时供热更文件；
/// 没有热更包 / 热更包过期时回落到**壳内置资源**（打包进二进制的前端）。
/// 这样 `__capture/frame` 帧通道在任何模式下都存在 —— 之前"无热更包走
/// tauri 内置协议"的回落模式会让这个端点落到 SPA 回落返回 index.html。
pub fn serve_web_update(
    app: &AppHandle,
    uri_path: &str,
    query: Option<&str>,
) -> tauri::http::Response<Vec<u8>> {
    // ⚠️ 静默取屏帧通道（20260913e 加）：走 WebView2 自定义协议拦截，
    // 不经过系统网络栈 —— 系统代理 / 防火墙 / 回环限制都影响不到它。
    // 前端 fetch('/__capture/frame')（同源，无 CORS 问题），拿到一帧完整画面。
    //
    // 1.4.0 起这一帧**不一定是 JPEG**：直送模式下是裸 BGRA/NV12（壳侧零编码）。
    // 所以响应带 `X-Indesk-Fmt` 声明格式，前端据此选解码方式；
    // `?after=<seq>` 用于让"帧号没变"的请求只回 204（省掉整帧重传）。
    if uri_path.starts_with("/__capture/frame") {
        let q = query.unwrap_or("");
        let (ow, oh) = crate::capture::out_size();
        let fmt = crate::capture::mode();

        // 元信息：前端启动取屏前先问一次，决定用 createImageBitmap 还是 VideoFrame
        if q.contains("meta=1") {
            let (strips, rows_per) = crate::capture::strip_layout();
            let body = serde_json::json!({
                "running": crate::capture::is_running(),
                "mode": fmt,
                "width": ow,
                "height": oh,
                // [20260918 capfix] 抓屏失败原因（空 = 正常）。前端轮询 meta 时
                // 若长期读到 fmt=starting，可同时看到这里的原因，不必再翻壳日志。
                "error": crate::capture::last_error(),
                // 分条并行 JPEG 的布局（mode = "jpegn" 时有效）：
                // 一帧 = strips 份独立 JPEG 首尾相接，第 i 条画在 y = i*rowsPer、高 = rowsPer
                // （最后一条高 = height - i*rowsPer）。
                "strips": strips,
                "rowsPer": rows_per,
            })
            .to_string()
            .into_bytes();
            return tauri::http::Response::builder()
                .status(200)
                .header("Content-Type", "application/json")
                .header("Cache-Control", "no-store, no-cache, must-revalidate")
                .header("Access-Control-Allow-Origin", "*")
                .body(body)
                .unwrap_or_else(|_| http_not_found());
        }

        // 帧号去重：直送模式一帧 4MB，重复帧没必要再传一遍
        let after: u64 = q
            .split('&')
            .find_map(|kv| kv.strip_prefix("after="))
            .and_then(|v| v.parse().ok())
            .unwrap_or(0);
        let (seq, bytes, lens) =
            crate::capture::latest_frame().unwrap_or((0, Vec::new(), Vec::new()));
        let (strips, rows_per) = crate::capture::strip_layout();
        // 1.4.1：把分条并行 JPEG 每条 JPEG 的**起始字节偏移**随帧一起下行
        // （形如 `0,15234,30456`）。前端据此直接切片，不必再扫 FFD8/FFD9 找边界 ——
        // 那个扫法有正确性隐患：JPEG 熵编码段里出现 FF D8 / FF D9 字节序列并不被排除。
        let offsets = crate::capture::offsets_csv(&lens);
        if after != 0 && seq <= after {
            return tauri::http::Response::builder()
                .status(204)
                .header("X-Indesk-Seq", seq.to_string())
                .header("X-Indesk-Fmt", fmt)
                .header("X-Indesk-Strips", strips.to_string())
                .header("X-Indesk-RowsPer", rows_per.to_string())
                .header("Cache-Control", "no-store")
                .header("Access-Control-Allow-Origin", "*")
                .body(Vec::new())
                .unwrap_or_else(|_| http_not_found());
        }

        let mut resp = tauri::http::Response::builder()
            .status(200)
            .header("Content-Type", crate::capture::content_type())
            .header("Cache-Control", "no-store, no-cache, must-revalidate")
            .header("X-Indesk-Seq", seq.to_string())
            .header("X-Indesk-Fmt", fmt)
            .header("X-Indesk-W", ow.to_string())
            .header("X-Indesk-H", oh.to_string())
            .header("X-Indesk-Strips", strips.to_string())
            .header("X-Indesk-RowsPer", rows_per.to_string())
            .header("Access-Control-Allow-Origin", "*");
        // 单张 JPEG / 直送像素时 offsets 为空串 → 不带这个头，老前端行为完全不变
        if !offsets.is_empty() {
            resp = resp.header("X-Indesk-Offsets", offsets);
        }
        return resp
            .body(bytes)
            .unwrap_or_else(|_| http_not_found());
    }
    let rel = uri_path.trim_start_matches('/');
    let rel = if rel.is_empty() { "index.html" } else { rel };
    // 目录穿越防护
    if rel.contains("..") {
        return http_not_found();
    }

    // ① 热更包优先
    let root = web_update_dir(app).join("dist");
    let mut full = root.join(rel);
    if full.is_dir() {
        full = full.join("index.html");
    }
    if let Ok(body) = std::fs::read(&full) {
        return tauri::http::Response::builder()
            .status(200)
            .header("Content-Type", mime_of(&full))
            .header("Access-Control-Allow-Origin", "*")
            .header("Cache-Control", "no-cache")
            .body(body)
            .unwrap_or_else(|_| http_not_found());
    }

    // ② 回落：壳内置资源（tauri 打包进二进制的前端）
    //    （20260913f 加：保证无热更包时 indesk.localhost 也能完整出页面 + 帧通道可用）
    let serve_embedded = |key: &str| -> Option<tauri::http::Response<Vec<u8>>> {
        app.asset_resolver().get(key.to_string()).map(|a| {
            tauri::http::Response::builder()
                .status(200)
                .header("Content-Type", mime_of(std::path::Path::new(key)))
                .header("Access-Control-Allow-Origin", "*")
                .header("Cache-Control", "no-cache")
                .body(a.bytes().to_owned())
                .unwrap_or_else(|_| http_not_found())
        })
    };
    if let Some(resp) = serve_embedded(rel) {
        return resp;
    }
    // ③ SPA 回落：任何未知路径都给 index.html（vue-router hash 模式实际用不到，保险）
    if rel != "index.html" {
        if let Some(resp) = serve_embedded("index.html") {
            return resp;
        }
    }
    http_not_found()
}

fn http_not_found() -> tauri::http::Response<Vec<u8>> {
    tauri::http::Response::builder()
        .status(404)
        .header("Access-Control-Allow-Origin", "*")
        .body(Vec::new())
        .unwrap_or_else(|_| unreachable!("404 响应构造不会失败"))
}

fn mime_of(path: &std::path::Path) -> &'static str {
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    match ext.as_str() {
        "html" | "htm" => "text/html; charset=utf-8",
        "js" | "mjs" | "cjs" => "text/javascript; charset=utf-8",
        "css" => "text/css; charset=utf-8",
        "json" | "map" => "application/json; charset=utf-8",
        "svg" => "image/svg+xml",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "ico" => "image/x-icon",
        "woff" => "font/woff",
        "woff2" => "font/woff2",
        "ttf" => "font/ttf",
        "otf" => "font/otf",
        "eot" => "application/vnd.ms-fontobject",
        "txt" => "text/plain; charset=utf-8",
        "wasm" => "application/wasm",
        "mp4" => "video/mp4",
        "webm" => "video/webm",
        "mp3" => "audio/mpeg",
        _ => "application/octet-stream",
    }
}

/// base64 的最小实现，避免为一个功能引入完整依赖。
/// `pub(crate)`：壳自更新（applyShellUpdate）也复用它解码新 exe。
pub(crate) mod base64_decode {
    pub fn decode(s: &str) -> Result<Vec<u8>, String> {
        const TABLE: &[u8; 64] =
            b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
        let mut lookup = [255u8; 256];
        for (i, &c) in TABLE.iter().enumerate() {
            lookup[c as usize] = i as u8;
        }
        let bytes: Vec<u8> = s
            .bytes()
            .filter(|b| !b" \n\r\t".contains(b))
            .collect();
        let mut out = Vec::with_capacity(bytes.len() * 3 / 4);
        let mut buf = 0u32;
        let mut bits = 0u32;
        for b in bytes {
            if b == b'=' {
                break;
            }
            let v = lookup[b as usize];
            if v == 255 {
                return Err(format!("非法 base64 字符: {}", b as char));
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
}

/// 按"字符"截断，避免在多字节 UTF-8 边界上 panic。
fn truncate(s: &str, max_chars: usize) -> String {
    if s.chars().count() <= max_chars {
        s.to_string()
    } else {
        let head: String = s.chars().take(max_chars).collect();
        format!("{head}...(截断)")
    }
}

/// 排查用日志：只有设置了环境变量 `INDESK_IPC_LOG=1` 时才写文件，
/// 位置为 `%TEMP%\indesk-ipc.log`。正式运行时零开销。
pub fn ipc_log(msg: &str) {
    if std::env::var("INDESK_IPC_LOG").is_err() {
        return;
    }
    use std::io::Write;
    let path = std::env::temp_dir().join("indesk-ipc.log");
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
    {
        let _ = writeln!(f, "{msg}");
    }
}

/// [20261010 批次014 S1'] 系统原生锁屏（LockWorkStation）。
/// 只锁"会话"，不碰物理键鼠：动鼠标即出登录界面，输密码回来，电脑照常可用。
#[cfg(windows)]
fn lock_workstation_impl(request_id: &str) -> Result<Value, String> {
    #[link(name = "user32")]
    extern "system" {
        fn LockWorkStation() -> i32;
    }
    // SAFETY：LockWorkStation 无参数、失败返回 0，不崩溃、不阻塞。
    let sent = unsafe { LockWorkStation() };
    crate::capture::capture_diag_pub(&format!("[lock] LockWorkStation 调用{}", if sent != 0 { "完成" } else { "失败" }));
    Ok(ok(request_id, json!({ "locked": sent != 0 })))
}

#[cfg(not(windows))]
fn lock_workstation_impl(request_id: &str) -> Result<Value, String> {
    Ok(ok(request_id, json!({ "locked": false, "error": "non-windows" })))
}

/// [20261010 批次015 工具栏] 在被控端打开任务管理器（taskmgr.exe 独立 GUI 进程，
/// DETACHED —— 不随壳退出，与会话无关）。
#[cfg(windows)]
fn open_task_manager_impl(request_id: &str) -> Result<Value, String> {
    use std::os::windows::process::CommandExt;
    const DETACHED_PROCESS: u32 = 0x0000_0008;
    const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
    let r = std::process::Command::new("C:\\Windows\\System32\\taskmgr.exe")
        .creation_flags(DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP)
        .spawn();
    let (opened, err) = match r {
        Ok(_) => (true, String::new()),
        Err(e) => (false, e.to_string()),
    };
    let note = if opened {
        "完成".to_string()
    } else {
        format!("失败: {err}")
    };
    crate::capture::capture_diag_pub(&format!("[taskmgr] 打开任务管理器{note}"));
    Ok(ok(request_id, json!({ "opened": opened, "error": err })))
}

#[cfg(not(windows))]
fn open_task_manager_impl(request_id: &str) -> Result<Value, String> {
    Ok(ok(request_id, json!({ "opened": false, "error": "non-windows" })))
}
