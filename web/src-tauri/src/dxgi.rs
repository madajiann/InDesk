//! DXGI Desktop Duplication 抓屏 + **GPU 内缩放**（1.4.0「换引擎」）。
//!
//! ## 为什么换掉 GDI
//!
//! 旧路径用 `BitBlt` / `StretchBlt(HALFTONE)` 抓主屏，有两个硬伤：
//!
//! 1. `BitBlt` 全屏拷贝是 **CPU** 做的（1080p 一帧 8MB），且必须整帧搬一遍；
//! 2. 尺寸不合适时用 `HALFTONE` 缩放 —— 它做逐像素加权滤波，是 GDI 里最慢的缩放模式，
//!    实测 1080p→720p 一帧要十几到二十几毫秒，直接顶住帧率上限。
//!
//! DXGI Desktop Duplication 是 Windows 8 起的官方桌面复制 API：由 GPU 把桌面输出
//! 直接复制到可读纹理，并且**桌面没变化时不产生任何拷贝**（`AcquireNextFrame` 超时返回）。
//! 这是 ToDesk / 向日葵 / RustDesk 同一层的做法。
//!
//! ## 1.4.0 换引擎：缩放改由 GPU 做（实测依据）
//!
//! 用 `examples/engine_proto.rs` 在本机（i5-4590 + GTX 750，源 1680x1050）逐段实测，
//! 结果是**推翻了 1.3.x 写在 remote/index.vue 里的判断**（那条注释说"瓶颈是
//! GPU→CPU 回读 7MB/帧"）：
//!
//! | 环节 | 实测 |
//! |---|---|
//! | CopyResource 全帧（GPU 内） | 0.00 ms/帧 |
//! | Map 回读全帧 BGRA 6.73MB | **1.28 ms/帧** ← 回读根本不慢 |
//! | memcpy 到 Vec（含分配） | 1.15 ms/帧 |
//! | CPU box 下采样 →1280x800 | **28.34 ms/帧** ← 真凶之一 |
//! | JPEG 编码 1680x1050 q85 | **48.86 ms/帧** ← 真凶之一（q50 也要 46.4ms） |
//! | JPEG 编码 1280x800 q85 | **29.28 ms/帧** |
//! | VideoProcessorBlt（GPU 缩放+换色） | **0.06 ms/帧** |
//! | 新路径全程（GPU 缩放 → 回读目标尺寸） | 2.58 ms/帧 |
//!
//! 结论：GPU→CPU 回读不是瓶颈（1.28ms），**CPU 单线程标量下采样（28.3ms）与
//! JPEG 编码（29~49ms）才是**。所以 1.4.0 做两件事：
//!
//! 1. **缩放交给 GPU**：D3D11 VideoProcessor 把桌面纹理直接缩放到目标尺寸
//!    （0.06ms/帧），CPU 不再遍历任何源像素 —— 本文件的 `Chain` 就是这条链；
//! 2. **回读只读目标尺寸**：暂存纹理从"源分辨率"改成"输出分辨率"
//!    （1680x1050 BGRA 6.73MB → 1280x800 BGRA 4.10MB），且壳侧**不再 JPEG 编码**
//!    （见 capture.rs：直送模式连编码线程都不启，前端 `new VideoFrame` 零解码上屏，
//!    最终压缩本来就由 WebRTC 硬件编码器完成）。
//!
//! 为什么不顺手上 NV12：NV12 省带宽（1.46MB vs 4.10MB），但会把
//! "RGB→YUV 用哪个色彩空间"变成必须猜的事（VideoProcessor 输出侧的色彩空间
//! 与前端 `new VideoFrame` 的默认假定必须一致）。本机 WebView2 实测
//! `new VideoFrame(buf,{format:'NV12'})` 可用、`BGRA` 也可用；**BGRA 零换色、
//! 像素级精确**，所以默认 BGRA，NV12 作为带宽优化留到 1.4.1 用像素比对验证后再开。
//!
//! ## 边界情况（都已处理）
//!
//! - **RDP / 无桌面会话不支持**：`DuplicateOutput` 会失败（DXGI_ERROR_UNSUPPORTED 等），
//!   由调用方回退 GDI 路径（`capture.rs::capture_loop_gdi`）。
//! - **会话锁定 / UAC 安全桌面**：`AcquireNextFrame` 返回 E_ACCESSDENIED，属可恢复的
//!   瞬时错误；调用方连续失败到阈值后再回退，避免锁屏一下就永久断流。
//! - **只有鼠标移动**：DXGI 把光标作为独立元数据传递，**不画进图像**。所以图像没变时
//!   `AccumulatedFrames == 0`，本模块直接跳过（不产生无意义的编码与带宽）。
//! - **VideoProcessor 不可用**（老驱动 / 基本显示适配器）：`gpu_scaling()` 返回 false，
//!   BGRA 模式自动回退到"全帧回读 + CPU 盒式下采样"，链路仍然出帧（只是慢）；
//!   NV12 模式因无 CPU 转换实现，则整体失败让上层回退 JPEG。
//! - 本模块**不绘制鼠标光标**：主控端显示本机光标（跟手 0 延迟），被控端再把光标画进
//!   画面会造成「一个跟手 + 一个滞后」的双鼠标。
//! - **[1.4.5 阶段①] `OutFormat::H264`**：VideoProcessorBlt 产出 NV12 纹理后
//!   **不回读 CPU**，直接零拷贝喂 NVENC 硬编（见 `crate::h264`）。
//!   该模式下本模块发布的是一帧一个访问单元的 **Annex-B 码流**，不是像素。

use core::mem::ManuallyDrop;
use std::sync::atomic::{AtomicUsize, Ordering};

