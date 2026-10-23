/// 账号页 —— 1:1 对齐 web 端 views/login/index.vue：
/// 左侧品牌区（logo + 宣传语 + 卖点 + 域名）+ 右侧表单区
/// （登录 / 注册 / 忘记密码 三 tab，邮箱验证码倒计时，跳过登录入口）。
/// 接口对齐 src/api/user.ts（Bearer token，注册成功自动登录）。
import 'package:flutter/material.dart';
import 'package:flutter/services.dart';

import '../api/user_api.dart';
import '../core/prefs.dart';
import '../core/shell_log.dart';
import 'widgets/input_formatters.dart';

class AccountPage extends StatefulWidget {
  const AccountPage({super.key, this.initialMode = 'login'});

  /// login | register | reset
  final String initialMode;

  @override
  State<AccountPage> createState() => _AccountPageState();
}

class _AccountPageState extends State<AccountPage> {
  static const brand = Color(0xFF2F6FED);
  static const ink = Color(0xFF182236);

  late String _mode; // login | register | reset
  bool _loading = false;
  int _countdown = 0;

  final _loginUser = TextEditingController();
  final _loginPwd = TextEditingController();
  final _regUser = TextEditingController();
  final _regEmail = TextEditingController();
  final _regCode = TextEditingController();
  final _regPwd = TextEditingController();
  final _regPwd2 = TextEditingController();
  final _resetEmail = TextEditingController();
  final _resetCode = TextEditingController();
  final _resetNewPwd = TextEditingController();

  // [20261017c] 所有密码框聚焦时禁用系统输入法（直接英文上屏）
  final _loginPwdFocus = createNoImeFocusNode();
  final _regPwdFocus = createNoImeFocusNode();
  final _regPwd2Focus = createNoImeFocusNode();
  final _resetPwdFocus = createNoImeFocusNode();

  @override
  void initState() {
    super.initState();
    _mode = widget.initialMode;
  }

  @override
  void dispose() {
    for (final c in [
      _loginUser, _loginPwd, _regUser, _regEmail, _regCode,
      _regPwd, _regPwd2, _resetEmail, _resetCode, _resetNewPwd,
    ]) {
      c.dispose();
    }
    for (final f in [
      _loginPwdFocus, _regPwdFocus, _regPwd2Focus, _resetPwdFocus,
    ]) {
      disposeNoImeFocusNode(f);
    }
    super.dispose();
  }

  void _startCountdown() {
    _countdown = 60;
    Future.doWhile(() async {
      await Future.delayed(const Duration(seconds: 1));
      if (!mounted) return false;
      setState(() => _countdown--);
      return _countdown > 0;
    });
  }

  void _toast(String msg, {bool ok = false}) {
    if (!mounted) return;
    ScaffoldMessenger.of(context)
      ..hideCurrentSnackBar()
      ..showSnackBar(SnackBar(
        content: Text(msg),
        behavior: SnackBarBehavior.floating,
        backgroundColor: ok ? const Color(0xFF21B573) : const Color(0xFFE5484D),
      ));
  }

  // ===== 登录 =====
  Future<void> _handleLogin() async {
    final u = _loginUser.text.trim();
    final p = _loginPwd.text;
    if (u.isEmpty || p.isEmpty) {
      _toast('请输入用户名和密码');
      return;
    }
    setState(() => _loading = true);
    final r = await usernameLogin(u, p);
    if (!mounted) return;
    setState(() => _loading = false);
    if (r.ok && r.data is String && (r.data as String).isNotEmpty) {
      ShellPrefs.token = r.data as String;
      ShellPrefs.username = u;
      _toast('登录成功', ok: true);
      await _fetchAccountExtras();
      if (!mounted) return;
      Navigator.of(context).pop(true); // 登录成功回壳
    } else {
      _toast(r.msg ?? '登录失败，请检查用户名密码');
    }
  }

