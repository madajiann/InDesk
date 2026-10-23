/// 远程画面页（主控端）—— 对齐 web/src/views/webrtc/index.vue 的控制面：
/// 画面 = RTCVideoView（contain，黑边不参与归一化）；
/// 输入 = 鼠标合帧(0~1000 归一化) / 拖动 / 单双击 / 滚轮格数 / 键盘 nut 键码；
/// 工具栏 = [20261017c] 1:1 对齐 web 版 pc-toolbar：浅色横排 ——
///   ☰菜单(下拉) | 画质档位(下拉) | 隐私屏 | ……spacer…… | 红色断开连接。
///   用户明确反馈旧版右上角一排小按钮难看，按原版布局回改。
import 'dart:async';
import 'dart:math' as math;

import 'package:flutter/gestures.dart';
import 'package:flutter/material.dart';
import 'package:flutter/rendering.dart';
import 'package:flutter/services.dart';
import 'package:flutter_webrtc/flutter_webrtc.dart';
import 'package:window_manager/window_manager.dart';

import '../core/clip_sync.dart';
import '../core/prefs.dart';
import '../core/recent.dart';
import '../core/screen.dart';
import '../core/shell_log.dart';
import '../core/window_channel.dart';
import '../signal/protocol.dart' show NutKey;
import '../webrtc/controller_session.dart';
import '../webrtc/keyboard_map.dart';

/// 滚轮一 tick 的像素（对齐 web WHEEL_TICK_PX）
const double _wheelTickPx = 100;

class RemotePage extends StatefulWidget {
  const RemotePage({super.key, required this.session});

  final ControllerSession session;

  @override
  State<RemotePage> createState() => _RemotePageState();
}

class _RemotePageState extends State<RemotePage> with WindowListener {
  final _renderer = RTCVideoRenderer();
  bool _rendererReady = false;
  bool _privacy = false;

  // [20261022 黑边修复] 用户实测：连上后画面顶部/底部各一条窄黑框，约 1 秒
  // 后才变完整。根因不是渲染 bug，而是**窗口出生比例与对端视频比例不一致**：
  // ensureSessionWindowSize() 只能按「上次会话的视频尺寸」出生，对端换了分辨率
  // 就必然错一档；随后 _statTimer 的 1s 轮询里 alignSessionWindowToVideo 把窗口
  // 收对了，黑边才消失 —— 用户看到的就是"先黑框、再变完整"。
  // 修法：**在拿到真实视频尺寸并完成窗口对齐之前，画面区只铺纯黑**，对齐完成
  // 后才一次性显示画面。这样用户只会看到 黑底 → 完整桌面，不会再有黑框中间态。
  bool _videoSettled = false;
  bool _settleStarted = false;
  Timer? _settleGuard;

  // [20261022 剪贴板同步] 右下角传输气泡：复制文本/文件后浮一张小卡（带进度），
  // 完成 3.5s 自动收起。位置在数据栏上方，不抢画面也不盖托盘角标。
  ClipEvent? _clipToast;
  Timer? _clipToastTimer;
  StreamSubscription<ClipEvent>? _clipSub;

  // [20261017c] 最后已知坐标（0~1000）：web 版点击指令原本不带坐标、
  // 只在被控「当前光标处」按下，前一条 mouseMove 一旦丢失点击就落在
  // 旧位置（实测症状「点什么都没反应」）。这里所有点击/滚轮都带上
  // 最后已知坐标，被控端先定位再按下，点击自身具备自愈能力。
  int _lastX = 0;
  int _lastY = 0;

  // [20261017c] 鼠标移动节流：对齐 web rAF 合帧语义，但**不依赖帧调度**
  //（addPostFrameCallback 在画面静止无新帧时可能一直挂起 → mouseMove
  // 一条都发不出 → 用户实机「鼠标点不了」的头号嫌疑）。改为普通节流：
  // 16ms 一条，多余丢弃，socket 压力与合帧等价且永不挂死。
  static const _moveThrottle = Duration(milliseconds: 16);
  DateTime _lastMoveSent = DateTime.fromMillisecondsSinceEpoch(0);

  // 滚轮累积器（小数余量留给下一事件，不丢滚动量）
  double _wheelAccY = 0;

  final _textCtrl = TextEditingController();
  final _focusNode = FocusNode(debugLabel: 'remoteInput');

  // [20261019 键盘重构] 打字问题根因：旧 _onKey 把字母/数字也当
  // keyboardPressKey/ReleaseKey 的 nut 键码逐键转发 → 被控端输入法收到的是
  // 裸字母键，拼音组了音永远不落字（用户实测：那边输入法显示打字、文字
  // 不进输入框）。对齐上游 web 主控的正确模型：
  //   ① 控制键（修饰键/方向/F1-12/Enter/Esc…）→ keyboardPressKey/ReleaseKey；
  //   ② 文字（字母/数字/空格/标点/中文）→ 隐藏 TextField 收 IME 组合结果，
  //      onChanged 差量 → keyboardType **整段上屏**（被控端 SendInput(UNICODE)
  //      直投 WM_CHAR，绕过被控端输入法 —— 上游注释原文：「被控端不必装同款
  //      输入法也能收到中文」）。
  final _imeCtrl = TextEditingController();
  final _imeFocus = FocusNode(debugLabel: 'remoteIme');
  String _imePrev = '';
  bool _imeBusy = false;
  // 组词保护：组词期间的 Enter（候选确认）/空格（选候选）/退格（删拼音）/
  // 方向键（翻候选）全部本地吞掉，绝不转发 —— 否则候选确认键会打到被控端
  // （上游同款处理：「组词期间的回车/空格已在 handleKeyDown 里拦掉」）。
  bool _wasComposing = false;

  bool get _imeComposing => _imeCtrl.value.composing != TextRange.empty;

  // [20261017d] 视频区 Listener 的 GlobalKey：坐标映射必须用「Listener 的真实
  // 尺寸」配「Listener 相对的 localPosition」。之前误用 context.size（=整个
  // Scaffold 1000x679），与 Listener(1000x637) 错位一个工具栏高度 → 2.3.5 起
  // 所有点击/移动整体偏上 ~42px（点 A 落 B、窗口拖不动、点击打不中）。
  final _listenerKey = GlobalKey();

  Size? get _listenerBox {
    final ro = _listenerKey.currentContext?.findRenderObject();
    return (ro is RenderBox && ro.hasSize) ? ro.size : null;
  }

  StreamSubscription<MediaStream>? _streamSub;
  // [20261019e H264 直送] 直送画面状态（Texture 接管显示层）
  StreamSubscription<bool>? _h264StateSub;
  bool _h264Up = false;
  int _h264W = 0;
  int _h264H = 0;
  // [20261019h] 窗口贴合视频比例的幂等标记 + 画面延迟基线（首采样的
  // 本地时钟-被控时钟差，之后只显示相对漂移，规避两机时钟偏差）。
  String _lastAlignedVideo = '';

  /// [20261019k] 视口宽高比与视频宽高比的相对偏差（contain 黑边的直接来源）。
  double _viewportDeviation(Size? vp, int vw, int vh) {
    if (vp == null || vp.width < 50 || vp.height < 50 || vw <= 0 || vh <= 0) {
      return 0;
    }
    final a1 = vp.width / vp.height;
    final a2 = vw / vh;
    return (a1 - a2).abs() / a2;
  }

  // [20261017d] 全屏只由工具栏菜单手动触发（对齐 web 上游：连接不改变
  // 主控窗口大小），状态跟随窗口真实事件，断开不做任何窗口还原。
  bool _isFullscreen = false;

  // 工具栏浮层（对齐 web pc-menu / pc-dd）
  bool _menuOpen = false;
  bool _qualityOpen = false;
  String _tier = ShellPrefs.tierId;

  // [20261018] 右上角会话数据栏（对齐 web pc-data-bar）：**默认展开**，
  // 不想看点一下收成「数据」小条。之前版本这里是一行硬编码文本，窄窗口
  // 直接出框看不清 —— 现在固定宽度+可收起，永不溢出。
  bool _dataOpen = true;
  Map<String, String> _statRows = const {};
  Timer? _statTimer;
  double _prevBytes = -1;
  int _prevFrames = -1;
  DateTime _prevStatAt = DateTime.fromMillisecondsSinceEpoch(0);
  // [20261019f] 直送态统计基线（口径切到 H264 直送时用）
  int _prevH264Bytes = -1;
  int _prevH264Decoded = -1;
  // [20261019] 被控端 IP 捕获（一次会话只存一次）
  bool _ipSaved = false;

  void _toggleDataBar() {
    setState(() => _dataOpen = !_dataOpen);
    if (_dataOpen) {
      _pollStats();
      _statTimer ??=
          Timer.periodic(const Duration(seconds: 2), (_) => _pollStats());
    } else {
      _statTimer?.cancel();
      _statTimer = null;
    }
  }