use windows::core::Interface;
use windows::Win32::Foundation::HMODULE;
use windows::Win32::Graphics::Direct3D::{
    D3D_DRIVER_TYPE_HARDWARE, D3D_FEATURE_LEVEL_10_0, D3D_FEATURE_LEVEL_10_1,
    D3D_FEATURE_LEVEL_11_0,
};
use windows::Win32::Graphics::Direct3D11::{
    D3D11CreateDevice, D3D11_BIND_RENDER_TARGET, D3D11_CPU_ACCESS_READ,
    D3D11_CREATE_DEVICE_BGRA_SUPPORT, D3D11_MAPPED_SUBRESOURCE, D3D11_MAP_READ, D3D11_SDK_VERSION,
    D3D11_TEX2D_VPIV, D3D11_TEX2D_VPOV, D3D11_TEXTURE2D_DESC, D3D11_USAGE_DEFAULT,
    D3D11_USAGE_STAGING, D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE,
    D3D11_VIDEO_PROCESSOR_CONTENT_DESC, D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC,
    D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC_0, D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC,
    D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC_0, D3D11_VIDEO_PROCESSOR_STREAM,
    D3D11_VIDEO_USAGE_PLAYBACK_NORMAL, D3D11_VPIV_DIMENSION_TEXTURE2D,
    D3D11_VPOV_DIMENSION_TEXTURE2D, ID3D11Device, ID3D11DeviceContext, ID3D11Resource,
    ID3D11Texture2D, ID3D11VideoContext, ID3D11VideoDevice, ID3D11VideoProcessor,
    ID3D11VideoProcessorEnumerator, ID3D11VideoProcessorInputView, ID3D11VideoProcessorOutputView,
};
use windows::Win32::Graphics::Dxgi::Common::{
    DXGI_FORMAT, DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_FORMAT_NV12, DXGI_RATIONAL, DXGI_SAMPLE_DESC,
};
use windows::Win32::Graphics::Dxgi::{
    IDXGIAdapter, IDXGIDevice, IDXGIOutput1, IDXGIOutputDuplication, IDXGIResource,
    DXGI_ERROR_WAIT_TIMEOUT, DXGI_OUTDUPL_FRAME_INFO,
};

use crate::h264::H264Encoder;
use crate::soft264::SoftEncoder;
use crate::capture::capture_diag;

/// `AcquireNextFrame` 的等待上限：桌面静止时到点即返回，不空转烧 CPU。
const ACQUIRE_TIMEOUT_MS: u32 = 200;

/// [20261020] 抓屏阶段原子标记（心跳看护杀核前读取落盘，定位卡死堵点）。
///
/// grab() 内部（AcquireNextFrame / GPU 缩放 / NVENC 编码）没有任何超时上限，
/// 而慢帧日志只在 grab **返回后**才打 —— 卡死的那次 grab 永远不返回，
/// 现场永远拿不到（12:44 实测心跳丢失 15.7s 却查不出堵在哪）。抓屏线程
/// 每进入一个阶段就写这个值，看护线程杀核前读出来写进 capture_diag.log。
///
/// 0=不在 grab 内（罕见，说明卡在循环外）1=acquire（AcquireNextFrame）
/// 2=process（GPU 缩放 + NVENC 编码 + ReleaseFrame）
pub static GRAB_STAGE: AtomicUsize = AtomicUsize::new(0);

pub fn grab_stage_name(v: usize) -> &'static str {
    match v {
        1 => "acquire(AcquireNextFrame)",
        2 => "process(GPU缩放/NVENC编码)",
        _ => "grab外",
    }
}

/// 一帧的像素格式（= 壳最终发布给前端的格式）。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum OutFormat {
    /// 4 字节/像素、通道顺序 B,G,R,A —— 与桌面纹理同格式，VideoProcessor 只做缩放不换色，
    /// 像素级精确；前端 `new VideoFrame(buf,{format:'BGRA'})` 直接可用。
    Bgra,
    /// 1.5 字节/像素、Y 平面 + 交错 UV 平面。省带宽，但需要前后端对色彩空间达成一致，
    /// 默认不启用（留作 1.4.1 的带宽优化）。
    Nv12,
    /// **[1.4.5 阶段①] 壳侧 NVENC 硬件 H.264**：发布内容是 **Annex-B 已编码码流**
    /// （一个访问单元 = 一帧），**不是像素数据**。
    ///
    /// 中间纹理仍是 NV12（VideoProcessorBlt 直出），但**不回读 CPU** —— 在 GPU 上
    /// 零拷贝注册给 NVENC 编码。于是本模式同时避开了两个已实测的瓶颈：
    /// 裸像素直送（BGRA 4MB/帧 → 11fps、NV12 1.46MB/帧 → 26.7fps 且偏色）
    /// 与 CPU JPEG（29~49ms/帧）。1080p60 每帧只有约 8KB。
    ///
    /// ⚠️ 前端必须用 `WebCodecs VideoDecoder`（`avc: {format:'annexb'}`）解码，
    /// **不能** `new VideoFrame(buf, {format})` —— 那是 bgra/nv12 的用法。
    H264,
}

impl OutFormat {
    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "bgra" => Some(Self::Bgra),
            "nv12" => Some(Self::Nv12),
            // "h264" 是正式名；"avc" 只是容错别名（前端可能这么传）。
            "h264" | "avc" => Some(Self::H264),
            _ => None,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Self::Bgra => "bgra",
            Self::Nv12 => "nv12",
            Self::H264 => "h264",
        }
    }

    /// 前端 `new VideoFrame(buf, { format })` 用的格式名。
    ///
    /// ⚠️ `H264` **没有**对应的 `VideoFrame` 格式 —— 它是已编码码流，必须走
    /// `VideoDecoder`。这里返回 `"AVC"` 是**刻意的非法值**（`VideoFrame` 不认它），
    /// 免得日后有人顺手拿它造出一个满屏花屏的 `VideoFrame` 还查不出原因。
    pub fn web_format(self) -> &'static str {
        match self {
            Self::Bgra => "BGRA",
            Self::Nv12 => "NV12",
            Self::H264 => "AVC",
        }
    }

    fn dxgi(self) -> DXGI_FORMAT {
        match self {
            // H264 的**中间纹理**是 NV12（VideoProcessor 直出），
            // 但发布给前端的不是这块纹理而是 NVENC 编出来的码流。
            Self::Bgra => DXGI_FORMAT_B8G8R8A8_UNORM,
            Self::Nv12 | Self::H264 => DXGI_FORMAT_NV12,
        }
    }

    /// 一帧紧凑排布（无行填充）时的字节数。
    ///
    /// ⚠️ `H264` 是**变长**码流，没有固定帧长，这里返回 **0**。
    /// 调用方**不能**拿它做 `buf.len() == frame_len()` 的等值判断，
    /// 否则 H264 模式会一帧都发不出去（见 capture.rs 里对 H264 的单独分支）。
    pub fn frame_len(self, w: u32, h: u32) -> usize {
        let px = (w as usize) * (h as usize);
        match self {
            Self::Bgra => px * 4,
            Self::Nv12 => px * 3 / 2,
            Self::H264 => 0,
        }
    }
}

