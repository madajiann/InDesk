/// 主控端连接页 —— 输入被控端控制码 + 验证码，发起远控。
/// 对齐 web 首页「输入控制码」入口的 deskJoin / deskStartRemote 时序。
/// [20261017c] 新增：记住密码（勾选后连接成功写入 RecentStore，下次同码
/// 自动回填）；密码框聚焦即禁系统 IME（打字直接英文上屏，无需切换输入法）。
import 'dart:async';

import 'package:flutter/material.dart';

import '../core/recent.dart';
import '../core/shell_log.dart';
import '../core/prefs.dart';
import '../core/window_channel.dart';
import '../signal/identity.dart';
import '../signal/protocol.dart';
import '../signal/signaling_client.dart';
import '../webrtc/controller_session.dart';
import 'remote_page.dart';
import 'widgets/input_formatters.dart';

class ConnectPage extends StatefulWidget {
  const ConnectPage({
    super.key,
    this.prefillCode = '',
    this.prefillPassword = '',
  });

  final String prefillCode;
  final String prefillPassword;

  @override
  State<ConnectPage> createState() => _ConnectPageState();
}

class _ConnectPageState extends State<ConnectPage> {
  late final _codeCtrl = TextEditingController(text: widget.prefillCode);
  // [20261017b] 密码框挂兜底清洗：IME 组词上屏绕过 inputFormatters 时，
  // controller 监听仍会把汉字滤掉、字母强制小写。
  late final _pwdCtrl =
      sanitizePasswordController(TextEditingController(text: widget.prefillPassword));
  // [20261017c] 聚焦禁 IME：根治「不切输入法打出来的是拼音/汉字」
  late final _pwdFocus = createNoImeFocusNode();
  SignalingClient? _signal;
  ControllerSession? _session;
  String _status = '未连接';
  bool _busy = false;
  bool _navigated = false;
  // [20261017c] 记住密码：默认勾选（连接成功后按勾选落盘）
  bool _remember = true;
  // [20261018] 连接进度（对齐旧版「连接百分比」画面）：
  // [20261019] 改为**时间驱动**平滑动画：事件只抬高目标值，显示值逐拍缓升，
  // 全程最短 ~2.4s —— 修复「连接太快百分比动画一闪而过/根本没看到」。
  double _progress = 0;
  Timer? _anim;
  DateTime _t0 = DateTime.now();
  // 收到远端流（视频就绪），百分比爬到 100% 且最短展示时间过后才进画面页
  bool _videoReady = false;

  /// 展示进度的阶段目标（未收到画面时随时间解锁）
  double _stageTarget(double elapsed) {
    if (elapsed < 0.5) return 0.08;
    if (elapsed < 1.2) return 0.30;
    if (elapsed < 2.2) return 0.55;
    return 0.80;
  }

  /// 动画心跳：显示值向目标缓升；视频就绪 + 最短展示时间到 → 进画面页
  void _tick() {
    if (!mounted || _navigated) {
      _anim?.cancel();
      _anim = null;
      return;
    }
    final t = DateTime.now().difference(_t0).inMilliseconds / 1000;
    final target = _videoReady ? 1.0 : _stageTarget(t);
    _progress += (target - _progress) * 0.15;
    if (_videoReady && _progress > 0.985) _progress = 1.0;
    _progress = _progress.clamp(0.0, 1.0);
    if (_videoReady && t >= 2.4 && _progress >= 0.98) {
      _navigated = true;
      _anim?.cancel();
      _anim = null;
      final session = _session;
      if (session != null && mounted) {
        ShellLog.w('[connect] 百分比完成，进画面页');
        Navigator.of(context).pushReplacement(MaterialPageRoute<void>(
          builder: (_) => RemotePage(session: session),
        ));
      }
      return;
    }
    setState(() {});
  }

  @override
  void initState() {
    super.initState();
    // 自动连接（--connect 参数直填时）
    if (widget.prefillCode.isNotEmpty && widget.prefillPassword.isNotEmpty) {
      Future.microtask(_connect);
    }
  }