  /// 从 RTCPeerConnection 拉统计：分辨率/帧率/码率/延迟/丢帧（对齐 web 数据栏）
  Future<void> _pollStats() async {
    final pc = widget.session.pc;
    if (pc == null || !mounted) return;
    try {
      String link = '已连接', res = '', fps = '', kbps = '', rtt = '', loss = '';
      final cs = pc.connectionState.toString();
      if (cs.contains('Connected')) {
        link = '已连接';
      } else if (cs.contains('Connecting')) {
        link = '连接中…';
      } else if (cs.contains('Failed')) {
        link = '已断开';
      }
      double bytes = -1;
      int frames = -1;
      final now = DateTime.now();
      // [20261019] ICE 候选收集：拿到被控端真实地址（点对点，免服务端配合）。
      // 键名做了多版本兼容：不同 libwebrtc 版本 remote-candidate /
      // remotecandidate、remoteCandidateId / remoteCandidateID 混用过。
      String? pairRemoteId;
      final remotes = <String, Map<dynamic, dynamic>>{};
      final seenTypes = <String>{};
      var pairSeen = false;
      for (final r in await pc.getStats()) {
        final v = r.values;
        seenTypes.add(r.type);
        if (r.type == 'candidate-pair') {
          pairSeen = true;
          final rv = v['currentRoundTripTime'];
          if (rv is num && rv > 0) rtt = '${(rv * 1000).round()} ms';
          final rid = (v['remoteCandidateId'] ?? v['remoteCandidateID'])
              ?.toString();
          final st = v['state']?.toString() ?? '';
          final nominated = v['nominated'] == true;
          if (rid != null &&
              (st == 'succeeded' || nominated || pairRemoteId == null)) {
            pairRemoteId = rid;
          }
        } else if (r.type == 'remote-candidate' ||
            r.type == 'remotecandidate') {
          remotes[r.id] = v;
        } else if (r.type == 'inbound-rtp' &&
            (v['kind'] == 'video' || v['mediaType'] == 'video')) {
          final w = v['frameWidth'];
          final h = v['frameHeight'];
          if (w is num && h is num && w > 0) res = '${w}x$h';
          bytes = (v['bytesReceived'] as num?)?.toDouble() ?? -1;
          frames = (v['framesDecoded'] as num?)?.toInt() ?? -1;
          final dropped = (v['framesDropped'] as num?)?.toInt() ?? 0;
          loss = dropped > 0 ? '丢帧 $dropped' : '正常';
        }
      }
      // [20261019] 被控 IP 入库（一次）：candidateType → 中文链路描述
      if (!_ipSaved && pairRemoteId != null && remotes[pairRemoteId] != null) {
        final rc = remotes[pairRemoteId]!;
        var ip = (rc['ip'] ?? rc['address'])?.toString() ?? '';
        var kind = (rc['candidateType'] ?? rc['candidate-type'])?.toString() ??
            '';
        if (ip.isEmpty) {
          // 兜底：从 SDP candidate 字符串解析
          // <foundation> <comp> <transport> <priority> <ip> <port> typ <type>
          final c = rc['candidate']?.toString() ?? '';
          if (c.isNotEmpty) {
            final parts = c.replaceAll('candidate:', '').trim().split(RegExp(r'\s+'));
            if (parts.length >= 5) {
              ip = parts[4];
              final tIdx = parts.indexOf('typ');
              if (tIdx >= 0 && tIdx + 1 < parts.length) kind = parts[tIdx + 1];
            }
          }
        }
        final code = widget.session.remoteCode;
        if (ip.isNotEmpty && code != null && code.isNotEmpty) {
          final label = switch (kind) {
            'host' => '内网直连',
            'srflx' || 'prflx' => '公网P2P',
            'relay' => '中继',
            _ => '',
          };
          RecentStore.updateIp(code, ip, ipType: label);
          _ipSaved = true;
          ShellLog.w('[remote] 已记录被控 IP: $ip ($label)');
        }
      } else if (!_ipSaved && pairSeen) {
        // 一次性诊断：键名/结构对不上时留证据，便于下一轮修
        ShellLog.w('[remote] IP 捕获未命中: pairRemoteId=$pairRemoteId '
            'remote候选数=${remotes.length} types=$seenTypes');
      }
      // [20261019c] 第二捕获源兜底：信令 candidate SDP 解析结果
      // （getStats 在部分版本不下发 remote-candidate 报告，此路必有数据）
      if (!_ipSaved) {
        final rip = widget.session.remoteIp;
        final rtype = widget.session.remoteIpType ?? '';
        final code = widget.session.remoteCode;
        if (rip != null && rip.isNotEmpty && code != null && code.isNotEmpty) {
          final label = switch (rtype) {
            'host' => '内网直连',
            'srflx' || 'prflx' => '公网P2P',
            'relay' => '中继',
            _ => '',
          };
          RecentStore.updateIp(code, rip, ipType: label);
          _ipSaved = true;
          ShellLog.w('[remote] 已记录被控 IP(candidate): $rip ($label)');
        }
      }
      final dt = now.difference(_prevStatAt).inMilliseconds;
      // [20261019f] 直送接管态：WebRTC 轨已无流（被控端 JPEG 链已停，inbound-rtp
      // 冻结在最后一帧），帧率/码率/分辨率/丢帧全部切到 H264 直送统计口径；
      // rtt 仍取 PC candidate-pair（通道本身还活着）。
      final direct = widget.session.h264.directUp;
      if (direct) {
        final h264 = widget.session.h264;
        if (dt > 200) {
          if (_prevH264Bytes >= 0) {
            kbps =
                '${(((h264.bytes - _prevH264Bytes) * 8) / dt).round()} kbps';
          }
          if (_prevH264Decoded >= 0) {
            fps =
                '${(((h264.decoded - _prevH264Decoded) * 1000) / dt).round()} fps';
          }
        }
        _prevH264Bytes = h264.bytes;
        _prevH264Decoded = h264.decoded;
        // [20261019j] 真端到端延迟：帧时间戳（被控轴）经钟差对齐折算到本机。
        // 旧的「相对基线漂移」量不到恒定延迟（用户实测 -8~+4ms 却秒级迟滞）。
        // 时钟未对齐前显示进度，不显示假数。
        if (h264.lastTsMs > 0) {
          final e2e = h264.endToEndLatencyMs();
          rtt = e2e != null ? '画面 $e2e ms' : '对齐中(${h264.clock.samples})';
        }
        // [20261019j] 拥塞心跳透出：被控在冻结/拥塞期 1s/条上报发送 fps、
        // 发送队列积压、冻结时长 —— 别让「拥塞降档」看着像无故障的正常。
        if (h264.congestFresh) {
          loss = '拥塞 发${h264.ctrlSendFps}fps '
              '积压${h264.ctrlBufferedKB}KB '
              '冻结${(h264.ctrlFrozenMs / 1000).toStringAsFixed(0)}s';
        } else if (h264.brokenCount > 0) {
          loss = '丢帧 ${h264.dropped} · 坏帧 ${h264.brokenCount}';
        } else if (h264.dropped > 0) {
          loss = '丢帧 ${h264.dropped}';
        } else {
          loss = '正常';
        }
        if (h264.width > 0 && h264.height > 0) {
          res = '${h264.width}x${h264.height}';
        }
      } else if (dt > 200) {
        if (bytes >= 0 && _prevBytes >= 0) {
          kbps = '${(((bytes - _prevBytes) * 8) / dt).round()} kbps';
        }
        if (frames >= 0 && _prevFrames >= 0) {
          fps = '${(((frames - _prevFrames) * 1000) / dt).round()} fps';
        }
      }
      _prevBytes = bytes;
      _prevFrames = frames;
      _prevStatAt = now;
      if (!direct && res.isEmpty && _renderer.videoWidth > 0) {
        res = '${_renderer.videoWidth}x${_renderer.videoHeight}';
      }
      // [20261019j/k] 全画面无黑边：窗口等比贴合视频宽高比。
      // 两种情况都要贴：①视频尺寸变了；②视口比例仍偏离视频比例 >1%
      // （自愈——首帧贴合时若量到旧布局/窗口动画未完，下一轮轮询修正）。
      final vw = direct
          ? widget.session.h264.width
          : _renderer.videoWidth;
      final vh = direct
          ? widget.session.h264.height
          : _renderer.videoHeight;
      if (vw > 0 && vh > 0) {
        final key = '${vw}x$vh';
        final vp = _listenerKey.currentContext?.size;
        if (_videoSettled &&
            (key != _lastAlignedVideo ||
                _viewportDeviation(vp, vw, vh) > 0.004)) {
          _lastAlignedVideo = key;
          alignSessionWindowToVideo(vw, vh, viewport: vp);
        }
      }
      if (!mounted) return;
      setState(() {
        _statRows = {
          '连通性': link,
          '分辨率': res,
          '帧率': fps,
          '码率': kbps,
          '延迟': rtt,
          '丢帧': loss,
          '档位': _tierName(_tier),
        };
      });
    } catch (_) {}
  }

