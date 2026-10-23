/// 桌面端自更新引擎（2.3.49 新增）
/// ---------------------------------------------------------------------------
/// 为什么需要它：Flutter 是 AOT 编译，Dart 代码没法像网页那样"换个 js 就生效"，
/// 界面逻辑的更新只能换安装包。以前的流程是「弹窗 → 打开官网 → 用户自己找包
/// → 下载 → 双击 → 走安装向导」，五步。这里把后四步收进程序里：
///
///   检查（version.json 的 desktopVersion/desktopUrl/desktopSha256）
///     → 下载到 %TEMP%\InDesk\update（带进度、可取消）
///     → 校验字节数与 SHA-256
///     → 静默执行安装器（/SILENT /CLOSEAPPLICATIONS /UPDATED=1）
///     → 安装器装完自动拉起新版（installer.iss 里的 IsUpdateInstall）
///
/// 边界的诚实说明：这仍然是"安装包"，只是全程不用用户操心。真正的二进制级
/// 热更在桌面端做不到（Dart AOT + 原生 DLL），要省安装包只能靠把
/// indesk-core.exe 拆出来单独热更（独立进程，可在线替换）。
library;

import 'dart:async';
import 'dart:convert';
import 'dart:io';

import 'package:crypto/crypto.dart';

import '../core/core_process.dart';
import '../core/shell_log.dart';

/// 官网 version.json 里的桌面端更新信息。缺字段就是没开启自更新。
class UpdateInfo {
  const UpdateInfo({
    required this.version,
    required this.url,
    this.size = 0,
    this.sha256 = '',
    this.notes = '',
  });

  final String version;
  final String url;
  final int size;
  final String sha256;
  final String notes;

  /// 22.3 MB / 980 KB —— 弹窗里给用户看的体积
  String get sizeText {
    if (size <= 0) return '';
    if (size >= 1 << 20) return '${(size / (1 << 20)).toStringAsFixed(1)} MB';
    return '${(size / 1024).round()} KB';
  }

  /// 有 sha256 才做完整性校验；没有就只比字节数
  bool get hasDigest => sha256.trim().length == 64;
}

/// 下载进度快照（150ms 一帧，别拿每一块去刷 UI）
class UpdateProgress {
  const UpdateProgress(this.received, this.total, this.bytesPerSec);
  final int received;
  final int total;
  final double bytesPerSec;

  double get ratio => total <= 0 ? 0 : (received / total).clamp(0.0, 1.0);
  String get receivedText =>
      '${(received / (1 << 20)).toStringAsFixed(1)} / ${(total / (1 << 20)).toStringAsFixed(1)} MB';
  String get speedText => bytesPerSec <= 0
      ? ''
      : '${(bytesPerSec / (1 << 20)).toStringAsFixed(1)} MB/s';
}

class UpdateCancelled implements Exception {
  const UpdateCancelled();
  @override
  String toString() => '已取消下载';
}

class Updater {
  /// 与 ShellLayout.webDeskUrl 同源；桌面端更新信息就挂在同一个文件里，
  /// 不再新增第二个契约（网页端 / 手机端读同一份）。
  static const kJsonUrl = 'https://desk.yomm.top:8443/version.json';

  static bool _cancelled = false;

  static void cancel() => _cancelled = true;

  /// 拉 version.json（带时间戳绕缓存；DIRECT 绕系统代理，对齐其它请求）
  static Future<Map<String, dynamic>?> fetchJson(
      {Duration timeout = const Duration(seconds: 12)}) async {
    HttpClient? client;
    try {
      client = HttpClient();
      // 注意：`..findProxy = (_) => 'DIRECT'..connectionTimeout = x` 会被解析成
      // 闭包体内的级联（`=>` 吞掉后面的 `..`），必须分开写。
      client.findProxy = (_) => 'DIRECT';
      client.connectionTimeout = timeout;
      final uri = Uri.parse(
          '$kJsonUrl?t=${DateTime.now().millisecondsSinceEpoch}');
      final req = await client.getUrl(uri).timeout(timeout);
      final resp = await req.close().timeout(timeout);
      if (resp.statusCode != 200) {
        ShellLog.w('[update] version.json HTTP ${resp.statusCode}');
        return null;
      }
      final text = await resp.transform(utf8.decoder).join().timeout(timeout);
      final j = jsonDecode(text);
      return j is Map<String, dynamic> ? j : null;
    } catch (e) {
      ShellLog.w('[update] 拉 version.json 失败: $e');
      return null;
    } finally {
      client?.close(force: true);
    }
  }

