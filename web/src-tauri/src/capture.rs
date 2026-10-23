//! 无人值守「静默取屏」核心实现（壳侧）。
//!
//! ## 为什么不能用 getDisplayMedia
//!
//! 早先两版尝试用 WebView2 的 `ICoreWebView2_27::ScreenCaptureStarting` 事件在宿主层
//! "静默放行"，实测无效。查微软官方文档后确认**这条路从设计上就不成立**：
//! 该事件参数的成员只有 `Cancel` 与 `Handled`，其中
//!   - `put_Cancel(TRUE)` = 取消取屏，页面收到 NotAllowedError；
//!   - `put_Handled(TRUE)` = **仅表示"阻止下游事件处理器被调用"**（事件路由控制），
//!     与"自动选择捕获源"毫无关系，也没有任何指定捕获源的方法。
//! 即 WebView2 不存在"宿主替用户选好屏幕、不显示选择窗口"的能力。
//!
//! 参考同类产品的做法也能印证这一点：
//!   - 原版 billd-desk 用 Electron 的 `desktopCapturer` —— Electron 带完整 Chromium
//!     **browser 层**，`--auto-select-desktop-capture-source` 才有意义；WebView2 只有
//!     content 层，读不到这类开关。
//!   - ToDesk / 向日葵 / 网易 UU 远程：被控端一律**原生采集**（Windows 上 GDI 或
//!     DXGI Desktop Duplication 取帧）+ 硬件编码，完全不经过浏览器取屏接口。
//!
//! 所以无人值守要成立，只能**绕开浏览器取屏链路**：由壳自己抓屏，
//! 前端把画面重绘到 canvas，再用 `canvas.captureStream()` 包装成标准视频轨交给 WebRTC。
//! 全程不调用 `getDisplayMedia`，因此不可能出现任何系统共享选择窗口。
//!
//! ## 两种发布格式（1.4.0「换引擎」）
//!
//! | 模式 | 壳侧做什么 | 前端做什么 | 每帧开销（本机实测） |
//! |---|---|---|---|
//! | `jpeg`（默认，兼容老前端） | GPU 缩放 → 回读目标尺寸 BGRA → **JPEG 编码** | `createImageBitmap` 解码 → canvas | 壳侧 29~49ms |
//! | `bgra` / `nv12`（直送） | GPU 缩放 → 回读目标尺寸像素，**不编码** | `new VideoFrame(...,{format})` 零解码 → canvas | 壳侧 **2.6ms** |
//!
//! 直送模式省掉的正是实测出来的两个真凶：CPU 盒式下采样（28.3ms/帧）与
//! JPEG 编码（29~49ms/帧）。原始测量数据见 `examples/engine_proto.rs` 的模块注释。
//! 注意直送模式**不是**"把压缩挪到别处"：最终压缩本来就由 WebRTC 硬件编码器
//! （`canvas.captureStream()` 那条轨）完成，本来的结构里就已经压了一次。
//!
//! ## 数据通道
//!
//! 壳在 `127.0.0.1` 上开一个只读服务（`multipart/x-mixed-replace`，载荷按发布格式
//! 为 JPEG 或裸 BGRA/NV12），前端另可走 WebView2 自定义协议 `/__capture/frame`。
//! 两者都仅监听/服务本机，抓屏线程只在被显式启动时运行。

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Condvar, Mutex, OnceLock};
use std::time::{Duration, Instant};

use crate::dxgi::OutFormat;

// ------------------------------------------------------------------ 状态

/// 最新一帧（单帧环形槽，只保留最新，天然丢弃积压帧）。
struct Slot {
    inner: Mutex<SlotInner>,
    cv: Condvar,
}

struct SlotInner {
    seq: u64,
    frame: Option<Vec<u8>>,
    /// 分条并行 JPEG（`MODE_JPEGN`）每条 JPEG 的**起始字节偏移**，首项恒为 0；
    /// 第 i 条的范围是 `[lens[i], lens[i+1])`，末条到 `frame.len()`。
    /// 空 = 单张 JPEG 或直送像素（前端据此走老路径）。
    ///
    /// **必须与 `frame` 同锁发布**：偏移描述的就是这一帧的字节切点，
    /// 若放在另一个静态变量里，客户端可能拿到 A 帧的字节配 B 帧的偏移。
    lens: Vec<u32>,
}

fn slot() -> &'static Slot {
    static S: OnceLock<Slot> = OnceLock::new();
    S.get_or_init(|| Slot {
        inner: Mutex::new(SlotInner {
            seq: 0,
            frame: None,
            lens: Vec::new(),
        }),
        cv: Condvar::new(),
    })
}

/// 原始帧槽（抓屏线程 → 编码线程）。20260913g 流水线化：
/// 抓屏只负责取帧与拷贝，JPEG 编码挪到独立线程，只编最新一帧
/// （抓屏期间不再被编码阻塞；编码跟不上时自动跳过积压帧，鼠标延迟反而更低）。
/// **直送模式（bgra/nv12）下没有编码线程，这个槽不会被用到。**
fn raw_slot() -> &'static Slot {
    static S: OnceLock<Slot> = OnceLock::new();
    S.get_or_init(|| Slot {
        inner: Mutex::new(SlotInner {
            seq: 0,
            frame: None,
            lens: Vec::new(),
        }),
        cv: Condvar::new(),
    })
}

/// 抓屏线程是否在跑。
static RUNNING: AtomicBool = AtomicBool::new(false);

/// **[1.4.38 privacy v2] 隐私屏开关**（被控端**本机物理屏幕**黑化）。
///
/// ## 语义变更（2026-10-02，取代 1.4.37 的"发布黑化"）
///
/// 1.4.37 把发布给主控端的帧整帧换黑——主控端黑屏、被控端显示器毫无变化，
/// 语义完全颠倒（用户实测判定"功能不对"）。正确语义：
///
/// - **被控端本机黑屏**：原生全屏黑色遮罩窗（`privacy_overlay` 模块），
///   带 `WDA_EXCLUDEFROMCAPTURE` —— 本机肉眼可见纯黑、抓屏结果里没有它；
/// - **主控端照常观看**：发布层（publish/publish_raw）**不再碰帧内容**，
///   真实画面照常发布，帧心跳/自愈判据零风险（§24 铁律依旧成立）。
///
/// 开启失败（如 Win10 19041 之前的系统不支持 WDA_EXCLUDEFROMCAPTURE）时
/// 返回 false，绝不留下"遮罩把远端画面也盖黑"的半残状态。
static PRIVACY_SCREEN: AtomicBool = AtomicBool::new(false);

/// 设置隐私屏开关。返回设置后的状态（true = 本机遮罩+输入锁均已就位）。
///
/// **[1.4.39 privacy v3]** 语义升级：隐私屏 = **黑屏 + 输入锁**。
/// - 开启：先开遮罩，再上输入锁；任一失败 ⇒ 立即回滚两边并返回 false，
///   绝不留"黑着屏但键鼠没锁"或"锁了键鼠但没黑屏"的半残状态；
/// - 关闭：**先解锁再拆遮罩**（顺序反了会有"黑屏还在、键鼠被锁"的窗口期，
///   本机用户点逃生横幅时会感觉操作没反应）。
pub fn set_privacy_screen(on: bool) -> bool {
    let ok = if on {
        let overlay_ok = super::privacy_overlay::set(true);
        let lock_ok = super::input_lock::set(true);
        if !(overlay_ok && lock_ok) {
            // 全有或全无：回滚两边（幂等，重复调用无害）。
            let _ = super::input_lock::set(false);
            let _ = super::privacy_overlay::set(false);
            capture_diag(&format!(
                "[privacy] 开启失败回滚（overlay={:?} lock={:?}）",
                overlay_ok, lock_ok
            ));
            false
        } else {
            true
        }
    } else {
        // 关闭：先解锁再拆遮罩；关锁失败不阻断拆遮罩（锁失效优先级更高）。
        let lock_ok = super::input_lock::set(false);
        let overlay_ok = super::privacy_overlay::set(false);
        overlay_ok && lock_ok
    };
    PRIVACY_SCREEN.store(ok && on, Ordering::SeqCst);
    capture_diag(&format!(
        "[privacy] 隐私屏 {}（ok={:?}）",
        if on { "开启" } else { "关闭" },
        ok
    ));
    ok && on
}

/// 当前隐私屏状态。
pub fn privacy_screen() -> bool {
    PRIVACY_SCREEN.load(Ordering::SeqCst)
}
/// 会话号：每次 start/stop 自增，旧线程据此自杀，避免多重抓屏线程并存。
static SESSION: AtomicU64 = AtomicU64::new(0);

/// **[20260918 capfix]** 抓屏线程句柄：`start()` 要**同步等旧线程真正退出**。
///
/// 为什么必须同步等：`DuplicateOutput` 在同一适配器上是**独占**资源
/// （真机实测 lab/bin/dxgi_excl.rs：旧 capturer 存活时再建必然
/// `0x80070057 参数错误`；旧 capturer drop 后立刻可建）。
/// 原实现只置 `RUNNING=false` 就 spawn 新线程 → 新线程建复制对象时旧 capturer
/// 还没 drop → 必然失败 → `OUT_MODE` 永远停在 `starting` → 前端零帧（死画面）。
static CAP_THREAD: Mutex<Option<std::thread::JoinHandle<()>>> = Mutex::new(None);
/// **[20260918 capfix]** 启动/停止序列化锁：并发 `captureStart` 不得同时走
/// "停旧 → 建新"这段临界区，否则两个新线程会互相抢复制对象。
static CAP_LOCK: Mutex<()> = Mutex::new(());

/// **[20260918 capfix]** 抓屏失败原因（供 `captureStatus` / 诊断文件读取）。
/// 空串 = 无错误。抓屏线程在任何失败路径都必须写这里 ——
/// 原实现的失败只 eprintln 到壳的 stderr，而壳由 explorer 拉起时 stderr 无处可去，
/// 现场就是"零线索"。
static CAP_ERR: Mutex<String> = Mutex::new(String::new());

