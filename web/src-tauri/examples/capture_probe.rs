//! 抓屏链路自检工具（仅开发用，不进安装包）：
//!
//! ```text
//! cargo run --release --example capture_probe
//! ```
//!
//! 校验「GDI 抓屏 → JPEG → 本机 MJPEG」整条链路，并打印实测帧率与首帧大小。
//! 任何一环坏掉（抓屏失败 / 编码失败 / 服务不通）都会在这里直接暴露，
//! 不会等到用户勾了无人值守才发现是黑屏。

use std::io::{Read, Write};
use std::net::TcpStream;
use std::time::{Duration, Instant};

fn main() {
    let opt = indesk_desktop_lib::capture::StartOptions {
        max_width: 0,
        quality: 70,
        fps: 15,
    };
    let (port, w, h) = indesk_desktop_lib::capture::start(opt).expect("取屏启动失败");
    println!("[probe] 服务端口={port} 输出尺寸={w}x{h}");

    let mut s = TcpStream::connect(("127.0.0.1", port)).expect("连接 MJPEG 服务失败");
    s.set_read_timeout(Some(Duration::from_secs(3))).ok();
    s.write_all(b"GET /mjpeg HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\n")
        .expect("发送请求失败");

    let mut buf = vec![0u8; 128 * 1024];
    let t0 = Instant::now();
    let mut total = 0usize;
    let mut jpegs = 0usize;
    let mut first_len = 0usize;
    let mut first_seen = false;
    // 首帧完整长度：从响应头结束到第一个 EOI
    let mut header_done = false;
    let mut acc: Vec<u8> = Vec::new();

    while t0.elapsed() < Duration::from_secs(5) {
        match s.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => {
                total += n;
                acc.extend_from_slice(&buf[..n]);
                if !header_done {
                    if let Some(p) = find(&acc, b"\r\n\r\n") {
                        acc.drain(..p + 4);
                        header_done = true;
                    }
                }
                if header_done {
                    if let Some(soi) = find(&acc, &[0xff, 0xd8]) {
                        if let Some(eoi) = find_from(&acc, &[0xff, 0xd9], soi + 2) {
                            if !first_seen {
                                first_len = eoi + 2 - soi;
                                first_seen = true;
                                // 顺便落盘一帧，人工可打开确认是桌面画面
                                let path = std::env::temp_dir().join("indesk-probe-first-frame.jpg");
                                let _ = std::fs::write(&path, &acc[soi..eoi + 2]);
                                println!("[probe] 首帧已存盘: {}", path.display());
                            }
                            jpegs += 1;
                            acc.drain(..eoi + 2);
                        }
                    }
                    // 防止缓冲区无限增长
                    if acc.len() > 4 * 1024 * 1024 {
                        acc.drain(..acc.len() - 1024);
                    }
                }
            }
            Err(_) => break,
        }
    }

    let secs = t0.elapsed().as_secs_f64();
    println!(
        "[probe] {secs:.2}s 收到 {total} 字节 / {jpegs} 帧 / 首帧 {first_len} 字节 → 实测 {:.1} fps",
        jpegs as f64 / secs
    );

    indesk_desktop_lib::capture::stop();

    if jpegs <= 5 {
        eprintln!("[probe] 失败：5 秒内仅 {jpegs} 帧，取屏链路异常");
        std::process::exit(1);
    }
    if first_len < 1024 {
        eprintln!("[probe] 失败：首帧仅 {first_len} 字节，疑似空白画面");
        std::process::exit(1);
    }
    println!("[probe] OK：抓屏链路正常");
}

fn find(hay: &[u8], needle: &[u8]) -> Option<usize> {
    find_from(hay, needle, 0)
}

fn find_from(hay: &[u8], needle: &[u8], from: usize) -> Option<usize> {
    if needle.is_empty() || hay.len() < needle.len() {
        return None;
    }
    (from..=hay.len() - needle.len()).find(|&i| &hay[i..i + needle.len()] == needle)
}