  /// 从 version.json 解析桌面端更新信息。
  /// 老版本服务端没有这些字段 → 返回 null，界面按"未开启自更新"处理。
  static UpdateInfo? parseDesktop(Map<String, dynamic>? j) {
    if (j == null) return null;
    final ver = (j['desktopVersion'] ?? '').toString().trim();
    final url = (j['desktopUrl'] ?? '').toString().trim();
    if (ver.isEmpty || url.isEmpty) return null;
    final size = int.tryParse((j['desktopSize'] ?? '').toString()) ?? 0;
    return UpdateInfo(
      version: ver,
      url: url,
      size: size,
      sha256: (j['desktopSha256'] ?? '').toString().trim(),
      notes: (j['desktopNotes'] ?? '').toString(),
    );
  }

  /// a 是否比 b 新（只比主.次.修订三段，与 shell_layout 的判据一致）
  static bool isNewer(String a, String b) {
    final pa = a.split('.').map((e) => int.tryParse(e.trim()) ?? 0).toList();
    final pb = b.split('.').map((e) => int.tryParse(e.trim()) ?? 0).toList();
    for (var i = 0; i < 3; i++) {
      final x = i < pa.length ? pa[i] : 0;
      final y = i < pb.length ? pb[i] : 0;
      if (x != y) return x > y;
    }
    return false;
  }

  /// 更新包落地目录：%TEMP%\InDesk\update（跟着当前用户，不需要管理员）
  static String get workDir {
    final base = Platform.environment['TEMP'] ??
        Platform.environment['TMP'] ??
        Directory.systemTemp.path;
    return '$base\\InDesk\\update';
  }

  /// 下载安装包到 workDir，返回文件。
  /// 先写 .part 再改名 —— 半截包永远不会被当成完整包执行。
  static Future<File> download(
    UpdateInfo info, {
    required void Function(UpdateProgress) onProgress,
  }) async {
    _cancelled = false;
    final dir = Directory(workDir);
    if (!dir.existsSync()) dir.createSync(recursive: true);

    // 清历史残留（上一版号的包、上次中断的 .part）
    for (final e in dir.listSync()) {
      if (e is! File) continue;
      try {
        e.deleteSync();
      } catch (_) {}
    }

    final name = Uri.parse(info.url).pathSegments.isEmpty
        ? 'indesk-setup.exe'
        : Uri.parse(info.url).pathSegments.last;
    final target = File('${dir.path}\\$name');
    final part = File('${target.path}.part');

    final client = HttpClient();
    client.findProxy = (_) => 'DIRECT';
    client.connectionTimeout = const Duration(seconds: 15);
    client.idleTimeout = const Duration(seconds: 30);

    IOSink? sink;
    try {
      final uri = Uri.parse(
          '${info.url}${info.url.contains('?') ? '&' : '?'}t=${DateTime.now().millisecondsSinceEpoch}');
      final req = await client.getUrl(uri);
      req.headers.set(HttpHeaders.userAgentHeader, 'InDesk-Updater/1.0');
      final resp = await req.close();
      if (resp.statusCode != 200) {
        throw HttpException('HTTP ${resp.statusCode}', uri: uri);
      }
      final total = resp.contentLength > 0 ? resp.contentLength : info.size;
      sink = part.openWrite();
      var got = 0;
      var lastEmit = 0;
      final t0 = DateTime.now();
      await for (final chunk in resp) {
        if (_cancelled) throw const UpdateCancelled();
        got += chunk.length;
        sink.add(chunk);
        final ms = DateTime.now().millisecondsSinceEpoch;
        if (ms - lastEmit >= 150) {
          lastEmit = ms;
          final sec = DateTime.now().difference(t0).inMilliseconds / 1000.0;
          onProgress(UpdateProgress(
              got, total, sec <= 0 ? 0 : got / sec));
        }
      }
      await sink.flush();
      await sink.close();
      sink = null;
      onProgress(UpdateProgress(got, total, 0));

      if (total > 0 && got != total) {
        throw Exception('下载不完整（$got / $total 字节）');
      }
      // Windows 上 rename 不能覆盖同名文件（只在 .part → 正式名时可能撞）
      if (target.existsSync()) {
        try {
          target.deleteSync();
        } catch (_) {}
      }
      part.renameSync(target.path);
      ShellLog.w('[update] 下载完成 ${target.path} ${target.lengthSync()} B');
      return target;
    } catch (_) {
      try {
        if (sink != null) {
          await sink.close();
        }
      } catch (_) {}
      try {
        if (part.existsSync()) part.deleteSync();
      } catch (_) {}
      rethrow;
    } finally {
      client.close(force: true);
    }
  }