/// 设置抓屏错误原因（同时写诊断文件，便于壳外取证）。
fn set_cap_err(msg: &str) {
    if let Ok(mut g) = CAP_ERR.lock() {
        *g = msg.to_string();
    }
    capture_diag(msg);
}

/// 当前抓屏错误原因（空 = 正常）。
pub fn last_error() -> String {
    CAP_ERR.lock().map(|g| g.clone()).unwrap_or_default()
}

/// **[20260918 capfix]** 抓屏诊断：把关键事件追加写到 exe 同目录 `capture_diag.log`。
/// 壳由 explorer 拉起时 stderr 丢失，这类"零诊断"是本次排障最大的时间黑洞。
/// [20261020] 改 pub(crate)：dxgi.rs 的建链里程碑也要落这里（NVENC/复制对象
/// 卡死现场必须有据可查）。
pub(crate) fn capture_diag(msg: &str) {
    use std::io::Write;
    let p = std::env::current_exe()
        .ok()
        .and_then(|e| e.parent().map(|d| d.join("capture_diag.log")));
    if let Some(p) = p {
        let ts = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis())
            .unwrap_or(0);
        let _ = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(p)
            .and_then(|mut f| f.write_all(format!("{ts} {msg}\r\n").as_bytes()));
    }
}
/// 只读服务端口（0 = 未启动）。
static PORT: AtomicU32 = AtomicU32::new(0);

/// [20260919 h264probe] 供 ipc 层写取证日志（captureStart 的调用者与参数）。
pub fn capture_diag_pub(msg: &str) {
    capture_diag(msg);
}
/// 输出尺寸，高 16 位宽 / 低 16 位高。
static OUT_SIZE: AtomicU32 = AtomicU32::new(0);

// 壳当前发布的帧格式。前端**必须**按它选解码方式，不能假定是 JPEG。
const MODE_STARTING: u32 = 255;
const MODE_JPEG: u32 = 0;
const MODE_BGRA: u32 = 1;
const MODE_NV12: u32 = 2;
/// 分条并行 JPEG：一帧 = N 份**独立 JPEG**首尾相接（N 由 `OUT_STRIPS` 给出），
/// 前端按 `X-Indesk-RowsPer` 逐条画到 canvas 对应纵坐标。
/// 为什么这么干见 `encode_jpeg_strips` 的注释。
const MODE_JPEGN: u32 = 3;
/// **[1.4.5 阶段①] 壳侧 NVENC 硬编 H.264**：一帧 = 一个 Annex-B 访问单元（AU）。
/// 前端必须用 `WebCodecs VideoDecoder`（`avc:{format:'annexb'}`）解码，
/// **不能** `new VideoFrame(...)`（见 `dxgi::OutFormat::H264`）。
const MODE_H264: u32 = 4;
/// **[20260918b capdead]** 「已停止」——与 `MODE_STARTING`（从未定型）严格区分。
///
/// 为什么必须区分：`stop()` 曾把 `OUT_MODE` 一并重置成 `MODE_STARTING`，于是
/// **「抓屏从未启动」与「启动过又停了」在外部完全同形**（都是 starting/Strips=1/RowsPer=0）。
/// 线上「连上就死画面」排障时，这个歧义让判断反复摇摆：到底是建链失败，
/// 还是前端压根没调 `captureStart`？两种解释都与观测自洽，无法收敛。
/// 现在 `stop()` 写 `MODE_STOPPED`，`starting` 就只剩一个含义：
/// **抓屏线程真正在跑、但还没定下格式**。
const MODE_STOPPED: u32 = 254;

/// **[20260918c idlekill]** 壳侧记录的真实主控（RTC 对端）数量。
///
/// 为什么必须有它 —— 这是「连上就死」在真机上真正致命的那一环：
/// 前端的空闲节能判据原本只看 `appStore.remoteDesk.size`（pinia Map 成员数），
/// 而该 Map 的写入点在 `acceptIncoming()`。**主控正在连的过程中**（信令握手已完成、
/// `deskStartRemoteResult` 尚未到达）`size` 仍为 0 —— 于是"主控刚把画面拉起来"
/// 的那一瞬间被判成"无人值守空闲"，`stopAnchorStream()` 把刚起来的取屏掐断。
/// 用户看到的就是**连接成功的一刹那画面冻住**。
///
/// 前端 `remoteDesk` 一旦变化就把它同步到这里，看门狗再回头查这个值：
/// 判据由"我以为有几个人"变成"**壳记着几个人**"，不再受前端时序影响。
static VIEWER_COUNT: AtomicU32 = AtomicU32::new(0);

/// 前端同步过来的当前主控数（见 `set_viewer_count` 的说明）。
pub fn set_viewer_count(n: u32) {
    VIEWER_COUNT.store(n, Ordering::Relaxed);
}

/// 当前主控数。
pub fn viewer_count() -> u32 {
    VIEWER_COUNT.load(Ordering::Relaxed)
}

/// [20260918e idleslow] 空闲降速开关。
///
/// 为什么是「降速」而不是 v52 那样「停管线」：停管线有"主控刚连上、
/// captureStart 还没跑完"的误杀窗口（连上就死的祸根）；降速只是抓屏/编码
/// 循环把节流目标改成 2fps、质量压到 50，**管线常开**，恢复 = 翻一个原子位，
/// 零延迟零风险。前端在看门狗里判定、经 `setCaptureIdle` IPC 写入。
static IDLE: AtomicBool = AtomicBool::new(false);

/// **[20260919 h264gate]** 最近一次 `start()` 请求的发布格式意图。
///
/// 为什么 OUT_MODE 之外还要记这个：`start()` 立刻返回、抓屏线程异步建链，
/// h264 请求后 `OUT_MODE` 要等 GPU 链建好（几十~几百 ms）才变成 H264 ——
/// 这段 `MODE_STARTING` 窗口内，JPEG 重拉（看门狗/档位同步等自愈路径）
/// 会穿过"OUT_MODE==H264"这道闸把会话抢走（实测 250ms 后就被抢了一次）。
/// 记下"意图"后，闸门在 starting 窗口内同样生效。
static WANT_MODE: AtomicU32 = AtomicU32::new(MODE_STOPPED);
/// 意图对应的会话号：闸门只在「意图 = h264 且该会话仍是最新会话」时生效；
/// stop() / 新 start 推进 SESSION 后自动放行。
static WANT_SESSION: AtomicU64 = AtomicU64::new(0);

/// 前端同步过来的空闲降速开关（详见 `IDLE` 说明）。
pub fn set_idle(on: bool) {
    if IDLE.swap(on, Ordering::Relaxed) != on {
        eprintln!("[InDesk] 抓屏空闲降速: {}", if on { "ON(2fps/q50)" } else { "OFF(全速)" });
    }
}

/// 空闲降速状态下的节流 fps。
fn effective_fps(fps: u32) -> u32 {
    if IDLE.load(Ordering::Relaxed) { 2 } else { fps }
}

/// 当前发布格式（`MODE_*`）。`MODE_STARTING` = 抓屏线程还没定下来。
static OUT_MODE: AtomicU32 = AtomicU32::new(MODE_STOPPED);
/// 分条数（`MODE_JPEGN` 时 >1，其余恒为 1）。
static OUT_STRIPS: AtomicU32 = AtomicU32::new(1);
/// 每条的行数（16 行对齐）。前端**只认这个值**，不要自己算，
/// 免得壳与前端各算一套、边界错一行导致画面撕裂。
static OUT_ROWS_PER: AtomicU32 = AtomicU32::new(0);

/// 「下一帧必须是关键帧」的请求。**H.264 模式专用**，其它模式调用无副作用。
static WANT_KEYFRAME: AtomicBool = AtomicBool::new(false);

/// **[1.4.5 阶段①]** 请求下一帧编成 IDR（关键帧）。
///
/// 调用时机：新客户端接入、丢包后恢复、切换分辨率/码率档位之后。
/// 不请求的话，客户端最坏要等到周期关键帧（`idr_period`，默认 2 秒）才有画面 ——
/// 表现就是「连上了但黑屏 1~2 秒」。
///
/// 幂等：连调多次只生效一次（`capture_loop_dxgi` 用 swap 消费）。
pub fn request_keyframe() {
    WANT_KEYFRAME.store(true, Ordering::Relaxed);
}

pub fn port() -> u16 {
    PORT.load(Ordering::Relaxed) as u16
}

/// 当前发布格式名："jpeg" / "jpegn" / "bgra" / "nv12" / "starting"。
pub fn mode() -> &'static str {
    match OUT_MODE.load(Ordering::Relaxed) {
        MODE_JPEG => "jpeg",
        MODE_JPEGN => "jpegn",
        MODE_BGRA => "bgra",
        MODE_NV12 => "nv12",
        MODE_H264 => "h264",
        // [20260918b capdead] "stopped" = 已停/从未启动；"starting" = 在跑但未定格式。
        // 分开之后，"抓屏没起来"与"建链没成功"才可区分（见 MODE_STOPPED 注释）。
        MODE_STOPPED => "stopped",
        _ => "starting",
    }
}

/// 当前发布格式对应的 HTTP Content-Type。
pub fn content_type() -> &'static str {
    match OUT_MODE.load(Ordering::Relaxed) {
        MODE_BGRA => "application/x-indesk-bgra",
        MODE_NV12 => "application/x-indesk-nv12",
        MODE_H264 => "application/x-indesk-h264",
        MODE_JPEGN => "application/x-indesk-jpegn",
        _ => "image/jpeg",
    }
}

/// 分条并行 JPEG 的条数与每条行数（供取帧响应头 `X-Indesk-Strips/-RowsPer` 用）。
pub fn strip_layout() -> (u32, u32) {
    (
        OUT_STRIPS.load(Ordering::Relaxed),
        OUT_ROWS_PER.load(Ordering::Relaxed),
    )
}

/// 输出尺寸（宽, 高）。抓屏未启动时返回 (0, 0)。
pub fn out_size() -> (u32, u32) {
    let v = OUT_SIZE.load(Ordering::Relaxed);
    ((v >> 16) & 0xFFFF, v & 0xFFFF)
}

