/// ws 桥客户端 —— 协议与 electron-shell/main.cjs 完全一致
///
/// 请求：`{"id":"<唯一>","channel":"<名>","payload":{...},"windowId":<n>}`
/// invoke 响应（对 id）：`{"id":"...","ok":true,"data":{...}}`
/// 广播事件：`{"event":"<事件名>","payload":{...},"windowId":<n>}`
import 'dart:async';
import 'dart:convert';
import 'dart:io';
import 'dart:math';

class WsReply {
  WsReply({required this.ok, this.data});
  final bool ok;
  final dynamic data;
}

class WsEvent {
  WsEvent({required this.event, this.payload, this.windowId});
  final String event;
  final dynamic payload;
  final int? windowId;
}

class WsBridge {
  /// [20261019 被控去 WebView2] 允许多实例：主控核（indesk-core.exe:23654）
  /// 与被控核（indesk-desktop.exe --core:23655）各持一条桥。
  WsBridge();
  static final WsBridge instance = WsBridge();

  /// localhost 桥专用 client：强制直连。
  /// WebSocket.connect 默认 findProxyFromEnvironment，若壳进程继承了
  /// 死代理（如 http_proxy 指向已退出的 Clash），127.0.0.1 的握手也会被
  /// 拖死 —— 桥是本机回环，任何代理都不该介入。
  final HttpClient _wsClient = HttpClient()..findProxy = (_) => 'DIRECT';

  WebSocket? _ws;
  int? _port;
  final _pending = <String, Completer<WsReply>>{};
  final _events = StreamController<WsEvent>.broadcast();
  Timer? _heartbeat;
  Timer? _reconnect;
  int _seq = 0;
  bool _closedByUs = false;

  Stream<WsEvent> get events => _events.stream;
  bool get isConnected => _ws != null && _ws!.readyState == WebSocket.open;

  /// 连接 core ws 桥；失败自动重试（指数退避 500ms→10s），断线亦然。
  Future<void> connect(int port) async {
    _port = port;
    _closedByUs = false;
    // 首连重试循环：core 刚拉起时桥可能晚几百毫秒就绪
    for (var attempt = 0;; attempt++) {
      try {
        await _open(port);
        break;
      } catch (_) {
        if (attempt > 8) rethrow; // 指数退避到 10s，约 45s 后放弃
        await Future.delayed(
            Duration(milliseconds: min(500 * (1 << min(attempt, 5)), 10000)));
      }
    }
    _heartbeat?.cancel();
    _heartbeat = Timer.periodic(const Duration(seconds: 20), (_) {
      if (isConnected) _ws!.add('ping');
    });
  }

  Future<void> _open(int port) async {
    final ws = await WebSocket.connect('ws://127.0.0.1:$port/ws',
            customClient: _wsClient)
        .timeout(const Duration(seconds: 5));
    _ws = ws;
    ws.listen(
      (data) {
        final text = data is List<int> ? utf8.decode(data) : data as String;
        if (text == 'pong') return;
        _dispatch(text);
      },
      onError: (_) {},
      onDone: () {
        // 只处理"当前连接"的断开，防止旧 socket 回调误杀新连接
        if (!identical(_ws, ws)) return;
        _ws = null;
        _failAllPending('ws closed');
        if (!_closedByUs) _scheduleReconnect();
      },
    );
  }

  void _scheduleReconnect() {
    _reconnect?.cancel();
    final delay = Duration(milliseconds: min(500 * (1 << _seq), 10000));
    _seq = min(_seq + 1, 5);
    _reconnect = Timer(delay, () async {
      try {
        await _open(_port!);
        _seq = 0;
      } catch (_) {
        _scheduleReconnect();
      }
    });
  }

  void _dispatch(String text) {
    Map<String, dynamic> msg;
    try {
      msg = jsonDecode(text) as Map<String, dynamic>;
    } catch (_) {
      return;
    }
    if (msg.containsKey('event')) {
      _events.add(WsEvent(
        event: msg['event'] as String,
        payload: msg['payload'],
        windowId: msg['windowId'] as int?,
      ));
    } else if (msg.containsKey('id')) {
      final completer = _pending.remove(msg['id']);
      if (completer != null && !completer.isCompleted) {
        final ok = msg.containsKey('result'); // core 协议: {id,result}|{id,error}
        completer.complete(WsReply(ok: ok, data: msg['result'] ?? msg['error']));
      }
    }
  }

  /// invoke：带 id 等待响应（30s 超时）
  Future<WsReply> invoke(String channel, {dynamic payload, int? windowId}) {
    final id = 'f${DateTime.now().microsecondsSinceEpoch}_${_seq++}';
    final completer = Completer<WsReply>();
    _pending[id] = completer;
    _sendRaw({
      'id': id,
      'channel': channel,
      'payload': payload ?? {},
      if (windowId != null) 'windowId': windowId,
    });
    return completer.future.timeout(
      const Duration(seconds: 30),
      onTimeout: () {
        _pending.remove(id);
        return WsReply(ok: false, data: 'timeout: $channel');
      },
    );
  }

  /// send：不等响应（core 会广播 response_* 事件）
  void send(String channel, {dynamic payload, int? windowId}) {
    _sendRaw({
      'id': 's${DateTime.now().microsecondsSinceEpoch}_${_seq++}',
      'channel': channel,
      'payload': payload ?? {},
      if (windowId != null) 'windowId': windowId,
    });
  }

  void _sendRaw(Map<String, dynamic> obj) {
    if (!isConnected) return;
    _ws!.add(jsonEncode(obj));
  }

  void _failAllPending(String reason) {
    for (final c in _pending.values) {
      if (!c.isCompleted) c.complete(WsReply(ok: false, data: reason));
    }
    _pending.clear();
  }

  Future<void> close() async {
    _closedByUs = true;
    _heartbeat?.cancel();
    _reconnect?.cancel();
    _ws?.close();
    _ws = null;
    _failAllPending('bridge closed');
  }
}
