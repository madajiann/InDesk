/// 被控会话状态机 —— [20261019 被控去 WebView2] 把 Tauri webview 里的被控
/// 前端逻辑（views/remote/index.vue 5000+ 行中被远控用到的部分）镜像进 Dart：
///   信令：deskJoin(10s 自愈)/deskUpdateUser(2s)/deskStartRemoteResult/
///         deskCloseRemote/deskBehavior(WS 兜底)/nativeWebRtcAnswer|Candidate
///   WebRTC：被控是 **offer 方**（web webRTC.ts 同构）：自建 4 条 DataChannel
///         （Message/Mouse/Video/File）+ ondatachannel 双绑收主控消息
///   输入：deskBehavior → _rid 去重 → 物理坐标换算 → 被控核 ipc_dispatch
///   H264：h264Ready → captureStart(NVENC) → 帧轮询 → VideoChannel 直发；
///         档位/容量学习语义对齐 2.3.24~2.3.26（会话中禁自动回升重建）
import 'dart:async';
import 'dart:convert';
import 'dart:io';
import 'dart:typed_data';

import 'package:flutter/foundation.dart';
import 'package:flutter_webrtc/flutter_webrtc.dart';

import '../config/turn_config.dart';
import '../core/clip_sync.dart';
import '../core/prefs.dart';
import '../core/shell_log.dart';
import '../core/ws_bridge.dart';
import '../signal/identity.dart' show ControllerIdentity;
import '../signal/protocol.dart';
import '../signal/signaling_client.dart';
import 'h264_sender.dart';
import 'host_core.dart';

enum ReceiverState { offline, online, incoming, controlled }

class ReceiverIncomingRequest {
  ReceiverIncomingRequest(this.raw);
  final Map<String, dynamic> raw;
  String get sender => raw['sender']?.toString() ?? '';
  String get masterDeviceCode => raw['deskUserUuid']?.toString() ?? '';
}

/// 档位/容量学习持久化（对齐 web indesk.h264tier / indesk.h264capkbps）
class _H264Learn {
  static File? _file() {
    final appdata = Platform.environment['APPDATA'];
    if (appdata == null || appdata.isEmpty) return null;
    return File('$appdata\\InDesk\\receiver_h264.json');
  }

  static Map<String, dynamic> _load() {
    try {
      final f = _file();
      if (f != null && f.existsSync()) {
        return Map<String, dynamic>.from(jsonDecode(f.readAsStringSync()) as Map);
      }
    } catch (_) {}
    return {};
  }

  static int tier = 0;
  static int capKbps = 0;
  static bool _migrated = false;

  static void init() {
    if (_migrated) return;
    _migrated = true;
    final m = _load();
    // [20261019r 迁移] 一次性清掉 2.3.23 时代可能学到的假容量
    if (m['caplearnV2'] != 1) {
      tier = 0;
      capKbps = 0;
      save();
      return;
    }
    tier = (m['tier'] as num? ?? 0).toInt().clamp(0, 4);
    capKbps = (m['capKbps'] as num? ?? 0).toInt();
  }

  static void save() {
    try {
      _file()?.writeAsStringSync(jsonEncode({
        'tier': tier,
        'capKbps': capKbps,
        'caplearnV2': 1,
      }));
    } catch (_) {}
  }
}

/// [20261021] 档位从 60fps 起降为 30fps 起：服务端下发的 maxFramerate 本就是
/// 30，旧表头写 60 等于让核按 2 倍负载满速抓屏编码（真机单帧抓+编 29ms，
/// 循环 100% 占满一个核），弱机直接饱和并把整条会话拖死。
// [20261021] 基准从 30fps 降到 24fps：本机 NVENC 被驱动钉在 P8 135MHz，
// 1280x800 单帧 12~16ms，30fps 要吃满 45% 编码器时间，任何一次外部抢占都会
// 直接掉帧。24fps 留出余量，肉眼差别很小，但掉帧/冻结明显减少。
const List<int> h264FpsTiers = [30, 24, 20, 15, 10];
/// [20261021 16:0x] 系数回到 1.0 起：上一版从 0.5 起是「卡顿」误判期的妥协，
/// 真因是进程优先级（已修）。0.5 意味着主控要 8000 只给 4000 —— 真机直接
/// 表现为「画面非常模糊」。0.85/0.65/… 保留为自适应降档。
const List<double> h264KbpsFactor = [1.0, 0.85, 0.65, 0.45, 0.3];

class ReceiverSession {
  ReceiverSession({
    required this.signal,
    required this.bridge,
    required this.onStateChanged,
    required this.onIncomingRequest,
  });

  final SignalingClient signal;
  final WsBridge bridge;
  final void Function(ReceiverState s) onStateChanged;
  final void Function(ReceiverIncomingRequest req) onIncomingRequest;

  ReceiverState _state = ReceiverState.offline;
  ReceiverState get state => _state;

  ControllerIdentity? _identity; // host_identity
  String get mySocketId => signal.socketId ?? '';
  String? _peerSocketId;

  // 画质（主控握手参数直通，默认高清档；对齐 web currentMax*）
  int currentMaxBitrate = 8000;
  int currentMaxFramerate = 30;
  int currentResolutionRatio = 1080;

  RTCPeerConnection? _pc;
  RTCDataChannel? _dcCtrlSelf; // 自建（发送用）
  RTCDataChannel? _dcMouseSelf;
  RTCDataChannel? _dcVideoSelf;
  /// [20261021] VideoChannel 失效日志去重（每轮失效只打一条，防刷爆日志）
  bool _dcVideoBadLogged = false;
  RTCDataChannel? _dcFileSelf;
  final List<RTCDataChannel> _dcRecv = []; // ondatachannel 到达的主控通道

  // [20261022 剪贴板同步] 被控端同样双向：把本机复制的东西推给主控，
  // 也接住主控推过来的文本/文件。与 controller_session 的协议完全对称。
  ClipSync? _clip;

