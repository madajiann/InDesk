/// H264 直送发送侧 —— 移植 web/src/utils/h264Stream.ts 语义到 Dart。
///
/// 数据面：被控核 NVENC 会话 → HTTP 轮询 /__capture/frame?after=<seq>
///        → 24B 包头分片 → DataChannel VideoChannel 直发主控。
/// 拥塞面：flutter_webrtc Windows 不回报 bufferedAmount（common/cpp 无
///        dataChannelBufferedAmountChange 事件），用**时钟探针 RTT 作积压
///        代理**：本端每秒在 VideoChannel 发 h264ClockPing{t1}，主控端现有
///        逻辑原样回 pong，RTT=本端收 pong 时刻-t1。通道积压 → pong 排队
///        → RTT 抬升 —— 与 bufferedAmount 增长同源同向。
/// 冻结语义：冻结=暂停拉帧（核侧槽位自然只留最新帧），恢复前必须等到
///        关键帧（chainOk 闸门）+ 主动 captureKeyframe，绝不裸发 P 帧。
import 'dart:async';
import 'dart:convert';
import 'dart:io';
import 'dart:typed_data';

import 'package:flutter/foundation.dart';

import '../core/shell_log.dart';
import 'h264_packet.dart';

const int congestHighRttMs = 300; // RTT 超此值 = 通道积压 → 冻结
const int congestLowRttMs = 120; // RTT 回落到此值 → 恢复
const int overloadAfterMs = 5000; // 持续冻结超此时长 = 过载降档

class H264SenderStats {
  int fetched = 0;
  int sent = 0;
  int packets = 0;
  int bytes = 0;
  int frozenMs = 0;
  int keys = 0;
  int rttMs = 0;
  bool frozen = false;
}

class H264Sender {
  H264Sender({
    required this.invoke,
    required this.frameBase,
    required this.sendPacket,
    required this.sendCtrl,
    required this.onOverload,
    required this.onStalled,
    required this.onChannelDead,
  });

  /// ipc 调用（captureStart/captureStop/captureKeyframe）
  final Future<Map<String, dynamic>?> Function(String, Map<String, dynamic>)
      invoke;
  /// 帧通道基址（http://127.0.0.1:<corePort>/__capture/frame）
  final String frameBase;
  /// 发一个分片（VideoChannel 二进制）；false = 通道不可用
  final Future<bool> Function(Uint8List pkt) sendPacket;
  /// VideoChannel 文本（h264Congest / h264ClockPing）
  final bool Function(String text) sendCtrl;
  /// 过载回调（调用方降档重建，语义对齐 web onOverload）
  final void Function() onOverload;
  /// [20261020] 断流自愈回调：非冻结 8s 无帧（核自毁重建/取屏会话消失）
  /// 时触发，调用方应重建取屏会话（captureStop + captureStart）。
  final void Function() onStalled;

  /// [20261021] VideoChannel 连续发送失败回调：通道已不可用（关/挂），
  /// 旧实现只 sleep(200ms) 重试，帧永远发不出去、上层毫不知情 —— 真机表现
  /// 是"画面定住不动、最后整条会话被判死"。调用方应拆除 PC 等主控重建。
  final void Function() onChannelDead;

  /// 连续发送失败计数（每次失败间隔 200ms，20 次≈4s 才算真死）。
  int _sendFailStreak = 0;
  bool _channelDeadFired = false;

  final stats = H264SenderStats();
  HttpClient? _http;
  Timer? _clockTimer;
  bool _running = false;
  int _frozenSince = 0;
  int _lastOverloadAt = 0;

