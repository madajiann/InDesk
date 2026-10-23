/// H264 直送主控端 —— 1:1 对齐 web 主控 h264Packet.ts / h264Player.ts：
/// 被控端壳侧 NVENC 编好 Annex-B 码流，按 24 字节小端包头分片，经 DataChannel
/// 『VideoChannel』直发本端（不进 WebRTC 媒体面）；本模块组帧后喂给原生
/// Media Foundation 解码器（runner h264_video.cpp），解码结果以 D3D11 共享
/// 纹理上屏（Flutter Texture）。握手与回退见 controller_session.dart。
///
/// 包头（24B，小端）：u32 frameId / u32 tsMs / u16 chunkIndex / u16 chunkCount
/// / u32 totalLen / u16 width / u16 height / u8 flags(bit0=关键帧) / u8+u16 保留。
library;

import 'dart:async';
import 'dart:typed_data';

import 'package:flutter/services.dart';

import '../core/shell_log.dart';
import 'clock_sync.dart';

const int _pktHeader = 24;
const int _flagKey = 1;

/// frameId 环形差值（u32 回绕安全）：正 = a 比 b 新。
int _signedDiff(int a, int b) {
  final d = (a - b) & 0xFFFFFFFF;
  return d >= 0x80000000 ? d - 0x100000000 : d;
}

/// 组装中的一帧。
class _Partial {
  _Partial({
    required this.count,
    required this.totalLen,
    required this.key,
    required this.width,
    required this.height,
  }) : chunks = List<Uint8List?>.filled(count, null);
  final int count;
  final int totalLen;
  final bool key;
  final int width;
  final int height;
  final List<Uint8List?> chunks;
  int got = 0;
  final int at = DateTime.now().millisecondsSinceEpoch;
}

/// 主控端 H264 直送接收器：组帧 + 原生解码桥 + 断帧保护。
class H264Direct {
  static const MethodChannel _ch = MethodChannel('indesk/h264');

  /// 原生外接纹理 id（init 成功后非空）。
  int? textureId;

  /// 首帧画面尺寸（来自包头/原生回调）。
  int width = 0;
  int height = 0;
  bool gotFirstFrame = false;

  /// 直送是否处于"接管画面"状态（revoke/复位后变 false，UI 切回 WebRTC 轨）。
  bool directUp = false;
  final _stateCtrl = StreamController<bool>.broadcast();
  Stream<bool> get stateStream => _stateCtrl.stream;

  /// 参考链断了 → 上层发 h264Keyframe 给被控端（节流已做）。
  void Function()? onNeedKeyframe;

  // 组帧状态
  final Map<int, _Partial> _partials = {};
  bool _broken = true;
  int _brokenSkip = 0; // broken 后已跳过的非关键帧数（最多 3 帧就恢复）
  int _lastSubmitted = -1;
  int _lastKfReqAt = 0;
  int _lastKeyAt = 0;

  // [20261021] 解码背压：平台通道 invokeMethod('feed') 是无界队列 —— 旧实现
  // 每帧无条件发射、既不 await 也不限并发。原生解码一旦跟不上（弱机/高分辨率），
  // 每帧 50~230KB 的载荷就在通道里无界堆积：内存暴涨 + 主线程持续做跨通道拷贝
  // → 主控 UI 事件循环被拖死（真机表现：鼠标点不动、打字打不上、画面越来越卡），
  // 最后连 VideoChannel 都不再被消费，被控端发送全失败、ICE 跟着判死。
  // 这里给 feed 加在飞计数：>2 时丢弃非关键帧并等下一个 IDR，永不无界堆积。
  int _feedInFlight = 0;
  int feedDropped = 0;

  // 统计（数据栏用）
  int packets = 0;
  int submitted = 0;
  int dropped = 0;
  int decoded = 0;
  int bytes = 0; // [20261019f] 收包字节累计（直送态码率显示用）
  int brokenCount = 0; // [20261019h] 参考链断帧次数（数据栏坏帧显示用）
  int lastTsMs = 0; // [20261019h] 最近提交解码帧的被控时间戳（画面延迟口径）
  DateTime? lastFrameAt;
  DateTime? lastCtrlAt;

  // [20261019j] 端到端延迟口径：被控端会对 h264ClockPing 回 pong（web 被控
  // bindChannelHandlers 现成逻辑），四时间戳法估出钟差后即可把帧时间戳折算
  // 成真延迟 —— 旧的「相对基线漂移」量不到恒定延迟，用户实测 -8~+4ms 却
  // 体验秒级迟滞，就是它瞎。
  final ClockSync clock = ClockSync();