  /// 分块算 SHA-256（不用把 22MB 整个读进内存）
  static Future<String> sha256Of(File f) async {
    final out = _DigestSink();
    final input = sha256.startChunkedConversion(out);
    await for (final chunk in f.openRead()) {
      input.add(chunk);
    }
    input.close();
    return out.value?.toString() ?? '';
  }

  /// 校验：字节数必须一致；有 sha256 就必须一致（大小写无关）
  static Future<String?> verify(File f, UpdateInfo info) async {
    if (!f.existsSync()) return '安装包不见了';
    final len = f.lengthSync();
    if (info.size > 0 && len != info.size) {
      return '体积不符（期望 ${info.size}，实际 $len）';
    }
    if (info.hasDigest) {
      final actual = await sha256Of(f);
      if (actual.toLowerCase() != info.sha256.toLowerCase()) {
        return 'SHA-256 不符（期望 ${info.sha256.substring(0, 12)}…，实际 ${actual.substring(0, 12)}…）';
      }
    } else {
      ShellLog.w('[update] version.json 未提供 desktopSha256，只校了体积');
    }
    return null;
  }

  /// 静默启动安装器。
  /// /SILENT          显示进度窗但不需要交互
  /// /SUPPRESSMSGBOXES 不要弹任何确认框
  /// /CLOSEAPPLICATIONS Restart Manager 兜底关掉还占着文件的进程
  /// /NORESTART       绝不重启系统
  /// /UPDATED=1       自定义参数 —— 安装完由 installer.iss 自动拉起 indesk.exe
  static Future<void> launchInstaller(File setup) async {
    ShellLog.w('[update] 启动安装器: ${setup.path}');
    await Process.start(
      setup.path,
      const [
        '/SILENT',
        '/SUPPRESSMSGBOXES',
        '/CLOSEAPPLICATIONS',
        '/NORESTART',
        '/UPDATED=1',
      ],
      mode: ProcessStartMode.detached,
    );
  }

  /// 干净退出，把文件句柄让给安装器：先杀被控壳，再停主控核，最后 exit(0)。
  static Future<void> quitForUpdate() async {
    try {
      await Process.run('taskkill', ['/f', '/im', 'indesk-desktop.exe']);
    } catch (_) {}
    try {
      await CoreProcess.stopShared();
    } catch (_) {}
    exit(0);
  }
}

/// 收 Digest 的极小 Sink（startChunkedConversion 需要一个 `Sink<Digest>`，
/// 而 package:convert 的 AccumulatorSink 不在依赖里，不为一件事加一个包）
class _DigestSink implements Sink<Digest> {
  Digest? value;

  @override
  void add(Digest data) => value = data;

  @override
  void close() {}
}
