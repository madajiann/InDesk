//! [20261021 七轮] **CPU 软件 H.264 编码器（OpenH264）** —— 无 NVENC 时的兜底路径。
//!
//! ## 定位（先看这段，别重复踩）
//!
//! 本轮先怀疑「NVENC 在 1GB 显存下不可用」，动手做了这条纯 CPU 路径；**结论被自己的
//! 实验推翻**：真正的病灶是进程优先级（计划任务 `InDeskGuard` 未设 Priority → 壳/核
//! 继承 **BelowNormal**），抓屏线程被普通优先级程序反复饿死。硬证据：
//! `slow soft-encode 4181ms`（纯 CPU 编码，CPU 100% 频率、系统负载仅 22% —— 在等调度）。
//! 把进程提到 ABOVE_NORMAL + 抓屏线程 HIGHEST 后，同机 NVENC **24fps 稳、12.6ms/帧、
//! 零慢帧**。所以硬编才是默认；本模块只在两种情况下启用：
//!   1. 显式要求（`captureStart { enc: "soft" }`，A/B 或排查用）；
//!   2. NVENC 初始化失败时自动兜底（无 NVIDIA 卡 / 驱动无编码器），比退回 JPEG 好。
//!
//! ## 性能（i5-4590，屏幕内容模式 4Mbps，**真实桌面内容**）
//!
//! | 分辨率 | 单帧 | 备注 |
//! |---|---|---|
//! | 1152x720 | 36ms（合成）/ **200~700ms（真实复杂帧）** | 多线程几乎无收益 |
//! | 1024x640 | 29ms（合成） | 软编只在兜底时用，别再指望它跑满 24fps |
//!
//! 结论：软编只能当兜底，不能当主力 —— 桌面一有大幅变化，OpenH264 在 4 核老 U 上
//! 单帧就要 200ms+。真正要提速只能换编码器（x264 ultrafast / 硬件），不是调参。

use std::time::Instant;

use openh264::encoder::{Encoder, EncoderConfig, RateControlMode, UsageType};
use openh264::formats::YUVSource;
use openh264::OpenH264API;

use crate::h264::FrameStat;

/// 一帧 I420（Y/U/V 三平面）视图：Y 直接借用调用方的连续 NV12 缓冲，U/V 借用拆分包。
struct Planar<'a> {
    y: &'a [u8],
    u: &'a [u8],
    v: &'a [u8],
    w: usize,
    h: usize,
}

impl YUVSource for Planar<'_> {
    fn dimensions(&self) -> (usize, usize) {
        (self.w, self.h)
    }
    fn strides(&self) -> (usize, usize, usize) {
        (self.w, self.w / 2, self.w / 2)
    }
    fn y(&self) -> &[u8] {
        self.y
    }
    fn u(&self) -> &[u8] {
        self.u
    }
    fn v(&self) -> &[u8] {
        self.v
    }
}

/// CPU 软编 H.264（NV12 输入，Annex-B 输出）。
pub struct SoftEncoder {
    enc: Encoder,
    w: usize,
    h: usize,
    fps: u32,
    bitrate: u32,
    frame_idx: u64,
    want_idr: bool,
    /// UV 拆分后的暂存（复用，避免每帧分配）
    u: Vec<u8>,
    v: Vec<u8>,
    /// 首帧 NAL 组成（诊断：确认 IDR 带 SPS/PPS）
    first_nal_types: Option<Vec<u8>>,
    // ---- 统计（每 5 秒由抓屏线程汇报）----
    stat_frames: u64,
    stat_bytes: u64,
    stat_us: u128,
    stat_idr: u64,
    stat_max_bytes: usize,
}

impl SoftEncoder {
    pub fn new(w: u32, h: u32, fps: u32, bitrate: u32) -> Result<Self, String> {
        // OpenH264 要求偶数宽高（YUV420 色度下采样）。
        if w < 16 || h < 16 {
            return Err(format!("尺寸过小：{w}x{h}"));
        }
        let w = (w & !1) as usize;
        let h = (h & !1) as usize;
        let cfg = EncoderConfig::new()
            .set_bitrate_bps(bitrate.max(200_000))
            .max_frame_rate(fps.max(1) as f32)
            // 屏幕内容模式：开启背景检测/跳块，桌面画面比摄像头画面省很多。
            .usage_type(UsageType::ScreenContentRealTime)
            .rate_control_mode(RateControlMode::Bitrate)
            // 不用编码器自己的跳帧：抓屏循环已经按目标帧率节流，
            // 让它再跳会让「丢帧→等 IDR」的闸门逻辑变得不可预测。
            .enable_skip_frame(false)
            .set_multiple_thread_idc(2)
            .sps_pps_strategy(openh264::encoder::SpsPpsStrategy::ConstantId);
        let enc = Encoder::with_api_config(OpenH264API::from_source(), cfg)
            .map_err(|e| format!("OpenH264 初始化失败: {e}"))?;
        Ok(Self {
            enc,
            w,
            h,
            fps: fps.max(1),
            bitrate,
            frame_idx: 0,
            want_idr: true,
            u: vec![0u8; w * h / 4],
            v: vec![0u8; w * h / 4],
            first_nal_types: None,
            stat_frames: 0,
            stat_bytes: 0,
            stat_us: 0,
            stat_idr: 0,
            stat_max_bytes: 0,
        })
    }