  // [20261021 九轮] **标准 WebRTC 视频轨**（libwebrtc 自采自编）：替代我们自己
  // 写的「DataChannel 分片 + 自建解码」。理由见 _attachScreenTrack 注释。
  MediaStream? _screenStream;
  MediaStreamTrack? _screenTrack;
  RTCRtpSender? _screenSender;
  bool _screenTrackUp = false;

  H264Sender? _h264;
  Timer? _joinTimer;
  Timer? _updateTimer;
  Timer? _rebindTimer;
  DateTime _h264SuppressedUntil = DateTime.fromMillisecondsSinceEpoch(0);
  bool _disposed = false;

  // _rid 去重（FIFO 上限 1200，对齐 web [v48]）
  final Set<String> _seenRid = {};

  ReceiverState _setState(ReceiverState s) {
    if (_disposed) return _state;
    if (_state == s) return _state;
    _state = s;
    onStateChanged(s);
    return s;
  }

  Future<void> start(ControllerIdentity identity) async {
    _identity = identity;
    _H264Learn.init();
    signal.subscribe(WsMsg.deskStartRemoteResult);
    signal.subscribe(WsMsg.deskCloseRemote);
    signal.subscribe(WsMsg.deskBehavior);
    signal.subscribe(WsMsg.nativeWebRtcAnswer);
    signal.subscribe(WsMsg.nativeWebRtcCandidate);
    signal.subscribe(WsMsg.changeMaxBitrate);
    signal.subscribe(WsMsg.changeMaxFramerate);
    signal.subscribe(WsMsg.changeResolutionRatio);
    signal.onMessage.listen(_onSignal);
    signal.onConnected.listen((_) {
      _sendJoin();
      _setState(ReceiverState.online);
    });
    signal.onDisconnected.listen((_) {
      // 信令断开：会话可能还活着（P2P），但房间身份已丢 → 拆干净重连
      _teardownPc('信令断开');
      if (!_disposed) _setState(ReceiverState.offline);
    });
    signal.connect();
    signal.startHeartbeat();
    // 房间身份自愈（web v49）：每 10s 幂等补发 deskJoin
    _joinTimer?.cancel();
    _joinTimer = Timer.periodic(const Duration(seconds: 10), (_) => _sendJoin());
    // 在线心跳（web：2s deskUpdateUser 续在线键）
    _updateTimer?.cancel();
    _updateTimer =
        Timer.periodic(const Duration(seconds: 2), (_) => _sendUpdateUser());
    ShellLog.w('[receiver] 会话已启动 uuid=${identity.uuid}');
  }

  Future<void> dispose() async {
    _disposed = true;
    _joinTimer?.cancel();
    _updateTimer?.cancel();
    _rebindTimer?.cancel();
    await _teardownPc('dispose');
    // [20261022] 剪贴板同步彻底释放（停止轮询 + 关掉未写完的接收文件句柄）
    await _clip?.dispose();
    _clip = null;
  }

  // ------------------------------------------------------- 信令上行

  void _sendJoin() {
    final id = _identity;
    if (id == null || !signal.connected) return;
    signal.send(WsMsg.deskJoin, <String, dynamic>{
      'deskUserUuid': id.uuid,
      'deskUserPassword': id.password,
      'live_room_id': id.uuid,
    });
  }

  void _sendUpdateUser() {
    final id = _identity;
    if (id == null || !signal.connected) return;
    signal.send(WsMsg.deskUpdateUser, <String, dynamic>{
      'roomId': id.uuid,
      'sender': mySocketId,
      'receiver': '',
      'maxBitrate': currentMaxBitrate,
      'maxFramerate': currentMaxFramerate,
      'resolutionRatio': currentResolutionRatio,
      'videoContentHint': 'motion',
      'audioContentHint': '',
      'deskUserUuid': id.uuid,
      'deskUserPassword': id.password,
      'remoteDeskUserUuid': '',
      'remoteDeskUserPassword': '',
    });
  }

  // ------------------------------------------------------- 信令下行

  void _onSignal(SignalMessage m) {
    if (_disposed) return;
    final d = m.data;
    // [诊断] 信令留痕（排障期；高频类除外：输入兜底/心跳）
    if (m.type != WsMsg.deskBehavior && m.type != WsMsg.deskJoined) {
      final preview = jsonEncode(d);
      ShellLog.w(
          '[receiver][sig] ← ${m.type} ${preview.length > 200 ? '${preview.substring(0, 200)}…' : preview}');
    }
    switch (m.type) {
      case WsMsg.deskStartRemoteResult:
        _onStartRemoteResult(d);
        break;
      case WsMsg.deskCloseRemote:
        final who = d['sender']?.toString() ?? '';
        if (who.isEmpty || who == mySocketId) return;
        ShellLog.w('[receiver] 主控已断开，释放会话 $who');
        _teardownPc('主控断开');
        break;
      case WsMsg.deskBehavior:
        // WS 兜底 B 链路（服务端 socket.to(room) 广播，天然发给本机）
        dispatchDeskBehavior(d, 'ws');
        break;
      case WsMsg.nativeWebRtcAnswer:
        _onRemoteAnswer(d);
        break;
      case WsMsg.nativeWebRtcCandidate:
        _onRemoteCandidate(d);
        break;
      case WsMsg.changeMaxBitrate:
        _onQualityChange('bitrate', d);
        break;
      case WsMsg.changeMaxFramerate:
        _onQualityChange('framerate', d);
        break;
      case WsMsg.changeResolutionRatio:
        _onQualityChange('resolution', d);
        break;
    }
  }

