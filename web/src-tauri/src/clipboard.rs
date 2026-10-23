//! [1.4.5 剪贴板同步] 壳侧剪贴板读写 —— **只做纯文本**，全走 Win32 原语。
//!
//! ## 为什么自己写而不用现成 crate
//!
//! 只需要「读一段文本 / 写一段文本 / 问一下有没有变」三件事。为这三件事引入
//! `arboard`（会拖进 image、objc 等一堆依赖）或 `clipboard-win`（会引入它自己的
//! 窗口/COM 生命周期管理），换来的复杂度远大于收益；而本项目对壳侧的要求是
//! **零新增行为风险** —— 现成 crate 在别的进程占用剪贴板时的重试策略、
//! 释放内存的时机，都是我们无法审计的黑盒。
//!
//! ## ⚠️ 四个陷阱（都踩过/查证过，改代码前先读完）
//!
//! ① **`CF_UNICODETEXT` 是 `u16`，而剪贴板 API 收 `u32`**。
//!    windows-sys 0.59 里 `pub const CF_UNICODETEXT: CLIPBOARD_FORMAT = 13u16`，
//!    而 `GetClipboardData(uformat: u32)`。不显式转换**直接编译失败**（这是好事），
//!    所以这里统一走 `CF_TEXT_U32`，只在一处转换。
//!
//! ② **`SetClipboardData` 成功后内存所有权转移给系统**，绝不能再 `GlobalFree` ——
//!    否则系统随后读到已释放内存（轻则粘贴出乱码，重则整机粘贴功能崩掉）。
//!    反过来：`SetClipboardData` **失败**时必须自己 `GlobalFree`，否则每次失败漏一块内存。
//!    本文件严格按这个二选一写（见 `write_text` 里的注释）。
//!
//! ③ **`OpenClipboard` 失败是常态而不是异常**：浏览器、输入法、任何正在复制的程序
//!    都可能短暂持有剪贴板。必须重试。这里 8 × 15ms = 120ms 上限：
//!    再长会拖住 IPC 调用，再短会漏掉正常竞争。
//!
//! ④ **读长度必须有上限**。剪贴板里可以有几百 MB 的文本（异常程序或被恶意占用），
//!    不设限就是把整个进程的内存交出去。`READ_MAX_CHARS` 是硬上限，
//!    超长会被截断并**在返回值里标出来**（前端据此拒绝同步，而不是偷偷同步半截）。
//!
//! ## 本文件刻意**不引用 `crate::`**
//!
//! 于是 `G:\InDesk\lab` 的取证 bin 可以用 `#[path = "..."] mod clipboard;`
//! 直接编译**这一份源码**并真机运行。也就是说："取证时跑通的代码" 与
//! "打进安装包的代码" 是同一个文件，不存在"验证过的和发布的是两份实现"这种走样。
//! （前端 `clipboardSync.ts` 用的是同一套纪律，理由见该文件头。）
//!
//! ## 边界（有意为之，不是没做完）
//!
//! - 只处理 `CF_UNICODETEXT`。图片、文件列表、HTML 一律视为「没有文本」，
//!   **不动对端剪贴板** —— 把对端的剪贴板清空是比"没同步"严重得多的故障。
//! - 不监听 `WM_CLIPBOARDUPDATE`（那需要一条消息循环 + 一个窗口）。
//!   前端按 ~700ms 轮询 `GetClipboardSequenceNumber`，这个调用**不需要打开剪贴板**、
//!   是几微秒级的取值，代价可忽略。

#[cfg(windows)]
mod win {
    use std::sync::Mutex;
    use std::time::Duration;

    use windows_sys::Win32::Foundation::{GlobalFree, HGLOBAL};
    use windows_sys::Win32::System::DataExchange::{
        CloseClipboard, EmptyClipboard, GetClipboardData, GetClipboardSequenceNumber,
        IsClipboardFormatAvailable, OpenClipboard, SetClipboardData,
    };
    use windows_sys::Win32::System::Memory::{
        GlobalAlloc, GlobalLock, GlobalSize, GlobalUnlock, GMEM_MOVEABLE,
    };
    use windows_sys::Win32::System::Ole::{CF_HDROP, CF_UNICODETEXT};
    use windows_sys::Win32::UI::Shell::{DragQueryFileW, HDROP};

