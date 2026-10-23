/// [20261022 剪贴板同步 v1] 文本 + 文件，**双向对称**。
///
/// ## 为什么要有这个文件
///
/// ToDesk / UU 那种「A 机复制、B 机直接 Ctrl+V」的体验，Windows 自己做不到：
/// 系统剪贴板存的是**本机内容**（文本是字符串，文件是 CF_HDROP 里的一串路径），
/// 跨机没有任何自动搬运。所以必须两端各自做三件事：
///
///   ① 盯着自己的剪贴板有没有变（`GetClipboardSequenceNumber` 轮询，700ms）
///   ② 变了就把内容通过数据通道送到对端
///   ③ 对端收到后写进**自己的**剪贴板 —— 用户随后 Ctrl+V 就拿得到
///
/// 这就是市面上所有远控软件的做法，没有更"透明"的路子。
///
/// ## 三条铁律（都是踩出来的）
///
/// 1. **必须防回环**。收到对端内容 → 写本机剪贴板 → 序列号变了 → 又被自己
///    的轮询读到 → 再发回对端 → 死循环刷爆通道。这里的做法是：**每次写剪贴板
///    之后立刻把写入后的序列号记成 `_lastSeq`**，下一轮看到同一个号直接跳过。
/// 2. **文件优先于文本**。在资源管理器里复制文件时，剪贴板里**同时**还有一段
///    文本（第一个文件名或路径）。只读文本会把文件名当成文字发过去，
///    对端收到一串没用的路径。所以先问文件，有文件就只发文件。
/// 3. **绝不静默丢内容**。超限（>50MB / >200 个 / 超长文本）一律发一条
///    `warn` 事件给 UI，让用户知道"没传，原因是这个"，而不是干等。
///
/// ## 文件分块协议（FileChannel）
///
/// 控制消息走 FileChannel 的**文本帧**（JSON，msgType + data）：
///   - `fileOffer` {xid, total, files:[{name,size}]}
///   - `fileDone`  {xid, bytes}
/// 数据走 FileChannel 的**二进制帧**，7 字节头：
///   `[0]=0x1D magic | [1..2]=fileIndex(BE) | [3..6]=offset(BE) | [7..]=payload`
/// 块大小 16KB（SCTP 上再大就容易分片，丢一块整帧重传，反而更慢）。
library;

import 'dart:async';
import 'dart:convert';
import 'dart:io';
import 'dart:typed_data';

import 'ipc.dart';
import 'shell_log.dart';

/// UI 事件（主控端画面角落的气泡、被控端日志都吃这个）
enum ClipEventKind { textSent, textRecv, fileStart, fileProgress, fileDone, fileRecv, warn }

class ClipEvent {
  const ClipEvent(this.kind, this.title, {this.detail = '', this.progress = 0});
  final ClipEventKind kind;
  final String title;
  final String detail;
  /// 0..1，仅 fileProgress 有意义
  final double progress;

  @override
  String toString() => '[$kind] $title ${detail.isEmpty ? '' : "($detail)"}';
}

/// 单个待发文件
class _OutFile {
  _OutFile({required this.name, required this.path, required this.size});
  final String name;
  final String path;
  final int size;
}

/// 接收中的单个文件
class _RecvFile {
  _RecvFile({required this.name, required this.path, required this.size, required this.raf});
  final String name;
  final String path;
  final int size;
  final RandomAccessFile raf;
  int got = 0;
}

class _RecvSession {
  _RecvSession({required this.xid, required this.dir, required this.files});
  final String xid;
  final String dir;
  final List<_RecvFile> files;
  int got = 0;
}

class ClipSync {
  ClipSync({
    required this.sendCtrl,
    required this.sendFileCtrl,
    required this.sendFileBin,
    this.onEvent,
    this.maxAutoBytes = 50 * 1024 * 1024,
    this.maxTextChars = 20000,
  });

