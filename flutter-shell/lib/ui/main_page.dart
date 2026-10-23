/// 远程控制页（主区默认页）—— 板块 1:1 对齐 web 端 views/remote/index.vue PC 版：
/// ① 此设备卡：设备代码（复制/刷新重注册）+ 临时密码（显示/隐藏）
/// ② 无人值守行：无人值守 ☐（+共享状态）· 隐私屏 ☐ · 会话结束锁屏 ☐
/// ③ 远程控制设备卡：9 位码输入（下拉常用设备）+ 连接 + tip
/// ④ 连接画质卡：画质档位 / 分辨率 / 码率 / 帧率（连接前预设，进会话自动下发）
import 'dart:async';

import 'package:flutter/material.dart';
import 'package:flutter/services.dart';

import '../core/prefs.dart';
import '../core/recent.dart';
import '../core/device_probe.dart';
import '../core/shell_log.dart';
import '../api/user_api.dart';
import '../receiver/host_core.dart';
import '../receiver/host_identity.dart';
import '../receiver/receiver_session.dart';
import '../signal/identity.dart';
import '../signal/protocol.dart' show kSignalingUrl;
import '../signal/signaling_client.dart';
import 'connect_page.dart';
import 'widgets/input_formatters.dart';
import 'widgets/pressable.dart';

class MainPage extends StatefulWidget {
  const MainPage({super.key});

  @override
  State<MainPage> createState() => _MainPageState();
}

class _MainPageState extends State<MainPage> {
  static const brand = Color(0xFF2F6FED);

  ControllerIdentity? _identity;
  /// 被控真实身份（host_identity.json）——「本机设备代码」用它，
  /// 主控身份 _identity 只用于发起连接时的服务端校验
  ControllerIdentity? _hostIdentity;
  /// [20261019 被控去 WebView2] 被控会话（信令/WebRTC/输入/H264 全在壳内）
  ReceiverSession? _receiver;
  Timer? _refreshTimer;
  bool _hidePwd = true; // 对齐 web cacheStore.hidePwd 默认隐藏
  bool? _hostRunning;
  List<Map<String, dynamic>> _recent = [];
  bool _showRecentDropdown = false;
  final _connectCtrl = TextEditingController();

  // 连接画质预设
  String _tierId = ShellPrefs.defaultTier;

  @override
  void initState() {
    super.initState();
    _recent = RecentStore.loadSync();
    _tierId = ShellPrefs.tierId;
    _bootstrap();
  }

  Future<void> _bootstrap() async {
    final id = await ensureControllerIdentity();
    if (!mounted) return;
    setState(() {
      _identity = id;
      // [20261016] 被控真实身份优先（对方连本机用的是它）
      _hostIdentity = loadHostIdentity();
    });
    // [20261019 被控去 WebView2] 被控角色整体搬进壳：被控核 --core(headless，
    // 零 WebView2) + Dart 信令/WebRTC/H264 直送。旧 Tauri UI 形态退役。
    await _bootReceiver();
    if (!mounted) return;
    // 周期刷新：状态/身份轻量轮询（被控核意外重启、身份补齐后自动亮码）
    _refreshTimer = Timer.periodic(const Duration(seconds: 15), (_) {
      if (!mounted) return;
      _checkHost();
    });
  }

  /// 启动被控角色：核 → 身份 → 会话（幂等，可重复调用）
  Future<void> _bootReceiver() async {
    final hc = HostCore.shared;
    final coreOk = await hc.ensureStarted();
    // 身份直营：本地校验（/desk_user/login），失效就注册新设备码
    final r = await ensureHostIdentity();
    if (r.ok && r.identity != null) {
      if (mounted) setState(() => _hostIdentity = r.identity);
    } else {
      ShellLog.w('[remote] 被控身份注册失败: ${r.msg}');
    }
    if (coreOk && r.identity != null && _receiver == null && mounted) {
      final session = ReceiverSession(
        signal: SignalingClient(url: kSignalingUrl),
        bridge: hc.bridge,
        onStateChanged: (s) {
          if (mounted) setState(() {});
        },
        onIncomingRequest: _showIncomingDialog,
      );
      await session.start(r.identity!);
      _receiver = session;
    }
    if (mounted) setState(() => _hostRunning = coreOk || _receiver != null);
  }