  void _onStartRemoteResult(Map<String, dynamic> d0) {
    // 兼容两种形状：{code,data} 直给 / 信封 {request_id,...,data:{code,data}}
    var d = d0;
    if (d['code'] == null && d['data'] is Map) {
      d = Map<String, dynamic>.from(d['data'] as Map);
    }
    if (d['code'] != 0) {
      ShellLog.w('[receiver] deskStartRemoteResult 失败: ${d['msg']}');
      return;
    }
    final data = d['data'];
    if (data is! Map) return;
    final receiver = data['receiver']?.toString() ?? '';
    ShellLog.w(
        '[receiver] 连入请求：receiver=$receiver mySocketId=$mySocketId');
    if (receiver != mySocketId) return; // 不是发给本机的
    _handleIncomingRequest(ReceiverIncomingRequest(
      Map<String, dynamic>.from(data),
    ));
  }

  void _handleIncomingRequest(ReceiverIncomingRequest req) {
    _setState(ReceiverState.incoming);
    if (ShellPrefs.unattended) {
      // 无人值守：直接放行（对齐 web acceptIncoming）
      acceptIncoming(req);
      return;
    }
    onIncomingRequest(req); // UI 弹确认框
  }

  Future<void> acceptIncoming(ReceiverIncomingRequest req) async {
    // 主控握手参数直通（对齐 web updateWebRtcRemoteDeskConfig）
    final mb = (req.raw['maxBitrate'] as num?)?.toInt() ?? 0;
    final mf = (req.raw['maxFramerate'] as num?)?.toInt() ?? 0;
    // [20261021 16:0x] resolutionRatio 此前漏读（默认 100 = 目标高度 100px），
    // 只能靠 h264_sender 里硬编码 maxWidth 兜住 → 主控要 1080 高清档却拿到
    // 1280x800，真机表现为「画面非常模糊」。这里补上直通。
    final rr = (req.raw['resolutionRatio'] as num?)?.toInt() ?? 0;
    if (mb > 0) currentMaxBitrate = mb;
    if (mf > 0) currentMaxFramerate = mf;
    if (rr > 100) currentResolutionRatio = rr;
    await _handleRTC(req.sender);
    if (ShellPrefs.privacyAutoOn) {
      unawaited(hostInvoke(bridge, 'privacyScreen', {'on': true}));
    }
    _setState(ReceiverState.controlled);
  }

  Future<void> rejectIncoming(ReceiverIncomingRequest req) async {
    signal.send(WsMsg.deskCloseRemote, <String, dynamic>{
      'roomId': _identity?.uuid ?? '',
      'sender': mySocketId,
      'receiver': req.sender,
    });
    if (_state == ReceiverState.incoming) _setState(ReceiverState.online);
  }

  // ------------------------------------------------------- WebRTC（offer 侧）

  Future<void> _handleRTC(String masterSocketId) async {
    try {
      await _handleRtcInner(masterSocketId);
    } catch (e, st) {
      ShellLog.w('[receiver] handleRTC 异常: $e\n$st');
    }
  }

  bool _linking = false; // 建链进行中（对齐 web linkingSet，防并发建链互踩）

  Future<void> _handleRtcInner(String masterSocketId) async {
    if (_identity == null || !signal.connected) {
      ShellLog.w(
          '[receiver] handleRTC 前置失败 identity=${_identity != null} signalConnected=${signal.connected}');
      return;
    }
    if (_linking) {
      // 对齐 web :3926 linkingSet.has(receiver) → return：建链尚未完成时
      // 新请求一律不进（重复信令/风暴场景），避免两条链路互踩。
      ShellLog.w('[receiver] 建链进行中，跳过本次请求（$masterSocketId）');
      return;
    }
    _linking = true;
    try {
      await _handleRtcLink(masterSocketId);
    } finally {
      _linking = false;
    }
  }