  @override
  void initState() {
    super.initState();
    windowManager.addListener(this);
    // [20261018] 会话页顶栏按钮左右都有 → 收窄原生拖动区两侧预留，
    // 中间空白区加宽（修复「连接上以后窗口拖不动」）
    setDragBand(260, 180);
    // [20261018] 数据栏默认展开 → 立即开始轮询统计
    _statTimer ??= Timer.periodic(const Duration(seconds: 2), (_) => _pollStats());
    _applyPresetFromPrefs();
    _initRenderer();
    // [20261022] 兜底：万一 onResize 一直不触发（老解码路径/异常流），
    // 3s 后强制放行，绝不能把用户永久钉在黑底上。
    _settleGuard = Timer(const Duration(seconds: 3), () {
      if (!mounted || _videoSettled) return;
      ShellLog.w('[remote] 首帧对齐超时，强制放行画面');
      setState(() => _videoSettled = true);
    });
    _streamSub = widget.session.remoteStreamStream.listen(_onStream);
    // [20261022] 剪贴板/文件同步进度 → 右下角气泡
    _clipSub = widget.session.clipEvents.listen(_onClipEvent);
    // [20261019e H264 直送] 订阅直送状态：直送接管画面后 Texture 盖过
    // WebRTC 轨（JPEG 链在被控端已停，轨上只剩最后一帧）；revoke 后切回。
    _h264StateSub = widget.session.h264.stateStream.listen((up) {
      ShellLog.w('[remote] H264 直送状态: $up '
          'texture=${widget.session.h264.textureId} '
          '${widget.session.h264.width}x${widget.session.h264.height}');
      if (mounted) {
        setState(() {
          _h264Up = up && widget.session.h264.textureId != null;
          _h264W = widget.session.h264.width;
          _h264H = widget.session.h264.height;
        });
        // [20261019k] 首帧解码即贴合窗口（不等 2s 统计轮询）——配合
        // ensureSessionWindowSize 按"上次视频尺寸"出生，正常连接全程零过渡。
        if (up && _h264W > 0 && _h264H > 0) {
          _lastAlignedVideo = '${_h264W}x$_h264H';
          // [20261022] 走同一条"先对齐再放行"路径，避免黑框中间态。
          _settleVideoOnce();
        }
      }
    });
    if (widget.session.h264.directUp) {
      _h264Up = true;
      _h264W = widget.session.h264.width;
      _h264H = widget.session.h264.height;
    }
    // [20261019 键盘重构] 焦点给隐藏输入框（IME 组合需要 TextInput 通道），
    // 控制键经焦点链冒泡到外层 Focus.onKeyEvent 转发。
    _imeCtrl.addListener(_onImeChanged);
    _imeFocus.addListener(_onImeFocusChanged);
    WidgetsBinding.instance.addPostFrameCallback((_) {
      if (mounted) _imeFocus.requestFocus();
    });
  }

  /// [20261019c] 焦点取证：隐藏框拿到/丢失焦点都留痕，_field 从未聚焦过
  /// 这类问题不能再靠猜。
  bool _imeHadFocus = false;
  void _onImeFocusChanged() {
    final has = _imeFocus.hasFocus;
    if (has != _imeHadFocus) {
      _imeHadFocus = has;
      ShellLog.w('[ime] 隐藏输入框焦点: ${has ? '获得' : '丢失'}');
    }
  }

  /// 连接画质预设 → 会话内下发。被控端握手期可能先按本地默认档起流
  /// （实测默认只有 2Mbps → 1080p 被 H264 编码器限到 13fps = 用户反馈的
  /// 「不流畅」），所以①进会话立即下发一次，②8 秒后再复发一次兜底
  /// （被控端重协商/档位同步路径可能覆盖），③值用 sessionPreset() 收敛
  /// 脏存储组合。
  void _applyPresetFromPrefs() {
    final preset = ShellPrefs.sessionPreset();
    void send() {
      if (!mounted) return;
      ShellLog.w('[remote] 预设下发开始: res=${preset.res} '
          'fps=${preset.fps} kbps=${preset.kbps} tier=${ShellPrefs.tierId}');
      try {
        widget.session.changeResolutionRatio(preset.res);
        ShellLog.w('[remote] 分辨率消息已发');
        widget.session.changeMaxFramerate(preset.fps);
        ShellLog.w('[remote] 帧率消息已发');
        widget.session.changeMaxBitrate(preset.kbps);
        ShellLog.w('[remote] 码率消息已发');
      } catch (e, st) {
        ShellLog.w('[remote] 下发画质预设失败: $e\n$st');
      }
    }

    // 立即 + 8 秒后复发（被控端连接初期可能用本地默认档起流）
    send();
    Future.delayed(const Duration(seconds: 8), send);
  }

  Future<void> _initRenderer() async {
    await _renderer.initialize();
    if (!mounted) return;
    // [20261022 黑边修复] 首帧尺寸一到位立刻对齐窗口，不再等 1s 统计轮询。
    _renderer.onResize = _onRendererResize;
    final existing = widget.session.remoteStream;
    if (existing != null) {
      _renderer.srcObject = existing;
      setState(() => _rendererReady = true);
      _probeVideoSize();
    } else {
      setState(() {});
    }
  }

  void _onRendererResize() {
    if (!mounted) return;
    ShellLog.w('[remote] 视频尺寸到达 '
        '${_renderer.videoWidth}x${_renderer.videoHeight}');
    _settleVideoOnce();
  }

  // ------------------------------------------------------------ 传输气泡

  void _onClipEvent(ClipEvent e) {
    if (!mounted) return;
    _clipToastTimer?.cancel();
    setState(() => _clipToast = e);
    // 进度事件由后续事件自然续命；终态 3.5s 后自己收起来。
    if (e.kind != ClipEventKind.fileProgress) {
      _clipToastTimer = Timer(const Duration(milliseconds: 3500), () {
        if (mounted) setState(() => _clipToast = null);
      });
    }
  }

  /// [20261022] InDesk 风格传输卡：深蓝黑底 + 左侧品牌色竖条 + 细进度线。
  /// 刻意不用 Material 的 SnackBar/Card —— 它们自带浅色 elevation，
  /// 压在远控画面上很突兀。
  Widget _clipToastCard(ClipEvent e) {
    const brand = Color(0xFF2F6FED);
    const ok = Color(0xFF3FC09A);
    const warn = Color(0xFFE0A33E);
    const bg = Color(0xF01A2233);
    late final IconData icon;
    var accent = brand;
    switch (e.kind) {
      case ClipEventKind.textSent:
        icon = Icons.arrow_upward_rounded;
        break;
      case ClipEventKind.textRecv:
        icon = Icons.arrow_downward_rounded;
        break;
      case ClipEventKind.fileStart:
      case ClipEventKind.fileProgress:
        icon = Icons.upload_file_rounded;
        break;
      case ClipEventKind.fileDone:
        icon = Icons.check_circle_outline_rounded;
        accent = ok;
        break;
      case ClipEventKind.fileRecv:
        icon = Icons.download_done_rounded;
        accent = ok;
        break;
      case ClipEventKind.warn:
        icon = Icons.info_outline_rounded;
        accent = warn;
        break;
    }
    final showBar = e.kind == ClipEventKind.fileProgress;
    return ClipRRect(
      borderRadius: BorderRadius.circular(12),
      child: Container(
        width: 246,
        decoration: BoxDecoration(
          color: bg,
          borderRadius: BorderRadius.circular(12),
          boxShadow: [
            BoxShadow(
                color: Colors.black.withOpacity(.30),
                blurRadius: 18,
                offset: const Offset(0, 8)),
          ],
        ),
        child: Stack(
          children: [
            Positioned(
                left: 0,
                top: 0,
                bottom: 0,
                width: 3,
                child: ColoredBox(color: accent)),
            Padding(
              padding: const EdgeInsets.fromLTRB(13, 10, 12, 11),
              child: Row(
                crossAxisAlignment: CrossAxisAlignment.start,
                children: [
                  Icon(icon, size: 15, color: accent),
                  const SizedBox(width: 8),
                  Expanded(
                    child: Column(
                      crossAxisAlignment: CrossAxisAlignment.start,
                      mainAxisSize: MainAxisSize.min,
                      children: [
                        Text(e.title,
                            maxLines: 1,
                            overflow: TextOverflow.ellipsis,
                            style: TextStyle(
                                fontSize: 12.5,
                                fontWeight: FontWeight.w600,
                                color: Colors.white.withOpacity(.95))),
                        if (e.detail.isNotEmpty) ...[
                          const SizedBox(height: 2),
                          Text(e.detail,
                              maxLines: 1,
                              overflow: TextOverflow.ellipsis,
                              style: TextStyle(
                                  fontSize: 11,
                                  color: Colors.white.withOpacity(.62))),
                        ],
                        if (showBar) ...[
                          const SizedBox(height: 7),
                          ClipRRect(
                            borderRadius: BorderRadius.circular(2),
                            child: SizedBox(
                              height: 3,
                              child: LinearProgressIndicator(
                                value: e.progress.clamp(0.0, 1.0),
                                backgroundColor: Colors.white.withOpacity(.12),
                                valueColor:
                                    const AlwaysStoppedAnimation<Color>(brand),
                              ),
                            ),
                          ),
                        ],
                      ],
                    ),
                  ),
                ],
              ),
            ),
          ],
        ),
      ),
    );
  }

