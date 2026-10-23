/// 壳偏好持久化 —— %APPDATA%\InDesk\shell_prefs.json
/// 板块与语义对齐 web 端 cacheStore/setting 页：画质档位、无人值守、
/// 隐私屏、会话结束锁屏、主窗口置顶、开机自启。
import 'dart:convert';
import 'dart:io';

class ShellPrefs {
  static const String defaultTier = 'hd'; // 对齐 web DEFAULT_TIER

  static Map<String, dynamic>? _cache;

  static Map<String, dynamic> get _c {
    if (_cache != null) return _cache!;
    try {
      final f = _file();
      if (f.existsSync()) {
        _cache = Map<String, dynamic>.from(
            jsonDecode(f.readAsStringSync()) as Map);
        return _cache!;
      }
    } catch (_) {}
    _cache = <String, dynamic>{};
    return _cache!;
  }

  static File _file() {
    final base = Platform.environment['APPDATA'] ?? Directory.systemTemp.path;
    final dir = Directory('$base\\InDesk');
    if (!dir.existsSync()) dir.createSync(recursive: true);
    return File('${dir.path}\\shell_prefs.json');
  }

  static void _save() {
    try {
      _file().writeAsStringSync(jsonEncode(_c));
    } catch (_) {}
  }

  static String get tierId => _c['tierId'] as String? ?? defaultTier;
  static set tierId(String v) {
    _c['tierId'] = v;
    _save();
  }

  static int get resolution => _c['resolution'] as int? ?? 1080;
  static set resolution(int v) {
    _c['resolution'] = v;
    _save();
  }

  static int get framerate => _c['framerate'] as int? ?? 30;
  static set framerate(int v) {
    _c['framerate'] = v;
    _save();
  }

  static int get bitrateKbps => _c['bitrateKbps'] as int? ?? 8000;
  static set bitrateKbps(int v) {
    _c['bitrateKbps'] = v;
    _save();
  }

  static bool get unattended => _c['unattended'] as bool? ?? false;
  static set unattended(bool v) {
    _c['unattended'] = v;
    _save();
  }

  static bool get privacyAutoOn => _c['privacyAutoOn'] as bool? ?? false;
  static set privacyAutoOn(bool v) {
    _c['privacyAutoOn'] = v;
    _save();
  }

  static bool get lockAfterSession =>
      _c['lockAfterSession'] as bool? ?? false;
  static set lockAfterSession(bool v) {
    _c['lockAfterSession'] = v;
    _save();
  }

  static bool get alwaysOnTop => _c['alwaysOnTop'] as bool? ?? false;
  static set alwaysOnTop(bool v) {
    _c['alwaysOnTop'] = v;
    _save();
  }

  static bool get autoStart => _c['autoStart'] as bool? ?? false;
  static set autoStart(bool v) {
    _c['autoStart'] = v;
    _save();
  }

  /// [20261019m] 上次会话实测的窗口 chrome 高度（标题栏+工具栏，逻辑像素）。
  /// 连接瞬间按「视频比例 + 真实 chrome」生成窗口，首帧即贴合零过渡。
  static double get lastChromeH => (_c['lastChromeH'] as num?)?.toDouble() ?? 0;
  static set lastChromeH(double v) {
    _c['lastChromeH'] = v;
    _save();
  }

  // ===== [20261019k] 上次会话视频尺寸（连接瞬间窗口直接按它定比例）=====
  // 用户铁律：连接上就是最终状态，不许有"先大后缩"的过渡。记住上次
  // 视频宽高，连接发起时窗口直接按该宽高比贴合；真实画面到达后通常
  // 一致 → 零过渡。仅换个分辨率不同的机器时才会在首帧瞬间无声校正。
  static int get lastVideoW => _c['lastVideoW'] as int? ?? 0;
  static set lastVideoW(int v) {
    _c['lastVideoW'] = v;
    _save();
  }

  static int get lastVideoH => _c['lastVideoH'] as int? ?? 0;
  static set lastVideoH(int v) {
    _c['lastVideoH'] = v;
    _save();
  }

  // ===== 账号登录态（对齐 web token 30 天持久化）=====
  static String get token => _c['token'] as String? ?? '';
  static set token(String v) {
    _c['token'] = v;
    _save();
  }

  static String get username => _c['username'] as String? ?? '';
  static set username(String v) {
    _c['username'] = v;
    _save();
  }

  static String get accountId => _c['accountId'] as String? ?? '';
  static set accountId(String v) {
    _c['accountId'] = v;
    _save();
  }

  static String get email => _c['email'] as String? ?? '';
  static set email(String v) {
    _c['email'] = v;
    _save();
  }

  /// 应用画质档位三连值（对齐 web tierParams）
  static void applyTier(String id) {
    switch (id) {
      case 'smooth':
        resolution = 540;
        framerate = 30;
        bitrateKbps = 2000;
        break;
      case 'balanced':
        resolution = 720;
        framerate = 30;
        bitrateKbps = 4000;
        break;
      default:
        resolution = 1080;
        framerate = 30;
        bitrateKbps = 8000; // hd 档标准值，对齐 web qualityTiers 8M
        tierId = 'hd';
        return;
    }
    tierId = id;
  }

  /// [20261018] 会话下发用的画质三连值。
  /// ⚠️ 不再直接读散存三值：旧版本（画质菜单 bug 时代）可能存下
  /// 「档位=高清 但 bitrate=2000」的脏组合（CDP 实测 1080p@2Mbps → 13fps，
  /// 用户感知「不流畅」的直接根源）。档位不是 custom 时一律用档位标准值
  /// 收敛；custom 才尊重散存值。
  static ({int res, int fps, int kbps}) sessionPreset() {
    switch (tierId) {
      case 'smooth':
        return (res: 540, fps: 30, kbps: 2000);
      case 'balanced':
        return (res: 720, fps: 30, kbps: 4000);
      case 'custom':
        return (res: resolution, fps: framerate, kbps: bitrateKbps);
      default:
        return (res: 1080, fps: 30, kbps: 8000);
    }
  }
}