  /// 主控请求连入 → 确认框（无人值守已在会话层自动放行，不会走到这）
  Future<void> _showIncomingDialog(ReceiverIncomingRequest req) async {
    final code = req.masterDeviceCode;
    final fmt = code.length == 9
        ? '${code.substring(0, 3)}-${code.substring(3, 6)}-${code.substring(6)}'
        : code;
    if (!mounted) return;
    final allow = await showDialog<bool>(
      context: context,
      barrierDismissible: false,
      builder: (ctx) => AlertDialog(
        title: const Text('远程连接请求'),
        content: Text('设备 $fmt 请求远程控制本机，是否允许？'),
        actions: [
          TextButton(
              onPressed: () => Navigator.pop(ctx, false),
              child: const Text('拒绝')),
          FilledButton(
              onPressed: () => Navigator.pop(ctx, true),
              child: const Text('允许')),
        ],
      ),
    );
    if (_receiver == null) return;
    if (allow == true) {
      await _receiver!.acceptIncoming(req);
    } else {
      await _receiver!.rejectIncoming(req);
    }
  }

  Future<void> _checkHost() async {
    if (!mounted) return;
    final running = HostCore.shared.running;
    setState(() {
      _hostRunning = running;
      _hostIdentity ??= loadHostIdentity();
    });
    // 被控核意外退出/从未起来：补偿拉起（ensureStarted 幂等，守护另带退避重启）
    if (!running) await _bootReceiver();
  }

  /// 「此设备」展示身份：**只认被控身份**（host_identity.json）。
  /// 被控没跑过就没有这个文件 —— 显示占位引导启动，
  /// 绝不拿主控身份冒充（那是另一条 desk_user 记录，对方连它永远无人应答）。
  ControllerIdentity? get _deviceIdentity => _hostIdentity;

  String get _codeFmt {
    final u = _deviceIdentity?.uuid ?? '';
    if (u.length == 9) {
      return '${u.substring(0, 3)}-${u.substring(3, 6)}-${u.substring(6)}';
    }
    return u;
  }

  /// 一次复制「设备代码 + 临时密码」（对齐 web handleCopyRemoteInfo）
  Future<void> _copyInfo() async {
    final id = _deviceIdentity;
    if (id == null) return;
    final codeFmt = _codeFmt;
    await Clipboard.setData(ClipboardData(text: '$codeFmt ${id.password}'));
    _toast('已复制设备代码 + 临时密码');
  }

  /// 刷新 = 删除本地身份重新注册（对齐 web handleResetDeskuuid）
  Future<void> _resetIdentity() async {
    final id = await createControllerIdentity();
    if (!mounted) return;
    setState(() => _identity = id);
    _toast(id == null ? '重新注册失败，请检查网络' : '已重新注册设备');
  }