/// 一次取帧的结果。
#[derive(Debug)]
pub enum Grab {
    /// 已取到新帧，并按目标尺寸写入调用方缓冲（紧凑排布，无行填充）。
    Frame,
    /// 桌面图像无变化（或仅光标移动），本次没有需要处理的内容。
    NoChange,
    /// 出错。调用方自行决定重试或回退其它抓屏路径。
    Err(String),
}

/// 目标尺寸 → 源尺寸的下采样映射（每行/列覆盖的源区间）。
/// 只在尺寸变化时重建：逐像素做整数除法会把下采样拖慢一个数量级。
/// **仅 GPU 缩放不可用时的回退路径会用到**（1.4.0 起正常路径不再下采样）。
struct ScaleMap {
    dw: usize,
    dh: usize,
    x0: Vec<u32>,
    x1: Vec<u32>,
    y0: Vec<u32>,
    y1: Vec<u32>,
}

impl ScaleMap {
    fn build(dw: usize, dh: usize, sw: usize, sh: usize) -> Self {
        // 第 i 个目标像素对应源区间 [i*s/d, (i+1)*s/d)，左闭右开且至少 1 像素宽
        let span = |i: usize, d: usize, s: usize| -> (u32, u32) {
            let a = (i * s) / d;
            let mut b = ((i + 1) * s + d - 1) / d;
            if b <= a {
                b = a + 1;
            }
            if b > s {
                b = s;
            }
            (a as u32, b as u32)
        };
        let mut x0 = Vec::with_capacity(dw);
        let mut x1 = Vec::with_capacity(dw);
        for x in 0..dw {
            let (a, b) = span(x, dw, sw);
            x0.push(a);
            x1.push(b);
        }
        let mut y0 = Vec::with_capacity(dh);
        let mut y1 = Vec::with_capacity(dh);
        for y in 0..dh {
            let (a, b) = span(y, dh, sh);
            y0.push(a);
            y1.push(b);
        }
        Self {
            dw,
            dh,
            x0,
            x1,
            y0,
            y1,
        }
    }
}

/// 整型 box 平均下采样（回退路径）。每个目标像素 = 对应源窗口内所有像素的算术平均。
fn downsample(src: &[u8], pitch: usize, m: &ScaleMap, dst: &mut [u8]) {
    let dw = m.dw;
    for y in 0..m.dh {
        let y0 = m.y0[y] as usize;
        let y1 = m.y1[y] as usize;
        let drow = y * dw * 4;
        for x in 0..dw {
            let x0 = m.x0[x] as usize;
            let x1 = m.x1[x] as usize;
            let mut sb = 0u32;
            let mut sg = 0u32;
            let mut sr = 0u32;
            let mut n = 0u32;
            for sy in y0..y1 {
                let row = &src[sy * pitch + x0 * 4..sy * pitch + x1 * 4];
                for c in row.chunks_exact(4) {
                    sb += c[0] as u32;
                    sg += c[1] as u32;
                    sr += c[2] as u32;
                }
                n += (x1 - x0) as u32;
            }
            let o = drow + x * 4;
            dst[o] = (sb / n) as u8;
            dst[o + 1] = (sg / n) as u8;
            dst[o + 2] = (sr / n) as u8;
            dst[o + 3] = 255;
        }
    }
}

fn create_tex(
    device: &ID3D11Device,
    w: u32,
    h: u32,
    fmt: DXGI_FORMAT,
    bind: u32,
    usage: windows::Win32::Graphics::Direct3D11::D3D11_USAGE,
    cpu_access: u32,
) -> Result<ID3D11Texture2D, String> {
    let d = D3D11_TEXTURE2D_DESC {
        Width: w,
        Height: h,
        MipLevels: 1,
        ArraySize: 1,
        Format: fmt,
        SampleDesc: DXGI_SAMPLE_DESC {
            Count: 1,
            Quality: 0,
        },
        Usage: usage,
        BindFlags: bind,
        CPUAccessFlags: cpu_access,
        MiscFlags: 0,
    };
    let mut t: Option<ID3D11Texture2D> = None;
    unsafe { device.CreateTexture2D(&d, None, Some(&mut t)) }
        .map_err(|e| format!("创建纹理失败({w}x{h}): {e}"))?;
    t.ok_or_else(|| "创建纹理返回空".to_string())
}

/// GPU 缩放链：桌面纹理 →（VideoProcessor 缩放）→ 输出纹理 → 暂存 → CPU。
/// 全部驻留 GPU，CPU 只在最后读一次目标尺寸的像素。
struct Chain {
    /// 源尺寸 BGRA 纹理。VideoProcessor 的输入视图要求资源带 `D3D11_BIND_RENDER_TARGET`，
    /// 而桌面复制给的纹理不保证带该标志，所以先原样拷进自己的纹理再用。
    src_tex: ID3D11Texture2D,
    src_res: ID3D11Resource,
    in_view: ID3D11VideoProcessorInputView,
    vp: ID3D11VideoProcessor,
    vp_enum: ID3D11VideoProcessorEnumerator,
    /// 输出尺寸 + 输出格式（`D3D11_BIND_RENDER_TARGET`，VideoProcessor 要求）。
    out_tex: ID3D11Texture2D,
    out_res: ID3D11Resource,
    out_view: ID3D11VideoProcessorOutputView,
    /// 输出尺寸 + 输出格式 + `D3D11_USAGE_STAGING`：回读只有这么多数据。
    /// **`H264` 模式为 `None`** —— 那条路不回读 CPU，纹理直接进编码器，
    /// 连这块暂存显存和一次全帧 GPU→CPU 拷贝都省掉。
    staging: Option<ID3D11Texture2D>,
    staging_res: Option<ID3D11Resource>,
}

