/// [20261017c] Windows 屏幕逻辑尺寸探测（dart:ffi，零依赖）。
/// 用途：主窗口初始尺寸自适应 —— 用户反馈「初始窗口直接全屏、想调小都
/// 不行」：老逻辑固定 1000x680（最小 800x560），在 1366x768 / 150% 缩放
/// 的机器上逻辑可用空间只有 ~911x512，窗口直接盖满甚至超出屏幕。
import 'dart:ffi';
import 'dart:math' as math;

typedef _GetSystemMetricsC = Int32 Function(Int32 nIndex);
typedef _GetSystemMetricsD = int Function(int nIndex);
typedef _GetDpiForSystemC = Uint32 Function();
typedef _GetDpiForSystemD = int Function();

({double width, double height}) screenLogicalSize() {
  try {
    final user32 = DynamicLibrary.open('user32.dll');
    final metrics =
        user32.lookupFunction<_GetSystemMetricsC, _GetSystemMetricsD>(
            'GetSystemMetrics');
    // 0 = SM_CXSCREEN, 1 = SM_CYSCREEN（主屏物理像素）
    final pw = metrics(0).toDouble();
    final ph = metrics(1).toDouble();
    var dpi = 96.0;
    try {
      final getDpi =
          user32.lookupFunction<_GetDpiForSystemC, _GetDpiForSystemD>(
              'GetDpiForSystem');
      final d = getDpi();
      if (d > 0) dpi = d.toDouble();
    } catch (_) {}
    final scale = dpi / 96.0;
    if (pw <= 0 || ph <= 0) return (width: 1280, height: 800);
    return (width: pw / scale, height: ph / scale);
  } catch (_) {
    return (width: 1280, height: 800);
  }
}

/// 主窗口初始/最小尺寸自适应屏幕（逻辑像素）：
/// 首选 1000x680，屏幕太小则收窄，保证四周至少留 40px 视觉余量。
({double iw, double ih, double mw, double mh}) adaptiveWindowSizes() {
  final s = screenLogicalSize();
  final w = math.max(560.0, math.min(1000.0, s.width - 40));
  final h = math.max(440.0, math.min(680.0, s.height - 40));
  return (
    iw: w,
    ih: h,
    mw: math.min(720.0, w),
    mh: math.min(500.0, h),
  );
}