  /// [20261022 黑边修复] 拿到真实视频尺寸后：先把窗口比例收对，再放行画面。
  /// 顺序很关键 —— 先放画面后收窗口，就是用户看到的"上下黑框 → 突然变完整"。
  Future<void> _settleVideoOnce() async {
    if (_settleStarted) return;
    final vw = _h264Up ? _h264W : _renderer.videoWidth;
    final vh = _h264Up ? _h264H : _renderer.videoHeight;
    if (vw <= 0 || vh <= 0) return;
    _settleStarted = true;
    _lastAlignedVideo = '${vw}x$vh';
    try {
      await alignSessionWindowToVideo(vw, vh,
          viewport: _listenerKey.currentContext?.size);
      // setSize 生效到 Flutter 重新布局要几帧；等 40ms 再放行，
      // 否则会"按旧尺寸渲染一帧 → 黑边闪一下"。
      await Future<void>.delayed(const Duration(milliseconds: 40));
    } catch (e) {
      ShellLog.w('[remote] 首帧对齐失败: $e');
    }
    if (!mounted) return;
    setState(() => _videoSettled = true);
    ShellLog.w('[remote] 画面放行（${vw}x$vh 比例已对齐）');
  }

  /// 临时诊断：确认 Windows 上 track 报告的视频宽高
  void _probeVideoSize() {
    Timer.periodic(const Duration(seconds: 3), (t) {
      if (!mounted || t.tick > 5) {
        t.cancel();
        return;
      }
      ShellLog.w('[remote] video=${_renderer.videoWidth}x'
          '${_renderer.videoHeight} ready=$_rendererReady');
      // [20261019] 自动适配已移除（用户反馈「连接上窗口停 2 秒然后变窄」：
      // 16:10 被控屏装进 16:9 预留尺寸里必然收窄，且 resize 过程整页重栅格化
      // 造成可感知卡顿）。会话窗口尺寸由 connect_page 的
      // ensureSessionWindowSize() 一次性设定，此后不再动。
    });
  }

  void _onStream(MediaStream stream) {
    if (!mounted) return;
    setState(() {
      _renderer.srcObject = stream;
      _rendererReady = true;
    });
  }

  @override
  void dispose() {
    _statTimer?.cancel();
    _settleGuard?.cancel();
    _clipToastTimer?.cancel();
    _clipSub?.cancel();
    windowManager.removeListener(this);
    _streamSub?.cancel();
    _h264StateSub?.cancel();
    _textCtrl.dispose();
    // [20261019 键盘重构] IME 输入框先行 unfocus + 释放
    try {
      _imeFocus.unfocus();
    } catch (_) {}
    _imeCtrl.removeListener(_onImeChanged);
    _imeCtrl.dispose();
    _imeFocus.dispose();
    // [20261017d] 同 NoIme 铁律：带焦点 dispose 前先 unfocus
    try {
      _focusNode.unfocus();
    } catch (_) {}
    _focusNode.dispose();
    _renderer.srcObject = null;
    _renderer.dispose();
    // [20261018] 断开 → 主界面恢复「默认打开尺寸」（用户明确要求：软件
    // 界面不要跟随远控窗口变大变小）。会话窗口一定被 ensureSessionWindowSize
    // 改大过，所以无条件还原。
    // [20261019g] 用户反馈"从大到小有个变化过程" → 先把窗口藏起来再
    // unmaximize/setSize/center，恢复完再显示。用户看到的就是直接切换，
    // 不再有还原动画。finally 兜底 show，任何一步失败窗口都不会丢。
    final def = adaptiveWindowSizes();
    Future(() async {
      try {
        await windowManager.hide();
        final m = await windowManager.isMaximized();
        if (m) await windowManager.unmaximize();
        await windowManager.setSize(Size(def.iw, def.ih));
        await windowManager.center();
      } catch (_) {
      } finally {
        try {
          await windowManager.show();
        } catch (_) {}
      }
    });
    setDragBand(300, 400); // 恢复主界面页拖动区
    // 会话 + 信令的统一销毁点：ConnectPage pushReplacement 后本页是唯一持有者
    widget.session.dispose();
    widget.session.signaling.dispose();
    super.dispose();
  }

  // [20261017d] 跟随窗口真实状态（用户从标题栏按钮/系统操作改变窗口也同步）
  @override
  void onWindowMaximize() {
    if (mounted) setState(() => _isFullscreen = true);
  }

  @override
  void onWindowUnmaximize() {
    if (mounted) setState(() => _isFullscreen = false);
  }

  // ---------------------------------------------------------- 坐标（0~1000）

  /// 对齐 web computeXY：以「实际绘制出的画面矩形」归一化。
  /// [20261019j contain] 画面改回 contain 完整显示（cover 裁掉了开始菜单，
  /// 用户死磕）→ 映射同步改 contain 语义：scale 取 min（完整放进视口），
  /// offset ≥ 0（黑边留白）。窗口已按视频比例贴合（含工具栏自标定），
  /// 正常无黑边；即便有，坐标映射也按画面真实矩形算。
  ({int x, int y}) _mapToRemote(Offset local, Size box) {
    final videoW = _h264Up ? _h264W.toDouble() : _renderer.videoWidth.toDouble();
    final videoH = _h264Up ? _h264H.toDouble() : _renderer.videoHeight.toDouble();
    double drawW = box.width, drawH = box.height;
    double offsetX = 0, offsetY = 0;
    if (videoW > 0 && videoH > 0) {
      final scale =
          math.min(box.width / videoW, box.height / videoH);
      drawW = videoW * scale;
      drawH = videoH * scale;
      offsetX = (box.width - drawW) / 2;
      offsetY = (box.height - drawH) / 2;
    }
    if (drawW <= 0 || drawH <= 0) return (x: _lastX, y: _lastY);
    final nx = (local.dx - offsetX).clamp(0.0, drawW) / drawW * 1000;
    final ny = (local.dy - offsetY).clamp(0.0, drawH) / drawH * 1000;
    return (x: nx.round().clamp(0, 1000), y: ny.round().clamp(0, 1000));
  }

  void _sendMove(({int x, int y}) r) {
    _lastX = r.x;
    _lastY = r.y;
    // [20261017d] 按住拖动期间也必须发**普通 mouseMove**（1:1 对齐 web 主控）：
    // 被控端 mouseDrag 是「按下→移动→抬起」一次性完整手势（nut-js mouse.drag
    // 语义），按住期间每帧发 mouseDrag = 连续的按下+抬起 → 每次都退化成单击，
    // 远程画面里任何窗口都拖不动。web 的做法：pressButtonLeft 一次、移动全程
    // mouseMove（被控系统按键保持按住）、releaseButtonLeft 收尾。
    widget.session.mouseMove(r.x, r.y);
  }

  /// 节流发移动（16ms 一条，永不依赖帧调度）
  void _scheduleMove() {
    final now = DateTime.now();
    if (now.difference(_lastMoveSent) < _moveThrottle) return;
    _lastMoveSent = now;
    final box = _listenerBox;
    if (box == null || _pendingMove == null) return;
    _sendMove(_mapToRemote(_pendingMove!, box));
  }

  Offset? _pendingMove;

  // ---------------------------------------------------------- 事件处理

  // [20261018] 右键状态跟踪：右键对齐 web 主控协议 —— contextmenu 场景
  // 发一次 rightClick(9)，不走 press/releaseButtonRight（被控该链路实测
  // 右键无反应）。PointerUpEvent.buttons 不可靠，用 down 事件记录。
  bool _pendingRight = false;