  Future<void> _fetchAccountExtras() async {
    final token = ShellPrefs.token;
    if (token.isEmpty) return;
    final info = await getUserInfo(token);
    if (info.ok && info.data is Map) {
      final name = (info.data as Map)['username']?.toString() ?? '';
      if (name.isNotEmpty && mounted) setState(() => ShellPrefs.username = name);
    }
    final aid = await myAccountId(token);
    if (aid.ok && aid.data is Map) {
      final d = aid.data as Map;
      ShellPrefs.accountId = d['account_id']?.toString() ?? '';
      ShellPrefs.email = d['email']?.toString() ?? '';
    }
    ShellLog.w('[account] extras: id=${ShellPrefs.accountId} '
        'email=${ShellPrefs.email}');
  }

  // ===== 注册 =====
  Future<void> _handleSendCode() async {
    final email = _regEmail.text.trim();
    if (!email.contains('@')) {
      _toast('请输入正确的邮箱');
      return;
    }
    final r = await sendEmailCode(email, 'register');
    if (r.ok) {
      _startCountdown();
      _toast('验证码已发送到邮箱', ok: true);
    } else {
      _toast(r.msg ?? '验证码发送失败');
    }
  }

  Future<void> _handleRegister() async {
    final u = _regUser.text.trim();
    final email = _regEmail.text.trim();
    final code = _regCode.text.trim();
    final p = _regPwd.text;
    final p2 = _regPwd2.text;
    if (u.length < 3 || u.length > 12) {
      _toast('用户名需 3-12 位');
      return;
    }
    if (!email.contains('@')) {
      _toast('请输入正确的邮箱');
      return;
    }
    if (code.isEmpty) {
      _toast('请输入邮箱验证码');
      return;
    }
    if (p.length < 6 || p.length > 18) {
      _toast('密码需 6-18 位');
      return;
    }
    if (p != p2) {
      _toast('两次密码不一致');
      return;
    }
    setState(() => _loading = true);
    final r = await registerByEmail(u, p, email, code);
    if (!mounted) return;
    setState(() => _loading = false);
    if (!r.ok) {
      _toast(r.msg ?? '注册失败');
      return;
    }
    // 注册成功自动登录（对齐 web store.register）
    final lr = await usernameLogin(u, p);
    if (!mounted) return;
    if (lr.ok && lr.data is String && (lr.data as String).isNotEmpty) {
      ShellPrefs.token = lr.data as String;
      ShellPrefs.username = u;
      await _fetchAccountExtras();
      if (!mounted) return;
      _toast('注册成功，已自动登录', ok: true);
      Navigator.of(context).pop(true);
    } else {
      _toast('注册成功，请登录', ok: true);
      setState(() => _mode = 'login');
      _loginUser.text = u;
    }
  }

  // ===== 忘记密码 =====
  Future<void> _handleSendResetCode() async {
    final email = _resetEmail.text.trim();
    if (!email.contains('@')) {
      _toast('请输入正确的邮箱');
      return;
    }
    final r = await sendEmailCode(email, 'reset');
    if (r.ok) {
      _startCountdown();
      _toast('验证码已发送到邮箱', ok: true);
    } else {
      _toast(r.msg ?? '验证码发送失败');
    }
  }

  Future<void> _handleReset() async {
    final email = _resetEmail.text.trim();
    final code = _resetCode.text.trim();
    final np = _resetNewPwd.text;
    if (!email.contains('@') || code.isEmpty) {
      _toast('请输入邮箱和验证码');
      return;
    }
    if (np.length < 6 || np.length > 18) {
      _toast('新密码需 6-18 位');
      return;
    }
    setState(() => _loading = true);
    final r = await resetPwd(email, code, np);
    if (!mounted) return;
    setState(() => _loading = false);
    if (r.ok) {
      _toast('密码已重置，请用新密码登录', ok: true);
      setState(() => _mode = 'login');
      _loginUser.text = '';
    } else {
      _toast(r.msg ?? '重置失败');
    }
  }

