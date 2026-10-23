/// 主控端远控会话 —— 对齐 web/src/hooks/use-websocket.ts 主控分支 + remoteDesk.ts。
///
/// 时序：
/// 1. connect 信令 → onConnected 发 deskJoin
/// 2. 发 deskStartRemote（失败自动重试一次，2s，对齐 web [20260921.4]）
/// 3. 收 deskStartRemoteResult（code==0 取 receiver）
/// 4. 收 nativeWebRtcOffer(isRemoteDesk) → 建 PC → setRemote → answer → 发 nativeWebRtcAnswer
/// 5. 双向 nativeWebRtcCandidate
/// 6. onTrack → 远端流交给 UI 渲染
/// 7. 输入：deskBehavior（坐标 0~1000 归一化）
/// 8. 结束：发 deskCloseRemote
import 'dart:async';
import 'dart:convert';
import 'dart:io' show RawDatagramSocket, RawSocketEvent, InternetAddress;
import 'dart:math' as math;
import 'dart:typed_data';

import 'package:flutter_webrtc/flutter_webrtc.dart';

import '../config/turn_config.dart';
import '../core/clip_sync.dart';
import '../core/shell_log.dart';
import '../signal/protocol.dart';
import '../signal/signaling_client.dart';
import 'h264_direct.dart';

/// 幂等闸门对齐 web：connected/completed 的存量 PC 跳过新 offer，残留重建
enum SessionState { idle, joining, starting, negotiating, connected, failed, closed }

class ControllerSession {
  ControllerSession({
    required this.signaling,
    required this.deskUserUuid,
    required this.deskUserPassword,
    required this.myUuid,
    required this.myPassword,
    // [20261017b] 默认对齐 web 主控高清档（原生≤1080 / 30fps / 8Mbps）。
    // ⚠️ 原 resolutionRatio=100 是灾难默认：被控把它当抓屏目标高度 →
    // 宽度兜底下限 640 → 抓屏缩到 640x400 = 「画面模糊 + 屏幕尺寸不对」。
    this.maxBitrate = 8000,
    this.maxFramerate = 30,
    this.resolutionRatio = 1080,
    this.videoContentHint = 'motion',
    this.audioContentHint = 'motion',
  });

  final SignalingClient signaling;
  /// 被控端凭据（服务端 flag1 校验 + 会话房间号）
  final String deskUserUuid;
  final String deskUserPassword;
  /// 主控端自己的身份（服务端 flag2 校验 + Redis 在线键）
  final String myUuid;
  final String myPassword;
  final int maxBitrate;
  final int maxFramerate;
  final int resolutionRatio;
  final String videoContentHint;
  final String audioContentHint;

  SessionState state = SessionState.idle;
  String? mySocketId;
  String? peerSocketId; // 被控端 socket_id（deskStartRemoteResult / offer.sender）

  RTCPeerConnection? pc;
  MediaStream? remoteStream;
  /// [20261019] 被控端设备码（连接时写入；最近连接记录展示 IP 用）
  String? remoteCode;

  // [20261019 输入直连] 被控端 offer 自带两条 DataChannel（webRTC.ts
  // new WebRTCClass 时 createDataChannel）：MessageChannel（可靠，点击/
  // 键盘/画质指令）+ MouseChannel（不可靠，鼠标移动）。answer 侧通过
  // onDataChannel 收到后，输入直接走 DC 点对点 —— 绕过信令服务器中转
  //（旧路径：主控→服务器→被控，点击延迟多一整个 RTT，用户反馈
  // 「鼠标点击反应很慢」的主因）。信令 WS 保留为兜底链路。
  RTCDataChannel? _dcCtrl;
  RTCDataChannel? _dcMouse;
  bool _dcLogged = false;

  // [20261022 剪贴板同步] 文本 + 文件双向搬运。通道分工：
  //   clipText      → MessageChannel（可靠文本帧，小、要求必达）
  //   fileOffer/Done → FileChannel（文本帧）
  //   文件数据       → FileChannel（二进制帧，16KB/块）
  // 为什么要独立一条 FileChannel：文件流会把 MessageChannel 的内部发送队列
  // 堵住，而点击/键盘也走那条 —— 传文件时手感会变卡，这是不可接受的。
  RTCDataChannel? _dcFile;
  ClipSync? _clip;
  final StreamController<ClipEvent> _clipCtrl =
      StreamController<ClipEvent>.broadcast();

  /// 剪贴板事件流（UI 用它显示"正在发送/收到文件"）
  Stream<ClipEvent> get clipEvents => _clipCtrl.stream;

  // [20261019e H264 直送] 被控端『VideoChannel』：二进制 = NVENC H.264 分片
  //（h264Packet 协议），文本 = 时钟探针/剪贴板。握手：VideoChannel open 后
  // 发 h264Ready → 被控切直送并停 JPEG 兼容链；直送建立失败由看门狗发
  // h264_stop 恢复 JPEG。msgType 值与 web WsMsgTypeEnum 逐字一致。
  final H264Direct h264 = H264Direct();
  RTCDataChannel? _dcVideo;
  Timer? _h264ReadyTimer;
  Timer? _h264Watchdog;
  // [20261019j] 时钟对齐探针（1s/条，VideoChannel 文本）——端到端延迟的前提。
  Timer? _clockPingTimer;
  DateTime? _h264ReadyAt;
  int _h264FailCount = 0;
  bool _h264StopSent = false;
  /// [20261021 九轮] 是否已收到被控端的**标准视频轨**。为 true 时不再发
  /// h264Ready（直送与轨道互斥，并存只会互抢带宽 + 让渲染路径来回切）。
  bool _trackVideoUp = false;
  /// [20261022 黑屏] 首帧探针：onTrack 只说明 SDP 里有 m=video，媒体面是否
  /// 通要看 getStats 里 inbound-rtp 的 bytesReceived / framesDecoded。
  Timer? _mediaProbe;
  int _mediaProbeTries = 0;
  int _weakTries = 0;
  int _statsErrCount = 0;
  bool _iceConnected = false;

  final _stateCtrl = StreamController<SessionState>.broadcast();
  final _remoteStreamCtrl = StreamController<MediaStream>.broadcast();
  final _errorCtrl = StreamController<String>.broadcast();
  final _logCtrl = StreamController<String>.broadcast();

  // [20261020] 视频就绪通知（connect_page 百分比动画解锁用）：
  // onTrack（JPEG 轨）与 H264 直送首帧都触发。H264 直送没有 MediaStream
  // 轨道，此前 connect_page 只等 remoteStreamStream → 直送模式永远卡在
  // 「正在连接 80%」进不了画面页（20261020 e2e 实锤）。
  final _videoReadyCtrl = StreamController<void>.broadcast();
  bool _videoReadyFired = false;