  Future<void> _handleRtcLink(String masterSocketId) async {
    // 幂等闸门（对齐 web 20260913/0914 修 + [20261020 重连拆旧建新]）：
    // web 的 rtcMap 按主控 socketId 分 key —— 同一主控 connected 才跳过，
    // 不同主控的请求天然不冲突。Dart 单 PC 结构必须补上这层判断：
    // 新请求的 sender != 当前 peer → 拆旧建新（新请求证明旧主控已不在；
    // 旧 PC 可能是 Failed 前的僵尸态，实测 13s 才被状态回调拆除，期间
    // 旧闸门把重连全拒掉 = 用户「断开再连接连不上」的直接根因）。
    if (_pc != null) {
      final cs = await _pc!.getConnectionState();
      final connected =
          cs == RTCPeerConnectionState.RTCPeerConnectionStateConnected;
      if (connected && _peerSocketId == masterSocketId) {
        ShellLog.w('[receiver] 同一主控已有会话，跳过（$masterSocketId）');
        return;
      }
      ShellLog.w(
          '[receiver] 拆除旧 PC（peer=$_peerSocketId state=$cs）为新主控 $masterSocketId 重建');
      await _teardownPc('重建');
    }
    _peerSocketId = masterSocketId;

    final config = <String, dynamic>{
      'iceServers': <Map<String, dynamic>>[
        {
          'urls': <String>[TurnConfig.url, TurnConfig.urlTcp],
          'username': TurnConfig.username,
          'credential': TurnConfig.credential,
        },
      ],
      'sdpSemantics': 'unified-plan',
    };
    final p = await createPeerConnection(config);

    p.onIceCandidate = (RTCIceCandidate c) {
      signal.send(WsMsg.nativeWebRtcCandidate, <String, dynamic>{
        'live_room_id': _identity!.uuid,
        'candidate': <String, dynamic>{
          'candidate': c.candidate,
          'sdpMid': c.sdpMid,
          'sdpMLineIndex': c.sdpMLineIndex,
        },
        'receiver': _peerSocketId,
        'sender': mySocketId,
      });
    };
    p.onConnectionState = (RTCPeerConnectionState s) {
      ShellLog.w('[receiver] PC 状态: $s');
      if (s == RTCPeerConnectionState.RTCPeerConnectionStateConnected) {
        // [20261022 剪贴板同步] 通道通了才开始轮询。
        _ensureClip()?.start();
      }
      if (s == RTCPeerConnectionState.RTCPeerConnectionStateFailed ||
          s == RTCPeerConnectionState.RTCPeerConnectionStateClosed) {
        if (_peerSocketId == masterSocketId) _teardownPc('PC $s');
      }
    };
    p.onDataChannel = (RTCDataChannel dc) {
      ShellLog.w('[receiver] 收到主控数据通道 ${dc.label}');
      _bindRecvChannel(dc);
    };

    // 自建 4 条 DataChannel（发送侧 + 乒乓应答；拓扑与 web webRTC.ts 一致）
    _dcCtrlSelf = await p.createDataChannel('MessageChannel', RTCDataChannelInit());
    _dcMouseSelf = await p.createDataChannel(
        'MouseChannel',
        RTCDataChannelInit()
          ..maxRetransmits = 0
          ..ordered = false);
    // [20261021 16:0x] VideoChannel 改**完全可靠**：原来 maxRetransmits=2
    // 意味着每个分片最多重传 2 次就被丢弃。8Mbps 下 IDR 一帧几十个分片，
    // 真机丢一个分片 → 整帧拼不齐 → 主控判参考链断 → 之后所有 P 帧都丢，
    // 只等下一个 IDR（idr_period 10s）→ 症状正是「画面几乎不动、点完没反应」，
    // 现场日志佐证：被控端 5 秒内被催 3~4 次 IDR。
    // 可靠 + 有序会让 WebRTC 自己按拥塞窗口降速（宁可慢一点，不要断链）。
    _dcVideoSelf = await p.createDataChannel(
        'VideoChannel', RTCDataChannelInit()..ordered = true);
    _dcFileSelf = await p.createDataChannel('FileChannel', RTCDataChannelInit());
    for (final dc in [_dcCtrlSelf!, _dcMouseSelf!, _dcVideoSelf!, _dcFileSelf!]) {
      _bindSelfChannel(dc);
    }

    // [20261021 九轮] 视频走**标准轨道**（libwebrtc 采集+编码，自带 NACK/GCC/PLI）。
    // 必须在 createOffer 之前 addTrack，SDP 里才有 video m-line。
    // 挂载失败（无屏/权限）才回退老的 DataChannel 直送 —— 保证不黑屏。
    final trackOk = await _attachScreenTrack(p);

    final offer = await p.createOffer(<String, dynamic>{});
    await p.setLocalDescription(offer);
    signal.send(WsMsg.nativeWebRtcOffer, <String, dynamic>{
      'isRemoteDesk': true,
      'live_room': <String, dynamic>{'id': _identity!.uuid},
      'live_room_id': _identity!.uuid,
      'sender': mySocketId,
      'receiver': masterSocketId,
      'sdp': <String, dynamic>{'type': offer.type, 'sdp': offer.sdp},
      // 主控连接窗显示对面构建号（与 kShellVersion 同步）
      'webBuild': 'flutter2.3.49',
    });
    _pc = p;
    ShellLog.w('[receiver] offer 已发 → $masterSocketId'
        '（视频=${trackOk ? 'track' : 'datachannel 回退'}）');

    // 兜底重绑（web bindAllChannels 2.5s 补绑同款：协商完成后的迟到通道）
    _rebindTimer?.cancel();
    _rebindTimer = Timer(const Duration(milliseconds: 2500), () {
      for (final dc in List<RTCDataChannel>.of(_dcRecv)) {
        _attachMessageHandler(dc);
      }
    });
  }

  Future<void> _onRemoteAnswer(Map<String, dynamic> d) async {
    if (d['receiver'] != mySocketId) return;
    final sdp = d['sdp'];
    if (sdp is! Map || _pc == null) return;
    if (_peerSocketId != null && d['sender'] != _peerSocketId) return;
    try {
      await _pc!.setRemoteDescription(
        RTCSessionDescription(sdp['sdp'] as String, sdp['type'] as String),
      );
      ShellLog.w('[receiver] answer 已应用');
    } catch (e) {
      ShellLog.w('[receiver] setRemoteDescription 失败: $e');
    }
  }

  Future<void> _onRemoteCandidate(Map<String, dynamic> d) async {
    if (d['receiver'] != mySocketId) return;
    final c = d['candidate'];
    if (c is! Map || _pc == null) return;
    try {
      await _pc!.addCandidate(RTCIceCandidate(
        c['candidate'] as String?,
        c['sdpMid'] as String?,
        (c['sdpMLineIndex'] as num?)?.toInt(),
      ));
    } catch (e) {
      ShellLog.w('[receiver] addCandidate 失败: $e');
    }
  }

  // ------------------------------------------------------- DC 消息绑定

  /// 自建通道：发送 + 乒乓应答（master 的 pong 回到这条）
  void _bindSelfChannel(RTCDataChannel dc) {
    dc.onMessage = (msg) {
      if (_disposed) return;
      if (msg.isBinary) {
        // [20261022] FileChannel 二进制 = 主控回发的文件块。收发不假设方向，
        // 统一交给 ClipSync 解（它自己按 magic 校验，别的流量会被忽略）。
        if (dc.label == 'FileChannel') {
          final b = msg.binary;
          if (b is Uint8List && b.isNotEmpty) {
            _ensureClip()?.handleFileBinary(b);
          }
        }
        return;
      }
      _onCtrlText(msg.text, dc);
    };
  }

  // ------------------------------------------- 剪贴板同步（文本 + 文件，双向）

  /// 与 controller_session 完全对称的协议，改一边必须改另一边。
  ClipSync? _ensureClip() {
    if (_disposed) return null;
    return _clip ??= ClipSync(
      sendCtrl: (json) => _dcSendRaw(_dcCtrlSelf, json),
      sendFileCtrl: (json) => _dcSendRaw(_dcFileSelf, json),
      sendFileBin: (bin) => _dcSendBin(_dcFileSelf, bin),
    );
  }