  bool _frozen = false;
  int _rttEma = 0;
  final List<int> _pendingPings = []; // 发出时刻，用于配对 pong（简化：单飞）
  // [20261020] 可观测性 + 冻结自愈：RTT 代理是有噪估计（web 用精确的
  // bufferedAmountChange，我们用 pong 时延），单尖峰就冻结 = 真机「连接上
  // 画面死」的候选根因。①连续 2 次超阈值才冻结；②冻结后 5s 收不到 pong
  // 判为陈旧信号，自愈（解冻 + 催 IDR）；③每 5s 向 shell.log 打一条心跳，
  // 现场不再零线索。
  int _lastSeq = -1;
  int _lastPongAt = 0;
  int _highStreak = 0;
  int _probeTicks = 0;
  // [20261020] 断流自愈：核自毁重建（wedge/心跳看护 exit 3）后，取屏会话
  // 随核消失，本 sender 的 HTTP 轮询会永远失败——而时钟探针由 Dart 发送，
  // 主控看门狗被它喂着永不 revoke → 画面冻死到会话结束。必须本端自检：
  // 非冻结状态 8s 拉不到帧 → 回调上层重建取屏会话（captureStart）。
  int _lastFrameAtMs = 0;
  bool _stallFired = false;

  int get diagLastSeq => _lastSeq;

  int _frameId = 0;

  bool get frozen => _frozen;

  /// 启动抓屏会话并开始发送循环。失败抛异常（调用方决定降级）。
  Future<({int width, int height})> start(
      {required int fps, required int bitrateKbps, int maxWidth = 0}) async {
    Map<String, dynamic> startParams() => <String, dynamic>{
          // [20261021 16:0x] maxWidth 改由调用方按主控下发的 resolutionRatio
          // 计算（0 = 原生分辨率）。上一版硬编码 1280 是为了「降卡顿」，真因
          // 是进程优先级（已修）；硬编码的代价是真机 1680x1050 屏被缩到
          // 1280x800，主控全屏后肉眼可见糊。
          'maxWidth': maxWidth,
          'quality': 85,
          'fps': fps.clamp(1, 60),
          'mode': 'h264',
          'bitrate': bitrateKbps,
          'from': 'h264-direct',
        };
    var res = await invoke('captureStart', startParams());
    if (res == null || res['code'] != 0) {
      throw Exception('captureStart 失败: ${res?['msg']}');
    }
    var d = res['data'];
    if (d is! Map || (d['width'] as num? ?? 0) == 0) {
      throw Exception('captureStart 参数无效');
    }
    var width = (d['width'] as num).toInt();
    var height = (d['height'] as num).toInt();
    var mode = (d['mode'] ?? 'starting').toString();

    // captureStart 异步建会话：轮询 meta 直到模式定型（对齐 web 注释）。
    // [20261020] 换届（captureStop→captureStart）要同步等旧抓屏线程退出 +
    // DXGI 独占复制对象重建 + NVENC 重初始化，真机实测可超 3s —— 轮询窗口
    // 放宽到 10s；卡在 starting 超 4s 或回报 stopped 时重发一次 captureStart
    // 兜底（核侧 CAP_LOCK 串行化，重发安全）。
    final deadline = DateTime.now().add(const Duration(seconds: 10));
    var nudged = false;
    while (mode != 'h264' && DateTime.now().isBefore(deadline)) {
      await Future.delayed(const Duration(milliseconds: 300));
      if (!nudged &&
          (mode == 'stopped' ||
              (mode == 'starting' &&
                  DateTime.now()
                      .isAfter(deadline.subtract(const Duration(seconds: 4)))))) {
        nudged = true;
        await invoke('captureStop', {});
        await Future.delayed(const Duration(milliseconds: 500));
        res = await invoke('captureStart', startParams());
        if (res != null && res['code'] == 0 && res['data'] is Map) {
          d = res['data'];
          if ((d['width'] as num? ?? 0) > 0) width = (d['width'] as num).toInt();
          if ((d['height'] as num? ?? 0) > 0) {
            height = (d['height'] as num).toInt();
          }
          mode = (d['mode'] ?? 'starting').toString();
        }
        continue;
      }
      try {
        final mr = await _get(frameBase.contains('?')
            ? '$frameBase&meta=1'
            : '$frameBase?meta=1');
        if (mr.statusCode == 200) {
          final m = jsonDecode(await mr.transform(utf8.decoder).join());
          if (m is Map && m['mode'] != null) {
            mode = m['mode'].toString();
            if ((m['width'] as num? ?? 0) > 0) width = (m['width'] as num).toInt();
            if ((m['height'] as num? ?? 0) > 0) {
              height = (m['height'] as num).toInt();
            }
          }
        }
      } catch (_) {}
    }
    if (mode != 'h264') {
      await invoke('captureStop', {});
      throw Exception('被控核不支持 H264（回报格式 $mode，等 10s）');
    }
    // 首帧就要可解：主动要 IDR（对齐 web）
    unawaited(invoke('captureKeyframe', {}));

    _running = true;
    _frozen = false;
    _frozenSince = 0;
    _runLoop();
    _startClockProbe();
    return (width: width, height: height);
  }

