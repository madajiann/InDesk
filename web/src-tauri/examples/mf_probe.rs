//! 硬件 H.264 编码可行性探针（开发用，不进安装包）。
//!
//! ```text
//! cargo run --example mf_probe
//! ```
//!
//! ## 这个探针要回答什么
//!
//! InDesk 1.4.0「换引擎」的目标是**取消 GPU→CPU 回读**：现在 `dxgi.rs` 每帧把
//! 8.8MB 的 BGRA 整帧 `Map` 回 CPU（实测约 50ms/帧），这是 20fps 天花板的成因。
//! 正确做法是让像素留在 GPU 上做完缩放与色彩转换、直接交给硬件编码器，
//! CPU 只接收压缩后的码流（约 0.25MB）。
//!
//! ## 硬件 MFT 的几道门槛（都是实跑撞出来的）
//!
//! 1. **COM 单元必须是 MTA**：Media Foundation 明确要求 MTA，
//!    `CoInitializeEx(COINIT_MULTITHREADED)`。用 STA 会在跨单元调用上出问题。
//! 2. **异步解锁**：硬件编码器 MFT 都是异步 MFT，必须把 `MF_TRANSFORM_ASYNC_UNLOCK`
//!    置 1。注意官方文档的**例外列表**：未解锁时 `SetOutputType`（仅编码器）与
//!    `GetAttributes` / `GetInputAvailableType` **仍可成功** ——
//!    所以"SetOutputType 成功"**不能**证明解锁已生效，只有 `SetInputType` 才能证明。
//! 3. **D3D 设备管理器**：硬件 MFT 要直接访问 GPU，需先
//!    `MFT_MESSAGE_SET_D3D_MANAGER` 交付一个 `IMFDXGIDeviceManager`。
//! 4. 上述都过了才轮到 SetInputType(NV12) → 事件驱动编码。
//!
//! ## 判定
//!
//! - 能编出含 NAL 起始码的码流 → 方案成立，后续只是工程量（D3D11 VideoProcessor
//!   做 BGRA→NV12 + 缩放，用 DXGI surface 直喂编码器，彻底不落 CPU）。
//! - 编不出来 → 退路是「GPU 缩放 + 只回读目标尺寸的 NV12」（回读量降到 1/3，帧率约翻倍）。

use std::time::Instant;

use windows::core::{GUID, Interface};
use windows::Win32::Foundation::HMODULE;
use windows::Win32::Graphics::Direct3D::{
    D3D_DRIVER_TYPE_HARDWARE, D3D_FEATURE_LEVEL_10_0, D3D_FEATURE_LEVEL_10_1,
    D3D_FEATURE_LEVEL_11_0,
};
use windows::Win32::Graphics::Direct3D11::{
    D3D11CreateDevice, ID3D11Device, D3D11_CREATE_DEVICE_BGRA_SUPPORT, D3D11_SDK_VERSION,
};
use windows::Win32::Media::MediaFoundation::*;
use windows::Win32::System::Com::{
    CoCreateInstance, CoInitializeEx, CLSCTX_INPROC_SERVER, COINIT_MULTITHREADED,
};

const W: u32 = 1920;
const H: u32 = 1080;
const FPS: u32 = 30;
const FRAMES: u32 = 90;
const BITRATE: u32 = 8_000_000;

/// `{60F44560-5A20-4857-BFEF-D29773CB8040}` NVIDIA H.264 Encoder MFT
const CLSID_NVENC: GUID = GUID::from_u128(0x60f44560_5a20_4857_bfef_d29773cb8040);
/// `{4BE8D3C0-0515-4A37-AD55-E4BAE19AF471}` Intel Quick Sync Video H.264 Encoder MFT
const CLSID_QSV: GUID = GUID::from_u128(0x4be8d3c0_0515_4a37_ad55_e4bae19af471);

// ---- 属性 GUID：windows-rs 0.61 未生成（mfobjects.h 的 DEFINE_GUID 未进 metadata），
//      按 SDK 头文件原值手写 ----
/// `MF_FRIENDLY_NAME` = `{314FFBAE-5B41-4C95-9C19-4E7D586FACE3}`
const MF_FRIENDLY_NAME_GUID: GUID = GUID::from_u128(0x314ffbae_5b41_4c95_9c19_4e7d586face3);
/// `MF_TRANSFORM_ASYNC` = `{F81A699A-649A-497D-8C73-29F8FED6AD7A}`
const MF_TRANSFORM_ASYNC_GUID: GUID = GUID::from_u128(0xf81a699a_649a_497d_8c73_29f8fed6ad7a);
/// `MF_TRANSFORM_ASYNC_UNLOCK` = `{E5666D6B-3422-4EB6-A421-DA7DB1FF5A3C}`
const MF_TRANSFORM_ASYNC_UNLOCK_GUID: GUID =
    GUID::from_u128(0xe5666d6b_3422_4eb6_a421_da7db1ff5a3c);