  /// MessageChannel 文本帧（可靠、有序）—— 文本同步走它
  final bool Function(String json) sendCtrl;

  /// FileChannel 文本帧 —— 文件控制消息
  final bool Function(String json) sendFileCtrl;

  /// FileChannel 二进制帧 —— 文件数据
  final bool Function(Uint8List bin) sendFileBin;

  final void Function(ClipEvent e)? onEvent;

  /// 复制即传的大小上限（超过不发，只提示）。50MB 是"无感"和"别把画面拖死"的分界。
  final int maxAutoBytes;

  /// 单条文本同步上限（对端剪贴板里几 MB 的文本会把通道堵死）
  final int maxTextChars;

  static const int chunkSize = 16 * 1024;
  static const int _magic = 0x1D;
  static const int _headerLen = 7;

  /// 限速：文件流和视频共用一条 ICE 通道，不限速会让画面掉帧。
  static const int _bytesPerMs = 8000; // ≈ 8MB/s

  Timer? _timer;
  bool _busy = false;
  bool _disposed = false;
  int _lastSeq = -1;
  String _lastSeenText = '';
  String _lastSentFilesKey = '';
  bool _sending = false;
  _RecvSession? _recv;

  /// 会话建立后调用。轮询 700ms —— 与 web 版同频，够快且几乎不耗 CPU
  /// （`GetClipboardSequenceNumber` 不打开剪贴板，是几微秒的取值）。
  void start() {
    _timer?.cancel();
    _timer = Timer.periodic(const Duration(milliseconds: 700), (_) => _tick());
    ShellLog.w('[clip] 剪贴板同步已启动');
    _sweepRecvCache();
  }

  /// 收件缓存清理：真身已经被用户粘到自己目录里了，缓存只是中转。
  /// 留 3 天足够应付"粘完就关、第二天再来"的用法，超期删掉免得 Temp 越堆越大。
  Future<void> _sweepRecvCache() async {
    try {
      final root = Directory('${Directory.systemTemp.path}/InDesk/recv');
      if (!await root.exists()) return;
      final now = DateTime.now();
      var n = 0;
      await for (final e in root.list(followLinks: false)) {
        try {
          final st = await e.stat();
          if (now.difference(st.modified).inDays >= 3) {
            await e.delete(recursive: true);
            n++;
          }
        } catch (_) {}
      }
      if (n > 0) ShellLog.w('[clip] 已清理 $n 个过期收件缓存');
    } catch (_) {}
  }

  void stop() {
    _timer?.cancel();
    _timer = null;
    final r = _recv;
    _recv = null;
    if (r != null) {
      for (final f in r.files) {
        try {
          f.raf.close();
        } catch (_) {}
      }
    }
  }

  Future<void> dispose() async {
    _disposed = true;
    stop();
  }

  void _emit(ClipEvent e) {
    ShellLog.w('[clip] $e');
    try {
      onEvent?.call(e);
    } catch (_) {}
  }

  /// 进度节流：16KB/块在 8MB/s 下是每秒 500 条，直接灌给 UI 会卡死。
  /// 100ms 一条（10Hz）对进度条完全够用。
  int _lastProgressMs = 0;
  void _emitProgress(ClipEvent e) {
    final now = DateTime.now().millisecondsSinceEpoch;
    if (now - _lastProgressMs < 100) return;
    _lastProgressMs = now;
    _emit(e);
  }

  // ------------------------------------------------------------------ 轮询