    /// 单次读回的最大码元数（UTF-16 code unit）。前端只同步 2 万字符，
    /// 这里留 50× 冗余；设上限只为防"几百 MB 文本吃光内存"（陷阱④）。
    pub const READ_MAX_CHARS: usize = 1_000_000;

    /// 进程内串行化（陷阱③的补强）。
    ///
    /// 两个 IPC 调用并发进来时，后到的 `OpenClipboard` 必然失败 —— 靠重试也能过，
    /// 但要在调用线程上白等最多 120ms。用一把互斥锁把「开→读/写→关」整段串起来，
    /// 就把这种竞争变成了排队，行为可预期。
    ///
    /// 用 `Mutex<()>` 而不是 `RwLock`：临界区里有写操作，读写锁没有意义。
    /// 中毒（`PoisonError`）不 panic —— 剪贴板操作失败不该让整个进程的服务降级，
    /// 拿回内部数据继续用即可。
    static LOCK: Mutex<()> = Mutex::new(());

    /// 陷阱①：唯一的 `u16 → u32` 转换点，避免每个调用点各写一次 `as u32`。
    const CF_TEXT_U32: u32 = CF_UNICODETEXT as u32;

    /// [20261022] CF_HDROP（文件列表）同样是 `u16` 常量、`u32` 入参。
    const CF_HDROP_U32: u32 = CF_HDROP as u32;

    /// 取剪贴板序列号。**不打开剪贴板**，可高频调用（前端轮询就用它）。
    ///
    /// 语义：系统级单调计数器，任何一次 `SetClipboardData` 都会让它 +1。
    /// 跨进程也成立 —— 别的程序复制东西，这个数字照样会变。
    /// 返回值 0 表示调用失败（用户会话未连接等），前端把它当作「暂时不可用」。
    pub fn sequence() -> u32 {
        unsafe { GetClipboardSequenceNumber() }
    }

    /// 一次读的快照。
    #[derive(Debug, Clone)]
    pub struct ClipSnapshot {
        /// 文本内容。`None` = 剪贴板里**没有可用文本**（空的/只有图片/只有文件）。
        pub text: Option<String>,
        /// 读取时的序列号，供前端做"这次读的是哪一版"的记账。
        pub seq: u32,
        /// 原始码元数（截断前）。`len` > `READ_MAX_CHARS` 即为「超长被截断」。
        pub raw_len: usize,
        /// 是否因为超过 `READ_MAX_CHARS` 而被截断。
        pub truncated: bool,
        /// 失败原因（`None` = 成功走完了流程，与「有没有文本」无关）。
        pub error: Option<String>,
    }

    impl ClipSnapshot {
        fn err(seq: u32, msg: impl Into<String>) -> Self {
            Self {
                text: None,
                seq,
                raw_len: 0,
                truncated: false,
                error: Some(msg.into()),
            }
        }
    }

    /// 尝试打开剪贴板（含重试，陷阱③）。
    fn open_retry() -> bool {
        for _ in 0..8 {
            if unsafe { OpenClipboard(std::ptr::null_mut()) } != 0 {
                return true;
            }
            std::thread::sleep(Duration::from_millis(15));
        }
        false
    }

    /// 读剪贴板文本快照。
    ///
    /// 不会因为「剪贴板里没有文本」而报错 —— 那是**正常状态**，
    /// 用 `text: None` 表达，前端据此选择"什么都不做"。
    /// `error` 只用于表达真正的失败（打不开剪贴板）。
    pub fn read_snapshot() -> ClipSnapshot {
        let seq = sequence();
        let _guard = match LOCK.lock() {
            Ok(g) => g,
            Err(p) => p.into_inner(),
        };
        if !open_retry() {
            return ClipSnapshot::err(seq, "OpenClipboard 失败（被其他进程占用）");
        }
        // 从这里开始，任何 return 之前都必须 CloseClipboard —— 用闭包包住，
        // 让"关"只有一个出口，避免以后加分支时漏掉一条路径（那会把剪贴板
        // 永久锁在本进程手里，症状是"全系统都不能复制了"）。
        let out = read_locked();
        unsafe { CloseClipboard() };
        out
    }

