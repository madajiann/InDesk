//! 1.4.0「换引擎」成本实测探针（一次性，不入交付包）。
//!
//! 目的：把「每帧到底花在哪」变成可复现的数字，不靠估计。
//!
//! 现状路径（src/dxgi.rs::copy_and_scale）每帧做四件事：
//!   ① CopyResource：桌面纹理 → 暂存纹理（暂存纹理是**源分辨率**）
//!   ② Map：把整帧 BGRA 从 GPU 拉回 CPU
//!   ③ memcpy（ow==sw 时）或 CPU box 下采样（ow!=sw 时）
//! 之后 capture.rs 的编码线程再做 ④ JPEG 编码。
//!
//! 本探针把 ①②③④ 分别计时，并给出新路径对照：
//!   * BGRA   —— 全分辨率回读 + CPU memcpy（对应 ②③ 的 memcpy 分支）
//!   * DS     —— 全分辨率回读 + CPU box 下采样（对应 ②③ 的 downsample 分支）
//!   * NV12   —— D3D11 VideoProcessor 在 GPU 内完成 BGRA→NV12 + 缩放，
//!               只把目标尺寸的 NV12 拉回 CPU（取代 ①②③ 整条）
//!   * JPEG   —— jpeg-encoder 在各尺寸/质量下的耗时（对应 ④）
//!
//! 全部用**同一张合成源纹理**反复跑 N 次取均值：源内容恒定，
//! 排除「桌面有没有变化」这个与本次测量无关的干扰变量。
//!
//! 运行：cargo run --release --example engine_proto

use std::time::Instant;

use windows::core::Interface;
use windows::Win32::Foundation::HMODULE;
use windows::Win32::Graphics::Direct3D::{
    D3D_DRIVER_TYPE_HARDWARE, D3D_FEATURE_LEVEL_10_0, D3D_FEATURE_LEVEL_10_1,
    D3D_FEATURE_LEVEL_11_0,
};
use windows::Win32::Graphics::Direct3D11::{
    D3D11CreateDevice, D3D11_BIND_RENDER_TARGET, D3D11_BIND_SHADER_RESOURCE, D3D11_CPU_ACCESS_READ,
    D3D11_CREATE_DEVICE_BGRA_SUPPORT, D3D11_MAPPED_SUBRESOURCE, D3D11_MAP_READ, D3D11_SDK_VERSION,
    D3D11_TEXTURE2D_DESC, D3D11_USAGE, D3D11_USAGE_DEFAULT, D3D11_USAGE_STAGING,
    D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE, D3D11_VIDEO_PROCESSOR_CONTENT_DESC,
    D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC, D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC_0,
    D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC, D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC_0,
    D3D11_VIDEO_PROCESSOR_STREAM, D3D11_VIDEO_USAGE_PLAYBACK_NORMAL, D3D11_VPIV_DIMENSION_TEXTURE2D,
    D3D11_VPOV_DIMENSION_TEXTURE2D, D3D11_TEX2D_VPIV, D3D11_TEX2D_VPOV, ID3D11Device,
    ID3D11DeviceContext, ID3D11Resource, ID3D11Texture2D, ID3D11VideoContext, ID3D11VideoDevice,
    ID3D11VideoProcessor, ID3D11VideoProcessorEnumerator, ID3D11VideoProcessorInputView,
    ID3D11VideoProcessorOutputView,
};
use windows::Win32::Graphics::Dxgi::Common::{
    DXGI_FORMAT, DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_FORMAT_NV12, DXGI_RATIONAL, DXGI_SAMPLE_DESC,
};
use windows::Win32::Graphics::Dxgi::IDXGIAdapter;

/// 源尺寸（= 本机桌面 1680x1050）与两个目标档位。
const SW: u32 = 1680;
const SH: u32 = 1050;
const SRC_PX: usize = (SW * SH) as usize;
const HALF: (u32, u32) = (1280, 800); // 720P 档
const ITER: u32 = 60;

fn px(w: u32, h: u32) -> usize {
    (w * h) as usize
}

