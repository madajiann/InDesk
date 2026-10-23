/// IPC 通道强类型封装 —— 通道清单全量对齐 web/src/event.ts（45 通道）
///
/// 分工：
/// - 窗口类（2a）：Flutter 本地实现，由 UI 层注入 WindowHooks，**不发 core**
/// - 其余（2b/2c/2d）：透传 core（被控端逻辑全在 Rust 里）
/// - 更新类（2e）：B 线自实现，见 update_manager.dart
import 'ws_bridge.dart';

/// 窗口钩子：Flutter 壳本地实现的窗口通道（core 无窗口，与 Electron 版同分工）
abstract class WindowHooks {
  void minimize();
  void maximize();
  void toggleMaximize();
  void close({int? windowId});
  void closeAll();
  void setBounds(int windowId, {int? x, int? y, int? width, int? height});
  void setPosition(int windowId, {required int x, required int y});
  double titlebarHeight();
  void moveScreenRightBottom();
  void openDevTools(); // debug 构建下生效
  void createWindow(Map<String, dynamic> spec); // 对齐 __core_createWindow
}

class Ipc {
  Ipc._();
  static final Ipc instance = Ipc._();
  static WindowHooks? hooks;

  WsBridge get _ws => WsBridge.instance;

  Future<dynamic> _invoke(String channel, {dynamic payload, int? windowId}) =>
      _ws.invoke(channel, payload: payload, windowId: windowId)
          .then((r) => r.ok ? r.data : null);

  /// [20261022] core 的 `ipc::dispatch` 只认 `payload.data`（协议见
  /// `receiver/host_core.dart` 注释：`{requestId, data:{...}}`）。上面的
  /// `_invoke` 把参数放在 payload 顶层，core 读到的永远是空对象 —— 之前
  /// 文本剪贴板写入(`clipboardSet`)一直是坏的，只是没有任何地方调用它，
  /// 所以没人发现。带参数的写入类命令必须走这条。
  Future<dynamic> _invokeData(String channel, Map<String, dynamic> data) =>
      _ws.invoke(channel, payload: <String, dynamic>{
        'requestId': 'f${DateTime.now().microsecondsSinceEpoch}',
        'data': data,
      }).then((r) => r.ok ? r.data : null);

  void _send(String channel, {dynamic payload, int? windowId}) =>
      _ws.send(channel, payload: payload, windowId: windowId);

  // ---- 2b 输入控制（透传 core）----
  Future<void> mouseMove(int windowId, int x, int y) =>
      _invoke('mouseMove', payload: {'x': x, 'y': y}, windowId: windowId);
  Future<void> mouseDrag(int windowId, int x, int y) =>
      _invoke('mouseDrag', payload: {'x': x, 'y': y}, windowId: windowId);
  Future<void> mouseSetPosition(int windowId, int x, int y) =>
      _invoke('mouseSetPosition', payload: {'x': x, 'y': y}, windowId: windowId);
  Future<void> mouseLeftClick(int windowId) =>
      _invoke('mouseLeftClick', windowId: windowId);
  Future<void> mouseRightClick(int windowId) =>
      _invoke('mouseRightClick', windowId: windowId);
  Future<void> mouseDoubleClick(int windowId) =>
      _invoke('mouseDoubleClick', windowId: windowId);
  Future<void> mousePressButtonLeft(int windowId) =>
      _invoke('mousePressButtonLeft', windowId: windowId);
  Future<void> mouseReleaseButtonLeft(int windowId) =>
      _invoke('mouseReleaseButtonLeft', windowId: windowId);
  Future<void> mouseScroll(int windowId, String dir, int amount) => _invoke(
      'mouseScroll$dir', payload: {'amount': amount}, windowId: windowId);
  Future<void> keyboardPressKey(int windowId, dynamic key) =>
      _invoke('keyboardPressKey', payload: key, windowId: windowId);
  Future<void> keyboardReleaseKey(int windowId, dynamic key) =>
      _invoke('keyboardReleaseKey', payload: key, windowId: windowId);
  Future<void> keyboardType(int windowId, dynamic payload) =>
      _invoke('keyboardType', payload: payload, windowId: windowId);
  Future<void> switchIme(int windowId, int mode) =>
      _invoke('switchIme', payload: {'mode': mode}, windowId: windowId);
  Future<void> releaseAllModifiers(int windowId) =>
      _invoke('releaseAllModifiers', windowId: windowId);