/// DXGI Desktop Duplication 抓屏器。**必须在将要使用的线程上创建**（复制对象有线程上下文）。
pub struct DxgiCapturer {
    device: ID3D11Device,
    context: ID3D11DeviceContext,
    /// VideoProcessor 那套接口；设备不支持时为 None，届时只能走 CPU 回退。
    video_device: Option<ID3D11VideoDevice>,
    video_context: Option<ID3D11VideoContext>,
    duplication: IDXGIOutputDuplication,
    src_w: u32,
    src_h: u32,
    /// 桌面纹理格式（通常 B8G8R8A8_UNORM），VideoProcessor 输入视图要用它。
    src_fmt: DXGI_FORMAT,
    out_w: u32,
    out_h: u32,
    fmt: OutFormat,
    /// **[1.4.5 阶段①]** H.264 硬编器（`OutFormat::H264` 才有值）。
    ///
    /// ⚠️ **必须声明在 `chain` 之前**：Rust 按声明顺序析构，而编码器持有
    /// `chain.out_tex` 的 NVENC 注册句柄 —— 必须先注销（丢编码器）再释放纹理，
    /// 否则是 use-after-free。
    enc: Option<H264Encoder>,
    /// **[20261021 七轮]** CPU 软编器（`OutFormat::H264` + `soft_mode` 才有值）。
    /// 与 `enc` 互斥：软编不碰显存/驱动，代价是吃 CPU（见 soft264.rs 头注释）。
    soft_enc: Option<SoftEncoder>,
    /// 软编专用：NV12 暂存纹理（H264 硬编时不需要，零拷贝直进编码器）。
    soft_staging: Option<ID3D11Texture2D>,
    soft_staging_res: Option<ID3D11Resource>,
    /// 回读后压紧的 NV12 连续缓冲（复用，避免每帧分配 1.5MB）。
    soft_nv12: Vec<u8>,
    /// 是否走 CPU 软编（否则 NVENC）。
    soft_mode: bool,
    /// 目标帧率（编码器需要，用于 idr_period 与时间戳折算）。
    fps: u32,
    /// 目标码率 bps（编码器 CBR 目标）。
    bitrate: u32,
    /// 上一帧之后是否被要求强制关键帧（新客户端接入 / 丢包恢复）。
    pending_idr: bool,
    /// GPU 缩放链；构建失败为 None（`gpu_scaling()` 返回 false）。
    chain: Option<Chain>,
    /// GPU 链构建失败原因（只报一次，避免每帧刷日志）。
    chain_err: Option<String>,
    /// 回退路径：源尺寸暂存纹理。
    cpu_staging: Option<ID3D11Texture2D>,
    cpu_map: Option<ScaleMap>,
    /// 是否已经成功产出过帧（用于「只有光标移动」时跳过无变化帧）
    produced: bool,
}

impl DxgiCapturer {
    /// 创建捕获器。`out_w`/`out_h` 为输出尺寸（NV12/H264 时必须偶数），`fmt` 为发布格式。
    /// `fps`/`bitrate` 只在 `OutFormat::H264` 下被用到（编码器参数）。
    /// 失败原因会原样带出，便于调用方判断是否回退（RDP 会话必失败）。
    pub fn new(
        out_w: u32,
        out_h: u32,
        fmt: OutFormat,
        fps: u32,
        bitrate: u32,
        soft: bool,
    ) -> Result<Self, String> {
        unsafe {
            // [20261020] 里程碑诊断：new() 内部（设备/复制对象/NVENC 链）没有任何
            // 超时上限，现场卡死时 OUT_MODE 永远停在 starting —— 每一步耗时
            // 必须落 capture_diag.log 才能定位卡在哪一步。
            let t_all = std::time::Instant::now();
            capture_diag(&format!(
                "DxgiCapturer::new enter {}x{} fmt={}",
                out_w,
                out_h,
                fmt.name()
            ));
            let levels = [
                D3D_FEATURE_LEVEL_11_0,
                D3D_FEATURE_LEVEL_10_1,
                D3D_FEATURE_LEVEL_10_0,
            ];
            let mut device: Option<ID3D11Device> = None;
            let mut context: Option<ID3D11DeviceContext> = None;
            D3D11CreateDevice(
                None::<&IDXGIAdapter>,
                D3D_DRIVER_TYPE_HARDWARE,
                HMODULE::default(),
                D3D11_CREATE_DEVICE_BGRA_SUPPORT,
                Some(&levels),
                D3D11_SDK_VERSION,
                Some(&mut device),
                None,
                Some(&mut context),
            )
            .map_err(|e| format!("D3D11CreateDevice 失败: {e}"))?;
            capture_diag(&format!(
                "DxgiCapturer::new device ok {}ms",
                t_all.elapsed().as_millis()
            ));
            let device = device.ok_or_else(|| "D3D11 设备为空".to_string())?;
            let context = context.ok_or_else(|| "D3D11 立即上下文为空".to_string())?;

            let dxgi_device: IDXGIDevice = device
                .cast()
                .map_err(|e| format!("取 IDXGIDevice 失败: {e}"))?;
            let adapter = dxgi_device
                .GetAdapter()
                .map_err(|e| format!("取 DXGI 适配器失败: {e}"))?;
            let output = adapter
                .EnumOutputs(0)
                .map_err(|e| format!("枚举主显示输出失败: {e}"))?;
            let output1: IDXGIOutput1 = output
                .cast()
                .map_err(|e| format!("取 IDXGIOutput1 失败: {e}"))?;
            let duplication = output1
                .DuplicateOutput(&device)
                .map_err(|e| format!("创建桌面复制失败（RDP/无桌面会话常见）: {e}"))?;
            capture_diag(&format!(
                "DxgiCapturer::new duplication ok {}ms",
                t_all.elapsed().as_millis()
            ));

            let d = duplication.GetDesc();
            let mut src_w = d.ModeDesc.Width;
            let mut src_h = d.ModeDesc.Height;
            // 少数驱动不填 ModeDesc，退回输出坐标兜底
            if src_w == 0 || src_h == 0 {
                if let Ok(od) = output.GetDesc() {
                    let r = od.DesktopCoordinates;
                    src_w = (r.right - r.left).max(0) as u32;
                    src_h = (r.bottom - r.top).max(0) as u32;
                }
            }
            let mut src_fmt = d.ModeDesc.Format;
            if src_fmt == DXGI_FORMAT(0) {
                src_fmt = DXGI_FORMAT_B8G8R8A8_UNORM;
            }

            let mut me = Self {
                device,
                context,
                video_device: None,
                video_context: None,
                duplication,
                src_w,
                src_h,
                src_fmt,
                out_w: out_w & !1,
                out_h: out_h & !1,
                fmt,
                enc: None,
                soft_enc: None,
                soft_staging: None,
                soft_staging_res: None,
                soft_nv12: Vec::new(),
                soft_mode: soft,
                fps: fps.max(1),
                bitrate,
                pending_idr: false,
                chain: None,
                chain_err: None,
                cpu_staging: None,
                cpu_map: None,
                produced: false,
            };

            // VideoProcessor 接口（拿不到就只走 CPU 回退）
            if let (Ok(vd), Ok(vc)) = (
                me.device.cast::<ID3D11VideoDevice>(),
                me.context.cast::<ID3D11VideoContext>(),
            ) {
                me.video_device = Some(vd);
                me.video_context = Some(vc);
                // 立刻建链（不等到第一帧）：这样上层在 start() 时就知道
                // 到底走没走 GPU，能如实告诉前端该按哪种格式解码。
                let t_chain = std::time::Instant::now();
                if let Err(e) = me.build_chain() {
                    me.chain_err = Some(e);
                    me.chain = None;
                }
                capture_diag(&format!(
                    "DxgiCapturer::new chain done gpu={} err={:?} {}ms",
                    me.chain.is_some(),
                    me.chain_err.as_deref().unwrap_or(""),
                    t_chain.elapsed().as_millis()
                ));
            } else {
                me.chain_err = Some("该设备不支持 D3D11 Video（ID3D11VideoDevice 不可用）".into());
            }

            Ok(me)
        }
    }