    /// 剪贴板**已打开**时读取（调用者负责关闭）。
    fn read_locked() -> ClipSnapshot {
        let seq = sequence();
        // 先问格式：没有文本格式就根本不该去 Get 数据（也避免把图片塞进 String）
        if unsafe { IsClipboardFormatAvailable(CF_TEXT_U32) } == 0 {
            return ClipSnapshot {
                text: None,
                seq,
                raw_len: 0,
                truncated: false,
                error: None,
            };
        }
        let handle = unsafe { GetClipboardData(CF_TEXT_U32) };
        if handle.is_null() {
            return ClipSnapshot {
                text: None,
                seq,
                raw_len: 0,
                truncated: false,
                error: None,
            };
        }
        let hmem = handle as HGLOBAL;
        let ptr = unsafe { GlobalLock(hmem) } as *const u16;
        if ptr.is_null() {
            return ClipSnapshot::err(seq, "GlobalLock 失败");
        }
        // 可读字节数由 GlobalSize 给出。**不能靠 strlen** —— UTF-16 内容里
        // 可能有嵌入的 NUL，也可能被系统加上尾部对齐填充。
        let bytes = unsafe { GlobalSize(hmem) };
        let units = bytes / 2;
        // 扫描到第一个 NUL 或上限为止。逐码元扫描是 O(n)，但剪贴板文本
        // 通常只有几十~几千码元，且这个函数只在序列号变化时被调用。
        let mut len = 0usize;
        while len < units && len < READ_MAX_CHARS {
            let ch = unsafe { *ptr.add(len) };
            if ch == 0 {
                break;
            }
            len += 1;
        }
        let slice = unsafe { std::slice::from_raw_parts(ptr, len) };
        let text = String::from_utf16_lossy(slice);
        let truncated = len >= READ_MAX_CHARS;
        unsafe { GlobalUnlock(hmem) };
        ClipSnapshot {
            text: Some(text),
            seq,
            raw_len: len,
            truncated,
            error: None,
        }
    }

    /// 把文本写进剪贴板。成功返回写入后的序列号。
    ///
    /// 空字符串会被**拒绝**：让对端剪贴板变成空，是比"没同步"严重得多的结果，
    /// 而且空剪贴板本来就没有"内容变了"这回事（前端侧的判据同样跳过空文本）。
    pub fn write_text(s: &str) -> Result<u32, String> {
        if s.is_empty() {
            return Err("拒绝写入空文本".into());
        }
        let _guard = match LOCK.lock() {
            Ok(g) => g,
            Err(p) => p.into_inner(),
        };
        if !open_retry() {
            return Err("OpenClipboard 失败（被其他进程占用）".into());
        }
        let out = write_locked(s);
        unsafe { CloseClipboard() };
        out.map(|_| sequence())
    }

    /// 剪贴板**已打开**时写入（调用者负责关闭）。
    fn write_locked(s: &str) -> Result<(), String> {
        // EmptyClipboard：清掉旧内容。清空后如果后面任何一步失败，
        // 剪贴板会停在"空"的状态 —— 这正是"必须先分配好再清空"的理由。
        // 所以顺序是：**先分配内存并填好内容，再 Empty，最后 Set**。
        // （原版写法是先 Empty 再分配，一旦分配失败就把用户的剪贴板清没了。）
        let wide: Vec<u16> = s.encode_utf16().chain(std::iter::once(0)).collect();
        let bytes = wide.len() * 2;
        let hmem = unsafe { GlobalAlloc(GMEM_MOVEABLE, bytes) } as HGLOBAL;
        if hmem.is_null() {
            return Err(format!("GlobalAlloc 失败（{bytes} 字节）"));
        }
        let dst = unsafe { GlobalLock(hmem) } as *mut u16;
        if dst.is_null() {
            // 分配成功但锁失败：这块内存归我们，必须自己释放
            unsafe { GlobalFree(hmem) };
            return Err("GlobalLock 失败".into());
        }
        unsafe {
            std::ptr::copy_nonoverlapping(wide.as_ptr(), dst, wide.len());
            GlobalUnlock(hmem);
        }

        if unsafe { EmptyClipboard() } == 0 {
            unsafe { GlobalFree(hmem) };
            return Err("EmptyClipboard 失败".into());
        }
        // 陷阱②：这一行之后，内存所有权归系统。
        let ret = unsafe { SetClipboardData(CF_TEXT_U32, hmem) };
        if ret.is_null() {
            // 失败 = 系统没有接管 → 我们自己释放，否则漏一块内存
            unsafe { GlobalFree(hmem) };
            return Err("SetClipboardData 失败".into());
        }
        // 成功 = 系统接管 → **绝对不能 GlobalFree**（陷阱②）
        Ok(())
    }

