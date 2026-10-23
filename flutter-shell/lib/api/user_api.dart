/// 账号 API —— 对齐 web 端 src/api/user.ts：
/// /user/username_login · /user/get_user_info · /account/send_code
/// /account/register_by_email · /account/reset_pwd · /account/my_account_id
/// 认证：Authorization: Bearer <token>（对齐 request.ts 拦截器）。
import 'dart:convert';
import 'dart:io';

import '../signal/identity.dart' show kUpstreamOrigin;

class ApiResult {
  ApiResult({required this.ok, this.data, this.msg});
  final bool ok;
  final dynamic data;
  final String? msg;
}

HttpClient _client() => HttpClient()..findProxy = (_) => 'DIRECT';

/// [20261016b] 上游服务端统一返回 `message` 字段（不是 msg），
/// 之前只读 msg 导致错误提示永远是 null → 界面显示「未知错误」。
String? _msgOf(Map<String, dynamic> json) {
  final m = json['message'] ?? json['msg'];
  return m?.toString();
}

Future<ApiResult> _post(String path, Map<String, dynamic> body,
    {String? token}) async {
  try {
    final req = await _client()
        .postUrl(Uri.parse('$kUpstreamOrigin$path'))
        .timeout(const Duration(seconds: 12));
    req.headers.contentType = ContentType.json;
    if (token != null && token.isNotEmpty) {
      req.headers.set('Authorization', 'Bearer $token');
    }
    req.write(jsonEncode(body));
    final resp = await req.close().timeout(const Duration(seconds: 12));
    final text = await resp.transform(utf8.decoder).join();
    final json = jsonDecode(text) as Map<String, dynamic>;
    final code = json['code'];
    return ApiResult(ok: code == 200, data: json['data'], msg: _msgOf(json));
  } catch (e) {
    return ApiResult(ok: false, msg: '网络异常：$e');
  }
}

Future<ApiResult> _get(String path, {String? token}) async {
  try {
    final req = await _client()
        .getUrl(Uri.parse('$kUpstreamOrigin$path'))
        .timeout(const Duration(seconds: 12));
    if (token != null && token.isNotEmpty) {
      req.headers.set('Authorization', 'Bearer $token');
    }
    final resp = await req.close().timeout(const Duration(seconds: 12));
    final text = await resp.transform(utf8.decoder).join();
    final json = jsonDecode(text) as Map<String, dynamic>;
    final code = json['code'];
    return ApiResult(ok: code == 200, data: json['data'], msg: _msgOf(json));
  } catch (e) {
    return ApiResult(ok: false, msg: '网络异常：$e');
  }
}

/// 用户名+密码登录 → data 为 token 字符串（对齐 store.usernameLogin）
Future<ApiResult> usernameLogin(String username, String password) =>
    _post('/user/username_login', {'username': username, 'password': password});

/// 当前登录用户信息（含 username）
Future<ApiResult> getUserInfo(String token) =>
    _get('/user/get_user_info', token: token);

/// 当前账号对外账号ID + 绑定邮箱
Future<ApiResult> myAccountId(String token) =>
    _get('/account/my_account_id', token: token);

/// 发送邮箱验证码（type=register|reset）
Future<ApiResult> sendEmailCode(String email, String type) =>
    _post('/account/send_code', {'email': email, 'type': type});

/// 邮箱验证码注册 → data 含 account_id
Future<ApiResult> registerByEmail(
        String username, String password, String email, String code) =>
    _post('/account/register_by_email',
        {'username': username, 'password': password, 'email': email, 'code': code});

/// 忘记密码重置
Future<ApiResult> resetPwd(String email, String code, String newpwd) =>
    _post('/account/reset_pwd', {'email': email, 'code': code, 'newpwd': newpwd});

/// [20261019e] 设备探测（对齐 web fetchFindReceiverByUuid）：
/// GET /desk_user/find_receiver_by_uuid?uuid=设备码
/// → data { receiver: socket_id(非空=在线), name: 设备名, ip: 出口IP }。
/// ip 由服务端在被控调 /desk_user/login 时记录（x-real-ip），这是
/// web 主控设备列表一直在用的同一条路，不是从 ICE candidate 猜的。
Future<ApiResult> findReceiverByUuid(String uuid) =>
    _get('/desk_user/find_receiver_by_uuid?uuid=$uuid');

/// [20261019e] IP 归属地（服务端代理，对齐 web ipGeo.fetchGeo）：
/// GET /desk_user/ip_geo?ip=x.x.x.x → data { geo: '省份 城市' }。
/// 服务端 7 天内存缓存，失败/超时由调用方兜底为裸 IP。
Future<ApiResult> ipGeo(String ip) => _get('/desk_user/ip_geo?ip=$ip');

Future<ApiResult> _put(String path, Map<String, dynamic> body) async {
  try {
    final req = await _client()
        .putUrl(Uri.parse('$kUpstreamOrigin$path'))
        .timeout(const Duration(seconds: 12));
    req.headers.contentType = ContentType.json;
    req.write(jsonEncode(body));
    final resp = await req.close().timeout(const Duration(seconds: 12));
    final text = await resp.transform(utf8.decoder).join();
    final json = jsonDecode(text) as Map<String, dynamic>;
    return ApiResult(
        ok: json['code'] == 200, data: json['data'], msg: _msgOf(json));
  } catch (e) {
    return ApiResult(ok: false, msg: '网络异常：$e');
  }
}

/// 被控设备改临时密码（对齐 web handleUpdatePassword：
/// PUT /desk_user/update_by_uuid {uuid, password旧, new_password}）。
///
/// [20261016b] 上游服务端有一个坑（curl 实测定性）：
/// 旧密码正确时密码**实际已改成功**，但响应仍返回 400「旧密码错误！」
///（服务端先改后验、失败不回滚）；旧密码错误时返回同样文案但不生效。
/// 所以 PUT 响应的 code 不可信 —— 改完后用 /desk_user/login 实测
/// 新密码，以登录结果为最终判据：
/// · login(新密码) 成功 → 改密成功（无论 PUT 返回什么）
/// · login(新密码) 失败 → 真失败，透传服务端 message
Future<ApiResult> updateDeskUserPassword(
    String uuid, String oldPwd, String newPwd) async {
  final r = await _put('/desk_user/update_by_uuid',
      {'uuid': uuid, 'password': oldPwd, 'new_password': newPwd});
  final v = await _post('/desk_user/login', {'uuid': uuid, 'password': newPwd});
  if (v.ok) {
    return ApiResult(ok: true, data: r.data, msg: '密码修改成功');
  }
  return ApiResult(ok: false, msg: r.msg ?? v.msg ?? '密码修改未生效');
}

/// 设备身份登录验证（POST /desk_user/login {uuid, password}）。
/// [20261016b] 改密失败时的二次诊断：旧密码还能登 → 真失败；
/// 旧密码也登不上 → 本地身份文件与服务端不同步。
Future<bool> verifyDeskLogin(String uuid, String password) async {
  final r = await _post('/desk_user/login', {'uuid': uuid, 'password': password});
  return r.ok;
}
