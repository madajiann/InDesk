/// 主控端自身身份 —— 对齐 web 端语义：
/// 服务端 deskStartRemote 校验 remoteDeskUserUuid/Password（= 主控自己），
/// 且要求该身份在 Redis 在线（靠周期 deskJoin 自己的房间续 10s 键）。
/// 身份通过 /desk_user/create 创建一次，持久化到 exe 同目录复用。
import 'dart:convert';
import 'dart:io';

import 'package:flutter/foundation.dart';

class ControllerIdentity {
  ControllerIdentity({required this.uuid, required this.password});

  final String uuid;
  final String password;

  Map<String, dynamic> toJson() => {'uuid': uuid, 'password': password};

  static ControllerIdentity? fromJson(Map<String, dynamic> j) {
    final u = j['uuid'] as String?;
    final p = j['password'] as String?;
    if (u == null || u.isEmpty || p == null || p.isEmpty) return null;
    return ControllerIdentity(uuid: u, password: p);
  }
}

/// 上游服务端 origin（与 web PRIVATE_DESK_ORIGIN 同源）
const kUpstreamOrigin = 'https://desk.yomm.top:8443';

File? _identityFile() {
  final appdata = Platform.environment['APPDATA'];
  if (appdata == null || appdata.isEmpty) return null;
  return File('$appdata\\InDesk\\controller_identity.json');
}

/// 被控真实身份（%APPDATA%\InDesk\host_identity.json，由被控模块启动时写入）。
/// 与主控 controller_identity 是两条不同的 /desk_user/create 记录：
/// 对方连本机时用的是被控这条，壳显示「本机设备代码」必须用它。
/// 文件不存在说明被控模块从未运行过 —— 不代创建（创建了也和被控
/// WebView 里的 roomId 对不上），显示占位并等被控拉起后刷新。
/// [20261016] 与被控写盘位置统一到 APPDATA（exe 目录在 Program Files 下
/// 只读，且安装包会把开发机的文件带出去导致两台机器代码相同）。
File? _hostIdentityFile() {
  // [20261020] 测试隔离：与 receiver/host_identity.dart 同一套覆盖，
  // INDESK_IDENTITY_FILE 指向独立文件，自动化测试不再碰用户真实身份。
  final override = Platform.environment['INDESK_IDENTITY_FILE'];
  if (override != null && override.isNotEmpty) return File(override);
  final appdata = Platform.environment['APPDATA'];
  if (appdata == null || appdata.isEmpty) return null;
  return File('$appdata\\InDesk\\host_identity.json');
}

ControllerIdentity? loadHostIdentity() {
  try {
    final f = _hostIdentityFile();
    if (f == null || !f.existsSync()) return null;
    return ControllerIdentity.fromJson(
      Map<String, dynamic>.from(jsonDecode(f.readAsStringSync()) as Map),
    );
  } catch (_) {
    return null;
  }
}

/// 壳端改密码成功后同步更新身份文件（被控下次启动靠它拿到新密码）。
bool saveHostIdentityPassword(String newPassword) {
  try {
    final f = _hostIdentityFile();
    if (f == null || !f.existsSync()) return false;
    final m = Map<String, dynamic>.from(jsonDecode(f.readAsStringSync()) as Map);
    m['password'] = newPassword;
    f.writeAsStringSync(jsonEncode(m));
    return true;
  } catch (_) {
    return false;
  }
}

/// 读本地缓存身份；无效则调 API 创建新身份并落盘。
Future<ControllerIdentity?> ensureControllerIdentity() async {  final f = _identityFile();
  if (f != null && f.existsSync()) {
    try {
      final id = ControllerIdentity.fromJson(
        Map<String, dynamic>.from(jsonDecode(f.readAsStringSync()) as Map),
      );
      if (id != null) {
        debugPrint('[identity] 复用本地身份 ${id.uuid}');
        return id;
      }
    } catch (_) {}
  }
  return createControllerIdentity();
}

/// 删除本地身份并重新注册（对齐 web 端 handleResetDeskuuid 刷新图标语义）。
Future<ControllerIdentity?> createControllerIdentity() async {
  final f = _identityFile();
  try {
    if (f != null && f.existsSync()) f.deleteSync();
  } catch (_) {}
  try {
    final req = await HttpClient()
        .postUrl(Uri.parse('$kUpstreamOrigin/desk_user/create'))
        .timeout(const Duration(seconds: 10));
    req.headers.contentType = ContentType.json;
    final resp = await req.close().timeout(const Duration(seconds: 10));
    final body = await resp.transform(utf8.decoder).join();
    final json = jsonDecode(body) as Map<String, dynamic>;
    final code = json['code'];
    final data = json['data'];
    if (code == 200 && data is Map) {
      final id = ControllerIdentity.fromJson(
        Map<String, dynamic>.from(data),
      );
      if (id != null) {
        if (f != null) {
          f.parent.createSync(recursive: true);
          f.writeAsStringSync(jsonEncode(id.toJson()));
        }
        debugPrint('[identity] 创建新身份 ${id.uuid}');
        return id;
      }
    }
    debugPrint('[identity] create 响应异常: $body');
  } catch (e) {
    debugPrint('[identity] 创建身份失败: $e');
  }
  return null;
}