  /// [20261019j] 被控拥塞心跳（h264Congest，冻结期间 1s/条）：
  /// 发送端实际 fps / 发送队列积压 KB / 已冻结时长 ms。0 值 = 通道闲。
  int ctrlSendFps = 0;
  int ctrlBufferedKB = 0;
  int ctrlFrozenMs = 0;
  DateTime? lastCongestAt;

  /// 拥塞心跳是否新鲜（3s 内有心跳才算在冻结/拥塞中）。
  bool get congestFresh {
    final a = lastCongestAt;
    return a != null && DateTime.now().difference(a).inMilliseconds < 3000;
  }

  /// 真端到端延迟（ms）：最近一帧的被控时间戳折算到本机时间轴。
  /// 时钟未对齐 / 结果不合理 → null（数据栏显示「对齐中」）。
  int? endToEndLatencyMs() {
    return clock.latencyOf(
      lastTsMs,
      DateTime.now().millisecondsSinceEpoch,
    );
  }

  /// 原生解码器是否可用（JPEG 兼容链是否该让位）。
  Future<bool> init() async {
    if (textureId != null) return true;
    try {
      _ch.setMethodCallHandler(_onNativeCall);
      final id = await _ch.invokeMethod<dynamic>('init');
      if (id is int && id >= 0) {
        textureId = id;
        ShellLog.w('[h264] 原生解码纹理就绪 id=$id');
        return true;
      }
      ShellLog.w('[h264] 原生解码 init 返回异常值: $id');
    } catch (e) {
      ShellLog.w('[h264] 原生解码初始化失败: $e');
      _ch.setMethodCallHandler(null);
    }
    return false;
  }

  Future<dynamic> _onNativeCall(MethodCall call) async {
    if (call.method == 'onFirstFrame') {
      final m = call.arguments;
      if (m is Map) {
        width = (m['w'] as num?)?.toInt() ?? width;
        height = (m['h'] as num?)?.toInt() ?? height;
      }
      gotFirstFrame = true;
      ShellLog.w('[h264] 首帧已解码上屏 ${width}x$height');
      _setUp(true);
    } else if (call.method == 'onDecoderFailed') {
      // 原生建不出解码器：纹理留着也黑屏，直接作废，UI 永远走 WebRTC 轨。
      textureId = null;
      _setUp(false);
    }
    return null;
  }

  void _setUp(bool up) {
    if (directUp == up) return;
    directUp = up;
    if (!_stateCtrl.isClosed) _stateCtrl.add(up);
  }

  /// DataChannel 二进制分片入口（VideoChannel onmessage 直调）。
  void onPacket(Uint8List buf) {
    if (buf.length < _pktHeader) return;
    packets += 1;
    bytes += buf.length;
    final bd = ByteData.sublistView(buf);
    final frameId = bd.getUint32(0, Endian.little);
    final tsMs = bd.getUint32(4, Endian.little);
    final chunkIndex = bd.getUint16(8, Endian.little);
    final chunkCount = bd.getUint16(10, Endian.little);
    final totalLen = bd.getUint32(12, Endian.little);
    final w = bd.getUint16(16, Endian.little);
    final h = bd.getUint16(18, Endian.little);
    final key = (bd.getUint8(20) & _flagKey) != 0;
    if (chunkCount == 0 || chunkIndex >= chunkCount || totalLen == 0) return;

    final payload = Uint8List.sublistView(buf, _pktHeader);
    // [20261019e] 采样留痕：每 300 片打一行，回环取证用
    if (packets % 300 == 0) {
      ShellLog.w('[h264] 收包 packets=$packets submitted=$submitted '
          'dropped=$dropped broken=$_broken feedInflight=$_feedInFlight '
          'feedDropped=$feedDropped');
    }

    // ① 单分片帧（绝大多数 P 帧）：零拷贝直送
    if (chunkCount == 1) {
      if (totalLen != payload.length) return;
      _retireStale(frameId);
      _submit(frameId, payload, key, tsMs, w, h);
      return;
    }

    // ② 多分片组帧
    var p = _partials[frameId];
    if (p == null) {
      if (chunkIndex != 0) {
        // 首片不是第 0 片：前片已丢在乱序窗口外，整帧作废
        _retireStale(frameId);
        return;
      }
      _retireStale(frameId);
      p = _Partial(
          count: chunkCount,
          totalLen: totalLen,
          key: key,
          width: w,
          height: h);
      _partials[frameId] = p;
    } else if (p.count != chunkCount || p.totalLen != totalLen) {
      _partials.remove(frameId);
      dropped += 1;
      return;
    }
    if (p.chunks[chunkIndex] != null) return; // 重复分片
    p.chunks[chunkIndex] = payload;
    p.got += 1;
    if (p.got < p.count) return;

    // 齐了
    _partials.remove(frameId);
    final out = Uint8List(totalLen);
    var off = 0;
    for (final c in p.chunks) {
      if (c == null || off + c.length > totalLen) {
        dropped += 1;
        return;
      }
      out.setRange(off, off + c.length, c);
      off += c.length;
    }
    if (off != totalLen) {
      dropped += 1;
      return;
    }
    _submit(frameId, out, p.key, tsMs, p.width, p.height);
  }

