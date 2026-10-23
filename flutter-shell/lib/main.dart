/// InDesk Flutter 壳入口 —— 语义对齐 electron-shell/main.cjs
///
/// 启动序列：
/// 1. 单实例互斥（复用 core 的互斥，主进程先探测默认端口）
/// 2. 拉起/复用 indesk-core.exe（无头，ws 桥 + web + 取屏全在里面）
/// 3. ws 连上 core 桥
/// 4. 进主界面（M1：连接列表/设备码/设置）
import 'dart:ffi' as ffi;
import 'dart:io';

import 'package:flutter/material.dart';
import 'package:system_tray/system_tray.dart';
import 'package:window_manager/window_manager.dart';

import 'core/core_process.dart';
import 'core/ipc.dart';
import 'core/screen.dart';
import 'core/screen_probe.dart';
import 'core/shell_log.dart';
import 'core/ws_bridge.dart';
import 'ui/shell_layout.dart';

/// 命令行：--connect <控制码> <验证码>  → 跳过主界面自动发起远控（E2E 用）
List<String> parseConnectArgs(List<String> args) {
  final i = args.indexOf('--connect');
  if (i >= 0 && args.length >= i + 3) {
    return args.sublist(i + 1, i + 3);
  }
  return const [];
}

/// 全局 core 守护引用：壳布局关窗时由此统一收尾（杀 core，防残留）
/// 系统托盘（对齐 ToDesk）：点 X = 隐藏窗口常驻托盘，托盘菜单可打开/退出。
final SystemTray _tray = SystemTray();
const _trayTitle = 'InDesk';

Future<void> _initSystemTray() async {
  try {
    await _tray.initSystemTray(
      title: _trayTitle,
      iconPath: 'assets/brand/indesk.ico',
      toolTip: '$_trayTitle - 便捷式免费远程连接软件',
    );
    final menu = Menu()
      ..buildFrom([
        MenuItemLabel(label: '打开主界面', onClicked: _onTrayOpen),
        MenuSeparator(),
        MenuItemLabel(label: '退出', onClicked: _onTrayQuit),
      ]);
    await _tray.setContextMenu(menu);
    // 左键单击 = 打开主界面；右键 = 弹菜单（对齐 ToDesk 习惯）
    _tray.registerSystemTrayEventHandler((eventName) {
      if (eventName == kSystemTrayEventClick) {
        _onTrayOpen();
      } else if (eventName == kSystemTrayEventRightClick) {
        _tray.popUpContextMenu();
      }
    });
    ShellLog.w('[tray] 系统托盘已就绪');
  } catch (e) {
    ShellLog.w('[tray] 托盘初始化失败（不影响主功能）: $e');
  }
}

void _onTrayOpen([MenuItemBase? _]) {
  windowManager.show();
  windowManager.focus();
}

/// 托盘退出：真正收尾（杀 core + 被控 → 销毁窗口 → 删托盘）。
/// [20261017] 被控托盘已移除（全机只留这一个图标），退出时把
/// indesk-desktop.exe 一并结束，不留无托盘的后台孤儿进程。
Future<void> _onTrayQuit([MenuItemBase? _]) async {
  ShellLog.w('[tray] 托盘退出：停 core + 被控 → 销毁窗口');
  try {
    await Process.run('taskkill', ['/f', '/im', 'indesk-desktop.exe']);
  } catch (_) {}
  await CoreProcess.stopShared();
  await _tray.destroy();
  await windowManager.destroy();
  exit(0);
}

/// 单实例锁的句柄：**必须是全局的**，进程退出前不释放（进程终止时由 OS 释放）。
/// 这里刻意不读取它 —— 持有引用本身就是目的（防止 GC 提前关闭句柄）。
// ignore: unused_field
RandomAccessFile? _instanceLock;

/// 抢单实例锁。成功 true；已被别的实例占用 false。
///
/// 用文件独占锁而不是窗口类名/端口探测：端口那套在「核还活着但壳死了」的
/// 场景下会把新壳误判成重复实例；文件锁只跟**壳进程**的存活严格绑定，
/// 进程一死锁就自动释放，语义正好。
bool _acquireSingleInstance() {
  try {
    final base = Platform.environment['LOCALAPPDATA'] ??
        Directory.systemTemp.path;
    final f = File('$base\\InDesk\\shell.lock');
    f.parent.createSync(recursive: true);
    final raf = f.openSync(mode: FileMode.append);
    raf.lockSync(FileLock.exclusive);
    _instanceLock = raf;
    return true;
  } catch (_) {
    return false;
  }
}

/// [20261021 十轮] 壳进程优先级 → ABOVE_NORMAL。
///
/// 九轮之后**抓屏 + 编码整体搬进了壳**（libwebrtc 视频轨是在 Dart 侧建的，
/// getDisplayMedia + addTrack），而计划任务拉起的 GUI 进程默认是 Normal。
/// 这台 4 核 i5-4590 上 ToDesk / billd-desk / 浏览器一起抢，编码线程被饿到
/// 掉帧，用户侧看到的就是"画面动一下停一下"。
///
/// Rust 核（indesk-desktop.exe）里那段 SetPriorityClass 只作用于核自己，
/// 管不到壳 —— 实测壳/核都还是 Normal，所以这里用 FFI 给壳自身设一次。
/// 用 FFI 而不是 `wmic`/`powershell` 子进程：不闪窗、不额外起进程。
void _raisePriority() {
  if (!Platform.isWindows) return;
  try {
    final k32 = ffi.DynamicLibrary.open('kernel32.dll');
    final getCurrentProcess =
        k32.lookupFunction<ffi.IntPtr Function(), int Function()>(
      'GetCurrentProcess',
    );
    final setPriorityClass =
        k32.lookupFunction<ffi.Int32 Function(ffi.IntPtr, ffi.Uint32),
            int Function(int, int)>('SetPriorityClass');
    const aboveNormal = 0x00008000; // ABOVE_NORMAL_PRIORITY_CLASS
    final ok = setPriorityClass(getCurrentProcess(), aboveNormal);
    ShellLog.w('[main] 壳优先级 → ABOVE_NORMAL：${ok != 0 ? "成功" : "失败"}');
  } catch (e) {
    ShellLog.w('[main] 壳优先级设置异常（不影响功能）: $e');
  }
}