  /// [20261016] 修改本机临时密码（对齐 web handleUpdatePassword）：
  /// PUT /desk_user/update_by_uuid，成功后同步刷新身份文件——
  /// 被控下次启动会从文件拿到新密码登录，设备代码保持不变。
  Future<void> _showEditPasswordDialog() async {
    final id = _deviceIdentity;
    if (id == null) {
      _toast('被控模块未运行，暂无法修改密码');
      return;
    }
    final ctrl = sanitizePasswordController(TextEditingController());
    // [20261017c] 聚焦禁 IME：不切输入法直接打英文小写
    final imeFocus = createNoImeFocusNode();
    final newPwd = await showDialog<String>(
      context: context,
      builder: (ctx) => AlertDialog(
        title: const Text('修改临时密码'),
        content: Column(mainAxisSize: MainAxisSize.min, children: [
          Text('当前密码：${id.password}',
              style: TextStyle(fontSize: 13, color: Colors.grey.shade600)),
          const SizedBox(height: 14),
          TextField(
            controller: ctrl,
            focusNode: imeFocus,
            autofocus: true,
            maxLength: 11,
            obscureText: false,
            // [20261017] 字母强制小写 + 滤掉汉字/全角（没切输入法直接打字
            // 上屏汉字 → 密码永远错；服务端密码本就是小写字母数字）
            inputFormatters: const [LowercaseAsciiFormatter()],
            decoration: const InputDecoration(
              labelText: '新密码（7-11 位）',
              hintText: '字母自动转小写，无需切换输入法',
              counterText: '',
              border: OutlineInputBorder(),
            ),
          ),
        ]),
        actions: [
          TextButton(
              onPressed: () => Navigator.pop(ctx), child: const Text('取消')),
          FilledButton(
              onPressed: () => Navigator.pop(ctx, ctrl.text.trim()),
              child: const Text('确认修改')),
        ],
      ),
    );
    if (newPwd == null || newPwd.isEmpty) return;
    disposeNoImeFocusNode(imeFocus);
    if (newPwd.length <= 6 || newPwd.length >= 12) {
      _toast('密码长度要求 7-11 位', ok: false);
      return;
    }
    if (newPwd == id.password) {
      _toast('新密码与当前密码一致', ok: false);
      return;
    }
    final r = await updateDeskUserPassword(id.uuid, id.password, newPwd);
    ShellLog.w('[pwd] api ok=${r.ok} msg=${r.msg} code-pwd=$newPwd');
    if (!mounted) return;
    if (r.ok) {
      final saved = saveHostIdentityPassword(newPwd);
      ShellLog.w('[pwd] saveHostIdentity=$saved');
      setState(() => _hostIdentity =
          ControllerIdentity(uuid: id.uuid, password: newPwd));
      _toast('密码修改成功', ok: true);
    } else {
      // [20261016b] 失败时再验证旧密码：若旧密码也登不上了，说明本地
      // 身份文件与服务端已不同步（如上次改密实际生效但当时未落盘），
      // 被控重启会自动重新注册出新设备代码。
      final oldOk = await verifyDeskLogin(id.uuid, id.password);
      ShellLog.w('[pwd] fail diag oldOk=$oldOk msg=${r.msg}');
      if (!mounted) return;
      if (!oldOk) {
        _toast('本地密码与服务端不同步，请重启被控模块重新注册', ok: false);
      } else {
        _toast('密码修改失败：${r.msg ?? '未知错误'}', ok: false);
      }
    }
  }

  void _connect() {
    final raw = _connectCtrl.text.trim();
    final digits = raw.replaceAll('-', '');
    if (digits.length != 9) {
      _toast('请输入 9 位设备代码，如 123-456-789');
      return;
    }
    RecentStore.addSync(digits);
    setState(() {
      _recent = RecentStore.loadSync();
      _showRecentDropdown = false;
    });
    // [20261017c] 已记住密码的设备自动回填，进连接页只需点「连接」
    final savedPwd = RecentStore.passwordOf(digits) ?? '';
    Navigator.of(context)
        .push(MaterialPageRoute<void>(
      builder: (_) => ConnectPage(
          prefillCode: digits, prefillPassword: savedPwd),
    ))
        .then((_) {
      if (mounted) setState(() => _recent = RecentStore.loadSync());
    });
  }

  void _toast(String msg, {bool ok = false}) {
    if (!mounted) return;
    ScaffoldMessenger.of(context)
      ..hideCurrentSnackBar()
      ..showSnackBar(SnackBar(
        content: Text(msg),
        behavior: SnackBarBehavior.floating,
        backgroundColor: ok ? const Color(0xFF21B573) : null,
      ));
  }

  @override
  void dispose() {
    _refreshTimer?.cancel();
    _connectCtrl.dispose();
    super.dispose();
  }

  @override
  Widget build(BuildContext context) {
    return SingleChildScrollView(
      padding: const EdgeInsets.fromLTRB(26, 18, 26, 18),
      child: Column(
        crossAxisAlignment: CrossAxisAlignment.stretch,
        children: [
          _buildLocalDevice(),
          const SizedBox(height: 12),
          _buildUnattendedRow(),
          const SizedBox(height: 12),
          _buildRemoteDevice(),
          const SizedBox(height: 12),
          _buildQualityCard(),
        ],
      ),
    );
  }

  BoxDecoration get _card => BoxDecoration(
        color: Colors.white,
        borderRadius: BorderRadius.circular(12),
        border: Border.all(color: const Color(0xFFDCE7F8)),
      );