/// 预置发布格式决策（1.4.1，消除「连上后等约 2 秒才出画面」）。
///
/// JPEG 家族的判断 —— 走单张还是分条 —— **只依赖输出高度与核数**，
/// 与 DXGI / GPU 缩放链是否可用无关，所以能在 `start()` 里当场定下来，
/// 让 `captureStart` 的应答直接带上真实格式，前端不必再轮询 meta。
///
/// 直送格式（bgra/nv12）不行：它取决于 GPU 缩放链能否建成
/// （见 `capture_loop` 的判定），只能在抓屏线程里定。此处保持 `MODE_STARTING`，
/// 前端仍走轮询兜底（那条路径生产上不用，但必须留着不能失效）。
fn preset_mode(want: Option<OutFormat>, want_multi: bool, oh: u32) {
    if want.is_some() {
        OUT_MODE.store(MODE_STARTING, Ordering::Relaxed);
        OUT_STRIPS.store(1, Ordering::Relaxed);
        OUT_ROWS_PER.store(0, Ordering::Relaxed);
        return;
    }
    if want_multi {
        let n = choose_strips(oh);
        if n > 1 {
            OUT_MODE.store(MODE_JPEGN, Ordering::Relaxed);
            OUT_STRIPS.store(n, Ordering::Relaxed);
            OUT_ROWS_PER.store(rows_per_strip(oh, n), Ordering::Relaxed);
            return;
        }
    }
    OUT_MODE.store(MODE_JPEG, Ordering::Relaxed);
    OUT_STRIPS.store(1, Ordering::Relaxed);
    OUT_ROWS_PER.store(0, Ordering::Relaxed);
}

/// 取最新一帧（帧号 + 字节 + 分条偏移），供 `http://indesk.localhost/__capture/frame`
/// 内部协议通道使用。该通道走 WebView2 自定义协议拦截，**不经过系统网络栈**，
/// 系统代理 / 防火墙 / 回环限制都影响不到它 —— 这是比 127.0.0.1 长连接更稳的取帧路径。
///
/// 1.4.1 起连同**每条的起始偏移**一起返回，且与 `seq` 取自同一把锁 ——
/// 偏移描述的就是这一帧的切点，绝不能让客户端拿到 A 帧的字节配 B 帧的偏移。
pub fn latest_frame() -> Option<(u64, Vec<u8>, Vec<u32>)> {
    let s = slot();
    let g = s.inner.lock().ok()?;
    g.frame.clone().map(|f| (g.seq, f, g.lens.clone()))
}

pub fn is_running() -> bool {
    RUNNING.load(Ordering::Relaxed)
}

/// 启动参数（前端可调）。
#[derive(Clone)]
pub struct StartOptions {
    /// 输出画面最大宽度，超出则等比缩小（0 = 不缩放，输出屏幕原始物理分辨率）。
    pub max_width: u32,
    /// JPEG 质量 20~95（仅 `mode = "jpeg"` 时有效）。
    pub quality: u8,
    /// 目标帧率。
    pub fps: u32,
    /// 期望的发布格式：`"jpeg"`（默认，兼容老前端）/ `"bgra"` / `"nv12"` / `"h264"`。
    /// 像素格式与 H.264 都需要 GPU 链可用；不可用时壳会自动退回 `jpeg`，
    /// 前端**以 `/__capture/frame` 返回的 `X-Indesk-Fmt`（或 meta）为准**。
    pub mode: String,
    /// **[1.4.5 阶段①]** H.264 目标码率（kbps，仅 `mode = "h264"` 生效）。
    /// 0 = 用默认档 4000（依据实测「服务器出口 ≈5.5Mbps」，v53 两端默认档即为 4000）。
    /// 前端应当把用户选的档位传进来，否则 WebRTC 那边的码率上限与壳侧编码码率不一致，
    /// 要么浪费带宽、要么被中继丢包。
    pub bitrate_kbps: u32,
    /// **[20261021 七轮]** H.264 编码后端：`"nvenc"`（默认，硬编）/ `"soft"`（CPU 软编）。
    /// NVENC 建不出来时自动兜底到软编，不再直接退 JPEG。
    /// 软编分辨率会收口到 1152 宽（实测 36ms/帧 ≈25fps，1280 宽只有 ≈21fps）。
    pub enc: String,
}

impl Default for StartOptions {
    fn default() -> Self {
        Self {
            max_width: 1920,
            quality: 62,
            fps: 15,
            mode: "jpeg".into(),
            bitrate_kbps: 4000,
            enc: "nvenc".into(),
        }
    }
}

// ------------------------------------------------------------------ 对外 API

/// 启动静默取屏，返回 (端口, 输出宽, 输出高)。
/// 1.4.1 起：JPEG 家族的发布格式在返回前就已定下（`preset_mode`），`mode()` 立刻可读；
/// 仅直送格式（bgra/nv12）可能短暂为 `MODE_STARTING`，用 `/__capture/frame` 的 meta 查询。
pub fn start(opt: StartOptions) -> Result<(u16, u32, u32), String> {
    // [20260919 h264gate] H264 直送会话**独占**抓屏：非 h264 的 captureStart 一律拒绝。
    //
    // 为什么必须有这道闸（20260919 h264probe 取证实锤）：主控连上 → h264Ready →
    // `captureStart mode=h264 from=h264-direct` 启动，**6 秒后**被
    // `mode=jpegn from=jpeg-silent` 覆盖 —— 被控端前端有多个会重拉 JPEG 抓屏的
    // 自愈路径（看门狗、capdead 重试、轨 ended 处理），它们不知道 h264 已经接管，
    // 一旦触发就把 h264 会话打回 jpegn。之后 h264Stream 继续把 JPEG 字节当
    // AU 发给主控端 → 解码器 Decoding error 风暴 → "切直送后几秒永久冻结"。
    // 停用 h264 的唯一入口是 captureStop（h264Stream.stop / 会话断开），届时
    // RUNNING/OUT_MODE 复位，JPEG 会话自然可以再起。
    // ⚠️ 闸门必须放在 stop_and_join **之前** —— 否则 JPEG start 虽被拒绝，
    // h264 抓屏线程也已经被它顺手杀掉了（start 第一步就是停旧会话）。
    // [20261010 heal] 僵尸放行：gate 拒绝的前提是 h264 会话「真的还活着」。
    // 若抓屏线程已退出而 RUNNING 仍为 true（异常 teardown 的僵尸态），gate 必须
    // 放行，让下面的 stop_and_join 清理残局后重建 —— 否则 h264 线程已死 +
    // JPEG 永久被拒 = 双死锁，只有重启壳能解（「重拉数千次全败」的元凶）。
    let cap_thread_dead = CAP_THREAD
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .as_ref()
        .map(|h| h.is_finished())
        .unwrap_or(true);
    if opt.mode != "h264"
        && WANT_MODE.load(Ordering::Relaxed) == MODE_H264
        && WANT_SESSION.load(Ordering::Relaxed) == SESSION.load(Ordering::Relaxed)
        && RUNNING.load(Ordering::Relaxed)
        && !cap_thread_dead
    {
        return Err("H264 直送会话进行中，JPEG 抓屏暂不可用".to_string());
    }
    // [20260918 capfix] 整个「停旧 → 建新」必须串行：并发 start 会互相抢
    // DXGI 复制对象（独占），表现为随机失败 + OUT_MODE 卡在 starting。
    let _serial = CAP_LOCK.lock().unwrap_or_else(|e| e.into_inner());

    // [20260918 capfix] 同步停旧：必须等旧线程**真正退出**（其 capturer 已 drop），
    // 否则新线程建 `DuplicateOutput` 必然 0x80070057。这是死画面的直接根因。
    stop_and_join();
    {
        let mut e = CAP_ERR.lock().unwrap_or_else(|x| x.into_inner());
        e.clear();
    }

    let p = ensure_server().ok_or_else(|| "本地取屏服务启动失败".to_string())?;
    let (sw, sh) = primary_screen_size();
    if sw == 0 || sh == 0 {
        return Err("无法获取主显示器尺寸".into());
    }

    // 等比缩放到 max_width 以内；宽高取偶数，编码器/NV12 都要求
    let (ow, oh) = if opt.max_width > 0 && sw > opt.max_width {
        let k = opt.max_width as f64 / sw as f64;
        (opt.max_width, ((sh as f64 * k).round() as u32).max(2))
    } else {
        (sw, sh)
    };
    let ow = (ow & !1).max(2);
    let oh = (oh & !1).max(2);
    let fps = opt.fps.clamp(1, 60);
    let quality = opt.quality.clamp(20, 95);

    // None = 走 JPEG（默认，兼容老前端）；"jpegn" 也是 None，但额外要求分条并行
    let want = OutFormat::parse(&opt.mode);
    let want_multi = opt.mode.eq_ignore_ascii_case("jpegn");

    // H.264 目标码率。0 = 默认 4000kbps（见 StartOptions::bitrate_kbps 注释）。
    // 下限 500kbps 防止前端传个 0/个位数把画质压成马赛克；上限 40Mbps 防止误传天文数字。
    let bitrate_bps: u32 = if opt.bitrate_kbps == 0 {
        4_000_000
    } else {
        (opt.bitrate_kbps as u64 * 1000).clamp(500_000, 40_000_000) as u32
    };

    // [20261021 七轮] H.264 编码后端：默认 NVENC 硬编。
    //
    // 本轮一度以为 NVENC 不可用、改走 CPU 软编；**实测推翻了这个判断**：
    // 真正病灶是本进程被计划任务带成 BelowNormal，抓屏线程被普通优先级的程序
    // 反复饿死（证据：`slow soft-encode 4181ms`，CPU 100% 频率、系统负载 22% ——
    // 在等调度，不是算不动）。把进程提到 ABOVE_NORMAL + 抓屏线程 HIGHEST 之后，
    // 同一台机器 NVENC 实测 **24fps 稳、12.6ms/帧、零慢帧**（1280x800 4000kbps）。
    // 软编保留为显式选项（enc="soft"）与无 NVIDIA 卡机器的自动兜底。
    let soft = opt.enc.eq_ignore_ascii_case("soft");

    // [20261021 七轮] 软编分辨率收口：1280x800 实测 43ms/帧（≈21fps），
    // 1152x720 是 36ms（≈25fps），且与 16:10 主屏（1680x1050）同比例。
    // 用 10% 的线性分辨率换 20% 帧率 —— 软编默认按 1152 宽收口；硬编不参与。
    let (ow, oh) = if soft && ow > 1152 {
        let k = 1152.0 / ow as f64;
        let nh = (((oh as f64 * k).round() as u32) & !1).max(2);
        capture_diag(&format!("软编分辨率收口 {ow}x{oh} → 1152x{nh}"));
        (1152u32, nh)
    } else {
        (ow, oh)
    };
    if want == Some(OutFormat::H264) {
        capture_diag(&format!(
            "H.264 编码后端 = {}（soft=CPU/OpenH264，nvenc=硬编）",
            if soft { "soft" } else { "nvenc" }
        ));
    }

    OUT_SIZE.store(((ow as u32) << 16) | (oh as u32 & 0xFFFF), Ordering::Relaxed);    // 1.4.1：当场定下 JPEG 家族的真实格式（单张 / 分条），`captureStart` 才能直接回报，
    // 前端一次就拿到，不必轮询 meta 等最多 2 秒才有画面。
    preset_mode(want, want_multi, oh);
    let session = SESSION.fetch_add(1, Ordering::Relaxed) + 1;
    RUNNING.store(true, Ordering::Relaxed);
    // [20260919 h264gate] 记录本会话的格式意图（配合 start() 入口的独占闸门）：
    // h264 会话存活期间（SESSION 未变），一切非 h264 的 start 都被拒；
    // stop() / 新 start 会推进 SESSION，闸门自动放行。
    if opt.mode == "h264" {
        WANT_MODE.store(MODE_H264, Ordering::Relaxed);
        WANT_SESSION.store(session, Ordering::Relaxed);
    } else {
        WANT_MODE.store(MODE_STOPPED, Ordering::Relaxed);
    }

    // 只起抓屏线程。编码线程由抓屏线程在「决定走 JPEG」之后自己起 ——
    // 因为"能否直送"取决于 GPU 缩放链能否建成，而那必须在抓屏线程上、
    // 建桌面复制对象时才知道（DXGI 复制对象有线程上下文）。
    let handle = std::thread::Builder::new()
        .name("indesk-capture".into())
        .spawn(move || {
            capture_loop(
                session, sw, sh, ow, oh, fps, quality, want, want_multi, bitrate_bps, soft,
            )
        })
        .map_err(|e| format!("取屏线程启动失败: {e}"))?;
    // [20260918 capfix] 存句柄，供下一次 start 同步等待（见 CAP_THREAD 注释）。
    *CAP_THREAD.lock().unwrap_or_else(|e| e.into_inner()) = Some(handle);

    Ok((p, ow, oh))
}