  void _fireVideoReady() {
    if (_videoReadyFired || _disposed) return;
    _videoReadyFired = true;
    if (!_videoReadyCtrl.isClosed) _videoReadyCtrl.add(null);
  }

  Stream<SessionState> get stateStream => _stateCtrl.stream;
  Stream<MediaStream> get remoteStreamStream => _remoteStreamCtrl.stream;
  Stream<void> get videoReadyStream => _videoReadyCtrl.stream;
  Stream<String> get errorStream => _errorCtrl.stream;
  Stream<String> get logStream => _logCtrl.stream;

  StreamSubscription<SignalMessage>? _msgSub;
  Timer? _onlineTimer;
  Timer? _myRoomTimer;
  int _startRetries = 0;
  bool _disposed = false;

  void _log(String s) {
    ShellLog.w('[session] $s');
    if (!_logCtrl.isClosed) _logCtrl.add(s);
  }

  void _setState(SessionState s) {
    state = s;
    if (!_stateCtrl.isClosed) _stateCtrl.add(s);
  }

  // ---------------------------------------------------------------- 启动

  Future<void> start() async {
    mySocketId = signaling.socketId;
    _log('mySocketId=$mySocketId');
    // [20261022] TURN 预检放到后台跑，不阻塞连接时序
    unawaited(_probeTurnServer());

    _msgSub = signaling.onMessage.listen(_onSignal);
    signaling.startHeartbeat();

    // 1. 加入房间（对齐 web：connect 后立刻 deskJoin）。
    //    ⚠️ 服务端 handleWsDeskJoin 的两个参数语义独立：
    //    · live_room_id       → socket.join 的房间（收广播用）= 被控房间
    //    · data.deskUserUuid  → Redis 在线键 key（exp 10s）= join 者**自己**的身份
    //      （web 主控端 sendJoin 填自己的码；填被控码会把被控在线键覆盖成主控
    //       socket_id → 服务端 startRemote 的 receiver 取错 → 被控端拒绝受理）
    _setState(SessionState.joining);
    signaling.send(WsMsg.deskJoin, <String, dynamic>{
      'deskUserUuid': myUuid,
      'deskUserPassword': myPassword,
      'live_room_id': deskUserUuid,
    });

    // 1b. 主控自己的在线键：服务端 deskJoin 写 Redis（exp 10s, key=join 的 uuid）。
    //     deskStartRemote 会校验 remoteDeskUserUuid 在线 → 加入自己房间 + 周期续期。
    _joinMyRoom();
    _myRoomTimer = Timer.periodic(const Duration(seconds: 8), (_) {
      if (!_disposed) _joinMyRoom();
    });

    // 2. 发起控制（join 后稍候，给服务端 socket.join 留一拍；失败自动重试一次）
    Timer(const Duration(milliseconds: 600), () {
      if (!_disposed && state == SessionState.joining) _sendStartRemote();
    });

    // 2b. 在线保活（对齐 web 的 __indeskOnlineKeeper：10s 幂等补发 deskUpdateUser）。
    //     ⚠️ [20261017 转圈根因②] deskUserUuid 必须填**主控自己的码**：
    //     服务端 handleWsDeskUpdateUser 以 key=deskUserUuid 写在线键
    //     （socket_id=发送方）。填被控码会把被控在线键覆盖成主控 socket →
    //     下一次 startRemote 的 receiver 指向主控旧 socket → 被控收不到
    //     start_remote → 永远无 offer → 画面页永远转圈（实测复现）。
    _onlineTimer = Timer.periodic(const Duration(seconds: 10), (_) {
      signaling.send(WsMsg.deskUpdateUser, <String, dynamic>{
        'roomId': deskUserUuid,
        'sender': mySocketId,
        'receiver': peerSocketId ?? '',
        'maxBitrate': maxBitrate,
        'maxFramerate': maxFramerate,
        'resolutionRatio': resolutionRatio,
        'videoContentHint': videoContentHint,
        'audioContentHint': audioContentHint,
        'deskUserUuid': myUuid,
        'deskUserPassword': myPassword,
        'remoteDeskUserUuid': deskUserUuid,
        'remoteDeskUserPassword': deskUserPassword,
      });
    });
  }

  void _joinMyRoom() {
    signaling.send(WsMsg.deskJoin, <String, dynamic>{
      'deskUserUuid': myUuid,
      'deskUserPassword': myPassword,
      'live_room_id': myUuid,
    });
  }

  void _sendStartRemote() {
    _setState(SessionState.starting);
    // ⚠️ 服务端命名语义与直觉相反（server/src/config/websocket/handleMsg.ts）：
    //   deskUserUuid/Password     = 主控自己的凭据（flag1 "主控密码错误"）
    //   remoteDeskUserUuid/Pass.. = 被控端凭据   （flag2 "被控密码错误" + Redis 在线校验，
    //                               receiver = 被控端 socket_id，被控端以此判定受理并发 offer）
    //   roomId                    = 被控端房间（result 广播到该房间，被控端据此触发采集）
    signaling.send(WsMsg.deskStartRemote, <String, dynamic>{
      'roomId': deskUserUuid,
      'sender': mySocketId,
      'receiver': '',
      'maxBitrate': maxBitrate,
      'maxFramerate': maxFramerate,
      'resolutionRatio': resolutionRatio,
      'videoContentHint': videoContentHint,
      'audioContentHint': audioContentHint,
      'deskUserUuid': myUuid,
      'deskUserPassword': myPassword,
      'remoteDeskUserUuid': deskUserUuid,
      'remoteDeskUserPassword': deskUserPassword,
    });
  }

  // ---------------------------------------------------------------- 信令分发

  void _onSignal(SignalMessage m) {
    if (_disposed) return;
    try {
      switch (m.type) {
        case WsMsg.deskJoined:
          _log('deskJoined: ${m.data}');
          // 只在首次加入被控房间后触发 startRemote；
          // 周期续在线键的 deskJoined（自己房间）忽略。
          if (state == SessionState.joining) _sendStartRemote();
          break;
        case WsMsg.deskStartRemoteResult:
          _onStartRemoteResult(m.data);
          break;
        case WsMsg.nativeWebRtcOffer:
          _onRemoteOffer(m.data);
          break;
        case WsMsg.nativeWebRtcCandidate:
          _onRemoteCandidate(m.data);
          break;
        case WsMsg.deskCloseRemote:
          _log('被控端关闭会话');
          close(notifyRemote: false);
          break;
      }
    } catch (e) {
      _log('信号处理异常 $m.type: $e');
    }
  }