  void stop() {
    _running = false;
    _clockTimer?.cancel();
    _clockTimer = null;
    _http?.close(force: true);
    _http = null;
    unawaited(invoke('captureStop', {}));
  }

  void dispose() {
    _running = false;
    _clockTimer?.cancel();
    _http?.close(force: true);
    _http = null;
  }

  HttpClient get _client {
    final c = _http;
    if (c != null) return c;
    final n = HttpClient()..findProxy = (_) => 'DIRECT';
    _http = n;
    return n;
  }

  Future<HttpClientResponse> _get(String url) async {
    final req = await _client
        .getUrl(Uri.parse(url))
        .timeout(const Duration(seconds: 5));
    return req.close().timeout(const Duration(seconds: 5));
  }

  /// 发送主循环：拉帧 → chainOk 闸门 → 分片直发
  Future<void> _runLoop() async {
    var lastSeq = -1;
    // [20261020] 初始 false（对齐 web h264Stream.ts:235）：会话刚建好时第一帧
    // ?after= 取到的必然是"当前最新帧"，可能是中途 P 帧 —— 主控没有参考帧解不出。
    // start() 已主动催过 IDR，闸门等 IDR 到达才放行首帧。
    _chainOk = false;
    var width = 0, height = 0;
    while (_running) {
      if (_frozen) {
        await Future.delayed(const Duration(milliseconds: 30));
        continue;
      }
      try {
        final resp = await _get('$frameBase?after=$lastSeq');
        if (resp.statusCode == 204) {
          await Future.delayed(const Duration(milliseconds: 4));
          continue;
        }
        if (resp.statusCode != 200) {
          await Future.delayed(const Duration(milliseconds: 100));
          continue;
        }
        final seq =
            int.tryParse(resp.headers.value('X-Indesk-Seq') ?? '') ?? lastSeq;
        if (seq <= lastSeq) {
          await resp.drain();
          await Future.delayed(const Duration(milliseconds: 4));
          continue;
        }
        // 跳帧检测：?after= 永远回最新帧，发送慢于抓屏时中间 AU 被跳过
        if (lastSeq >= 0 && seq > lastSeq + 1) {
          _chainOk = false;
          ShellLog.w('[h264-send] 跳帧 seq=$lastSeq→$seq，闸门关闭并催 IDR');
          // [20261020] 对齐 web :273/:468：关闸的同时主动催关键帧。旧实现只
          // 关闸不催 —— 编码器自然关键帧间隔 2s 级，期间画面 1fps 幻灯片
          // （12:44 实测 5 秒 fetched+257/sent+4 即此症）。
          _requestKeyframe();
        }
        lastSeq = seq;
        _lastSeq = seq;
        final w =
            int.tryParse(resp.headers.value('X-Indesk-W') ?? '') ?? width;
        final h =
            int.tryParse(resp.headers.value('X-Indesk-H') ?? '') ?? height;
        if (w > 0) width = w;
        if (h > 0) height = h;
        final bytes = await resp.fold<BytesBuilder>(
            BytesBuilder(copy: false), (b, chunk) {
          b.add(chunk);
          return b;
        });
        final frame = bytes.takeBytes();
        stats.fetched += 1;
        _lastFrameAtMs = _nowMs();
        _stallFired = false;
        final scan = scanFrame(frame);
        if (!_chainOk && !scan.key) {
          // [20261020] 对齐 web :466-474：断链后 P 帧直接丢弃**并催 IDR**，
          // 主动要关键帧让链条尽快恢复 —— 干等编码器自然关键帧 = 画面死。
          _requestKeyframe();
          continue; // 断链后只放行 IDR（对齐 web chainOk）
        }
        _chainOk = true;
        if (scan.key) {
          stats.keys += 1;
          _kfPending = false; // 关键帧已到手，解除催帧在途标记
        }
        final pkts =
            packetize(frame, _frameId++, _nowMs(), width, height, scan.key);
        var ok = true;
        for (final p in pkts) {
          ok = await sendPacket(p);
          if (!ok) break;
          stats.packets += 1;
          stats.bytes += p.length;
        }
        if (ok) {
          stats.sent += 1;
          _sendFailStreak = 0;
          _channelDeadFired = false;
        }
        if (!ok) {
          // 通道没了：交给上层会话处理（这里慢退避）
          _sendFailStreak += 1;
          if (_sendFailStreak == 20 && !_channelDeadFired) {
            _channelDeadFired = true;
            ShellLog.w('[h264-send] VideoChannel 连续 $_sendFailStreak 次发送失败'
                '（约 4s），判定通道已死 → 请求重建会话');
            onChannelDead();
          }
          await Future.delayed(const Duration(milliseconds: 200));
        }
      } catch (_) {
        await Future.delayed(const Duration(milliseconds: 100));
      }
    }
  }