/// 停止静默取屏（取消无人值守时调用）。**异步**：只发退出信号，不等线程结束。
pub fn stop() {
    RUNNING.store(false, Ordering::Relaxed);
    SESSION.fetch_add(1, Ordering::Relaxed);
    WANT_KEYFRAME.store(false, Ordering::Relaxed);
    // [20260918b capdead] 写 MODE_STOPPED 而**不是** MODE_STARTING：
    // 停掉之后外部必须能区分「停了」与「正在建链」，否则死画面的根因无法收敛
    // （详见 MODE_STOPPED 的注释）。
    OUT_MODE.store(MODE_STOPPED, Ordering::Relaxed);
    OUT_STRIPS.store(1, Ordering::Relaxed);
    OUT_ROWS_PER.store(0, Ordering::Relaxed);
    // 唤醒所有等待新帧的连接线程，让它们干净退出
    slot().cv.notify_all();
    raw_slot().cv.notify_all();
}

/// **[20260918 capfix]** 停止并**同步等待**抓屏线程退出（其内部 capturer 已 drop）。
///
/// 必须给足等待时间：旧线程可能正阻塞在 `AcquireNextFrame`（等待上限
/// `ACQUIRE_TIMEOUT_MS`）或正在 `build_chain`，这些都无法被信号打断。
/// `capture.rs::start()` 依赖本函数建立的「前一个复制对象已释放」保证。
pub fn stop_and_join() {
    stop();
    let handle = CAP_THREAD
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .take();
    if let Some(h) = handle {
        // 最多等 5 秒：正常情况几十毫秒即可（线程在 ≤100ms 的取帧等待里醒来）。
        // 超时不让 start 失败 —— 交给 DXGI 建链重试兜底，并留下诊断。
        let t0 = std::time::Instant::now();
        let mut waited = false;
        while !h.is_finished() {
            if t0.elapsed() >= std::time::Duration::from_secs(5) {
                capture_diag("stop_and_join：等待旧抓屏线程退出超时（5s），继续启动新会话");
                // [20261020] 旧线程还攥着桌面复制对象（独占），新线程的
                // DuplicateOutput 必然失败 —— 把建链重试窗口从 1.5s 放宽到
                // 12s（实测旧线程在桌面恢复活动后会退出并释放复制对象），
                // 否则 stop→start 换届后的整个会话都被打回 JPEG/零帧。
                CAP_JOIN_TIMED_OUT.store(true, Ordering::Relaxed);
                // [20261020] 终极兜底：20s 后旧线程仍不退出 = 抓屏已不可恢复
                //（NVENC/驱动卡死，复制对象永久被占）。核此时已无存在价值，
                // 自毁退出，由壳（HostCore 崩溃退避重启）拉起新进程 —— 干净的
                // GPU 状态比带病坚持有用得多。壳的 sender 重试（2.5s×2）+
                // 主控 revoke 重试（8s）正好覆盖核重建窗口。
                spawn_wedge_escalation(h);
                // 这里不 join 直接返回，靠 start 里的建链重试兜底。
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
            waited = true;
        }
        if waited {
            capture_diag(&format!(
                "stop_and_join：旧抓屏线程已退出，等待 {}ms",
                t0.elapsed().as_millis()
            ));
        }
        let _ = h.join();
    }
}

/// [20261020] 上一轮 stop_and_join 是否超时（旧线程仍持有复制对象）。
/// 抓屏线程据此把建链重试窗口从 1.5s 放宽到 12s。
static CAP_JOIN_TIMED_OUT: AtomicBool = AtomicBool::new(false);

/// [20261020] 抓屏心跳（ms 时间戳）：抓屏循环每次 grab() 返回（含 NoChange）
/// 都刷新。常驻看护线程据此发现「会话中抓屏卡死」—— 20261020 上午用户实测
/// 「连接上画面冻死」的直接根因：抓屏线程卡在一次 grab() 里，帧停了但
/// Dart 侧时钟探针还在跳，主控看门狗永不触发，画面冻结到会话结束。
static LAST_GRAB_TS: AtomicU64 = AtomicU64::new(0);

fn now_ms_u64() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// 抓屏心跳看护（每次 start 起一个，随 stop 退出）：
/// RUNNING 期间 >8s 心跳未刷新 = 抓屏线程卡死 → 核自毁，由壳重建。
/// 会话中卡死此前无任何自愈路径（wedge-watch 只在换届 stop_and_join 时布防）。
fn spawn_capture_heartbeat_watch() {
    let _ = std::thread::Builder::new()
        .name("indesk-cap-hb".into())
        .spawn(|| loop {
            std::thread::sleep(std::time::Duration::from_secs(1));
            if !RUNNING.load(Ordering::Relaxed) {
                return;
            }
            let last = LAST_GRAB_TS.load(Ordering::Relaxed);
            // [20261020] 阈值 15s：实测本卡（GTX 750/536.23）桌面静止时 grab
            // 偶发阻塞 ~8.5s 后自行返回 —— 8s 阈值会在它返回前就把进程杀了，
            // 慢帧诊断永远落不了盘。15s > 阻塞上限，先取证再判死。
            if last > 0 && now_ms_u64().saturating_sub(last) > 15_000 {
                // [20261020] 杀核前落盘堵点：GRAB_STAGE 由抓屏线程实时维护
                // （0=grab外 1=acquire 2=process），慢帧日志只在 grab 返回后打、
                // 卡死的那次永远打不出来 —— 这个值是卡死现场的唯一证据。
                let stage = crate::dxgi::GRAB_STAGE.load(Ordering::Relaxed);
                capture_diag(&format!(
                    "抓屏心跳丢失 {}ms（线程卡死在 grab 内 stage={} {}）→ 核自毁重建",
                    now_ms_u64().saturating_sub(last),
                    stage,
                    crate::dxgi::grab_stage_name(stage),
                ));
                std::process::exit(3);
            }
        });
}

/// [20261020] 卡死线程升级看护：20s 内旧线程仍不退出 → 进程自毁（exit 3）。
/// 壳检测到核退出会按退避策略重建（HostCore），抓屏状态从零开始。
fn spawn_wedge_escalation(handle: std::thread::JoinHandle<()>) {
    let _ = std::thread::Builder::new()
        .name("indesk-wedge-watch".into())
        .spawn(move || {
            // [20261020] 10s（原 20s）：断线重连的黑屏窗口 = join 5s + 这里
            // 10s + 核重建 2s ≈ 17s 最坏；再短会误杀"桌面恢复活动后自行
            // 退出"的线程（实测有此路径）。
            for _ in 0..10 {
                if handle.is_finished() {
                    capture_diag("wedge-watch：旧抓屏线程最终退出，无需自毁");
                    return;
                }
                std::thread::sleep(std::time::Duration::from_secs(1));
            }
            capture_diag(&format!(
                "wedge-watch：旧抓屏线程 10s 仍未退出，核自毁重建（exit 3，stage={} {}）",
                crate::dxgi::GRAB_STAGE.load(Ordering::Relaxed),
                crate::dxgi::grab_stage_name(crate::dxgi::GRAB_STAGE.load(Ordering::Relaxed)),
            ));
            std::process::exit(3);
        });
}

// ------------------------------------------------------------------ 只读取帧服务

fn ensure_server() -> Option<u16> {
    let p = PORT.load(Ordering::Relaxed);
    if p != 0 {
        return Some(p as u16);
    }
    let listener = TcpListener::bind("127.0.0.1:0").ok()?;
    let port = listener.local_addr().ok()?.port();
    PORT.store(port as u32, Ordering::Relaxed);
    std::thread::Builder::new()
        .name("indesk-framesrv".into())
        .spawn(move || {
            for stream in listener.incoming() {
                match stream {
                    Ok(s) => {
                        let _ = std::thread::Builder::new()
                            .name("indesk-framesrv-conn".into())
                            .spawn(move || serve_client(s));
                    }
                    Err(_) => break,
                }
            }
        })
        .ok()?;
    Some(port)
}

fn serve_client(mut s: TcpStream) {
    let _ = s.set_nodelay(true);
    let _ = s.set_read_timeout(Some(Duration::from_secs(5)));

    let mut buf = [0u8; 2048];
    let n = match s.read(&mut buf) {
        Ok(n) => n,
        Err(_) => return,
    };
    if n == 0 {
        return;
    }
    let req = String::from_utf8_lossy(&buf[..n]);
    let path = req.split_whitespace().nth(1).unwrap_or("/");
    if !path.starts_with("/frames") && !path.starts_with("/mjpeg") {
        let _ = s.write_all(
            b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nAccess-Control-Allow-Origin: *\r\nConnection: close\r\n\r\n",
        );
        return;
    }

    let (strips, rows_per) = strip_layout();
    let head = format!(
        concat!(
            "HTTP/1.1 200 OK\r\n",
            "Content-Type: multipart/x-mixed-replace; boundary=--indeskframe\r\n",
            "X-Indesk-Fmt: {}\r\n",
            "X-Indesk-Strips: {}\r\n",
            "X-Indesk-RowsPer: {}\r\n",
            "Cache-Control: no-store, no-cache, must-revalidate\r\n",
            "Pragma: no-cache\r\n",
            "Access-Control-Allow-Origin: *\r\n",
            "Connection: close\r\n\r\n"
        ),
        mode(),
        strips,
        rows_per
    );
    if s.write_all(head.as_bytes()).is_err() {
        return;
    }
    let _ = s.set_write_timeout(Some(Duration::from_secs(5)));

    let slot = slot();
    let mut last = 0u64;
    while RUNNING.load(Ordering::Relaxed) {
        // 等一帧新的（同一帧只发一次）
        let frame = {
            let mut g = match slot.inner.lock() {
                Ok(g) => g,
                Err(_) => return,
            };
            while g.seq == last && RUNNING.load(Ordering::Relaxed) {
                let (ng, to) = match slot.cv.wait_timeout(g, Duration::from_millis(200)) {
                    Ok(v) => v,
                    Err(_) => return,
                };
                g = ng;
                if to.timed_out() {
                    break;
                }
            }
            last = g.seq;
            g.frame.clone().map(|f| (f, g.lens.clone()))
        };
        let Some((data, lens)) = frame else { continue };

        // 每帧带上格式，前端据此决定是否重建解码器（格式理论上不会中途变，
        // 但 GDI 回退等边界情况下确实可能变）。
        // 1.4.1 起再带 `X-Indesk-Offsets`（每条 JPEG 起始偏移）；单张 / 直送时为空值，
        // 前端解析不到数字即自动走老路径（扫 FFD8/FFD9）。
        let header = format!(
            "--indeskframe\r\nContent-Type: {}\r\nX-Indesk-Fmt: {}\r\nX-Indesk-Strips: {}\r\nX-Indesk-RowsPer: {}\r\nX-Indesk-Offsets: {}\r\nContent-Length: {}\r\n\r\n",
            content_type(),
            mode(),
            strip_layout().0,
            strip_layout().1,
            offsets_csv(&lens),
            data.len()
        );
        if s.write_all(header.as_bytes()).is_err()
            || s.write_all(&data).is_err()
            || s.write_all(b"\r\n").is_err()
            || s.flush().is_err()
        {
            break;
        }
    }
}

// ------------------------------------------------------------------ 抓屏 / 编码

fn primary_screen_size() -> (u32, u32) {
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        GetSystemMetrics, SM_CXSCREEN, SM_CYSCREEN,
    };
    unsafe {
        let w = GetSystemMetrics(SM_CXSCREEN);
        let h = GetSystemMetrics(SM_CYSCREEN);
        (w.max(0) as u32, h.max(0) as u32)
    }
}

