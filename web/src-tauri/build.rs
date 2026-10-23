fn main() {
    // 壳构建时间戳（秒）：用于判断"热更包是否比当前壳新"。
    // 热更包应用时间 < 壳构建时间 ⇒ 视为过期，启动时删除并回落内置资源，
    // 避免"旧热更包锁死新壳"的死循环（2026-09-12 的 405 事故）。
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    println!("cargo:rustc-env=INDESK_BUILD_TS={ts}");
    tauri_build::build();
}
