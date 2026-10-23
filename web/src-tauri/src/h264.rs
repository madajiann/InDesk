//! 壳侧 NVENC 硬件 H.264 编码（阶段① 画面引擎）。
//!
//! ## 为什么走 NVENC 直连，而不是 Media Foundation
//!
//! 1.4.5 的原计划是用 MF 直接调「NVIDIA H.264 Encoder MFT」，但**实测全路作废**：
//! 在本机（i5-4590 + GTX750 + Intel HD4600）做 4 个适配器 × 2 个编码器（NVENC/QSV）
//! 的完整交叉矩阵，**全部**返回 `MF_E_TRANSFORM_ASYNC_LOCKED`（0xC00D6D77）；
//! 即便按官方文档在 `IMFTransform::GetAttributes()` 上设了
//! `MF_TRANSFORM_ASYNC_UNLOCK = 1`（读回确为 1）也无效。取证脚本见 `lab/src/main.rs`
//! 的初版（MF 矩阵探针，已存 `lab/src/mf_probe_windows061.rs.bak`）。
//!
//! 2. 而 `nvEncodeAPI64.dll` 是驱动自带的正规接口，本机实测可用：
//! `NvEncodeAPIGetMaxSupportedVersion` 返回 `0xC1` → **API 12.1**
//! （注意编码方式：低 4 位是 minor、其余是 major，不是头文件里
//! `MAJOR | (MINOR << 24)` 那套，两者不能混用）。
//!
//! ## 为什么必须壳侧编码（而不是把裸像素直送前端）
//!
//! 「直送」两条路都已被**端到端实测否决**（数据见 `src/utils/nativeScreen.ts` 模块注释）：
//!   - BGRA 直送一帧 4MB → WebView2 自定义协议只有 **11fps**（比带压缩还慢）；
//!   - NV12 直送一帧 1.46MB → 26.7fps，但**偏色**（BGRA→NV12→BGRA 对照
//!     meanAbsDiff 9.40 / pctOver8 50.2%，是噪声底 0.05 的 188 倍）；
//!   - 壳侧 CPU JPEG 是硬伤：原生 1680×1050 仅 18.7fps（42.9ms/帧）。
//!
//! H.264 1080p60 在 8Mbps 档每帧约 8KB —— **比 NV12 的 1.46MB 小约 180 倍**，
//! 通道瓶颈与 CPU 编码瓶颈一并解掉；且编码在 GPU 上，CPU 占用近 0。
//!
//! ## 本机实测（1080p，GTX750，预设 P3 + LowLatency + CBR）
//!
//! | 分辨率 | 单帧耗时 | 理论上限 | 对 60fps 余量 |
//! |---|---|---|---|
//! | 1280×720 | 1.82 ms | 548 fps | 9.1× |
//! | 1920×1080 | 3.17 ms | 315 fps | 5.2× |
//! | 2560×1440 | 5.42 ms | 185 fps | 3.1× |
//!
//! 输出经 Annex-B 解析确认：High Profile、level 按分辨率自动选（3.2/4.2/5.1）、
//! 1080p 时 coded 高度 1088 且 frame cropping 正确裁回 1080（**无绿边**）。
//!
//! ## 两个关键配置（都是实测验证过的，别随手改）
//!
//! - **`repeatSPSPPS`（`NV_ENC_CONFIG_H264` 位域 bit12，掩码 `0x1000`）**：
//!   每个 IDR 前都重发 SPS/PPS。不打开时实测 300 帧内 SPS/PPS **只出现 1 次**，
//!   新客户端中途接入会拿到 IDR 却缺参数集、**无法解码**。打开后实测
//!   SPS/PPS 数量与 IDR 数量严格相等。
//!   位偏移由 `lab/tools/bitfield_offsets.py` 从 12.1 头文件机械解析得到，
//!   不靠手数（手数位错了不报错，只是开关静默失效）。
//! - **`NV_ENC_PIC_FLAGS::FORCEIDR`（0x2）**：`enablePTD = 1`（编码器自决帧类型）下
//!   仍能把指定帧钉成 IDR。实测：`idr_period=120` 时强制第 60/200 帧 →
//!   IDR 落在 [0, 60, 180, 200]（180 = 60+120，说明**强制帧真的重置了 GOP 计数**，
//!   不是简单插个 I 帧；240 那次自动 IDR 因顺延到 320 而消失）。这是
//!   「新客户端接入 / 丢包后恢复」能立刻出画面的前提。
//!
//! ## 零拷贝
//!
//! 输入纹理直接注册给 NVENC（`nvEncRegisterResource` + `nvEncMapInputResource`），
//! 编码器从 D3D11 纹理上直接读，**不经过 CPU 回读**。
//! 见 `vendor/nvenc`（本仓内联、已按 API 12.1 对齐结构体版本与布局）。

