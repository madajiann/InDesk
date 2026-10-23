//! [20261015 · Electron 混合壳] core 模式桥接层
//!
//! 启动方式：`indesk-desktop.exe --core --core-port <port>`（由 Electron 壳拉起）。
//! 核心思想：**无窗口运行**——不创建任何 WebviewWindow，就完全不加载 WebView2，
//! 于是"客户机器缺 WebView2"这个问题从根上消失；渲染引擎由 Electron 自带。
//!
//! axum 在 127.0.0.1:<port> 提供两类服务：
//!   1. `GET /ws`  —— IPC 桥：
//!      · 收 `{id, channel, payload}` → `ipc::dispatch(...)` → 回 `{id, result|error}`
//!        （与 lib.rs 的 tauri 命令 `ipc_dispatch` 走同一条路，语义 1:1）
//!      · 事件 → `{event, payload}` 广播（来自 ipc.rs reply() 等收口点的 broadcast）
//!   2. `GET /*`   —— 直接复用 `ipc::serve_web_update`：
//!        web UI（热更包 → 壳内置资源两级回落）+ `/__capture/frame` 帧通道 +
//!        `?meta=1` 元信息，全部原样可用。
//!
//! Electron 壳：加载 `http://127.0.0.1:<port>/index.html`，preload 暴露
//! `window.electronAPI.ipcRenderer`（send/invoke/on/removeListener）→ ws 桥。
//! 前端 `utils/ipc-adapter.ts` 检测到 electronAPI 即走 Electron 分支，**业务代码零改动**。
//!
//! 协议语义与原版 billd-desk Electron 壳对齐（ipc.rs 注释自证：dispatch 就是
//! 原 Electron ipcMain 通道的收敛移植），所以老前端在两种壳下行为一致。

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::OnceLock;

use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{FromRequestParts, State};
use axum::response::IntoResponse;
use axum::routing::any;
use axum::Router;
use futures_util::{SinkExt, StreamExt};
use serde_json::Value;
use tokio::sync::broadcast;

/// 是否处于 core（无窗口）模式。由 lib.rs 在 `--core` 时置位。
static CORE_MODE: AtomicBool = AtomicBool::new(false);
/// 事件广播端：ws 客户端订阅它收 `{event, payload}` 推送。
static BROADCAST_TX: OnceLock<broadcast::Sender<String>> = OnceLock::new();
/// core 实际监听的端口（写进进度/诊断日志用）。
static CORE_PORT: OnceLock<u16> = OnceLock::new();

// ============================================================================
// [20261015 core 本地反代]
//
// 背景：本地壳（Electron/Flutter）下 web 从 `http://127.0.0.1:<core_port>` 加载，
// web/src/constant.ts 的同域约定使 AXIOS_BASEURL 与 WEBSOCKET_URL 都指向 core——
// 这正是"本地壳零配置、无证书、无 CORS"架构的根基。但 core 原先只有 /ws 桥 +
// 静态资产 fallback，`/api/*` 与 `/socket.io/*` 全被 fallback 吞成 index.html
// （实测：被控端 `WebSocket ... Unexpected response code: 200`，desk_user/create
// 失败，localStorage 恒空）。两条反代补齐这个欠账：
//   · `/api/*`            → https://desk.yomm.top:8443  （HTTP 透传）
//   · `/socket.io/*`      → wss://desk.yomm.top:8443    （WS 升级透传 + HTTP polling）
// 反代目标与 web 的 PRIVATE_DESK_ORIGIN 常量同源（8443 必须带上，443 是别的业务）。
// ============================================================================

const UPSTREAM_ORIGIN: &str = "https://desk.yomm.top:8443";

/// 后端业务 API 路径前缀（来源：web/src/api/*.ts 全量盘点 20261015）。
/// web 的 axios baseURL = 本地 origin（同域约定），业务路径直接挂在根上
/// （如 /desk_user/create），core 必须把这些前缀透传到真实服务端，
/// 其余路径仍走 fallback（web 静态资产 / hash 路由 / 帧通道）。
const API_PREFIXES: &[&str] = &[
    "desk_user",
    "desk_version",
    "live_config",
    "live_room",
    "user_live_room",
    "live_play",
    "srs",
    "user",
    "wechat_user",
    "qq_user",
    "area",
    "order",
    "goods",
    "wallet",
    "account",
    "signin_record",
    "qiniu_data",
    "gift_record",
];