  void _onPointerDown(PointerDownEvent e) {
    // [20261019c] 焦点自愈：键盘焦点必须始终落在隐藏 IME 输入框上（旧注释
    // 说"焦点交给 Focus 的 autofocus"，但该 autofocus 已在键盘重构中移除，
    // 导致隐藏框从未获得焦点 → IME 打不开 → 打字一个字都发不出去，被控
    // 端日志键盘消息为 0 的实锤根因）。_imeFocus 与本页同生命周期，此处
    // requestFocus 不存在旧 NoIme disposing 节点的卡死问题。
    if (!_imeFocus.hasFocus) {
      _imeFocus.requestFocus();
      ShellLog.w('[ime] 点击画面 → 重新聚焦隐藏输入框');
    }
    final right = e.buttons == kSecondaryMouseButton;
    final box = _listenerBox;
    final r = box != null ? _mapToRemote(e.localPosition, box) : (x: _lastX, y: _lastY);
    _lastX = r.x;
    _lastY = r.y;
    _pendingRight = right;
    if (!right) {
      widget.session.mouseDown(right: false, x: r.x, y: r.y);
    }
  }

  void _onPointerUp(PointerUpEvent e) {
    if (_pendingRight) {
      _pendingRight = false;
      widget.session.rightClick(x: _lastX, y: _lastY);
    } else {
      widget.session.mouseUp(right: false, x: _lastX, y: _lastY);
    }
  }

  void _onPointerCancel(PointerCancelEvent e) {
    _pendingRight = false;
    widget.session.mouseUp(right: false, x: _lastX, y: _lastY);
  }

  void _onPointerHover(PointerHoverEvent e) {
    _pendingMove = e.localPosition;
    _scheduleMove();
  }

  void _onPointerMove(PointerMoveEvent e) {
    _pendingMove = e.localPosition;
    _scheduleMove();
  }

  void _onPointerSignal(PointerSignalEvent e) {
    if (e is PointerScrollEvent) {
      _wheelAccY += e.scrollDelta.dy;
      final ticks = _wheelAccY ~/ _wheelTickPx;
      _wheelAccY -= ticks * _wheelTickPx;
      if (ticks != 0) {
        widget.session.scroll(ticks: ticks, x: _lastX, y: _lastY);
      }
    }
  }

  /// [20261019] 控制键集合：这些键必须按 press/release nut 键码转发
  /// （与上游 NUT_KEY_MAP 语义一致）。**字母/数字/空格/标点不在其中**。
  bool _isControlKey(LogicalKeyboardKey k) {
    final set = _controlKeys;
    return set.contains(k);
  }

  static final Set<LogicalKeyboardKey> _controlKeys = <LogicalKeyboardKey>{
      LogicalKeyboardKey.escape, LogicalKeyboardKey.enter,
      LogicalKeyboardKey.numpadEnter, LogicalKeyboardKey.tab,
      LogicalKeyboardKey.backspace, LogicalKeyboardKey.delete,
      LogicalKeyboardKey.insert, LogicalKeyboardKey.home,
      LogicalKeyboardKey.end, LogicalKeyboardKey.pageUp,
      LogicalKeyboardKey.pageDown, LogicalKeyboardKey.arrowUp,
      LogicalKeyboardKey.arrowDown, LogicalKeyboardKey.arrowLeft,
      LogicalKeyboardKey.arrowRight, LogicalKeyboardKey.capsLock,
      LogicalKeyboardKey.numLock, LogicalKeyboardKey.scrollLock,
      LogicalKeyboardKey.pause, LogicalKeyboardKey.printScreen,
      LogicalKeyboardKey.contextMenu, LogicalKeyboardKey.controlLeft,
      LogicalKeyboardKey.controlRight, LogicalKeyboardKey.shiftLeft,
      LogicalKeyboardKey.shiftRight, LogicalKeyboardKey.altLeft,
      LogicalKeyboardKey.altRight, LogicalKeyboardKey.metaLeft,
      LogicalKeyboardKey.metaRight,
      LogicalKeyboardKey.f1, LogicalKeyboardKey.f2, LogicalKeyboardKey.f3,
      LogicalKeyboardKey.f4, LogicalKeyboardKey.f5, LogicalKeyboardKey.f6,
      LogicalKeyboardKey.f7, LogicalKeyboardKey.f8, LogicalKeyboardKey.f9,
      LogicalKeyboardKey.f10, LogicalKeyboardKey.f11, LogicalKeyboardKey.f12,
  };

  KeyEventResult _onKey(FocusNode node, KeyEvent event) {
    // 组词保护优先：输入法组词期间，候选操作键不出主控。
    final composing = _imeComposing;
    final was = _wasComposing;
    _wasComposing = composing;
    if ((composing || was) &&
        (event.logicalKey == LogicalKeyboardKey.enter ||
            event.logicalKey == LogicalKeyboardKey.numpadEnter ||
            event.logicalKey == LogicalKeyboardKey.space ||
            event.logicalKey == LogicalKeyboardKey.backspace ||
            event.logicalKey == LogicalKeyboardKey.escape ||
            event.logicalKey == LogicalKeyboardKey.arrowUp ||
            event.logicalKey == LogicalKeyboardKey.arrowDown ||
            event.logicalKey == LogicalKeyboardKey.arrowLeft ||
            event.logicalKey == LogicalKeyboardKey.arrowRight)) {
      return KeyEventResult.handled;
    }
    final nut = logicalToNut(event.logicalKey);
    if (nut == null) return KeyEventResult.ignored;
    final isDown = event is KeyDownEvent || event is KeyRepeatEvent;
    if (event is! KeyDownEvent &&
        event is! KeyRepeatEvent &&
        event is! KeyUpEvent) {
      return KeyEventResult.ignored;
    }
    // 控制键 → press/release 转发。
    // 修饰键组合（Ctrl+C/V/X、Alt+F4…）：字母键在 Ctrl/Alt/Win 按住时也必须
    // 走键码通路（否则 'c' 会以纯文本落到被控端，组合键失效）。
    final pressed = HardwareKeyboard.instance.logicalKeysPressed;
    final combo = pressed.contains(LogicalKeyboardKey.controlLeft) ||
        pressed.contains(LogicalKeyboardKey.controlRight) ||
        pressed.contains(LogicalKeyboardKey.altLeft) ||
        pressed.contains(LogicalKeyboardKey.altRight) ||
        pressed.contains(LogicalKeyboardKey.metaLeft) ||
        pressed.contains(LogicalKeyboardKey.metaRight);
    if (_isControlKey(event.logicalKey) || combo) {
      if (isDown) {
        widget.session.keyDown(nut);
      } else {
        widget.session.keyUp(nut);
      }
      return KeyEventResult.handled;
    }
    // 其余可打印键 → 交给隐藏 TextField（IME/差量通路）。
    // [20261019c] 打字瞬间若焦点不在隐藏框（窗口切换/点过工具栏等），
    // 重新聚焦并放弃当前按键（IME 需要 TextInput 通道先建立）。
    if (!_imeFocus.hasFocus) {
      ShellLog.w('[ime] 打字时焦点不在隐藏框 → 重新聚焦');
      _imeFocus.requestFocus();
      return KeyEventResult.handled;
    }
    return KeyEventResult.ignored;
  }

  /// [20261019] 隐藏输入框内容变化 → 差量注入（上游「软键盘差量转发」同款）：
  /// 变长 = keyboardType(新增文本整段上屏)；变短 = 逐字符 Backspace 键码。
  void _onImeChanged() {
    if (_imeBusy) return;
    // 组词进行中（拼音在框里）：吸收中间态，不发送 —— 提交后 _imePrev 仍是
    // 组词前基线（''），差量只会把「最终上屏文本」发出去。
    if (_imeComposing) return;
    final now = _imeCtrl.text;
    final old = _imePrev;
    if (now == old) return;
    _imeBusy = true;
    try {
      var p = 0;
      while (p < old.length && p < now.length && old[p] == now[p]) {
        p++;
      }
      var so = old.length, sn = now.length;
      while (so > p && sn > p && old[so - 1] == now[sn - 1]) {
        so--;
        sn--;
      }
      final removed = old.length - so;
      final added = now.substring(p, sn);
      if (removed > 0 && removed <= 64) {
        for (var i = 0; i < removed; i++) {
          widget.session.keyDown(NutKey.backspace);
          widget.session.keyUp(NutKey.backspace);
        }
      }
      if (added.isNotEmpty) {
        ShellLog.w('[ime] 发送文本 len=${added.length} '
            'focus=$_imeHadFocus');
        widget.session.typeText(added);
      }
      // 内容即时清空（下一轮差量从零开始）；清空触发本监听时靠 _imeBusy 拦住。
      _imePrev = '';
      _imeCtrl.value = const TextEditingValue();
    } finally {
      _imeBusy = false;
    }
  }

  // ---------------------------------------------------------- 工具栏动作

  void _closeMenus() {
    if (_menuOpen || _qualityOpen) {
      setState(() {
        _menuOpen = false;
        _qualityOpen = false;
      });
    }
  }