    pub fn size(&self) -> (u32, u32) {
        (self.w as u32, self.h as u32)
    }

    pub fn bitrate(&self) -> u32 {
        self.bitrate
    }

    pub fn request_idr(&mut self) {
        self.want_idr = true;
    }

    /// 编码一帧连续 NV12（Y 平面 `w*h`，其后 UV 交错 `w*h/2`，无行填充）。
    pub fn encode_nv12(&mut self, nv12: &[u8], dst: &mut Vec<u8>) -> Result<Option<FrameStat>, String> {
        let y_len = self.w * self.h;
        let uv_len = y_len / 2;
        if nv12.len() < y_len + uv_len {
            return Err(format!(
                "NV12 缓冲区过小：{} < {}",
                nv12.len(),
                y_len + uv_len
            ));
        }
        let (y, uv) = nv12.split_at(y_len);

        // UV 交错 → 两个平面（实测 0.22ms/帧 @1280x800）
        let t_deint = Instant::now();
        for (i, pair) in uv[..uv_len].chunks_exact(2).enumerate() {
            self.u[i] = pair[0];
            self.v[i] = pair[1];
        }
        let deint_us = t_deint.elapsed().as_micros();

        let is_idr = self.want_idr;
        if is_idr {
            self.enc.force_intra_frame();
            self.want_idr = false;
        }

        let src = Planar {
            y,
            u: &self.u,
            v: &self.v,
            w: self.w,
            h: self.h,
        };
        let ts_ms = self.frame_idx * 1000 / self.fps.max(1) as u64;
        let t0 = Instant::now();
        let bs = self
            .enc
            .encode_at(&src, openh264::Timestamp::from_millis(ts_ms))
            .map_err(|e| format!("OpenH264 编码失败（第 {} 帧）: {e}", self.frame_idx))?;
        let enc_us = t0.elapsed().as_micros();
        let t1 = Instant::now();
        let mut nals = Vec::new();
        bs.write_vec(&mut nals);
        let copy_us = t1.elapsed().as_micros();
        let us = t0.elapsed().as_micros();
        self.frame_idx += 1;

        dst.clear();
        let len = nals.len();
        if len > 0 {
            dst.extend_from_slice(&nals);
        }

        if self.first_nal_types.is_none() && len > 0 {
            // Annex-B：以 00 00 01 / 00 00 00 01 分隔，取每个 NAL 头的低 5 位
            let types = nal_types(&nals);
            crate::capture::capture_diag(&format!(
                "soft264 首帧 NAL={:?} bytes={}（含 7=SPS 8=PPS 5=IDR 说明可中途接入）",
                types, len
            ));
            self.first_nal_types = Some(types);
        }

        if us > 150_000 {
            crate::capture::capture_diag(&format!(
                "slow soft-encode {}ms idr={} bytes={} frame={}（拆分 {}us / 编码 {}ms / 拷贝 {}us）",
                us / 1000,
                is_idr,
                len,
                self.frame_idx,
                deint_us,
                enc_us / 1000,
                copy_us
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

/// 扫 Annex-B 码流取 NAL 类型序列（仅诊断用，最多取 8 个）。
fn nal_types(buf: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    let mut i = 0usize;
    while i + 3 < buf.len() && out.len() < 8 {
        if buf[i] == 0 && buf[i + 1] == 0 && buf[i + 2] == 1 {
            let mut j = i + 3;
            if j < buf.len() {
                out.push(buf[j] & 0x1f);
            }
            // 跳到下一个起始码
            j += 1;
            while j + 2 < buf.len()
                && !(buf[j] == 0 && buf[j + 1] == 0 && buf[j + 2] == 1)
            {
                j += 1;
            }
            i = j;
        } else {
            i += 1;
        }
    }
    out
}
