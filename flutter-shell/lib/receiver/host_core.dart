/// 被控核宿主 —— [20261019 被控去 WebView2] 架构核心一环。
///
/// indesk-desktop.exe 以 `--core --core-port <p>` headless 运行（复用当年为
/// Electron 壳修好的 core_bridge 形态）：不创建窗口、不加载 WebView2，
/// axum 在 127.0.0.1 提供：
///   · GET /ws                —— IPC 桥（协议与主控核桥完全一致）
///   · GET /__capture/frame   —— 抓屏帧通道（NVENC H264 / JPEG）
///   · /api/* /socket.io/*    —— 服务端透传（本壳不消费，Dart 直连信令）
/// 抓屏/NVENC/输入注入/剪贴板/隐私屏等能力全部经 /ws 的 ipc_dispatch 调用。
import 'dart:async';
import 'dart:io';

import 'package:flutter/foundation.dart';

import '../core/ws_bridge.dart';
import '../core/shell_log.dart';

/// ipc_dispatch 调用（payload 形状对齐 web ipc-adapter：
/// {requestId, data:{...}}；响应 {code:0, data}）
Future<Map<String, dynamic>?> hostInvoke(
  WsBridge bridge,
  String channel, [
  Map<String, dynamic> data = const {},
]) async {
  final r = await bridge.invoke(channel, payload: {
    'requestId': 'h${DateTime.now().microsecondsSinceEpoch}',
    'data': data,
  });
  if (!r.ok) {
    debugPrint('[host-core] $channel 失败: ${r.data}');
    return null;
  }
  final m = r.data;
  if (m is Map<String, dynamic>) return m;
  return null;
}

class HostCore {
  HostCore._();
  static final HostCore shared = HostCore._();

  static const exeName = 'indesk-desktop.exe';
  static const corePort = 23655; // 23654 被主控核占用

  final WsBridge bridge = WsBridge();
  Process? _proc;
  bool _stopping = false;
  bool _starting = false;
  int _restarts = 0;

  /// 屏幕坐标基准（ipc 通道 getPrimaryDisplaySize / scaleFactor，物理像素换算用）
  int dispW = 0;
  int dispH = 0;
  double scaleFactor = 1;

  bool get running => _proc != null || bridge.isConnected;

  /// 拉起被控核并连上桥；幂等。
  Future<bool> ensureStarted() async {
    if (_starting) return false;
    _starting = true;
    try {
      if (!bridge.isConnected) {
        final ok = await _spawnAndConnect();
        if (!ok) return false;
      }
      await _probeScreen();
      return bridge.isConnected;
    } finally {
      _starting = false;
    }
  }

  Future<bool> _spawnAndConnect() async {
    final dir = File(Platform.resolvedExecutable).parent.path;
    final exe = '$dir\\$exeName';
    if (!File(exe).existsSync()) {
      ShellLog.w('[host-core] 被控核不存在: $exe');
      return false;
    }
    // 复用已在跑的实例（如上一代壳/手动拉起过）
      if (!await _probeHttp()) {
        try {
          _proc = await Process.start(exe, [
            '--core',
            '--core-port',
            '$corePort',
          ], workingDirectory: dir);
          // [20261020 2.3.31] ⭐ 持续消费核的 stdout/stderr 管道并落盘。
          // Process.start 默认接管 stdio 为管道；不读的话核的 eprintln 诊断在
          // ~4KB 内核缓冲写满后 WriteFile 永久阻塞 —— 抓屏线程卡死、15s 心跳
          // 看护杀核、30s 一轮无限自毁循环（web/tauri 时代核壳同进程无此问题）。
          // [20261020b] 消费之外还要**落盘**（core_stderr.log，>2MB 启动时清空）：
          // 抓屏线程若 panic，panic 消息就在 stderr 里 —— 丢弃 = 永远查不到。
          final stdioLog = File('$dir\\core_stderr.log');
          try {
            if (stdioLog.existsSync() && stdioLog.lengthSync() > 2 * 1024 * 1024) {
              stdioLog.deleteSync();
            }
          } catch (_) {}
          final stdioSink = stdioLog.openWrite(mode: FileMode.append);
          _proc!.stdout.listen(stdioSink.add, onError: (_) {}, cancelOnError: false);
          _proc!.stderr.listen(stdioSink.add, onError: (_) {}, cancelOnError: false);
          _proc!.exitCode.then((_) {
            try {
              stdioSink.close();
            } catch (_) {}
          });
          ShellLog.w('[host-core] 被控核已拉起 --core:$corePort');
        } catch (e) {
          ShellLog.w('[host-core] 拉起失败: $e');
          return false;
        }
      _proc!.exitCode.then((code) {
        _proc = null;
        if (_stopping) return;
        // 意外退出 → 退避重启（2s/30s）
        _restarts++;
        final delay = Duration(seconds: _restarts > 5 ? 30 : 2);
        Future.delayed(delay, () {
          if (_stopping) return;
          _spawnAndConnect();
        });
      });
      // 就绪轮询：最多 15s
      var ready = false;
      for (var i = 0; i < 30; i++) {
        await Future.delayed(const Duration(milliseconds: 500));
        if (await _probeHttp()) {
          ready = true;
          break;
        }
      }
      if (!ready) {
        ShellLog.w('[host-core] 被控核 15s 未就绪');
        return false;
      }
    }
    try {
      await bridge.connect(corePort);
      _restarts = 0;
      ShellLog.w('[host-core] 桥已连接 :$corePort');
      return true;
    } catch (e) {
      ShellLog.w('[host-core] 桥连接失败: $e');
      return false;
    }
  }

  Future<bool> _probeHttp() async {
    try {
      final req = await (HttpClient()..findProxy = (_) => 'DIRECT')
          .getUrl(Uri.parse('http://127.0.0.1:$corePort/__capture/frame?meta=1'))
          .timeout(const Duration(seconds: 2));
      final resp = await req.close().timeout(const Duration(seconds: 2));
      await resp.drain();
      return resp.statusCode == 200;
    } catch (_) {
      return false;
    }
  }

  /// 读屏幕物理尺寸与缩放（输入坐标换算基准，对齐 web appStore.primaryDisplaySize）
  Future<void> _probeScreen() async {
    try {
      final sf = await hostInvoke(bridge, 'scaleFactor');
      final ds = await hostInvoke(bridge, 'getPrimaryDisplaySize');
      scaleFactor = (sf?['data']?['scaleFactor'] as num?)?.toDouble() ?? 1;
      final d = ds?['data'];
      dispW = (d?['width'] as num?)?.toInt() ?? 0;
      dispH = (d?['height'] as num?)?.toInt() ?? 0;
      ShellLog.w('[host-core] 屏幕 ${dispW}x$dispH sf=$scaleFactor');
    } catch (_) {}
  }

  Future<void> stop() async {
    _stopping = true;
    _proc?.kill();
    _proc = null;
    await bridge.close();
  }
}

/// 解析 ipc ok() 响应的内层数据（{code:0,data:{...}} → data，失败回 null）
Map<String, dynamic>? okData(Map<String, dynamic>? res) {
  if (res == null) return null;
  final code = res['code'];
  if (code is num && code != 0) return null;
  final d = res['data'];
  if (d is Map<String, dynamic>) return d;
  return const {};
}
