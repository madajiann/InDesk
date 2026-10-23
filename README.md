# InDesk

轻量、可自托管的跨平台远程桌面。Windows 桌面端 + Android 端 + 浏览器端。

**English**: InDesk is a lightweight, self-hostable remote desktop project — a native Flutter shell on Windows, an Android client, and a browser client, all driven by a headless Rust core.

> 当前版本：桌面端 `2.3.49` ｜ Android `1.3.3`（versionCode 19）｜ Web `20261022.2`

---

## 这是什么

一个不依赖第三方云端的远程桌面实现：连接受 YOU 控制，信令与中继跑在你自己或你信任的服务器上。

### 已实现

| 能力 | 说明 |
|---|---|
| 远程桌面 | 硬件编码优先，弱网自动降级到软件编码 |
| 鼠标键盘 | 绝对/相对两种模式，支持多显示器切换 |
| 剪贴板同步 | 文本双向即时同步 |
| 文件直贴 | 在远程机器上 `Ctrl+V` 直接落盘，反向同理 |
| 文件传输 | 会话内双向传输，带进度 |
| 窗口行为 | 最小化到托盘、开机自启、无人值守 |
| 桌面端自更新 | 检测 → 下载 → SHA-256 校验 → 静默安装 → 自动重启 |
| 网页版 | 浏览器直接发起控制，无需安装 |

### 没有做的

- 移动端被控（Android 只能作为主控）
- macOS 端（在规划中）
- 音频转发

---

## 架构

```
┌──────────────────┐         ┌──────────────────┐
│  桌面端（主控/被控）│         │  浏览器 / Android │
│  Flutter 原生 UI  │         │   WebView / Vue  │
└────────┬─────────┘         └────────┬─────────┘
         │  IPC（本地进程间）           │  WebRTC
┌────────┴─────────┐                  │
│  indesk-core     │◄─────────────────┘
│  Rust 无头核心    │
│  采集/编解码/输入  │
│  剪贴板/文件/协议  │
└────────┬─────────┘
         │  WebSocket 信令 + TURN 中继
┌────────┴─────────┐
│   服务端（不含）   │
└──────────────────┘
```

**设计取舍**：桌面端 UI 用 Flutter 原生渲染，不用 WebView。代价是界面层无法热更新（改界面必须发安装包），换来的是启动速度、内存占用和原生手感。核心逻辑全部下沉到 `indesk-core` 这个独立进程里——它是可以单独替换升级的。

---

## 仓库结构

```
flutter-shell/       Windows 桌面壳（Flutter / Dart）
  lib/core/          IPC 客户端、剪贴板引擎、core 进程管理
  lib/ui/            主界面、远控页、设置、自更新
  lib/receiver/      被控端会话
  lib/signal/        信令
  lib/config/        TURN 配置（构建期注入，仓库内为空）

web/
  src-tauri/         indesk-core（Rust）—— 采集、编解码、输入、剪贴板、协议
  ui-patch/          网页版外观补丁层（CSS/JS 注入源码 + 构建脚本）

mobile-app/          Android 壳（Java + WebView）
installer/           Windows 安装器（Inno Setup）
brand/               图标源文件
```

### 本仓库不包含的两块

| 缺什么 | 原因 | 影响 |
|---|---|---|
| **服务端**（信令 / 中继 / 账号） | 未开源 | clone 下来能编译出全部客户端，但没有服务端连不上任何设备。需要你自己的 WebSocket 信令服务 + TURN 中继（Coturn 即可） |
| **网页版前端源码**（Vue3） | 衍生自 BilldDesk，见 `NOTICE` | `web/ui-patch/` 只包含补丁层与基线快照。要构建完整网页版，需先取得上游前端源码 |

---

## 构建

### 前置

| 组件 | 版本 |
|---|---|
| Flutter | 3.x（stable） |
| Rust | 1.75+（`cargo`） |
| Node.js | 18+ / pnpm 9（构建网页版补丁时需要） |
| JDK | 17 |
| Inno Setup | 6.x（仅打 Windows 安装包时需要） |
| Visual Studio | 2022 + Desktop C++ 工作负载（Flutter Windows 必需） |

### 1. Rust 核心（indesk-core）

```bash
cd web/src-tauri
cargo build --release
# 产物：target/release/indesk-desktop.exe → 复制为 indesk-core.exe
```

### 2. Windows 桌面端

```bash
cd flutter-shell
flutter pub get
flutter build windows --release \
  --dart-define=TURN_URL=turn:你的服务器:3478 \
  --dart-define=TURN_URL_TCP='turn:你的服务器:3478?transport=tcp' \
  --dart-define=TURN_USERNAME=... \
  --dart-define=TURN_CREDENTIAL=...
# 产物：build/windows/x64/runner/Release/
```

**TURN 凭据不进仓库**（`lib/config/turn_config.dart` 全部从 `--dart-define` 读）。
不传这些参数也能编译，但只走 STUN 直连，跨严格 NAT 会连不上。

强烈建议用 coturn 的 `use-auth-secret` 临时凭证（每次连接前向自己的服务端申请、
短时有效），不要把长期静态账号打进客户端——静态账号一旦被从二进制里扒出来，
带宽账单就是你的。

把上一步的 `indesk-core.exe` 放进同一个 Release 目录，程序启动时会拉起它。

### 3. Windows 安装包

```bash
cd installer
ISCC.exe installer.iss
# 产物：out/InDesk-<版本>-x64-setup.exe
```

VC++ 运行库（msvcp140 / vcruntime140 / vcruntime140_1）由你自己放 `installer/redist/`，
或改为在目标机安装 VC++ 2015-2022 x64 运行库；缺失时这 3 行会被跳过。

### 4. Android

```bash
cd mobile-app
gradle wrapper            # 首次：仓库未附带 gradle wrapper
gradle assembleRelease
```

签名：把 `keystore.properties.example` 复制为 `keystore.properties`，填入你自己的
keystore。**仓库里没有任何签名材料，也没有密码。**

`app/src/main/assets/web/` 是"主文档加载失败时的离线兜底页"，属于构建产物
（线上 web 构建的镜像），已列入 `.gitignore`。不放它也能编译，只是断网时没有兜底页。

### 5. 网页版补丁

```bash
cd web/ui-patch
python build.py           # 生成 indesk-ui-patch-<批次>.css/.js
```

---

## 配置

所有密钥、TURN 凭据、签名信息都在仓库之外：

```bash
cp mobile-app/keystore.properties.example mobile-app/keystore.properties
```

**仓库永远不包含任何真实凭据。** 提交前请确认 `git status` 里没有
`.env.local`、`*.jks`、`*.pem`、`keystore.properties`。

---

## 许可证

本项目采用 **Apache License 2.0**（见 `LICENSE`）。

选 Apache 而不是 MIT 的原因：它带有明确的专利授权与贡献条款（第 3、5 条），
对贡献者和使用者都是更清晰的边界。

第三方组件声明见 `NOTICE`。`web/ui-patch/v11-base.*` 衍生自 BilldDesk（MIT），
原始许可证保留在 `web/ui-patch/LICENSE-BilldDesk.txt`。

## 商标

Apache-2.0 **不授予商标许可**。`InDesk` 名称与图标仅指本项目的官方发布版本；
二次分发或修改版本请更换名称与图标，避免用户误认为官方版本——对远程控制类
软件，假冒客户端是最直接的攻击面。
