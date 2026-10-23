/// InDesk 壳布局 —— 板块 1:1 对齐 web 端 src/layout/index.vue：
/// ① 自绘顶栏 system-bar（品牌+版本 | 使用教程/手机端APP/通知中心/三条杠 | 最小化/关闭）
/// ② 左侧栏 sidebar 160px（头像/用户名、普通版|加强版、主导航：远程控制/设备列表/
///    屏幕墙(预留)/高级设置，辅助组：常见问题/使用反馈群，底部：账号登录）
/// ③ 主区 .view（RouterView → 远程控制 / 设备列表 / 高级设置）
import 'dart:async';
import 'dart:io';

import 'package:flutter/material.dart';
import 'package:flutter/services.dart';
import 'package:window_manager/window_manager.dart';

import '../core/ipc.dart';
import '../core/prefs.dart';
import '../core/shell_log.dart';
import '../core/win32.dart';
import 'account_page.dart';
import 'connect_page.dart';
import 'device_list_page.dart';
import 'main_page.dart';
import 'settings_page.dart';
import 'updater.dart';
import 'widgets/pressable.dart';

class ShellLayout extends StatefulWidget {
  const ShellLayout({super.key, this.autoConnect = const []});

  final List<String> autoConnect;

  @override
  State<ShellLayout> createState() => _ShellLayoutState();
}

class _ShellLayoutState extends State<ShellLayout> with WindowListener {
  int _tab = 0; // 0=远程控制 1=设备列表 2=高级设置
  String _version = '';
  bool _showBurgerMenu = false;
  bool _showNotice = false;
  // [20261017c] 最大化/还原按钮状态（用户反馈右上角缺这颗按钮）
  bool _maximized = false;
  // [20261017] 官网 version.json 实时数据（通知中心/版本更新用）
  String _remoteCoreVer = '';
  String _remoteNotes = '';
  String _remoteApkUrl = defaultApkUrl;
  bool _remoteFetchFailed = false;

  // ===== [2.3.49] 桌面端自更新 =====
  // 官网 version.json 里的 desktop* 字段；null = 服务端没开自更新或拉取失败
  UpdateInfo? _deskUpdate;
  bool _showUpdateCard = false; // 右下角"发现新版本"浮卡
  bool _updating = false; // 下载 / 校验 / 准备安装中
  bool _dlDialogOpen = false; // 进度弹窗是否在屏上
  final ValueNotifier<UpdateProgress?> _dlProgress = ValueNotifier(null);
  Timer? _updateTimer;

  static const _brand = Color(0xFF2F6FED);

  // ===== [20261017] 品牌/联系信息 —— 对齐 web 端 src/constant.ts BRAND_INFO =====
  // 铁律：界面展示的联系方式一律来自这组常量（原版「品牌收口」原则）。
  static const kShellVersion = '2.3.49'; // 与 installer.iss MyAppVersion / pubspec 同步
  static const brandFeedbackGroup = '973145008'; // 使用反馈群群号
  static const brandEmail = '7892134@qq.com'; // 客服邮箱
  static const brandSite = 'https://desk.yomm.top'; // 官网
  static const webDeskUrl = 'https://desk.yomm.top:8443'; // 手机端服务地址
  // 安卓安装包兜底地址：仅在 version.json 拉取失败时使用。
  // 必须跟着最新 APK 走（1.3.0 是带 6 张 1.3MB 头像的旧包，10MB；1.3.3 起 1MB）。
  static const defaultApkUrl =
      'https://desk.yomm.top:8443/updates/InDesk_1.3.3.apk'; // 安卓安装包

  @override
  void initState() {
    super.initState();
    windowManager.addListener(this);
    _loadVersion();
    windowManager.setAlwaysOnTop(ShellPrefs.alwaysOnTop);
    // [20261017c] 初始化最大化状态（上次会话可能残留最大化）
    windowManager.isMaximized().then((m) {
      if (mounted) setState(() => _maximized = m);
    });
    // [20261017] 拉取官网版本信息（version.json，与 web 端 use-auto-update 同源），
    // 通知中心/版本更新弹窗用真实远端数据，不再是无内容占位。
    // [2.3.49] 顺带做桌面端自更新检查：有新版就在右下角出一张浮卡（不打断操作）。
    _fetchRemoteVersion();
    // 常驻托盘的程序一开就是几天，6 小时再看一眼，别让人一直停在旧版
    _updateTimer = Timer.periodic(const Duration(hours: 6), (_) {
      if (!_updating) _fetchRemoteVersion();
    });
  }

  // [20261017c] 跟踪窗口最大化状态变化，同步顶栏按钮图标
  @override
  void onWindowMaximize() {
    if (mounted) setState(() => _maximized = true);
  }

  @override
  void onWindowUnmaximize() {
    if (mounted) setState(() => _maximized = false);
  }

  @override
  void dispose() {
    _updateTimer?.cancel();
    _dlProgress.dispose();
    windowManager.removeListener(this);
    super.dispose();
  }

  /// 关窗 = 退出主控：先杀 core 再销毁窗口（防止 indesk-core.exe 残留后台）。
  /// 自绘 X 按钮直连此方法（不依赖 preventClose/onWindowClose 事件链）。
  /// 点 X = 隐藏到系统托盘（进程常驻，core/主控会话全部保留）——
  /// 对齐 ToDesk：点关闭不应退出程序，真正退出走托盘菜单「退出」。
  Future<void> _closeApp() async {
    ShellLog.w('[layout] 关窗 → 隐藏到托盘（进程常驻）');
    _closePopups();
    await windowManager.hide();
  }

  Future<void> _loadVersion() async {
    final v = await Ipc.instance.getShellVersion();
    if (!mounted) return;
    // core 返回 {code:0,data:{version:1.4.43}}，取内层
    var txt = v?.toString() ?? '';
    final m = RegExp(r'version: ([\d.]+)').firstMatch(txt);
    setState(() => _version = m?.group(1) ?? '');
  }

