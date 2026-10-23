/// core 子进程守护 —— 语义对齐 electron-shell/main.cjs 的 startCore()
///
/// 职责：
/// 1. 以 `--core --core-port <port>` 拉起 resources/indesk-core.exe
/// 2. 轮询 http://127.0.0.1:<port>/index.html 直到 200（core 桥就绪）
/// 3. core 意外退出时自动重启（带退避）
/// 4. 应用退出时连带杀掉 core
import 'dart:async';
import 'dart:io';

class CoreProcess {
  CoreProcess({this.exeName = 'indesk-core.exe', this.startPort = 23654});

  /// 全局单例：main() 启动的 core 守护；关窗收尾用它停止。
  static final CoreProcess shared = CoreProcess();

  /// 关窗收尾：杀掉共享 core 实例（由壳布局 onWindowClose 调用）。
  static Future<void> stopShared() => shared.stop();

  final String exeName;
  final int startPort;

  Process? _proc;
  int? _port;
  String? _exePath; // [20261021] 核绝对路径（锚定壳 exe 目录，免疫 cwd）
  bool _stopping = false;
  int _restartCount = 0;
  final _portController = StreamController<int>.broadcast();

  /// core 桥就绪的端口（ws 客户端用）
  Stream<int> get onPortReady => _portController.stream;
  int? get port => _port;
  bool get isRunning => _proc != null;

  /// 拉起 core 并等待 HTTP 就绪；成功返回实际端口。
  Future<int> start() async {
    _stopping = false;
    // [20261021] ⭐ 绝对路径：以前用 File(exeName) 相对路径找核，开机自启
    // （HKCU Run）时 cwd=System32，核必然找不到 → 壳静默 exit(1)，表现为
    // "shell start 后一行日志都没有"。改锚定壳 exe 所在目录，免疫 cwd。
    final exeDir = File(Platform.resolvedExecutable).parent.path;
    final exe = File('$exeDir\\$exeName');
    if (!exe.existsSync()) {
      throw StateError('core 不存在: $exeDir\\$exeName (cwd=${Directory.current.path})');
    }
    _exePath = exe.path;
    // 若已有 core 在默认端口活着（如用户手动起过），直接复用
    final reused = await _probePort(startPort);
    if (reused) {
      _port = startPort;
      _portController.add(startPort);
      return startPort;
    }
    return _spawn(startPort);
  }

  Future<int> _spawn(int port) async {
    // 普通托管模式：exitCode/kill 可用；GUI 子系统无控制台窗口，不会闪黑框
    final proc = await Process.start(
      _exePath ?? exeName,
      ['--core', '--core-port', '$port'],
    );
    _proc = proc;
    proc.stdout.listen((_) {});
    proc.stderr.listen((_) {});
    proc.exitCode.then((code) {
      _proc = null;
      if (_stopping) return;
      // 意外退出 → 退避重启
      _restartCount++;
      final delay = Duration(seconds: _restartCount > 5 ? 30 : 2);
      Future.delayed(delay, () async {
        if (_stopping) return;
        try {
          await _spawn(port);
        } catch (_) {}
      });
    });
    // 就绪轮询：最多 15s
    for (var i = 0; i < 30; i++) {
      await Future.delayed(const Duration(milliseconds: 500));
      if (await _probePort(port)) {
        _port = port;
        _restartCount = 0;
        _portController.add(port);
        return port;
      }
    }
    throw StateError('core 端口 $port 15s 内未就绪');
  }

  Future<bool> _probePort(int port) async {
    try {
      final req = await HttpClient()
          .getUrl(Uri.parse('http://127.0.0.1:$port/index.html'))
          .timeout(const Duration(seconds: 2));
      final resp = await req.close().timeout(const Duration(seconds: 2));
      await resp.drain();
      return resp.statusCode == 200;
    } catch (_) {
      return false;
    }
  }

  Future<void> stop() async {
    _stopping = true;
    _proc?.kill();
    _proc = null;
  }
}