    /// 桌面原始分辨率（物理像素）。
    pub fn source_size(&self) -> (u32, u32) {
        (self.src_w, self.src_h)
    }

    /// 输出分辨率。
    pub fn out_size(&self) -> (u32, u32) {
        (self.out_w, self.out_h)
    }

    /// 输出像素格式。
    pub fn out_format(&self) -> OutFormat {
        self.fmt
    }

    /// 是否真的走 GPU 缩放（false = 回退 CPU 盒式下采样）。
    pub fn gpu_scaling(&self) -> bool {
        self.chain.is_some()
    }

    /// GPU 链失败原因（供上层诊断输出）。
    pub fn chain_error(&self) -> Option<&str> {
        self.chain_err.as_deref()
    }

    /// **[1.4.5 阶段①]** 请求下一帧编成 IDR（关键帧）。
    ///
    /// 触发时机：新客户端接入、丢包后恢复、分辨率/码率变更后。
    /// 不请求的话客户端最坏要等到周期关键帧（默认 2 秒）才有画面。
    /// 幂等：连调多次只生效一次。
    pub fn request_keyframe(&mut self) {
        if self.fmt == OutFormat::H264 {
            self.pending_idr = true;
        }
    }

    /// 编码器累计统计（帧数, 总字节, 总微秒, IDR 数, 单帧最大字节）；非 H264 模式返回 None。
    /// 取用即清零，供抓屏线程每 5 秒汇报一次。
    pub fn take_encoder_stats(&mut self) -> Option<(u64, u64, u128, u64, usize)> {
        if let Some(e) = self.enc.as_mut() {
            return Some(e.take_stats());
        }
        self.soft_enc.as_mut().map(|e| e.take_stats())
    }

    /// 实际生效的码率（bps）。非 H264 模式返回 0。
    pub fn encoder_bitrate(&self) -> u32 {
        if let Some(e) = self.enc.as_ref() {
            return e.bitrate();
        }
        self.soft_enc.as_ref().map(|e| e.bitrate()).unwrap_or(0)
    }