  // ===== [20261017] 官网版本探测（对齐 web 端 use-auto-update 数据源）=====
  // [2.3.49] 改为读 desktop* 字段。旧逻辑拿 shellVersion（退役 Tauri 壳的
  // 1.4.43）跟本机 core 的 1.4.48 比大小，方向反了且源是死的 —— 永远不会提示。
  Future<void> _fetchRemoteVersion() async {
    final j = await Updater.fetchJson();
    if (!mounted) return;
    if (j == null) {
      setState(() => _remoteFetchFailed = true);
      return;
    }
    final apk = (j['apkUrl'] ?? '').toString();
    final desk = Updater.parseDesktop(j);
    setState(() {
      _remoteCoreVer = (j['shellVersion'] ?? '').toString();
      _remoteNotes = (j['shellNotes'] ?? j['notes'] ?? '').toString();
      if (apk.isNotEmpty) {
        _remoteApkUrl = apk.startsWith('http')
            ? apk
            : '$webDeskUrl${apk.startsWith('/') ? '' : '/'}$apk';
      }
      _remoteFetchFailed = false;
      _deskUpdate = desk;
      // 有新版且用户没关过这张卡、也没在更新中 → 出浮卡
      _showUpdateCard =
          desk != null && !_updating && Updater.isNewer(desk.version, kShellVersion);
    });
  }

  /// 官网是否有更新的桌面端（比的是真实安装包版本，不再比 core）
  bool get _hasNewRemote =>
      _deskUpdate != null && Updater.isNewer(_deskUpdate!.version, kShellVersion);

  void _copy(String s) {
    Clipboard.setData(ClipboardData(text: s));
    _toast('复制成功！');
  }

  /// 系统默认浏览器打开网址（rundll32 免依赖；失败兜底复制链接）
  Future<void> _openExternal(String url) async {
    try {
      await Process.start('rundll32', ['url.dll,FileProtocolHandler', url],
          mode: ProcessStartMode.detached);
    } catch (_) {
      _copy(url);
    }
  }

  void _showAccountEntry() {
    _closePopups();
    if (ShellPrefs.token.isNotEmpty) {
      _showAccountInfo();
    } else {
      _openAccountPage();
    }
  }

  Future<void> _openAccountPage() async {
    // 对齐 web handleAccountEntry：未登录 → 登录页（登录/注册/忘记密码）
    final ok = await Navigator.of(context).push(MaterialPageRoute<bool>(
      builder: (_) => const AccountPage(),
    ));
    if (ok == true && mounted) setState(() {}); // 侧栏用户名/底部项刷新
  }

  void _showAccountInfo() {
    _closePopups();
    showDialog<void>(
      context: context,
      builder: (ctx) => AlertDialog(
        shape: RoundedRectangleBorder(borderRadius: BorderRadius.circular(14)),
        title: Row(children: [
          Container(
            width: 30,
            height: 30,
            alignment: Alignment.center,
            decoration: const BoxDecoration(
                shape: BoxShape.circle,
                gradient: LinearGradient(
                    colors: [Color(0xFF1E54C8), _brand])),
            child: Text(
                ShellPrefs.username.isNotEmpty
                    ? ShellPrefs.username.characters.first.toUpperCase()
                    : '?',
                style: const TextStyle(
                    color: Colors.white,
                    fontSize: 13,
                    fontWeight: FontWeight.w700)),
          ),
          const SizedBox(width: 10),
          Expanded(
            child: Text(
                ShellPrefs.username.isEmpty ? '账号信息' : ShellPrefs.username,
                style: const TextStyle(fontSize: 16)),
          ),
        ]),
        content: Column(
          mainAxisSize: MainAxisSize.min,
          crossAxisAlignment: CrossAxisAlignment.start,
          children: [
            _accountRow('用户名', ShellPrefs.username.isEmpty ? '—' : ShellPrefs.username),
            _accountRow('账号ID', ShellPrefs.accountId.isEmpty ? '—' : ShellPrefs.accountId),
            _accountRow('绑定邮箱', ShellPrefs.email.isEmpty ? '未绑定' : ShellPrefs.email),
          ],
        ),
        actions: [
          TextButton(
              onPressed: () => Navigator.of(ctx).pop(),
              child: const Text('知道了')),
        ],
      ),
    );
  }

  Widget _accountRow(String k, String v) => Padding(
        padding: const EdgeInsets.only(bottom: 8),
        child: Row(children: [
          SizedBox(
              width: 70,
              child: Text(k,
                  style: TextStyle(
                      fontSize: 12.5, color: Colors.grey.shade500))),
          Expanded(
            child: Text(v,
                style: const TextStyle(
                    fontSize: 12.5,
                    color: Color(0xFF182236),
                    fontWeight: FontWeight.w600)),
          ),
        ]),
      );

  Future<void> _logout() async {
    _closePopups();
    ShellPrefs.token = '';
    ShellPrefs.username = '';
    ShellPrefs.accountId = '';
    ShellPrefs.email = '';
    setState(() {});
    _toast('已退出登录');
  }

  void _toast(String msg) {
    if (!mounted) return;
    ScaffoldMessenger.of(context)
      ..hideCurrentSnackBar()
      ..showSnackBar(SnackBar(
          content: Text(msg), behavior: SnackBarBehavior.floating));
  }

  void _closePopups() {
    if (_showBurgerMenu || _showNotice) {
      setState(() {
        _showBurgerMenu = false;
        _showNotice = false;
      });
    }
  }

  // ===== 顶栏弹层内容 =====

  void _showTutorial() {
    _closePopups();
    _infoDialog('使用教程', [
      '① 安装：双击安装包按提示安装，首次启动会自动注册本机设备码（此设备 → 设备代码）。',
      '② 把设备码和临时密码发给对方：点设备代码右侧的复制按钮，一次复制「设备代码 + 临时密码」，发给需要连你这台电脑的人。',
      '③ 对方输入设备码连接：在「远程控制设备」里输入你的 9 位设备码，点「连接」，再输入临时密码即可看到你的屏幕。',
      '④ 无人值守：勾选后本机自动允许远程连接、不弹确认窗，适合长期被控的机器。',
      '⑤ 忘记临时密码：在主界面「此设备 → 临时密码」点铅笔图标修改。',
    ]);
  }