  Future<void> _tick() async {
    if (_disposed || _busy || _sending) return;
    _busy = true;
    try {
      final seq = await _seq();
      if (seq <= 0 || seq == _lastSeq) return;
      _lastSeq = seq;

      // 铁律②：先问文件。复制文件时剪贴板里同时有"文本"（文件名），
      // 先读文本会把文件名当文字发过去。
      final files = await _clipFiles();
      if (files.isNotEmpty) {
        await _sendFiles(files);
        return;
      }

      final text = await _clipText();
      if (text == null || text.isEmpty) return;
      if (text == _lastSeenText) return;
      _lastSeenText = text;
      if (text.length > maxTextChars) {
        _emit(ClipEvent(ClipEventKind.warn, '文本过长，未同步',
            detail: '${text.length} 字符 > 上限 $maxTextChars'));
        return;
      }
      final ok = sendCtrl(jsonEncode(<String, dynamic>{
        'msgType': 'clipText',
        'requestId': 'clip',
        'data': <String, dynamic>{'text': text, 'seq': seq},
      }));
      if (ok) {
        _emit(ClipEvent(ClipEventKind.textSent, '已同步文本',
            detail: text.length > 24 ? '${text.substring(0, 24)}…' : text));
      }
    } catch (e) {
      ShellLog.w('[clip] 轮询异常: $e');
    } finally {
      _busy = false;
    }
  }

  Future<int> _seq() async {
    final r = await Ipc.instance.clipboardSeq();
    if (r is Map) return (r['seq'] as num?)?.toInt() ?? 0;
    return 0;
  }

  Future<List<String>> _clipFiles() async {
    final r = await Ipc.instance.clipboardGetFiles();
    if (r is Map) {
      final l = r['files'];
      if (l is List) return l.map((e) => e.toString()).toList();
    }
    return const <String>[];
  }

  Future<String?> _clipText() async {
    final r = await Ipc.instance.clipboardGet();
    if (r is Map) {
      final t = r['text'];
      if (t is String && t.isNotEmpty) return t;
    }
    return null;
  }

  // ------------------------------------------------------------- 发送文件

  Future<void> _sendFiles(List<String> paths) async {
    final key = paths.join('|');
    if (key == _lastSentFilesKey) return;

    final entries = <_OutFile>[];
    var skippedDir = 0;
    var total = 0;
    for (final p in paths) {
      try {
        final f = File(p);
        if (!await f.exists()) continue;
        final st = await f.stat();
        if (st.type != FileSystemEntityType.file) {
          skippedDir++; // 目录暂不支持：下面必须告知用户，不能静默吞掉
          continue;
        }
        entries.add(_OutFile(name: _baseName(p), path: p, size: st.size));
        total += st.size;
      } catch (_) {}
    }
    if (entries.isEmpty) {
      if (skippedDir > 0) {
        _emit(ClipEvent(ClipEventKind.warn, '文件夹暂不支持直传',
            detail: '$skippedDir 个文件夹已跳过，压缩成 zip 后再复制'));
      }
      return;
    }

    _lastSentFilesKey = key;
    if (total > maxAutoBytes) {
      _emit(ClipEvent(ClipEventKind.warn, '文件过大，未自动发送',
          detail: '${_human(total)} > ${_human(maxAutoBytes)}'));
      return;
    }

    final xid = DateTime.now().microsecondsSinceEpoch.toString();
    final offerOk = sendFileCtrl(jsonEncode(<String, dynamic>{
      'msgType': 'fileOffer',
      'data': <String, dynamic>{
        'xid': xid,
        'total': total,
        'files': [
          for (final e in entries) <String, dynamic>{'name': e.name, 'size': e.size}
        ],
      },
    }));
    if (!offerOk) {
      _emit(const ClipEvent(ClipEventKind.warn, '文件通道未就绪', detail: '对端可能未建立通道'));
      return;
    }

    _sending = true;
    _emit(ClipEvent(ClipEventKind.fileStart,
        '正在发送 ${entries.length} 个文件', detail: _human(total)));
    var sent = 0;
    final sw = Stopwatch()..start();
    try {
      for (var i = 0; i < entries.length; i++) {
        final e = entries[i];
        final raf = await File(e.path).open();
        try {
          var offset = 0;
          while (offset < e.size) {
            final buf = await raf.read(chunkSize);
            if (buf.isEmpty) break;
            if (!sendFileBin(_encodeChunk(i, offset, buf))) {
              throw const SocketException('file channel closed');
            }
            offset += buf.length;
            sent += buf.length;
            _emitProgress(ClipEvent(ClipEventKind.fileProgress, '发送中',
                detail: '${_human(sent)} / ${_human(total)}',
                progress: total == 0 ? 0 : sent / total));
            await _pace(sent, sw);
          }
        } finally {
          await raf.close();
        }
      }
      sendFileCtrl(jsonEncode(<String, dynamic>{
        'msgType': 'fileDone',
        'data': <String, dynamic>{'xid': xid, 'bytes': sent},
      }));
      _emit(ClipEvent(ClipEventKind.fileDone,
          '已发送 ${entries.length} 个文件',
          detail: skippedDir > 0
              ? '${_human(sent)} · 另有 $skippedDir 个文件夹已跳过'
              : '${_human(sent)} · 在对端按 Ctrl+V 粘贴',
          progress: 1));
    } catch (e) {
      ShellLog.w('[clip] 文件发送中断: $e');
      _emit(ClipEvent(ClipEventKind.warn, '文件发送中断', detail: '$e'));
    } finally {
      _sending = false;
    }
  }