  /// [20261020] 探测取屏会话是否还活着（meta.running）。false = 会话消失
  /// （核自毁重建/被外部停掉）→ 触发上层重建。探测失败（核重启中）静默跳过。
  Future<void> _checkCaptureAlive() async {
    try {
      final mr = await _get(frameBase.contains('?')
          ? '$frameBase&meta=1'
          : '$frameBase?meta=1');
      if (mr.statusCode != 200) return;
      final m = jsonDecode(await mr.transform(utf8.decoder).join());
      if (m is Map && m['running'] == false && !_stallFired) {
        _stallFired = true;
        ShellLog.w('[h264-send] meta.running=false（取屏会话消失），触发自愈重建');
        onStalled();
      }
    } catch (_) {}
  }

  /// 拥塞探针：1s/条 h264ClockPing；RTT 高 → 冻结 + h264Congest 心跳；
  /// 持续冻结 >5s → 过载回调（降档）。每 5 ticks 打诊断心跳。
  void _startClockProbe() {
    _clockTimer?.cancel();
    _clockTimer = Timer.periodic(const Duration(seconds: 1), (_) {
      if (!_running) return;
      _probeTicks += 1;
      if (_probeTicks % 5 == 0) {
        ShellLog.w('[h264-send] seq=$_lastSeq fetched=${stats.fetched} '
            'sent=${stats.sent} keys=${stats.keys} frozen=$_frozen '
            'rtt=$_rttEma chainOk=$_chainOk');
      }
      // [20261020] 冻结自愈：冻结判定依赖 pong 回流，若 pong 本身断了
      // （通道抖动/主控卡顿），冻结状态就永远出不去 = 画面死到会话结束。
      // 冻结中 5s 无 pong → 判定信号陈旧，解冻并催 IDR 重新建链。
      if (_frozen &&
          _lastPongAt > 0 &&
          _nowMs() - _lastPongAt > 5000) {
        ShellLog.w('[h264-send] 冻结中 5s 无 pong，判定陈旧信号，自愈');
        _frozen = false;
        stats.frozen = false;
        _frozenSince = 0;
        _highStreak = 0;
        _rttEma = 0;
        chainBroken();
      }
      // [20261020] 断流自愈：判据用 meta.running=false（取屏会话已消失，
      // 典型场景=核自毁重建后新核上还没有 capture 会话）。⚠️ 不能用
      // 「N 秒无帧」——桌面静止时 NVENC 本来就不出帧，空闲误报会造成
      // 每 10s 重启一次取屏的灾难循环。
      if (!_frozen && !_stallFired && _probeTicks % 2 == 0) {
        _checkCaptureAlive();
      }
      // 过载判定
      if (_frozen) {
        final frozenMs = _nowMs() - _frozenSince;
        stats.frozenMs = frozenMs;
        if (frozenMs > overloadAfterMs &&
            _nowMs() - _lastOverloadAt > 15000) {
          _lastOverloadAt = _nowMs();
          onOverload();
        }
      }
      // 心跳：冻结期间每秒发 h264Congest（主控 12s 无帧保险丝靠它区分拥塞/死亡）
      if (_frozen) {
        sendCtrl(jsonEncode(<String, dynamic>{
          'msgType': 'h264Congest',
          'requestId': 'congest',
          'data': <String, dynamic>{
            'bufferedKB': (_rttEma * 2), // RTT 积压代理的粗估值
            'frozenMs': _nowMs() - _frozenSince,
            'fps': 0,
          },
        }));
      }
      // 探针
      _pendingPings.add(_nowMs());
      if (_pendingPings.length > 3) _pendingPings.removeAt(0);
      sendCtrl(jsonEncode(<String, dynamic>{
        'msgType': 'h264ClockPing',
        'requestId': 'clock',
        'data': <String, dynamic>{'t1': _nowMs()},
      }));
    });
  }