  /// [20261017] 手机端APP —— 对齐 web 端原版弹窗：服务地址 + 安卓安装包
  ///（apkUrl 取自官网 version.json 实时数据，点击复制 / 打开下载）
  void _showMobileApp() {
    _closePopups();
    showDialog<void>(
      context: context,
      builder: (ctx) => AlertDialog(
        shape: RoundedRectangleBorder(borderRadius: BorderRadius.circular(14)),
        title: const Text('手机端 APP',
            style: TextStyle(fontSize: 17, fontWeight: FontWeight.w700)),
        content: Column(
          mainAxisSize: MainAxisSize.min,
          crossAxisAlignment: CrossAxisAlignment.start,
          children: [
            const Text('InDesk 安卓端',
                style: TextStyle(
                    fontSize: 13.5,
                    fontWeight: FontWeight.w700,
                    color: Color(0xFF182236))),
            const SizedBox(height: 4),
            const Text('手机端为安卓 WebView 壳，装好后访问本服务地址即可远程控制电脑。',
                style:
                    TextStyle(fontSize: 12.5, height: 1.6, color: Color(0xFF3A465C))),
            const SizedBox(height: 12),
            _copyRow('服务地址', webDeskUrl, () => _copy(webDeskUrl)),
            const SizedBox(height: 8),
            _copyRow('安卓安装包', '点击复制下载地址（v${_remoteApkUrl.split('/').last})',
                () => _copy(_remoteApkUrl)),
          ],
        ),
        actions: [
          TextButton(
              onPressed: () => Navigator.of(ctx).pop(),
              child: const Text('关闭')),
          FilledButton(
              onPressed: () => _openExternal(_remoteApkUrl),
              child: const Text('打开下载')),
        ],
      ),
    );
  }

  /// [20261017] 关于我们 —— 对齐 web 端 views/about：logo + 名称 + 版本 + 版权 + 官网链接
  void _showAbout() {
    _closePopups();
    showDialog<void>(
      context: context,
      builder: (ctx) => AlertDialog(
        shape: RoundedRectangleBorder(borderRadius: BorderRadius.circular(14)),
        content: Column(mainAxisSize: MainAxisSize.min, children: [
          ClipRRect(
              borderRadius: BorderRadius.circular(10),
              child: Image.asset('assets/brand/indesk.png',
                  width: 72, height: 72)),
          const SizedBox(height: 12),
          const Text('InDesk',
              style: TextStyle(
                  fontSize: 17,
                  fontWeight: FontWeight.w800,
                  color: Color(0xFF182236))),
          const SizedBox(height: 6),
          Text('界面版本 v$kShellVersion · 核心版本 v$_version',
              style: TextStyle(fontSize: 12.5, color: Colors.grey.shade600)),
          const SizedBox(height: 16),
          Text('Copyright © InDesk',
              style: TextStyle(fontSize: 12, color: Colors.grey.shade500)),
          InkWell(
            onTap: () => _openExternal(brandSite),
            child: Text(brandSite,
                style: const TextStyle(
                    fontSize: 12,
                    color: _brand,
                    fontWeight: FontWeight.w700)),
          ),
        ]),
        actions: [
          TextButton(
              onPressed: () => Navigator.of(ctx).pop(),
              child: const Text('知道了')),
        ],
      ),
    );
  }

  /// [20261017] 联系客服 —— 对齐 web 端 handleContact：BRAND_INFO 真实联系方式
  void _showContact() {
    _closePopups();
    showDialog<void>(
      context: context,
      builder: (ctx) => AlertDialog(
        shape: RoundedRectangleBorder(borderRadius: BorderRadius.circular(14)),
        title: const Text('联系客服',
            style: TextStyle(fontSize: 17, fontWeight: FontWeight.w700)),
        content: const Text(
            '使用反馈群：$brandFeedbackGroup\n邮箱：$brandEmail\n\n点击下方按钮可复制反馈群号',
            style: TextStyle(fontSize: 13, height: 1.7, color: Color(0xFF3A465C))),
        actions: [
          TextButton(
              onPressed: () => Navigator.of(ctx).pop(),
              child: const Text('关闭')),
          FilledButton(
              onPressed: () => _copy(brandFeedbackGroup),
              child: const Text('复制群号')),
        ],
      ),
    );
  }

