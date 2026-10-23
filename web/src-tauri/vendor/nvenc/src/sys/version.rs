//! Contains the nvenc version numbers, encoded as they are in C, for the version this crate was built against

// ─────────────────────────────────────────────────────────────────────────────
// [InDesk 内联补丁] 原 crate 声明 NVENC API 13.0，但本机 NVIDIA 驱动（31.0.15.3623 /
// 536.23）支持上限是 12.1，`nvEncOpenEncodeSessionEx` 会因 apiVersion 过高直接
// 返回 NV_ENC_ERR_INVALID_VERSION。已降到本机驱动能力上限（12.1），与
// nv-codec-headers 的 n12.1.14.0 一致。
// 版本号编码（头文件原文）：NVENCAPI_VERSION = MAJOR | (MINOR << 24)
// 而驱动 NvEncodeAPIGetMaxSupportedVersion 的回传是**另一种**编码：
//   低 4 位 = minor，其余位 = major（实测本机回传 0xC1 → 12.1）
// 两者不可混用 —— 故提供 from_driver_max() 做换算。
// ─────────────────────────────────────────────────────────────────────────────

pub const NVENC_MAJOR_VERSION: u16 = 12;
pub const NVENC_MINOR_VERSION: u8 = 1;

pub const NVENC_API_VERSION: u32 = NVENC_MAJOR_VERSION as u32 | (NVENC_MINOR_VERSION as u32) << 24;

pub const NVENC_API_LIST_VERSION: u32 = struct_version(2);

pub const fn struct_version(nvenc_api_version: u8) -> u32 {
    NVENC_API_VERSION | (nvenc_api_version as u32) << 16 | 0x7 << 28
}

/// 把 `NvEncodeAPIGetMaxSupportedVersion` 的回传值换算成 `NVENCAPI_VERSION` 编码。
///
/// 驱动回传：低 4 位为 minor，其余为 major。本函数还原成 `major | (minor << 24)`。
pub const fn from_driver_max(raw: u32) -> u32 {
    let minor = raw & 0x0F;
    let major = (raw >> 4) & 0x00FF_FFFF;
    major | (minor << 24)
}

/// 驱动回传值的大版本是否为 `TARGET_MAJOR`——crate 按 12.x 布局编译，只有 12.x 驱动兼容。
pub const fn driver_supports_this(raw: u32) -> bool {
    ((raw >> 4) & 0x00FF_FFFF) == 12u32
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn driver_encoding_roundtrip() {
        // 实测本机驱动回传 0xC1 → 12.1
        assert_eq!(from_driver_max(0xC1), NVENC_API_VERSION);
        assert!(driver_supports_this(0xC1));
        // 13.0 驱动（0xD0）不满足本 crate 的 12.x 布局约束判断
        assert!(!driver_supports_this(0xD0));
    }
}