  // ===== ① 此设备卡 =====
  Widget _buildLocalDevice() {
    return Container(
      padding: const EdgeInsets.all(18),
      decoration: _card,
      child: Column(crossAxisAlignment: CrossAxisAlignment.start, children: [
        Row(children: [
          Text('此设备',
              style: TextStyle(
                  fontSize: 13.5,
                  fontWeight: FontWeight.w700,
                  color: Colors.grey.shade800)),
          const SizedBox(width: 8),
          _hostDot(),
        ]),
        const SizedBox(height: 14),
        Row(children: [
          // 左：设备代码
          Expanded(
            child: Column(
                crossAxisAlignment: CrossAxisAlignment.start,
                children: [
                  Text('设备代码',
                      style: TextStyle(
                          fontSize: 12, color: Colors.grey.shade500)),
                  const SizedBox(height: 6),
                  Row(children: [
                    Flexible(
                      child: Text(
                          // [20261016b] 占位按真实状态：被控在跑=注册中，
                          // 没在跑=提示拉起（壳启动会自动静默拉起被控）
                          _codeFmt.isEmpty
                              ? (_hostRunning == true ? '注册中…' : '被控未启动')
                              : _codeFmt,
                          style: const TextStyle(
                              fontSize: 26,
                              fontWeight: FontWeight.w800,
                              letterSpacing: 2.5,
                              color: Color(0xFF182236))),
                    ),
                    const SizedBox(width: 10),
                    _miniIconBtn(
                        Icons.copy_rounded, '复制设备代码+密码', _copyInfo),
                    const SizedBox(width: 4),
                    _miniIconBtn(
                        // [20261016] 重置的是主控身份（发起连接时的服务端校验身份）；
                        // 本机设备代码由被控模块管理，重置需在旧界面（托盘打开）操作
                        Icons.refresh_rounded,
                        '重新注册主控身份', _resetIdentity),
                  ]),
                ]),
          ),
          Container(width: 1, height: 44, color: const Color(0xFFDCE7F8)),
          const SizedBox(width: 22),
          // 右：临时密码
          Expanded(
            child: Column(
                crossAxisAlignment: CrossAxisAlignment.start,
                children: [
                  Text('临时密码',
                      style: TextStyle(
                          fontSize: 12, color: Colors.grey.shade500)),
                  const SizedBox(height: 6),
                  Row(children: [
                    Flexible(
                      child: Text(
                          _deviceIdentity == null
                              ? '—'
                              : (_hidePwd
                                  ? '••••••••'
                                  : _deviceIdentity!.password),
                          style: const TextStyle(
                              fontSize: 22,
                              fontWeight: FontWeight.w700,
                              letterSpacing: 2,
                              color: Color(0xFF182236))),
                    ),
                    const SizedBox(width: 10),
                    _miniIconBtn(
                        _hidePwd
                            ? Icons.visibility_off_outlined
                            : Icons.visibility_outlined,
                        _hidePwd ? '显示密码' : '隐藏密码',
                        () => setState(() => _hidePwd = !_hidePwd)),
                    const SizedBox(width: 4),
                    _miniIconBtn(Icons.edit_outlined, '修改临时密码',
                        _showEditPasswordDialog),
                  ]),
                ]),
          ),
        ]),
      ]),
    );
  }

  Widget _hostDot() {
    final s = _receiver?.state;
    final (color, text) = switch (s) {
      ReceiverState.controlled => (const Color(0xFF2F6FED), '正在被控制'),
      ReceiverState.incoming => (const Color(0xFF2F6FED), '连接请求中'),
      ReceiverState.online => (const Color(0xFF21B573), '在线等待中'),
      ReceiverState.offline => (const Color(0xFFD97706), '连接服务中…'),
      null => _hostRunning == true
          ? (const Color(0xFF21B573), '被控运行中')
          : (const Color(0xFFD97706), '被控未启动'),
    };
    return Row(mainAxisSize: MainAxisSize.min, children: [
      Icon(Icons.circle, size: 7, color: color),
      const SizedBox(width: 4),
      Text(text, style: TextStyle(fontSize: 11, color: Colors.grey.shade500)),
      if (_hostRunning == false) ...[
        const SizedBox(width: 6),
        Pressable(
          onTap: _bootReceiver,
          radius: 4,
          child: Text('启动',
              style: TextStyle(
                  fontSize: 11,
                  color: brand,
                  fontWeight: FontWeight.w600)),
        ),
      ],
    ]);
  }