use std::time::Instant;

use windows::Win32::Graphics::Direct3D11::{ID3D11Device, ID3D11Texture2D};

use nvenc::bitstream::BitStream;
use nvenc::encoder::{Encoder, RegisteredResource};
use nvenc::session::{InitParams, Session};
use nvenc::sys::enums::{
    NVencBFrameRefMode, NVencBufferFormat, NVencH264EntropyCodingMode, NVencMultiPass,
    NVencNumRefFrames, NVencParamsRcMode, NVencPicFlags, NVencPicStruct, NVencPicType,
    NVencTuningInfo, NVencVUIColorPrimaries, NVencVUIMatrixCoeffs, NVencVUITransCharacteristics,
    NVencVUIVideoFormat,
};
use nvenc::sys::guids::{NV_ENC_CODEC_H264_GUID, NV_ENC_PRESET_P1_GUID};

/// `NV_ENC_CONFIG_H264` 位域 bit12 = `repeatSPSPPS`。
/// 由 `lab/tools/bitfield_offsets.py` 从 `nvEncodeAPI 12.1` 头文件机械解析得出
/// （掩码 0x00001000，同字内还有 outputAUD=bit6=0x40、disableSPSPPS=bit7=0x80）。
const H264_BITFLAG_REPEAT_SPSPPS: u32 = 0x0000_1000;

/// 一次编码的结果。
#[derive(Debug, Clone, Copy)]
pub struct FrameStat {
    /// 本帧访问单元（AU）的 Annex-B 字节数。
    pub bytes: usize,
    /// 本帧是否是 IDR（关键帧）。新客户端必须从 IDR 起才能解。
    pub is_idr: bool,
    /// 本帧编码耗时（含取回，微秒）。
    pub encode_us: u128,
}

/// NVENC H.264 编码器（NV12 输入，零拷贝从 D3D11 纹理取图）。
///
/// **字段顺序有意义**：Rust 按声明顺序析构，而 `RegisteredResource` 的 Drop 会做
/// `nvEncUnmapInputResource` / `nvEncUnregisterResource`，必须在编码器真正销毁之前完成。
/// 所以顺序固定为 registered → encoder → buf，实际析构链为
/// 「注销纹理 → 释放位流缓冲 → 销毁编码器」。
pub struct H264Encoder {
    registered: RegisteredResource,
    encoder: Encoder,
    buf: BitStream,
    w: u32,
    h: u32,
    fps: u32,
    bitrate: u32,
    /// 已提交的帧序号（NVENC 要求单调递增）。
    frame_idx: u64,
    /// 下一帧强制成 IDR。
    want_idr: bool,
    // ---- 诊断（每 5 秒由抓屏线程汇报一次）----
    stat_frames: u64,
    stat_bytes: u64,
    stat_us: u128,
    stat_idr: u64,
    stat_max_bytes: usize,
}