/// 直接编码 BGRA（jpeg-encoder 0.6 原生支持 ColorType::Bgra，无需先换 RGBA，
/// 省掉每帧 8MB 的逐字节 swap 循环 —— 20260913g）
fn encode_jpeg(bgra: &[u8], w: u32, h: u32, quality: u8) -> Result<Vec<u8>, String> {
    let mut out = Vec::with_capacity((w as usize * h as usize / 6) + 4096);
    let enc = jpeg_encoder::Encoder::new(&mut out, quality);
    enc.encode(bgra, w as u16, h as u16, jpeg_encoder::ColorType::Bgra)
        .map_err(|e| e.to_string())?;
    Ok(out)
}

/// 「分条并行 JPEG」并行度（1.4.0 换引擎第二枪）。
///
/// 动因是实测：JPEG 编码占了整帧 90% 的时间（原生 1680x1050 = 39.1ms，
/// 720P 1280x800 = 25.0ms），而 `simd` 只榨出 15~20% —— 因为 AVX2 只加速
/// FDCT 与 BGRA→YCbCr，**霍夫曼编码与位流打包本质串行**，再多 SIMD 也吃不到。
/// JPEG 又天然可切：MCU 不存在跨行依赖，把图按行切成 N 条、各编一份独立 JPEG，
/// 与整图编码的像素结果逐块一致（切点按 MCU 高度 16 行对齐）。
///
/// 所以这里按核数分条并行编码，前端收 N 份再逐条画回 canvas。
///
/// 并行度取 `核数/2`（上限 4）：必须**留一半核**给 WebRTC 编码器与前端绘制，
/// 否则四核机上四线程 JPEG 会和编码器抢核，端到端帧率反而掉。
fn choose_strips(h: u32) -> u32 {
    let cores = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(4);
    let want = ((cores / 2).clamp(1, 4)) as u32;
    // 每条至少 32 行（2 个 MCU 高），否则拆了也没收益
    let max_by_rows = (h / 32).max(1);
    want.min(max_by_rows).max(1)
}

/// 每条的行数：`align16(ceil(h/n))`。**壳与前端必须用同一公式**，
/// 所以壳把结果通过 `X-Indesk-RowsPer` 直接告诉前端，前端不要自己算。
fn rows_per_strip(h: u32, n: u32) -> u32 {
    if n <= 1 {
        return h;
    }
    let per = (h + n - 1) / n;
    ((per + 15) / 16) * 16
}

/// 分条并行编码：把 BGRA 按行切 N 条，N 个线程各编一份独立 JPEG，首尾相接返回。
/// 前端按 `X-Indesk-Strips` / `X-Indesk-RowsPer` 逐条解码 + 逐条画到对应纵坐标。
///
/// 1.4.1 起**同时返回每条的起始偏移**（`Vec<u32>`，首项恒为 0），随帧一起下行到
/// `X-Indesk-Offsets`。前端据此直接切片，不必再去字节流里扫 `FFD8`/`FFD9` ——
/// 那个扫法不只是慢，还有正确性隐患：JPEG 熵编码段里出现 `FF D8` / `FF D9`
/// 字节序列并不被排除，扫到错的位置就会把一条画歪（表现为画面撕裂）。
///
/// ⚠️ 切点**必须**是 16 行（MCU 高度）的整数倍，否则边界 MCU 会被两条各编一次、
/// 出现一条 1 像素的接缝。
fn encode_jpeg_strips(
    bgra: &[u8],
    w: u32,
    h: u32,
    quality: u8,
    n: u32,
) -> Result<(Vec<u8>, Vec<u32>), String> {
    if n <= 1 {
        // 单张：无分条，偏移留空（前端走单张 JPEG 路径）
        return Ok((encode_jpeg(bgra, w, h, quality)?, Vec::new()));
    }
    let rows = rows_per_strip(h, n) as usize;
    let stride = w as usize * 4;
    let parts: Result<Vec<Vec<u8>>, String> = std::thread::scope(|s| {
        let mut handles = Vec::new();
        let mut y = 0usize;
        while y < h as usize {
            let sh = rows.min(h as usize - y);
            let a = y * stride;
            let b = a + sh * stride;
            if b > bgra.len() {
                return Err("分条越界（输入帧长度不足）".to_string());
            }
            let slice: &[u8] = &bgra[a..b];
            handles.push(s.spawn(move || encode_jpeg(slice, w, sh as u32, quality)));
            y += sh;
        }
        let mut oks = Vec::with_capacity(handles.len());
        for hd in handles {
            match hd.join() {
                Ok(r) => oks.push(r?),
                Err(_) => return Err("分条编码线程 panic".to_string()),
            }
        }
        Ok(oks)
    });
    let parts = parts?;
    let total: usize = parts.iter().map(|p| p.len()).sum();
    let mut out = Vec::with_capacity(total);
    let mut lens: Vec<u32> = Vec::with_capacity(parts.len());
    let mut acc = 0usize;
    for p in parts {
        lens.push(acc as u32);
        out.extend_from_slice(&p);
        acc += p.len();
    }
    debug_assert_eq!(acc, total);
    Ok((out, lens))
}

/// 把每条起始偏移渲染成响应头值（形如 `0,15234,30456`）。空 = 单张 / 直送，不带该头。
pub fn offsets_csv(lens: &[u32]) -> String {
    lens.iter()
        .map(|v| v.to_string())
        .collect::<Vec<_>>()
        .join(",")
}

