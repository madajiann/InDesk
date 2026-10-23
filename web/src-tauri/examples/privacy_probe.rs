//! [1.4.38 隐私屏探针] 直接驱动 privacy_overlay，供真机验证三件事：
//! 1. 遮罩窗存在且置顶（FindWindowW "InDeskPrivacyOverlay"）；
//! 2. GetWindowDisplayAffinity = WDA_EXCLUDEFROMCAPTURE(0x11)；
//! 3. 物理屏黑的同时，GDI 抓屏（ImageGrab/BitBlt）仍拿到真实画面 ——
//!    这是"本机黑屏、主控端照常看"的核心属性。
//!
//! 用法：privacy_probe on [hold_secs]   开启并保持 N 秒（默认 8）后自动关闭
//!       privacy_probe off              立即关闭
//!       privacy_probe state            只查当前是否开着（退出码 0=开）
#![allow(clippy::needless_range_loop)]

#[cfg(windows)]
fn main() {
    use indesk_desktop_lib::privacy_overlay;
    use std::time::Duration;

    let arg = std::env::args().nth(1).unwrap_or_default();

    match arg.as_str() {
        "on" => {
            let hold: u64 = std::env::args()
                .nth(2)
                .and_then(|s| s.parse().ok())
                .unwrap_or(8);
            let ok = privacy_overlay::set(true);
            println!("[probe] set(true) -> {}", ok);
            if ok {
                println!("[probe] holding {}s ...", hold);
                std::thread::sleep(Duration::from_secs(hold));
                println!("[probe] set(false) -> {}", privacy_overlay::set(false));
            }
        }
        "off" => {
            println!("[probe] set(false) -> {}", privacy_overlay::set(false));
        }
        _ => {
            println!("usage: privacy_probe on [secs] | off");
        }
    }
}

#[cfg(not(windows))]
fn main() {
    println!("windows only");
}
