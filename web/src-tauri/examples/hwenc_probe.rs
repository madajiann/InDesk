//! 硬编诊断探针 v2（开发用，不进安装包）。
//!
//! ```text
//! cargo run --release --example hwenc_probe
//! ```
//!
//! ## 为什么要有 v2
//!
//! `mf_probe.rs` 报了 `MF_E_TRANSFORM_ASYNC_LOCKED (0xC00D6D77)` —— 四路（枚举 A0/A1 + CLSID B-NVENC/B-QSV）
//! 全部卡在 `SET_D3D_MANAGER` 与 `SetInputType`。关键线索：探针打印的 `UNLOCK 前=空` 说明
//! **`activate.SetUINT32(UNLOCK)` 根本没有传到 MFT 的属性存储里**（那个返回值被 `let _ =` 丢了），
//! MFT 是在"锁定态"被激活的，事后在实例属性上补设无效。
//!
//! 本探针把四种组合一次测清，用实测结果决定产品里怎么写：
//!
//! | 变体 | 枚举/创建方式 | 用途判定 |
//! |---|---|---|
//! | V1 | `HARDWARE\|SORTANDFILTER`（**不带** ASYNCMFT） | MF 是否给同步包装（能用就直接用，最省事） |
//! | V2 | `HARDWARE\|SORTANDFILTER\|ASYNCMFT` + activate 解锁（**打印返回值**） | 解锁到底能不能生效 |
//! | V3 | `CoCreateInstance(NVENC/QSV)` + 实例属性解锁 | 绕过枚举的旁路 |
//! | V4 | 任一候选设为可用的前提下，再测 **D3D11 纹理零拷贝输入** | 零回读是否成立 |
//!
//! 每种「输入方式 × 编码器」都打点：设置类型是否成功、`ms/帧`、码流首帧字节、NAL 起始码、
//! 首帧是否关键帧（`MFSampleExtension_CleanPoint`）。
//!
//! 判定口径：**能编出含 NAL 起始码的码流**才算成立；`SetOutputType` 成功不算（官方文档的
//! 例外列表：未解锁时 SetOutputType / GetAttributes / GetInputAvailableType 仍可成功）。

use std::time::Instant;

use windows::core::{GUID, Interface, IUnknown};
use windows::Win32::Foundation::HMODULE;
use windows::Win32::Graphics::Direct3D::{
    D3D_DRIVER_TYPE_HARDWARE, D3D_FEATURE_LEVEL_10_0, D3D_FEATURE_LEVEL_10_1,
    D3D_FEATURE_LEVEL_11_0,
};
use windows::Win32::Graphics::Direct3D11::{
    D3D11_BIND_RENDER_TARGET, D3D11_BIND_SHADER_RESOURCE, D3D11_CREATE_DEVICE_BGRA_SUPPORT,
    D3D11_SDK_VERSION, D3D11_TEXTURE2D_DESC, D3D11_USAGE_DEFAULT, D3D11CreateDevice, ID3D11Device,
    ID3D11DeviceContext, ID3D11Resource, ID3D11Texture2D,
};
use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT_NV12, DXGI_SAMPLE_DESC};
use windows::Win32::Media::MediaFoundation::*;
use windows::Win32::System::Com::{
    CoCreateInstance, CoInitializeEx, CLSCTX_INPROC_SERVER, COINIT_MULTITHREADED,
};

/// `{60F44560-5A20-4857-BFEF-D29773CB8040}` NVIDIA H.264 Encoder MFT
const CLSID_NVENC: GUID = GUID::from_u128(0x60f44560_5a20_4857_bfef_d29773cb8040);
/// `{4BE8D3C0-0515-4A37-AD55-E4BAE19AF471}` Intel Quick Sync Video H.264 Encoder MFT
const CLSID_QSV: GUID = GUID::from_u128(0x4be8d3c0_0515_4a37_ad55_e4bae19af471);

// ---- 属性 GUID：windows-rs 0.61 未生成（mfobjects.h 的 DEFINE_GUID 未进 metadata），按头文件原值手写 ----
const MF_FRIENDLY_NAME_GUID: GUID = GUID::from_u128(0x314ffbae_5b41_4c95_9c19_4e7d586face3);
const MF_TRANSFORM_ASYNC_GUID: GUID = GUID::from_u128(0xf81a699a_649a_497d_8c73_29f8fed6ad7a);
const MF_TRANSFORM_ASYNC_UNLOCK_GUID: GUID =
    GUID::from_u128(0xe5666d6b_3422_4eb6_a421_da7db1ff5a3c);