  @override
  void dispose() {
    _anim?.cancel();
    _codeCtrl.dispose();
    disposeNoImeFocusNode(_pwdFocus);
    _pwdCtrl.dispose();
    // ⚠️ 所有权已转交 RemotePage（pushReplacement 后本页 dispose 会被调用）。
    // 20261015 实锤：这里 dispose 掉 session/signal 会把信令断开
    //（disconnect reason=io client disconnect）→ candidate 断流 → ICE 永远连不上。
    // 统一由 RemotePage.dispose 负责 session + signaling 的销毁。
    _session = null;
    _signal = null;
    super.dispose();
  }

  /// 连接成功（收到远端流）时按勾选记住/清除密码
  void _savePassword() {
    final code = _codeCtrl.text.trim();
    final pwd = _pwdCtrl.text.trim();
    if (code.isEmpty) return;
    if (_remember && pwd.isNotEmpty) {
      RecentStore.addSync(code, password: pwd);
      ShellLog.w('[connect] 已记住设备 $code 的连接密码');
    } else {
      RecentStore.addSync(code);
      RecentStore.forgetPassword(code);
    }
  }

  Future<void> _connect() async {
    final code = _codeCtrl.text.trim();
    final pwd = _pwdCtrl.text.trim();
    if (code.isEmpty || pwd.isEmpty) {
      _toast('请输入控制码和验证码');
      return;
    }
    setState(() {
      _busy = true;
      _progress = 0.04;
      _status = '正在连接信令服务器…';
    });
    // [20261018] 点连接的瞬间窗口就撑大到会话尺寸（用户要求：先变大再播
    // 百分比动画，画面出来后不再有窗口跳变）。失败/取消由对应分支还原。
    ensureSessionWindowSize();
    // [20261019] 启动时间驱动的百分比动画（最短 ~2.4s，到 100% 才切画面页）
    _t0 = DateTime.now();
    _videoReady = false;
    _progress = 0.04;
    _anim?.cancel();
    _anim = Timer.periodic(const Duration(milliseconds: 60), (_) => _tick());
    ShellLog.w('[connect] 发起连接 code=$code');

    // 主控自己的身份（服务端 deskStartRemote 强校验 + Redis 在线键）
    final my = await ensureControllerIdentity();
    if (my == null) {
      if (!mounted) return;
      _anim?.cancel();
      restoreMainWindowSize();
      setState(() {
        _busy = false;
        _status = '创建主控身份失败（服务端不通？）';
      });
      return;
    }
    ShellLog.w('[connect] 主控身份 uuid=${my.uuid}');

    final signal = SignalingClient(url: kSignalingUrl);
    _signal = signal;
    signal.subscribe(WsMsg.deskJoined);
    signal.subscribe(WsMsg.deskStartRemoteResult);
    signal.subscribe(WsMsg.nativeWebRtcOffer);
    signal.subscribe(WsMsg.nativeWebRtcCandidate);
    signal.subscribe(WsMsg.deskCloseRemote);

    // 连接超时保护
    final watchdog = Timer(const Duration(seconds: 12), () {
      if (_busy && mounted) {
        _anim?.cancel();
        restoreMainWindowSize();
        setState(() {
          _busy = false;
          _status = '连接超时（信令服务器不通？）';
        });
      }
    });

    late final StreamSubscription<void> sub;
    sub = signal.onConnected.listen((_) {
      watchdog.cancel();
      if (!mounted) return;
      ShellLog.w('[connect] 信令已连上 socketId=${signal.socketId}');
      setState(() {
        _progress = 0.30;
        _status = '已连接服务器，正在发起控制…';
      });

      final session = ControllerSession(
        signaling: signal,
        deskUserUuid: code,
        deskUserPassword: pwd,
        myUuid: my.uuid,
        myPassword: my.password,
        // [20261017b] 模糊+尺寸不对根因：原默认 resolutionRatio=100 被被控
        // 当成「抓屏高度 100px」→ 宽度兜底下限 640 → 抓屏缩到 640x400。
        // 对齐 web 主控握手参数（高清档 = 原生≤1080 / 30fps / 8Mbps）。
        // [20261020] 握手必须走 sessionPreset() 档位标准值——散存值可能
        // 是旧版脏数据（如 hd 存了 12000），与 remote_page 下发的预设
        // 不一致会让被控判 cfgdrift 白白重启一次取流。
        maxBitrate: ShellPrefs.sessionPreset().kbps,
        maxFramerate: ShellPrefs.sessionPreset().fps,
        resolutionRatio: ShellPrefs.sessionPreset().res,
      );
      session.remoteCode = code;
      _session = session;

      session.videoReadyStream.listen((_) {
        // [20261020] 视频就绪（JPEG onTrack 或 H264 直送首帧）→ 标记就绪，
        // 导航交给 _tick（百分比爬满 + 最短展示时间后切换，保证用户一定
        // 看得到连接动画）。此前只听 remoteStreamStream——H264 直送没有
        // MediaStream 轨道 → 永远卡在「正在连接 80%」进不了画面页（实锤）。
        // ⚠️ 防重入：被控端 handleRTC 拆重建会发两次 offer → 重复置位幂等无害。
        if (_navigated) return;
        _videoReady = true;
        _savePassword();
        ShellLog.w('[connect] 视频就绪，等待百分比动画完成');
      });

      session.errorStream.listen((msg) {
        ShellLog.w('[connect] 会话错误: $msg');
        if (!mounted) return;
        _anim?.cancel();
        restoreMainWindowSize();
        setState(() {
          _busy = false;
          _status = msg;
        });
      });

      // [20261022 黑屏] 把媒体面进展直接显示在百分比页上：
      // 「等首帧 2.0s（ICE=未通）」这种状态比干等 80% 的黑屏有用得多。
      session.logStream.listen((line) {
        if (!mounted || _navigated) return;
        if (line.startsWith('[media]') || line.startsWith('[turn]')) {
          setState(() => _status = line);
        }
      });

      session.stateStream.listen((st) {
        if (!mounted) return;
        if (st == SessionState.negotiating) {
          setState(() {
            _progress = 0.55;
            _status = '正在协商媒体通道…';
          });
        } else if (st == SessionState.connected) {
          setState(() {
            _progress = 0.80;
            _status = '通道已建立，正在接收画面…';
          });
        }
      });

      session.start();
    });

    signal.onDisconnected.listen((_) {
      ShellLog.w('[connect] 信令断开');
      if (!mounted) return;
      setState(() => _status = '信令断开');
    });

    signal.connect();
  }

