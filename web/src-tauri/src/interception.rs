//! [§22 · 20260917] 驱动级注入：Interception 虚拟键鼠过滤驱动。
//!
//! ## 根因（本机全天实测取证）
//! 游戏客户端「我的天真」自带 nProtect GameGuard 保护（`D:\我的天真\GameGuard\`
//! 下 GameMon.des / GameMon64.des / GGKernel64.sys），其内核层过滤：
//!   1. 游戏运行期间：丢弃所有带 LLMHF_INJECTED 标记的合成鼠标**按钮**事件（移动放行）；
//!   2. 更狠：**异步 posted 键盘类消息（WM_CHAR/WM_KEYDOWN）全系统剥离**——PostMessage
//!      返回成功但零效果（同步 SendMessage 正常，说明通道本身没坏，是队列层过滤）；
//!   3. 键盘 SendInput 注入同样被吞（GameMon 驻留期间，游戏关了也吞）。
//! 用户态三条路（SendInput / PostMessage / 覆盖层）全灭。A/B 对照：**ToDesk 能控该
//! 游戏**（用户 20260917 实测），证明「硬件级输入」拦不住 —— ToDesk 内部即虚拟设备
//! 驱动注入。本模块复刻同一条路：Interception 开源过滤驱动（oblitum/Interception），
//! 注入事件从键盘/鼠标类驱动栈上层进入，与真硬件无差别，无 LLMHF_INJECTED 标记。
//!
//! ## 依赖
//! - `install-interception.exe /install` 已执行（20260917 13:16 落盘
//!   system32\drivers\keyboard.sys + mouse.sys 过滤版，重启后生效）
//! - `interception.dll`（11KB，随壳部署在 exe 同目录）
//! - 卸载：`install-interception.exe /uninstall` + 重启，干干净净
//!
//! ## 设计
//! - `available()`：懒加载 DLL + create_context，失败永久缓存"不可用"（零开销回退）
//! - 按钮/滚轮/按键走驱动；**移动仍走 SendInput**（移动未被拦，且高频移动无必要过驱动）
//! - 驱动不可用时回退既有链路（§21 PostMessage 兼容 / SendInput）
//! - 驱动可用时**跳过 §21 兼容探测**（探测本身是一次 SendInput，省掉）

#![cfg(windows)]

use std::ffi::c_void;
use std::io::{Read, Write};
use std::sync::atomic::{AtomicBool, AtomicIsize, Ordering};
use std::sync::Mutex;

/// Interception 端口号规则：INTERCEPTION_KEYBOARD(n) = n + 1（1..=10）；
/// INTERCEPTION_MOUSE(n) = 10 + n + 1（11..=20）。但具体哪个口有设备必须运行期枚举（§27）。

/// 鼠标 state 常量（interception.h InterceptionMouseState）
const MS_LEFT_DOWN: u16 = 0x001;
const MS_LEFT_UP: u16 = 0x002;
const MS_RIGHT_DOWN: u16 = 0x004;
const MS_RIGHT_UP: u16 = 0x008;
const MS_WHEEL: u16 = 0x400;
const MS_HWHEEL: u16 = 0x800;

/// 按键 state 常量（InterceptionKeyState）：DOWN=0 / UP=1 / E0=2
const KEY_UP: u16 = 0x001;
const KEY_E0: u16 = 0x002;

/// InterceptionMouseStroke：state(2) flags(2) rolling(2) x(4) y(4) information(4)，
/// 对齐后 sizeof = 20（x 从 offset 8 开始）。缓冲区统一 20 字节，键盘 stroke 用前 8 字节。
const STROKE_SIZE: usize = 20;

