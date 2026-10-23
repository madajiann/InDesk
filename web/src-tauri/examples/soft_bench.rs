//! [20261021 七轮] CPU 软编（OpenH264）链路自检 —— 不进安装包。
//!
//! ```text
//! cargo run --release --example soft_bench -- 1280 800 24 4000 120
//! ```
//! 参数：`宽 高 fps kbps 帧数`（都可省略）。
//!
//! 目的：在接进抓屏循环之前，先用**真实模块**（`soft264::SoftEncoder`）量出
//! 单帧耗时分布与码流 NAL 组成，确认：
//!   1. 关键帧（force IDR）输出里带 SPS(7)/PPS(8)，中途接入能解码；
//!   2. 每帧耗时 p95/max 是否收敛（不像 NVENC 那样出现 850ms 黑洞）。

use indesk_desktop_lib::soft264::SoftEncoder;
use std::time::Instant;

fn make_nv12(w: usize, h: usize, tick: u32) -> Vec<u8> {
    let mut buf = vec![128u8; w * h * 3 / 2];
    // 屏幕内容：横向条带 + 高频细线 + 一个移动方块
    for row in 0..h {
        let base = if row % 80 < 40 { 210u8 } else { 60u8 };
        for col in 0..w {
            let mut v = base;
            if col % 7 == 0 {
                v = v.wrapping_add(50);
            }
            if col % 97 == 0 || row % 53 == 0 {
                v = 250;
            }
            buf[row * w + col] = v;
        }
    }
    let (bx, by) = ((tick as usize * 7) % (w - 200), (tick as usize * 3) % (h - 200));
    for row in by..by + 200 {
        for col in bx..bx + 200 {
            buf[row * w + col] = 128;
        }
    }
    buf
}

fn main() {
    let a: Vec<String> = std::env::args().skip(1).collect();
    let w: usize = a.first().and_then(|s| s.parse().ok()).unwrap_or(1280);
    let h: usize = a.get(1).and_then(|s| s.parse().ok()).unwrap_or(800);
    let fps: u32 = a.get(2).and_then(|s| s.parse().ok()).unwrap_or(24);
    let kbps: u32 = a.get(3).and_then(|s| s.parse().ok()).unwrap_or(4000);
    let n: usize = a.get(4).and_then(|s| s.parse().ok()).unwrap_or(120);

    let mut enc = SoftEncoder::new(w as u32, h as u32, fps, kbps * 1000).unwrap();
    println!("软编 {w}x{h} 目标 {fps}fps {kbps}kbps，{n} 帧");

    let mut out = Vec::new();
    let mut times = Vec::with_capacity(n);
    let mut total_bytes = 0usize;
    let mut max_bytes = 0usize;
    let mut key_bytes = Vec::new();
    let wall = Instant::now();

    for i in 0..n {
        if i == n / 2 {
            enc.request_idr();
        }
        let frame = make_nv12(w, h, i as u32);
        let t0 = Instant::now();
        let st = enc.encode_nv12(&frame, &mut out).unwrap();
        let us = t0.elapsed().as_micros();
        times.push(us);
        if let Some(st) = st {
            total_bytes += st.bytes;
            max_bytes = max_bytes.max(st.bytes);
            if st.is_idr {
                key_bytes.push(st.bytes);
            }
        }
    }

    let elapsed = wall.elapsed().as_secs_f64();
    let mut sorted = times.clone();
    sorted.sort();
    let avg = sorted.iter().sum::<u128>() as f64 / sorted.len() as f64 / 1000.0;
    let p50 = sorted[sorted.len() / 2] as f64 / 1000.0;
    let p95 = sorted[(sorted.len() as f64 * 0.95) as usize] as f64 / 1000.0;
    let max = *sorted.last().unwrap() as f64 / 1000.0;
    println!(
        "单帧: avg={avg:.1}ms p50={p50:.1}ms p95={p95:.1}ms max={max:.1}ms → 理论上限 {:.0}fps",
        1000.0 / avg
    );
    println!(
        "码流: 总 {:.0}KB 平均 {:.0}B/帧 最大 {max_bytes}B 关键帧 {:?}B 实际 {:.1}fps",
        total_bytes as f64 / 1024.0,
        total_bytes as f64 / n as f64,
        key_bytes,
        n as f64 / elapsed
    );
}