  Widget _miniIconBtn(IconData icon, String tip, VoidCallback onTap) {
    return Tooltip(
      message: tip,
      child: Pressable(
        onTap: onTap,
        radius: 6,
        child: Container(
          padding: const EdgeInsets.all(5),
          child: Icon(icon, size: 16, color: Colors.grey.shade500),
        ),
      ),
    );
  }

  // ===== ② 无人值守行 =====
  Widget _buildUnattendedRow() {
    return Container(
      padding: const EdgeInsets.symmetric(horizontal: 18, vertical: 10),
      decoration: _card,
      child: Row(children: [
        _checkItem('无人值守', ShellPrefs.unattended, (v) {
          ShellPrefs.unattended = v;
          setState(() {});
          _toast(
              v ? '无人值守已开启：连接请求自动放行，开机自启联动' : '无人值守已关闭：连接请求需手动确认',
              ok: v);
        }),
        const SizedBox(width: 22),
        if (ShellPrefs.unattended) ...[
          Row(mainAxisSize: MainAxisSize.min, children: [
            Icon(Icons.circle,
                size: 7,
                color: _hostRunning == true
                    ? const Color(0xFF21B573)
                    : const Color(0xFFD97706)),
            const SizedBox(width: 4),
            Text(
              _hostRunning == true ? '屏幕共享已就绪' : '屏幕共享准备中…',
              style: TextStyle(
                  fontSize: 11.5,
                  color: _hostRunning == true
                      ? const Color(0xFF21B573)
                      : const Color(0xFFD97706)),
            ),
          ]),
          const SizedBox(width: 22),
        ],
        _checkItem('隐私屏', ShellPrefs.privacyAutoOn, (v) {
          ShellPrefs.privacyAutoOn = v;
          setState(() {});
        }),
        const SizedBox(width: 22),
        _checkItem('会话结束锁屏', ShellPrefs.lockAfterSession, (v) {
          ShellPrefs.lockAfterSession = v;
          setState(() {});
        }),
      ]),
    );
  }

  Widget _checkItem(String label, bool value, ValueChanged<bool> onChange) {
    return Pressable(
      onTap: () => onChange(!value),
      radius: 6,
      child: Row(mainAxisSize: MainAxisSize.min, children: [
        Container(
          width: 15,
          height: 15,
          decoration: BoxDecoration(
            color: value ? brand : Colors.white,
            borderRadius: BorderRadius.circular(3.5),
            border: Border.all(
                color: value ? brand : Colors.grey.shade400, width: 1.4),
          ),
          child: value
              ? const Icon(Icons.check, size: 11, color: Colors.white)
              : null,
        ),
        const SizedBox(width: 7),
        Text(label,
            style: TextStyle(
                fontSize: 12.5,
                color:
                    value ? const Color(0xFF182236) : Colors.grey.shade700,
                fontWeight: value ? FontWeight.w600 : FontWeight.w400)),
      ]),
    );
  }