fn pack2(hi: u32, lo: u32) -> u64 {
    ((hi as u64) << 32) | lo as u64
}

fn main() {
    let code = unsafe { run() };
    std::process::exit(code);
}

unsafe fn run() -> i32 {
    // Media Foundation 要求 MTA（STA 会在跨单元调用上出问题）
    let hr = CoInitializeEx(None, COINIT_MULTITHREADED);
    eprintln!("[mf] CoInitializeEx(MTA) -> {hr:?}");
    if let Err(e) = MFStartup(MF_VERSION, MFSTARTUP_FULL) {
        eprintln!("[mf] MFStartup 失败: {e}");
        return 2;
    }
    eprintln!("[mf] MFStartup OK");

    let mgr = match make_d3d_manager() {
        Ok(m) => {
            eprintln!("[mf] D3D11 设备 + IMFDXGIDeviceManager 就绪");
            m
        }
        Err(e) => {
            eprintln!("[mf] D3D 设备管理器创建失败: {e}");
            MFShutdown().ok();
            return 2;
        }
    };

    let mut rc = 3;

    // ================= 路线 A：MFTEnumEx + IMFActivate::ActivateObject =================
    eprintln!("\n########## 路线 A：MFTEnumEx + ActivateObject ##########");
    let out_info = MFT_REGISTER_TYPE_INFO {
        guidMajorType: MFMediaType_Video,
        guidSubtype: MFVideoFormat_H264,
    };
    let mut pp: *mut Option<IMFActivate> = std::ptr::null_mut();
    let mut n: u32 = 0;
    // MFT_ENUM_FLAG_ASYNCMFT 是关键：官方文档「异步 MFT」一节明确写
    // "若要枚举异步 MFT，请调用 MFTEnumEx 并在 Flags 参数中设置 MFT_ENUM_FLAG_ASYNCMFT"。
    // 不带这个标志枚举出来的实例是"未启用异步"的，后续 SetInputType 会一直被
    // MF_E_TRANSFORM_ASYNC_LOCKED 挡住（即使 UNLOCK 属性读回已是 1）。
    let flags = MFT_ENUM_FLAG_HARDWARE | MFT_ENUM_FLAG_SORTANDFILTER | MFT_ENUM_FLAG_ASYNCMFT;
    if let Err(e) = MFTEnumEx(
        MFT_CATEGORY_VIDEO_ENCODER,
        flags,
        None,
        Some(&out_info),
        &mut pp,
        &mut n,
    ) {
        eprintln!("[mf] MFTEnumEx 失败: {e}");
    } else {
        eprintln!("[mf] 硬件 H.264 编码器 MFT 数量 = {n}");
        let acts: &[Option<IMFActivate>] = std::slice::from_raw_parts(pp, n as usize);
        for (i, a) in acts.iter().enumerate() {
            let Some(activate) = a.as_ref() else { continue };
            eprintln!(
                "\n[mf] ===== A[{i}] {} =====",
                friendly_name(activate)
            );
            let _ = activate.SetUINT32(&MF_TRANSFORM_ASYNC_UNLOCK_GUID, 1);
            match activate.ActivateObject::<IMFTransform>() {
                Ok(t) => match probe_transform(&t, &mgr) {
                    Ok(r) => {
                        report_ok(&r);
                        rc = 0;
                    }
                    Err(e) => eprintln!("[mf] 失败：{e}"),
                },
                Err(e) => eprintln!("[mf] ActivateObject 失败: {e}"),
            }
        }
    }

    // ================= 路线 B：CoCreateInstance + 已知 CLSID =================
    eprintln!("\n########## 路线 B：CoCreateInstance（已知 CLSID）##########");
    for (label, clsid) in [("NVIDIA NVENC", CLSID_NVENC), ("Intel QSV", CLSID_QSV)] {
        eprintln!("\n[mf] ===== B {label} =====");
        match CoCreateInstance::<_, IMFTransform>(&clsid, None, CLSCTX_INPROC_SERVER) {
            Ok(t) => match probe_transform(&t, &mgr) {
                Ok(r) => {
                    report_ok(&r);
                    rc = 0;
                }
                Err(e) => eprintln!("[mf] 失败：{e}"),
            },
            Err(e) => eprintln!("[mf] CoCreateInstance 失败: {e}"),
        }
    }

    MFShutdown().ok();
    eprintln!("\n[mf] 结论：{}", if rc == 0 { "硬编可用" } else { "硬编不可用" });
    rc
}