  /// [20261017] 版本更新 —— 对齐 web 端 views/version：当前版本 + 官网最新版本
  /// + 更新说明（version.json 实时数据）+ 官网下载入口
  /// [2.3.49] 加「立即更新」：下载 → 校验 → 静默安装 → 自动重启，不用再开浏览器
  void _showVersionUpdate() {
    _closePopups();
    final up = _deskUpdate;
    final hasNew = _hasNewRemote;
    final notes = (up != null && up.notes.isNotEmpty) ? up.notes : _remoteNotes;
    showDialog<void>(
      context: context,
      builder: (ctx) => AlertDialog(
        shape: RoundedRectangleBorder(borderRadius: BorderRadius.circular(14)),
        title: const Text('版本更新',
            style: TextStyle(fontSize: 17, fontWeight: FontWeight.w700)),
        content: SizedBox(
          width: 360,
          child: Column(
            mainAxisSize: MainAxisSize.min,
            crossAxisAlignment: CrossAxisAlignment.start,
            children: [
              _verRow('当前界面版本', 'v$kShellVersion'),
              _verRow('当前核心版本', _version.isEmpty ? '检测中…' : 'v$_version'),
              _verRow(
                  '官网最新版本',
                  up == null
                      ? (_remoteFetchFailed ? '获取失败' : '未开启自更新')
                      : 'v${up.version}'),
              if (hasNew) ...[
                const SizedBox(height: 4),
                Container(
                  padding: const EdgeInsets.fromLTRB(12, 10, 12, 10),
                  decoration: BoxDecoration(
                    color: const Color(0xFFEDF3FE),
                    borderRadius: BorderRadius.circular(10),
                    border: Border.all(color: const Color(0xFFDCE7F8)),
                  ),
                  child: Row(children: [
                    const Icon(Icons.system_update_alt_rounded,
                        size: 18, color: _brand),
                    const SizedBox(width: 8),
                    Expanded(
                      child: Column(
                        crossAxisAlignment: CrossAxisAlignment.start,
                        children: [
                          Text(
                              '发现新版本 v${up!.version}'
                              '${up.sizeText.isEmpty ? '' : ' · ${up.sizeText}'}',
                              style: const TextStyle(
                                  fontSize: 12.5,
                                  fontWeight: FontWeight.w700,
                                  color: Color(0xFF182236))),
                          const SizedBox(height: 2),
                          const Text('点「立即更新」自动下载安装，装完自动重启；已保存的设置和登录状态都不动。',
                              style: TextStyle(
                                  fontSize: 11.5,
                                  height: 1.5,
                                  color: Color(0xFF6B7688))),
                        ],
                      ),
                    ),
                  ]),
                ),
              ],
              const SizedBox(height: 10),
              const Text('更新说明',
                  style: TextStyle(
                      fontSize: 12.5,
                      fontWeight: FontWeight.w700,
                      color: Color(0xFF182236))),
              const SizedBox(height: 4),
              ConstrainedBox(
                constraints: const BoxConstraints(maxHeight: 160),
                child: SingleChildScrollView(
                  child: Text(
                      notes.isEmpty
                          ? (_remoteFetchFailed ? '暂无更新说明（网络获取失败）' : '暂无更新说明')
                          : notes,
                      style: const TextStyle(
                          fontSize: 12, height: 1.6, color: Color(0xFF6B7688))),
                ),
              ),
            ],
          ),
        ),
        actions: [
          TextButton(
              onPressed: () {
                Navigator.of(ctx).pop();
                _fetchRemoteVersion();
              },
              child: const Text('刷新')),
          TextButton(
              onPressed: () => _openExternal(brandSite),
              child: const Text('官网下载')),
          if (hasNew)
            FilledButton(
                onPressed: () {
                  Navigator.of(ctx).pop();
                  _startUpdate();
                },
                child: const Text('立即更新'))
          else
            TextButton(
                onPressed: () => Navigator.of(ctx).pop(),
                child: const Text('关闭')),
        ],
      ),
    );
  }

  // ===== [2.3.49] 自更新流程 =====

  /// 下载 → 校验 → 确认 → 静默安装 → 退出交给安装器拉起新版
  Future<void> _startUpdate() async {
    final up = _deskUpdate;
    if (up == null || _updating) return;
    if (!_hasNewRemote) {
      _toast('已经是最新版本 v$kShellVersion');
      return;
    }
    setState(() {
      _updating = true;
      _showUpdateCard = false;
    });
    _dlProgress.value = UpdateProgress(0, up.size, 0);
    _showDownloadDialog(up);
    try {
      final f = await Updater.download(up, onProgress: (p) {
        _dlProgress.value = p;
      });
      final err = await Updater.verify(f, up);
      if (err != null) throw Exception(err);
      _closeDownloadDialog();
      if (!mounted) return;
      final go = await _confirmInstall(up);
      if (go != true) {
        setState(() => _updating = false);
        return;
      }
      await Updater.launchInstaller(f);
      // 给安装器一点时间拿到文件句柄 / 弹 UAC，然后干净退出把文件让出来
      await Future.delayed(const Duration(milliseconds: 900));
      await Updater.quitForUpdate();
    } catch (e) {
      _closeDownloadDialog();
      if (!mounted) return;
      setState(() => _updating = false);
      _toast(e is UpdateCancelled
          ? '已取消更新'
          : '更新失败：${e.toString().replaceFirst('Exception: ', '')}');
    }
  }

  void _showDownloadDialog(UpdateInfo up) {
    _dlDialogOpen = true;
    showDialog<void>(
      context: context,
      barrierDismissible: false,
      builder: (ctx) => AlertDialog(
        shape: RoundedRectangleBorder(borderRadius: BorderRadius.circular(14)),
        title: Text('正在下载 v${up.version}',
            style: const TextStyle(fontSize: 16, fontWeight: FontWeight.w700)),
        content: SizedBox(
          width: 320,
          child: Column(
            mainAxisSize: MainAxisSize.min,
            crossAxisAlignment: CrossAxisAlignment.start,
            children: [
              ValueListenableBuilder<UpdateProgress?>(
                valueListenable: _dlProgress,
                builder: (_, p, __) {
                  final ratio = p?.ratio ?? 0;
                  return Column(
                    crossAxisAlignment: CrossAxisAlignment.start,
                    children: [
                      ClipRRect(
                        borderRadius: BorderRadius.circular(4),
                        child: LinearProgressIndicator(
                          value: ratio <= 0 ? null : ratio,
                          minHeight: 7,
                          backgroundColor: const Color(0xFFEDF1F7),
                          valueColor: const AlwaysStoppedAnimation(_brand),
                        ),
                      ),
                      const SizedBox(height: 8),
                      Row(children: [
                        Text('${(ratio * 100).toStringAsFixed(0)}%',
                            style: const TextStyle(
                                fontSize: 12.5,
                                fontWeight: FontWeight.w700,
                                color: Color(0xFF182236))),
                        const SizedBox(width: 8),
                        Text(
                            [
                              if (p != null) p.receivedText,
                              if (p != null && p.speedText.isNotEmpty) p.speedText,
                            ].join(' · '),
                            style: const TextStyle(
                                fontSize: 11.5, color: Color(0xFF6B7688))),
                      ]),
                    ],
                  );
                },
              ),
              const SizedBox(height: 10),
              const Text('下载完成后自动校验文件完整性，再静默安装。',
                  style: TextStyle(
                      fontSize: 11.5, height: 1.5, color: Color(0xFF6B7688))),
            ],
          ),
        ),
        actions: [
          TextButton(
              onPressed: () {
                Updater.cancel();
                Navigator.of(ctx).pop();
                _dlDialogOpen = false;
              },
              child: const Text('取消下载')),
        ],
      ),
    );
  }