  // ---- 2c 查询/系统 ----
  Future<dynamic> getMousePosition(int windowId) =>
      _invoke('getMousePosition', windowId: windowId);
  Future<dynamic> scaleFactor() => _invoke('scaleFactor');
  Future<dynamic> workAreaSize() => _invoke('workAreaSize');
  Future<dynamic> getPrimaryDisplaySize() => _invoke('getPrimaryDisplaySize');
  Future<String> getPlatform() async =>
      (await _invoke('getPlatform'))?.toString() ?? 'unknown';
  Future<String> getShellVersion() async =>
      (await _invoke('getShellVersion'))?.toString() ?? '';
  Future<void> shellOpenExternal(String url) =>
      _invoke('shellOpenExternal', payload: {'url': url});
  Future<void> powerSaveBlockerStart() =>
      _invoke('powerSaveBlockerStart');
  Future<dynamic> clipboardSeq() => _invoke('clipboardSeq');
  Future<dynamic> clipboardGet() => _invoke('clipboardGet');
  Future<void> clipboardSet(String text) =>
      _invokeData('clipboardSet', <String, dynamic>{'text': text});
  // [20261022 剪贴板文件] CF_HDROP 读写：Dart 的 Clipboard API 只有文本，
  // 文件列表必须由 core 用 Win32 原语处理（见 clipboard.rs）。
  Future<dynamic> clipboardGetFiles() => _invoke('clipboardGetFiles');
  Future<void> clipboardSetFiles(List<String> files) =>
      _invokeData('clipboardSetFiles', <String, dynamic>{'files': files});

  // ---- 2d 被控会话 ----
  Future<void> privacyScreen(int windowId, bool enable) => _invoke(
      'privacyScreen', payload: {'enable': enable}, windowId: windowId);
  Future<void> sendCtrlAltDel(int windowId) =>
      _invoke('sendCtrlAltDel', windowId: windowId);
  Future<void> openTaskManager(int windowId) =>
      _invoke('openTaskManager', windowId: windowId);
  Future<void> lockWorkstation() => _invoke('lock_workstation');
  Future<void> setUnattended(bool enable) =>
      _invoke('setUnattended', payload: {'enable': enable});
  Future<void> nudgeUserGesture(int windowId) =>
      _invoke('nudgeUserGesture', windowId: windowId);
  Future<void> captureStart() => _invoke('captureStart');
  Future<void> captureStop() => _invoke('captureStop');

  // ---- 2a 窗口类：本地实现 ----
  void windowMinimize() => hooks?.minimize();
  void windowMaximize() => hooks?.maximize();
  void windowToggleMaximize() => hooks?.toggleMaximize();
  void closeWindow({int? windowId}) => hooks?.close(windowId: windowId);
  void closeAllWindow() => hooks?.closeAll();
  void setWindowBounds(int windowId,
          {int? x, int? y, int? width, int? height}) =>
      hooks?.setBounds(windowId, x: x, y: y, width: width, height: height);
  void setWindowPosition(int windowId, {required int x, required int y}) =>
      hooks?.setPosition(windowId, x: x, y: y);
  double getWindowTitlebarHeight() => hooks?.titlebarHeight() ?? 32;
  void handleMoveScreenRightBottom() => hooks?.moveScreenRightBottom();
  void handleOpenDevTools() => hooks?.openDevTools();

  /// 广播事件分发入口：main.dart 里把 core 事件接到这里，
  /// 窗口类事件走本地 hooks，其余抛给 UI 层订阅。
  void handleCoreEvent(WsEvent e) {
    switch (e.event) {
      case '__core_createWindow':
        hooks?.createWindow((e.payload ?? {}) as Map<String, dynamic>);
      default:
        break; // UI 层通过 WsBridge.instance.events 自行订阅
    }
  }
}