/// 发布「最终帧」（JPEG 模式放编码结果；直送模式直接放原始像素）。
/// `lens` = 分条 JPEG 每条的起始偏移（单张传空）。
///
/// **[1.4.38 privacy v2]** 发布层**不再参与隐私屏**——旧版在这里把整帧换成
/// 纯黑 JPEG，语义颠倒（主控端黑屏、被控端毫无变化）。现在隐私屏由
/// `privacy_overlay` 的原生遮罩窗负责：本机黑屏、抓屏照常拿到真实桌面，
/// 本函数与真实画面流零耦合。
fn publish(jpg: Vec<u8>, lens: Vec<u32>) {
    let s = slot();
    if let Ok(mut g) = s.inner.lock() {
        g.frame = Some(jpg);
        g.lens = lens;
        g.seq = g.seq.wrapping_add(1);
        drop(g);
        s.cv.notify_all();
    }
}

/// 直送模式发布（裸 BGRA/NV12）。
/// **[1.4.38 privacy v2]** 同 `publish`：不再做任何黑化替换。
fn publish_raw(bgra: Vec<u8>) {
    let s = raw_slot();
    if let Ok(mut g) = s.inner.lock() {
        g.frame = Some(bgra);
        g.lens = Vec::new();
        g.seq = g.seq.wrapping_add(1);
        drop(g);
        s.cv.notify_all();
    }
}

/// 编码线程主循环（20260913g 流水线）：等原始帧 → 只编最新一帧 → 发布 JPEG。
/// 每 5 秒输出一次编码耗时/实际帧率诊断，性能可直接自证。
/// **直送模式不启动本线程**（1.4.0：壳侧不再压缩，最终压缩交给 WebRTC 硬件编码器）。
/// 1.4.0 起支持**分条并行**：`nstrips > 1` 时按行切条、多线程各编一份独立 JPEG。
fn encoder_loop(session: u64, ow: u32, oh: u32, quality: u8, fps: u32, nstrips: u32) {
    let s = raw_slot();
    let mut last_raw = 0u64;
    let mut enc_count: u64 = 0;
    let mut enc_ms_total: u128 = 0;
    let mut last_report = Instant::now();
    let mut skip_logged = false;
    // [20260919 adq] 运动自适应质量。
    //
    // 动因：回到原生 1050p 抓屏后（20260919 clarity-native），拖动窗口这类
    // 全屏变化的场景里，JPEG 编码 + 前端解码/绘制 + WebRTC 编码整条管线
    // 容易过载 —— 表现为拖拽顿挫（用户实测反馈）。
    //
    // 信号：`jumped`（编码跟不上抓屏、发布槽里已有更新的帧）就是"过载"的
    // 直接证据。跳帧时把质量临时降 20（运动中的画面本身会掩掉质量损失，
    // 字节数变小 → 解码/绘制/WebRTC 编码全线减压）；连续 30 帧没再跳帧
    // 就每帧 +2 缓慢回升，最高不越过用户设定的静态质量。静止画面
    // jumped 不触发，质量恒定 —— 清晰度不受影响。
    let mut dyn_q: i32 = quality as i32;
    let mut recover_countdown: u32 = 0;

    while RUNNING.load(Ordering::Relaxed) && SESSION.load(Ordering::Relaxed) == session {
        let frame = {
            let mut g = match s.inner.lock() {
                Ok(g) => g,
                Err(_) => return,
            };
            while g.seq == last_raw && RUNNING.load(Ordering::Relaxed) && SESSION.load(Ordering::Relaxed) == session {
                let (ng, to) = match s.cv.wait_timeout(g, Duration::from_millis(200)) {
                    Ok(v) => v,
                    Err(_) => return,
                };
                g = ng;
                if to.timed_out() {
                    break;
                }
            }
            let jumped = g.seq != last_raw + 1 && g.seq != last_raw;
            last_raw = g.seq;
            (g.frame.clone(), jumped)
        };
        let (Some(bgra), jumped) = frame else { continue };
        if SESSION.load(Ordering::Relaxed) != session {
            break;
        }
        if jumped && !skip_logged {
            skip_logged = true;
            eprintln!("[InDesk] 编码流水线：编码跟不上抓屏，自动跳过积压帧（正常保护，不丢最新画面）");
        }
        // [20260919 adq] 跳帧 = 过载信号 → 降质减压；稳住后缓慢回升。
        if jumped {
            dyn_q = (dyn_q - 20).max(55);
            recover_countdown = 30;
        } else if recover_countdown > 0 {
            recover_countdown -= 1;
        } else if dyn_q < quality as i32 {
            dyn_q = (dyn_q + 2).min(quality as i32);
        }

        let t0 = Instant::now();
        // [20260918e idleslow] 空闲时质量压到 50（节流在抓屏循环里做，编码线程
        // 靠条件变量跟随抓屏节奏，2fps 自然只有 2 次编码）。
        // [20260919 adq] 静态质量 = 用户设定；运动中 = 自适应 dyn_q。
        let mut q_now = dyn_q;
        if IDLE.load(Ordering::Relaxed) {
            q_now = q_now.min(50);
        }
        match encode_jpeg_strips(&bgra, ow, oh, q_now as u8, nstrips) {
            Ok((jpg, lens)) => publish(jpg, lens),
            Err(e) => {
                static ENC_ERR_ONCE: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
                if !ENC_ERR_ONCE.swap(true, Ordering::Relaxed) {
                    eprintln!("[InDesk] 取屏编码失败：{e}");
                }
            }
        }
        enc_count += 1;
        enc_ms_total += t0.elapsed().as_millis();

        if last_report.elapsed() >= Duration::from_secs(5) {
            let avg = if enc_count > 0 { enc_ms_total / enc_count as u128 } else { 0 };
            eprintln!(
                "[InDesk] 编码诊断：5s 编 {enc_count} 帧（实测 {}fps），平均 {avg}ms/帧（{}x{} q{quality}/dyn{dyn_q}，分条 {nstrips}，目标 {fps}fps）",
                enc_count / 5, ow, oh
            );
            enc_count = 0;
            enc_ms_total = 0;
            last_report = Instant::now();
        }
    }
}

/// 抓屏入口。决策顺序：
///
/// 1. 建 DXGI 捕获器（GPU 缩放链在 `new()` 里就绪，所以这里就能定下发布格式）；
/// 2. 请求了像素格式（bgra/nv12）且条件满足 → **直送模式**：壳侧不编码，前端零解码；
///    否则 → **JPEG 模式**：起编码线程，行为与 1.4.0 之前一致（老前端不受影响）；
/// 3. DXGI 不可用 / 中途失效 → 回退 GDI（BitBlt/StretchBlt 本就产出目标尺寸 BGRA，
///    两种发布格式都能用）。
/// [20261010 自愈根治] 会话内无限自愈外层。
///
/// 背景（批次 015 前「永久断开、重启壳才好」的定罪链条）：
/// ① DXGI 抓帧连续失败约 10s（锁屏/安全桌面/显示模式切换即可触发）→ 回退 GDI；
/// ② GDI 入口 GetDC/CreateDIBSection 在恶劣桌面态失败 → **线程静默 return**，
///    无重试、无标记，而 RUNNING 仍为 true（僵尸态）；
/// ③ 若上一会话是 h264 直送且异常结束（未走 captureStop），h264gate 在僵尸态下
///    把 JPEG captureStart 永久拒绝 → 前端重拉数千次全败。
///
/// 修复：把单轮逻辑抽到 `capture_loop_once`，本函数在外层循环——
/// 单轮任何原因退出，只要会话还活着（RUNNING && SESSION 未变），就
/// capture_diag 落痕 + 退避（0.5s→1s→…封顶 5s）后重建再战。
/// 锁屏/切换桌面期间可能暂时无帧，但恢复后 ≤5s 自动出帧；引擎线程不再可能
/// 进入「死了但 RUNNING=true」的僵尸态。
fn capture_loop(
    session: u64,
    sw: u32,
    sh: u32,
    ow: u32,
    oh: u32,
    fps: u32,
    quality: u8,
    want: Option<OutFormat>,
    want_multi: bool,
    bitrate_bps: u32,
    soft: bool,
) {
    // [20261021 七轮] 抓屏/编码线程高于普通程序：本进程曾被计划任务带成 BelowNormal，
    // 实测出现 `slow soft-encode 4181ms`（纯 CPU 编码在等调度，不是算力不够）。
    #[cfg(windows)]
    {
        use windows::Win32::System::Threading::{
            GetCurrentThread, SetThreadPriority, THREAD_PRIORITY_HIGHEST,
        };
        unsafe {
            let _ = SetThreadPriority(GetCurrentThread(), THREAD_PRIORITY_HIGHEST);
        }
    }
    let mut backoff_ms: u64 = 0;
    // [20261020] 常驻抓屏心跳看护：会话中抓屏线程卡死（一次 grab 永不返回）
    // 时自毁重建 —— 此前只有换届 stop_and_join 路径有 wedge 兜底，会话中
    // 卡死 = 画面冻死到会话结束（20261020 上午用户实测）。
    LAST_GRAB_TS.store(now_ms_u64(), Ordering::Relaxed);
    spawn_capture_heartbeat_watch();
    let mut tries: u32 = 0;
    loop {
        capture_loop_once(session, sw, sh, ow, oh, fps, quality, want, want_multi, bitrate_bps, soft);
        // 会话结束（stop / 新 start 推进 SESSION）→ 正常退出，不重试
        if !RUNNING.load(Ordering::Relaxed) || SESSION.load(Ordering::Relaxed) != session {
            return;
        }
        tries += 1;
        capture_diag(&format!(
            "[heal] 抓屏引擎第 {tries} 次重建（上一轮意外退出而会话仍在）：退避后重试"
        ));
        std::thread::sleep(Duration::from_millis(backoff_ms.max(500)));
        backoff_ms = (backoff_ms + 1000).min(5000);
    }
}