  void _closeDownloadDialog() {
    if (!_dlDialogOpen) return;
    _dlDialogOpen = false;
    if (mounted) Navigator.of(context).pop();
  }

  Future<bool?> _confirmInstall(UpdateInfo up) {
    return showDialog<bool>(
      context: context,
      builder: (ctx) => AlertDialog(
        shape: RoundedRectangleBorder(borderRadius: BorderRadius.circular(14)),
        title: Text('安装 v${up.version}',
            style: const TextStyle(fontSize: 16, fontWeight: FontWeight.w700)),
        content: const Text(
            '点「立即安装」后，主程序会关闭并自动完成安装，装完自动重新打开。\n\n'
            '注意两件事：\n'
            '· 如果弹出 Windows 管理员授权窗口，请点「是」，否则装不上；\n'
            '· 正在进行的远程会话会断开，建议等这次会话结束再更新。',
            style: TextStyle(fontSize: 12.5, height: 1.7, color: Color(0xFF3A465C))),
        actions: [
          TextButton(
              onPressed: () => Navigator.of(ctx).pop(false),
              child: const Text('稍后')),
          FilledButton(
              onPressed: () => Navigator.of(ctx).pop(true),
              child: const Text('立即安装')),
        ],
      ),
    );
  }

  /// [20261017] 常见问题 —— 对齐 web 端 handleOpenFaq 原文
  void _showFaq() {
    _closePopups();
    _infoDialog('常见问题', [
      '1. 连不上：请确认对方已开机并登录 InDesk，设备码与临时密码输入正确。',
      '2. 画面卡顿：在连接详情里把画质档位调低，或开启 H264 直送。',
      '3. 忘记临时密码：在主界面「此设备 → 临时密码」点铅笔图标修改。',
      '4. 更多问题请进「使用反馈群」反馈。',
    ]);
  }

  /// [20261017] 使用反馈群 —— 对齐 web 端 handleOpenFeedback 原文（群号 973145008）
  void _showFeedback() {
    _closePopups();
    showDialog<void>(
      context: context,
      builder: (ctx) => AlertDialog(
        shape: RoundedRectangleBorder(borderRadius: BorderRadius.circular(14)),
        title: const Text('使用反馈群',
            style: TextStyle(fontSize: 17, fontWeight: FontWeight.w700)),
        content: const Text(
            '使用反馈群群号：$brandFeedbackGroup\n\n加群请填写 ID 号，为验证使用。\n点击下方按钮复制群号，进群反馈问题与建议。',
            style: TextStyle(fontSize: 13, height: 1.7, color: Color(0xFF3A465C))),
        actions: [
          TextButton(
              onPressed: () => Navigator.of(ctx).pop(),
              child: const Text('关闭')),
          FilledButton(
              onPressed: () => _copy(brandFeedbackGroup),
              child: const Text('复制群号')),
        ],
      ),
    );
  }

  /// 复制行（手机端APP 弹窗用）：左标签 + 右可点值
  Widget _copyRow(String label, String value, VoidCallback onTap) {
    return Row(children: [
      SizedBox(
          width: 70,
          child: Text(label,
              style: TextStyle(fontSize: 12.5, color: Colors.grey.shade600))),
      Expanded(
        child: Pressable(
          onTap: onTap,
          radius: 6,
          child: Padding(
            padding: const EdgeInsets.symmetric(horizontal: 6, vertical: 4),
            child: Text(value,
                style: const TextStyle(
                    fontSize: 12.5,
                    color: _brand,
                    fontWeight: FontWeight.w600)),
          ),
        ),
      ),
    ]);
  }

  Widget _verRow(String k, String v) {
    return Padding(
      padding: const EdgeInsets.only(bottom: 6),
      child: Row(children: [
        SizedBox(
            width: 96,
            child: Text(k,
                style:
                    TextStyle(fontSize: 12.5, color: Colors.grey.shade600))),
        Text(v,
            style: const TextStyle(
                fontSize: 12.5,
                fontWeight: FontWeight.w700,
                color: Color(0xFF182236))),
      ]),
    );
  }

