/// 信令链路探针（M2 冒烟）：验证 engine.io 握手 / socket.id / 发送不炸。
/// 纯 Dart，无 Flutter 依赖。用法：flutter 无关，
///   G:\flutter\bin\dart run tool/signal_probe.dart
import 'dart:async';
import 'dart:io';

import 'package:indesk/signal/signaling_client.dart';

import '../lib/signal/protocol.dart';

Future<void> main() async {
  final url = kSignalingUrl;
  stdout.writeln('[probe] 连接 $url …');
  final signal = SignalingClient(url: url);
  final done = Completer<void>();

  Timer(const Duration(seconds: 15), () {
    stdout.writeln('[probe] 15s 超时，握手失败');
    done.complete();
  });

  signal.onConnected.listen((_) {
    stdout.writeln('[probe] CONNECTED, socketId=${signal.socketId}');
    // 发一个无害 join（乱码房间，验证 emit 链路与服务端 ack/广播不炸）
    signal.send(WsMsg.deskJoin, <String, dynamic>{
      'deskUserUuid': 'probe_room_should_not_exist',
      'deskUserPassword': '',
      'live_room_id': 'probe_room_should_not_exist',
    });
    Future.delayed(const Duration(seconds: 3), done.complete);
  });
  signal.onDisconnected.listen((_) => stdout.writeln('[probe] DISCONNECTED'));
  signal.onMessage.listen((m) =>
      stdout.writeln('[probe] 收到 ${m.type}: ${m.data}'));

  signal.connect();
  await done.future;
  stdout.writeln('[probe] 结束');
  exit(0);
}