  void _onStartRemoteResult(Map<String, dynamic> d) {
    final code = (d['code'] as num?)?.toInt() ?? -1;
    final data = d['data'];
    final recv = data is Map ? data['receiver'] as String? : null;
    // [20261017] receiver 指向自己 = 被控在线键被旧会话污染（Redis exp10s），
    // 等键过期+被控保活重建后再试；连同普通失败一起走 11s×3 重试。
    final polluted = recv != null && recv == mySocketId;
    if (code != 0 || polluted) {
      if (_startRetries < 3) {
        _startRetries++;
        _log('startRemote 失败（code=$code${polluted ? ' receiver=自己,在线键被污染' : ' ${d['msg']}'}），11s 后重试 $_startRetries/3');
        Timer(const Duration(seconds: 11), () {
          if (!_disposed && (state == SessionState.starting ||
              state == SessionState.negotiating)) {
            _sendStartRemote();
          }
        });
        return;
      }
      _setState(SessionState.failed);
      _errorCtrl.add(polluted
          ? '被控端状态未就绪，请确认对方 InDesk 正在运行后重试'
          : '发起控制失败：${d['msg'] ?? code}');
      return;
    }
    _startRetries = 0;
    peerSocketId = recv ?? peerSocketId;
    _log('startRemote OK, receiver=$peerSocketId，等待被控端 offer…');
    _setState(SessionState.negotiating);
  }

  // ---------------------------------------------------------------- WebRTC

  /// offer 处理串行链：被控端会快速连发两次 offer（handleRTC 拆残留重建），
  /// 而建 PC + answer 需要约 2s。若并发处理，offer2 在 `pc` 赋值前进来会走
  /// "pc==null" 分支再建一个 PC → 两个 answer 交叉错配 → ICE Failed（实测）。
  /// 串行化后 offer2 等待 offer1 完整落地，随后按幂等闸门拆旧建新，只发一份 answer。
  Future<void> _offerChain = Future<void>.value();

  void _onRemoteOffer(Map<String, dynamic> d) {
    final snapshot = Map<String, dynamic>.from(d);
    _offerChain = _offerChain.then(
      (_) async {
        if (_disposed) return;
        try {
          await _doRemoteOffer(snapshot);
        } catch (e) {
          _log('offer 处理异常: $e');
        }
      },
    );
  }

  Future<void> _doRemoteOffer(Map<String, dynamic> d) async {
    final sender = d['sender'] as String?;
    final receiver = d['receiver'] as String?;
    if (receiver != mySocketId) {
      _log('offer 不是发给我的（receiver=$receiver）');
      return;
    }
    // 幂等闸门（对齐 web [v43]）：已连接的会话跳过；非连接态残留拆除重建
    if (pc != null) {
      final cs = await pc!.getConnectionState();
      if (cs == RTCPeerConnectionState.RTCPeerConnectionStateConnected) {
        _log('已有 connected 会话，忽略新 offer');
        return;
      }
      _log('拆除非连接态残留 PC（$cs）后重建');
      await _teardownPc();
    }
    peerSocketId = sender;

    final sdp = d['sdp'];
    if (sdp is! Map) {
      _errorCtrl.add('offer sdp 格式异常');
      return;
    }

    _setState(SessionState.negotiating);
    _log('收到被控端 offer（${sdp['type']}），建 PC…');

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

    p.onTrack = (RTCTrackEvent event) {
      _log('onTrack: ${event.track.kind}（SDP 里带轨 ≠ 媒体已到，等首帧统计）');
      // [20261021 九轮] 被控端改走标准视频轨（libwebrtc 采集+编码）后，
      // 这里就是唯一的画面来源，不再需要自建 H264 解码器。
      if (event.track.kind == 'video') {
        _trackVideoUp = true;
      }
      // Chrome 被控端始终带 msid（stream 非空）；空流兜底暂不合成
      if (event.streams.isNotEmpty) {
        remoteStream = event.streams.first;
        if (!_remoteStreamCtrl.isClosed) {
          _remoteStreamCtrl.add(remoteStream!);
        }
      }
      // [20261022 黑屏] **不要在这里 fireVideoReady**。onTrack 在
      // setRemoteDescription 时就触发（answer 还没发出去），此时 ICE 可能
      // 一根候选都没通 —— 进度条却直接冲到 100% 进画面页 = 「100% 黑屏」。
      // 真正的就绪判据改成「getStats 里 inbound-rtp 真收到字节/解出帧」。
      if (event.track.kind == 'video') _startMediaProbe();
    };
    p.onIceCandidate = (RTCIceCandidate c) {
      signaling.send(WsMsg.nativeWebRtcCandidate, <String, dynamic>{
        'live_room_id': deskUserUuid,
        'candidate': <String, dynamic>{
          'candidate': c.candidate,
          'sdpMid': c.sdpMid,
          'sdpMLineIndex': c.sdpMLineIndex,
        },
        'receiver': peerSocketId,
        'sender': mySocketId,
      });
    };
    p.onConnectionState = (RTCPeerConnectionState s) {
      _log('PC 状态: $s');
      if (s == RTCPeerConnectionState.RTCPeerConnectionStateConnected) {
        _iceConnected = true;
        _setState(SessionState.connected);
        // [20261022 剪贴板同步] 通道连通后才开始轮询（提前会满屏发送失败）。
        _ensureClip()?.start();
        // [20261021 九轮] 被控端现在优先挂标准视频轨。PC 连通后给 onTrack
        // 1.2s 先到的窗口：有轨道就走 libwebrtc 原生解码渲染（NACK/丢包隐藏/
        // 关键帧请求全由它负责）；确实没有轨道才回退老的 H264 直送握手。
        Future<void>.delayed(const Duration(milliseconds: 1200), () {
          if (_disposed || pc == null) return;
          if (_trackVideoUp) {
            _log('已收到远端视频轨，跳过 H264 直送握手');
            return;
          }
          _startH264Handshake();
        });
      } else if (s == RTCPeerConnectionState.RTCPeerConnectionStateFailed) {
        _setState(SessionState.failed);
        // teardown 后 PC 事件仍可能迟到，此时 error 流已关闭，直接 add 会崩
        if (!_disposed && !_errorCtrl.isClosed) {
          _errorCtrl.add('WebRTC 连接失败（ICE 不通）');
        }
      }
    };
    // [20261022 黑屏] ICE 层状态单独留痕：连不上时能分清是「候选没收到」
    // 还是「收到了但一对都不通」——这是黑屏排查的关键分层证据。
    p.onIceConnectionState = (RTCIceConnectionState s) {
      _log('ICE 状态: $s');
      if (s == RTCIceConnectionState.RTCIceConnectionStateConnected ||
          s == RTCIceConnectionState.RTCIceConnectionStateCompleted) {
        _iceConnected = true;
      }
    };
    p.onIceGatheringState = (RTCIceGatheringState s) {
      _log('ICE 收集: $s');
    };
    // [20261019 输入直连] 被控端 offer 里已带 MessageChannel/MouseChannel
    // 两条数据通道，answer 后从这里到达。label 分流：Mouse 承载移动帧
    // （不可靠），其余走 Message（可靠）—— 与 web 主控完全同构。
    p.onDataChannel = (RTCDataChannel dc) {
      if (_disposed) return;
      _log('收到数据通道 ${dc.label}');
      if (dc.label == 'MouseChannel') {
        _dcMouse = dc;
      } else if (dc.label == 'MessageChannel') {
        _dcCtrl = dc;
        // [20261022] 这条是被控端自建的 MessageChannel，**专门用来收**
        // 它主动发来的东西（剪贴板文本、拥塞回调）。以前没有绑 onMessage，
        // 被控端发过来的一切都被丢掉。
        _bindClipText(dc);
      } else if (dc.label == 'FileChannel') {
        // 被控端自建的 FileChannel = 它往我们这边发文件的通道（只收）。
        _bindFileRecv(dc);
      } else if (dc.label == 'VideoChannel' || dc.label == 'cbVideoChannel') {
        _bindVideoChannel(dc);
      }
      if (!_dcLogged && _dcCtrl != null) {
        _dcLogged = true;
        _log('输入已切换 DataChannel 直连（绕过信令服务器）');
      }
    };

    // [20261019e] 主控端自建 4 条 DataChannel（对齐 web webRTC.ts 1005-1048）：
    // 双方同名通道按 id 配对，**不依赖 onDataChannel**——flutter_webrtc
    // Windows 端的 ondatachannel 事件会丢（实测：上次 3/4 条、这次 1/4 条），
    // 丢通道 = 输入直连失效 + H264 直送没通道，全链残废。web 主控正是靠
    // 自建通道才稳定的。顺序必须与 web 一致（id 分配按创建顺序）。
    _dcCtrl = await p.createDataChannel('MessageChannel', RTCDataChannelInit());
    _dcMouse = await p.createDataChannel(
        'MouseChannel',
        RTCDataChannelInit()
          ..maxRetransmits = 0
          ..ordered = false);
    final video = await p.createDataChannel(
        'VideoChannel',
        RTCDataChannelInit()
          ..maxRetransmits = 2
          ..ordered = true);
    _bindVideoChannel(video);
    final file = await p.createDataChannel('FileChannel', RTCDataChannelInit());
    // [20261022] 发送方向：主控 → 被控端。同时绑接收处理器，双向都安全。
    _dcFile = file;
    _bindFileRecv(file);
    _log('已自建 4 条数据通道（Message/Mouse/Video/File）');

    await p.setRemoteDescription(
      RTCSessionDescription(sdp['sdp'] as String, sdp['type'] as String),
    );
    // 主控端只收不发
    final answer = await p.createAnswer(<String, dynamic>{
      'offerToReceiveVideo': false,
      'offerToReceiveAudio': false,
    });
    await p.setLocalDescription(answer);
    signaling.send(WsMsg.nativeWebRtcAnswer, <String, dynamic>{
      'sdp': <String, dynamic>{'type': answer.type, 'sdp': answer.sdp},
      'sender': mySocketId,
      'receiver': peerSocketId,
      'live_room_id': deskUserUuid,
    });
    _log('answer 已发');
    pc = p;
  }