  void _runMenu(VoidCallback action) {
    _closeMenus();
    action();
  }

  void _togglePrivacy() {
    _privacy = !_privacy;
    widget.session.privacyScreen(_privacy);
    setState(() {});
  }

  /// 画质档位（对齐 web 端 QUALITY_TIERS：一次切档三连发）
  void _applyQuality(String preset) {
    setState(() {
      _tier = preset;
      ShellPrefs.applyTier(preset);
    });
    switch (preset) {
      case 'hd': // 高清：原生1080 / 30fps / 8Mbps
        widget.session.changeResolutionRatio(1080);
        widget.session.changeMaxFramerate(30);
        widget.session.changeMaxBitrate(8000);
      case 'balanced': // 均衡（对齐 web 档位名）：720P / 30fps / 4Mbps
        widget.session.changeResolutionRatio(720);
        widget.session.changeMaxFramerate(30);
        widget.session.changeMaxBitrate(4000);
      case 'smooth': // 流畅：540P / 30fps / 2Mbps
        widget.session.changeResolutionRatio(540);
        widget.session.changeMaxFramerate(30);
        widget.session.changeMaxBitrate(2000);
    }
  }

  Future<void> _sendText() async {
    final text = _textCtrl.text.trim();
    if (text.isEmpty) return;
    widget.session.typeText(text);
    _textCtrl.clear();
    if (mounted) Navigator.of(context).pop();
  }

  void _showTextChat() {
    showModalBottomSheet<void>(
      context: context,
      builder: (_) => Padding(
        padding: EdgeInsets.only(
          left: 16,
          right: 16,
          top: 16,
          bottom: 16 + MediaQuery.of(context).viewInsets.bottom,
        ),
        child: Row(
          children: [
            Expanded(
              child: TextField(
                controller: _textCtrl,
                autofocus: true,
                decoration:
                    const InputDecoration(hintText: '输入文字投送到被控端'),
                onSubmitted: (_) => _sendText(),
              ),
            ),
            const SizedBox(width: 8),
            FilledButton(onPressed: _sendText, child: const Text('发送')),
          ],
        ),
      ),
    );
  }

  // ---------------------------------------------------------- UI

  @override
  Widget build(BuildContext context) {
    final s = widget.session;
    return Scaffold(
      backgroundColor: Colors.black,
      body: Column(
        children: [
          _buildToolbar(),
          Expanded(
            child: Focus(
              focusNode: _focusNode,
              onKeyEvent: _onKey,
              child: MouseRegion(
                // [20261021 十三轮] 撤回光标隐身方案，指针回到"主控本机光标"。
                //
                // 十二轮曾把被控端系统光标换成全透明（SetSystemCursor），想让
                // 画面里只剩一个 0 延迟的本机指针 —— 但被控端那台机器上的真实
                // 鼠标也跟着没了，本机用户直接没法用。此路彻底废弃。
                //
                // 现状（有意为之）：主控显示本机光标 = 一移动就跟手、0 延迟；
                // 视频帧里 libwebrtc 合成的那颗被控端光标会晚一拍跟在后面。
                // 那颗是"被控端真实光标位置"的回显，不是 bug，也不再隐藏。
                cursor: SystemMouseCursors.basic,
                child: Listener(
                  key: _listenerKey,
                  onPointerDown: _onPointerDown,
                  onPointerUp: _onPointerUp,
                  onPointerCancel: _onPointerCancel,
                  onPointerMove: _onPointerMove,
                  onPointerHover: _onPointerHover,
                  onPointerSignal: _onPointerSignal,
                  child: Stack(
                    fit: StackFit.expand,
                    children: [
                      // [20261019 键盘重构] 隐藏 IME 输入框：中文/英文/标点
                      // 统一经它组合后差量整段上屏（1px + 全透明，不挡鼠标）。
                      Positioned(
                        left: 0,
                        top: 0,
                        child: SizedBox(
                          width: 1,
                          height: 1,
                          child: Opacity(
                            opacity: 0,
                            child: TextField(
                              controller: _imeCtrl,
                              focusNode: _imeFocus,
                              maxLines: 1,
                              autocorrect: false,
                              enableSuggestions: false,
                              keyboardType: TextInputType.text,
                              decoration: const InputDecoration(
                                isDense: true,
                                border: InputBorder.none,
                                enabledBorder: InputBorder.none,
                                focusedBorder: InputBorder.none,
                              ),
                              style: const TextStyle(fontSize: 1),
                            ),
                          ),
                        ),
                      ),
                      // [20261022 黑边修复] 三条硬规则：
                      //   ① 尺寸/比例没对齐之前，一帧画面都不放（只铺黑），
                      //      否则必然出现"上下黑框 → 突然变完整"的中间态；
                      //   ② H264 直送去掉 1680x1050 的假兜底 —— 那是个猜的值，
                      //      对端不是这个分辨率时黑边就是它造成的；
                      //   ③ 对齐完成后按真实尺寸 contain，永不裁剪。
                      if (_videoSettled &&
                          _h264Up &&
                          widget.session.h264.textureId != null &&
                          _h264W > 0 &&
                          _h264H > 0)
                        // [20261019e H264 直送] NVENC 直送解码纹理：
                        // 被控端原生编码 → DC 直发 → 原生解码 → 上屏，
                        // 全链无 JPEG/无重编码/无 WebRTC 媒体面（旧版同款管线）。
                        // [20261019j] contain 完整显示（用户死磕：必须整屏可见，
                        // 上下都不能裁）—— 窗口已按视频比例贴合（含工具栏高度
                        // 自标定），正常情况下无黑边；contain 保证永不裁剪。
                        ClipRect(
                          child: SizedBox.expand(
                            child: FittedBox(
                              fit: BoxFit.contain,
                              child: SizedBox(
                                width: _h264W.toDouble(),
                                height: _h264H.toDouble(),
                                child: Texture(
                                    textureId: widget.session.h264.textureId!),
                              ),
                            ),
                          ),
                        )
                      else if (_videoSettled && _rendererReady)
                        RTCVideoView(
                          _renderer,
                          objectFit:
                              RTCVideoViewObjectFit.RTCVideoViewObjectFitContain,
                        )
                      else if (_rendererReady)
                        // 画面已到但窗口比例还在收：纯黑，不给用户看黑框。
                        const ColoredBox(color: Color(0xFF000000))
                      else
                        const Center(
                          child: Column(
                            mainAxisSize: MainAxisSize.min,
                            children: [
                              CircularProgressIndicator(),
                              SizedBox(height: 12),
                              Text('等待远程画面…',
                                  style: TextStyle(color: Colors.white70)),
                            ],
                          ),
                        ),
                      // 下拉浮层遮罩（点画面先收菜单）
                      if (_menuOpen || _qualityOpen)
                        Positioned.fill(
                          child: GestureDetector(
                            behavior: HitTestBehavior.opaque,
                            onTap: _closeMenus,
                            child: const SizedBox.expand(),
                          ),
                        ),
                      if (_menuOpen)
                        Positioned(left: 8, top: 44, child: _menuPanel()),
                      if (_qualityOpen)
                        Positioned(left: 84, top: 44, child: _qualityPanel()),
                      // [20261022] 会话数据栏移到**右下角**（用户要求），
                      // 且底部留 68px 净空：Windows 任务栏/右下角托盘角标
                      // （时钟、输入法、显示桌面）正好在画面最底部一条，
                      // 面板与收起态 chip 都锚在 bottom:68 → 展开时向上长，
                      // 永远压不到角标；right:12 同理不贴边。
                      if (_dataOpen)
                        Positioned(right: 12, bottom: 68, child: _dataPanel())
                      else
                        Positioned(right: 12, bottom: 68, child: _dataChip()),
                      // [20261022] 剪贴板/文件传输气泡：叠在数据栏正上方，
                      // 同为右下角锚点，不干扰画面主体。
                      if (_clipToast != null)
                        Positioned(
                          right: 12,
                          bottom: 104,
                          child: _clipToastCard(_clipToast!),
                        ),
                      // 状态浮层（协商失败等）
                      StreamBuilder<String>(
                        stream: s.errorStream,
                        builder: (context, snap) {
                          if (!snap.hasData) return const SizedBox.shrink();
                          return Container(
                            color: Colors.red.withOpacity(.85),
                            padding: const EdgeInsets.all(12),
                            child: Text(snap.data!,
                                style: const TextStyle(color: Colors.white)),
                          );
                        },
                      ),
                    ],
                  ),
                ),
              ),
            ),
          ),
        ],
      ),
    );
  }