  // ===== ③ 远程控制设备卡 =====
  Widget _buildRemoteDevice() {
    final digits = _connectCtrl.text.replaceAll('-', '');
    final canConnect = digits.length == 9;
    return Container(
      padding: const EdgeInsets.all(18),
      decoration: _card,
      child: Column(crossAxisAlignment: CrossAxisAlignment.start, children: [
        Text('远程控制设备',
            style: TextStyle(
                fontSize: 13.5,
                fontWeight: FontWeight.w700,
                color: Colors.grey.shade800)),
        const SizedBox(height: 14),
        Row(crossAxisAlignment: CrossAxisAlignment.start, children: [
          Expanded(
            // [20261017] 鼠标悬停输入框区域 → 自动展开历史连接记录，直接点选
            // 填入；移开自动收起。箭头点击 toggle 保留作兜底。
            child: MouseRegion(
              onEnter: (_) {
                if (_recent.isNotEmpty && !_showRecentDropdown) {
                  setState(() => _showRecentDropdown = true);
                }
              },
              onExit: (_) {
                if (_showRecentDropdown) {
                  setState(() => _showRecentDropdown = false);
                }
              },
              child: Column(children: [
              // 输入行（含下拉箭头）
              Stack(children: [
                TextField(
                  controller: _connectCtrl,
                  keyboardType: TextInputType.number,
                  inputFormatters: [
                    FilteringTextInputFormatter.allow(RegExp(r'[\d-]'))
                  ],
                  onChanged: (_) => setState(() {}),
                  onSubmitted: (_) => _connect(),
                  style: const TextStyle(
                      fontSize: 14.5,
                      letterSpacing: 1.5,
                      fontWeight: FontWeight.w600),
                  decoration: InputDecoration(
                    hintText: '例如 123-456-789',
                    hintStyle: TextStyle(
                        fontSize: 13,
                        letterSpacing: 0,
                        color: Colors.grey.shade400),
                    filled: true,
                    fillColor: const Color(0xFFF2F7FE),
                    contentPadding: const EdgeInsets.symmetric(
                        horizontal: 14, vertical: 13),
                    border: OutlineInputBorder(
                        borderRadius: BorderRadius.circular(9),
                        borderSide: BorderSide(color: Colors.grey.shade300)),
                    enabledBorder: OutlineInputBorder(
                        borderRadius: BorderRadius.circular(9),
                        borderSide: BorderSide(color: Colors.grey.shade300)),
                    focusedBorder: OutlineInputBorder(
                        borderRadius: BorderRadius.circular(9),
                        borderSide:
                            const BorderSide(color: brand, width: 1.5)),
                  ),
                ),
                if (_recent.isNotEmpty)
                  Positioned(
                    right: 8,
                    top: 0,
                    bottom: 0,
                    child: Center(
                      child: Pressable(
                        // [20261019d] 每次展开都重读记录：会话期间 RemotePage
                        // 会往记录里补 IP/链路类型，而主界面 push 回调在
                        // pushReplacement 进画面页时就已触发（早于 IP 入库），
                        // 断开回来不重读的话界面永远看不到 IP（实测踩坑）。
                        onTap: () {
                          if (!_showRecentDropdown) {
                            _recent = RecentStore.loadSync();
                            // [20261019e] 展开即探测（服务端 find_receiver_by_uuid
                            // 拿设备名/出口IP，逐台回填），界面增量刷新
                            _probeRecent();
                          }
                          setState(() =>
                              _showRecentDropdown = !_showRecentDropdown);
                        },
                        radius: 4,
                        child: Icon(
                            _showRecentDropdown
                                ? Icons.keyboard_arrow_up_rounded
                                : Icons.keyboard_arrow_down_rounded,
                            size: 20, color: Colors.grey.shade500),
                      ),
                    ),
                  ),
              ]),
              // 常用设备下拉（对齐 web link-device-list）
              if (_showRecentDropdown && _recent.isNotEmpty)
                Container(
                  margin: const EdgeInsets.only(top: 6),
                  decoration: BoxDecoration(
                    color: Colors.white,
                    borderRadius: BorderRadius.circular(9),
                    border: Border.all(color: const Color(0xFFDCE7F8)),
                  ),
                  child: Column(children: [
                    for (final it in _recent) _recentRow(it),
                  ]),
                ),
            ]),
            ),
          ),
          const SizedBox(width: 14),
          // 连接大按钮（[20261017] 按下加深一档，有实体反馈）
          Pressable.builder(
            onTap: canConnect ? _connect : null,
            radius: 9,
            builder: (pressed) => Container(
              width: 108,
              height: 46,
              alignment: Alignment.center,
              decoration: BoxDecoration(
                color: !canConnect
                    ? const Color(0xFFB9CFF5)
                    : (pressed
                        ? const Color(0xFF1E54C8)
                        : brand),
                borderRadius: BorderRadius.circular(9),
              ),
              child: const Text('连 接',
                  style: TextStyle(
                      fontSize: 15,
                      fontWeight: FontWeight.w700,
                      color: Colors.white,
                      letterSpacing: 2)),
            ),
          ),
        ]),
        const SizedBox(height: 10),
        Text('已准备好连接',
            style: TextStyle(fontSize: 11.5, color: Colors.grey.shade400)),
      ]),
    );
  }