  @override
  Widget build(BuildContext context) {
    return Scaffold(
      backgroundColor: const Color(0xFFF2F7FE),
      body:       Row(crossAxisAlignment: CrossAxisAlignment.stretch, children: [
        // 左侧品牌区（内容居中，ClipRect 防装饰圆溢出）
        Container(
          width: 340,
          color: Colors.white,
          child: ClipRect(
            child: Stack(children: [
              Positioned(
                right: -60,
                top: -60,
                child: _circle(220, const Color(0x142F6FED)),
              ),
              Positioned(
                left: -80,
                bottom: -40,
                child: _circle(260, const Color(0x1021B573)),
              ),
              Positioned.fill(
                child: Padding(
                  padding: const EdgeInsets.symmetric(horizontal: 32),
                  child: Column(
                    mainAxisAlignment: MainAxisAlignment.center,
                    crossAxisAlignment: CrossAxisAlignment.center,
                    children: [
                      Row(mainAxisSize: MainAxisSize.min, children: [
                        Container(
                          width: 34,
                          height: 34,
                          alignment: Alignment.center,
                          decoration: BoxDecoration(
                            color: brand,
                            borderRadius: BorderRadius.circular(9),
                          ),
                          child: const Text('in',
                              style: TextStyle(
                                  color: Colors.white,
                                  fontSize: 17,
                                  fontWeight: FontWeight.w800)),
                        ),
                        const SizedBox(width: 10),
                        const Text('InDesk',
                            style: TextStyle(
                                fontSize: 21,
                                fontWeight: FontWeight.w800,
                                color: ink)),
                      ]),
                      const SizedBox(height: 22),
                      const Text('远程协助\n简单、直达、可靠',
                          textAlign: TextAlign.center,
                          style: TextStyle(
                              fontSize: 24,
                              fontWeight: FontWeight.w800,
                              color: ink,
                              height: 1.4)),
                      const SizedBox(height: 18),
                      _point('设备码一键直连，无需复杂配置'),
                      _point('注册账号统一管理设备与连接记录'),
                      _point('邮箱绑定，验证码随时找回密码'),
                      const SizedBox(height: 30),
                      Text('desk.yomm.top',
                          style: TextStyle(
                              fontSize: 11.5, color: Colors.grey.shade400)),
                    ],
                  ),
                ),
              ),
            ]),
          ),
        ),
        // 右侧表单区
        Expanded(
          child: Column(children: [
            // 顶栏：返回
            SizedBox(
              height: 46,
              child: Row(children: [
                const SizedBox(width: 18),
                InkWell(
                  onTap: () => Navigator.of(context).pop(false),
                  borderRadius: BorderRadius.circular(8),
                  child: Container(
                    padding: const EdgeInsets.symmetric(
                        horizontal: 10, vertical: 6),
                    child: Row(children: [
                      Icon(Icons.arrow_back_rounded,
                          size: 17, color: Colors.grey.shade600),
                      const SizedBox(width: 5),
                      Text('返回',
                          style: TextStyle(
                              fontSize: 13, color: Colors.grey.shade600)),
                    ]),
                  ),
                ),
              ]),
            ),
            Expanded(
              child: Center(
                child: SingleChildScrollView(
                  padding: const EdgeInsets.symmetric(horizontal: 24),
                  child: ConstrainedBox(
                    constraints: const BoxConstraints(maxWidth: 360),
                    child: _buildForm(),
                  ),
                ),
              ),
            ),
          ]),
        ),
      ]),
    );
  }

  Widget _circle(double size, Color color) => Container(
        width: size,
        height: size,
        decoration: BoxDecoration(color: color, shape: BoxShape.circle),
      );

  Widget _point(String text) => Padding(
        padding: const EdgeInsets.only(bottom: 10),
        child: Row(mainAxisSize: MainAxisSize.min, children: [
          Icon(Icons.check_circle_outline_rounded,
              size: 15, color: brand),
          const SizedBox(width: 8),
          Text(text, style: TextStyle(fontSize: 12.5, color: Colors.grey.shade600)),
        ]),
      );