  /// [2.3.49] 右下角"发现新版本"浮卡
  Widget _updateCard() {
    final up = _deskUpdate!;
    return Container(
      width: 316,
      padding: const EdgeInsets.fromLTRB(14, 12, 12, 10),
      decoration: BoxDecoration(
        color: Colors.white,
        borderRadius: BorderRadius.circular(12),
        border: Border.all(color: const Color(0xFFDCE7F8)),
        boxShadow: [
          BoxShadow(
              color: Colors.black.withOpacity(.08),
              blurRadius: 20,
              offset: const Offset(0, 6)),
        ],
      ),
      child: Column(
        mainAxisSize: MainAxisSize.min,
        crossAxisAlignment: CrossAxisAlignment.start,
        children: [
          Row(children: [
            Container(
              width: 28,
              height: 28,
              decoration: BoxDecoration(
                  color: _brand.withOpacity(.10),
                  borderRadius: BorderRadius.circular(8)),
              child: const Icon(Icons.system_update_alt_rounded,
                  size: 17, color: _brand),
            ),
            const SizedBox(width: 9),
            Expanded(
              child: Text('发现新版本 v${up.version}',
                  style: const TextStyle(
                      fontSize: 13,
                      fontWeight: FontWeight.w700,
                      color: Color(0xFF182236))),
            ),
            InkWell(
              onTap: () => setState(() => _showUpdateCard = false),
              borderRadius: BorderRadius.circular(6),
              child: const Padding(
                padding: EdgeInsets.all(3),
                child: Icon(Icons.close_rounded,
                    size: 15, color: Color(0xFF9AA5B4)),
              ),
            ),
          ]),
          const SizedBox(height: 6),
          Text(
              up.sizeText.isEmpty
                  ? '一键下载安装，装完自动重启'
                  : '${up.sizeText} · 一键下载安装，装完自动重启',
              style: const TextStyle(
                  fontSize: 11.5, height: 1.5, color: Color(0xFF6B7688))),
          const SizedBox(height: 10),
          Row(children: [
            FilledButton(
              onPressed: _startUpdate,
              style: FilledButton.styleFrom(
                backgroundColor: _brand,
                minimumSize: const Size(0, 32),
                padding: const EdgeInsets.symmetric(horizontal: 14),
                shape: RoundedRectangleBorder(
                    borderRadius: BorderRadius.circular(9)),
              ),
              child: const Text('立即更新',
                  style: TextStyle(
                      fontSize: 12.5, fontWeight: FontWeight.w600)),
            ),
            const SizedBox(width: 4),
            TextButton(
              onPressed: () => setState(() => _showUpdateCard = false),
              style: TextButton.styleFrom(
                foregroundColor: const Color(0xFF6B7688),
                minimumSize: const Size(0, 32),
                padding: const EdgeInsets.symmetric(horizontal: 10),
              ),
              child: const Text('稍后', style: TextStyle(fontSize: 12.5)),
            ),
          ]),
        ],
      ),
    );
  }

  void _infoDialog(String title, List<String> lines) {
    showDialog<void>(
      context: context,
      builder: (ctx) => AlertDialog(
        shape: RoundedRectangleBorder(borderRadius: BorderRadius.circular(14)),
        title: Text(title, style: const TextStyle(fontSize: 17)),
        content: Column(
          mainAxisSize: MainAxisSize.min,
          crossAxisAlignment: CrossAxisAlignment.start,
          children: [
            for (final l in lines)
              Padding(
                padding: const EdgeInsets.only(bottom: 8),
                child: Text(l,
                    style: const TextStyle(
                        fontSize: 12.5, height: 1.6, color: Color(0xFF3A465C))),
              ),
          ],
        ),
        actions: [
          TextButton(
              onPressed: () => Navigator.of(ctx).pop(),
              child: const Text('知道了')),
        ],
      ),
    );
  }

  @override
  Widget build(BuildContext context) {
    if (widget.autoConnect.length == 2) {
      // E2E/命令行直连：跳过壳布局（语义与 web 端 --connect 一致）
      return ConnectPage(
          prefillCode: widget.autoConnect[0],
          prefillPassword: widget.autoConnect[1]);
    }
    return Scaffold(
      backgroundColor: const Color(0xFFF2F7FE),
      // [20261016b] 不再加 Dart 层最大化补偿：窗口边框由 runner 层
      // WM_NCCALCSIZE 原生接管（客户区=全窗口），插件+Dart 双重补偿
      // 反而造成「边框重叠」。内容天然贴合窗口边缘。
      //
      // [20261017b] 顶栏下拉面板（三条杠菜单/通知中心）此前挂在顶栏
      // Stack 里 Positioned(top:38)：① Stack 只有 40 高，默认
      // Clip.hardEdge 把溢出面板整个裁掉；② 即使不裁，Column 里后绘制的
      // Expanded 主区也会盖住溢出内容 → 用户看到「点了没反应」。
      // 修复：面板提到 body 顶层 Stack（绘制在主区之上），并加全屏
      // 透明遮罩（点击外部关闭面板）。
      body: Stack(children: [
        Column(children: [
          _buildSystemBar(),
          Expanded(
            child: Row(
              crossAxisAlignment: CrossAxisAlignment.stretch,
              children: [
                _buildSidebar(),
                Expanded(
                  child: IndexedStack(
                    index: _tab,
                    children: const [
                      MainPage(),
                      DeviceListPage(),
                      SettingsPage(),
                    ],
                  ),
                ),
              ],
            ),
          ),
        ]),
        // 点击面板外任意区域 → 关闭面板（面板自身在遮罩之上，不受影响）
        if (_showBurgerMenu || _showNotice)
          Positioned.fill(
            child: GestureDetector(
              behavior: HitTestBehavior.opaque,
              onTap: _closePopups,
              child: const SizedBox.expand(),
            ),
          ),
        if (_showBurgerMenu)
          Positioned(top: 38, right: 90, child: _burgerPanel()),
        if (_showNotice)
          Positioned(top: 38, right: 40, child: _noticePanel()),
        // [2.3.49] 发现新版本浮卡：右下角轻提示，不抢焦点、不遮界面
        // bottom:62 是为了让开底部 floating SnackBar（toast）
        if (_showUpdateCard && _hasNewRemote && !_updating)
          Positioned(right: 16, bottom: 62, child: _updateCard()),
      ]),
    );
  }