    /// 建立 GPU 缩放链。失败时不 panic，返回原因由上层决定回退。
    unsafe fn build_chain(&mut self) -> Result<(), String> {
        // 重建链之前**必须先释放旧编码器**：它注册的是即将被替换掉的 out_tex 纹理，
        // 顺序反了就是 use-after-free（分辨率切换时走这条）。
        self.enc = None;
        // 软编器同样要在纹理被替换前丢掉（它持有 CPU 帧缓冲与暂存纹理引用）
        self.soft_enc = None;
        self.soft_staging = None;
        self.soft_staging_res = None;

        let vd = self
            .video_device
            .as_ref()
            .ok_or_else(|| "无 ID3D11VideoDevice".to_string())?;
        let sw = self.src_w;
        let sh = self.src_h;
        let ow = self.out_w.max(2);
        let oh = self.out_h.max(2);
        let of = self.fmt.dxgi();

        let src_tex = create_tex(
            &self.device,
            sw,
            sh,
            self.src_fmt,
            D3D11_BIND_RENDER_TARGET.0 as u32,
            D3D11_USAGE_DEFAULT,
            0,
        )?;
        let out_tex = create_tex(
            &self.device,
            ow,
            oh,
            of,
            D3D11_BIND_RENDER_TARGET.0 as u32,
            D3D11_USAGE_DEFAULT,
            0,
        )?;
        // 所有格式都建一块暂存纹理：H.264 硬编平时用不到（零拷贝直进 NVENC），
        // 但 NVENC 初始化失败时要**自动兜底到 CPU 软编**，那条路必须有回读目标。
        // 代价只有一帧 NV12 的显存（1152x720 约 1.5MB）。
        let staging = Some(create_tex(
            &self.device,
            ow,
            oh,
            of,
            0,
            D3D11_USAGE_STAGING,
            D3D11_CPU_ACCESS_READ.0 as u32,
        )?);

        let content = D3D11_VIDEO_PROCESSOR_CONTENT_DESC {
            InputFrameFormat: D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE,
            InputFrameRate: DXGI_RATIONAL {
                Numerator: 30,
                Denominator: 1,
            },
            InputWidth: sw,
            InputHeight: sh,
            OutputFrameRate: DXGI_RATIONAL {
                Numerator: 30,
                Denominator: 1,
            },
            OutputWidth: ow,
            OutputHeight: oh,
            Usage: D3D11_VIDEO_USAGE_PLAYBACK_NORMAL,
        };
        let vp_enum = vd
            .CreateVideoProcessorEnumerator(&content)
            .map_err(|e| format!("CreateVideoProcessorEnumerator 失败: {e}"))?;

        // 能力核对：输入格式要能作输入（bit0），输出格式要能作输出（bit1）
        let fin = vp_enum
            .CheckVideoProcessorFormat(self.src_fmt)
            .map_err(|e| format!("CheckVideoProcessorFormat(输入) 失败: {e}"))?;
        if fin & 1 == 0 {
            return Err(format!(
                "VideoProcessor 不支持输入格式 {}",
                self.src_fmt.0
            ));
        }
        let fout = vp_enum
            .CheckVideoProcessorFormat(of)
            .map_err(|e| format!("CheckVideoProcessorFormat(输出) 失败: {e}"))?;
        if fout & 2 == 0 {
            return Err(format!("VideoProcessor 不支持输出格式 {}", of.0));
        }

        let vp = vd
            .CreateVideoProcessor(&vp_enum, 0)
            .map_err(|e| format!("CreateVideoProcessor 失败: {e}"))?;

        let out_view = {
            let d = D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC {
                ViewDimension: D3D11_VPOV_DIMENSION_TEXTURE2D,
                Anonymous: D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC_0 {
                    Texture2D: D3D11_TEX2D_VPOV { MipSlice: 0 },
                },
            };
            let mut v: Option<ID3D11VideoProcessorOutputView> = None;
            vd.CreateVideoProcessorOutputView(&out_tex, &vp_enum, &d, Some(&mut v))
                .map_err(|e| format!("CreateVideoProcessorOutputView 失败: {e}"))?;
            v.ok_or_else(|| "输出视图为空".to_string())?
        };
        let in_view = {
            let d = D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC {
                FourCC: 0,
                ViewDimension: D3D11_VPIV_DIMENSION_TEXTURE2D,
                Anonymous: D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC_0 {
                    Texture2D: D3D11_TEX2D_VPIV {
                        MipSlice: 0,
                        ArraySlice: 0,
                    },
                },
            };
            let mut v: Option<ID3D11VideoProcessorInputView> = None;
            vd.CreateVideoProcessorInputView(&src_tex, &vp_enum, &d, Some(&mut v))
                .map_err(|e| format!("CreateVideoProcessorInputView 失败: {e}"))?;
            v.ok_or_else(|| "输入视图为空".to_string())?
        };

        let src_res: ID3D11Resource = src_tex
            .cast()
            .map_err(|e| format!("src cast 失败: {e}"))?;
        let out_res: ID3D11Resource = out_tex
            .cast()
            .map_err(|e| format!("out cast 失败: {e}"))?;
        let staging_res: Option<ID3D11Resource> = match staging.as_ref() {
            Some(t) => Some(t.cast().map_err(|e| format!("staging cast 失败: {e}"))?),
            None => None,
        };

        // ---------- H264：把 out_tex 零拷贝注册进 NVENC ----------
        // 建不出编码器就**让整条链失败**（返回 Err），上层据此自动退回 JPEG ——
        // 这样不需要在抓屏循环里再加一套"H264 可用吗"的判断分支。
        if self.fmt == OutFormat::H264 {
            // 先按调用方指定建；NVENC 建不出来时**自动兜底 CPU 软编**（不再直接退 JPEG）。
            let mut nvenc_err: Option<String> = None;
            if !self.soft_mode {
                match H264Encoder::new(&self.device, &out_tex, ow, oh, self.fps, self.bitrate) {
                    Ok(e) => self.enc = Some(e),
                    Err(e) => nvenc_err = Some(e),
                }
            }
            if self.enc.is_none() {
                if let Some(e) = nvenc_err.as_deref() {
                    capture_diag(&format!("NVENC 不可用（{e}）→ 自动兜底 CPU 软编"));
                }
                match SoftEncoder::new(ow, oh, self.fps, self.bitrate) {
                    Ok(e) => {
                        self.soft_mode = true;
                        self.soft_enc = Some(e);
                        self.soft_nv12 = vec![0u8; (ow as usize) * (oh as usize) * 3 / 2];
                        self.soft_staging = staging.clone();
                        self.soft_staging_res = staging_res.clone();
                        capture_diag(&format!(
                            "soft264 ready {}x{} {}fps {}kbps（CPU 编码，不占显存）",
                            ow, oh, self.fps, self.bitrate / 1000
                        ));
                    }
                    Err(e) => {
                        return Err(format!(
                            "H.264 编码器不可用（NVENC: {}；软编: {e}）；将退回 JPEG",
                            nvenc_err.as_deref().unwrap_or("未尝试")
                        ))
                    }
                }
            }
        }

        self.chain = Some(Chain {
            src_tex,
            src_res,
            in_view,
            vp,
            vp_enum,
            out_tex,
            out_res,
            out_view,
            staging,
            staging_res,
        });
        Ok(())
    }

    /// 取一帧，按目标尺寸/格式写入 `dst`（紧凑排布）。
    pub fn grab(&mut self, dst: &mut Vec<u8>) -> Grab {
        // 包装层：无论哪条路径退出都清掉阶段标记，避免看护读到上一帧的旧值。
        let out = self.grab_inner(dst);
        GRAB_STAGE.store(0, Ordering::Relaxed);
        out
    }

    fn grab_inner(&mut self, dst: &mut Vec<u8>) -> Grab {
        // [20261020] 阶段计时：acquire（AcquireNextFrame）/ process（GPU 缩放
        // + NVENC 编码）。卡死现场靠慢帧日志里的阶段值定位。
        let t_grab = std::time::Instant::now();
        let mut info: DXGI_OUTDUPL_FRAME_INFO = unsafe { std::mem::zeroed() };
        let mut res: Option<IDXGIResource> = None;

        GRAB_STAGE.store(1, Ordering::Relaxed);
        match unsafe { self.duplication.AcquireNextFrame(ACQUIRE_TIMEOUT_MS, &mut info, &mut res) }
        {
            Ok(()) => {}
            Err(e) if e.code() == DXGI_ERROR_WAIT_TIMEOUT => {
                let ms = t_grab.elapsed().as_millis();
                if ms > 1000 {
                    crate::capture::capture_diag(&format!(
                        "slow grab phase: acquire={}ms（超时返回竟耗时过长）",
                        ms
                    ));
                }
                return Grab::NoChange;
            }
            Err(e) => return Grab::Err(format!("AcquireNextFrame 失败: {e}")),
        }

        GRAB_STAGE.store(2, Ordering::Relaxed);
        let t_acq = t_grab.elapsed().as_millis();
        if t_acq > 400 {
            crate::capture::capture_diag(&format!(
                "slow acquire {}ms（等待桌面新帧超长，非编码耗时）",
                t_acq
            ));
        }

        // 桌面图像没变（只有光标在动，或纯超时唤醒）：跳过，别做无意义的拷贝与编码。
        // 首次必须照常处理，防止个别驱动始终把 AccumulatedFrames 报 0 导致永远不出帧。
        if info.AccumulatedFrames == 0 && self.produced {
            let _ = unsafe { self.duplication.ReleaseFrame() };
            return Grab::NoChange;
        }

        let out = unsafe { self.process(res, dst) };
        if let Grab::Frame = out {
            self.produced = true;
        }

        // 无论成功与否都必须释放，否则下一次 AcquireNextFrame 必然失败
        let _ = unsafe { self.duplication.ReleaseFrame() };
        let ms = t_grab.elapsed().as_millis();
        if ms > 1000 {
            crate::capture::capture_diag(&format!(
                "slow grab phase: total={}ms acquire={}ms process={}ms",
                ms,
                t_acq,
                ms.saturating_sub(t_acq)
            ));
        }
        out
    }