  Widget _buildForm() {
    return Container(
      padding: const EdgeInsets.all(26),
      decoration: BoxDecoration(
        color: Colors.white,
        borderRadius: BorderRadius.circular(14),
        border: Border.all(color: const Color(0xFFDCE7F8)),
      ),
      child: Column(mainAxisSize: MainAxisSize.min,
          crossAxisAlignment: CrossAxisAlignment.stretch, children: [
        Text(_title,
            style: const TextStyle(
                fontSize: 19, fontWeight: FontWeight.w800, color: ink)),
        const SizedBox(height: 5),
        Text(_sub,
            style: TextStyle(fontSize: 12, color: Colors.grey.shade500)),
        const SizedBox(height: 16),
        // seg-tabs
        Container(
          padding: const EdgeInsets.all(3),
          decoration: BoxDecoration(
            color: const Color(0xFFF2F7FE),
            borderRadius: BorderRadius.circular(9),
          ),
          child: Row(children: [
            _seg('登录', _mode == 'login', () => setState(() => _mode = 'login')),
            _seg('注册', _mode == 'register', () => setState(() => _mode = 'register')),
            _seg('忘记密码', _mode == 'reset', () => setState(() => _mode = 'reset')),
          ]),
        ),
        const SizedBox(height: 16),
        if (_mode == 'login') _buildLogin(),
        if (_mode == 'register') _buildRegister(),
        if (_mode == 'reset') _buildReset(),
        if (_mode == 'login') ...[
          const SizedBox(height: 12),
          Center(
            child: InkWell(
              onTap: () => Navigator.of(context).pop(false),
              child: Text('跳过登录，直接使用设备码 →',
                  style: TextStyle(fontSize: 12, color: Colors.grey.shade500)),
            ),
          ),
        ],
      ]),
    );
  }

  String get _title => switch (_mode) {
        'register' => '注册账号',
        'reset' => '找回密码',
        _ => '欢迎回来',
      };

  String get _sub => switch (_mode) {
        'register' => '通过注册邮箱 + 验证码创建账号',
        'reset' => '通过注册邮箱 + 验证码设置新密码',
        _ => '登录后统一管理设备与连接记录',
      };

  Widget _seg(String label, bool active, VoidCallback onTap) {
    return Expanded(
      child: InkWell(
        onTap: onTap,
        borderRadius: BorderRadius.circular(7),
        child: Container(
          padding: const EdgeInsets.symmetric(vertical: 7),
          decoration: BoxDecoration(
            color: active ? Colors.white : Colors.transparent,
            borderRadius: BorderRadius.circular(7),
            border: active
                ? Border.all(color: const Color(0xFFDCE7F8))
                : null,
          ),
          alignment: Alignment.center,
          child: Text(label,
              style: TextStyle(
                  fontSize: 12.5,
                  fontWeight: active ? FontWeight.w700 : FontWeight.w400,
                  color: active ? brand : Colors.grey.shade600)),
        ),
      ),
    );
  }

  Widget _field(TextEditingController ctrl, String hint, IconData icon,
      {bool obscure = false,
      ValueChanged<String>? onSubmit,
      TextInputType? keyboard,
      FocusNode? focusNode}) {
    return Padding(
      padding: const EdgeInsets.only(bottom: 12),
      child: TextField(
        controller: ctrl,
        focusNode: focusNode,
        obscureText: obscure,
        keyboardType: keyboard,
        onSubmitted: onSubmit,
        style: const TextStyle(fontSize: 13.5),
        decoration: InputDecoration(
          hintText: hint,
          hintStyle: TextStyle(fontSize: 13, color: Colors.grey.shade400),
          prefixIcon: Icon(icon, size: 17, color: Colors.grey.shade400),
          contentPadding:
              const EdgeInsets.symmetric(horizontal: 14, vertical: 13),
          border: OutlineInputBorder(
              borderRadius: BorderRadius.circular(9),
              borderSide: BorderSide(color: Colors.grey.shade300)),
          enabledBorder: OutlineInputBorder(
              borderRadius: BorderRadius.circular(9),
              borderSide: BorderSide(color: Colors.grey.shade300)),
          focusedBorder: OutlineInputBorder(
              borderRadius: BorderRadius.circular(9),
              borderSide: const BorderSide(color: brand, width: 1.5)),
        ),
      ),
    );
  }