fn report_ok(r: &ProbeOk) {
    eprintln!(
        "[mf] 硬编成功：{} 帧用时 {:.2}ms/帧（等效 {:.1}fps），输出 {} 字节，\
         首帧 {} 字节，NAL 起始码 {}",
        FRAMES,
        r.ms_per_frame,
        if r.ms_per_frame > 0.0 { 1000.0 / r.ms_per_frame } else { 0.0 },
        r.total,
        r.first_bytes,
        if r.nal_ok { "存在" } else { "缺失(可疑)" }
    );
    eprintln!("[mf]    码流已落盘: {}", r.path);
}

unsafe fn make_d3d_manager() -> Result<IMFDXGIDeviceManager, String> {
    let levels = [
        D3D_FEATURE_LEVEL_11_0,
        D3D_FEATURE_LEVEL_10_1,
        D3D_FEATURE_LEVEL_10_0,
    ];
    let mut dev: Option<ID3D11Device> = None;
    D3D11CreateDevice(
        None,
        D3D_DRIVER_TYPE_HARDWARE,
        HMODULE::default(),
        D3D11_CREATE_DEVICE_BGRA_SUPPORT,
        Some(&levels),
        D3D11_SDK_VERSION,
        Some(&mut dev),
        None,
        None,
    )
    .map_err(|e| format!("D3D11CreateDevice 失败: {e}"))?;
    let dev = dev.ok_or_else(|| "D3D11 设备为空".to_string())?;

    let mut token: u32 = 0;
    let mut mgr: Option<IMFDXGIDeviceManager> = None;
    MFCreateDXGIDeviceManager(&mut token, &mut mgr)
        .map_err(|e| format!("MFCreateDXGIDeviceManager 失败: {e}"))?;
    let mgr = mgr.ok_or_else(|| "DXGI 设备管理器为空".to_string())?;
    mgr.ResetDevice(&dev, token)
        .map_err(|e| format!("IMFDXGIDeviceManager::ResetDevice 失败: {e}"))?;
    Ok(mgr)
}

unsafe fn friendly_name(a: &IMFActivate) -> String {
    let l = a.GetStringLength(&MF_FRIENDLY_NAME_GUID).unwrap_or(0);
    if l == 0 {
        return "(无名字)".to_string();
    }
    let mut buf = vec![0u16; (l + 1) as usize];
    match a.GetString(&MF_FRIENDLY_NAME_GUID, &mut buf, None) {
        Ok(()) => String::from_utf16_lossy(&buf[..l as usize]),
        Err(_) => "(读名字失败)".into(),
    }
}

struct ProbeOk {
    ms_per_frame: f64,
    first_bytes: usize,
    nal_ok: bool,
    total: usize,
    path: String,
}