static HTTP_CLIENT: OnceLock<reqwest::Client> = OnceLock::new();

fn http_client() -> &'static reqwest::Client {
    HTTP_CLIENT.get_or_init(|| {
        reqwest::Client::builder()
            .danger_accept_invalid_certs(false)
            // 自家上游直连：无视系统代理/环境变量代理（用户机 Clash 挂掉时
            // reqwest 默认读系统代理 → "积极拒绝" → 整个反代面全灭）
            .no_proxy()
            .timeout(std::time::Duration::from_secs(30))
            .build()
            .unwrap_or_else(|_| reqwest::Client::new())
    })
}

/// `/api/*` 与 `/socket.io/*`（非 ws 升级）的 HTTP 透传。
async fn proxy_http(
    method: axum::http::Method,
    uri: axum::http::Uri,
    headers: axum::http::HeaderMap,
    body: axum::body::Bytes,
) -> axum::response::Response {
    let path_q = uri
        .path_and_query()
        .map(|x| x.as_str().to_string())
        .unwrap_or_else(|| uri.path().to_string());
    let url = format!("{}{}", UPSTREAM_ORIGIN, path_q);

    let mut req = http_client().request(method, &url);
    // 透传业务头（跳过 host / 长度由 reqwest 重算）
    for (k, v) in headers.iter() {
        if matches!(
            k.as_str(),
            "host" | "content-length" | "connection" | "transfer-encoding" | "accept-encoding"
        ) {
            continue;
        }
        if let Ok(val) = v.to_str() {
            req = req.header(k.as_str(), val);
        }
    }
    if !body.is_empty() {
        req = req.body(body.to_vec());
    }

    match req.send().await {
        Ok(resp) => {
            let status = axum::http::StatusCode::from_u16(resp.status().as_u16())
                .unwrap_or(axum::http::StatusCode::BAD_GATEWAY);
            let mut builder = axum::http::Response::builder().status(status);
            for (k, v) in resp.headers().iter() {
                if matches!(
                    k.as_str(),
                    "transfer-encoding" | "connection" | "content-encoding" | "content-length"
                ) {
                    continue;
                }
                builder = builder.header(k.as_str(), v.as_bytes());
            }
            let bytes = resp.bytes().await.unwrap_or_default();
            builder
                .body(axum::body::Body::from(bytes))
                .unwrap_or_else(|_| axum::http::StatusCode::BAD_GATEWAY.into_response())
        }
        Err(e) => {
            crate::ipc::ipc_log(&format!("[proxy] HTTP {path_q} 失败: {e}"));
            axum::http::StatusCode::BAD_GATEWAY.into_response()
        }
    }
}

/// `/socket.io/*` 入口：ws 升级走 WS 透传，其余（polling）走 HTTP 透传。
/// 注意：WebSocketUpgrade 提取器对非升级请求会直接 reject（400），
/// 所以必须先看 UPGRADE 头再手工提取。
async fn proxy_socket_io_entry(req: axum::extract::Request) -> axum::response::Response {
    let is_ws_upgrade = req
        .headers()
        .get(axum::http::header::UPGRADE)
        .and_then(|v| v.to_str().ok())
        .map(|v| v.eq_ignore_ascii_case("websocket"))
        .unwrap_or(false);

    let uri = req.uri().clone();
    if is_ws_upgrade {
        let (mut parts, _) = req.into_parts();
        match WebSocketUpgrade::from_request_parts(&mut parts, &()).await {
            Ok(ws) => proxy_ws_upgrade(ws, parts.uri.clone()).await,
            Err(rej) => {
                let _ = uri;
                (axum::http::StatusCode::BAD_REQUEST, rej.body_text()).into_response()
            }
        }
    } else {
        let (parts, body) = req.into_parts();
        let bytes = match axum::body::to_bytes(body, 16 * 1024 * 1024).await {
            Ok(b) => b,
            Err(_) => return axum::http::StatusCode::BAD_GATEWAY.into_response(),
        };
        proxy_http(parts.method, uri, parts.headers, bytes).await
    }
}

