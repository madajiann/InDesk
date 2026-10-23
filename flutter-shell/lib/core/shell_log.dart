/// 文件日志 —— GUI 子系统 stdout 不进管道，E2E 取证全靠它。
/// 写到 exe 同目录 shell.log（与 electron 壳的 userData/shell.log 语义对齐）。
import 'dart:io';

import 'package:flutter/foundation.dart';

class ShellLog {
  static File? _file;
  static IOSink? _sink;

  static void init() {
    try {
      final exe = Platform.resolvedExecutable;
      final dir = File(exe).parent.path;
      _file = File('$dir\\shell.log');
      _sink = _file!.openWrite(mode: FileMode.append);
      _sink!.writeln('==== shell start ${DateTime.now()} ====');
    } catch (_) {}
  }

  static void w(String msg) {
    final line = '[${DateTime.now().toIso8601String()}] $msg';
    debugPrint(line);
    try {
      _sink?.writeln(line);
      _sink?.flush();
    } catch (_) {}
  }

  static void close() {
    try {
      _sink?.close();
    } catch (_) {}
    _sink = null;
  }
}