#[repr(C)]
#[derive(Clone, Copy)]
struct MouseStroke {
    state: u16,
    flags: u16,
    rolling: i16,
    x: i32,
    y: i32,
    information: u32,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct KeyStroke {
    code: u16,
    state: u16,
    information: u32,
}

/// 联合体必须与 C 端 20 字节缓冲一致；键盘 stroke 只占前 8 字节。
#[repr(C)]
union StrokeBuf {
    m: MouseStroke,
    k: KeyStroke,
    raw: [u8; STROKE_SIZE],
}

type CreateContextFn = unsafe extern "system" fn() -> *mut c_void;
type DestroyContextFn = unsafe extern "system" fn(*mut c_void);
type SendFn = unsafe extern "system" fn(*mut c_void, i32, *const StrokeBuf, u32) -> i32;
type PredicateFn = unsafe extern "system" fn(i32) -> i32;
type SetFilterFn = unsafe extern "system" fn(*mut c_void, PredicateFn, u16);
type GetHardwareIdFn = unsafe extern "system" fn(*mut c_void, i32, *mut u16, u32) -> i32;

struct Api {
    _lib: isize, // HMODULE（保持装载）
    ctx: *mut c_void,
    send: SendFn,
    _destroy: DestroyContextFn,
    _set_filter: SetFilterFn,
    _get_hwid: GetHardwareIdFn,
    /// 实际抓到设备的鼠标端口（11..=20）。[§27] 关键：端口号不是固定的！
    /// RDP/远程桌面鼠标设备等会先占走 MOUSE(0)=11，USB 鼠标落位 12——
    /// 本机实测（20260917 17:34）：11 空置、12 才是真鼠标，写死 11 = 点击全部空放。
    mouse_ports: Vec<i32>,
    /// 实际抓到设备的键盘端口（1..=10）
    kbd_ports: Vec<i32>,
}

unsafe impl Send for Api {}
unsafe impl Sync for Api {}

/// [§30] 助手进程后端：壳(GUI)被 GameGuard 的 npggNT64.des 注入后进程内
/// create_context 恒失败（实测 18:36/18:47 两轮 10 连败），而极简无窗口的
/// 助手进程不被拦截。上下文放助手进程里，壳经 stdin/stdout 管道投递 stroke。
struct HelperProc {
    child: Mutex<std::process::Child>,
    mouse_ports: Vec<i32>,
    kbd_ports: Vec<i32>,
}

unsafe impl Send for HelperProc {}
unsafe impl Sync for HelperProc {}

impl HelperProc {
    /// 助手子进程是否还活着（被反外挂清掉后必须立刻作废，否则管道断裂=会话级假死）。
    fn alive(&self) -> bool {
        let mut child = self.child.lock().unwrap_or_else(|e| e.into_inner());
        !matches!(child.try_wait(), Ok(Some(_)))
    }

