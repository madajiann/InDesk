//! 壳侧 H.264 硬编链路自检（仅开发用，不进安装包）。
//!
//! ```text
//! cargo run --release --example h264_probe -- 1280 720 6
//! ```
//! 参数：`宽 高 秒数`（都可省略，默认 1280 720 6）。
//! 环境变量：`PROBE_BITRATE`（kbps，默认 4000）、`PROBE_OUT`（码流落盘路径）、
//! `PROBE_KEYFRAME_EVERY`（每 N 帧请求一次关键帧，验 `request_keyframe()` 通路）。
//!
//! 与 lab 探针的区别：**这里用的是真实 DXGI 桌面复制**，不是合成帧。
//! 也就是说它验证的是产线那条真链路：
//!   桌面 → AcquireNextFrame → CopyResource → VideoProcessorBlt(缩放着色成 NV12)
//!        → NVENC 零拷贝硬编 → Annex-B 访问单元
//! 全程不回读 CPU。任何一环坏掉这里都会直接暴露。
//!
//! ⚠️ 桌面**静止**时 `grab()` 会返回 `NoChange`（这是设计，不是故障）——
//! 所以帧数少不代表链路坏。脚本会尝试 `InvalidateRect(NULL)` 逼桌面重绘，
//! 但真正可靠的验证方式是测试期间让屏幕有点变化。

use std::time::{Duration, Instant};

use indesk_desktop_lib::dxgi::{DxgiCapturer, Grab, OutFormat};