Future<void> main(List<String> args) async {
  WidgetsFlutterBinding.ensureInitialized();
  ShellLog.init();
  _raisePriority();
  ShellLog.w('[main] 启动, args=$args');

  // [20261021 九轮] 抓屏能力探针：INDESK_PROBE_SCREEN=1（只采）/2（VP8 环回压测）
  // /3（H264 优先环回压测），结果写进 shell.log 后退出。
  // 放在最前面 = 不拉核、不建窗口、不连信令（不污染在线身份、不弹任何窗）。
  final probeMode = Platform.environment['INDESK_PROBE_SCREEN'] ?? '';
  if (probeMode == '1' || probeMode == '2' || probeMode == '3') {
    await ScreenProbe.run();
    exit(0);
  }

  // [20261021 十轮] 单实例互斥（必须在**创建任何窗口之前**）。
  // 计划任务现在每分钟执行一次 `indesk.exe --auto` 来兜底守护（不再用
  // powershell 脚本 —— 那玩意儿每次都闪一个控制台窗口，用户已经投诉）。
  // 由于计划任务会无脑启动 exe，第二个实例必须在这里静默退出，
  // 否则每分钟都会叠一个窗口出来。
  if (!_acquireSingleInstance()) {
    ShellLog.w('[main] 已有实例在运行，本进程退出（单实例互斥）');
    exit(0);
  }

  await windowManager.ensureInitialized();

  // [20261017c] 窗口尺寸自适应屏幕：固定 1000x680（最小 800x560）在
  // 1366x768 / 高 DPI 缩放机器上会直接盖满屏幕（用户反馈「初始窗口
  // 直接全屏、想调小都调不了」）。按屏幕逻辑尺寸收窄，保证不超屏。
  final sizes = adaptiveWindowSizes();
  final windowOptions = WindowOptions(
    title: 'InDesk-便捷式免费远程连接软件',
    size: Size(sizes.iw, sizes.ih),
    minimumSize: Size(sizes.mw, sizes.mh),
    center: true,
    // [20261016] 透明背景会让 Flutter Windows 走分层合成，整机拖拽/滚动明显掉帧
    //（用户反馈"打开以后卡顿"）—— 改为实色浅底，与顶栏品牌色衔接。
    backgroundColor: Color(0xFFF3F7FC),
    skipTaskbar: false,
    titleBarStyle: TitleBarStyle.hidden, // 自绘顶栏（对齐 web 壳 system-bar）
  );
  await windowManager.waitUntilReadyToShow(windowOptions, () async {
    // 关窗 = 隐藏到托盘（shell_layout._closeApp → hide，见其注释）。
    // 真正退出走托盘菜单「退出」（_onTrayQuit）。
    await windowManager.show();
    await windowManager.focus();
  });

  // 1-2. core 守护（[20261021] 失败必须落盘 + 重试 3 次。以前单次失败只走
  // debugPrint 不落盘 → 壳静默 exit(1)，"shell start 后一行日志都没有"的死因
  // 全靠猜。14:01 实测 keepalive 重拉一次就成功——首启失败重试即活。）
  int? port;
  for (var attempt = 1; attempt <= 3 && port == null; attempt++) {
    try {
      port = await CoreProcess.shared.start();
    } catch (e) {
      ShellLog.w('[main] core 启动失败（第 $attempt/3 次）: $e');
      if (attempt < 3) await Future.delayed(const Duration(seconds: 3));
    }
  }
  if (port == null) {
    ShellLog.w('[main] core 三次启动失败，壳退出（失败原因见上方日志）');
    exit(1);
  }

  // 3. ws 桥
  await WsBridge.instance.connect(port);

  // 3b. 桥自检：getShellVersion 真实往返（走 core Rust dispatch 全链路）
  final ver = await Ipc.instance.getShellVersion();
  ShellLog.w('[M1] bridge E2E OK, shellVersion=$ver');

  // 3c. 托盘常驻（点 X = 隐藏，退出走托盘菜单）
  await _initSystemTray();

  // 4. UI（--connect 参数 → 直接进自动连接；常规启动进壳布局）
  final connectArgs = parseConnectArgs(args);
  runApp(InDeskApp(autoConnect: connectArgs));
}

class InDeskApp extends StatelessWidget {
  const InDeskApp({super.key, this.autoConnect = const []});

  final List<String> autoConnect;

  @override
  Widget build(BuildContext context) {
    return MaterialApp(
      title: 'InDesk',
      debugShowCheckedModeBanner: false,
      theme: ThemeData(
        colorScheme: ColorScheme.fromSeed(seedColor: const Color(0xFF2F6FED)),
        useMaterial3: true,
        fontFamily: 'Microsoft YaHei UI',
      ),
      home: ShellLayout(autoConnect: autoConnect),
    );
  }
}