/// 与 src/dxgi.rs 完全相同的下采样映射（同一算法才能代表真实成本）。
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

fn downsample(src: &[u8], pitch: usize, m: &ScaleMap, dst: &mut [u8]) {
    for y in 0..m.dh {
        let y0 = m.y0[y] as usize;
        let y1 = m.y1[y] as usize;
        let drow = y * m.dw * 4;
        for x in 0..m.dw {
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

fn avg(v: &[u128]) -> f64 {
    if v.is_empty() {
        return 0.0;
    }
    v.iter().sum::<u128>() as f64 / v.len() as f64
}

fn report(label: &str, us: &[u128], note: &str) {
    let a = avg(us);
    println!(
        "{:<36} {:>8.2} ms/帧   {:>6.1} fps 上限   {}",
        label,
        a / 1000.0,
        if a > 0.0 { 1_000_000.0 / a } else { 0.0 },
        note
    );
}

/// 映射一次并返回 (pData, RowPitch)。调用方负责 Unmap。
unsafe fn map_read(
    context: &ID3D11DeviceContext,
    tex: &ID3D11Texture2D,
) -> Option<(D3D11_MAPPED_SUBRESOURCE, ())> {
    let mut ms: D3D11_MAPPED_SUBRESOURCE = std::mem::zeroed();
    if context.Map(tex, 0, D3D11_MAP_READ, 0, Some(&mut ms)).is_err() {
        return None;
    }
    if ms.pData.is_null() {
        context.Unmap(tex, 0);
        return None;
    }
    Some((ms, ()))
}

fn main() {
    unsafe {
        let levels = [
            D3D_FEATURE_LEVEL_11_0,
            D3D_FEATURE_LEVEL_10_1,
            D3D_FEATURE_LEVEL_10_0,
        ];
        let mut device: Option<ID3D11Device> = None;
        let mut context: Option<ID3D11DeviceContext> = None;
        if let Err(e) = D3D11CreateDevice(
            None::<&IDXGIAdapter>,
            D3D_DRIVER_TYPE_HARDWARE,
            HMODULE::default(),
            D3D11_CREATE_DEVICE_BGRA_SUPPORT,
            Some(&levels),
            D3D11_SDK_VERSION,
            Some(&mut device),
            None,
            Some(&mut context),
        ) {
            println!("D3D11CreateDevice 失败: {e}");
            return;
        }
        let device = device.unwrap();
        let context = context.unwrap();

        let video_device: ID3D11VideoDevice = match device.cast() {
            Ok(v) => v,
            Err(e) => {
                println!("该设备不支持 D3D11 Video（ID3D11VideoDevice 不可用）: {e}");
                return;
            }
        };
        let video_context: ID3D11VideoContext = match context.cast() {
            Ok(v) => v,
            Err(e) => {
                println!("ID3D11VideoContext 不可用: {e}");
                return;
            }
        };

        // ---------------- 合成源纹理（内容恒定，避免桌面变化干扰） ----------------
        let src_tex = match create_tex(
            &device,
            SW,
            SH,
            DXGI_FORMAT_B8G8R8A8_UNORM,
            (D3D11_BIND_RENDER_TARGET.0 | D3D11_BIND_SHADER_RESOURCE.0) as u32,
            D3D11_USAGE_DEFAULT,
            0,
        ) {
            Some(t) => t,
            None => {
                println!("创建源纹理失败");
                return;
            }
        };

        // 灌入非零内容（否则测量可能被零平面优化干扰）
        {
            let mut up = vec![0u8; SRC_PX * 4];
            for i in 0..SRC_PX {
                up[i * 4] = (i % 251) as u8;
                up[i * 4 + 1] = ((i / 1680) % 241) as u8;
                up[i * 4 + 2] = ((i / 7) % 233) as u8;
                up[i * 4 + 3] = 255;
            }
            if let Some(t) = create_tex(
                &device,
                SW,
                SH,
                DXGI_FORMAT_B8G8R8A8_UNORM,
                D3D11_BIND_SHADER_RESOURCE.0 as u32,
                D3D11_USAGE_DEFAULT,
                0,
            ) {
                let tres: ID3D11Resource = t.cast().unwrap();
                context.UpdateSubresource(
                    &tres,
                    0,
                    None,
                    up.as_ptr() as *const core::ffi::c_void,
                    SW * 4,
                    (SRC_PX * 4) as u32,
                );
                context.CopyResource(&src_tex.cast::<ID3D11Resource>().unwrap(), &tres);
            }
        }

        println!("== 1.4.0 换引擎成本实测（源 {SW}x{SH}，{ITER} 次取均值）==");
        // 关键前提：jpeg-encoder 开了 `simd` 特性后走运行期 AVX2 分派，
        // 本机 CPU 到底有没有 AVX2 决定了这一枪有没有收益。此处直接打印事实。
        #[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
        println!(
            "CPU AVX2 可用 = {}（jpeg-encoder simd 路径生效条件）",
            std::arch::is_x86_feature_detected!("avx2")
        );
        println!();

        // ---------------- ① 现状：全分辨率回读 + CPU memcpy ----------------
        let staging_src = match create_tex(
            &device,
            SW,
            SH,
            DXGI_FORMAT_B8G8R8A8_UNORM,
            0,
            D3D11_USAGE_STAGING,
            D3D11_CPU_ACCESS_READ.0 as u32,
        ) {
            Some(s) => s,
            None => {
                println!("创建暂存纹理失败");
                return;
            }
        };
        let src_res: ID3D11Resource = src_tex.cast().unwrap();
        let stg_res: ID3D11Resource = staging_src.cast().unwrap();

        let mut t_copy = Vec::new();
        let mut t_map = Vec::new();
        let mut t_memcpy = Vec::new();
        let mut native_buf: Vec<u8> = Vec::new();
        for _ in 0..ITER {
            let t0 = Instant::now();
            context.CopyResource(&stg_res, &src_res);
            let t1 = Instant::now();
            if let Some((ms, _)) = map_read(&context, &staging_src) {
                let t2 = Instant::now();
                let pitch = ms.RowPitch as usize;
                let len = pitch * (SH as usize - 1) + SW as usize * 4;
                let s = std::slice::from_raw_parts(ms.pData as *const u8, len);
                // 复刻 dxgi.rs 的 memcpy 分支（含 Vec 重新分配）
                native_buf.clear();
                native_buf.extend_from_slice(s);
                let t3 = Instant::now();
                context.Unmap(&staging_src, 0);
                t_copy.push((t1 - t0).as_micros());
                t_map.push((t2 - t1).as_micros());
                t_memcpy.push((t3 - t2).as_micros());
            }
        }
        println!("--- 现状路径拆解（桌面原生 {SW}x{SH}，回读 {:.2}MB）---", (SRC_PX * 4) as f64 / 1048576.0);
        report("① CopyResource(全帧)", &t_copy, "GPU 内拷贝");
        report("② Map 拿指针(全帧 BGRA)", &t_map, "GPU→CPU 同步");
        report("③ memcpy 到 Vec(含分配)", &t_memcpy, "CPU");
        let sum_native: Vec<u128> = (0..t_copy.len())
            .map(|i| t_copy[i] + t_map[i] + t_memcpy[i])
            .collect();
        report("①②③ 合计", &sum_native, "<- 现状原生档抓屏侧");
        println!();

        // ---------------- ② 现状：720P 档（仍是全帧回读 + CPU 下采样） ----------------
        let mut t_ds = Vec::new();
        let map_half = ScaleMap::build(HALF.0 as usize, HALF.1 as usize, SW as usize, SH as usize);
        let mut half_buf = vec![255u8; px(HALF.0, HALF.1) * 4];
        for _ in 0..ITER {
            context.CopyResource(&stg_res, &src_res);
            if let Some((ms, _)) = map_read(&context, &staging_src) {
                let t0 = Instant::now();
                let pitch = ms.RowPitch as usize;
                let len = pitch * (SH as usize - 1) + SW as usize * 4;
                let s = std::slice::from_raw_parts(ms.pData as *const u8, len);
                half_buf.resize(px(HALF.0, HALF.1) * 4, 255);
                downsample(s, pitch, &map_half, &mut half_buf);
                t_ds.push(t0.elapsed().as_micros());
                context.Unmap(&staging_src, 0);
            }
        }
        println!("--- 现状路径 720P 档（回读仍是全帧 7.06MB，另加 CPU 下采样）---");
        report("③' CPU box 下采样 →1280x800", &t_ds, "单线程标量");
        let sum_half: Vec<u128> = (0..t_copy.len())
            .map(|i| t_copy[i] + t_map[i] + t_ds.get(i).copied().unwrap_or(0))
            .collect();
        report("①②③' 合计", &sum_half, "<- 现状 720P 档抓屏侧");
        println!();

        // ---------------- ③ 新路径：VideoProcessor GPU 内 BGRA→NV12 + 缩放 ----------------
        let (ow, oh) = HALF;
        let (ow, oh) = (ow & !1, oh & !1);
        let content = D3D11_VIDEO_PROCESSOR_CONTENT_DESC {
            InputFrameFormat: D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE,
            InputFrameRate: DXGI_RATIONAL {
                Numerator: 30,
                Denominator: 1,
            },
            InputWidth: SW,
            InputHeight: SH,
            OutputFrameRate: DXGI_RATIONAL {
                Numerator: 30,
                Denominator: 1,
            },
            OutputWidth: ow,
            OutputHeight: oh,
            Usage: D3D11_VIDEO_USAGE_PLAYBACK_NORMAL,
        };
        let enumr: ID3D11VideoProcessorEnumerator =
            match video_device.CreateVideoProcessorEnumerator(&content) {
                Ok(e) => e,
                Err(e) => {
                    println!("CreateVideoProcessorEnumerator 失败: {e}");
                    return;
                }
            };
        let bgra_in_res = enumr.CheckVideoProcessorFormat(DXGI_FORMAT_B8G8R8A8_UNORM);
        let nv12_out_res = enumr.CheckVideoProcessorFormat(DXGI_FORMAT_NV12);
        let bgra_in = bgra_in_res.as_ref().ok().copied();
        let nv12_out = nv12_out_res.as_ref().ok().copied();
        let fb = bgra_in.map(|v| v & 1 != 0).unwrap_or(false);
        let fo = nv12_out.map(|v| v & 2 != 0).unwrap_or(false);
        println!(
            "VideoProcessor 能力：BGRA 可作输入={fb}，NV12 可作输出={fo}（原始 {bgra_in:?} / {nv12_out:?}）"
        );
        if let Err(e) = &bgra_in_res {
            println!("  BGRA 能力查询失败: {e}");
        }
        if let Err(e) = &nv12_out_res {
            println!("  NV12 能力查询失败: {e}");
        }
        if !fb || !fo {
            println!("!! 本机 VideoProcessor 不支持该格式组合，新路径不成立（需退回 BGRA 回读方案）");
        }

        let vp: ID3D11VideoProcessor = match video_device.CreateVideoProcessor(&enumr, 0) {
            Ok(v) => v,
            Err(e) => {
                println!("CreateVideoProcessor 失败: {e}");
                return;
            }
        };

        let out_tex = match create_tex(
            &device,
            ow,
            oh,
            DXGI_FORMAT_NV12,
            D3D11_BIND_RENDER_TARGET.0 as u32,
            D3D11_USAGE_DEFAULT,
            0,
        ) {
            Some(t) => t,
            None => {
                println!("创建 NV12 输出纹理失败");
                return;
            }
        };
        let out_view: ID3D11VideoProcessorOutputView = {
            let d = D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC {
                ViewDimension: D3D11_VPOV_DIMENSION_TEXTURE2D,
                Anonymous: D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC_0 {
                    Texture2D: D3D11_TEX2D_VPOV { MipSlice: 0 },
                },
            };
            let mut v: Option<ID3D11VideoProcessorOutputView> = None;
            match video_device.CreateVideoProcessorOutputView(&out_tex, &enumr, &d, Some(&mut v)) {
                Ok(()) => match v {
                    Some(x) => x,
                    None => {
                        println!("输出视图为空");
                        return;
                    }
                },
                Err(e) => {
                    println!("CreateVideoProcessorOutputView 失败: {e}");
                    return;
                }
            }
        };
        let in_view: ID3D11VideoProcessorInputView = {
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
            match video_device.CreateVideoProcessorInputView(&src_tex, &enumr, &d, Some(&mut v)) {
                Ok(()) => match v {
                    Some(x) => x,
                    None => {
                        println!("输入视图为空");
                        return;
                    }
                },
                Err(e) => {
                    println!("CreateVideoProcessorInputView 失败: {e}");
                    return;
                }
            }
        };

        let nv12_staging = match create_tex(
            &device,
            ow,
            oh,
            DXGI_FORMAT_NV12,
            0,
            D3D11_USAGE_STAGING,
            D3D11_CPU_ACCESS_READ.0 as u32,
        ) {
            Some(s) => s,
            None => {
                println!("创建 NV12 暂存纹理失败");
                return;
            }
        };
        let nv12_res: ID3D11Resource = nv12_staging.cast().unwrap();
        let nv12_out_res: ID3D11Resource = out_tex.cast().unwrap();
        let nv12_len = px(ow, oh) * 3 / 2;

        let mut t_vp = Vec::new();
        let mut t_vp_copy = Vec::new();
        let mut t_vp_map = Vec::new();
        let mut t_vp_memcpy = Vec::new();
        for _ in 0..ITER {
            let mut stream = D3D11_VIDEO_PROCESSOR_STREAM {
                Enable: true.into(),
                OutputIndex: 0,
                InputFrameOrField: 0,
                PastFrames: 0,
                FutureFrames: 0,
                ..Default::default()
            };
            stream.pInputSurface = core::mem::ManuallyDrop::new(Some(in_view.clone()));
            let t0 = Instant::now();
            let r = video_context.VideoProcessorBlt(&vp, &out_view, 0, &[stream]);
            let t1 = Instant::now();
            if let Err(e) = r {
                println!("VideoProcessorBlt 失败: {e}");
                return;
            }
            context.CopyResource(&nv12_res, &nv12_out_res);
            let t2 = Instant::now();
            match map_read(&context, &nv12_staging) {
                Some((ms, _)) => {
                    let t3 = Instant::now();
                    let s = std::slice::from_raw_parts(ms.pData as *const u8, nv12_len);
                    let mut nv12_buf: Vec<u8> = Vec::with_capacity(nv12_len);
                    nv12_buf.extend_from_slice(s);
                    let t4 = Instant::now();
                    context.Unmap(&nv12_staging, 0);
                    t_vp.push((t1 - t0).as_micros());
                    t_vp_copy.push((t2 - t1).as_micros());
                    t_vp_map.push((t3 - t2).as_micros());
                    t_vp_memcpy.push((t4 - t3).as_micros());
                }
                None => {
                    println!("NV12 Map 失败");
                    return;
                }
            }
        }
        println!();
        println!(
            "--- 新路径（GPU 内 BGRA→NV12+缩放，只回读 {ow}x{oh} NV12 {:.2}MB）---",
            nv12_len as f64 / 1048576.0
        );
        report("① VideoProcessorBlt", &t_vp, "GPU 缩放+换色");
        report("② CopyResource → staging", &t_vp_copy, "GPU 内拷贝");
        report("③ Map 拿指针", &t_vp_map, "GPU→CPU 同步");
        report("④ memcpy 到 Vec", &t_vp_memcpy, "CPU");
        let sum_vp: Vec<u128> = (0..t_vp.len())
            .map(|i| t_vp[i] + t_vp_copy[i] + t_vp_map[i] + t_vp_memcpy[i])
            .collect();
        report("①~④ 合计", &sum_vp, "<- 新路径抓屏侧（不需 JPEG）");
        println!();

        // ---------------- ④ JPEG 编码成本 ----------------
        println!("--- JPEG 编码成本（jpeg-encoder 0.6，与 capture.rs 同库同调用）---");
        let n_src = px(SW, SH) * 4;
        let n_half = px(HALF.0, HALF.1) * 4;
        if native_buf.len() >= n_src {
            for q in [85u8, 50u8] {
                let mut us = Vec::new();
                let mut last = 0usize;
                for _ in 0..10 {
                    let mut out = Vec::with_capacity(n_src / 6 + 4096);
                    let t0 = Instant::now();
                    let enc = jpeg_encoder::Encoder::new(&mut out, q);
                    let _ = enc.encode(
                        &native_buf[..n_src],
                        SW as u16,
                        SH as u16,
                        jpeg_encoder::ColorType::Bgra,
                    );
                    us.push(t0.elapsed().as_micros());
                    last = out.len();
                }
                report(
                    &format!("JPEG encode {SW}x{SH} q{q}"),
                    &us,
                    &format!("输出 {:.0} KB", last as f64 / 1024.0),
                );
            }
        }
        if half_buf.len() >= n_half {
            let mut us = Vec::new();
            let mut last = 0usize;
            for _ in 0..10 {
                let mut out = Vec::with_capacity(n_half / 6 + 4096);
                let t0 = Instant::now();
                let enc = jpeg_encoder::Encoder::new(&mut out, 85);
                let _ = enc.encode(
                    &half_buf[..n_half],
                    HALF.0 as u16,
                    HALF.1 as u16,
                    jpeg_encoder::ColorType::Bgra,
                );
                us.push(t0.elapsed().as_micros());
                last = out.len();
            }
            report(
                &format!("JPEG encode {}x{} q85", HALF.0, HALF.1),
                &us,
                &format!("输出 {:.0} KB", last as f64 / 1024.0),
            );
        }

        println!();
        // ---------------- ⑤ 色度采样档位对照（验证「改采样档位」是不是解） ----------------
        // 读源码得到的事实：`Encoder::new` 在 quality < 90 时**默认就是 F_2_2（4:2:0）**。
        // 所以「把档位改小以减少工作量」这个方向本身是错的 —— 这里用实测把它钉死。
        println!("--- 色度采样档位对照（{}x{} q85，同一张图）---", HALF.0, HALF.1);
        for (name, sf) in [
            (
                "F_1_1 (4:4:4 全色度)",
                jpeg_encoder::SamplingFactor::F_1_1,
            ),
            (
                "F_2_1 (4:2:2 半水平)",
                jpeg_encoder::SamplingFactor::F_2_1,
            ),
            (
                "F_2_2 (4:2:0 默认档)",
                jpeg_encoder::SamplingFactor::F_2_2,
            ),
        ] {
            let mut us = Vec::new();
            let mut last = 0usize;
            for _ in 0..10 {
                let mut out = Vec::with_capacity(n_half / 6 + 4096);
                let mut enc = jpeg_encoder::Encoder::new(&mut out, 85);
                enc.set_sampling_factor(sf);
                let t0 = Instant::now();
                let _ = enc.encode(
                    &half_buf[..n_half],
                    HALF.0 as u16,
                    HALF.1 as u16,
                    jpeg_encoder::ColorType::Bgra,
                );
                us.push(t0.elapsed().as_micros());
                last = out.len();
            }
            report(
                &format!("  {}x{} q85 {name}", HALF.0, HALF.1),
                &us,
                &format!("输出 {:.0} KB", last as f64 / 1024.0),
            );
        }

        println!();
        println!("== 合计对照（抓屏侧 + 编码侧，同帧口径）==");
        report("现状 原生1680x1050 抓屏(①②③)", &sum_native, "");
        report("现状 720P 抓屏(①②③')", &sum_half, "");
        report("新路径 720P 抓屏(①~④)", &sum_vp, "");
        // 端到端口径：新路径抓屏 + 目标尺寸 JPEG 编码（= capture.rs 当前的 JPEG 模式）
        let mut enc_end = Vec::new();
        for i in 0..sum_vp.len() {
            let mut out = Vec::with_capacity(n_half / 6 + 4096);
            let t0 = Instant::now();
            let enc = jpeg_encoder::Encoder::new(&mut out, 85);
            let _ = enc.encode(
                &half_buf[..n_half],
                HALF.0 as u16,
                HALF.1 as u16,
                jpeg_encoder::ColorType::Bgra,
            );
            enc_end.push(t0.elapsed().as_micros() + sum_vp[i]);
        }
        report("新路径 720P 抓屏 + JPEG q85", &enc_end, "<- 端到端壳侧");

        // ---------------- ⑥ 分条并行编码（决策依据：多线程能不能救 JPEG） ----------------
        // 背景：JPEG 编码占了整帧 90% 的时间（原生 39ms / 720P 25ms），
        // simd 只榨出 15~20%，剩下的瓶颈是霍夫曼编码这类**本质串行**的部分
        // （jpeg-encoder 的 FDCT 已是单 MCU 独立可并行，但整图 Huffman 顺序写流）。
        // 唯一不换编码器的出路：把图**按行切成 N 条，每条编成一份独立 JPEG**，
        // 前端收 N 份再拼画。这里只测「这么做能快多少」，先拿数字再决定动不动架构。
        // 切点按 16 行对齐（MCU 高度 16），保证与整图编码的像素边界一致。
        println!();
        println!("--- 分条并行 JPEG（同一张图按 16 行对齐切 N 条，各编一份独立 JPEG）---");
        let mk = |w: usize, h: usize| -> Vec<u8> {
            let mut v = vec![0u8; w * h * 4];
            for i in 0..w * h {
                v[i * 4] = ((i * 7) % 251) as u8;
                v[i * 4 + 1] = ((i / 3) % 241) as u8;
                v[i * 4 + 2] = ((i / 11) % 233) as u8;
                v[i * 4 + 3] = 255;
            }
            v
        };
        for (label, w, h) in [("1680x1050", SW, SH), ("1280x800", HALF.0, HALF.1)] {
            let data = mk(w as usize, h as usize);
            for n in [1usize, 2, 3, 4, 6] {
                let rows_per = ((h as usize + n - 1) / n + 15) / 16 * 16;
                let mut us = Vec::new();
                let mut total_kb = 0.0f64;
                for _ in 0..8 {
                    let t0 = Instant::now();
                    let outs: Vec<Vec<u8>> = std::thread::scope(|s| {
                        let mut handles = Vec::new();
                        let mut y = 0usize;
                        while y < h as usize {
                            let sh = rows_per.min(h as usize - y);
                            let start = y * w as usize * 4;
                            let end = start + sh * w as usize * 4;
                            let slice: &[u8] = &data[start..end];
                            handles.push(s.spawn(move || {
                                let mut out = Vec::with_capacity(slice.len() / 8 + 4096);
                                let enc = jpeg_encoder::Encoder::new(&mut out, 85);
                                let _ = enc.encode(
                                    slice,
                                    w as u16,
                                    sh as u16,
                                    jpeg_encoder::ColorType::Bgra,
                                );
                                out
                            }));
                            y += sh;
                        }
                        handles.into_iter().map(|hh| hh.join().unwrap()).collect()
                    });
                    us.push(t0.elapsed().as_micros());
                    total_kb = outs.iter().map(|o| o.len()).sum::<usize>() as f64 / 1024.0;
                }
                report(
                    &format!("  {label} 切 {n} 条并行"),
                    &us,
                    &format!("合计输出 {total_kb:.0} KB"),
                );
            }
        }
    }
}

fn create_tex(
    device: &ID3D11Device,
    w: u32,
    h: u32,
    fmt: DXGI_FORMAT,
    bind: u32,
    usage: D3D11_USAGE,
    cpu: u32,
) -> Option<ID3D11Texture2D> {
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
        CPUAccessFlags: cpu,
        MiscFlags: 0,
    };
    let mut t: Option<ID3D11Texture2D> = None;
    unsafe { device.CreateTexture2D(&d, None, Some(&mut t)).ok()? };
    t
}