    /// 发一批 stroke，返回 send 是否全部返回 1。协议见 interception_helper 源码。
    fn send_strokes(&self, port: i32, strokes: &[[u8; 20]]) -> bool {
        let mut child = self.child.lock().unwrap_or_else(|e| e.into_inner());
        let Some(stdin) = child.stdin.as_mut() else { return false };
        let mut buf = Vec::with_capacity(16 + strokes.len() * 20);
        buf.extend_from_slice(&0x49444B53u32.to_le_bytes()); // "SKDI"
        buf.extend_from_slice(&port.to_le_bytes());
        buf.extend_from_slice(&(strokes.len() as u32).to_le_bytes());
        buf.extend_from_slice(&0u32.to_le_bytes());
        for s in strokes {
            buf.extend_from_slice(s);
        }
        if stdin.write_all(&buf).and_then(|_| stdin.flush()).is_err() {
            return false;
        }
        let mut out = [0u8; 1];
        let Some(stdout) = child.stdout.as_mut() else { return false };
        match stdout.read_exact(&mut out) {
            Ok(_) => out[0] == 1,
            Err(_) => false,
        }
    }
}

impl Drop for HelperProc {
    fn drop(&mut self) {
        // 杀掉助手（stdin 断开也会让它自己退出，这里兜底）
        let mut child = self.child.lock().unwrap_or_else(|e| e.into_inner());
        let _ = child.kill();
    }
}

enum Backend {
    InProc(Api),
    Helper(HelperProc),
}

impl Backend {
    fn ports(&self) -> (Vec<i32>, Vec<i32>) {
        match self {
            Backend::InProc(a) => (a.mouse_ports.clone(), a.kbd_ports.clone()),
            Backend::Helper(h) => (h.mouse_ports.clone(), h.kbd_ports.clone()),
        }
    }
}

static API: Mutex<Option<Backend>> = Mutex::new(None);
/// 三态：false=从未试过；true=已定性（可用或不可用，看 API 是否 Some）
static PROBED: AtomicBool = AtomicBool::new(false);
/// create_context 失败次数（诊断用）
pub static CREATE_FAILS: AtomicIsize = AtomicIsize::new(0);

fn load_api() -> Option<Api> {
    use windows_sys::Win32::System::LibraryLoader::{GetModuleHandleW, GetProcAddress, LoadLibraryW};

    unsafe {
        // DLL 与壳 exe 同目录（D:\InDesk\interception.dll）。已装载过就直接拿句柄。
        let name: Vec<u16> = "interception.dll\0".encode_utf16().collect();
        let lib = GetModuleHandleW(name.as_ptr());
        let lib = if !lib.is_null() {
            lib as isize
        } else {
            LoadLibraryW(name.as_ptr()) as isize
        };
        if lib == 0 {
            return None;
        }
        let h = lib as *mut std::ffi::c_void;
        // windows-sys 0.59 的 GetProcAddress 返回 Option<fn>，转成地址再 transmute
        let get = |sym: &[u8]| -> Option<usize> {
            GetProcAddress(h, sym.as_ptr()).map(|f| f as usize)
        };
        let create: CreateContextFn =
            std::mem::transmute(get(b"interception_create_context\0")?);
        let destroy: DestroyContextFn =
            std::mem::transmute(get(b"interception_destroy_context\0")?);
        let send: SendFn = std::mem::transmute(get(b"interception_send\0")?);
        let set_filter: SetFilterFn = std::mem::transmute(get(b"interception_set_filter\0")?);
        let get_hwid: GetHardwareIdFn =
            std::mem::transmute(get(b"interception_get_hardware_id\0")?);
        // [§28] create_context 可能有驱动侧清理延迟（GameMon 驻留 / 前一实例刚退出等），
        // 偶发返回 NULL。带重试，最多 ~2s。
        let ctx = create_retry(&create, 10);
        if ctx.is_null() {
            CREATE_FAILS.fetch_add(1, Ordering::SeqCst);
            return None;
        }
    // 预热：挂 50ms 键盘过滤器激活注入能力，然后【销毁重建】——
    // 预热期回环堆积的物理键盘事件随销毁丢弃；不销毁会死锁后续 send（§30 实测）。
    unsafe extern "system" fn kbd_pred(d: i32) -> i32 {
        if (1..=10).contains(&d) { 1 } else { 0 }
    }
    set_filter(ctx, kbd_pred, 0xFFFF); // INTERCEPTION_FILTER_KEY_ALL
    std::thread::sleep(std::time::Duration::from_millis(50));
    destroy(ctx);
    let ctx = create_retry(&create, 10);
    if ctx.is_null() {
        CREATE_FAILS.fetch_add(1, Ordering::SeqCst);
        return None;
    }
        // [§27] 枚举实际抓到设备的端口。硬件 ID 查询返回 0 = 该口无设备。
        // [§31] 缓冲必须 ≥512 字节：hwid 实测可达 ~340 字节（256 会全返回 0）
        unsafe fn live_ports(
            get_hwid: &GetHardwareIdFn,
            ctx: *mut c_void,
            start: i32,
            end: i32,
        ) -> Vec<i32> {
            let mut v = Vec::new();
            for p in start..=end {
                let mut buf = [0u16; 256];
                unsafe {
                    if (get_hwid)(ctx, p, buf.as_mut_ptr(), 512) != 0 {
                        v.push(p);
                    }
                }
            }
            v
        }
        let kbd_ports = unsafe { live_ports(&get_hwid, ctx, 1, 10) };
        let mouse_ports = unsafe { live_ports(&get_hwid, ctx, 11, 20) };
        if mouse_ports.is_empty() && kbd_ports.is_empty() {
            // 过滤驱动没抓到任何设备（异常状态），按不可用处理走回退
            unsafe { destroy(ctx) };
            CREATE_FAILS.fetch_add(1, Ordering::SeqCst);
            return None;
        }
        Some(Api {
            _lib: lib,
            ctx,
            send,
            _destroy: destroy,
            _set_filter: set_filter,
            _get_hwid: get_hwid,
            mouse_ports,
            kbd_ports,
        })
    }
}

/// create_context 带重试：驱动偶发返回 NULL（清理延迟/被占用），间隔 200ms 重试。
fn create_retry(create: &CreateContextFn, max_tries: u32) -> *mut c_void {
    for i in 0..max_tries {
        let ctx = unsafe { create() };
        if !ctx.is_null() {
            return ctx;
        }
        if i + 1 < max_tries {
            std::thread::sleep(std::time::Duration::from_millis(200));
        }
    }
    std::ptr::null_mut()
}

/// [§30] 拉起助手进程并解析状态行。exe 与壳同目录。
fn spawn_helper() -> Option<Backend> {
    let exe = std::env::current_exe().ok()?;
    let path = exe.parent()?.join("interception_helper.exe");
    if !path.exists() {
        return None;
    }
    // [§34] 必须 CREATE_NO_WINDOW：助手是控制台程序，从 GUI 壳里 spawn 时 Windows 会给
    // 它**新建一个可见的控制台窗口**。本机实测（20260917 20:55）该窗口 993x519 盖在屏幕
    // 正中（182,182），既是用户看到的「桌面上关不掉的程序」，又会把落在它上面的注入点击
    // 全部吃掉。加标志后进程照常跑、零窗口。
    use std::os::windows::process::CommandExt;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    let child = std::process::Command::new(&path)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .creation_flags(CREATE_NO_WINDOW)
        .spawn()
        .ok()?;
    let mut child = child;
    // 读状态行：HELPER1 ok=1 mouse=12 kbd=1,2
    let mut line = String::new();
    use std::io::BufRead;
    {
        let Some(stdout) = child.stdout.as_mut() else {
            let _ = child.kill();
            return None;
        };
        let mut reader = std::io::BufReader::new(stdout);
        match reader.read_line(&mut line) {
            Ok(_) => {}
            Err(_) => {
                let _ = child.kill();
                return None;
            }
        }
    }
    let parse_ports = |tag: &str| -> Vec<i32> {
        line.split_whitespace()
            .find(|kv| kv.starts_with(tag))
            .and_then(|kv| kv.split('=').nth(1))
            .map(|v| v.split(',').filter_map(|x| x.parse().ok()).collect())
            .unwrap_or_default()
    };
    if !line.starts_with("HELPER1 ok=1") {
        let _ = child.kill();
        return None;
    }
    let mouse_ports = parse_ports("mouse=");
    let kbd_ports = parse_ports("kbd=");
    if mouse_ports.is_empty() && kbd_ports.is_empty() {
        let _ = child.kill();
        return None;
    }
    Some(Backend::Helper(HelperProc {
        child: Mutex::new(child),
        mouse_ports,
        kbd_ports,
    }))
}

/// 初始化后端：[§33] **永远优先助手进程**。
///
/// 本机实测（20260917 晚，`我的天真` + nProtect GameGuard）：反外挂把
/// `npggNT64.des` 注入进壳进程后，壳进程内的 `interception_send` 会被伪造成
/// 「成功」——黑匣子里每一次 click 都是 `driver ok=true`，但系统输入状态毫无变化
/// （同刻用干净进程直连驱动：左键/右键 GAKS 全部真实置位、相对移动精确命中）。
/// 也就是说**进程被注入后，进程内后端=假成功**，必须走不被注入的助手进程。
/// 仅当助手不可用（缺失/上下文建不起来）时才退回进程内。
fn init_backend() -> Option<Backend> {
    if let Some(b) = spawn_helper() {
        return Some(b);
    }
    load_api().map(Backend::InProc)
}

/// 驱动级注入是否可用（懒探测，失败不永久缓存：[§28] 每 5s 自动重探一次）。
pub fn available() -> bool {
    if !PROBED.load(Ordering::SeqCst) {
        let mut g = API.lock().unwrap_or_else(|e| e.into_inner());
        if !PROBED.load(Ordering::SeqCst) {
            *g = init_backend();
            PROBED.store(true, Ordering::SeqCst);
            LAST_FAIL.store(now_secs(), Ordering::SeqCst);
        }
    }
    // [§33] 助手探活：实测反外挂会在启动后几十秒内把助手进程清掉，
    // 若继续把它当成"可用"，之后每次点击都静默失败（管道断了）→ 会话级假死。
    // 所以每次调用前先看子进程还在不在，死了立刻作废、下面立刻重建。
    {
        let mut g = API.lock().unwrap_or_else(|e| e.into_inner());
        let dead = match g.as_ref() {
            Some(Backend::Helper(h)) => !h.alive(),
            _ => false,
        };
        if dead {
            *g = None;
            PROBED.store(false, Ordering::SeqCst);
        }
    }
    let mut ok = API.lock().unwrap_or_else(|e| e.into_inner()).is_some();
    // [§28] 失败后不自弃：限流重探（1s；初始化那一刻置位 LAST_FAIL，避免刚建好又重建）。
    if !ok && PROBED.load(Ordering::SeqCst) {
        let now = now_secs();
        let last = LAST_FAIL.load(Ordering::SeqCst);
        if now - last >= 1
            && LAST_FAIL
                .compare_exchange(last, now, Ordering::SeqCst, Ordering::SeqCst)
                .is_ok()
        {
            let mut g = API.lock().unwrap_or_else(|e| e.into_inner());
            if g.is_none() {
                *g = init_backend();
            }
            ok = g.is_some();
        }
    }
    ok
}

fn now_secs() -> isize {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as isize)
        .unwrap_or(0)
}