    // ---------------------------------------------------------- 文件列表（CF_HDROP）
    //
    // [20261022 剪贴板文件同步] 与文本是**完全不同的数据形状**：文本是一段
    // 字符串，文件是一块 `DROPFILES` 结构 + 一串双 NUL 结尾的路径。
    //
    // ⚠️ 三个新坑（查证过，改之前先读完）：
    // ① `DragQueryFileW` 的 `ifile = 0xFFFFFFFF` 才是「问有多少个文件」，
    //    不是取第 4G 个。必须先问数量再逐个取名，没有别的正确用法。
    // ② **剪贴板里的 HDROP 句柄属系统所有**，读完绝不能 `DragFinish` ——
    //    谁 Finish 谁就在释放系统内存，用户随后 Ctrl+V 会拿到野指针
    //    （症状：粘贴时资源管理器崩）。只有自己 `DragAcceptFiles` 得来的
    //    句柄才需要 Finish。
    // ③ 写文件列表必须 `fWide = TRUE` + UTF-16 双 NUL 结尾。用 ANSI 写，
    //    中文路径到对端就是乱码（fWide = FALSE 时系统按 ACP 解释字节）。

    /// 单次最多枚举/写入的文件数：防有程序往剪贴板塞几万个文件把 IPC 拖死。
    pub const FILES_MAX: usize = 200;

    /// 文件列表快照。
    #[derive(Debug, Clone)]
    pub struct FileSnapshot {
        /// 绝对路径列表。空 = 剪贴板里没有文件（纯文本/图片/空都算没有）。
        pub files: Vec<String>,
        pub seq: u32,
        /// 超过 `FILES_MAX` 被截断（前端据此提示"文件过多"而不是静默只传一半）。
        pub truncated: bool,
        pub error: Option<String>,
    }

    fn no_files(seq: u32) -> FileSnapshot {
        FileSnapshot {
            files: Vec::new(),
            seq,
            truncated: false,
            error: None,
        }
    }

    /// 读剪贴板里的文件列表（不报错 = 没有文件，是正常状态）。
    pub fn read_files() -> FileSnapshot {
        let seq = sequence();
        let _guard = match LOCK.lock() {
            Ok(g) => g,
            Err(p) => p.into_inner(),
        };
        if !open_retry() {
            return FileSnapshot {
                files: Vec::new(),
                seq,
                truncated: false,
                error: Some("OpenClipboard 失败（被其他进程占用）".into()),
            };
        }
        let out = read_files_locked();
        unsafe { CloseClipboard() };
        out
    }

    fn read_files_locked() -> FileSnapshot {
        let seq = sequence();
        if unsafe { IsClipboardFormatAvailable(CF_HDROP_U32) } == 0 {
            return no_files(seq);
        }
        let handle = unsafe { GetClipboardData(CF_HDROP_U32) };
        if handle.is_null() {
            return no_files(seq);
        }
        let hdrop = handle as HDROP;
        // 坑①：0xFFFFFFFF = 问数量
        let total = unsafe { DragQueryFileW(hdrop, 0xFFFF_FFFF, std::ptr::null_mut(), 0) } as usize;
        let truncated = total > FILES_MAX;
        let mut files = Vec::with_capacity(total.min(FILES_MAX));
        for i in 0..total.min(FILES_MAX) {
            // 先问这条路径要多少码元（不含结尾 NUL），再按长度取
            let len = unsafe { DragQueryFileW(hdrop, i as u32, std::ptr::null_mut(), 0) } as usize;
            if len == 0 {
                continue;
            }
            let mut buf = vec![0u16; len + 1];
            let got = unsafe {
                DragQueryFileW(hdrop, i as u32, buf.as_mut_ptr(), (len + 1) as u32)
            } as usize;
            if got == 0 {
                continue;
            }
            files.push(String::from_utf16_lossy(&buf[..got]));
        }
        // 坑②：不 DragFinish（句柄属系统）
        FileSnapshot {
            files,
            seq,
            truncated,
            error: None,
        }
    }

