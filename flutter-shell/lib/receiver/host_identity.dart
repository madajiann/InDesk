/// 被控端身份（本机设备代码）—— [20261019 被控去 WebView2] Flutter 直营版。
///
/// 旧架构：身份由 Tauri 被控 webview 前端 initDeskUser() 注册并写
/// host_identity.json，壳只读。新架构：壳自己就是被控宿主，身份的
/// 校验/创建/改密全部在 Dart 完成（服务端接口与 web 完全一致）：
/// - 已有身份 → POST /desk_user/login 校验，通过即用；
/// - 校验失败/无身份 → POST /desk_user/create 注册新设备码并落盘。
/// 文件位置不变（%APPDATA%\InDesk\host_identity.json），老版本升级无缝。
import 'dart:async';
import 'dart:convert';
import 'dart:io';

import 'package:flutter/foundation.dart';

import '../signal/identity.dart'
    show ControllerIdentity, kUpstreamOrigin, loadHostIdentity;

class HostIdentityResult {
  HostIdentityResult({required this.ok, this.identity, this.msg});
  final bool ok;
  final ControllerIdentity? identity;
  final String? msg;
}

File? _hostIdentityFile() {
  // [20261020] 测试隔离：INDESK_IDENTITY_FILE 指定独立身份文件，回环/自动化
  // 测试用它避免与用户真实身份（%APPDATA% 共享文件）互相覆盖 —— 此前测试
  // 与用户实例互杀时曾把用户设备码竞态重建掉，现场必须防复发。
  final override = Platform.environment['INDESK_IDENTITY_FILE'];
  if (override != null && override.isNotEmpty) return File(override);
  final appdata = Platform.environment['APPDATA'];
  if (appdata == null || appdata.isEmpty) return null;
  return File('$appdata\\InDesk\\host_identity.json');
}

bool _saveHostIdentity(ControllerIdentity id) {
  try {
    final f = _hostIdentityFile();
    if (f == null) return false;
    f.parent.createSync(recursive: true);
    f.writeAsStringSync(jsonEncode(id.toJson()));
    return true;
  } catch (_) {
    return false;
  }
}

/// POST JSON 到服务端并解析 {code, data}（desk_user 系接口公共形状）。
Future<Map<String, dynamic>?> _postDeskUser(
    String path, Map<String, dynamic> body) async {
  try {
    final client = HttpClient();
    final req = await client
        .postUrl(Uri.parse('$kUpstreamOrigin$path'))
        .timeout(const Duration(seconds: 10));
    req.headers.contentType = ContentType.json;
    req.write(jsonEncode(body));
    final resp = await req.close().timeout(const Duration(seconds: 10));
    final text = await resp.transform(utf8.decoder).join();
    client.close(force: true);
    final json = jsonDecode(text);
    if (json is Map<String, dynamic>) return json;
  } catch (e) {
    debugPrint('[host-id] $path 失败: $e');
  }
  return null;
}

/// 校验已有身份（POST /desk_user/login）
Future<bool> verifyHostLogin(String uuid, String password) async {
  final res = await _postDeskUser('/desk_user/login', {
    'uuid': uuid,
    'password': password,
  });
  return res != null && res['code'] == 200;
}

/// 确保被控身份可用：校验 → 失败重建 → 落盘。永不抛异常。
Future<HostIdentityResult> ensureHostIdentity() async {
  final cached = loadHostIdentity();
  if (cached != null) {
    final ok = await verifyHostLogin(cached.uuid, cached.password);
    if (ok) {
      debugPrint('[host-id] 身份校验通过 ${cached.uuid}');
      return HostIdentityResult(ok: true, identity: cached);
    }
    debugPrint('[host-id] 本地身份失效（服务端清档/改密），重新注册');
  }
  final res = await _postDeskUser('/desk_user/create', {});
  if (res != null && res['code'] == 200 && res['data'] is Map) {
    final id = ControllerIdentity.fromJson(
      Map<String, dynamic>.from(res['data'] as Map),
    );
    if (id != null) {
      _saveHostIdentity(id);
      debugPrint('[host-id] 注册新身份 ${id.uuid}');
      return HostIdentityResult(ok: true, identity: id);
    }
  }
  return HostIdentityResult(
      ok: false, msg: res?['msg']?.toString() ?? '注册失败（网络/服务端异常）');
}

/// 修改本机临时密码（PUT /desk_user/update_by_uuid，对齐 web handleUpdatePassword）
Future<({bool ok, String? msg})> updateHostPassword(
    String uuid, String oldPwd, String newPwd) async {
  try {
    final client = HttpClient();
    final req = await client
        .putUrl(Uri.parse('$kUpstreamOrigin/desk_user/update_by_uuid'))
        .timeout(const Duration(seconds: 10));
    req.headers.contentType = ContentType.json;
    req.write(jsonEncode({
      'uuid': uuid,
      'password': oldPwd,
      'new_password': newPwd,
    }));
    final resp = await req.close().timeout(const Duration(seconds: 10));
    final text = await resp.transform(utf8.decoder).join();
    client.close(force: true);
    final json = jsonDecode(text);
    if (json is Map<String, dynamic> && json['code'] == 200) {
      return (ok: true, msg: null);
    }
    return (
      ok: false,
      msg: json is Map ? json['msg']?.toString() ?? '失败' : '失败'
    );
  } catch (e) {
    return (ok: false, msg: e.toString());
  }
}