/// 上次失败重探的时间戳（秒）
static LAST_FAIL: AtomicIsize = AtomicIsize::new(0);

/// 供诊断：强制重探（create_context 失败后驱动可能被装好）。
pub fn reprobe() -> bool {
    PROBED.store(false, Ordering::SeqCst);
    available()
}

/// [§29] 启动期探测 + 落盘：壳一起来就测驱动并把结果写进 input_diag.log，
/// 不依赖"有人点击"才触发（现场排障我自己能看，不用让用户反复试）。
pub fn startup_probe_log() {
    let ok = available();
    let ports = {
        let g = API.lock().unwrap_or_else(|e| e.into_inner());
        match g.as_ref() {
            Some(b) => {
                let (m, k) = b.ports();
                let kind = match b {
                    Backend::InProc(_) => "inproc",
                    Backend::Helper(_) => "helper",
                };
                format!("backend={} mouse={:?} kbd={:?}", kind, m, k)
            }
            None => "none".to_string(),
        }
    };
    // [§33] 启动自检（不需要用户点任何东西）：用驱动相对移动 +40px 再移回，
    // 回读光标真实位移写进黑匣子。这是「壳 → 助手 → 驱动 → 系统输入栈」整条链路的
    // 活体证明：位移≈0 就是链路死的（此前正是被注入的进程内后端假成功所掩盖）。
    let selftest = if ok {
        // ① 链路活性：相对移动 +40px 再回读真实位移（位移≈0 = 壳→助手→驱动→系统 链路死）
        let (x0, y0) = crate::input::mouse_position();
        let sent = mouse_move_rel(40, 0);
        std::thread::sleep(std::time::Duration::from_millis(25));
        let (x1, _y1) = crate::input::mouse_position();
        let _ = mouse_move_rel(-40, 0);
        std::thread::sleep(std::time::Duration::from_millis(10));
        let (x2, _y2) = crate::input::mouse_position();
        // ② [§34] 定位精度：走**产品真实定位路径**（mouse_move）移到屏幕中点，回读误差。
        // 定位一旦跑偏，再正确的点击也是点到别处 —— 这是"点了没反应"的另一半根因。
        let (w, h) = crate::input::screen_size();
        let (tx, ty) = (w / 2, h / 2);
        crate::input::mouse_move(tx, ty);
        std::thread::sleep(std::time::Duration::from_millis(80));
        let (px, py) = crate::input::mouse_position();
        let err = (px - tx).abs() + (py - ty).abs();
        crate::input::mouse_move(x2, y0); // 还原光标
        format!(
            "selftest sent={} moved_x={} restored={} ({}); pos target=({},{}) got=({},{}) err={} ({})",
            sent,
            x1 - x0,
            x2 - x0,
            if (x1 - x0).abs() >= 8 { "LINK-ALIVE" } else { "LINK-DEAD" },
            tx,
            ty,
            px,
            py,
            err,
            if err <= 4 { "POS-OK" } else { "POS-BAD" }
        )
    } else {
        "selftest skipped(no backend)".to_string()
    };
    let line = format!(
        "=== shell startup ts={} driver_available={} create_fails={} ports={} {}\r\n",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0),
        ok,
        CREATE_FAILS.load(Ordering::SeqCst),
        ports,
        selftest,
    );
    let p = std::env::current_exe()
        .ok()
        .and_then(|e| e.parent().map(|d| d.join("input_diag.log")));
    if let Some(p) = p {
        let _ = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&p)
            .and_then(|mut f| std::io::Write::write_all(&mut f, line.as_bytes()));
    }
}