  void _toast(String s) {
    ScaffoldMessenger.of(context)
        .showSnackBar(SnackBar(content: Text(s), duration: const Duration(seconds: 2)));
  }

  /// [20261018] 用户主动取消连接：销毁会话+信令（未导航时所有权还在本页）
  void _cancelConnect() {
    _anim?.cancel();
    try {
      _session?.dispose();
    } catch (_) {}
    try {
      _signal?.dispose();
    } catch (_) {}
    _session = null;
    _signal = null;
    restoreMainWindowSize();
    if (!mounted) return;
    setState(() {
      _busy = false;
      _progress = 0;
      _status = '已取消连接';
    });
  }

  /// [20261018] 连接过渡页 —— 对齐旧版「连接百分比」画面：
  /// 品牌深蓝渐变 + 设备码 + 大号百分比 + 细进度条 + 状态行 + 取消。
  Widget _connectingView() {
    final code = _codeCtrl.text.trim();
    final pct = (_progress * 100).round();
    return Container(
      color: const Color(0xFF101B33),
      child: Center(
        child: Column(mainAxisAlignment: MainAxisAlignment.center, children: [
          Container(
            width: 52,
            height: 52,
            alignment: Alignment.center,
            decoration: BoxDecoration(
              color: const Color(0xFF2F6FED).withOpacity(.18),
              shape: BoxShape.circle,
            ),
            child: const Icon(Icons.desktop_windows_rounded,
                size: 28, color: Color(0xFF7FA6F5)),
          ),
          const SizedBox(height: 18),
          Text(code.isEmpty ? '正在建立连接' : '正在连接 $code',
              style: const TextStyle(
                  fontSize: 17,
                  fontWeight: FontWeight.w700,
                  color: Colors.white)),
          const SizedBox(height: 26),
          // 大号百分比（等宽字体，数字跳动不抖版式）
          Text('$pct%',
              style: const TextStyle(
                  fontFamily: 'Consolas',
                  fontSize: 44,
                  fontWeight: FontWeight.w700,
                  color: Colors.white,
                  height: 1.0)),
          const SizedBox(height: 18),
          SizedBox(
            width: 260,
            child: ClipRRect(
              borderRadius: BorderRadius.circular(3),
              child: LinearProgressIndicator(
                value: _progress.clamp(0.0, 1.0),
                minHeight: 5,
                backgroundColor: Colors.white.withOpacity(.14),
                valueColor:
                    const AlwaysStoppedAnimation<Color>(Color(0xFF2F6FED)),
              ),
            ),
          ),
          const SizedBox(height: 16),
          Text(_status,
              style: TextStyle(
                  fontSize: 12.5, color: Colors.white.withOpacity(.6))),
          const SizedBox(height: 28),
          TextButton(
            onPressed: _cancelConnect,
            child: Text('取消连接',
                style: TextStyle(
                    fontSize: 13, color: Colors.white.withOpacity(.75))),
          ),
        ]),
      ),
    );
  }

