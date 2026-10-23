/// 信令客户端 —— socket.io v4（对齐 web/src/utils/network/webSocket.ts）。
///
/// 传输：engine.io websocket-only（与 web 的 transports:['websocket'] 一致）。
/// 发送：emit(msgType, envelope)；接收：on(msgType) → envelope(Map)。
import 'dart:async';

import 'package:socket_io_client/socket_io_client.dart' as sio;

import 'protocol.dart';

import '../core/shell_log.dart';

/// 收到的原始信令消息（已解信封）
class SignalMessage {
  SignalMessage({required this.type, required this.data});

  final String type;
  final Map<String, dynamic> data;

  Map<String, dynamic> asMap() => data;
}

class SignalingClient {
  SignalingClient({required this.url});

  final String url;
  sio.Socket? _socket;

  String? get socketId => _socket?.id;
  bool get connected => _socket?.connected ?? false;

  final _connectedCtrl = StreamController<void>.broadcast();
  final _disconnectedCtrl = StreamController<void>.broadcast();
  final _messageCtrl = StreamController<SignalMessage>.broadcast();

  Stream<void> get onConnected => _connectedCtrl.stream;
  Stream<void> get onDisconnected => _disconnectedCtrl.stream;
  Stream<SignalMessage> get onMessage => _messageCtrl.stream;

  /// 事件订阅表（连接/重连后由本类自动恢复）
  final List<String> _subscribedEvents = <String>[];
  /// 当前 socket 实例上已绑定的事件（防重复绑定导致回调双发）
  final Set<String> _boundEvents = <String>{};

  bool _disposed = false;
  Timer? _watchdog;

  void connect() {
    final opts = sio.OptionBuilder()
        .setTransports(<String>['websocket'])
        .enableForceNew()
        .build();
    final socket = sio.io(url, opts);

    // 生命周期
    socket.onConnect((_) {
      _socket = socket;
      _boundEvents.clear();
      _restoreSubscriptions();
      ShellLog.w('[signal] 已连接 socketId=${socket.id}');
      _connectedCtrl.add(null);
    });
    socket.onDisconnect((reason) {
      // 主动重建时被 dispose 的旧实例也会回调到这里：必须与"真断开"区分，
      // 否则 receiver 会把正在跑 WebRTC 的 PC 当信令断开来拆（每 5 分钟误拆一次）。
      final stale = !identical(_socket, socket);
      ShellLog.w('[signal] disconnect reason=$reason stale=$stale');
      if (stale) return;
      _disconnectedCtrl.add(null);
      // [20261021] 真断开 2s 后兜底重建（库自带退避重连，这里只保证不被卡死）。
      Future.delayed(const Duration(seconds: 2), () {
        if (!_disposed && identical(_socket, socket) && !connected) {
          _rebuild('onDisconnect 兜底');
        }
      });
    });
    socket.onConnectError((err) {
      ShellLog.w('[signal] connectError=$err');
      _disconnectedCtrl.add(null);
    });
    socket.onError((err) {
      ShellLog.w('[signal] socket onError=$err');
    });

    _socket = socket;
    socket.connect();
    _startWatchdog();
  }

  /// [20261021] 僵尸连接看护 —— 本机实测的"壳活着但信令离线 9 小时"根因：
  /// engine.io 走「服务端主动 ping」模式，客户端只在收到 ping 时回 pong，
  /// 自身不探测下行。宽带重播/NAT 超时/服务器重启造成 TCP 半开（没有 FIN）
  /// 时，客户端 socket.connected 仍为 true、库的自动重连永不触发，表现为
  /// 进程健在、日志静止、主控连不上。watchdog 每 5 分钟主动换一条连接：
  /// 半开最长存在 5 分钟即自愈；WS 连接重建对已建立的 WebRTC 直连无损。
  void _startWatchdog() {
    _watchdog ??= Timer.periodic(const Duration(minutes: 5), (_) {
      if (_disposed) return;
      _rebuild(connected ? 'regular-rotate 防半开' : 'detected-offline');
    });
  }

  void _rebuild(String why) {
    if (_disposed) return;
    ShellLog.w('[signal] 主动重建连接（$why）');
    final old = _socket;
    _socket = null;
    _boundEvents.clear();
    try {
      old?.dispose();
    } catch (_) {}
    connect();
  }

  /// 注册事件监听（连接前调用也行，连上后自动绑定）
  void subscribe(String event) {
    if (!_subscribedEvents.contains(event)) _subscribedEvents.add(event);
    _bindEvent(event);
  }

  void _restoreSubscriptions() {
    for (final e in List<String>.of(_subscribedEvents)) {
      _bindEvent(e);
    }
  }

  void _bindEvent(String event) {
    final socket = _socket;
    if (socket == null) return;
    if (!_boundEvents.add(event)) return; // 已绑定，防双发
    // dart socket_io_client 的 on 回调参数是动态的（单 payload = List/Map）。
    // ⚠️ 服务端下行 socket.emit(msgType, data) 的 data 就是业务对象本身
    // （无信封；信封只在客户端→服务端方向）。
    socket.on(event, (payload) {
      dynamic d = payload;
      if (d is List && d.isNotEmpty) d = d.first;
      if (d is Map) {
        _messageCtrl.add(SignalMessage(
          type: event,
          data: Map<String, dynamic>.from(d),
        ));
      }
    });
  }

  /// 发送（自动套信封，对齐 WebSocketClass.send）
  void send(String msgType, Map<String, dynamic> data) {
    final socket = _socket;
    if (socket == null || !socket.connected) {
      // 对齐 web：未连接不发送，只留痕
      // ignore: avoid_print
      print('[signal] 未连接，丢弃 $msgType');
      return;
    }
    socket.emit(msgType, envelope(socketId: socketId, data: data));
  }

  /// 心跳（对齐 web 的 WsMsgTypeEnum.heartbeat）
  Timer? _hbTimer;
  void startHeartbeat() {
    _hbTimer ??= Timer.periodic(const Duration(seconds: 30), (_) {
      send(WsMsg.heartbeat, <String, dynamic>{});
    });
  }

  void stopHeartbeat() {
    _hbTimer?.cancel();
    _hbTimer = null;
  }

  void dispose() {
    ShellLog.w('[signal] SignalingClient.dispose() 被调用（谁在销毁信令？）');
    _disposed = true;
    _watchdog?.cancel();
    _watchdog = null;
    stopHeartbeat();
    try {
      _socket?.dispose();
    } catch (_) {}
    _connectedCtrl.close();
    _disconnectedCtrl.close();
    _messageCtrl.close();
  }
}