/// [§33] 投递一次性**相对移动**（dx/dy 像素）。
/// ⚠️ Interception 常量：`MOUSE_MOVE_RELATIVE = 0x000`、`MOUSE_MOVE_ABSOLUTE = 0x001`
/// —— flags 必须写 0，写 1 会被当成「绝对定位」把光标瞬移到屏幕左上角（本机实测踩过）。
pub fn mouse_move_rel(dx: i32, dy: i32) -> bool {
    if dx == 0 && dy == 0 {
        return true;
    }
    let mut g = API.lock().unwrap_or_else(|e| e.into_inner());
    let Some(_backend) = g.as_ref() else { return false };
    let st = StrokeBuf {
        m: MouseStroke {
            state: 0,
            flags: 0,
            rolling: 0,
            x: dx,
            y: dy,
            information: 0,
        },
    };
    match _backend {
        Backend::InProc(a) => unsafe {
            a.mouse_ports.iter().any(|&p| (a.send)(a.ctx, p, &st, 1) == 1)
        },
        Backend::Helper(h) => {
            let mut bytes = [0u8; 20];
            bytes[8..12].copy_from_slice(&dx.to_le_bytes());
            bytes[12..16].copy_from_slice(&dy.to_le_bytes());
            h.mouse_ports.iter().any(|&p| h.send_strokes(p, &[bytes]))
        }
    }
}