  // [20261019c] 被控 IP 第二捕获源：被控端经信令发来的每条 candidate SDP
  // 都在这里过手，直接解析 IP/type（srflx 优先于 host，mDNS .local 剔除）。
  // getStats 在部分 libwebrtc 版本不下发 remote-candidate 报告，此路是
  // 必然有数据的兜底。等级：srflx(3) > prflx(2) > host(1)。
  String? remoteIp;
  String? remoteIpType;

  void _captureRemoteIpFromCandidate(String cand) {
    // SDP candidate: <foundation> <comp> <transport> <priority> <ip> <port>
    //                typ <type> ...  → ip=parts[4], typ 后一位=类型
    final s = cand.replaceAll('candidate:', '').trim();
    final parts = s.split(RegExp(r'\s+'));
    if (parts.length < 5) return;
    final ip = parts[4];
    final tIdx = parts.indexOf('typ');
    final typ = (tIdx >= 0 && tIdx + 1 < parts.length) ? parts[tIdx + 1] : '';
    if (ip.endsWith('.local')) return; // mDNS 混淆地址，不可用
    const rank = {'srflx': 3, 'prflx': 2, 'host': 1};
    final r = rank[typ] ?? 0;
    if (r == 0) return;
    final cur = rank[remoteIpType] ?? 0;
    if (remoteIp == null || r > cur) {
      remoteIp = ip;
      remoteIpType = typ;
      _log('捕获被控地址: $ip ($typ)');
    }
  }

  Future<void> _onRemoteCandidate(Map<String, dynamic> d) async {
    if (d['receiver'] != mySocketId) return;
    final c = d['candidate'];
    if (c is! Map) return;
    final cand = c['candidate'] as String? ?? '';
    _log('收到远端 candidate: ${cand.length > 60 ? cand.substring(0, 60) : cand}');
    _captureRemoteIpFromCandidate(cand);
    if (pc == null) {
      _log('尚无 PC，丢弃 candidate');
      return;
    }
    try {
      await pc!.addCandidate(RTCIceCandidate(
        c['candidate'] as String?,
        c['sdpMid'] as String?,
        (c['sdpMLineIndex'] as num?)?.toInt(),
      ));
    } catch (e) {
      _log('addCandidate 失败: $e');
    }
  }

  // ------------------------------------------------- 首帧探针 / TURN 预检