  /// 发已经序列化好的 JSON 字符串（ClipSync 自己拼的协议帧）。
  /// 注意别和下面那个收 Map 的 `_dcSendText` 混了：那个是老代码用的。
  bool _dcSendRaw(RTCDataChannel? dc, String json) {
    if (dc == null || dc.state != RTCDataChannelState.RTCDataChannelOpen) {
      return false;
    }
    try {
      dc.send(RTCDataChannelMessage(json));
      return true;
    } catch (_) {
      return false;
    }
  }

  bool _dcSendBin(RTCDataChannel? dc, Uint8List bin) {
    if (dc == null || dc.state != RTCDataChannelState.RTCDataChannelOpen) {
      return false;
    }
    try {
      dc.send(RTCDataChannelMessage.fromBinary(bin));
      return true;
    } catch (_) {
      return false;
    }
  }

  /// ondatachannel 到达的主控通道：全部消息在这里收（输入/h264 握手/时钟）
  void _bindRecvChannel(RTCDataChannel dc) {
    if (!_dcRecv.any((e) => identical(e, dc))) _dcRecv.add(dc);
    _attachMessageHandler(dc);
  }

  void _attachMessageHandler(RTCDataChannel dc) {
    dc.onMessage = (msg) {
      if (_disposed) return;
      if (msg.isBinary) {
        // [20261022] 主控自建的 FileChannel 到达这里时，文件数据块从这收。
        if (dc.label == 'FileChannel') {
          final b = msg.binary;
          if (b is Uint8List && b.isNotEmpty) {
            _ensureClip()?.handleFileBinary(b);
          }
        }
        return; // 其余二进制（主控不发）忽略
      }
      _onCtrlText(msg.text, dc);
    };
  }

  /// 控制文本统一分发（对齐 web bindChannelHandlers：按 msgType 路由）
  void _onCtrlText(String text, RTCDataChannel dc) {
    if (text.isEmpty) return;
    Map<String, dynamic>? m;
    try {
      final v = jsonDecode(text);
      if (v is Map<String, dynamic>) m = v;
    } catch (_) {
      return;
    }
    if (m == null) return;
    final type = m['msgType']?.toString() ?? '';
    final data = m['data'];
    final dm = data is Map ? Map<String, dynamic>.from(data) : <String, dynamic>{};
    switch (type) {
      // [20261022 剪贴板同步] 文本与文件控制消息都从这里进（文件块走二进制分支）。
      case 'clipText':
        _ensureClip()?.handleCtrlText(m);
        break;
      case 'fileOffer':
      case 'fileDone':
        _ensureClip()?.handleFileText(m);
        break;
      case 'deskBehavior':
        dispatchDeskBehavior(dm, 'dc');
        break;
      case 'h264Ready':
        _onH264Ready();
        break;
      case 'h264_stop':
        _onH264Stop();
        break;
      case 'h264Keyframe':
        unawaited(hostInvoke(bridge, 'captureKeyframe', {}));
        break;
      case 'h264ClockPing':
        // 主控时钟探针（1s/条）：原通道回 pong（t2=本机 epoch，与 web 一致）
        _dcSendText(dc, <String, dynamic>{
          'msgType': 'h264ClockPong',
          'requestId': m['requestId'] ?? 'clock',
          'data': <String, dynamic>{
            't1': dm['t1'],
            't2': DateTime.now().millisecondsSinceEpoch,
          },
        });
        break;
      case 'h264ClockPong':
        // 本端探针的 pong → 喂拥塞 RTT
        final t1 = (dm['t1'] as num?)?.toInt() ?? 0;
        _h264?.onClockPong(t1);
        break;
      default:
        break;
    }
  }

  bool _dcSendText(RTCDataChannel? dc, Map<String, dynamic> obj) {
    if (dc == null ||
        dc.state != RTCDataChannelState.RTCDataChannelOpen) {
      return false;
    }
    try {
      dc.send(RTCDataChannelMessage(jsonEncode(obj)));
      return true;
    } catch (_) {
      return false;
    }
  }

  // ------------------------------------------------------- 输入路由