impl H264Encoder {
    /// 建编码器并**立即把输入纹理注册进 NVENC**（失败即整体失败，不留半初始化状态）。
    ///
    /// `device` 必须是 `tex` 所属的那个 D3D11 设备 —— NVENC 会话与输入资源必须同设备，
    /// 否则注册会失败或退化成隐式拷贝。调用方（`dxgi.rs`）传的就是捕获器自己的设备。
    ///
    /// `bitrate` 单位 bps。屏幕共享用 CBR 压住延迟抖动；`vbv_buffer_size` 取半秒，
    /// 突发不至于把上行打满。
    pub fn new(
        device: &ID3D11Device,
        tex: &ID3D11Texture2D,
        w: u32,
        h: u32,
        fps: u32,
        bitrate: u32,
    ) -> Result<Self, String> {
        if w < 2 || h < 2 {
            return Err(format!("尺寸过小：{w}x{h}"));
        }

        // ---------- 1) 会话 ----------
        // 失败原因原样带出（无 NVIDIA 卡时是 LibraryNotFound：dll 加载失败），
        // 由上层决定退回 JPEG —— 这里绝不 panic（vendor 已把原来的 unwrap 改掉）。
        let session = Session::open_dx(device)
            .map_err(|e| format!("nvEncOpenEncodeSessionEx 失败: {e:?}"))?;

        // ---------- 2) 能力核对 ----------
        let codecs = session
            .get_encode_codecs()
            .map_err(|e| format!("查询编码器列表失败: {e:?}"))?;
        if !codecs.contains(&NV_ENC_CODEC_H264_GUID) {
            return Err(format!(
                "该设备未提供 H.264 硬件编码器（只报出 {} 个编码器）",
                codecs.len()
            ));
        }

        // ---------- 3) 预设 + 调参 ----------
        // [20261021 5轮] P4 → P1：P4 的"运动质量更好"是以每帧运算量为代价的，
        // 而本机 GPU 被钉在 P8 135MHz（纯 NVENC 负载不触发升频），实测 P4 要
        // 27ms/帧 → 30fps 根本喂不动。P1 是最快档，先把帧产出来。
        let (session, mut cfg) = session
            .get_encode_preset_config_ex(
                NV_ENC_CODEC_H264_GUID,
                NV_ENC_PRESET_P1_GUID,
                NVencTuningInfo::LowLatency,
            )
            .map_err(|e| format!("取预设配置失败: {e:?}"))?;

        {
            let pc = &mut cfg.preset_cfg;
            // 无限 GOP：屏幕内容帧间几乎不动，长 GOP 最省带宽（关键帧按需强制）。
            pc.gop_len = 0xFFFF_FFFF;
            // 无 B 帧 —— B 帧要靠未来帧做参考，直接加延迟，远程桌面不能要。
            pc.frame_interval_p = 1;
            pc.rc_params.rate_control_mode = NVencParamsRcMode::CBR;
            pc.rc_params.average_bit_rate = bitrate;
            pc.rc_params.max_bit_rate = bitrate;
            pc.rc_params.vbv_buffer_size = bitrate / 2; // 半秒缓冲
            pc.rc_params.vbv_initial_delay = 1;
            pc.rc_params.look_ahead_depth = 0; // 关前视：它要缓存多帧，纯加延迟
            // [20261019q] 双通道（1/4 分辨率第一遍）：CBR 下码率分配更准，
            // 静止→运动切换时的画质塌陷（"水波纹"）明显减轻；代价仅编码器
            // 内部一遍低分辨率预分析，毫秒级，无端到端延迟。
            // [20261021] 关闭多遍编码：TwoPassQuarterResolution 会在正式编码前
            // 额外跑一遍 1/4 分辨率预分析，在本机（GTX 750，1GB 显存，且与
            // 其它屏幕编码器共卡）实测编码 22ms/帧、周期 1.2s 停顿 —— 实时
            // 投屏根本不需要多遍率控，单遍 CBR 足够，省掉的是纯 GPU 时间。
            pc.rc_params.multi_pass = NVencMultiPass::Disabled;

            unsafe {
                let h = &mut pc.encode_codec_config.h264;
                // 兜底周期关键帧：客户端异常断开时没有"请求关键帧"的机会，
                // 靠它保证每 2 秒一定有一次可接入点。（想更省带宽可调大。）
                // [20261021] 周期关键帧从 2s 放宽到 10s：IDR 帧体积是 P 帧的数倍，
                // 在低频率 NVENC 上每 2 秒一次就是固定节奏的卡顿尖峰。丢帧/断链
                // 恢复本来就有显式 requestKeyframe（captureKeyframe），不需要靠它兜底。
                h.idr_period = fps.max(1) * 10;
                // 见文件头：不开这项，300 帧里 SPS/PPS 只出现 1 次，中途接入必黑屏。
                h.bitflags |= H264_BITFLAG_REPEAT_SPSPPS;
                // [20261021] CAVLC：CABAC 每帧要多做一遍算术编码，在 P8 低频率下
                // 是纯开销；码率略涨（同画质约 +5~8%），换来编码时间明显下降。
                h.entropy_coding_mode = NVencH264EntropyCodingMode::Cavlc;
                h.max_num_ref_frames = NVencNumRefFrames::Frames1 as u32;
                h.use_b_frames_as_ref = NVencBFrameRefMode::Disabled;

                // VUI 写全，保证对端解码不偏色：BT709 + 有限范围。
                // 前端 WebCodecs 解码按同一套解释，两端一致才不会出现直送 NV12 那种偏色。
                let vui = &mut h.h264_vui_parameters;
                vui.color_description_present_flag = 1;
                vui.color_primaries = NVencVUIColorPrimaries::BT709;
                vui.transfer_characteristics = NVencVUITransCharacteristics::BT709;
                vui.color_matrix = NVencVUIMatrixCoeffs::BT709;
                vui.video_full_range_flag = 0;
                vui.video_signal_type_present_flag = 1;
                vui.video_format = NVencVUIVideoFormat::Unspecified;
            }
        }

        // ---------- 4) 初始化 ----------
        // enable_ptd = true：帧类型交给编码器自决（它会做得比我们准），
        // 需要强制关键帧时用 NVencPicFlags::ForceIDR，而不是关掉 PTD。
        let init = InitParams {
            encode_guid: NV_ENC_CODEC_H264_GUID,
            // [20261021 5轮] P4（偏质量）在本机把 NVENC 压到 27ms/帧：GPU 被驱动
            // 钉在 P8 135MHz（纯 NVENC 负载不触发升频，实测 nvidia-smi -lgc 也锁不上），
            // 可用算力只有满频的 1/7。P1 是最快档，同样的时钟下每帧运算量最小。
            preset_guid: NV_ENC_PRESET_P1_GUID,
            resolution: [w, h],
            aspect_ratio: [w, h],
            frame_rate: [fps.max(1), 1],
            tuning_info: NVencTuningInfo::LowLatency,
            buffer_format: NVencBufferFormat::NV12,
            encode_config: &mut cfg.preset_cfg,
            enable_ptd: true,
            max_encoder_resolution: [0, 0],
        };
        let encoder = session
            .init_encoder(init)
            .map_err(|e| format!("nvEncInitializeEncoder 失败: {e:?}"))?;

        // ---------- 5) 零拷贝注册输入纹理 ----------
        let registered = encoder
            .register_resource_dx11(tex, NVencBufferFormat::NV12, w)
            .map_err(|e| format!("注册输入纹理失败: {e:?}"))?;

        // ---------- 6) 位流缓冲 ----------
        // 单缓冲 + 同步取回：编码一帧只等它自己（实测 3ms），延迟最低且不会积压。
        // 不用多缓冲轮转 —— 那会引入"取回的是上一帧"的额外一帧延迟。
        let buf = encoder
            .create_bitstream_buffer()
            .map_err(|e| format!("创建位流缓冲失败: {e:?}"))?;

        Ok(Self {
            registered,
            encoder,
            buf,
            w,
            h,
            fps: fps.max(1),
            bitrate,
            frame_idx: 0,
            want_idr: true, // 首帧必须是 IDR，否则对端无从解起
            stat_frames: 0,
            stat_bytes: 0,
            stat_us: 0,
            stat_idr: 0,
            stat_max_bytes: 0,
        })
    }