#[derive(Clone, Copy)]
struct Cfg {
    w: u32,
    h: u32,
    fps: u32,
    frames: u32,
    bitrate: u32,
}

fn env_u32(k: &str, d: u32) -> u32 {
    std::env::var(k).ok().and_then(|v| v.parse().ok()).unwrap_or(d)
}

impl Cfg {
    fn from_env() -> Self {
        Self {
            w: env_u32("MF_W", 1280),
            h: env_u32("MF_H", 720),
            fps: env_u32("MF_FPS", 60),
            frames: env_u32("MF_FRAMES", 120),
            bitrate: env_u32("MF_BITRATE", 4_000_000),
        }
    }
}

struct Stats {
    ms: f64,
    total: usize,
    first: usize,
    nal: bool,
    key_first: bool,
    path: String,
}

fn main() {
    let code = unsafe { run() };
    std::process::exit(code);
}

unsafe fn run() -> i32 {
    let c = Cfg::from_env();
    eprintln!(
        "[v2] 目标配置 {}x{} @{}fps ×{}帧 码率{}bps",
        c.w, c.h, c.fps, c.frames, c.bitrate
    );

    let hr = CoInitializeEx(None, COINIT_MULTITHREADED);
    eprintln!("[v2] CoInitializeEx(MTA) -> {hr:?}");
    if let Err(e) = MFStartup(MF_VERSION, MFSTARTUP_FULL) {
        eprintln!("[v2] MFStartup 失败: {e}");
        return 2;
    }

    let (device, context) = match d3d() {
        Ok(v) => v,
        Err(e) => {
            eprintln!("[v2] D3D11 创建失败: {e}");
            return 2;
        }
    };
    let mgr = match device_manager(&device) {
        Ok(m) => m,
        Err(e) => {
            eprintln!("[v2] 设备管理器失败: {e}");
            return 2;
        }
    };
    eprintln!("[v2] D3D11 设备 + IMFDXGIDeviceManager 就绪");

    let mut any_ok = false;

    // ---------------- V1：不带 ASYNCMFT（看 MF 是否给同步包装） ----------------
    eprintln!("\n########## V1：MFTEnumEx(HARDWARE|SORTANDFILTER) 不带 ASYNCMFT ##########");
    let mut acts: *mut Option<IMFActivate> = std::ptr::null_mut();
    let mut n: u32 = 0;
    let flags1 = MFT_ENUM_FLAG_HARDWARE | MFT_ENUM_FLAG_SORTANDFILTER;
    match MFTEnumEx(MFT_CATEGORY_VIDEO_ENCODER, flags1, None, Some(&out_info()), &mut acts, &mut n) {
        Ok(()) => {
            eprintln!("[v2] 枚举到硬件编码器 {n} 个（不含 ASYNCMFT 标志）");
            for i in 0..n as usize {
                let Some(a) = (*(acts.add(i))).as_ref() else { continue };
                eprintln!("\n[v2] ===== V1[{i}] {} =====", friendly_name(a));
                let unlock_rc = a.SetUINT32(&MF_TRANSFORM_ASYNC_UNLOCK_GUID, 1);
                eprintln!("[v2]   activate.SetUINT32(UNLOCK)= {:?}", unlock_rc.is_ok());
                match a.ActivateObject::<IMFTransform>() {
                    Ok(t) => {
                        if try_all_styles(&t, &mgr, &device, &context, &c, "V1") {
                            any_ok = true;
                        }
                    }
                    Err(e) => eprintln!("[v2]   ActivateObject 失败: {e}"),
                }
            }
        }
        Err(e) => eprintln!("[v2] V1 枚举失败: {e}"),
    }

    // ---------------- V2：ASYNCMFT + 解锁（打印返回值，看解锁到底进没进去） ----------------
    eprintln!("\n########## V2：MFTEnumEx(+ASYNCMFT) + activate 解锁 ##########");
    let mut acts2: *mut Option<IMFActivate> = std::ptr::null_mut();
    let mut n2: u32 = 0;
    let flags2 = MFT_ENUM_FLAG_HARDWARE | MFT_ENUM_FLAG_ASYNCMFT | MFT_ENUM_FLAG_SORTANDFILTER;
    match MFTEnumEx(MFT_CATEGORY_VIDEO_ENCODER, flags2, None, Some(&out_info()), &mut acts2, &mut n2)
    {
        Ok(()) => {
            eprintln!("[v2] 枚举到 {n2} 个（含 ASYNCMFT）");
            for i in 0..n2 as usize {
                let Some(a) = (*(acts2.add(i))).as_ref() else { continue };
                eprintln!("\n[v2] ===== V2[{i}] {} =====", friendly_name(a));
                let rc = a.SetUINT32(&MF_TRANSFORM_ASYNC_UNLOCK_GUID, 1);
                eprintln!(
                    "[v2]   activate.SetUINT32(UNLOCK) -> {:?} (err={:?})",
                    rc.is_ok(),
                    rc.err()
                );
                match a.ActivateObject::<IMFTransform>() {
                    Ok(t) => {
                        dump_attrs(&t, "激活后");
                        if try_all_styles(&t, &mgr, &device, &context, &c, "V2") {
                            any_ok = true;
                        }
                    }
                    Err(e) => eprintln!("[v2]   ActivateObject 失败: {e}"),
                }
            }
        }
        Err(e) => eprintln!("[v2] V2 枚举失败: {e}"),
    }

    // ---------------- V3：CoCreateInstance 旁路 ----------------
    eprintln!("\n########## V3：CoCreateInstance + 实例属性解锁 ##########");
    for (label, clsid) in [("NVIDIA NVENC", CLSID_NVENC), ("Intel QSV", CLSID_QSV)] {
        eprintln!("\n[v2] ===== V3 {label} =====");
        match CoCreateInstance::<_, IMFTransform>(&clsid, None, CLSCTX_INPROC_SERVER) {
            Ok(t) => {
                if let Ok(attrs) = t.GetAttributes() {
                    let rc = attrs.SetUINT32(&MF_TRANSFORM_ASYNC_UNLOCK_GUID, 1);
                    eprintln!(
                        "[v2]   实例属性 SetUINT32(UNLOCK) -> {:?} (err={:?})",
                        rc.is_ok(),
                        rc.err()
                    );
                }
                dump_attrs(&t, "解锁后");
                if try_all_styles(&t, &mgr, &device, &context, &c, "V3") {
                    any_ok = true;
                }
            }
            Err(e) => eprintln!("[v2]   CoCreateInstance 失败: {e}"),
        }
    }

    MFShutdown().ok();
    eprintln!(
        "\n[v2] 结论：{}",
        if any_ok { "至少一条硬编路径可用" } else { "硬编全部不可用" }
    );
    if any_ok {
        0
    } else {
        3
    }
}

