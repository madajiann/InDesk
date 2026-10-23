/// [20261018] Dart -> Windows runner 的窗口级通道辅助（indesk/window）。
/// 目前：setDragBand —— 按页面切换顶栏原生拖动区（HTCAPTION）两侧预留宽度。
/// 主界面页 300/400（右侧一排顶栏按钮），远控会话页 260/180（工具栏
/// 左右都有按钮，中间空白区加宽，修复「连接上以后窗口拖不动」）。
import 'package:flutter/services.dart';
import 'package:window_manager/window_manager.dart';

import 'prefs.dart';
import 'screen.dart';
import 'shell_log.dart';

const MethodChannel _winChannel = MethodChannel('indesk/window');

Future<void> setDragBand(int leftKeep, int rightKeep) async {
  try {
    await _winChannel
        .invokeMethod<bool>('setDragBand', {'left': leftKeep, 'right': rightKeep});
  } catch (e) {
    ShellLog.w('[win] setDragBand 失败: $e');
  }
}

/// [20261018] 连接发起时立即把窗口撑到「远控会话大小」。
/// [20261019k] 零过渡：若记住了上次会话的视频尺寸（ShellPrefs），直接按
/// 该宽高比贴合（视口 = 视频比例 + 42px 工具栏），连接瞬间就是最终状态；
/// 没有记录才退回屏幕适配上限。RemotePage 首帧到位后自校验微调（通常零差异）。
Future<void> ensureSessionWindowSize() async {
  try {
    if (await windowManager.isMaximized()) {
      await windowManager.unmaximize();
    }
    final scr = screenLogicalSize();
    final maxW = (scr.width - 56).clamp(560.0, scr.width - 56);
    final maxH = (scr.height - 72).clamp(400.0, scr.height - 72);
    // chrome 高度用上一次会话的实测值（标题栏+工具栏，随系统 DPI 变化），
    // 没有实测记录才退回 42+32 的估算值。
    final chromeH =
        ShellPrefs.lastChromeH > 20 ? ShellPrefs.lastChromeH : 74.0;
    double w = maxW;
    double h = maxH;
    final vw = ShellPrefs.lastVideoW;
    final vh = ShellPrefs.lastVideoH;
    if (vw > 100 && vh > 100) {
      final aspect = vw / vh;
      final innerH = maxH - chromeH;
      double innerW = innerH * aspect;
      if (innerW > maxW) innerW = maxW;
      if (innerW >= 200 && innerH >= 200) {
        w = innerW;
        h = innerH + chromeH;
      }
    }
    await windowManager.setSize(Size(w, h));
    await windowManager.center();
    ShellLog.w('[win] 会话窗口已就位: ${w.round()}x${h.round()}'
        '${(vw > 0 ? ' (按上次视频 ${vw}x$vh)' : '')}');
  } catch (e) {
    ShellLog.w('[win] 会话窗口就位失败: $e');
  }
}

/// [20261018] 回到主界面默认尺寸（连接取消/失败时调用）。
Future<void> restoreMainWindowSize() async {
  try {
    final def = adaptiveWindowSizes();
    if (await windowManager.isMaximized()) {
      await windowManager.unmaximize();
    }
    await windowManager.setSize(Size(def.iw, def.ih));
    await windowManager.center();
  } catch (_) {}
}

/// [20261019j] 拿到真实视频尺寸后把会话窗口等比贴合视频宽高比。
/// [20261019m] 推翻 chrome 建模法（chrome=外框-视口 再建模回去）：建模对
/// 测量时机敏感，量错一次就留下永久黑边（用户三轮投诉"两边黑框"）。
/// 改为**实测视口直接修正**：视口比视频"宽"就收窗口宽、比视频"高"就收
/// 窗口高，一步到位不猜 chrome 构成；每 2s 轮询自愈，最多两次收敛。
/// 同时记录实测 chromeH 供下次连接窗口出生使用（零过渡）。最大化不调整。
Future<void> alignSessionWindowToVideo(int vw, int vh,
    {Size? viewport}) async {
  try {
    if (vw <= 0 || vh <= 0) return;
    if (await windowManager.isMaximized()) return;
    final scr = screenLogicalSize();
    final cur = await windowManager.getSize();
    double w = cur.width, h = cur.height;
    if (viewport != null &&
        viewport.width > 100 &&
        viewport.height > 100) {
      final videoAspect = vw / vh;
      final vpAspect = viewport.width / viewport.height;
      if (vpAspect > videoAspect) {
        // 视口偏宽 → 左右黑边：收窗口宽
        final excess = viewport.width - viewport.height * videoAspect;
        w = cur.width - excess;
      } else if (vpAspect < videoAspect) {
        // 视口偏高 → 上下黑边：收窗口高
        final excess = viewport.height - viewport.width / videoAspect;
        h = cur.height - excess;
      }
      // 记录实测 chrome 高度（外框-视口），下次连接窗口按真实值出生
      final measuredChromeH = cur.height - viewport.height;
      if (measuredChromeH > 20 && measuredChromeH < 200) {
        ShellPrefs.lastChromeH = measuredChromeH;
      }
    } else {
      // 没有视口实测：按记住的 chrome 高度建模退一步（兜底）
      final chromeH = ShellPrefs.lastChromeH > 20 ? ShellPrefs.lastChromeH : 74.0;
      final maxW = (scr.width - 56).clamp(560.0, double.infinity);
      final maxH = (scr.height - 72).clamp(400.0, double.infinity);
      final aspect = vw / vh;
      double innerH = maxH - chromeH;
      double innerW = innerH * aspect;
      if (innerW > maxW) {
        innerW = maxW;
        innerH = innerW / aspect;
      }
      if (innerW < 200 || innerH < 200) return;
      w = innerW;
      h = innerH + chromeH;
    }
    // 夹到屏幕内，下限保护
    w = w.clamp(480.0, scr.width - 16);
    h = h.clamp(360.0, scr.height - 16);
    // [20261019k] 记住本次视频尺寸：下次连接窗口直接按它出生（零过渡）。
    if (ShellPrefs.lastVideoW != vw || ShellPrefs.lastVideoH != vh) {
      ShellPrefs.lastVideoW = vw;
      ShellPrefs.lastVideoH = vh;
    }
    // 2px 绝对容差：±2px 肉眼不可见；避免浮点残差反复 setSize 抖动。
    if ((cur.width - w).abs() < 2 && (cur.height - h).abs() < 2) {
      return;
    }
    await windowManager.setSize(Size(w, h));
    await windowManager.center();
    ShellLog.w(
        '[win] 会话窗口贴合: ${w.round()}x${h.round()} '
        '(视口 ${viewport?.width.round()}x${viewport?.height.round()}, '
        '视频 ${vw}x$vh, 原 ${cur.width.round()}x${cur.height.round()})');
  } catch (e) {
    ShellLog.w('[win] 贴合视频比例失败: $e');
  }
}