    unsafe fn process(&mut self, res: Option<IDXGIResource>, dst: &mut Vec<u8>) -> Grab {
        let Some(res) = res else {
            return Grab::Err("AcquireNextFrame 未返回桌面资源".into());
        };
        let tex: ID3D11Texture2D = match res.cast() {
            Ok(t) => t,
            Err(e) => return Grab::Err(format!("取 ID3D11Texture2D 失败: {e}")),
        };
        let mut td: D3D11_TEXTURE2D_DESC = std::mem::zeroed();
        tex.GetDesc(&mut td);
        if td.Width == 0 || td.Height == 0 {
            return Grab::Err("桌面纹理尺寸为 0".into());
        }

        // 分辨率/格式变化（切显示模式）：整条链重建
        if td.Width != self.src_w || td.Height != self.src_h || td.Format != self.src_fmt {
            self.src_w = td.Width;
            self.src_h = td.Height;
            self.src_fmt = td.Format;
            // ⚠️ 顺序：编码器注册着旧 out_tex，必须先注销再释放纹理。
            self.enc = None;
            self.soft_enc = None;
            self.soft_staging = None;
            self.soft_staging_res = None;
            self.chain = None;
            self.cpu_staging = None;
            self.cpu_map = None;
            if self.video_device.is_some() {
                if let Err(e) = self.build_chain() {
                    self.chain_err = Some(e);
                    self.chain = None;
                }
            }
        }

        let tex_res: ID3D11Resource = match tex.cast() {
            Ok(r) => r,
            Err(e) => return Grab::Err(format!("桌面纹理 cast 失败: {e}")),
        };

        // ---------- 正常路径：GPU 缩放 → 只回读目标尺寸 ----------
        if self.chain.is_some() {
            match self.gpu_path(&tex_res, dst) {
                Ok(()) => return Grab::Frame,
                Err(e) => {
                    // 链失效（驱动重置 / 设备丢失）：丢弃后退回 CPU 路径，并留一次日志
                    if self.chain_err.is_none() {
                        self.chain_err = Some(format!("GPU 路径失败，回退 CPU: {e}"));
                    }
                    self.chain = None;
                }
            }
        }

        // ---------- 回退路径：全帧回读 + CPU 盒式下采样（只支持 BGRA 输出） ----------
        if self.fmt != OutFormat::Bgra {
            return Grab::Err(format!(
                "GPU 缩放不可用且 {} 无 CPU 转换实现（{}）",
                self.fmt.name(),
                self.chain_err.as_deref().unwrap_or("原因未知")
            ));
        }
        self.cpu_path(&tex_res, td.Width as usize, td.Height as usize, dst)
    }

    /// GPU 路径：CopyResource → VideoProcessorBlt → CopyResource → Map → 紧凑排布。
    unsafe fn gpu_path(&mut self, tex_res: &ID3D11Resource, dst: &mut Vec<u8>) -> Result<(), String> {
        let chain = self.chain.as_ref().ok_or_else(|| "链缺失".to_string())?;
        let vc = self
            .video_context
            .as_ref()
            .ok_or_else(|| "无 ID3D11VideoContext".to_string())?;

        // 桌面纹理 → 自己的源纹理（VideoProcessor 输入视图要求带 RENDER_TARGET，
        // 桌面复制给的纹理不保证；同时这一步也让格式/尺寸固定下来）
        self.context.CopyResource(&chain.src_res, tex_res);

        let mut stream = D3D11_VIDEO_PROCESSOR_STREAM {
            Enable: true.into(),
            OutputIndex: 0,
            InputFrameOrField: 0,
            PastFrames: 0,
            FutureFrames: 0,
            ..Default::default()
        };
        stream.pInputSurface = ManuallyDrop::new(Some(chain.in_view.clone()));
        vc.VideoProcessorBlt(&chain.vp, &chain.out_view, 0, &[stream])
            .map_err(|e| format!("VideoProcessorBlt 失败: {e}"))?;

        // ---------- H264：[1.4.5 阶段①] 到此为止，纹理已在 GPU 上，直接进编码器 ----------
        // 下面「拷贝到暂存 → Map 回读 → 紧凑排布」整段都跳过：
        // NVENC 从 out_tex 上直接读，一字节都不经过 CPU。这就是硬编相对
        // 直送/JPEG 两条老路的全部优势来源。
        if self.fmt == OutFormat::H264 {
            if self.soft_mode {
                // ---------- CPU 软编：NV12 回读 → 压紧 → OpenH264 ----------
                // 只有这一条路会经过 CPU；回读量是 1280x800x1.5 ≈ 1.5MB，
                // 实测 Map + 压紧 <1ms（远小于软编本身的 44ms/帧）。
                let (staging, staging_res) =
                    match (self.soft_staging.as_ref(), self.soft_staging_res.as_ref()) {
                        (Some(a), Some(b)) => (a, b),
                        _ => return Err("软编缺少 NV12 暂存纹理".to_string()),
                    };
                self.context.CopyResource(staging_res, &chain.out_res);

                let mut mapped: D3D11_MAPPED_SUBRESOURCE = std::mem::zeroed();
                self.context
                    .Map(staging, 0, D3D11_MAP_READ, 0, Some(&mut mapped))
                    .map_err(|e| format!("软编映射暂存纹理失败: {e}"))?;
                if mapped.pData.is_null() {
                    self.context.Unmap(staging, 0);
                    return Err("软编映射返回空指针".into());
                }
                let w = self.out_w as usize;
                let h = self.out_h as usize;
                let y_len = w * h;
                let row = mapped.RowPitch as usize;
                let p = mapped.pData as *const u8;
                // Y 平面 h 行 + UV 平面 h/2 行，逐行压掉 RowPitch 填充
                self.soft_nv12.resize(y_len + y_len / 2, 0);
                let dst_ptr = self.soft_nv12.as_mut_ptr();
                for r in 0..h {
                    std::ptr::copy_nonoverlapping(p.add(r * row), dst_ptr.add(r * w), w);
                }
                let uv_src = p.add(row * h);
                for r in 0..(h / 2) {
                    std::ptr::copy_nonoverlapping(uv_src.add(r * row), dst_ptr.add(y_len + r * w), w);
                }
                self.context.Unmap(staging, 0);

                let enc = self
                    .soft_enc
                    .as_mut()
                    .ok_or_else(|| "软编编码器未就绪（链状态异常）".to_string())?;
                if self.pending_idr {
                    self.pending_idr = false;
                    enc.request_idr();
                }
                match enc.encode_nv12(&self.soft_nv12, dst) {
                    Ok(Some(_)) => {}
                    Ok(None) => dst.clear(),
                    Err(e) => return Err(e),
                }
                return Ok(());
            }

            let enc = self
                .enc
                .as_mut()
                .ok_or_else(|| "H.264 编码器未就绪（链状态异常）".to_string())?;
            if self.pending_idr {
                self.pending_idr = false;
                enc.request_idr();
            }
            match enc.encode_into(dst) {
                Ok(Some(_stat)) => {}
                // 编码器吞帧（正常现象）：dst 保持空，上层判定空帧后不发布
                Ok(None) => dst.clear(),
                Err(e) => return Err(e),
            }
            return Ok(());
        }

        let (Some(staging), Some(staging_res)) = (chain.staging.as_ref(), chain.staging_res.as_ref())
        else {
            return Err("非 H264 模式却缺少暂存纹理".to_string());
        };
        self.context.CopyResource(staging_res, &chain.out_res);

        let mut mapped: D3D11_MAPPED_SUBRESOURCE = std::mem::zeroed();
        self.context
            .Map(staging, 0, D3D11_MAP_READ, 0, Some(&mut mapped))
            .map_err(|e| format!("映射暂存纹理失败: {e}"))?;
        if mapped.pData.is_null() {
            self.context.Unmap(staging, 0);
            return Err("映射返回空指针".into());
        }
        copy_out(
            mapped.pData as *const u8,
            mapped.RowPitch as usize,
            self.out_w,
            self.out_h,
            self.fmt,
            dst,
        );
        self.context.Unmap(staging, 0);
        Ok(())
    }