/// `/api/*` 入口：纯 HTTP 透传。
async fn proxy_api_entry(req: axum::extract::Request) -> axum::response::Response {
    let (parts, body) = req.into_parts();
    let bytes = match axum::body::to_bytes(body, 16 * 1024 * 1024).await {
        Ok(b) => b,
        Err(_) => return axum::http::StatusCode::BAD_GATEWAY.into_response(),
    };
    proxy_http(parts.method, parts.uri, parts.headers, bytes).await
}

/// WS 双向透传（engine.io 帧：text/json + binary 均有）。
async fn proxy_ws_upgrade(ws: WebSocketUpgrade, uri: axum::http::Uri) -> axum::response::Response {
    let upstream = format!(
        "{}{}",
        UPSTREAM_ORIGIN.replacen("https://", "wss://", 1),
        uri.path_and_query().map(|x| x.as_str()).unwrap_or("/socket.io/")
    );
    ws.on_upgrade(move |client| async move {
        let (up_sink, mut up_stream) = match tokio_tungstenite::connect_async(&upstream).await {
            Ok((s, _)) => s.split(),
            Err(e) => {
                crate::ipc::ipc_log(&format!("[proxy] WS 连上游失败 {upstream}: {e}"));
                return;
            }
        };
        let (mut c_sink, mut c_stream) = client.split();

        // 本地客户端 → 上游
        let to_upstream = async move {
            let mut up_sink = up_sink;
            while let Some(Ok(msg)) = c_stream.next().await {
                let tm = match msg {
                    Message::Text(t) => tokio_tungstenite::tungstenite::Message::Text(t),
                    Message::Binary(b) => tokio_tungstenite::tungstenite::Message::Binary(b),
                    Message::Ping(p) => tokio_tungstenite::tungstenite::Message::Ping(p),
                    Message::Pong(p) => tokio_tungstenite::tungstenite::Message::Pong(p),
                    Message::Close(_) => {
                        let _ = up_sink.close().await;
                        break;
                    }
                    _ => continue,
                };
                if up_sink.send(tm).await.is_err() {
                    break;
                }
            }
        };

        // 上游 → 本地客户端
        let to_client = async move {
            let mut c_sink = c_sink;
            while let Some(Ok(msg)) = up_stream.next().await {
                use tokio_tungstenite::tungstenite::Message as TM;
                let cm = match msg {
                    TM::Text(t) => Message::Text(t),
                    TM::Binary(b) => Message::Binary(b),
                    TM::Ping(p) => Message::Ping(p),
                    TM::Pong(p) => Message::Pong(p),
                    TM::Close(_) => {
                        let _ = c_sink.close().await;
                        break;
                    }
                    TM::Frame(_) => continue,
                };
                if c_sink.send(cm).await.is_err() {
                    break;
                }
            }
        };

        tokio::join!(to_upstream, to_client);
    })
}

pub fn is_core_mode() -> bool {
    CORE_MODE.load(Ordering::Relaxed)
}

pub fn core_port() -> Option<u16> {
    CORE_PORT.get().copied()
}

/// 事件广播入口：ipc.rs reply() 与三处直接 emit 点调用。
/// 非 core 模式是 no-op（Tauri 事件照走原路）。
pub fn broadcast(channel: &str, payload: &Value) {
    if !CORE_MODE.load(Ordering::Relaxed) {
        return;
    }
    if let Some(tx) = BROADCAST_TX.get() {
        let line = serde_json::json!({ "event": channel, "payload": payload }).to_string();
        // 没有订阅者（Electron 壳还没连上）时 send 返回 Err，属正常，静默
        let _ = tx.send(line);
    }
}

/// 启动 core 模式的本地服务。在 tauri setup 里调用（复用 tauri 的 tokio runtime）。
pub fn start(app: tauri::AppHandle, port: u16) {
    CORE_MODE.store(true, Ordering::Relaxed);
    let _ = CORE_PORT.set(port);
    let (tx, _) = broadcast::channel(256);
    let _ = BROADCAST_TX.set(tx);

    let app_state = std::sync::Arc::new(app);
    let mut router = Router::new()
        .route("/ws", any(ws_handler))
        .route("/socket.io", any(proxy_socket_io_entry))
        .route("/socket.io/", any(proxy_socket_io_entry));
    for prefix in API_PREFIXES {
        router = router
            .route(&format!("/{prefix}"), any(proxy_api_entry))
            .route(&format!("/{prefix}/*rest"), any(proxy_api_entry));
    }
    let router = router.fallback(serve_any).with_state(app_state);

    let addr = std::net::SocketAddr::from(([127, 0, 0, 1], port));
    tauri::async_runtime::spawn(async move {
        let listener = match tokio::net::TcpListener::bind(addr).await {
            Ok(l) => l,
            Err(e) => {
                crate::ipc::ipc_log(&format!("[core] axum bind {addr} 失败: {e}"));
                return;
            }
        };
        crate::ipc::ipc_log(&format!("[core] 桥服务就绪 http://{addr}"));
        if let Err(e) = axum::serve(listener, router).await {
            crate::ipc::ipc_log(&format!("[core] axum serve 异常退出: {e}"));
        }
    });
}