    /// 把文件列表写进剪贴板 —— 用户随后在资源管理器里 Ctrl+V 就能粘出来。
    pub fn write_files(paths: &[String]) -> Result<u32, String> {
        if paths.is_empty() {
            return Err("拒绝写入空文件列表".into());
        }
        if paths.len() > FILES_MAX {
            return Err(format!("文件过多（{} > {FILES_MAX}）", paths.len()));
        }
        let _guard = match LOCK.lock() {
            Ok(g) => g,
            Err(p) => p.into_inner(),
        };
        if !open_retry() {
            return Err("OpenClipboard 失败（被其他进程占用）".into());
        }
        let out = write_files_locked(paths);
        unsafe { CloseClipboard() };
        out.map(|_| sequence())
    }

    fn write_files_locked(paths: &[String]) -> Result<(), String> {
        // DROPFILES 布局（x64 也是 20 字节）：
        //   DWORD pFiles; POINT pt{2×LONG}; BOOL fNC; BOOL fWide;
        const HDR: usize = 20;
        let mut wide: Vec<u16> = Vec::new();
        for p in paths {
            wide.extend(p.encode_utf16());
            wide.push(0);
        }
        wide.push(0); // 整个列表的终止 NUL（坑③：双 NUL）
        let bytes = HDR + wide.len() * 2;
        let hmem = unsafe { GlobalAlloc(GMEM_MOVEABLE, bytes) } as HGLOBAL;
        if hmem.is_null() {
            return Err(format!("GlobalAlloc 失败（{bytes} 字节）"));
        }
        let base = unsafe { GlobalLock(hmem) } as *mut u8;
        if base.is_null() {
            unsafe { GlobalFree(hmem) };
            return Err("GlobalLock 失败".into());
        }
        unsafe {
            std::ptr::write_bytes(base, 0, HDR);
            // pFiles: 文件列表相对结构体起点的偏移
            *(base as *mut u32) = HDR as u32;
            // pt = (0,0)、fNC = FALSE 由上面的清零保证
            // fWide = TRUE（偏移 16）
            *(base.add(16) as *mut i32) = 1;
            std::ptr::copy_nonoverlapping(
                wide.as_ptr() as *const u8,
                base.add(HDR),
                wide.len() * 2,
            );
            GlobalUnlock(hmem);
        }
        if unsafe { EmptyClipboard() } == 0 {
            unsafe { GlobalFree(hmem) };
            return Err("EmptyClipboard 失败".into());
        }
        let ret = unsafe { SetClipboardData(CF_HDROP_U32, hmem) };
        if ret.is_null() {
            unsafe { GlobalFree(hmem) };
            return Err("SetClipboardData 失败".into());
        }
        Ok(())
    }
}

#[cfg(windows)]
pub use win::{
    read_files, read_snapshot, sequence, write_files, write_text, ClipSnapshot, FileSnapshot,
    FILES_MAX, READ_MAX_CHARS,
};

#[cfg(not(windows))]
mod other {
    /// 非 Windows 平台占位：剪贴板同步只在 Windows 壳上提供。
    pub const READ_MAX_CHARS: usize = 0;

    #[derive(Debug, Clone)]
    pub struct ClipSnapshot {
        pub text: Option<String>,
        pub seq: u32,
        pub raw_len: usize,
        pub truncated: bool,
        pub error: Option<String>,
    }

    pub fn sequence() -> u32 {
        0
    }

    pub fn read_snapshot() -> ClipSnapshot {
        ClipSnapshot {
            text: None,
            seq: 0,
            raw_len: 0,
            truncated: false,
            error: Some("当前平台不支持剪贴板同步".into()),
        }
    }

    pub fn write_text(_s: &str) -> Result<u32, String> {
        Err("当前平台不支持剪贴板同步".into())
    }

    /// [20261022] 非 Windows 桩：文件列表同样不支持。
    pub const FILES_MAX: usize = 0;

    #[derive(Debug, Clone)]
    pub struct FileSnapshot {
        pub files: Vec<String>,
        pub seq: u32,
        pub truncated: bool,
        pub error: Option<String>,
    }

    pub fn read_files() -> FileSnapshot {
        FileSnapshot {
            files: Vec::new(),
            seq: 0,
            truncated: false,
            error: Some("当前平台不支持剪贴板文件同步".into()),
        }
    }

    pub fn write_files(_paths: &[String]) -> Result<u32, String> {
        Err("当前平台不支持剪贴板文件同步".into())
    }
}

#[cfg(not(windows))]
pub use other::{
    read_files, read_snapshot, sequence, write_files, write_text, ClipSnapshot, FileSnapshot,
    FILES_MAX, READ_MAX_CHARS,
};