/// 对已创建的 IMFTransform 走完整流程：解锁 → D3D manager → 类型 → 编码。
unsafe fn probe_transform(
    transform: &IMFTransform,
    mgr: &IMFDXGIDeviceManager,
) -> Result<ProbeOk, String> {
    // ---- 门槛 2：解锁异步（读前/读后对照，以判定属性是否真的进了 MFT 的存储）----
    {
        let attrs: IMFAttributes = transform
            .GetAttributes()
            .map_err(|e| format!("取 IMFAttributes 失败: {e}"))?;
        let rd = |k: &GUID| -> String {
            attrs
                .GetUINT32(k)
                .map(|v| v.to_string())
                .unwrap_or_else(|_| "空".into())
        };
        let is_async = rd(&MF_TRANSFORM_ASYNC_GUID);
        let before = rd(&MF_TRANSFORM_ASYNC_UNLOCK_GUID);
        let set = attrs.SetUINT32(&MF_TRANSFORM_ASYNC_UNLOCK_GUID, 1);
        let after = rd(&MF_TRANSFORM_ASYNC_UNLOCK_GUID);
        eprintln!(
            "[mf]   ASYNC={is_async}  UNLOCK 前={before}  SetUINT32={:?}  后={after}",
            set.map(|_| "OK")
        );
    }

    // ---- 门槛 3：把 D3D 设备管理器交给 MFT ----
    match transform.ProcessMessage(MFT_MESSAGE_SET_D3D_MANAGER, mgr.as_raw() as usize) {
        Ok(()) => eprintln!("[mf]   SET_D3D_MANAGER OK"),
        Err(e) => eprintln!("[mf]   SET_D3D_MANAGER 失败: {e}"),
    }

    // ---- 输出类型（H.264）----
    let out_type: IMFMediaType = MFCreateMediaType().map_err(|e| format!("MFCreateMediaType: {e}"))?;
    set_guid(&out_type, &MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
    set_guid(&out_type, &MF_MT_SUBTYPE, &MFVideoFormat_H264)?;
    set_u32(&out_type, &MF_MT_AVG_BITRATE, BITRATE)?;
    set_u64(&out_type, &MF_MT_FRAME_SIZE, pack2(W, H))?;
    set_u64(&out_type, &MF_MT_FRAME_RATE, pack2(FPS, 1))?;
    let _ = set_u64(&out_type, &MF_MT_PIXEL_ASPECT_RATIO, pack2(1, 1));
    let _ = set_u32(&out_type, &MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32);
    let _ = set_u32(&out_type, &MF_MT_MPEG2_PROFILE, eAVEncH264VProfile_Main.0 as u32);
    transform
        .SetOutputType(0, &out_type, 0)
        .map_err(|e| format!("SetOutputType(H264) 失败: {e}"))?;
    eprintln!("[mf]   SetOutputType(H264 {W}x{H} {FPS}fps) OK");

    // ---- 输入类型（NV12）----
    let in_type: IMFMediaType = MFCreateMediaType().map_err(|e| format!("MFCreateMediaType: {e}"))?;
    set_guid(&in_type, &MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
    set_guid(&in_type, &MF_MT_SUBTYPE, &MFVideoFormat_NV12)?;
    set_u64(&in_type, &MF_MT_FRAME_SIZE, pack2(W, H))?;
    set_u64(&in_type, &MF_MT_FRAME_RATE, pack2(FPS, 1))?;
    let _ = set_u64(&in_type, &MF_MT_PIXEL_ASPECT_RATIO, pack2(1, 1));
    let _ = set_u32(&in_type, &MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32);
    if let Err(e) = transform.SetInputType(0, &in_type, 0) {
        eprintln!("[mf]   SetInputType(NV12) 失败: {e}");
        for idx in 0..8u32 {
            match transform.GetInputAvailableType(0, idx) {
                Ok(mt) => {
                    let sub = mt
                        .GetGUID(&MF_MT_SUBTYPE)
                        .map(|g| format!("{g:?}"))
                        .unwrap_or_else(|_| "?".into());
                    let sz = mt.GetUINT64(&MF_MT_FRAME_SIZE).map(unpack2).unwrap_or((0, 0));
                    eprintln!("[mf]      可接受输入[{idx}] sub={sub} size={}x{}", sz.0, sz.1);
                }
                Err(_) => break,
            }
        }
        return Err(format!("SetInputType(NV12) 失败: {e}"));
    }
    eprintln!("[mf]   SetInputType(NV12) OK");

    let mut si = MFT_INPUT_STREAM_INFO::default();
    let mut pad = 0u32;
    if transform.GetInputStreamInfo(0, &mut si).is_ok() {
        pad = si.cbSize;
    }

    transform
        .ProcessMessage(MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, 0)
        .map_err(|e| format!("BEGIN_STREAMING 失败: {e}"))?;
    let _ = transform.ProcessMessage(MFT_MESSAGE_NOTIFY_START_OF_STREAM, 0);

    let gen: IMFMediaEventGenerator = transform
        .cast()
        .map_err(|e| format!("取 IMFMediaEventGenerator 失败: {e}"))?;

    let nv12_len = (W * H * 3 / 2) as usize + pad as usize;
    let mut sent = 0u32;
    let mut got = 0u32;
    let mut total = 0usize;
    let mut first_bytes = 0usize;
    let mut nal_ok = false;
    let mut stream: Vec<u8> = Vec::with_capacity(1 << 20);
    let mut drained = false;
    let t0 = Instant::now();

    while got < FRAMES {
        let ev = gen
            .GetEvent(MEDIA_EVENT_GENERATOR_GET_EVENT_FLAGS(0))
            .map_err(|e| format!("GetEvent 失败: {e}"))?;
        let ty = ev.GetType().map_err(|e| format!("GetType: {e}"))?;
        let want_input = METransformNeedInput.0 as u32;
        let have_output = METransformHaveOutput.0 as u32;
        let drain_done = METransformDrainComplete.0 as u32;

        if ty == want_input {
            if sent < FRAMES {
                let sample = make_sample(W, H, sent, nv12_len)
                    .map_err(|e| format!("构造输入样本失败: {e}"))?;
                transform
                    .ProcessInput(0, &sample, 0)
                    .map_err(|e| format!("ProcessInput 失败: {e}"))?;
                sent += 1;
            } else if !drained {
                drained = true;
                let _ = transform.ProcessMessage(MFT_MESSAGE_COMMAND_DRAIN, 0);
            }
        } else if ty == have_output {
            let mut data = MFT_OUTPUT_DATA_BUFFER::default();
            let mut status = 0u32;
            transform
                .ProcessOutput(0, std::slice::from_mut(&mut data), &mut status)
                .map_err(|e| format!("ProcessOutput 失败: {e}"))?;
            if let Some(sample) = data.pSample.take() {
                let buf = sample
                    .ConvertToContiguousBuffer()
                    .map_err(|e| format!("取连续缓冲失败: {e}"))?;
                let mut ptr = std::ptr::null_mut();
                let mut cur = 0u32;
                buf.Lock(&mut ptr, None, Some(&mut cur))
                    .map_err(|e| format!("Lock 失败: {e}"))?;
                if !ptr.is_null() && cur > 0 {
                    let bytes = std::slice::from_raw_parts(ptr, cur as usize);
                    if got == 0 {
                        first_bytes = cur as usize;
                        nal_ok = has_nal(bytes);
                    }
                    total += cur as usize;
                    stream.extend_from_slice(bytes);
                }
                let _ = buf.Unlock();
            }
            got += 1;
        } else if ty == drain_done {
            break;
        }
    }

    let ms = t0.elapsed().as_secs_f64() * 1000.0 / got.max(1) as f64;
    let path = std::env::temp_dir().join("indesk-mf-probe.h264");
    let _ = std::fs::write(&path, &stream);
    Ok(ProbeOk {
        ms_per_frame: ms,
        first_bytes,
        nal_ok,
        total,
        path: path.display().to_string(),
    })
}

unsafe fn set_guid(t: &IMFMediaType, key: &GUID, val: &GUID) -> Result<(), String> {
    t.SetGUID(key, val).map_err(|e| format!("SetGUID 失败: {e}"))
}
unsafe fn set_u32(t: &IMFMediaType, key: &GUID, val: u32) -> Result<(), String> {
    t.SetUINT32(key, val).map_err(|e| format!("SetUINT32 失败: {e}"))
}
unsafe fn set_u64(t: &IMFMediaType, key: &GUID, val: u64) -> Result<(), String> {
    t.SetUINT64(key, val).map_err(|e| format!("SetUINT64 失败: {e}"))
}

fn unpack2(v: u64) -> (u32, u32) {
    ((v >> 32) as u32, (v & 0xffff_ffff) as u32)
}

/// 合成一帧 NV12 假画面（Y 平面渐变 + 移动亮块，UV 固定 128）。
unsafe fn make_sample(w: u32, h: u32, i: u32, total_len: usize) -> Result<IMFSample, String> {
    let data_len = w * h * 3 / 2;
    let buf = MFCreateMemoryBuffer(total_len as u32)
        .map_err(|e| format!("MFCreateMemoryBuffer: {e}"))?;
    let mut ptr = std::ptr::null_mut();
    buf.Lock(&mut ptr, None, None).map_err(|e| format!("Lock: {e}"))?;
    let p = ptr as *mut u8;
    for y in 0..h {
        for x in 0..w {
            *p.add((y * w + x) as usize) = ((x + y + i * 4) & 0xff) as u8;
        }
    }
    let span = w.saturating_sub(200).max(1);
    let bx = (i * 11) % span;
    for y in 100..300.min(h) {
        for x in bx..(bx + 200).min(w) {
            *p.add((y * w + x) as usize) = 235;
        }
    }
    std::ptr::write_bytes(p.add((w * h) as usize), 128, (w * h / 2) as usize);
    buf.Unlock().map_err(|e| format!("Unlock: {e}"))?;
    buf.SetCurrentLength(data_len).map_err(|e| format!("SetCurrentLength: {e}"))?;

    let sample = MFCreateSample().map_err(|e| format!("MFCreateSample: {e}"))?;
    sample.AddBuffer(&buf).map_err(|e| format!("AddBuffer: {e}"))?;
    let dur = 10_000_000i64 / FPS as i64;
    sample.SetSampleTime((i as i64) * dur).map_err(|e| format!("SetSampleTime: {e}"))?;
    sample.SetSampleDuration(dur).map_err(|e| format!("SetSampleDuration: {e}"))?;
    Ok(sample)
}

/// Annex-B 起始码：00 00 01 或 00 00 00 01。
fn has_nal(b: &[u8]) -> bool {
    b.windows(4).any(|w| w == [0, 0, 0, 1]) || b.windows(3).any(|w| w == [0, 0, 1])
}