  /// 收到主控回的 pong（t1=本端发送时刻）→ RTT → 冻结状态机
  void onClockPong(int t1) {
    final rtt = _nowMs() - t1;
    if (rtt < 0 || rtt > 60000) return;
    _lastPongAt = _nowMs();
    stats.rttMs = rtt;
    _rttEma = _rttEma == 0 ? rtt : (_rttEma * 7 + rtt) ~/ 8;
    // [20261020] 连续 2 次超阈值才冻结：单尖峰（主控解码卡一拍/系统抖动）
    // 不再直接冻结画面。
    if (!_frozen && _rttEma > congestHighRttMs) {
      _highStreak += 1;
      if (_highStreak >= 2) {
        _frozen = true;
        stats.frozen = true;
        _frozenSince = _nowMs();
        ShellLog.w('[h264-send] 冻结（rtt=$_rttEma 连续$_highStreak次超阈值）');
      }
    } else if (!_frozen) {
      _highStreak = 0;
    }
    if (_frozen && _rttEma < congestLowRttMs) {
      _frozen = false;
      stats.frozen = false;
      _highStreak = 0;
      ShellLog.w('[h264-send] 恢复（rtt=$_rttEma），催 IDR');
      // 恢复前必须催关键帧：冻结期间核侧仍在出帧，链条已断
      chainBroken();
    }
  }

  bool _chainOk = true;
  /// [20261021] 催 IDR 的冷却与「在途」标记。
  ///
  /// 原实现只有 250ms 节流：一旦编码器在低频率 GPU 上编一个 IDR 要几百毫秒到
  /// 1 秒，这段窗口里发送侧仍在跳帧 → 又催 → 又编 IDR，自己把自己钉死在
  /// 「跳帧↔关键帧」循环里（现场表现就是周期性卡死）。现在：
  ///  · 请求后置 `_kfPending`，收到关键帧才清；
  ///  · 在途期间不再重复请求（除非超过 3s 还没有关键帧，才补一次）。
  int _lastKfReqAt = 0;
  bool _kfPending = false;

  /// 主动催被控核出一个关键帧（幂等、带在途去重）。
  /// 这是断链恢复的唯一主动手段：跳帧/丢 P 帧/冻结恢复三处共用。
  void _requestKeyframe() {
    final now = _nowMs();
    if (_kfPending && now - _lastKfReqAt < 3000) return;
    if (!_kfPending && now - _lastKfReqAt < 250) return;
    _lastKfReqAt = now;
    _kfPending = true;
    unawaited(invoke('captureKeyframe', {}));
  }

  void chainBroken() {
    _chainOk = false;
    _requestKeyframe();
  }

  static int _nowMs() => DateTime.now().millisecondsSinceEpoch;
}