  // ===== [20261018] 工具栏 2.0 —— 无边框现代风格 =====
  // 用户反馈旧版描边矩形按钮「跟补丁一样贴上去的」。新风格：白底 +
  // 无边框圆角按钮（悬停/按下/激活三态底色），红色断开为实心圆角，
  // 细分隔线区隔功能组 —— 与 Windows 11 / web 原版工具栏观感一致。
  static const _tbBg = Colors.white;
  static const _tbBorder = Color(0xFFE5EAF2);
  static const _btnText = Color(0xFF3A4250);
  static const _brand = Color(0xFF2F6FED);
  static const _danger = Color(0xFFE5484D);
  static const _okGreen = Color(0xFF21B573);
  static const _warnAmber = Color(0xFFD97706);

  Widget _buildToolbar() {
    return Container(
      height: 42,
      color: _tbBg,
      padding: const EdgeInsets.symmetric(horizontal: 10),
      child: Row(children: [
        _tbBtn(Icons.menu_rounded, '菜单',
            active: _menuOpen, onTap: () => setState(() {
              _menuOpen = !_menuOpen;
              _qualityOpen = false;
            })),
        const SizedBox(width: 4),
        _tbBtn(Icons.tune_rounded, '画质', trailing: _tierName(_tier),
            active: _qualityOpen, onTap: () => setState(() {
              _qualityOpen = !_qualityOpen;
              _menuOpen = false;
            })),
        const SizedBox(width: 4),
        _tbBtn(_privacy ? Icons.visibility_rounded : Icons.visibility_off_rounded,
            '隐私屏', active: _privacy, onTap: _togglePrivacy),
        const Spacer(),
        _disconnectBtn(),
        const SizedBox(width: 8),
        Container(width: 1, height: 16, color: _tbBorder),
        const SizedBox(width: 4),
        _winBtn(Icons.remove_rounded, () => windowManager.minimize()),
        _winBtn(
            _isFullscreen
                ? Icons.filter_none_rounded
                : Icons.crop_square_rounded,
            _toggleFullscreen),
        _winBtn(Icons.close_rounded, _hideAndLeave, close: true),
      ]),
    );
  }

  /// [20261018] 无边框三态按钮：悬停浅灰、按下更深、激活品牌浅蓝
  Widget _tbBtn(IconData icon, String label,
      {bool active = false, VoidCallback? onTap, String? trailing}) {
    return _HoverBtn(
      active: active,
      onTap: onTap,
      child: Row(mainAxisSize: MainAxisSize.min, children: [
        Icon(icon, size: 15, color: active ? _brand : _btnText),
        const SizedBox(width: 6),
        Text(label,
            style: TextStyle(
                fontSize: 12.5,
                color: active ? _brand : _btnText,
                fontWeight: FontWeight.w500)),
        if (trailing != null) ...[
          const SizedBox(width: 5),
          Text(trailing,
              style: TextStyle(
                  fontSize: 11.5,
                  color: active ? _brand : const Color(0xFF7A8290))),
          Icon(Icons.keyboard_arrow_down_rounded,
              size: 13, color: active ? _brand : const Color(0xFF9AA3B2)),
        ],
      ]),
    );
  }

  /// [20261019h] 断开/关闭：先把窗口藏起来再退出会话页 —— 用户要求"直接
  /// 断开，不要大框变小框的过渡画面"。窗口保持隐藏直到 dispose 里的
  /// 恢复尺寸逻辑走完再 show（finally 兜底）。
  Future<void> _hideAndLeave() async {
    try {
      await windowManager.hide();
    } catch (_) {}
    if (mounted && Navigator.of(context).canPop()) {
      Navigator.of(context).pop();
    }
  }

  /// [20261018] 断开连接：淡红底色 tonal 按钮（与灰色工具栏自然融合，
  /// 又有警示语义——实心大红块在白工具栏上太突兀，用户反馈不好看）
  Widget _disconnectBtn() {
    return _HoverBtn(
      baseColor: const Color(0xFFFDECEC),
      hoverColor: const Color(0xFFF9D7D7),
      pressColor: const Color(0xFFF0C4C4),
      child: Row(mainAxisSize: MainAxisSize.min, children: [
        const Icon(Icons.link_off_rounded, size: 14, color: Color(0xFFD9484D)),
        const SizedBox(width: 5),
        const Text('断开连接',
            style: TextStyle(
                fontSize: 12.5,
                color: Color(0xFFD9484D),
                fontWeight: FontWeight.w600)),
      ]),
      onTap: _hideAndLeave,
    );
  }

  /// [20261018] 会话窗口右上角的系统三钮（悬停浅灰圆角）
  Widget _winBtn(IconData icon, VoidCallback onTap, {bool close = false}) {
    return _HoverBtn(
      radius: 6,
      baseColor: Colors.transparent,
      hoverColor: const Color(0xFFF1F4F8),
      pressColor: const Color(0xFFE2E8F0),
      padding: EdgeInsets.zero,
      onTap: onTap,
      child: SizedBox(
        width: 30,
        height: 28,
        child: Icon(icon,
            size: close ? 16 : 15,
            color: close ? const Color(0xFF3A4250) : const Color(0xFF5A6474)),
      ),
    );
  }

  /// [20261018] 数据栏收起态：右下角小 chip（[20261022] 由右上角移来，
  /// 面板向上展开，箭头朝上表示"点我展开"）
  Widget _dataChip() {
    return GestureDetector(
      onTap: _toggleDataBar,
      child: Container(
        padding: const EdgeInsets.symmetric(horizontal: 10, vertical: 5),
        decoration: BoxDecoration(
          color: Colors.black.withOpacity(.45),
          borderRadius: BorderRadius.circular(6),
        ),
        child: Row(mainAxisSize: MainAxisSize.min, children: [
          Text('数据',
              style: TextStyle(
                  fontSize: 11.5, color: Colors.white.withOpacity(.9))),
          const SizedBox(width: 4),
          Icon(Icons.keyboard_arrow_up,
              size: 15, color: Colors.white.withOpacity(.7)),
        ]),
      ),
    );
  }

  /// [20261018] 数据栏展开态：固定 200 宽半透明面板，行内文字超长省略
  Widget _dataPanel() {
    return Container(
      width: 200,
      padding: const EdgeInsets.fromLTRB(12, 8, 12, 10),
      decoration: BoxDecoration(
        color: Colors.black.withOpacity(.72),
        borderRadius: BorderRadius.circular(8),
      ),
      child: Column(crossAxisAlignment: CrossAxisAlignment.start,
          mainAxisSize: MainAxisSize.min, children: [
        GestureDetector(
          onTap: _toggleDataBar,
          behavior: HitTestBehavior.opaque,
          child: Row(children: [
            Text('会话数据',
                style: TextStyle(
                    fontSize: 11.5,
                    fontWeight: FontWeight.w700,
                    color: Colors.white.withOpacity(.9))),
            const Spacer(),
            Icon(Icons.keyboard_arrow_down,
                size: 15, color: Colors.white.withOpacity(.7)),
          ]),
        ),
        const SizedBox(height: 4),
        for (final e in _statRows.entries)
          Padding(
            padding: const EdgeInsets.symmetric(vertical: 1.5),
            child: Row(children: [
              SizedBox(
                  width: 46,
                  child: Text(e.key,
                      style: TextStyle(
                          fontSize: 11, color: Colors.white.withOpacity(.55)))),
              Expanded(
                child: Text(
                    e.value.isEmpty ? '—' : e.value,
                    maxLines: 1,
                    overflow: TextOverflow.ellipsis,
                    // [20261018] 对齐 web 数据栏：数值用等宽字体（Consolas，
                    // 与 web pc-db-v 同款），重要指标按健康度着色
                    style: TextStyle(
                        fontFamily: 'Consolas',
                        fontSize: 11.5,
                        color: _statColor(e.key, e.value),
                        fontWeight: FontWeight.w600)),
              ),
            ]),
          ),
      ]),
    );
  }

  /// [20261018] 指标健康度配色（对齐 web 数据栏语义）：
  /// 延迟 <60ms 绿 / <150ms 琥珀 / 更差红；帧率码率同理分档。
  Color _statColor(String key, String value) {
    int? numIn(String v) {
      final m = RegExp(r'(-?\d+(\.\d+)?)').firstMatch(v);
      return m == null ? null : double.tryParse(m.group(1)!)?.round();
    }

    switch (key) {
      case '连通性':
        if (value == '已连接') return _okGreen;
        if (value == '连接中…') return _warnAmber;
        if (value == '已断开') return _danger;
        return Colors.white;
      case '延迟':
        final n = numIn(value);
        if (n == null) return Colors.white;
        if (n < 60) return _okGreen;
        if (n < 150) return _warnAmber;
        return _danger;
      case '帧率':
        final n = numIn(value);
        if (n == null) return Colors.white;
        if (n >= 25) return _okGreen;
        if (n >= 15) return _warnAmber;
        return _danger;
      case '码率':
        final n = numIn(value);
        if (n == null) return Colors.white;
        if (n >= 4000) return _okGreen;
        if (n >= 1500) return _warnAmber;
        return _danger;
      case '丢帧':
        if (value.isEmpty || value == '正常') return _okGreen;
        return _danger;
      default:
        return Colors.white;
    }
  }

