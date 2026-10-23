/// 连接记录本地存储 —— %APPDATA%/InDesk/recent.json
/// 结构：[{code, at, pwd?}]，最新在前，最多 20 条，去重。
/// [20261017c] pwd：记住的连接密码（记住密码勾选时随连接成功写入）。
import 'dart:convert';
import 'dart:io';

class RecentStore {
  static const int _max = 20;

  static File _file() {
    final base = Platform.environment['APPDATA'] ?? Directory.systemTemp.path;
    final dir = Directory('$base\\InDesk');
    if (!dir.existsSync()) dir.createSync(recursive: true);
    return File('${dir.path}\\recent.json');
  }

  static List<Map<String, dynamic>> loadSync() {
    try {
      final f = _file();
      if (!f.existsSync()) return [];
      final raw = jsonDecode(f.readAsStringSync());
      if (raw is! List) return [];
      return raw.whereType<Map>().map((e) => Map<String, dynamic>.from(e)).toList();
    } catch (_) {
      return [];
    }
  }

  /// 保存时间戳与去重；[password] 非空时记住密码，传空串则清除已存密码。
  static void addSync(String code, {String? password}) {
    if (code.trim().isEmpty) return;
    final list = loadSync();
    list.removeWhere((e) => e['code'] == code);
    final item = <String, dynamic>{
      'code': code,
      'at': DateTime.now().millisecondsSinceEpoch,
    };
    // [20261019] 不带 password 参数：保留旧记录里的已存密码与已知 IP 信息
    final oldList = loadSync().where((e) => e['code'] == code).toList();
    if (oldList.isNotEmpty && oldList.first['pwd'] is String) {
      item['pwd'] = oldList.first['pwd'];
    }
    if (password != null) {
      if (password.isNotEmpty) {
        item['pwd'] = password;
      }
    }
    if (oldList.isNotEmpty) {
      if (oldList.first['ip'] is String) item['ip'] = oldList.first['ip'];
      if (oldList.first['ipType'] is String) {
        item['ipType'] = oldList.first['ipType'];
      }
      // [20261019e] 探测回填的设备名/归属地也一并保留
      if (oldList.first['name'] is String) item['name'] = oldList.first['name'];
      if (oldList.first['geo'] is String) item['geo'] = oldList.first['geo'];
    }
    list.insert(0, item);
    while (list.length > _max) {
      list.removeLast();
    }
    _write(list);
  }

  static String? passwordOf(String code) {
    for (final e in loadSync()) {
      if (e['code'] == code) {
        final p = e['pwd'];
        return p is String && p.isNotEmpty ? p : null;
      }
    }
    return null;
  }

  /// 清除指定设备码的记住密码（取消勾选时）
  static void forgetPassword(String code) {
    final list = loadSync();
    var changed = false;
    for (final e in list) {
      if (e['code'] == code && e['pwd'] != null) {
        e.remove('pwd');
        changed = true;
      }
    }
    if (changed) _write(list);
  }

  /// [20261019] 记录被控端 IP 与链路类型（从主控 ICE 统计捕获，真实点对
  /// 点地址，无需服务端配合）。candidateType: host=内网 / srflx|prflx=公网P2P
  /// / relay=中继。
  static void updateIp(String code, String ip, {String? ipType}) {
    if (ip.isEmpty) return;
    final list = loadSync();
    var changed = false;
    for (final e in list) {
      if (e['code'] == code) {
        if (e['ip'] != ip || e['ipType'] != (ipType ?? '')) changed = true;
        e['ip'] = ip;
        if (ipType != null && ipType.isNotEmpty) e['ipType'] = ipType;
        break;
      }
    }
    if (changed) _write(list);
  }

  static String? ipOf(String code) {
    for (final e in loadSync()) {
      if (e['code'] == code) return e['ip'] as String?;
    }
    return null;
  }

  /// [20261019e] 合并探测结果（设备名/出口IP/归属地），只写非空值，
  /// 其余字段（code/at/pwd/ipType）原样保留。
  static void merge(String code, Map<String, dynamic> fields) {
    final list = loadSync();
    var changed = false;
    for (final e in list) {
      if (e['code'] == code) {
        for (final kv in fields.entries) {
          final v = kv.value;
          if (v is String && v.isNotEmpty && e[kv.key] != v) {
            e[kv.key] = v;
            changed = true;
          }
        }
        break;
      }
    }
    if (changed) _write(list);
  }

  static void removeSync(String code) {
    final list = loadSync();
    list.removeWhere((e) => e['code'] == code);
    _write(list);
  }

  static void clearSync() {
    _write([]);
  }

  static void _write(List<Map<String, dynamic>> list) {
    try {
      _file().writeAsStringSync(jsonEncode(list));
    } catch (_) {}
  }
}