  Future<void> _pace(int sent, Stopwatch sw) async {
    final expectMs = sent / _bytesPerMs;
    final diff = expectMs - sw.elapsedMilliseconds;
    if (diff > 2) {
      await Future<void>.delayed(Duration(milliseconds: diff.round()));
    }
  }

  Uint8List _encodeChunk(int index, int offset, List<int> data) {
    final out = Uint8List(_headerLen + data.length);
    out[0] = _magic;
    out[1] = (index >> 8) & 0xFF;
    out[2] = index & 0xFF;
    out[3] = (offset >> 24) & 0xFF;
    out[4] = (offset >> 16) & 0xFF;
    out[5] = (offset >> 8) & 0xFF;
    out[6] = offset & 0xFF;
    out.setRange(_headerLen, out.length, data);
    return out;
  }

  // ------------------------------------------------------------- 接收

  /// 对端发来的 MessageChannel 文本（msgType == clipText）
  Future<void> handleCtrlText(Map<String, dynamic> m) async {
    if (m['msgType'] != 'clipText') return;
    final d = m['data'];
    if (d is! Map) return;
    final text = d['text'];
    if (text is! String || text.isEmpty) return;
    if (text.length > maxTextChars) return;
    if (text == _lastSeenText) return;
    _lastSeenText = text;
    await Ipc.instance.clipboardSet(text);
    // 铁律①：写完后立刻把新序列号记住，下一轮轮询看到同一个号就跳过，
    // 否则「收到 → 写 → 读到 → 再发」会无限循环。
    _lastSeq = await _seq();
    _emit(ClipEvent(ClipEventKind.textRecv, '已收到文本',
        detail: text.length > 24 ? '${text.substring(0, 24)}…' : text));
  }

  /// 对端发来的 FileChannel 文本帧
  Future<void> handleFileText(Map<String, dynamic> m) async {
    final t = m['msgType'];
    final d = m['data'];
    final dm = d is Map ? Map<String, dynamic>.from(d) : <String, dynamic>{};
    if (t == 'fileOffer') {
      await _onOffer(dm);
    } else if (t == 'fileDone') {
      await _onDone(dm);
    }
  }