  String _tierName(String id) {
    switch (id) {
      case 'hd':
        return '高清';
      case 'balanced':
        return '均衡';
      case 'smooth':
        return '流畅';
      default:
        return '自定义';
    }
  }

  Widget _menuPanel() {
    return _PanelCard(
      // [20261017e] 168 太窄：文字换行/被裁（用户反馈「点开显示不全」）
      width: 200,
      children: [
        _menuItem(Icons.keyboard_rounded, 'Ctrl+Alt+Del',
            () => _runMenu(widget.session.sendCtrlAltDel)),
        _menuItem(Icons.monitor_heart_outlined, '任务管理器',
            () => _runMenu(widget.session.openTaskManager)),
        _menuItem(Icons.lock_outline_rounded, '锁定被控端',
            () => _runMenu(widget.session.lockScreen)),
        _menuItem(Icons.keyboard_alt_outlined, '松开系统键',
            () => _runMenu(widget.session.releaseAllModifiers)),
        _menuItem(Icons.chat_bubble_outline_rounded, '文字互动',
            () => _runMenu(_showTextChat)),
        _menuItem(Icons.fullscreen_rounded, _isFullscreen ? '退出全屏' : '开启全屏',
            () => _runMenu(_toggleFullscreen)),
        const Divider(height: 1, color: _tbBorder),
        _menuItem(Icons.link_off_rounded, '断开连接',
            () => _runMenu(() => Navigator.of(context).pop()),
            danger: true),
      ],
    );
  }

  Widget _qualityPanel() {
    return _PanelCard(
      // [20261017e] 150 装不下「高清 1080P · 30fps · 8M」一整行 → 换行
      // 后又被旧版固定高度裁掉；放宽到 230 保证单行完整显示。
      width: 230,
      children: [
        _qualityItem('hd', '高清 1080P · 30fps · 8M'),
        _qualityItem('balanced', '均衡 720P · 30fps · 4M'),
        _qualityItem('smooth', '流畅 540P · 30fps · 2M'),
        _qualityItem('custom', '自定义（主界面设置）'),
      ],
    );
  }

  Widget _qualityItem(String id, String label) {
    final on = _tier == id;
    return _PressableRect(
      onTap: () => _runMenu(() => _applyQuality(id)),
      color: on ? const Color(0xFFEAF1FE) : Colors.transparent,
      borderColor: Colors.transparent,
      radius: 0,
      child: Container(
        width: double.infinity,
        padding: const EdgeInsets.symmetric(horizontal: 12, vertical: 9),
        child: Row(children: [
          Expanded(
            child: Text(label,
                style: TextStyle(
                    fontSize: 12,
                    color: on ? const Color(0xFF2F6FED) : _btnText,
                    fontWeight: on ? FontWeight.w700 : FontWeight.w400)),
          ),
          if (on)
            const Icon(Icons.check_rounded, size: 14, color: Color(0xFF2F6FED)),
        ]),
      ),
    );
  }

  Future<void> _toggleFullscreen() async {
    // [20261017d] 全屏 = 最大化（唯一入口是这里的手动菜单项）
    if (_isFullscreen) {
      await windowManager.unmaximize();
    } else {
      await windowManager.maximize();
    }
  }

  Widget _menuItem(IconData icon, String label, VoidCallback onTap,
      {bool danger = false}) {
    return _PressableRect(
      onTap: onTap,
      color: Colors.transparent,
      borderColor: Colors.transparent,
      radius: 0,
      child: Container(
        width: double.infinity,
        padding: const EdgeInsets.symmetric(horizontal: 12, vertical: 9),
        child: Row(children: [
          Icon(icon,
              size: 14, color: danger ? _danger : const Color(0xFF5A6474)),
          const SizedBox(width: 8),
          // [20261017e] Expanded 兜底：窗口再窄文字也只换行不溢出
          Expanded(
            child: Text(label,
                style: TextStyle(
                    fontSize: 12.5,
                    color: danger ? _danger : _btnText,
                    fontWeight: FontWeight.w500)),
          ),
        ]),
      ),
    );
  }
}

// ===== 工具栏小组件 =====

class _PressableRect extends StatefulWidget {
  const _PressableRect({
    required this.child,
    this.onTap,
    this.color = Colors.white,
    this.borderColor = const Color(0xFFDCDFE6),
    this.radius = 6,
  });

  final Widget child;
  final VoidCallback? onTap;
  final Color color;
  final Color borderColor;
  final double radius;

  @override
  State<_PressableRect> createState() => _PressableRectState();
}

class _PressableRectState extends State<_PressableRect> {
  bool _pressed = false;

  @override
  Widget build(BuildContext context) {
    return GestureDetector(
      onTapDown: (_) => setState(() => _pressed = true),
      onTapUp: (_) => setState(() => _pressed = false),
      onTapCancel: () => setState(() => _pressed = false),
      onTap: widget.onTap,
      child: Container(
        // [20261017e] 固定 height:30 会把换行后的两行文字直接裁掉（画质
        // 项在窄面板下「显示不全文字」的根源）——改最小高度+左中对齐。
        constraints: const BoxConstraints(minHeight: 30),
        alignment: Alignment.centerLeft,
        padding: const EdgeInsets.symmetric(horizontal: 10),
        decoration: BoxDecoration(
          color: _pressed
              ? Color.lerp(widget.color, Colors.black, 0.08)
              : widget.color,
          border: Border.all(color: widget.borderColor),
          borderRadius: BorderRadius.circular(widget.radius),
        ),
        child: widget.child,
      ),
    );
  }
}

class _PanelCard extends StatelessWidget {
  const _PanelCard({required this.width, required this.children});

  final double width;
  final List<Widget> children;

  @override
  Widget build(BuildContext context) {
    return Material(
      elevation: 6,
      borderRadius: BorderRadius.circular(8),
      child: Container(
        width: width,
        decoration: BoxDecoration(
          color: Colors.white,
          borderRadius: BorderRadius.circular(8),
          border: Border.all(color: const Color(0xFFDFE3E8)),
        ),
        child: Column(
          crossAxisAlignment: CrossAxisAlignment.start,
          mainAxisSize: MainAxisSize.min,
          children: children,
        ),
      ),
    );
  }
}

/// [20261018] 无边框三态按钮（悬停/按下/激活）：远控工具栏 2.0 的基础件。
class _HoverBtn extends StatefulWidget {
  const _HoverBtn({
    required this.child,
    required this.onTap,
    this.active = false,
    this.radius = 7,
    this.baseColor = Colors.transparent,
    this.activeColor = const Color(0xFFE8F0FE),
    this.hoverColor = const Color(0xFFF1F4F8),
    this.pressColor = const Color(0xFFE2E8F0),
    this.padding = const EdgeInsets.symmetric(horizontal: 10),
  });

  final Widget child;
  final VoidCallback? onTap;
  final bool active;
  final double radius;
  final Color baseColor;
  final Color activeColor;
  final Color hoverColor;
  final Color pressColor;
  final EdgeInsetsGeometry padding;

  @override
  State<_HoverBtn> createState() => _HoverBtnState();
}

class _HoverBtnState extends State<_HoverBtn> {
  bool _hover = false;
  bool _pressed = false;

  @override
  Widget build(BuildContext context) {
    final Color bg;
    if (widget.active) {
      bg = _pressed ? widget.pressColor : widget.activeColor;
    } else if (widget.onTap != null && _pressed) {
      bg = widget.pressColor;
    } else if (widget.onTap != null && _hover) {
      bg = widget.hoverColor;
    } else {
      bg = widget.baseColor;
    }
    return MouseRegion(
      cursor: SystemMouseCursors.click,
      onEnter: (_) => setState(() => _hover = true),
      onExit: (_) {
        setState(() {
          _hover = false;
          _pressed = false;
        });
      },
      child: GestureDetector(
        onTapDown: widget.onTap == null
            ? null
            : (_) => setState(() => _pressed = true),
        onTapUp: widget.onTap == null
            ? null
            : (_) => setState(() => _pressed = false),
        onTapCancel: widget.onTap == null
            ? null
            : () => setState(() => _pressed = false),
        onTap: widget.onTap,
        behavior: HitTestBehavior.opaque,
        child: Container(
          constraints: const BoxConstraints(minHeight: 30),
          alignment: Alignment.center,
          padding: widget.padding,
          decoration: BoxDecoration(
            color: bg,
            borderRadius: BorderRadius.circular(widget.radius),
          ),
          child: widget.child,
        ),
      ),
    );
  }
}