/// 投递一个鼠标按钮事件。返回 true = 驱动投递成功。
pub fn mouse_button(left: bool, down: bool) -> bool {
    let mut g = API.lock().unwrap_or_else(|e| e.into_inner());
    let Some(_backend) = g.as_ref() else { return false };
    let state = match (left, down) {
        (true, true) => MS_LEFT_DOWN,
        (true, false) => MS_LEFT_UP,
        (false, true) => MS_RIGHT_DOWN,
        (false, false) => MS_RIGHT_UP,
    };
    let st = StrokeBuf {
        m: MouseStroke {
            state,
            flags: 0,
            rolling: 0,
            x: 0,
            y: 0,
            information: 0,
        },
    };
    match _backend {
        // [§27] 发往所有抓到设备的鼠标端口，任一成功即真成功
        Backend::InProc(a) => unsafe {
            a.mouse_ports
                .iter()
                .any(|&p| (a.send)(a.ctx, p, &st, 1) == 1)
        },
        Backend::Helper(h) => {
            let mut bytes = [0u8; 20];
            bytes[0..2].copy_from_slice(&state.to_le_bytes());
            h.mouse_ports.iter().any(|&p| h.send_strokes(p, &[bytes]))
        }
    }
}

/// 投递滚轮。delta_ticks 以「格」为单位，每格 = WHEEL_DELTA(120)（与 SendInput mouseData 同语义）。
pub fn mouse_wheel(delta_ticks: i32, horizontal: bool) -> bool {
    let mut g = API.lock().unwrap_or_else(|e| e.into_inner());
    let Some(_backend) = g.as_ref() else { return false };
    let rolling = delta_ticks
        .saturating_mul(120)
        .clamp(i16::MIN as i32, i16::MAX as i32) as i16;
    let st = StrokeBuf {
        m: MouseStroke {
            state: if horizontal { MS_HWHEEL } else { MS_WHEEL },
            flags: 0,
            rolling,
            x: 0,
            y: 0,
            information: 0,
        },
    };
    match _backend {
        Backend::InProc(a) => unsafe {
            a.mouse_ports
                .iter()
                .any(|&p| (a.send)(a.ctx, p, &st, 1) == 1)
        },
        Backend::Helper(h) => {
            let mut bytes = [0u8; 20];
            bytes[0..2].copy_from_slice(
                &(if horizontal { MS_HWHEEL } else { MS_WHEEL }).to_le_bytes(),
            );
            bytes[4..6].copy_from_slice(&(rolling as u16).to_le_bytes());
            h.mouse_ports.iter().any(|&p| h.send_strokes(p, &[bytes]))
        }
    }
}