  Future<void> _onOffer(Map<String, dynamic> d) async {
    final xid = d['xid']?.toString() ?? '';
    if (xid.isEmpty) return;
    final list = d['files'];
    if (list is! List || list.isEmpty) return;
    final dir = Directory('${Directory.systemTemp.path}/InDesk/recv/$xid');
    try {
      await dir.create(recursive: true);
    } catch (e) {
      _emit(ClipEvent(ClipEventKind.warn, '无法创建接收目录', detail: '$e'));
      return;
    }
    final files = <_RecvFile>[];
    for (final x in list) {
      final mm = x is Map ? x : const {};
      final name = _safeName(mm['name']?.toString() ?? 'file');
      final size = (mm['size'] as num?)?.toInt() ?? 0;
      final path = '${dir.path}/$name';
      final raf = await File(path).open(mode: FileMode.write);
      files.add(_RecvFile(name: name, path: path, size: size, raf: raf));
    }
    _recv = _RecvSession(xid: xid, dir: dir.path, files: files);
    _emit(ClipEvent(ClipEventKind.fileStart,
        '正在接收 ${files.length} 个文件',
        detail: _human(files.fold<int>(0, (a, b) => a + b.size))));
  }

  /// 对端发来的 FileChannel 二进制帧
  void handleFileBinary(Uint8List b) {
    final r = _recv;
    if (r == null || b.length <= _headerLen || b[0] != _magic) return;
    final idx = (b[1] << 8) | b[2];
    if (idx >= r.files.length) return;
    final f = r.files[idx];
    final payload = Uint8List.sublistView(b, _headerLen);
    try {
      f.raf.writeFromSync(payload);
    } catch (e) {
      ShellLog.w('[clip] 写入失败 ${f.name}: $e');
      return;
    }
    f.got += payload.length;
    r.got += payload.length;
    final totalSize = r.files.fold<int>(0, (a, b) => a + b.size);
    _emitProgress(ClipEvent(ClipEventKind.fileProgress, '接收中',
        detail: '${_human(r.got)} / ${_human(totalSize)}',
        progress: totalSize == 0 ? 0 : r.got / totalSize));
  }

  Future<void> _onDone(Map<String, dynamic> d) async {
    final r = _recv;
    if (r == null) return;
    _recv = null;
    final paths = <String>[];
    for (final f in r.files) {
      try {
        await f.raf.close();
      } catch (_) {}
      // 0 字节文件也必须留住（got 永远是 0，但空文件本身是合法内容）
      if (f.got > 0 || f.size == 0) paths.add(f.path);
    }
    if (paths.isEmpty) return;
    try {
      await Ipc.instance.clipboardSetFiles(paths);
      // 铁律①：写剪贴板后同步序列号，防止把刚收到的文件又发回去。
      _lastSeq = await _seq();
      _lastSentFilesKey = paths.join('|');
    } catch (e) {
      _emit(ClipEvent(ClipEventKind.warn, '写入剪贴板失败', detail: '$e'));
      return;
    }
    _emit(ClipEvent(ClipEventKind.fileRecv,
        '收到 ${paths.length} 个文件',
        detail: '已放入剪贴板 · 按 Ctrl+V 粘贴', progress: 1));
  }

  // ------------------------------------------------------------- 小工具

  static String _baseName(String path) {
    final s = path.replaceAll('\\', '/');
    final i = s.lastIndexOf('/');
    return i >= 0 ? s.substring(i + 1) : s;
  }

  /// 落盘文件名清洗：**绝不能**让对端传过来的名字带路径穿越
  /// （`..\..\Windows\System32\xxx` 这种），只保留最后一段并去掉非法字符。
  static String _safeName(String raw) {
    var s = _baseName(raw);
    s = s.replaceAll(RegExp(r'[<>:"|?*\x00-\x1F]'), '_');
    if (s.isEmpty || s == '.' || s == '..') s = 'file';
    if (s.length > 120) s = s.substring(s.length - 120);
    return s;
  }

  static String _human(int bytes) {
    if (bytes < 1024) return '$bytes B';
    if (bytes < 1024 * 1024) return '${(bytes / 1024).toStringAsFixed(1)} KB';
    if (bytes < 1024 * 1024 * 1024) {
      return '${(bytes / 1024 / 1024).toStringAsFixed(1)} MB';
    }
    return '${(bytes / 1024 / 1024 / 1024).toStringAsFixed(2)} GB';
  }
}