  // ===== ① 顶栏 system-bar =====
  // [20261016b] 按钮必须在 DragToMoveArea 之外：DragToMoveArea 带
  // onDoubleTap（双击最大化），按钮在其子树内会因手势竞技场等待双击
  // 判定超时，点击响应固定慢 ~300ms（用户反馈"最小化/关闭反应慢"）。
  // 左侧品牌+空白区拖拽，右侧按钮独立直连。
  // [20261017b] 下拉面板已移至 body 顶层 Stack（见 build 注释）。
  Widget _buildSystemBar() {
    return Container(
      height: 40,
      color: Colors.white,
      child: Row(children: [
          Expanded(
            child: DragToMoveArea(
              child: Row(children: [
                const SizedBox(width: 14),
                ClipRRect(
                    borderRadius: BorderRadius.circular(7),
                    child: Image.asset('assets/brand/indesk.png',
                        width: 22, height: 22)),
                const SizedBox(width: 8),
                Text('InDesk',
                    style: TextStyle(
                        fontSize: 13.5,
                        fontWeight: FontWeight.w800,
                        color: Colors.grey.shade800)),
                const SizedBox(width: 6),
                // [20261019c] 顶栏显示壳版本（随安装包更新），核心版本
                // 作小字补充 —— 用户核对「装没装新版」看这里，不再误认
                // 核心固定版本 1.4.43 为未更新。
                Text('v$kShellVersion',
                    style: TextStyle(
                        fontSize: 11.5,
                        fontWeight: FontWeight.w700,
                        color: Colors.grey.shade600)),
                Text(' · 核心 v$_version',
                    style: TextStyle(
                        fontSize: 10.5, color: Colors.grey.shade400)),
                const Spacer(),
              ]),
            ),
          ),
          _barBtn(Icons.menu_book_outlined, '使用教程', _showTutorial),
          _barBtn(Icons.smartphone_outlined, '手机端APP', _showMobileApp),
          // [20261017] 有新版本时按钮上挂红点角标（对齐原版 .badge）
          Stack(clipBehavior: Clip.none, children: [
            _barBtn(Icons.notifications_outlined, '通知中心', () {
              setState(() {
                _showNotice = !_showNotice;
                _showBurgerMenu = false;
              });
            }),
            if (_hasNewRemote)
              Positioned(
                top: 2,
                right: 2,
                child: Container(
                  width: 7,
                  height: 7,
                  decoration: const BoxDecoration(
                      color: Color(0xFFE5484D), shape: BoxShape.circle),
                ),
              ),
          ]),
          _barBtn(Icons.menu, '', () {
            setState(() {
              _showBurgerMenu = !_showBurgerMenu;
              _showNotice = false;
            });
          }),
          const SizedBox(width: 4),
          _winBtn(Icons.remove_rounded, minimizeNative),
          // [20261017c] 最大化/还原切换（用户反馈右上角没有这颗按钮）
          _winBtn(
              _maximized
                  ? Icons.filter_none_rounded
                  : Icons.crop_square_rounded,
              () async {
            if (await windowManager.isMaximized()) {
              await windowManager.unmaximize();
            } else {
              await windowManager.maximize();
            }
          }),
          _winBtn(Icons.close_rounded, _closeApp, onClose: true),
          const SizedBox(width: 10),
      ]),
    );
  }

  Widget _barBtn(IconData icon, String label, VoidCallback onTap) {
    // [20261017] Pressable：按下叠加高亮层，点击有实体反馈
    return Pressable(
      onTap: onTap,
      radius: 6,
      child: Container(
        padding: const EdgeInsets.symmetric(horizontal: 9, vertical: 5),
        child: Row(children: [
          Icon(icon, size: 15, color: Colors.grey.shade600),
          if (label.isNotEmpty) ...[
            const SizedBox(width: 4),
            Text(label,
                style: TextStyle(fontSize: 12, color: Colors.grey.shade700)),
          ],
        ]),
      ),
    );
  }

  Widget _winBtn(IconData icon, void Function()? action,
      {bool onClose = false}) {
    return Pressable(
      onTap: action,
      radius: 6,
      child: Container(
        width: 30,
        height: 26,
        alignment: Alignment.center,
        child: Icon(icon,
            size: onClose ? 17 : 15,
            color: onClose ? Colors.grey.shade700 : Colors.grey.shade600),
      ),
    );
  }

  Widget _burgerPanel() {
    return Material(
      elevation: 6,
      borderRadius: BorderRadius.circular(10),
      child: Container(
        width: 120,
        decoration: BoxDecoration(
            color: Colors.white, borderRadius: BorderRadius.circular(10)),
        child: Column(children: [
          _menuItem('关于我们', _showAbout),
          _menuItem('联系客服', _showContact),
          _menuItem('版本更新', _showVersionUpdate),
        ]),
      ),
    );
  }

  /// [20261017] 通知中心 —— 对齐 web 端原版语义：
  /// 有新版本 → 「发现新版本」通知（点击看更新说明）；
  /// 无更新 → 当前版本信息条（点击进版本更新详情），不再是空白占位。
  Widget _noticePanel() {
    return Material(
      elevation: 6,
      borderRadius: BorderRadius.circular(10),
      child: Container(
        width: 240,
        decoration: BoxDecoration(
            color: Colors.white, borderRadius: BorderRadius.circular(10)),
        child: Column(crossAxisAlignment: CrossAxisAlignment.start, children: [
          Padding(
            padding: const EdgeInsets.fromLTRB(12, 12, 12, 4),
            child: Text('通知中心',
                style: TextStyle(
                    fontSize: 13,
                    fontWeight: FontWeight.w700,
                    color: Colors.grey.shade800)),
          ),
          Pressable.builder(
            radius: 0,
            onTap: () {
              setState(() => _showNotice = false);
              _showVersionUpdate();
            },
            builder: (pressed) => Container(
              width: double.infinity,
              color: pressed ? const Color(0xFFEDF1F7) : Colors.transparent,
              padding: const EdgeInsets.symmetric(horizontal: 12, vertical: 9),
              child: Column(
                  crossAxisAlignment: CrossAxisAlignment.start,
                  children: [
                    Row(children: [
                      Icon(
                          _hasNewRemote
                              ? Icons.system_update_rounded
                              : Icons.verified_outlined,
                          size: 15,
                          color: _hasNewRemote
                              ? const Color(0xFFD97706)
                              : const Color(0xFF21B573)),
                      const SizedBox(width: 6),
                      Expanded(
                        child: Text(
                            _hasNewRemote
                                ? '发现新版本 v${_deskUpdate?.version ?? _remoteCoreVer}'
                                : '当前版本 v$kShellVersion',
                            style: const TextStyle(
                                fontSize: 12.5,
                                fontWeight: FontWeight.w700,
                                color: Color(0xFF182236))),
                      ),
                    ]),
                    const SizedBox(height: 3),
                    Text(
                        _hasNewRemote
                            ? '点击立即下载并安装，装完自动重启'
                            : '核心 v${_version.isEmpty ? '…' : _version} · 点击查看版本详情',
                        style: TextStyle(
                            fontSize: 11.5, color: Colors.grey.shade500)),
                  ]),
            ),
          ),
          const SizedBox(height: 4),
        ]),
      ),
    );
  }