/// 投递一个按键事件（扫描码）。`extended` 对应 KEYEVENTF_EXTENDEDKEY（E0 前缀键）。
pub fn key_scancode(scan: u16, down: bool, extended: bool) -> bool {
    let mut g = API.lock().unwrap_or_else(|e| e.into_inner());
    let Some(_backend) = g.as_ref() else { return false };
    if scan == 0 {
        return false;
    }
    let mut state = if down { 0 } else { KEY_UP };
    if extended {
        state |= KEY_E0;
    }
    let st = StrokeBuf {
        k: KeyStroke {
            code: scan & 0xFF,
            state,
            information: 0,
        },
    };
    match _backend {
        Backend::InProc(a) => unsafe {
            a.kbd_ports
                .iter()
                .any(|&p| (a.send)(a.ctx, p, &st, 1) == 1)
        },
        Backend::Helper(h) => {
            let mut bytes = [0u8; 20];
            bytes[0..2].copy_from_slice(&(scan & 0xFF).to_le_bytes());
            bytes[2..4].copy_from_slice(&state.to_le_bytes());
            h.kbd_ports.iter().any(|&p| h.send_strokes(p, &[bytes]))
        }
    }
}

/// VK → 扫描码（MAPVK_VK_TO_VSC_EX：高字节 0xE0 = 扩展键集）。
/// 返回 `(scan, extended)`；None = 该 VK 无扫描码映射（如纯 Unicode）。
pub fn vk_to_scancode(vk: u16) -> Option<(u16, bool)> {
    use windows_sys::Win32::UI::Input::KeyboardAndMouse::{MapVirtualKeyW, MAPVK_VK_TO_VSC_EX};
    let r = unsafe { MapVirtualKeyW(vk as u32, MAPVK_VK_TO_VSC_EX) };
    if r == 0 {
        return None;
    }
    let hi = (r >> 8) & 0xFFFF;
    let lo = r & 0xFF;
    if lo == 0 {
        return None;
    }
    match hi {
        0xE0 => Some((lo as u16, true)),
        0xE1 => Some((lo as u16, true)), // 极少（Pause），按 E1 语义不区分处理
        _ => Some((lo as u16, false)),
    }
}