  /// [20261022 黑屏] 等真正的视频数据到来才宣布「画面就绪」。
  ///
  /// 背景（2026-10-22 实锤）：主控连被控，`onTrack` 在 setRemoteDescription
  /// 时（answer 都还没发）就触发 → 旧逻辑立刻 fireVideoReady → 连接页百分比
  /// 冲到 100% 进画面页；而 ICE 一对候选都没通（对端只给了一个 host 候选
  /// 192.168.2.224，本机在 192.168.1.x，TURN 3478 又是死的）→ 一帧都收不到
  /// → 用户看到的就是「100% 之后一直黑屏」。日志证据：`[remote] video=0x0`。
  ///
  /// 现在：每 400ms 查一次 inbound-rtp，收到字节/解出帧才算就绪；
  /// 14s 还没有 → 明确报错（不再默默黑屏）。getStats 本身不可用时退回
  /// 旧的「6s 后放行」，防止某个 libwebrtc 版本把这里变成新的卡死点。
  void _startMediaProbe() {
    if (_mediaProbe != null || _disposed) return;
    _mediaProbeTries = 0;
    _weakTries = 0;
    _statsErrCount = 0;
    _log('[media] 开始等首帧（onTrack 已到，验证媒体面是否真的通）');
    _mediaProbe = Timer.periodic(const Duration(milliseconds: 400), (t) async {
      if (_disposed || pc == null) {
        t.cancel();
        _mediaProbe = null;
        return;
      }
      _mediaProbeTries++;
      try {
        var got = false;
        var framesSeen = 0;
        var detail = '';
        for (final rep in await pc!.getStats()) {
          final v = rep.values;
          if (rep.type != 'inbound-rtp') continue;
          final kind = v['kind'] ?? v['mediaType'];
          if (kind != 'video') continue;
          final frames = (v['framesDecoded'] as num?)?.toInt() ?? 0;
          final bytes = (v['bytesReceived'] as num?)?.toDouble() ?? 0;
          if (frames > 0 || bytes > 0) {
            got = true;
            if (frames > framesSeen) framesSeen = frames;
            final w = (v['frameWidth'] as num?)?.toInt() ?? 0;
            final h = (v['frameHeight'] as num?)?.toInt() ?? 0;
            detail = 'frames=$frames bytes=${bytes.toInt()} ${w}x$h';
          }
        }
        // 判据优先级：解出帧（framesDecoded>0）最硬；只有字节没有帧时
        // 说明 RTP 到了但解码没起来（对端采集停摆/锁屏的典型形态），
        // 再等 3.2s 仍无帧才按「弱信号」放行，避免把用户永久卡在 80%。
        if (got && framesSeen == 0) {
          _weakTries++;
          if (_weakTries == 1) _log('[media] 收到 RTP 但未解出帧（$detail），继续等');
        }
        if (got && (framesSeen > 0 || _weakTries >= 8)) {
          t.cancel();
          _mediaProbe = null;
          _log('[media] 首帧已到达（$detail）→ 画面就绪');
          if (remoteStream == null && !_remoteStreamCtrl.isClosed) {
            _log('[media] 注意：已有媒体但 remoteStream 为空（onTrack 未给流）');
          }
          _fireVideoReady();
          return;
        }
        _statsErrCount = 0;
        if (_mediaProbeTries % 5 == 0) {
          _log('[media] 等首帧 ${(_mediaProbeTries * 400 / 1000).toStringAsFixed(1)}s '
              '无 inbound-rtp 数据（ICE=${_iceConnected ? "已通" : "未通"}）');
        }
        if (_mediaProbeTries >= 35) {
          t.cancel();
          _mediaProbe = null;
          _log('[media] 14s 未收到任何视频数据 → 判定媒体面不通');
          if (!_disposed && !_errorCtrl.isClosed) {
            _errorCtrl.add(_iceConnected
                ? '已连上但收不到画面：对方未出帧（可能锁屏/采集失败）'
                : '已连上但画面通道不通：TURN(3478) 不可用或双方网络无法互通');
          }
        }
      } catch (e) {
        _statsErrCount++;
        if (_statsErrCount >= 5) {
          t.cancel();
          _mediaProbe = null;
          _log('[media] getStats 连续失败（$e）→ 退回旧判据直接放行');
          _fireVideoReady();
        }
      }
    });
  }

  /// [20261022] TURN/STUN 预检：直连不通时全靠它中转。3478 一死，跨网段的
  /// 两端（如 192.168.1.x ↔ 192.168.2.x）必然黑屏 —— 提前留一条硬证据。
  Future<void> _probeTurnServer() async {
    _log('[turn] 预检开始 url=${TurnConfig.url}');
    RawDatagramSocket? sock;
    try {
      final uri = TurnConfig.url.replaceFirst('turn:', '');
      final hostPart = uri.split('?').first;
      final idx = hostPart.lastIndexOf(':');
      final host = idx > 0 ? hostPart.substring(0, idx) : hostPart;
      final port = idx > 0 ? (int.tryParse(hostPart.substring(idx + 1)) ?? 3478) : 3478;
      sock = await RawDatagramSocket.bind(InternetAddress.anyIPv4, 0);
      final tid = List<int>.generate(12, (i) => math.Random().nextInt(256));
      final req = <int>[
        0x00, 0x01, 0x00, 0x00, // Binding Request, length 0
        0x21, 0x12, 0xA4, 0x42, // magic cookie
        ...tid,
      ];
      sock.send(req, InternetAddress(host), port);
      final done = Completer<bool>();
      late final StreamSubscription<RawSocketEvent> sub;
      sub = sock.listen((ev) {
        if (ev == RawSocketEvent.read && !done.isCompleted) done.complete(true);
      });
      final ok = await done.future.timeout(const Duration(milliseconds: 1500),
          onTimeout: () => false);
      await sub.cancel();
      _log(ok
          ? '[turn] STUN 预检通过：$host:$port 有响应'
          : '[turn] ⚠️ STUN/TURN 不可达：$host:$port 无响应（跨网段连接必黑屏，'
              '需在服务器启动 turn-coturn\\run-coturn.bat）');
    } catch (e) {
      _log('[turn] STUN 预检异常: $e');
    } finally {
      try {
        sock?.close();
      } catch (_) {}
    }
  }

  // ------------------------------------------------------------- H264 直送

  // ------------------------------------------- 剪贴板同步（文本 + 文件，双向）

  /// 收发约定（与 receiver_session 完全对称，改一边必须改另一边）：
  /// - MessageChannel 文本帧 `{msgType:'clipText', data:{text,seq}}`
  /// - FileChannel 文本帧 `fileOffer` / `fileDone`
  /// - FileChannel 二进制帧 `[0]=0x1D | [1..2]=idx | [3..6]=offset | payload`
  void _bindClipText(RTCDataChannel dc) {
    dc.onMessage = (RTCDataChannelMessage msg) {
      if (_disposed || msg.isBinary) return;
      final m = _decodeJson(msg.text);
      if (m == null) return;
      if (m['msgType'] == 'clipText') _ensureClip()?.handleCtrlText(m);
    };
  }

  void _bindFileRecv(RTCDataChannel dc) {
    dc.onMessage = (RTCDataChannelMessage msg) {
      if (_disposed) return;
      final clip = _ensureClip();
      if (clip == null) return;
      if (msg.isBinary) {
        final b = msg.binary;
        if (b is Uint8List && b.isNotEmpty) clip.handleFileBinary(b);
      } else {
        final m = _decodeJson(msg.text);
        if (m != null) clip.handleFileText(m);
      }
    };
  }

  Map<String, dynamic>? _decodeJson(String s) {
    try {
      final v = jsonDecode(s);
      if (v is Map<String, dynamic>) return v;
    } catch (_) {}
    return null;
  }