fn out_info() -> MFT_REGISTER_TYPE_INFO {
    MFT_REGISTER_TYPE_INFO {
        guidMajorType: MFMediaType_Video,
        guidSubtype: MFVideoFormat_H264,
    }
}

/// 先试同步用法，失败再试异步事件驱动 —— 这样不用事先判断 MF 给的是同步包装还是裸异步 MFT。
unsafe fn try_all_styles(
    t: &IMFTransform,
    mgr: &IMFDXGIDeviceManager,
    device: &ID3D11Device,
    context: &ID3D11DeviceContext,
    c: &Cfg,
    tag: &str,
) -> bool {
    // 1) 先只做类型设置，判定实例到底能不能用（这一步才是 ASYNC_LOCKED 的判据）
    let set = match (set_d3d_manager(t, mgr), set_types(t, c)) {
        (Ok(()), Ok(())) => {
            eprintln!("[v2]   [{tag}] D3D_MANAGER + SetInputType/SetOutputType 全部成功 ★");
            true
        }
        (a, b) => {
            let ea = a.err().unwrap_or_default();
            let eb = b.err().unwrap_or_default();
            eprintln!("[v2]   [{tag}] 类型设置失败：d3d_mgr={ea} types={eb}");
            false
        }
    };
    if !set {
        return false;
    }

    // 2) 同步用法（同步包装 / 同步 MFT）
    match encode(t, c, None, c.frames.min(10)) {
        Ok(s) => eprintln!(
            "[v2]   [{tag}] 同步用法 OK：{:.2}ms/帧，首帧{}字节 NAL={} 关键帧={}",
            s.ms, s.first, s.nal, s.key_first
        ),
        Err(e) => {
            eprintln!("[v2]   [{tag}] 同步用法失败：{e}");
            // 3) 异步用法（裸异步 MFT）
            match encode_async(t, c, c.frames.min(10)) {
                Ok(s) => eprintln!(
                    "[v2]   [{tag}] 异步用法 OK：{:.2}ms/帧，首帧{}字节 NAL={} 关键帧={}",
                    s.ms, s.first, s.nal, s.key_first
                ),
                Err(e2) => {
                    eprintln!("[v2]   [{tag}] 异步用法也失败：{e2}");
                    return false;
                }
            }
        }
    }

    // 4) 满帧计时（CPU 输入）
    if let Ok(s) = encode(t, c, None, c.frames) {
        report("CPU输入", tag, &s);
        save(&s, tag, "cpu");
    }
    // 5) D3D 纹理零拷贝输入
    match encode(t, c, Some((device, context)), c.frames) {
        Ok(s) => {
            report("D3D纹理", tag, &s);
            save(&s, tag, "d3d");
        }
        Err(e) => eprintln!("[v2]   [{tag}] D3D 纹理输入失败：{e}"),
    }
    true
}