  /// [20261019e] 最近连接探测：逐台调服务端 find_receiver_by_uuid
  /// （web 主控同一条路），设备名/出口IP/归属地回填 RecentStore 并刷新。
  bool _probingRecent = false;
  void _probeRecent() {
    if (_probingRecent) return;
    _probingRecent = true;
    probeDevices(_recent.map((e) => e['code'] as String).toList(), (code, r) {
      RecentStore.merge(code, {'name': r.name, 'ip': r.ip, 'geo': r.geo});
      if (mounted) setState(() => _recent = RecentStore.loadSync());
    }).then((_) => _probingRecent = false);
  }

  Widget _recentRow(Map<String, dynamic> it) {
    final code = it['code'] as String;
    final dt = DateTime.fromMillisecondsSinceEpoch(it['at'] as int);
    final ts =
        '${dt.month}/${dt.day} ${dt.hour.toString().padLeft(2, '0')}:${dt.minute.toString().padLeft(2, '0')}';
    // [20261019e] IP 展示对齐 web 主控：服务端探测（find_receiver_by_uuid）
    // 拿出口 IP + 归属地；会话中 ICE 捕获的 ipType 作为链路补充信息。
    final name = (it['name'] ?? '').toString();
    final ip = (it['ip'] ?? '').toString();
    final geo = (it['geo'] ?? '').toString();
    final ipType = it['ipType'] as String?;
    final loc = displayLoc(ip, geo);
    final ipLine = loc.isEmpty
        ? 'IP 暂无（连接一次后自动记录）'
        : (isLanIp(ip) ? loc : 'IP $loc');
    return Pressable.builder(
      onTap: () {
        // [20261018] 点历史记录 = 只把设备码填进输入框，**不再自动发起
        // 连接**（用户反馈：点历史直接连上了很不好，应由用户自行点「连接」）
        _connectCtrl.text = code;
        setState(() => _showRecentDropdown = false);
      },
      radius: 0,
      builder: (pressed) => Container(
        color: pressed ? const Color(0xFFF2F7FE) : Colors.transparent,
        padding: const EdgeInsets.symmetric(horizontal: 14, vertical: 8),
        child: Row(children: [
          Icon(Icons.computer_rounded, size: 18, color: Colors.grey.shade500),
          const SizedBox(width: 10),
          Expanded(
            child: Column(
                crossAxisAlignment: CrossAxisAlignment.start,
                mainAxisSize: MainAxisSize.min,
                children: [
                  Row(children: [
                    Text(code,
                        style: const TextStyle(
                            fontSize: 13.5,
                            fontWeight: FontWeight.w600,
                            letterSpacing: 1.2,
                            color: Color(0xFF182236))),
                    // [20261019e] 设备名（服务端探测回填，通常是被控主机名）
                    if (name.isNotEmpty) ...[
                      const SizedBox(width: 8),
                      Flexible(
                        child: Text(name,
                            maxLines: 1,
                            overflow: TextOverflow.ellipsis,
                            style: TextStyle(
                                fontSize: 11.5,
                                color: Colors.grey.shade500)),
                      ),
                    ],
                    const SizedBox(width: 8),
                    // [20261017c] 已记住密码标记
                    if (RecentStore.passwordOf(code) != null)
                      Container(
                        padding: const EdgeInsets.symmetric(
                            horizontal: 6, vertical: 1),
                        decoration: BoxDecoration(
                          color: const Color(0xFFE7F6EE),
                          borderRadius: BorderRadius.circular(4),
                        ),
                        child: const Text('已存密码',
                            style: TextStyle(
                                fontSize: 10, color: Color(0xFF21B573))),
                      ),
                  ]),
                  const SizedBox(height: 3),
                  // [20261019e] 第二行：归属地+IP（链路类型） · 最近连接时间
                  Text(
                      '$ipLine${(ipType == null || ipType.isEmpty) ? '' : ' · $ipType'} · 最近连接 $ts',
                      maxLines: 1,
                      overflow: TextOverflow.ellipsis,
                      style: TextStyle(
                          fontSize: 11.5, color: Colors.grey.shade400)),
                ]),
          ),
          const SizedBox(width: 10),
          Pressable(
            onTap: () {
              RecentStore.removeSync(it['code'] as String);
              setState(() {
                _recent = RecentStore.loadSync();
                if (_recent.isEmpty) _showRecentDropdown = false;
              });
            },
            radius: 8,
            child: Icon(Icons.delete_outline_rounded,
                size: 16, color: Colors.grey.shade400),
          ),
        ]),
      ),
    );
  }