  /// 第一次需要时才建（没有会话就不占资源）。事件转发到 [clipEvents]。
  ClipSync? _ensureClip() {
    if (_disposed) return null;
    return _clip ??= ClipSync(
      sendCtrl: (json) => _dcSendText(_dcCtrl, json),
      sendFileCtrl: (json) => _dcSendText(_dcFile, json),
      sendFileBin: (bin) => _dcSendBin(_dcFile, bin),
      onEvent: (e) {
        if (!_clipCtrl.isClosed) _clipCtrl.add(e);
      },
    );
  }

  bool _dcSendText(RTCDataChannel? dc, String json) {
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

  /// 绑定被控端视频通道：二进制 → 组帧器；文本 → 时钟探针应答。
  /// 被控端 bindChannelHandlers 对通道文本帧的处理（remote/index.vue）：
  /// {msgType:'h264ClockPing'} 每秒一条（冻结心跳），必须回 pong，
  /// 否则被控端视为主控死亡（对齐 web attachVideoSink 同款应答）。
  void _bindVideoChannel(RTCDataChannel dc) {
    _dcVideo = dc;
    h264.lastCtrlAt = DateTime.now();
    // [20261019j] 时钟对齐探针：1s/条发 h264ClockPing（t1=本机 epoch），
    // 被控端 bindChannelHandlers 回 pong{t1,t2=被控 performance.now()}。
    // 三四个往返后钟差就绪，数据栏才能显示真端到端延迟。
    _clockPingTimer?.cancel();
    _clockPingTimer = Timer.periodic(const Duration(seconds: 1), (_) {
      final v = _dcVideo;
      if (v == null ||
          v.state != RTCDataChannelState.RTCDataChannelOpen) {
        return;
      }
      try {
        v.send(RTCDataChannelMessage(jsonEncode(<String, dynamic>{
          'msgType': 'h264ClockPing',
          'requestId': 'clock',
          'data': <String, dynamic>{
            't1': DateTime.now().millisecondsSinceEpoch,
          },
        })));
      } catch (_) {}
    });
    h264.onNeedKeyframe = () {
      _dcTry(_dcCtrl, 'h264Keyframe', {});
    };
    dc.onMessage = (msg) {
      if (_disposed) return;
      if (msg.isBinary) {
        final bin = msg.binary;
        if (bin is Uint8List && bin.isNotEmpty) {
          h264.onPacket(bin);
          // [20261020] H264 直送首帧 = 视频就绪（无轨道，替代 onTrack）
          if (!_videoReadyFired && h264.gotFirstFrame) _fireVideoReady();
        }
        return;
      }
      final text = msg.text;
      if (text.isEmpty) return;
      h264.lastCtrlAt = DateTime.now();
      Map<String, dynamic>? m;
      try {
        final v = jsonDecode(text);
        if (v is Map<String, dynamic>) m = v;
      } catch (_) {}
      if (m != null && m['msgType'] == 'h264ClockPing') {
        try {
          dc.send(RTCDataChannelMessage(jsonEncode(<String, dynamic>{
            'msgType': 'h264ClockPong',
            'requestId': m['requestId'] ?? 'clock',
            'data': <String, dynamic>{
              't1': (m['data'] as Map?)?['t1'],
              't2': DateTime.now().millisecondsSinceEpoch,
            },
          })));
        } catch (_) {}
      } else if (m != null && m['msgType'] == 'h264ClockPong') {
        // [20261019j] 自己发的探针被被控端回回来了 → 喂钟差估计器。
        final d = m['data'];
        if (d is Map) {
          h264.clock.addSample(
            (d['t1'] as num?)?.toInt() ?? 0,
            (d['t2'] as num?)?.toInt() ?? 0,
            DateTime.now().millisecondsSinceEpoch,
          );
        }
      } else if (m != null && m['msgType'] == 'h264Congest') {
        // [20261019j] 被控冻结心跳：透出真实发送 fps / 积压 / 冻结时长，
        // 数据栏据此显示「拥塞」，别再让冻结看着像正常。
        final d = m['data'];
        if (d is Map) {
          h264.ctrlSendFps = (d['fps'] as num?)?.toInt() ?? 0;
          h264.ctrlBufferedKB = (d['bufferedKB'] as num?)?.toInt() ?? 0;
          h264.ctrlFrozenMs = (d['frozenMs'] as num?)?.toInt() ?? 0;
          h264.lastCongestAt = DateTime.now();
        }
      }
      // 其余文本（剪贴板同步等）暂不消费
    };
  }

  /// PC 连接后握手：原生解码就绪才发 h264Ready（发早了被控会停掉 JPEG
  /// 兼容链而我们解不出画面 = 黑屏）。原生不可用则永远不发，静默走老链。
  Future<void> _startH264Handshake() async {
    if (_disposed || _h264ReadyTimer != null) return;
    // [20261021 九轮] 已有标准视频轨 → 直送无意义（旧路径正是"糊+点不动"的根因）
    if (_trackVideoUp) {
      _log('已有远端视频轨，不启动 H264 直送握手');
      return;
    }
    _log('H264 握手开始（原生解码 init…）');
    final ok = await h264.init();
    if (!ok || _disposed) return; // init 失败：保持 JPEG 兼容链（现状）
    _h264FailCount = 0;
    var tries = 0;
    // [20261019k] 500→200ms：握手重试更密，压缩"进度条走完 → 首帧"的黑屏窗口
    _h264ReadyTimer = Timer.periodic(const Duration(milliseconds: 200), (t) {
      if (_disposed) {
        t.cancel();
        return;
      }
      tries += 1;
      if (tries > 40) {
        t.cancel();
        _h264ReadyTimer = null;
        return;
      }
      if (_dcTry(_dcCtrl, 'h264Ready', {})) {
        t.cancel();
        _h264ReadyTimer = null;
        _h264ReadyAt = DateTime.now();
        _log('已请求被控切换 H264 直送');
        _startH264Watchdog();
      }
    });
  }

  /// 直送看门狗（对齐 web revokeH264 的健康语义，判据从"通道状态"换成
  /// "帧/心跳内容"，因为 captureStream 类通道永远报健康）：
  /// ① 发了 ready 6s 仍无首帧 → revoke；② 直送活着但 12s 无帧且无心跳 →
  /// revoke（冻结心跳每秒一条，没心跳说明直送已死而非拥塞）。
  void _startH264Watchdog() {
    _h264Watchdog?.cancel();
    _h264Watchdog = Timer.periodic(const Duration(seconds: 2), (t) {
      if (_disposed) {
        t.cancel();
        return;
      }
      final now = DateTime.now();
      if (!h264.gotFirstFrame && _h264ReadyAt != null) {
        // [20261019f] 每次留痕收包数：区分"通道没来包"与"包来了没出帧"。
        _log('[h264] 看门狗 packets=${h264.packets} '
            'submitted=${h264.submitted}');
        if (now.difference(_h264ReadyAt!).inSeconds >= 6) {
          _revokeH264('6s 无首帧');
        }
        return;
      }
      if (h264.gotFirstFrame) {
        final idle = now.difference(h264.lastActivityAt ?? now).inSeconds;
        if (idle >= 12) {
          _revokeH264('12s 无帧且无心跳');
        }
      }
    });
  }

  /// 撤销直送：通知被控恢复 JPEG+WebRTC 兼容链（被控端 10 分钟内忽略新
  /// h264Ready，天然防死循环），UI 切回视频轨。
  void _revokeH264(String reason) {
    _h264Watchdog?.cancel();
    _h264Watchdog = null;
    _h264ReadyTimer?.cancel();
    _h264ReadyTimer = null;
    _h264ReadyAt = null;
    _h264FailCount += 1;
    h264.reset();
    _log('H264 直送撤销（$reason），失败计数=$_h264FailCount');
    if (!_h264StopSent) {
      _h264StopSent = true;
      _dcTry(_dcCtrl, 'h264_stop', {});
      _log('已通知被控端 h264_stop（恢复 JPEG 兼容通道）');
    }
    // [20261021 九轮] 已经有视频轨 = 画面本来就在走 libwebrtc，撤销直送后
    // 绝不能按老逻辑"8s 后重试握手"，否则会陷入 revoke→重试 的死循环。
    if (_trackVideoUp) {
      _log('已有远端视频轨，停止 H264 直送重试');
      return;
    }
    // [20261019 Flutter被控] Flutter 被控端没有 JPEG 兜底轨道
    // （flutter_webrtc Windows 无取屏能力，H264 直送是唯一画面源）——
    // 撤销后不能干等"恢复的 JPEG"，8s 后重试握手（上限 8 次，防死循环）。
    if (!_disposed && _h264FailCount <= 8) {
      Future.delayed(const Duration(seconds: 8), () {
        if (_disposed || pc == null || _h264ReadyTimer != null) return;
        _h264StopSent = false;
        h264.reset();
        _log('H264 握手重试（第 $_h264FailCount 次撤销后）');
        _startH264Handshake();
      });
    }
  }

  // ---------------------------------------------------------------- 输入

  /// DataChannel 发送（信封与 web dataChannelSend 完全同构：{msgType,
  /// requestId, data}；被控端 bindChannelHandlers 按 msgType 分发）。
  bool _dcTry(RTCDataChannel? dc, String msgType, Map<String, dynamic> data) {
    if (dc == null ||
        dc.state != RTCDataChannelState.RTCDataChannelOpen) {
      return false;
    }
    try {
      dc.send(RTCDataChannelMessage(jsonEncode(<String, dynamic>{
        'msgType': msgType,
        'requestId': randomRequestId(),
        'data': data,
      })));
      return true;
    } catch (_) {
      return false;
    }
  }

  /// web 的 computeXY 产物：x/y ∈ [0,1000]，相对画面内容矩形。
  /// [20261019 输入直连] 优先 DataChannel 点对点（移动帧走不可靠的
  /// MouseChannel，其余走可靠的 MessageChannel），通道未就绪回退信令中继。
  ///
  /// [20261019l 移动帧双发] —— 拖拽秒级迟滞的第二个根因（对齐 web v50）：
  /// web 主控的移动帧是 **DC + WS 中继双发**（血泪教训 v50：DC 存在"假通道
  /// 黑洞"——本端 readyState=open 但对端没有消费者；被控端 ondatachannel
  /// 事件丢失时 MouseChannel 必成假通道）→ 移动帧整批蒸发，症状正是
  /// 「按下有效（点击走可靠 MessageChannel）、拖拽窗口不动、点哪都不对位」。
  /// flutter 端此前只发 DC 单腿，恰好踩中。现对齐 web：移动帧 DC 之外**必走
  /// WS 可靠腿**（33ms 限流 ≈30fps，约 6KB/s），被控按 _rid 去重不重注；
  /// 点击/键盘保持 DC 单发（web 实测双发会把单击合成双击，禁止）。
  void _behavior(int type, int x, int y, {Object? key, int amount = 0}) {
    if (type == 2 || type == 4) {
      ShellLog.w('[input] send type=$type x=$x y=$y');
    }
    final rid = randomRequestId();
    final data = <String, dynamic>{
      'type': type,
      'key': key ?? <int>[0],
      'x': x,
      'y': y,
      'amount': amount,
      'roomId': deskUserUuid,
      '_rid': rid, // [20261019l] 被控端 dc+ws 双发去重键（对齐 web v50）
    };
    final isMove = type == DeskBehavior.mouseMove ||
        type == DeskBehavior.mouseDrag;
    final ok = isMove
        ? (_dcTry(_dcMouse, WsMsg.deskBehavior, data) ||
            _dcTry(_dcCtrl, WsMsg.deskBehavior, data))
        : _dcTry(_dcCtrl, WsMsg.deskBehavior, data);
    if (!ok) {
      // 信令中继兜底（DC 未开/发送失败）
      signaling.send(WsMsg.deskBehavior, <String, dynamic>{
        'roomId': deskUserUuid,
        'sender': mySocketId,
        'receiver': peerSocketId,
        'type': type,
        'key': key ?? <int>[0],
        'x': x,
        'y': y,
        'amount': amount,
        '_rid': rid,
      });
      return;
    }
    // [20261019l] 移动帧 WS 可靠腿（v50 双发），33ms 限流。
    if (isMove) {
      final now = DateTime.now().millisecondsSinceEpoch;
      if (now - _lastWsMoveAt >= 33) {
        _lastWsMoveAt = now;
        signaling.send(WsMsg.deskBehavior, <String, dynamic>{
          'roomId': deskUserUuid,
          'sender': mySocketId,
          'receiver': peerSocketId,
          'type': type,
          'key': key ?? <int>[0],
          'x': x,
          'y': y,
          'amount': amount,
          '_rid': rid,
        });
      }
    }
  }

  /// [20261019l] 移动帧 WS 腿限流（对齐 web lastWsMoveSentAt/33ms）。
  int _lastWsMoveAt = 0;

  void mouseMove(int x, int y) => _behavior(DeskBehavior.mouseMove, x, y);
  void mouseDrag(int x, int y) => _behavior(DeskBehavior.mouseDrag, x, y);

  /// [20261017c] 点击带上最后已知坐标：web 版被控端对 pressButton 有
  /// 「指令自带坐标则先定位再按下」的自愈逻辑——mouseMove 一旦丢失/未绑定，
  /// 不带坐标的点击就落在旧位置（症状：点什么都没反应）。
  void mouseDown({required bool right, int x = 0, int y = 0}) => _behavior(
      right ? DeskBehavior.pressButtonRight : DeskBehavior.pressButtonLeft,
      x,
      y);

  void mouseUp({required bool right, int x = 0, int y = 0}) => _behavior(
      right ? DeskBehavior.releaseButtonRight : DeskBehavior.releaseButtonLeft,
      x,
      y);

  /// [20261018] 右键 1:1 对齐 web 主控：contextmenu → 一次 rightClick(9)。
  /// press/releaseButtonRight(3/5) 链路被控端实测右键无反应，弃用。
  void rightClick({int x = 0, int y = 0}) =>
      _behavior(DeskBehavior.rightClick, x, y);

  void doubleClick() => _behavior(DeskBehavior.doubleClick, 0, 0);

  /// 滚轮：ticks 正=下滚 负=上滚；amount=格数夹 1~10（对齐 wheelTickAmount）
  void scroll({required int ticks, int x = 0, int y = 0}) {
    final amount = math.max(1, math.min(10, ticks.abs()));
    _behavior(ticks > 0 ? DeskBehavior.scrollDown : DeskBehavior.scrollUp, x, y,
        amount: amount);
  }

  /// 键盘（nut 键码数组）
  void keyDown(int nut) =>
      _behavior(DeskBehavior.keyboardPressKey, 0, 0, key: <int>[nut]);
  void keyUp(int nut) =>
      _behavior(DeskBehavior.keyboardReleaseKey, 0, 0, key: <int>[nut]);

  /// 整段文本上屏（输入法，key 传字符串）
  void typeText(String text) =>
      _behavior(DeskBehavior.keyboardType, 0, 0, key: text);

  void releaseAllModifiers() => _behavior(DeskBehavior.releaseAllModifiers, 0, 0);
  void sendCtrlAltDel() => _behavior(DeskBehavior.sendCtrlAltDel, 0, 0);
  void lockScreen() => _behavior(DeskBehavior.lockScreen, 0, 0);
  void openTaskManager() => _behavior(DeskBehavior.openTaskManager, 0, 0);
  void privacyScreen(bool on) =>
      _behavior(on ? DeskBehavior.privacyScreenOn : DeskBehavior.privacyScreenOff, 0, 0);

  // ------------------------------------------------------------- 画质档位（对齐上游）

  /// 改码率上限（kbps）。[20261019] 优先 DataChannel 直连（对齐 web 主控
  /// index.vue 的 dataChannelSend 路径），未就绪回退信令中继。
  void changeMaxBitrate(int kbps) {
    final ok = _dcTry(_dcCtrl, WsMsg.changeMaxBitrate, {'val': kbps});
    if (!ok) {
      signaling.send(WsMsg.changeMaxBitrate, <String, dynamic>{
        'live_room_id': deskUserUuid,
        'val': kbps,
      });
    }
    _log('画质档位: 码率 ${kbps}kbps ${ok ? '(DC直连)' : '(信令中继)'}');
  }

  /// 改最大帧率。上游被控端：handlConstraints(frameRate: val)（智能降帧联动退出）。
  void changeMaxFramerate(int fps) {
    final ok = _dcTry(_dcCtrl, WsMsg.changeMaxFramerate, {'val': fps});
    if (!ok) {
      signaling.send(WsMsg.changeMaxFramerate, <String, dynamic>{
        'live_room_id': deskUserUuid,
        'val': fps,
      });
    }
    _log('画质档位: 帧率 ${fps}fps ${ok ? '(DC直连)' : '(信令中继)'}');
  }

  /// 改分辨率档位（上游语义：高度值；静默取屏档位联动 core 参数）。
  void changeResolutionRatio(int height) {
    final ok = _dcTry(_dcCtrl, WsMsg.changeResolutionRatio, {'val': height});
    if (!ok) {
      signaling.send(WsMsg.changeResolutionRatio, <String, dynamic>{
        'live_room_id': deskUserUuid,
        'val': height,
      });
    }
    _log('画质档位: 分辨率 ${height}p ${ok ? '(DC直连)' : '(信令中继)'}');
  }

  // ---------------------------------------------------------------- 收尾

  Future<void> _teardownPc() async {
    try {
      await pc?.close();
    } catch (_) {}
    pc = null;
    remoteStream = null;
    _mediaProbe?.cancel();
    _mediaProbe = null;
    // [20261019] DC 随 PC 关闭；置空防旧通道残留被当成可用链路
    _dcCtrl = null;
    _dcMouse = null;
    _dcVideo = null;
    _dcFile = null;
    // [20261022] 剪贴板轮询跟着停（实例保留，重连复用）
    _clip?.stop();
    _clockPingTimer?.cancel();
    _clockPingTimer = null;
    // [20261019e] 直送状态机复位：组帧残帧清空、看门狗停、计时归零。
    // （不解绑原生纹理 —— 会话重连时直接复用，避免重复 init。）
    _h264ReadyTimer?.cancel();
    _h264ReadyTimer = null;
    _h264Watchdog?.cancel();
    _h264Watchdog = null;
    _h264ReadyAt = null;
    h264.reset();
  }

  void close({bool notifyRemote = true}) {
    if (_disposed) return;
    if (notifyRemote) {
      signaling.send(WsMsg.deskCloseRemote, <String, dynamic>{
        'roomId': deskUserUuid,
        'sender': mySocketId,
        'receiver': peerSocketId,
      });
    }
    _teardownPc();
    _setState(SessionState.closed);
    _myRoomTimer?.cancel();
    _myRoomTimer = null;
  }

  void dispose() {
    // ⚠️ 顺序不能反：close() 开头检查 _disposed，先置位会让 close 变空操作
    //（PC 不被 close、deskCloseRemote 不发，20261015 实锤）。
    close(notifyRemote: true);
    _disposed = true;
    _onlineTimer?.cancel();
    _myRoomTimer?.cancel();
    _clockPingTimer?.cancel();
    _clockPingTimer = null;
    _msgSub?.cancel();
    // [20261019e] 释放原生解码纹理（本会话对象生命周期终点）。
    h264.dispose();
    // [20261022] 剪贴板同步随会话销毁（轮询停 + 未写完的接收文件句柄关闭）
    _clip?.dispose();
    _clip = null;
    if (!_clipCtrl.isClosed) _clipCtrl.close();
    signaling.stopHeartbeat();
    _stateCtrl.close();
    _remoteStreamCtrl.close();
    _videoReadyCtrl.close();
    _errorCtrl.close();
    _logCtrl.close();
  }
}