#[allow(clippy::too_many_arguments)]
fn capture_loop_once(
    session: u64,
    sw: u32,
    sh: u32,
    ow: u32,
    oh: u32,
    fps: u32,
    quality: u8,
    want: Option<OutFormat>,
    want_multi: bool,
    bitrate_bps: u32,
    soft: bool,
) {
    // ① 建捕获器。无论要不要直送，都让 DXGI 侧产出**目标尺寸 BGRA**
    //    —— 这本身就替代了 1.4.0 之前那次 28.3ms/帧的 CPU 盒式下采样。
    //
    // [20260918 capfix] **建链重试**：`DuplicateOutput` 是独占资源，若上一个会话的
    // capturer 恰好还在释放途中（stop_and_join 超时兜底路径），这里会拿到
    // `0x80070057 参数错误`。重试 1.5s 即可覆盖该窗口 —— 比"直接回退 GDI/永不写
    // OUT_MODE 导致前端零帧"好得多（实测：旧 capturer drop 后 72ms 就能建起来）。
    let want_fmt = want.unwrap_or(OutFormat::Bgra);
    // [20261020] 抓屏线程里程碑：建捕获器是 new() 里唯一可能长时间阻塞的
    // 阶段（D3D 设备/桌面复制/NVENC 链），耗时落 diag 才能区分
    // 「线程没起来」和「起来了但 OUT_MODE 没定」。
    capture_diag(&format!(
        "capture thread: creating capturer fmt={} bitrate={}kbps",
        want_fmt.name(),
        bitrate_bps / 1000
    ));
    let t_cap = Instant::now();
    // [20261020] 旧线程 join 超时 = 桌面复制对象还被它攥着（独占），重试
    // 窗口放宽到 12s（实测旧线程会在桌面恢复活动后退出并释放）；干净换届
    // 时维持 1.5s 不变。
    let retry_ms: u64 = if CAP_JOIN_TIMED_OUT.load(Ordering::Relaxed) {
        capture_diag("capture thread: 上届 join 超时，建链重试窗口放宽到 12s");
        12_000
    } else {
        1_500
    };
    CAP_JOIN_TIMED_OUT.store(false, Ordering::Relaxed);
    let deadline = Instant::now() + Duration::from_millis(retry_ms);
    let mut cap: Option<crate::dxgi::DxgiCapturer> = loop {
        match crate::dxgi::DxgiCapturer::new(ow, oh, want_fmt, fps, bitrate_bps, soft) {
            Ok(c) => break Some(c),
            Err(e) => {
                if Instant::now() >= deadline {
                    eprintln!("[InDesk] 抓屏路径：GDI（DXGI 不可用：{e}）");
                    // [20260918 capfix] 失败原因必须落盘：壳由 explorer 拉起时 stderr 丢失，
                    // 原实现就是因此变成"零线索"。
                    set_cap_err(&format!(
                        "DXGI 不可用（重试 1.5s 仍失败），已回退 GDI：{e}"
                    ));
                    break None;
                }
                std::thread::sleep(Duration::from_millis(120));
            }
        }
    };

    // ② 定发布格式
    let gpu = cap.as_ref().map(|c| c.gpu_scaling()).unwrap_or(false);
    let (direct, pub_mode, nstrips) = match want {
        // BGRA 直送：GPU 链没有也行（回退路径 / GDI 都产出目标尺寸 BGRA）
        Some(OutFormat::Bgra) if cap.is_some() => (true, MODE_BGRA, 1),
        // NV12 必须要 GPU 链，否则没有 CPU 转换实现
        Some(OutFormat::Nv12) if gpu => (true, MODE_NV12, 1),
        // H.264 更要 GPU 链：编码器是把 VideoProcessor 直出的 NV12 纹理零拷贝注册进
        // NVENC 的。编码器建不出来时 `build_chain` 会直接失败 → gpu=false →
        // 落到下面的 JPEG 兜底（本机无 NVIDIA 卡时就是这条）。
        Some(OutFormat::H264) if gpu => (true, MODE_H264, 1),
        // 分条并行 JPEG（前端点名要 "jpegn" 才走这里）
        _ if want_multi => {
            let n = choose_strips(oh);
            if n > 1 {
                (false, MODE_JPEGN, n)
            } else {
                (false, MODE_JPEG, 1)
            }
        }
        _ => (false, MODE_JPEG, 1),
    };
    OUT_MODE.store(pub_mode, Ordering::Relaxed);
    capture_diag(&format!(
        "capture thread: capturer ready {}ms direct={} pub_mode={}",
        t_cap.elapsed().as_millis(),
        direct,
        pub_mode
    ));
    OUT_STRIPS.store(nstrips, Ordering::Relaxed);
    OUT_ROWS_PER.store(if nstrips > 1 { rows_per_strip(oh, nstrips) } else { 0 }, Ordering::Relaxed);

    if let Some(c) = cap.as_ref() {
        let (nw, nh) = c.source_size();
        eprintln!(
            "[InDesk] 抓屏路径：DXGI Desktop Duplication（桌面 {nw}x{nh} → 输出 {ow}x{oh}，GPU 缩放={gpu}，发布格式={}，{}）",
            c.out_format().name(),
            match want {
                Some(OutFormat::H264) if direct => {
                    format!("壳侧 NVENC 硬编 H.264（目标 {}kbps）", bitrate_bps / 1000)
                }
                _ if direct => "壳侧不编码（直送）".to_string(),
                _ => format!("壳侧 JPEG 编码（分条 {nstrips}）"),
            }
        );
        if let Some(err) = c.chain_error() {
            eprintln!("[InDesk] GPU 缩放链未就绪：{err}");
        }
    }
    if want.is_some() && !direct {
        eprintln!(
            "[InDesk] 请求的直送格式（{}）不可用，已退回 JPEG 模式",
            want.unwrap().name()
        );
    }

    // ③ JPEG 模式才起编码线程
    if !direct {
        let _ = std::thread::Builder::new()
            .name("indesk-encode".into())
            .spawn(move || encoder_loop(session, ow, oh, quality, fps, nstrips));
    }

    if let Some(c) = cap.take() {
        match capture_loop_dxgi(session, c, ow, oh, fps, direct) {
            Ok(()) => return,
            Err(e) => {                // 会话已停 → 正常退出，不算失败
                if !RUNNING.load(Ordering::Relaxed) || SESSION.load(Ordering::Relaxed) != session {
                    return;
                }
                eprintln!("[InDesk] DXGI 抓屏中断（{e}），回退 GDI 抓屏");
            }
        }
    }
    capture_loop_gdi(session, sw, sh, ow, oh, fps, direct);
}