  /// deskBehavior → _rid 去重 → 物理坐标 → 被控核注入
  void dispatchDeskBehavior(Map<String, dynamic> d, String source) {
    if (d.isEmpty) return;
    final rid = d['_rid']?.toString();
    if (rid != null && rid.isNotEmpty) {
      if (_seenRid.contains(rid)) return;
      _seenRid.add(rid);
      if (_seenRid.length > 1200) {
        _seenRid.remove(_seenRid.first);
      }
    }
    final type = (d['type'] as num?)?.toInt();
    if (type == null) return;
    final sf = HostCore.shared.scaleFactor > 0 ? HostCore.shared.scaleFactor : 1.0;
    final dw = HostCore.shared.dispW;
    final dh = HostCore.shared.dispH;
    final x = dw * sf * (((d['x'] as num?) ?? 0) / 1000);
    final y = dh * sf * (((d['y'] as num?) ?? 0) / 1000);
    final hasXY = ((d['x'] as num?) ?? 0) != 0 || ((d['y'] as num?) ?? 0) != 0;
    final key = d['key'];
    final amount = (d['amount'] as num?)?.toInt() ?? 1;

    Future<void> call(String channel, Map<String, dynamic> data) =>
        hostInvoke(bridge, channel, data);

    switch (type) {
      case DeskBehavior.mouseMove:
      case DeskBehavior.mouseDrag:
        call(type == DeskBehavior.mouseMove ? 'mouseMove' : 'mouseDrag',
            {'x': x, 'y': y});
        break;
      case DeskBehavior.setPosition:
        call('mouseSetPosition', {'x': x, 'y': y});
        break;
      case DeskBehavior.leftClick:
        call('mouseLeftClick', {});
        break;
      case DeskBehavior.rightClick:
        call('mouseRightClick', {});
        break;
      case DeskBehavior.doubleClick:
        call('mouseDoubleClick', {});
        break;
      case DeskBehavior.pressButtonLeft:
        if (hasXY) call('mouseSetPosition', {'x': x, 'y': y});
        call('mousePressButtonLeft', {});
        break;
      case DeskBehavior.releaseButtonLeft:
        if (hasXY) call('mouseSetPosition', {'x': x, 'y': y});
        call('mouseReleaseButtonLeft', {});
        break;
      case DeskBehavior.pressButtonRight:
      case DeskBehavior.releaseButtonRight:
        // 右键 press/release：web 被控走 mousePressButtonRight（ipc.rs 无独立
        // 通道时退化为 click 语义）—— 保持与 ipc.rs 通道表一致
        call('mouseRightClick', {});
        break;
      case DeskBehavior.scrollDown:
        call('mouseScrollDown', {'amount': amount});
        break;
      case DeskBehavior.scrollUp:
        call('mouseScrollUp', {'amount': amount});
        break;
      case DeskBehavior.scrollLeft:
        call('mouseScrollLeft', {'amount': amount});
        break;
      case DeskBehavior.scrollRight:
        call('mouseScrollRight', {'amount': amount});
        break;
      case DeskBehavior.keyboardType:
        call('keyboardType', {'key': key ?? ''});
        break;
      case DeskBehavior.keyboardPressKey:
        call('keyboardPressKey', {'key': key});
        break;
      case DeskBehavior.keyboardReleaseKey:
        call('keyboardReleaseKey', {'key': key});
        break;
      case DeskBehavior.switchIme:
        call('switchIme', {'method': (key is List && key.isNotEmpty) ? key[0] : 0});
        break;
      case DeskBehavior.releaseAllModifiers:
        call('releaseAllModifiers', {});
        break;
      case DeskBehavior.privacyScreenOn:
        call('privacyScreen', {'on': true});
        break;
      case DeskBehavior.privacyScreenOff:
        call('privacyScreen', {'on': false});
        break;
      case DeskBehavior.textChat:
        final text = key is String ? key : '';
        if (text.isNotEmpty) call('keyboardType', {'key': text});
        break;
      case DeskBehavior.sendCtrlAltDel:
        call('sendCtrlAltDel', {});
        break;
      case DeskBehavior.openTaskManager:
        call('openTaskManager', {});
        break;
      case DeskBehavior.lockScreen:
        call('lock_workstation', {});
        break;
      default:
        break;
    }
  }

  // ------------------------------------------- 标准 WebRTC 视频轨（九轮）

  /// [20261021 九轮] 挂一条**标准 WebRTC 视频轨**（对齐 billddesk 的做法）。
  ///
  /// ## 为什么推倒重来
  ///
  /// 旧路径是我们自己写的：DXGI/NVENC 出 H.264 → 拆包 → DataChannel 分片 →
  /// 主控自建解码器。这条路**没有 NACK、没有拥塞控制、没有丢包隐藏**：
  /// 丢一个分片整帧作废 → 主控判参考链断 → 只能等下一个 IDR（10 秒）→
  /// 用户看到的就是「画面很糊、点了半天没反应」（实际是画面根本没更新）。
  ///
  /// billddesk 在同机同网正常，因为它是 `desktopCapturer` + `addTrack`，
  /// 编码、重传、PLI/FIR、GCC 全由 libwebrtc 负责（见
  /// `G:\billddesk源码\web\src\utils\network\webRTC.ts:147`）。
  ///
  /// flutter_webrtc 的 Windows 插件**本来就带桌面捕获**
  /// （`common/cpp/src/flutter_screen_capture.cc`，编译宏 RTC_DESKTOP_DEVICE），
  /// 所以这条路在我们自己的壳里也能走，不必依赖 Electron。
  ///
  /// 成功返回 true；失败返回 false 由调用方回退 H264 直送（不黑屏）。
  Future<bool> _attachScreenTrack(RTCPeerConnection p) async {
    try {
      final sources = await desktopCapturer.getSources(
        types: [SourceType.Screen],
        thumbnailSize: ThumbnailSize(0, 0),
      );
      ShellLog.w('[receiver] 屏幕源 ${sources.length} 个'
          '${sources.isEmpty ? '' : '：${sources.map((s) => '${s.id}/${s.name}').join(', ')}'}');
      if (sources.isEmpty) return false;

      // 主控只看到一个桌面 → 取第一块整屏（多屏时是主屏）
      final src = sources.first;
      final stream =
          await navigator.mediaDevices.getDisplayMedia(<String, dynamic>{
        'video': <String, dynamic>{
          'deviceId': <String, dynamic>{'exact': src.id},
          'mandatory': <String, dynamic>{
            'frameRate': currentMaxFramerate.toDouble(),
            'minFrameRate': 10.0,
          },
          'optional': <String, dynamic>{},
        },
        'audio': false,
      });
      final tracks = stream.getVideoTracks();
      if (tracks.isEmpty) {
        ShellLog.w('[receiver] getDisplayMedia 无视频轨，回退 H264 直送');
        await stream.dispose();
        return false;
      }
      final track = tracks.first;
      final sender = await p.addTrack(track, stream);

      _screenStream = stream;
      _screenTrack = track;
      _screenSender = sender;
      _screenTrackUp = true;
      await _applyVideoSenderParams();

      // [20261021 十三轮] 这里原本调 CursorHider.hide() 把被控端系统光标
      // 换成透明光标，好让帧里不带光标。实测代价是**被控端本机也没有鼠标了**，
      // 用户当场炸掉 —— 已彻底移除，被控端光标该什么样就什么样。

      // 轨道自己会请求关键帧（PLI/FIR），这里只做可观测性日志
      ShellLog.w('[receiver] 视频轨已挂载（libwebrtc 采集+编码），'
          '目标 ${currentMaxFramerate}fps ${currentMaxBitrate}kbps');
      return true;
    } catch (e) {
      ShellLog.w('[receiver] 视频轨挂载失败，回退 H264 直送: $e');
      return false;
    }
  }

