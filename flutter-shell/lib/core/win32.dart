/// Win32 交互小助手（dart:ffi，仅 Windows）：
/// minimizeNative() —— 发送 WM_SYSCOMMAND/SC_MINIMIZE，走系统 DWM
/// 最小化动画（比 windowManager.minimize 的 ShowWindow 路径顺滑）。
/// 窗口边框/客户区由 runner 层 WM_NCCALCSIZE 原生接管（见 flutter_window.cpp），
/// Dart 层不再需要边框补偿计算。
import 'dart:ffi';

final DynamicLibrary _user32 = DynamicLibrary.open('user32.dll');

final int Function(int, int, int, int) _sendMessageW =
    _user32
        .lookupFunction<IntPtr Function(IntPtr, Uint32, IntPtr, IntPtr),
            int Function(int, int, int, int)>('SendMessageW');

final int Function() _getForegroundWindow =
    _user32
        .lookupFunction<IntPtr Function(), int Function()>(
            'GetForegroundWindow');

/// 系统最小化（带 DWM 动画）
void minimizeNative() =>
    _sendMessageW(_getForegroundWindow(), 0x0112 /*WM_SYSCOMMAND*/, 0xF020 /*SC_MINIMIZE*/, 0);