  /// 清掉比 frameId 更旧的残帧（乱序窗口毫秒级，更老的永远凑不齐）。
  void _retireStale(int frameId) {
    if (_partials.isEmpty) return;
    final keys = List<int>.from(_partials.keys);
    for (final k in keys) {
      if (_signedDiff(frameId, k) > 0) {
        _partials.remove(k);
        dropped += 1;
      }
    }
  }

  void _submit(int frameId, Uint8List frame, bool key, int tsMs, int w, int h) {
    final tid = textureId;
    if (tid == null) return; // 原生没就绪就不消费（也不发 ready 的会话到不了这）

    // 丢帧保护：参考链断了，后续 P 帧只会花屏 → 等 IDR。
    if (_lastSubmitted >= 0 &&
        _signedDiff(frameId, _lastSubmitted) > 1 &&
        !key) {
      if (!_broken) brokenCount += 1;
      _broken = true;
    }
    final now = DateTime.now().millisecondsSinceEpoch;
    if (_broken && !key) {
      _requestKeyframe(now);
      // [20261021 16:0x] 原来这里 return 到拿到 IDR 为止 —— 一旦丢一帧而
      // IDR 又要等 idr_period（10s），画面就定住 10 秒，用户看到的就是
      // 「点啥也不管用」。现在最多跳过 3 帧（约 100ms）就恢复提交：解码器
      // 对缺失参考会出短暂花屏，但画面继续动，远好过定住。
      _brokenSkip += 1;
      if (_brokenSkip <= 3) return;
      _broken = false;
      _brokenSkip = 0;
    }

    _lastSubmitted = frameId;
    lastTsMs = tsMs; // [20261019h] 画面延迟口径
    // [20261021] 背压闸门：解码器落后时丢非关键帧（关键帧永远放行，否则永远
    // 恢复不了）。丢帧必然断参考链 → 置 broken 并催 IDR，与 web 的
    // decodeQueueSize 语义一致。
    // [20261021 15:5x] 阈值 2 → 6：2 太紧，主控解码单帧有正常抖动（软解
    // 1080p 一次 20~40ms），只要抖动就累到 2 → 每帧非关键帧被丢 → 整条
    // 参考链断 → 只剩 IDR 能显示（真机症状：画面几乎不动、点完没反应、
    // 被控端 5s 内被催 3~4 次 IDR）。阈值 6 仍能拦住真正的解码卡死。
    if (!key && _feedInFlight >= 6) {
      feedDropped += 1;
      _broken = true;
      _requestKeyframe(now);
      return;
    }
    // 丢帧后补了 IDR：从这帧重新同步
    _feedInFlight += 1;
    unawaited(() async {
      try {
        await _ch.invokeMethod('feed', {
          'data': frame,
          'key': key,
          'ts': tsMs,
          'w': w,
          'h': h,
        });
      } catch (_) {
        dropped += 1;
      } finally {
        _feedInFlight -= 1;
      }
    }());
    submitted += 1;
    decoded += 1; // [20261019f] 数据栏直送态帧率口径
    lastFrameAt = DateTime.now();
    if (w > 0) width = w;
    if (h > 0) height = h;
    if (key) {
      _broken = false;
      _brokenSkip = 0;
    }
  }

  void _requestKeyframe(int now) {
    if (now - _lastKfReqAt < 300) return; // 节流：断帧风暴时别刷爆被控端
    _lastKfReqAt = now;
    onNeedKeyframe?.call();
  }

  /// 会话数据栏/看门狗读的活跃时间戳。
  DateTime? get lastActivityAt {
    final a = lastFrameAt;
    final b = lastCtrlAt;
    if (a == null) return b;
    if (b == null) return a;
    return a.isAfter(b) ? a : b;
  }

  /// 会话重建时清空组帧状态（解码器纹理保留复用）。
  void reset() {
    _partials.clear();
    _broken = true;
    _lastSubmitted = -1;
    gotFirstFrame = false;
    _setUp(false);
  }

  Future<void> dispose() async {
    textureId = null;
    gotFirstFrame = false;
    directUp = false;
    reset();
    if (!_stateCtrl.isClosed) _stateCtrl.close();
    try {
      await _ch.invokeMethod('dispose');
    } catch (_) {}
  }
}