fn env_u32(k: &str, d: u32) -> u32 {
    std::env::var(k)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(d)
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let w = args
        .first()
        .and_then(|s| s.parse().ok())
        .unwrap_or_else(|| env_u32("PROBE_W", 1280));
    let h = args
        .get(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or_else(|| env_u32("PROBE_H", 720));
    let secs = args
        .get(2)
        .and_then(|s| s.parse().ok())
        .unwrap_or_else(|| env_u32("PROBE_SECS", 6));
    let bitrate = env_u32("PROBE_BITRATE", 4000) * 1000;
    let kf_every = env_u32("PROBE_KEYFRAME_EVERY", 0) as usize;

    println!("[h264probe] 目标 {w}x{h} {bitrate}bps，采集 {secs}s");

    // ---------- 建捕获器：H264 模式下这一步会连带把 NVENC 编码器也建出来 ----------
    // 建不出来就直接是 Err（链失败），上层产品里会据此回退 JPEG。
    let mut cap = match DxgiCapturer::new(w, h, OutFormat::H264, 60, bitrate, std::env::var("PROBE_ENC").map(|v| v != "nvenc").unwrap_or(true)) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("[h264probe] 失败：H264 链路建不起来 → {e}");
            std::process::exit(2);
        }
    };
    println!(
        "[h264probe] 捕获器就绪：桌面 {}x{} → 输出 {:?}，GPU 缩放={}",
        cap.source_size().0,
        cap.source_size().1,
        cap.out_size(),
        cap.gpu_scaling()
    );
    if let Some(e) = cap.chain_error() {
        eprintln!("[h264probe] 链诊断：{e}");
    }

    let mut buf: Vec<u8> = Vec::new();
    let mut stream: Vec<u8> = Vec::with_capacity(4 << 20);
    let mut frames = 0usize;
    let mut nochange = 0usize;
    let mut errs = 0usize;
    let mut keyframes = 0usize;
    let mut first_nal: Vec<u8> = Vec::new();
    let mut nudged = false;
    let t0 = Instant::now();
    let budget = Duration::from_secs(secs as u64);

    while t0.elapsed() < budget {
        // 一点桌面变化是一个"帧"能否产生的前提。跑了一会儿还没几帧就试着逼桌面重绘一次。
        if frames < 3 && t0.elapsed() > Duration::from_millis(1200) && !nudged {
            nudged = true;
            unsafe {
                // hWnd = NULL → 使整个桌面失效，迫使 DWM 重新合成，从而产生新帧。
                let _ = windows::Win32::Graphics::Gdi::InvalidateRect(None, None, true);
            }
            println!("[h264probe] 已请求桌面重绘（桌面静止时不会产生新帧）");
        }

        match cap.grab(&mut buf) {
            Grab::Frame => {
                if buf.is_empty() {
                    // 编码器吞帧，正常现象
                    continue;
                }
                if frames == 0 {
                    first_nal = nal_types(&buf);
                    keyframes += usize::from(first_nal.contains(&5)) ;
                } else if buf.windows(5).any(|s| s[0] == 0 && s[1] == 0 && s[2] == 0 && s[3] == 1 && (s[4] & 0x1F) == 5)
                    || buf.windows(4).any(|s| s[0] == 0 && s[1] == 0 && s[2] == 1 && (s[3] & 0x1F) == 5)
                {
                    keyframes += 1;
                }
                frames += 1;
                stream.extend_from_slice(&buf);

                if kf_every > 0 && frames % kf_every == 0 {
                    cap.request_keyframe();
                }
            }
            Grab::NoChange => nochange += 1,
            Grab::Err(e) => {
                errs += 1;
                if errs <= 3 {
                    eprintln!("[h264probe] 取帧错误：{e}");
                }
            }
        }
        // 桌面静止时 AcquireNextFrame 自己会等（200ms 超时），不用额外 sleep
    }

    let el = t0.elapsed().as_secs_f64();
    let stats = cap.take_encoder_stats();

    println!("\n=========== 结果 ===========");
    println!("采集时长            : {el:.2}s");
    println!("产出帧数            : {frames}（其中关键帧起的 {keyframes}）");
    println!("无变化跳过          : {nochange} 次（桌面静止，设计如此）");
    println!("取帧错误            : {errs}");
    println!("码流总字节          : {}", stream.len());
    if let Some((n, bytes, us, idrs, mx)) = stats {
        let per = if n > 0 { us / n as u128 } else { 0 };
        println!("编码器账目          : {n} 帧 / IDR {idrs} 帧 / 平均 {per}us per 帧");
        println!("编码器输出          : {bytes} 字节，单帧最大 {mx} 字节");
        println!("编码实际码率        : {:.2} Mbps（目标 {:.2}）", bytes as f64 * 8.0 / el / 1e6, bitrate as f64 / 1e6);
        println!("编码器目标码率      : {}kbps", cap.encoder_bitrate() / 1000);
    }
    println!("首帧 NAL 类型序列   : {first_nal:?}");
    println!("  （7=SPS 8=PPS 5=IDR 1=非IDR切片；首帧含 7/8/5 才说明带内参数集齐全）");

    if let Ok(p) = std::env::var("PROBE_OUT") {
        match std::fs::write(&p, &stream) {
            Ok(()) => println!("码流已落盘: {p}（{} 字节）", stream.len()),
            Err(e) => eprintln!("落盘失败: {e}"),
        }
    }

    // 判定：有帧、有输出、首帧带 SPS+PPS+IDR
    let ok = frames > 0
        && stream.len() > 0
        && first_nal.contains(&7)
        && first_nal.contains(&8)
        && first_nal.contains(&5);
    if ok {
        println!("\n判定：**壳侧 NVENC H.264 链路可用** ✓");
    } else if frames == 0 {
        println!("\n判定：一帧都没产出 —— 极可能是桌面全程静止（不是链路故障）。");
        println!("       请让屏幕有点变化后重跑。");
        std::process::exit(3);
    } else {
        println!("\n判定：有帧但首帧参数集不全 ✗");
        std::process::exit(4);
    }
}

/// 扫 Annex-B 起始码，返回 nal_unit_type 序列（H.264：type = byte & 0x1F）。
fn nal_types(b: &[u8]) -> Vec<u8> {
    let mut v = Vec::new();
    let mut i = 0usize;
    while i + 3 < b.len() {
        if b[i] == 0 && b[i + 1] == 0 && b[i + 2] == 0 && b[i + 3] == 1 {
            v.push(b[i + 4] & 0x1F);
            i += 5;
        } else if b[i] == 0 && b[i + 1] == 0 && b[i + 2] == 1 {
            v.push(b[i + 3] & 0x1F);
            i += 4;
        } else {
            i += 1;
        }
    }
    v
}