fn report(feed: &str, tag: &str, s: &Stats) {
    eprintln!(
        "[v2]   >>> [{tag}/{feed}] {:.2} ms/帧（{:.1} fps 上限），{} 字节总输出，首帧 {} 字节，\
         起始码={}，首帧关键帧={}",
        s.ms,
        1000.0 / s.ms.max(0.001),
        s.total,
        s.first,
        s.nal,
        s.key_first
    );
}

fn save(s: &Stats, tag: &str, feed: &str) {
    eprintln!("[v2]       码流落盘: {}", s.path);
    let _ = (tag, feed);
}

unsafe fn dump_attrs(t: &IMFTransform, when: &str) {
    if let Ok(a) = t.GetAttributes() {
        let rd = |k: &GUID| -> String {
            a.GetUINT32(k)
                .map(|v| v.to_string())
                .unwrap_or_else(|_| "空".into())
        };
        eprintln!(
            "[v2]   {when}属性：ASYNC={} UNLOCK={}",
            rd(&MF_TRANSFORM_ASYNC_GUID),
            rd(&MF_TRANSFORM_ASYNC_UNLOCK_GUID)
        );
    }
}

unsafe fn set_d3d_manager(t: &IMFTransform, mgr: &IMFDXGIDeviceManager) -> Result<(), String> {
    // 先补设一次解锁属性（若前面没设过）：读回值一并打出，便于区分"没设上"和"设上了但无效"
    if let Ok(a) = t.GetAttributes() {
        let _ = a.SetUINT32(&MF_TRANSFORM_ASYNC_UNLOCK_GUID, 1);
        let v = a
            .GetUINT32(&MF_TRANSFORM_ASYNC_UNLOCK_GUID)
            .map(|x| x.to_string())
            .unwrap_or_else(|_| "空".into());
        let asyncv = a
            .GetUINT32(&MF_TRANSFORM_ASYNC_GUID)
            .map(|x| x.to_string())
            .unwrap_or_else(|_| "空".into());
        eprintln!("[v2]   实例属性：ASYNC={asyncv} UNLOCK={v}");
    }
    t.ProcessMessage(MFT_MESSAGE_SET_D3D_MANAGER, mgr.as_raw() as usize)
        .map_err(|e| format!("SET_D3D_MANAGER: {e}"))
}