  Widget _primaryBtn(String label, VoidCallback onTap) {
    return SizedBox(
      height: 44,
      child: ElevatedButton(
        onPressed: _loading ? null : onTap,
        style: ElevatedButton.styleFrom(
          backgroundColor: brand,
          foregroundColor: Colors.white,
          elevation: 0,
          shape:
              RoundedRectangleBorder(borderRadius: BorderRadius.circular(9)),
        ),
        child: _loading
            ? const SizedBox(
                width: 18,
                height: 18,
                child: CircularProgressIndicator(
                    strokeWidth: 2, color: Colors.white))
            : Text(label,
                style: const TextStyle(
                    fontSize: 14.5, fontWeight: FontWeight.w700)),
      ),
    );
  }

  Widget _buildLogin() {
    return Column(crossAxisAlignment: CrossAxisAlignment.stretch, children: [
      _field(_loginUser, '用户名', Icons.person_outline_rounded,
          onSubmit: (_) => _handleLogin()),
      _field(_loginPwd, '密码', Icons.lock_outline_rounded,
          obscure: true,
          focusNode: _loginPwdFocus,
          onSubmit: (_) => _handleLogin()),
      const SizedBox(height: 2),
      _primaryBtn('登 录', _handleLogin),
    ]);
  }

  Widget _codeField(TextEditingController ctrl, VoidCallback sendCode) {
    return Padding(
      padding: const EdgeInsets.only(bottom: 12),
      child: Row(children: [
        Expanded(
          child: TextField(
            controller: ctrl,
            keyboardType: TextInputType.number,
            inputFormatters: [FilteringTextInputFormatter.digitsOnly],
            style: const TextStyle(fontSize: 13.5),
            decoration: InputDecoration(
              hintText: '邮箱验证码',
              hintStyle:
                  TextStyle(fontSize: 13, color: Colors.grey.shade400),
              prefixIcon: Icon(Icons.shield_outlined,
                  size: 17, color: Colors.grey.shade400),
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
                  borderSide: const BorderSide(color: brand, width: 1.5)),
            ),
          ),
        ),
        const SizedBox(width: 8),
        InkWell(
          onTap: _countdown > 0 || _loading ? null : sendCode,
          borderRadius: BorderRadius.circular(9),
          child: Container(
            height: 46,
            padding: const EdgeInsets.symmetric(horizontal: 13),
            alignment: Alignment.center,
            decoration: BoxDecoration(
              color: _countdown > 0
                  ? Colors.grey.shade200
                  : const Color(0xFFEAF1FE),
              borderRadius: BorderRadius.circular(9),
            ),
            child: Text(
              _countdown > 0 ? '${_countdown}s' : '获取验证码',
              style: TextStyle(
                  fontSize: 12.5,
                  fontWeight: FontWeight.w600,
                  color: _countdown > 0
                      ? Colors.grey.shade500
                      : brand),
            ),
          ),
        ),
      ]),
    );
  }

  Widget _buildRegister() {
    return Column(crossAxisAlignment: CrossAxisAlignment.stretch, children: [
      _field(_regUser, '用户名（3-12位）', Icons.person_outline_rounded),
      _field(_regEmail, '邮箱（用于找回密码）', Icons.mail_outline_rounded,
          keyboard: TextInputType.emailAddress),
      _codeField(_regCode, _handleSendCode),
      _field(_regPwd, '密码（6-18位）', Icons.lock_outline_rounded,
          obscure: true, focusNode: _regPwdFocus),
      _field(_regPwd2, '确认密码', Icons.lock_outline_rounded,
          obscure: true,
          focusNode: _regPwd2Focus,
          onSubmit: (_) => _handleRegister()),
      const SizedBox(height: 2),
      _primaryBtn('注 册', _handleRegister),
    ]);
  }

  Widget _buildReset() {
    return Column(crossAxisAlignment: CrossAxisAlignment.stretch, children: [
      _field(_resetEmail, '注册邮箱', Icons.mail_outline_rounded,
          keyboard: TextInputType.emailAddress),
      _codeField(_resetCode, _handleSendResetCode),
      _field(_resetNewPwd, '新密码（6-18位）', Icons.lock_outline_rounded,
          obscure: true,
          focusNode: _resetPwdFocus,
          onSubmit: (_) => _handleReset()),
      const SizedBox(height: 2),
      _primaryBtn('重置密码', _handleReset),
    ]);
  }
}