    pub fn size(&self) -> (u32, u32) {
        (self.w, self.h)
    }

    pub fn bitrate(&self) -> u32 {
        self.bitrate
    }

    /// 请求下一帧为 IDR。**新客户端接入、丢包后恢复、切换分辨率后**都要调
    /// —— 否则客户端要等到下一个周期关键帧（默认 2 秒）才有画面。
    /// 连续多次调用只生效一次（幂等）。
    pub fn request_idr(&mut self) {
        self.want_idr = true;
    }

    /// 编码一帧：提交 NV12 纹理 → 等本帧编完 → 把 Annex-B 访问单元写入 `out`。
    ///
    /// `out` 会被清空重填，调用方可复用同一块缓冲，避免每帧分配。
    /// 返回 `None` 表示本帧没有产出（编码器吞帧，正常现象，调用方跳过即可）。
    ///
    /// **输入纹理的内容必须已经在 GPU 上就绪**（VideoProcessorBlt 完成后），
    /// 本函数不做任何 CPU 侧上传。
    pub fn encode_into(&mut self, out: &mut Vec<u8>) -> Result<Option<FrameStat>, String> {
        let ty = if self.want_idr {
            NVencPicType::IDR
        } else {
            NVencPicType::P
        };
        // 强制 IDR 双保险：既传 picture_type=IDR，也带 FORCEIDR 标志。
        // enablePTD=1 时 picture_type 只是"建议"，FORCEIDR 才是硬保证（实测有效）。
        let flags = if self.want_idr {
            NVencPicFlags::ForceIDR as u32
        } else {
            0
        };
        let is_idr = self.want_idr;
        self.want_idr = false;

        let t0 = Instant::now();
        self.encoder
            .encode_picture_flagged(
                &self.registered,
                &self.buf,
                self.frame_idx as usize,
                // 时间戳按帧率折算成 100ns 单位（NVENC 惯例），
                // 前端可用它做抖动计算/音画对齐。
                self.frame_idx * 10_000_000 / self.fps.max(1) as u64,
                NVencBufferFormat::NV12,
                NVencPicStruct::Frame,
                ty,
                flags,
                None,
            )
            .map_err(|e| format!("第 {} 帧提交编码失败: {e:?}", self.frame_idx))?;
        self.frame_idx += 1;

        // wait = true：阻塞到本帧位流就绪。单缓冲下这一步是必须的，
        // 且它天然把编码耗时压在"单帧编码时间"内（实测 3ms），不会积压。
        let guard = self
            .buf
            .try_lock(true)
            .map_err(|e| format!("取回位流失败: {e:?}"))?;
        let bytes = guard.as_slice();
        let len = bytes.len();

        out.clear();
        if len > 0 {
            out.extend_from_slice(bytes);
        }
        drop(guard);

        let us = t0.elapsed().as_micros();
        // [20261021 5轮] 单帧异常耗时取证：把 IDR 与否、输出字节一起落盘，
        // 用来判断「周期性 1~2s 卡顿」是不是关键帧编码撑爆的。
        if us > 150_000 {
            crate::capture::capture_diag(&format!(
                "slow encode {}ms idr={} bytes={} frame={}",
                us / 1000,
                is_idr,
                len,
                self.frame_idx
            ));
        }
        self.stat_frames += 1;
        self.stat_bytes += len as u64;
        self.stat_us += us;
        if is_idr {
            self.stat_idr += 1;
        }
        if len > self.stat_max_bytes {
            self.stat_max_bytes = len;
        }

        if len == 0 {
            return Ok(None);
        }
        Ok(Some(FrameStat {
            bytes: len,
            is_idr,
            encode_us: us,
        }))
    }

    /// 取累计统计并清零（抓屏线程每 5 秒汇报一次）。
    /// 返回 `(帧数, 总字节, 总微秒, IDR 数, 单帧最大字节)`。
    pub fn take_stats(&mut self) -> (u64, u64, u128, u64, usize) {
        let v = (
            self.stat_frames,
            self.stat_bytes,
            self.stat_us,
            self.stat_idr,
            self.stat_max_bytes,
        );
        self.stat_frames = 0;
        self.stat_bytes = 0;
        self.stat_us = 0;
        self.stat_idr = 0;
        self.stat_max_bytes = 0;
        v
    }
}