unsafe fn set_types(t: &IMFTransform, c: &Cfg) -> Result<(), String> {
    let out_type = MFCreateMediaType().map_err(|e| e.to_string())?;
    out_type.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video).map_err(|e| e.to_string())?;
    out_type.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_H264).map_err(|e| e.to_string())?;
    out_type.SetUINT32(&MF_MT_AVG_BITRATE, c.bitrate).map_err(|e| e.to_string())?;
    out_type.SetUINT64(&MF_MT_FRAME_SIZE, pack2(c.w, c.h)).map_err(|e| e.to_string())?;
    out_type.SetUINT64(&MF_MT_FRAME_RATE, pack2(c.fps, 1)).map_err(|e| e.to_string())?;
    let _ = out_type.SetUINT64(&MF_MT_PIXEL_ASPECT_RATIO, pack2(1, 1));
    let _ = out_type.SetUINT32(
        &MF_MT_INTERLACE_MODE,
        MFVideoInterlace_Progressive.0 as u32,
    );
    // 关键：色彩矩阵写进输出类型，编码器会把 VUI 带进 SPS（前端解码才不会偏色）
    let _ = out_type.SetUINT32(&MF_MT_YUV_MATRIX, MFVideoTransferMatrix_BT709.0 as u32);
    let _ = out_type.SetUINT32(&MF_MT_MPEG2_PROFILE, eAVEncH264VProfile_High.0 as u32);
    t.SetOutputType(0, &out_type, 0)
        .map_err(|e| format!("SetOutputType: {e}"))?;

    let in_type = MFCreateMediaType().map_err(|e| e.to_string())?;
    in_type.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video).map_err(|e| e.to_string())?;
    in_type.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_NV12).map_err(|e| e.to_string())?;
    in_type.SetUINT64(&MF_MT_FRAME_SIZE, pack2(c.w, c.h)).map_err(|e| e.to_string())?;
    in_type.SetUINT64(&MF_MT_FRAME_RATE, pack2(c.fps, 1)).map_err(|e| e.to_string())?;
    let _ = in_type.SetUINT64(&MF_MT_PIXEL_ASPECT_RATIO, pack2(1, 1));
    let _ = in_type.SetUINT32(
        &MF_MT_INTERLACE_MODE,
        MFVideoInterlace_Progressive.0 as u32,
    );
    let _ = in_type.SetUINT32(&MF_MT_YUV_MATRIX, MFVideoTransferMatrix_BT709.0 as u32);
    if let Err(e) = t.SetInputType(0, &in_type, 0) {
        for idx in 0..6u32 {
            if let Ok(mt) = t.GetInputAvailableType(0, idx) {
                let sub = mt
                    .GetGUID(&MF_MT_SUBTYPE)
                    .map(|g| format!("{g:?}"))
                    .unwrap_or_else(|_| "?".into());
                let sz = mt.GetUINT64(&MF_MT_FRAME_SIZE).map(unpack2).unwrap_or((0, 0));
                eprintln!("[v2]      可接受输入[{idx}] sub={sub} {}x{}", sz.0, sz.1);
            }
        }
        return Err(format!("SetInputType(NV12): {e}"));
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
unsafe fn encode(
    t: &IMFTransform,
    c: &Cfg,
    d3d: Option<(&ID3D11Device, &ID3D11DeviceContext)>,
    frames: u32,
) -> Result<Stats, String> {
    let _ = t.ProcessMessage(MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, 0);

    let mut provides = false;
    let mut out_size = 0u32;
    if let Ok(info) = t.GetOutputStreamInfo(0) {
        provides = info.dwFlags & MFT_OUTPUT_STREAM_PROVIDES_SAMPLES.0 as u32 != 0;
        out_size = info.cbSize;
    }

    let mut acc = Acc::default();
    let t0 = Instant::now();

    for i in 0..frames {
        let sample = match d3d {
            Some((dev, ctx)) => bgra_to_nv12_d3d_sample(dev, ctx, c, i)?,
            None => cpu_nv12_sample(c, i)?,
        };
        sample
            .SetSampleTime(i as i64 * 10_000_000 / c.fps as i64)
            .map_err(|e| e.to_string())?;
        sample
            .SetSampleDuration(10_000_000 / c.fps as i64)
            .map_err(|e| e.to_string())?;

        // ProcessInput 可能因"还没要输入"返回 MF_E_NOTACCEPTING —— 先取一次输出再重试
        if let Err(e) = t.ProcessInput(0, &sample, 0) {
            let _ = drain(t, provides, out_size, None);
            t.ProcessInput(0, &sample, 0)
                .map_err(|e2| format!("ProcessInput 失败: {e} / 重试: {e2}"))?;
        }
        drain(t, provides, out_size, Some(&mut acc))?;
    }
    // 收尾取净
    let _ = t.ProcessMessage(MFT_MESSAGE_COMMAND_DRAIN, 0);
    let _ = drain(t, provides, out_size, Some(&mut acc));

    let ms = t0.elapsed().as_secs_f64() * 1000.0 / frames.max(1) as f64;
    let path = std::env::temp_dir().join(format!("indesk-hwenc-{}.h264", std::process::id()));
    let _ = std::fs::write(&path, &acc.stream);
    Ok(Stats {
        ms,
        total: acc.total,
        first: acc.first,
        nal: acc.nal,
        key_first: acc.key,
        path: path.display().to_string(),
    })
}

#[derive(Default)]
struct Acc {
    total: usize,
    first: usize,
    nal: bool,
    key: bool,
    stream: Vec<u8>,
}

unsafe fn drain(
    t: &IMFTransform,
    provides: bool,
    out_size: u32,
    mut acc: Option<&mut Acc>,
) -> Result<u32, String> {
    let mut got = 0u32;
    loop {
        let mut data = MFT_OUTPUT_DATA_BUFFER::default();
        if !provides {
            let s = MFCreateSample().map_err(|e| e.to_string())?;
            let b = MFCreateMemoryBuffer(out_size.max(1 << 16)).map_err(|e| e.to_string())?;
            s.AddBuffer(&b).map_err(|e| e.to_string())?;
            data.pSample = std::mem::ManuallyDrop::new(Some(s));
        }
        let mut status = 0u32;
        match t.ProcessOutput(0, std::slice::from_mut(&mut data), &mut status) {
            Ok(()) => {
                if let Some(sample) = data.pSample.take() {
                    if let Some(a) = acc.as_mut() {
                        if let Ok(buf) = sample.ConvertToContiguousBuffer() {
                            let mut ptr = std::ptr::null_mut();
                            let mut cur = 0u32;
                            if buf.Lock(&mut ptr, None, Some(&mut cur)).is_ok() {
                                if !ptr.is_null() && cur > 0 {
                                    let bytes = std::slice::from_raw_parts(ptr, cur as usize);
                                    if a.total == 0 {
                                        a.first = cur as usize;
                                        a.nal = has_nal(bytes);
                                        a.key = sample
                                            .GetUINT32(&MFSampleExtension_CleanPoint)
                                            .map(|v| v != 0)
                                            .unwrap_or(false);
                                    }
                                    a.total += cur as usize;
                                    if a.stream.len() < (16 << 20) {
                                        a.stream.extend_from_slice(bytes);
                                    }
                                }
                                let _ = buf.Unlock();
                            }
                        }
                    }
                }
                got += 1;
            }
            Err(e) if e.code() == MF_E_TRANSFORM_NEED_MORE_INPUT => break,
            Err(e) => return Err(format!("ProcessOutput: {e}")),
        }
    }
    Ok(got)
}

/// 异步用法（裸异步 MFT 的事件驱动形态）。
unsafe fn encode_async(t: &IMFTransform, c: &Cfg, frames: u32) -> Result<Stats, String> {
    t.ProcessMessage(MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, 0)
        .map_err(|e| e.to_string())?;
    let _ = t.ProcessMessage(MFT_MESSAGE_NOTIFY_START_OF_STREAM, 0);
    let gen: IMFMediaEventGenerator = t.cast().map_err(|e| format!("事件生成器: {e}"))?;

    let mut sent = 0u32;
    let mut got = 0u32;
    let mut total = 0usize;
    let mut first = 0usize;
    let mut nal = false;
    let mut key_first = false;
    let mut stream: Vec<u8> = Vec::with_capacity(1 << 21);
    let t0 = Instant::now();

    while got < frames {
        let ev = gen
            .GetEvent(MEDIA_EVENT_GENERATOR_GET_EVENT_FLAGS(0))
            .map_err(|e| format!("GetEvent: {e}"))?;
        let ty = ev.GetType().map_err(|e| e.to_string())?;
        if ty == METransformNeedInput.0 as u32 {
            if sent < frames {
                let s = cpu_nv12_sample(c, sent)?;
                s.SetSampleTime(sent as i64 * 10_000_000 / c.fps as i64).ok();
                s.SetSampleDuration(10_000_000 / c.fps as i64).ok();
                t.ProcessInput(0, &s, 0).map_err(|e| format!("ProcessInput: {e}"))?;
                sent += 1;
            } else {
                let _ = t.ProcessMessage(MFT_MESSAGE_COMMAND_DRAIN, 0);
            }
        } else if ty == METransformHaveOutput.0 as u32 {
            let mut data = MFT_OUTPUT_DATA_BUFFER::default();
            let mut status = 0u32;
            t.ProcessOutput(0, std::slice::from_mut(&mut data), &mut status)
                .map_err(|e| format!("ProcessOutput: {e}"))?;
            if let Some(sample) = data.pSample.take() {
                if let Ok(buf) = sample.ConvertToContiguousBuffer() {
                    let mut ptr = std::ptr::null_mut();
                    let mut cur = 0u32;
                    if buf.Lock(&mut ptr, None, Some(&mut cur)).is_ok() {
                        if !ptr.is_null() && cur > 0 {
                            let bytes = std::slice::from_raw_parts(ptr, cur as usize);
                            if total == 0 {
                                first = cur as usize;
                                nal = has_nal(bytes);
                                key_first = sample
                                    .GetUINT32(&MFSampleExtension_CleanPoint)
                                    .map(|v| v != 0)
                                    .unwrap_or(false);
                            }
                            total += cur as usize;
                            if stream.len() < (16 << 20) {
                                stream.extend_from_slice(bytes);
                            }
                        }
                        let _ = buf.Unlock();
                    }
                }
            }
            got += 1;
        } else if ty == METransformDrainComplete.0 as u32 {
            break;
        }
    }
    let ms = t0.elapsed().as_secs_f64() * 1000.0 / got.max(1) as f64;
    let path = std::env::temp_dir().join(format!("indesk-hwenc-async-{}.h264", std::process::id()));
    let _ = std::fs::write(&path, &stream);
    Ok(Stats { ms, total, first, nal, key_first, path: path.display().to_string() })
}

// ---------------- 造输入样本 ----------------

unsafe fn cpu_nv12_sample(c: &Cfg, i: u32) -> Result<IMFSample, String> {
    let len = (c.w * c.h * 3 / 2) as usize;
    let buf = MFCreateMemoryBuffer(len as u32).map_err(|e| e.to_string())?;
    let mut ptr = std::ptr::null_mut();
    buf.Lock(&mut ptr, None, None).map_err(|e| e.to_string())?;
    fill_nv12(ptr as *mut u8, c.w, c.h, i);
    buf.Unlock().map_err(|e| e.to_string())?;
    buf.SetCurrentLength(len as u32).map_err(|e| e.to_string())?;
    let s = MFCreateSample().map_err(|e| e.to_string())?;
    s.AddBuffer(&buf).map_err(|e| e.to_string())?;
    Ok(s)
}

unsafe fn bgra_to_nv12_d3d_sample(
    dev: &ID3D11Device,
    ctx: &ID3D11DeviceContext,
    c: &Cfg,
    i: u32,
) -> Result<IMFSample, String> {
    // 纹理：VideoProcessor 的输出也带 RENDER_TARGET；SHADER_RESOURCE 一起给上（MFCreateDXGISurfaceBuffer 要求）
    let desc = D3D11_TEXTURE2D_DESC {
        Width: c.w,
        Height: c.h,
        MipLevels: 1,
        ArraySize: 1,
        Format: DXGI_FORMAT_NV12,
        SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
        Usage: D3D11_USAGE_DEFAULT,
        BindFlags: D3D11_BIND_RENDER_TARGET.0 as u32 | D3D11_BIND_SHADER_RESOURCE.0 as u32,
        CPUAccessFlags: 0,
        MiscFlags: 0,
    };
    let mut tex: Option<ID3D11Texture2D> = None;
    dev.CreateTexture2D(&desc, None, Some(&mut tex))
        .map_err(|e| format!("CreateTexture2D: {e}"))?;
    let tex = tex.ok_or_else(|| "纹理为空".to_string())?;

    // 上传一帧 NV12（产品里这步由 VideoProcessor 在 GPU 内完成，探针只为验证编码器吃不吃纹理）
    let len = (c.w * c.h * 3 / 2) as usize;
    let mut nv12 = vec![0u8; len];
    fill_nv12(nv12.as_mut_ptr(), c.w, c.h, i);
    let res: ID3D11Resource = tex.cast().map_err(|e| e.to_string())?;
    ctx.UpdateSubresource(&res, 0, None, nv12.as_ptr() as *const core::ffi::c_void, c.w, c.w * c.h);

    let unk: IUnknown = tex.cast().map_err(|e| e.to_string())?;
    let buf = MFCreateDXGISurfaceBuffer(&ID3D11Texture2D::IID, &unk, 0, false)
        .map_err(|e| format!("MFCreateDXGISurfaceBuffer: {e}"))?;
    let s = MFCreateSample().map_err(|e| e.to_string())?;
    s.AddBuffer(&buf).map_err(|e| e.to_string())?;
    Ok(s)
}

/// 生成一帧 NV12 测试图（Y 平面渐变 + 移动亮块，UV 恒定 128）。
unsafe fn fill_nv12(p: *mut u8, w: u32, h: u32, i: u32) {
    for y in 0..h {
        for x in 0..w {
            *p.add((y * w + x) as usize) = ((x + y + i * 4) & 0xff) as u8;
        }
    }
    let span = w.saturating_sub(200).max(1);
    let bx = (i * 11) % span;
    for y in 60..240.min(h) {
        for x in bx..(bx + 200).min(w) {
            *p.add((y * w + x) as usize) = 235;
        }
    }
    std::ptr::write_bytes(p.add((w * h) as usize), 128, (w * h / 2) as usize);
}

unsafe fn d3d() -> Result<(ID3D11Device, ID3D11DeviceContext), String> {
    let levels = [
        D3D_FEATURE_LEVEL_11_0,
        D3D_FEATURE_LEVEL_10_1,
        D3D_FEATURE_LEVEL_10_0,
    ];
    let mut dev: Option<ID3D11Device> = None;
    let mut ctx: Option<ID3D11DeviceContext> = None;
    D3D11CreateDevice(
        None,
        D3D_DRIVER_TYPE_HARDWARE,
        HMODULE::default(),
        D3D11_CREATE_DEVICE_BGRA_SUPPORT,
        Some(&levels),
        D3D11_SDK_VERSION,
        Some(&mut dev),
        None,
        Some(&mut ctx),
    )
    .map_err(|e| format!("D3D11CreateDevice: {e}"))?;
    Ok((
        dev.ok_or("设备为空")?,
        ctx.ok_or("上下文为空")?,
    ))
}

unsafe fn device_manager(dev: &ID3D11Device) -> Result<IMFDXGIDeviceManager, String> {
    let mut token = 0u32;
    let mut mgr: Option<IMFDXGIDeviceManager> = None;
    MFCreateDXGIDeviceManager(&mut token, &mut mgr).map_err(|e| e.to_string())?;
    let mgr = mgr.ok_or_else(|| "管理器为空".to_string())?;
    mgr.ResetDevice(dev, token).map_err(|e| e.to_string())?;
    Ok(mgr)
}

unsafe fn friendly_name(a: &IMFActivate) -> String {
    let l = a.GetStringLength(&MF_FRIENDLY_NAME_GUID).unwrap_or(0);
    if l == 0 {
        return "(无名字)".into();
    }
    let mut buf = vec![0u16; (l + 1) as usize];
    match a.GetString(&MF_FRIENDLY_NAME_GUID, &mut buf, None) {
        Ok(()) => String::from_utf16_lossy(&buf[..l as usize]),
        Err(_) => "(读名字失败)".into(),
    }
}

fn pack2(hi: u32, lo: u32) -> u64 {
    ((hi as u64) << 32) | lo as u64
}

fn unpack2(v: u64) -> (u32, u32) {
    ((v >> 32) as u32, (v & 0xffff_ffff) as u32)
}

fn has_nal(b: &[u8]) -> bool {
    b.windows(4).any(|w| w == [0, 0, 0, 1]) || b.windows(3).any(|w| w == [0, 0, 1])
}