  /// 把当前画质档位应用到发送侧（码率/帧率上限），WebRTC 的 GCC 在此之上
  /// 自行根据丢包与 RTT 收放 —— 这正是我们缺的那一层。
  Future<void> _applyVideoSenderParams() async {
    final sender = _screenSender;
    if (sender == null) return;
    try {
      final params = sender.parameters;
      final enc = (params.encodings != null && params.encodings!.isNotEmpty)
          ? params.encodings!.first
          : RTCRtpEncoding();
      enc.active = true;
      enc.maxBitrate = currentMaxBitrate * 1000;
      enc.maxFramerate = currentMaxFramerate;
      params.encodings = <RTCRtpEncoding>[enc];
      // [20261021 九轮] 桌面场景保清晰优先：带宽不够时宁可掉帧，不要降分辨率。
      // 上次「为省性能自动降分辨率」正是画面变糊的直接原因。
      params.degradationPreference = RTCDegradationPreference.MAINTAIN_RESOLUTION;
      await sender.setParameters(params);
      ShellLog.w('[receiver] 视频发送参数已应用：'
          '${currentMaxBitrate}kbps / ${currentMaxFramerate}fps');
    } catch (e) {
      ShellLog.w('[receiver] setParameters 失败（用默认值继续）: $e');
    }
  }

  /// 停轨并释放（拆除 PC 前必须调，否则 libwebrtc 的 capturer 会继续抓屏）
  Future<void> _stopScreenTrack() async {
    _screenTrackUp = false;
    _screenSender = null;
    try {
      await _screenTrack?.stop();
    } catch (_) {}
    try {
      await _screenStream?.dispose();
    } catch (_) {}
    _screenTrack = null;
    _screenStream = null;
  }

  // ------------------------------------------------------- H264 直送

  ({int fps, int kbps, String cfg}) _h264EffectiveCfg() {
    final tier = _H264Learn.tier.clamp(0, h264FpsTiers.length - 1);
    final fps = h264FpsTiers[tier];
    final wantKbps = (currentMaxBitrate * h264KbpsFactor[tier]).round();
    final kbps = _H264Learn.capKbps > 0
        ? (wantKbps.clamp(1200, _H264Learn.capKbps))
        : wantKbps.clamp(300, 40000);
    return (fps: fps, kbps: kbps, cfg: '$kbps/$fps');
  }

  Future<void> _onH264Ready() async {
    // [20261021 九轮] 视频轨道已经在跑 → 直送没有存在意义，直接忽略。
    // 两路并存会双倍占带宽、还会让主控的渲染路径来回切。
    if (_screenTrackUp) {
      ShellLog.w('[receiver] 已用标准视频轨，忽略 h264Ready');
      return;
    }
    // 停用窗口内忽略（对齐 web 20260921.16）
    if (DateTime.now().isBefore(_h264SuppressedUntil)) {
      ShellLog.w('[receiver] h264 停用窗口内，忽略 h264Ready');
      return;
    }
    if (_h264 != null) return; // 幂等
    await _startH264Downstream();
  }

  /// 抓屏目标宽度（像素）。`currentResolutionRatio` 语义是「目标高度」
  /// （服务端高清档 = 1080，均衡 720，流畅 540）。屏幕本身不高于目标高度
  /// 时返回 0 = 不缩放（原生分辨率），否则按比例收宽度。
  int _targetMaxWidth() {
    final dw = HostCore.shared.dispW;
    final dh = HostCore.shared.dispH;
    final targetH = currentResolutionRatio;
    if (dw <= 0 || dh <= 0 || targetH < 480) return 0; // 参数异常一律原生
    if (dh <= targetH) return 0;
    return (dw * targetH / dh).round();
  }

  Future<void> _startH264Downstream() async {
    final cfg = _h264EffectiveCfg();
    final sender = H264Sender(
      invoke: (ch, data) => hostInvoke(bridge, ch, data),
      frameBase: 'http://127.0.0.1:${HostCore.corePort}/__capture/frame',
      sendPacket: (Uint8List p) async {
        final dc = _dcVideoSelf;
        if (dc == null) return false;
        // [20261021] 不再用 `dc.state != Open` 作为发送前置闸门：flutter_webrtc
        // 的状态上报在密集发送下并不可靠（真机现场：ping 只有 4ms、关联完全
        // 健康，却因状态位被判成不可用而整整 13 秒一帧不发）。改为先发、失败
        // 再取证 —— 真关掉的通道 send 会抛，语义不变但不会误杀。
        try {
          await dc.send(RTCDataChannelMessage.fromBinary(p));
          _dcVideoBadLogged = false;
          return true;
        } catch (e) {
          if (!_dcVideoBadLogged) {
            _dcVideoBadLogged = true;
            ShellLog.w('[receiver] VideoChannel send 失败: state=${dc.state} err=$e');
          }
          return false;
        }
      },
      sendCtrl: (text) => _dcSendText(_dcVideoSelf, jsonDecode(text) as Map<String, dynamic>),
      onOverload: _onH264Overload,
      // [20261020] 断流自愈：核自毁重建后取屏会话消失，时钟探针仍活着会
      // 让主控看门狗失明 —— 本端 8s 无帧直接重建取屏会话（captureStart）。
      onStalled: () => unawaited(_restartH264()),
      // [20261021] 通道已死：拆 PC 让主控（12s 无帧 revoke → 8s 重试握手）
      // 拿到一条全新连接。旧实现没有这条路径 —— VideoChannel 一死就永远
      // 卡在"每帧失败 + 空转"，画面定住直到用户手动断开。
      onChannelDead: () => unawaited(_teardownPc('VideoChannel 失效')),
    );
    // [20261020] 启动失败自动重试：核侧换届（停旧建新）在真机上可能超过
    // 一次性窗口，失败后 2.5s 重试一次 —— 正好衔接主控 revoke 后 8s 的
    // 重试握手，别让一次瞬时失败直接把会话打回黑屏。
    Object? lastErr;
    for (var attempt = 1; attempt <= 2; attempt++) {
      try {
        final dim = await sender.start(
            fps: cfg.fps, bitrateKbps: cfg.kbps, maxWidth: _targetMaxWidth());
        _h264 = sender;
        ShellLog.w(
            '[receiver] H264 直送已启动 ${dim.width}x${dim.height} 目标 ${cfg.fps}fps ${cfg.kbps}kbps'
            '${attempt > 1 ? '（第 $attempt 次尝试）' : ''}');
        return;
      } catch (e) {
        lastErr = e;
        sender.dispose();
        ShellLog.w('[receiver] H264 启动失败（第 $attempt 次）: $e');
        if (attempt < 2) {
          await Future.delayed(const Duration(milliseconds: 2500));
        }
      }
    }
    ShellLog.w('[receiver] H264 启动重试仍失败: $lastErr');
  }