  // ===== ④ 连接画质卡 =====
  Widget _buildQualityCard() {
    return Container(
      padding: const EdgeInsets.all(18),
      decoration: _card,
      child: Column(crossAxisAlignment: CrossAxisAlignment.start, children: [
        Row(children: [
          Text('连接画质',
              style: TextStyle(
                  fontSize: 13.5,
                  fontWeight: FontWeight.w700,
                  color: Colors.grey.shade800)),
          const SizedBox(width: 10),
          Expanded(
            child: Text('先选画质档位；连接后自动下发，会话中也可在工具栏随时调整',
                style: TextStyle(fontSize: 11, color: Colors.grey.shade400),
                overflow: TextOverflow.ellipsis),
          ),
        ]),
        const SizedBox(height: 14),
        _qualityRow<String>('画质档位', const [
          ('smooth', '流畅'),
          ('balanced', '均衡'),
          ('hd', '高清'),
        ], _tierId, (v) {
          ShellPrefs.applyTier(v);
          setState(() => _tierId = v);
        }),
        const SizedBox(height: 10),
        _qualityRow<int>('分辨率', const [
          (540, '540P'),
          (720, '720P'),
          (1080, '原生'),
        ], ShellPrefs.resolution, (v) {
          ShellPrefs.resolution = v;
          ShellPrefs.tierId = 'custom';
          setState(() => _tierId = 'custom');
        }),
        const SizedBox(height: 10),
        _qualityRow<int>('码率', const [
          (2000, '2Mbps'),
          (4000, '4Mbps'),
          (8000, '8Mbps'),
        ], ShellPrefs.bitrateKbps, (v) {
          ShellPrefs.bitrateKbps = v;
          ShellPrefs.tierId = 'custom';
          setState(() => _tierId = 'custom');
        }),
        const SizedBox(height: 10),
        _qualityRow<int>('帧率', const [
          (15, '15fps'),
          (30, '30fps'),
          (60, '60fps'),
        ], ShellPrefs.framerate, (v) {
          ShellPrefs.framerate = v;
          ShellPrefs.tierId = 'custom';
          setState(() => _tierId = 'custom');
        }),
      ]),
    );
  }

  Widget _qualityRow<T>(String label, List<(T, String)> options, T current,
      ValueChanged<T> onSelect) {
    return Row(children: [
      SizedBox(
          width: 64,
          child: Text(label,
              style:
                  TextStyle(fontSize: 12.5, color: Colors.grey.shade600))),
      const SizedBox(width: 8),
      Expanded(
        child: Wrap(spacing: 8, children: [
          for (final (val, name) in options)
            Pressable.builder(
              onTap: () => onSelect(val),
              radius: 7,
              builder: (pressed) => Container(
                padding:
                    const EdgeInsets.symmetric(horizontal: 13, vertical: 6),
                decoration: BoxDecoration(
                  color: current == val
                      ? (pressed
                          ? const Color(0xFF1E54C8)
                          : brand)
                      : (pressed
                          ? const Color(0xFFE4EDFB)
                          : const Color(0xFFF2F7FE)),
                  borderRadius: BorderRadius.circular(7),
                  border: Border.all(
                      color:
                          current == val ? brand : const Color(0xFFDCE7F8)),
                ),
                child: Text(name,
                    style: TextStyle(
                        fontSize: 12,
                        fontWeight: current == val
                            ? FontWeight.w700
                            : FontWeight.w400,
                        color: current == val
                            ? Colors.white
                            : Colors.grey.shade700)),
              ),
            ),
        ]),
      ),
    ]);
  }
}