/// DXGI 抓屏主循环。抓屏线程只做「取帧 + GPU 缩放 + 拷贝」；
/// JPEG 模式下编码在独立线程（20260913g 流水线），直送模式下没有任何编码。
fn capture_loop_dxgi(
    session: u64,
    mut cap: crate::dxgi::DxgiCapturer,
    ow: u32,
    oh: u32,
    fps: u32,
    direct: bool,
) -> Result<(), String> {
    use crate::dxgi::Grab;

    // [20260918e idleslow] 节流间隔不再预计算 —— 空闲降速要求每帧重读开关，
    // 见两处抓屏循环里的 effective_fps()。
    let mut next = Instant::now();
    let mut buf: Vec<u8> = Vec::new();
    let mut first_ok = false;
    let mut err_logged = false;
    let mut consec_err = 0u32;
    let mut frames = 0u64;
    let mut us_total: u128 = 0;
    let mut nochange = 0u64;
    let mut last_report = Instant::now();
    let fmt = cap.out_format();
    let expect = fmt.frame_len(ow, oh);
    // ⚠️ H.264 是**变长**码流：`frame_len()` 恒为 0，绝不能用等值判断，
    // 否则一帧都发不出去（这条坑不写下来下次一定踩）。
    let is_h264 = fmt == OutFormat::H264;

    while RUNNING.load(Ordering::Relaxed) && SESSION.load(Ordering::Relaxed) == session {
        // 关键帧请求：新客户端接入/丢包恢复时立刻出一帧 IDR，避免对端黑屏等 2 秒。
        // swap 消费 —— 不消费会每帧都强制 IDR，码率直接爆。
        if WANT_KEYFRAME.swap(false, Ordering::Relaxed) {
            cap.request_keyframe();
        }

        let t0 = Instant::now();
        match cap.grab(&mut buf) {
            Grab::Frame => {
                // [20261020] 阶段标记 3：Frame 分支处理（publish/帧率节流）。
                // 心跳停 15s 且 stage=3 = 卡在发布或节流 sleep；配合 stage=4
                // 把「grab 返回后到下一轮 grab 前」的全部路径纳入定位网。
                crate::dxgi::GRAB_STAGE.store(3, Ordering::Relaxed);
                consec_err = 0;
                us_total += t0.elapsed().as_micros();
                frames += 1;
                if !first_ok {
                    first_ok = true;
                    eprintln!(
                        "[InDesk] 静默取屏已出帧：{ow}x{oh}（DXGI，{}），原始帧 {} 字节",
                        cap.out_format().name(),
                        buf.len()
                    );
                }
                let usable = if is_h264 { !buf.is_empty() } else { buf.len() == expect };
                if usable {
                    let b = std::mem::take(&mut buf);
                    if direct {
                        // 直送：这一帧就是最终帧，不经编码线程。
                        // 直送没有分条，偏移传空（前端据此走单帧路径）
                        publish(b, Vec::new());
                    } else {
                        publish_raw(b);
                    }
                }
                // 帧率控制：只在真出帧时节流；桌面静止时 AcquireNextFrame 自会等待，不额外占 CPU
                // [20260918e idleslow] 每帧重读空闲开关：降速时目标 2fps
                next += Duration::from_micros(1_000_000 / effective_fps(fps).max(1) as u64);
                let now = Instant::now();
                if next > now {
                    std::thread::sleep(next - now);
                } else {
                    next = now;
                }
            }
            Grab::NoChange => {
                consec_err = 0;
                nochange += 1;
            }
            Grab::Err(e) => {
                if !err_logged {
                    err_logged = true;
                    eprintln!("[InDesk] DXGI 取帧失败：{e}");
                }
                consec_err += 1;
                // 每 250ms 重试一次；连续失败约 10 秒判定不可用（如切到 RDP 会话），回退 GDI
                if consec_err >= 40 {
                    return Err(format!("连续 {consec_err} 次取帧失败，最后错误：{e}"));
                }
                // [20260918 capfix] 睡之前先查停止信号。
                //
                // 原来这里无条件 sleep(250ms)：`stop()` 后线程最坏还要在这条
                // 重试链上走满 40 × 250ms = **10 秒**才退出，而它的 capturer 一直
                // 不肯放手 —— `start()` 的同步等待（stop_and_join，上限 5s）因此超时，
                // 新会话的 DXGI 复制对象建不出来（独占）→ OUT_MODE 卡在 starting →
                // 前端零帧。这正是用户报的「连上就死画面」的死锁环节。
                // 把"是否还要继续"的判定提到 sleep 之前，stop() 立刻生效、最坏 250ms 退出。
                if !RUNNING.load(Ordering::Relaxed) || SESSION.load(Ordering::Relaxed) != session {
                    return Ok(());
                }
                std::thread::sleep(Duration::from_millis(250));
            }
        }

        // [20261020] 阶段标记 4：慢日志 → 心跳刷新 → 5s 诊断（eprintln）区。
        // 心跳停 15s 且 stage=4 = 卡在诊断打印或 capture_diag 落盘。
        crate::dxgi::GRAB_STAGE.store(4, Ordering::Relaxed);
        // [20261020] 单次 grab 卡 >1s 落盘：NVENC/复制对象卡死现场此前完全
        // 无线索，这就是直接证据（正常单帧 3~16ms）。
        let grab_ms = t0.elapsed().as_millis();
        if grab_ms > 1000 {
            capture_diag(&format!(
                "slow grab: {}ms frames={}（若反复出现即 NVENC/驱动卡死）",
                grab_ms, frames
            ));
        }
        // [20261020] 心跳：grab 返回（含 NoChange/Err）即刷新，供看护线程判死。
        LAST_GRAB_TS.store(now_ms_u64(), Ordering::Relaxed);

        if last_report.elapsed() >= Duration::from_secs(5) {
            let avg = if frames > 0 { us_total / frames as u128 } else { 0 };
            eprintln!(
                "[InDesk] 抓屏诊断(DXGI)：5s 出 {frames} 帧（{}fps），平均 {avg}us/帧，无变化跳过 {nochange} 次（{ow}x{oh}，目标 {fps}fps）",
                frames / 5
            );
            // H.264 模式额外报编码器账目：这几行直接决定"达标没有"能不能自证
            // （阶段① 验收标准：60fps、文字清晰、CPU 10% 以内）。
            if let Some((n, bytes, us, idrs, mx)) = cap.take_encoder_stats() {
                let per = if n > 0 { us / n as u128 } else { 0 };
                let mbps = bytes as f64 * 8.0 / 5.0 / 1e6;
                eprintln!(
                    "[InDesk] 编码诊断：5s 编 {n} 帧（其中 IDR {idrs}），平均 {per}us/帧，输出 {mbps:.2} Mbps，单帧最大 {mx} B（目标码率 {}kbps）",
                    cap.encoder_bitrate() / 1000
                );
            }
            frames = 0;
            us_total = 0;
            nochange = 0;
            last_report = Instant::now();
        }
        // 一轮循环走完：清阶段标记（下一轮 grab 进入时会重新标 1/2）。
        crate::dxgi::GRAB_STAGE.store(0, Ordering::Relaxed);
    }
    Ok(())
}

/// GDI 抓屏主循环（DXGI 不可用时的回退路径）：BitBlt/StretchBlt 抓主屏 → 原始 BGRA 发布。
///
/// ⚠️ 这里用 `CreateDIBSection`（DIB 段，可直接读像素）而**不是**
/// `CreateCompatibleBitmap` + `GetDIBits`：实测后者在本机会出现 BitBlt 成功但
/// `GetDIBits` 拿不到数据、导致整条链路一帧都发不出去的情况；
/// DIB 段方案少一次全屏拷贝，也无需依赖 GetDIBits 的格式协商。
fn capture_loop_gdi(session: u64, sw: u32, sh: u32, ow: u32, oh: u32, fps: u32, direct: bool) {
    use windows_sys::Win32::Graphics::Gdi::{
        BitBlt, CreateCompatibleDC, CreateDIBSection, DeleteDC, DeleteObject, GdiFlush, GetDC,
        ReleaseDC, SelectObject, SetBrushOrgEx, SetStretchBltMode, StretchBlt, BITMAPINFO,
        BITMAPINFOHEADER, BI_RGB, DIB_RGB_COLORS, HALFTONE, SRCCOPY,
    };

    // [20260918e idleslow] 节流间隔不再预计算 —— 空闲降速要求每帧重读开关，
    // 见两处抓屏循环里的 effective_fps()。
    let mut next = Instant::now();
    let len = ow as usize * oh as usize * 4;

    unsafe {
        let screen_dc = GetDC(std::ptr::null_mut());
        if screen_dc.is_null() {
            eprintln!("[InDesk] 取屏失败：GetDC(NULL) 返回空");
            return;
        }
        let mem_dc = CreateCompatibleDC(screen_dc);
        if mem_dc.is_null() {
            eprintln!("[InDesk] 取屏失败：CreateCompatibleDC 返回空");
            let _ = ReleaseDC(std::ptr::null_mut(), screen_dc);
            return;
        }

        let mut bi: BITMAPINFO = std::mem::zeroed();
        bi.bmiHeader.biSize = std::mem::size_of::<BITMAPINFOHEADER>() as u32;
        bi.bmiHeader.biWidth = ow as i32;
        // 负高度 = top-down，与 canvas 左上原点一致，省一次翻转
        bi.bmiHeader.biHeight = -(oh as i32);
        bi.bmiHeader.biPlanes = 1;
        bi.bmiHeader.biBitCount = 32;
        bi.bmiHeader.biCompression = BI_RGB;

        let mut bits: *mut std::ffi::c_void = std::ptr::null_mut();
        let bmp = CreateDIBSection(
            mem_dc,
            &bi,
            DIB_RGB_COLORS,
            &mut bits,
            std::ptr::null_mut(),
            0,
        );
        if bmp.is_null() || bits.is_null() {
            eprintln!("[InDesk] 取屏失败：CreateDIBSection 返回空");
            let _ = DeleteDC(mem_dc);
            let _ = ReleaseDC(std::ptr::null_mut(), screen_dc);
            return;
        }

        let old = SelectObject(mem_dc, bmp);
        // 缩放时用 HALFTONE，避免文字出现严重锯齿
        SetStretchBltMode(mem_dc, HALFTONE);
        SetBrushOrgEx(mem_dc, 0, 0, std::ptr::null_mut());

        let mut first_ok = false;
        let mut fail_logged = false;
        let mut diag_frames = 0u64;
        let mut diag_us: u128 = 0;
        let mut diag_last = Instant::now();

        while RUNNING.load(Ordering::Relaxed) && SESSION.load(Ordering::Relaxed) == session {
            let t_cap = Instant::now();
            let ok = if ow == sw && oh == sh {
                BitBlt(mem_dc, 0, 0, ow as i32, oh as i32, screen_dc, 0, 0, SRCCOPY)
            } else {
                StretchBlt(
                    mem_dc,
                    0,
                    0,
                    ow as i32,
                    oh as i32,
                    screen_dc,
                    0,
                    0,
                    sw as i32,
                    sh as i32,
                    SRCCOPY,
                )
            };

            if ok == 0 {
                if !fail_logged {
                    fail_logged = true;
                    eprintln!(
                        "[InDesk] 取屏失败：BitBlt/StretchBlt 返回 0（{}）",
                        std::io::Error::last_os_error()
                    );
                }
            } else {
                // 光标不再画进画面（20260914 决定性修复）：
                // 主控端改为显示本机光标，跟手 0 延迟。若这里继续把被控端光标
                // 画进帧，主控会同时看到「一个跟手 + 一个滞后」的两个鼠标，更乱。
                // 观看模式需要的远端光标，后续用独立的 cursor 元数据通道补回。
                // GDI 绘图是异步的：CPU 直接读 DIB 段之前必须 flush
                GdiFlush();
                // 只拷贝目标尺寸的 BGRA 帧发布
                let src = std::slice::from_raw_parts(bits as *const u8, len);
                let mut bgra = Vec::with_capacity(len);
                bgra.extend_from_slice(src);
                if !first_ok {
                    first_ok = true;
                    eprintln!(
                        "[InDesk] 静默取屏已出帧：{ow}x{oh}（GDI），原始帧 {} 字节",
                        bgra.len()
                    );
                }
                if direct {
                    // 直送没有分条，偏移传空
                    publish(bgra, Vec::new());
                } else {
                    publish_raw(bgra);
                }
                diag_frames += 1;
                diag_us += t_cap.elapsed().as_micros();
                if diag_last.elapsed() >= Duration::from_secs(5) {
                    let avg = if diag_frames > 0 {
                        diag_us / diag_frames as u128
                    } else {
                        0
                    };
                    eprintln!(
                        "[InDesk] 抓屏诊断(GDI)：5s 出 {diag_frames} 帧，平均 {avg}us/帧（{ow}x{oh}）"
                    );
                    diag_frames = 0;
                    diag_us = 0;
                    diag_last = Instant::now();
                }
            }

            // 帧率控制
            // [20260918e idleslow] 每帧重读空闲开关：降速时目标 2fps
            next += Duration::from_micros(1_000_000 / effective_fps(fps).max(1) as u64);
            let now = Instant::now();
            if next > now {
                std::thread::sleep(next - now);
            } else {
                next = now;
            }
        }

        SelectObject(mem_dc, old);
        let _ = DeleteObject(bmp);
        let _ = DeleteDC(mem_dc);
        let _ = ReleaseDC(std::ptr::null_mut(), screen_dc);
    }
}