  void _stopH264Downstream() {
    _h264?.stop();
    _h264 = null;
  }

  void _onH264Stop() {
    // [20261020] 10 分钟停用窗口是旧「revoke 后回退 JPEG」架构的遗产——
    // Flutter 被控没有 JPEG 兜底，revoke 后唯一出路就是重启直送。真机实测：
    // 主控 6s 无首帧 revoke → 重试握手 8s 后到达 → 全被 10 分钟窗口忽略
    // → 用户感知「断开后再也连不上」。改为 3s 冷却：只防 revoke 风暴，
    // 不挡主控的正常重试握手。
    ShellLog.w('[receiver] 收到 h264_stop，停直送（3s 冷却后可重试）');
    _stopH264Downstream();
    _h264SuppressedUntil = DateTime.now().add(const Duration(seconds: 3));
  }

  /// 过载降档（对齐 web 20261019r：cap=被拒码率×0.7 单调收敛，会话中禁回升）
  void _onH264Overload() {
    final failedKbps = _h264EffectiveCfg().kbps;
    final cap =
        (failedKbps * 0.7).round().clamp(1200, 16000);
    if (cap < _H264Learn.capKbps || _H264Learn.capKbps == 0) {
      _H264Learn.capKbps = cap;
      _H264Learn.save();
      ShellLog.w('[receiver] 学习链路容量上限 $cap kbps');
    }
    if (_H264Learn.tier >= h264FpsTiers.length - 1) {
      ShellLog.w('[receiver] 已是保底档位 tier4，保持最低档');
      return;
    }
    _H264Learn.tier += 1;
    _H264Learn.save();
    ShellLog.w(
        '[receiver] 链路过载 → 降档 ${_H264Learn.tier}（${h264FpsTiers[_H264Learn.tier]}fps），重建直送');
    _restartH264();
  }

  bool _h264Restarting = false;
  Future<void> _restartH264() async {
    if (_h264Restarting) return;
    _h264Restarting = true;
    try {
      _stopH264Downstream();
      await Future.delayed(const Duration(milliseconds: 300));
      await _startH264Downstream();
    } finally {
      _h264Restarting = false;
    }
  }

  // ------------------------------------------------------- 画质同步

  void _onQualityChange(String kind, Map<String, dynamic> d) {
    final val = (d['val'] as num?)?.toInt() ?? 0;
    if (val <= 0) return;
    var changed = false;
    switch (kind) {
      case 'bitrate':
        if (val != currentMaxBitrate) {
          currentMaxBitrate = val;
          changed = true;
        }
        break;
      case 'framerate':
        if (val != currentMaxFramerate) {
          currentMaxFramerate = val;
          changed = true;
        }
        break;
      case 'resolution':
        if (val != currentResolutionRatio) {
          currentResolutionRatio = val;
          changed = true;
        }
        break;
    }
    // cfgdrift 语义（对齐 web 20260930）：参数变化才重建（本端在会话中）
    if (changed && _screenTrackUp) {
      // [20261021 九轮] 视频轨不需要重建：码率/帧率上限直接 setParameters 生效，
      // 分辨率档位交给 libwebrtc 的 scaleResolutionDownBy（这里只调码率与帧率，
      // 保持原生清晰度——上次为了"省性能"降分辨率正是模糊的来源）。
      unawaited(_applyVideoSenderParams());
    }
    if (changed && _h264 != null) {
      unawaited(_restartH264());
    }
  }

  // ------------------------------------------------------- 拆除

  Future<void> _teardownPc(String reason) async {
    final hadSession = _pc != null;
    _peerSocketId = null;
    _stopH264Downstream();
    // [20261021 九轮] 先停采再关 PC：libwebrtc 的桌面捕获器是 PC 之外的对象，
    // 不显式 stop 会一直抓屏（用户会看到隐私提示常亮 + 白吃 CPU）。
    await _stopScreenTrack();
    _dcRecv.clear();
    _dcCtrlSelf = null;
    _dcMouseSelf = null;
    _dcVideoSelf = null;
    _dcFileSelf = null;
    // [20261022] 剪贴板轮询跟着停（实例保留，下一个主控进来复用）
    _clip?.stop();
    final p = _pc;
    _pc = null;
    if (p != null) {
      try {
        await p.close();
      } catch (_) {}
    }
    unawaited(hostInvoke(bridge, 'captureStop', {}));
    unawaited(hostInvoke(bridge, 'setRemoteViewerCount', {'count': 0}));
    if (hadSession) {
      if (ShellPrefs.lockAfterSession) {
        unawaited(hostInvoke(bridge, 'lock_workstation', {}));
      }
      if (!_disposed && _state == ReceiverState.controlled) {
        _setState(ReceiverState.online);
      }
    }
    if (reason.isNotEmpty) ShellLog.w('[receiver] 会话拆除：$reason');
  }
}