  @override
  Widget build(BuildContext context) {
    // [20261018] 连接中不再展示「设备+密码」表单页（用户反馈闪一下很难看），
    // 直接切全屏百分比连接画面；失败/超时自动回到表单页显示错误。
    if (_busy) {
      return Scaffold(body: _connectingView());
    }
    return Scaffold(
      appBar: AppBar(title: const Text('连接被控端')),
      body: Center(
        child: ConstrainedBox(
          constraints: const BoxConstraints(maxWidth: 420),
          child: Padding(
            padding: const EdgeInsets.all(24),
            child: Column(
              mainAxisSize: MainAxisSize.min,
              crossAxisAlignment: CrossAxisAlignment.stretch,
              children: [
                TextField(
                  controller: _codeCtrl,
                  enabled: !_busy,
                  decoration: const InputDecoration(
                    labelText: '控制码',
                    border: OutlineInputBorder(),
                  ),
                ),
                const SizedBox(height: 16),
                TextField(
                  controller: _pwdCtrl,
                  focusNode: _pwdFocus,
                  enabled: !_busy,
                  obscureText: true,
                  // [20261017] 字母强制小写 + 滤掉汉字/全角；
                  // [20261017c] 聚焦即禁 IME，打字直接英文上屏
                  inputFormatters: const [LowercaseAsciiFormatter()],
                  decoration: const InputDecoration(
                    labelText: '验证码',
                    border: OutlineInputBorder(),
                  ),
                  onSubmitted: (_) => _busy ? null : _connect(),
                ),
                const SizedBox(height: 6),
                // [20261017c] 记住密码
                CheckboxListTile(
                  value: _remember,
                  onChanged: _busy
                      ? null
                      : (v) => setState(() => _remember = v ?? true),
                  controlAffinity: ListTileControlAffinity.leading,
                  dense: true,
                  contentPadding: EdgeInsets.zero,
                  title: const Text('记住密码（下次连接该设备自动填入）',
                      style: TextStyle(fontSize: 13)),
                ),
                const SizedBox(height: 12),
                FilledButton(
                  onPressed: _busy ? null : _connect,
                  child: Padding(
                    padding: const EdgeInsets.symmetric(vertical: 12),
                    child: Text(_busy ? '连接中…' : '连接'),
                  ),
                ),
                const SizedBox(height: 16),
                Text(
                  _status,
                  textAlign: TextAlign.center,
                  style: TextStyle(
                      color: Theme.of(context).colorScheme.secondary,
                      fontSize: 13),
                ),
              ],
            ),
          ),
        ),
      ),
    );
  }
}