    /// 回退路径：全帧回读 + CPU 盒式下采样（1.4.0 前的老行为，保留做兜底）。
    unsafe fn cpu_path(
        &mut self,
        tex_res: &ID3D11Resource,
        sw: usize,
        sh: usize,
        dst: &mut Vec<u8>,
    ) -> Grab {
        let need_staging = self.cpu_staging.is_none();
        if need_staging {
            match create_tex(
                &self.device,
                sw as u32,
                sh as u32,
                self.src_fmt,
                0,
                D3D11_USAGE_STAGING,
                D3D11_CPU_ACCESS_READ.0 as u32,
            ) {
                Ok(s) => {
                    self.cpu_staging = Some(s);
                }
                Err(e) => return Grab::Err(format!("创建暂存纹理失败: {e}")),
            }
        }
        let staging = match self.cpu_staging.as_ref() {
            Some(s) => s,
            None => return Grab::Err("暂存纹理缺失".into()),
        };

        self.context.CopyResource(staging, tex_res);

        let mut mapped: D3D11_MAPPED_SUBRESOURCE = std::mem::zeroed();
        if let Err(e) = self
            .context
            .Map(staging, 0, D3D11_MAP_READ, 0, Some(&mut mapped))
        {
            return Grab::Err(format!("映射暂存纹理失败: {e}"));
        }
        if mapped.pData.is_null() {
            self.context.Unmap(staging, 0);
            return Grab::Err("映射返回空指针".into());
        }

        let pitch = mapped.RowPitch as usize;
        let dw = self.out_w as usize;
        let dh = self.out_h as usize;

        if sw == dw && sh == dh && pitch == sw * 4 {
            // 原尺寸且无行填充：一次 memcpy
            let len = pitch * sh;
            let src = std::slice::from_raw_parts(mapped.pData as *const u8, len);
            dst.clear();
            dst.extend_from_slice(src);
        } else {
            let need_map = match &self.cpu_map {
                Some(m) => m.dw != dw || m.dh != dh,
                None => true,
            };
            if need_map {
                self.cpu_map = Some(ScaleMap::build(dw, dh, sw, sh));
            }
            let len = pitch * (sh - 1) + sw * 4;
            let src = std::slice::from_raw_parts(mapped.pData as *const u8, len);
            dst.clear();
            dst.resize(dw * dh * 4, 255);
            match self.cpu_map.as_ref() {
                Some(m) => downsample(src, pitch, m, dst),
                None => {
                    self.context.Unmap(staging, 0);
                    return Grab::Err("下采样映射缺失".into());
                }
            }
        }

        self.context.Unmap(staging, 0);
        Grab::Frame
    }
}

/// 把 D3D11 暂存纹理里带行填充的像素，按目标格式紧密排布拷进 `dst`
/// （前端 `new VideoFrame` 要求平面紧凑，不接受 RowPitch 填充）。
unsafe fn copy_out(
    src: *const u8,
    row_pitch: usize,
    w: u32,
    h: u32,
    fmt: OutFormat,
    dst: &mut Vec<u8>,
) {
    let w = w as usize;
    let h = h as usize;
    let need = fmt.frame_len(w as u32, h as u32);
    dst.clear();
    dst.reserve(need);

    let rows = |plane_base: usize, n: usize, row_bytes: usize, dst: &mut Vec<u8>| {
        if row_pitch == row_bytes {
            // 无填充：整块一次拷完（N 行也无所谓）
            let p = src.add(plane_base);
            dst.extend_from_slice(std::slice::from_raw_parts(p, row_bytes * n));
        } else {
            for y in 0..n {
                let p = src.add(plane_base + y * row_pitch);
                dst.extend_from_slice(std::slice::from_raw_parts(p, row_bytes));
            }
        }
    };

    match fmt {
        OutFormat::Bgra => rows(0, h, w * 4, dst),
        OutFormat::Nv12 => {
            // D3D11 规定 NV12 的 UV 平面紧跟在 Y 平面之后（偏移 = RowPitch * Height）
            rows(0, h, w, dst);
            rows(row_pitch * h, h / 2, w, dst);
        }
        // **H264 走不到这里**：它在 `gpu_path` 里过了 VideoProcessorBlt 就直接进编码器
        // （没有 CPU 回读，本函数是回读后的紧凑排布），而 `cpu_path` 又只支持 Bgra。
        // 保留这个分支只为让 match 穷尽；真跑进来就把 dst 清空，
        // 上层判"空帧"后不会发布，不会发出脏数据。
        OutFormat::H264 => dst.clear(),
    }
}