/// 非 /ws 的一切请求：转交 serve_web_update（web UI + 帧通道 + meta）。
async fn serve_any(
    State(app): State<std::sync::Arc<tauri::AppHandle>>,
    uri: axum::http::Uri,
) -> axum::response::Response {
    let resp = crate::ipc::serve_web_update(&app, uri.path(), uri.query());
    let mut builder = axum::http::Response::builder().status(resp.status());
    for (k, v) in resp.headers() {
        builder = builder.header(k.as_str(), v.as_bytes());
    }
    builder
        .body(axum::body::Body::from(resp.into_body()))
        .unwrap_or_else(|_| axum::http::StatusCode::INTERNAL_SERVER_ERROR.into_response())
}

async fn ws_handler(ws: WebSocketUpgrade, State(app): State<std::sync::Arc<tauri::AppHandle>>) -> axum::response::Response {
    ws.on_upgrade(move |socket| handle_socket(socket, app))
}

/// 单条 ws 连接的生命周期：收请求 → dispatch；同时把事件广播流转发出去。
async fn handle_socket(socket: WebSocket, app: std::sync::Arc<tauri::AppHandle>) {
    let (mut sender, mut receiver) = socket.split();
    let (resp_tx, mut resp_rx) = tokio::sync::mpsc::unbounded_channel::<Message>();

    // 事件广播 → 该客户端
    if let Some(tx) = BROADCAST_TX.get() {
        let mut rx = tx.subscribe();
        let rtx = resp_tx.clone();
        tauri::async_runtime::spawn(async move {
            loop {
                match rx.recv().await {
                    Ok(line) => {
                        if rtx.send(Message::Text(line)).is_err() {
                            break;
                        }
                    }
                    Err(broadcast::error::RecvError::Lagged(n)) => {
                        // 掉队：跳过 n 条，不中断（事件是高频的，断线代价更大）
                        let _ = n;
                        continue;
                    }
                    Err(broadcast::error::RecvError::Closed) => break,
                }
            }
        });
    }

    // 响应回写任务
    let write_task = tauri::async_runtime::spawn(async move {
        while let Some(msg) = resp_rx.recv().await {
            if sender.send(msg).await.is_err() {
                break;
            }
        }
    });

    // 请求读取：每条消息 spawn 一个任务做 dispatch（长操作不阻塞后续命令）
    while let Some(Ok(msg)) = receiver.next().await {
        let app = app.clone();
        let rtx = resp_tx.clone();
        match msg {
            Message::Text(text) => {
                tauri::async_runtime::spawn(async move {
                    let reply = match serde_json::from_str::<Value>(&text) {
                        Ok(v) => {
                            let id = v.get("id").cloned().unwrap_or(Value::Null);
                            let channel = v
                                .get("channel")
                                .and_then(|c| c.as_str())
                                .unwrap_or("")
                                .to_string();
                            let payload = v.get("payload").cloned().unwrap_or(Value::Null);
                            match crate::ipc::dispatch(&app, &channel, payload).await {
                                Ok(result) => serde_json::json!({ "id": id, "result": result }),
                                Err(e) => serde_json::json!({ "id": id, "error": e }),
                            }
                        }
                        Err(e) => serde_json::json!({ "id": null, "error": format!("bad json: {e}") }),
                    };
                    let _ = rtx.send(Message::Text(reply.to_string()));
                });
            }
            Message::Close(_) => break,
            _ => {}
        }
    }

    let _ = resp_tx.send(Message::Close(None));
    let _ = write_task.await;
}