  Widget _menuItem(String label, VoidCallback onTap) {
    return Pressable(
      onTap: onTap,
      radius: 10,
      child: Container(
        width: double.infinity,
        padding: const EdgeInsets.symmetric(horizontal: 14, vertical: 10),
        child: Text(label, style: const TextStyle(fontSize: 12.5)),
      ),
    );
  }

  // ===== ② 侧栏 sidebar =====
  Widget _buildSidebar() {
    return Container(
      width: 160,
      color: Colors.white,
      child: Column(crossAxisAlignment: CrossAxisAlignment.stretch, children: [
        const SizedBox(height: 10),
        // 头像 + 用户名（登录态跟随 ShellPrefs）
        Pressable(
          onTap: _showAccountEntry,
          radius: 0,
          child: Column(children: [
            Container(
              width: 34,
              height: 34,
              alignment: Alignment.center,
              decoration: const BoxDecoration(
                shape: BoxShape.circle,
                gradient: LinearGradient(colors: [
                  Color(0xFF1E54C8),
                  _brand
                ]),
              ),
              child: ShellPrefs.username.isEmpty
                  ? const Icon(Icons.person_rounded, color: Colors.white, size: 19)
                  : Text(
                      ShellPrefs.username.characters.first.toUpperCase(),
                      style: const TextStyle(
                          color: Colors.white,
                          fontSize: 15,
                          fontWeight: FontWeight.w800)),
            ),
            const SizedBox(height: 5),
            Text(ShellPrefs.username.isEmpty ? '未登录' : ShellPrefs.username,
                style: TextStyle(fontSize: 12, color: Colors.grey.shade600)),
            const SizedBox(height: 5),
            Row(mainAxisAlignment: MainAxisAlignment.center, children: [
              Container(
                padding:
                    const EdgeInsets.symmetric(horizontal: 7, vertical: 2),
                decoration: BoxDecoration(
                  color: const Color(0xFFEAF1FE),
                  borderRadius: BorderRadius.circular(4),
                ),
                child: Text('普通版',
                    style: TextStyle(
                        fontSize: 10.5, color: _brand,
                        fontWeight: FontWeight.w600)),
              ),
              const SizedBox(width: 6),
              Text('加强版',
                  style: TextStyle(
                      fontSize: 10.5, color: Colors.grey.shade400)),
            ]),
          ]),
        ),
        const SizedBox(height: 10),
        Divider(height: 1, color: const Color(0xFFE5EAF2)),
        const SizedBox(height: 6),
        _sideItem(Icons.desktop_windows_outlined, '远程控制', 0),
        _sideItem(Icons.grid_view_outlined, '设备列表', 1),
        _sideItem(Icons.monitor_outlined, '屏幕墙', null, soon: true),
        _sideItem(Icons.settings_outlined, '高级设置', 2),
        const Divider(height: 20, color: Color(0xFFE5EAF2)),
        _sideItem(Icons.help_outline_rounded, '常见问题', null,
            onTap: _showFaq),
        _sideItem(Icons.forum_outlined, '使用反馈群', null,
            onTap: _showFeedback),
        const Spacer(),
        Divider(height: 1, color: Colors.grey.shade200),
        if (ShellPrefs.token.isNotEmpty) ...[
          _sideItem(Icons.person_outline_rounded, '账号信息', null,
              onTap: _showAccountInfo),
          _sideItem(Icons.logout_rounded, '退出登录', null, onTap: _logout),
        ] else
          _sideItem(Icons.person_outline_rounded, '账号登录', null,
              onTap: _openAccountPage),
        const SizedBox(height: 8),
      ]),
    );
  }

  Widget _sideItem(IconData icon, String label, int? tabIndex,
      {bool soon = false, VoidCallback? onTap}) {
    final active = tabIndex != null && _tab == tabIndex;
    // [20261017] Pressable.builder：按下时底色加深一档，有实体反馈
    return Pressable.builder(
      onTap: () {
        _closePopups();
        if (tabIndex != null) {
          setState(() => _tab = tabIndex);
        } else if (onTap != null) {
          onTap();
        }
      },
      radius: 8,
      builder: (pressed) => Container(
        margin: const EdgeInsets.symmetric(horizontal: 8, vertical: 1),
        padding: const EdgeInsets.symmetric(horizontal: 12, vertical: 9),
        decoration: BoxDecoration(
          color: active
              ? (pressed ? const Color(0xFFD8E5FD) : const Color(0xFFEAF1FE))
              : (pressed ? const Color(0xFFEDF1F7) : Colors.transparent),
          borderRadius: BorderRadius.circular(8),
        ),
        child: Row(children: [
          Icon(icon,
              size: 16,
              color: active ? _brand : Colors.grey.shade500),
          const SizedBox(width: 9),
          Text(label,
              style: TextStyle(
                  fontSize: 12.5,
                  color: active ? _brand : Colors.grey.shade700,
                  fontWeight: active ? FontWeight.w700 : FontWeight.w400)),
          if (soon) ...[
            const Spacer(),
            Container(
              padding: const EdgeInsets.symmetric(horizontal: 4, vertical: 1),
              decoration: BoxDecoration(
                color: Colors.grey.shade200,
                borderRadius: BorderRadius.circular(3),
              ),
              child: Text('预留',
                  style:
                      TextStyle(fontSize: 9.5, color: Colors.grey.shade500)),
            ),
          ],
        ]),
      ),
    );
  }
}

