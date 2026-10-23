/// 设备探测 —— 对齐 web 主控 deviceManage/index.vue 的 refresh()：
/// 逐台调 GET /desk_user/find_receiver_by_uuid?uuid=码，
/// 服务端返回 { receiver(在线), name(设备名), ip(出口IP) }，回填进 RecentStore。
/// [20261019e] 这就是 web 主控设备列表一直用的 IP 来源（服务端在被控
/// /desk_user/login 时记录 x-real-ip），不是从 ICE candidate 猜的。
import '../api/user_api.dart' show findReceiverByUuid, ipGeo;

/// 内网 IP 判断（对齐 web ipGeo.isLanIp）
bool isLanIp(String ip) =>
    RegExp(r'^(127\.|10\.|192\.168\.|172\.(1[6-9]|2\d|3[01])\.|169\.254\.)')
        .hasMatch(ip);

class ProbeResult {
  final bool online;
  final String name;
  final String ip;
  final String geo;
  const ProbeResult(
      {required this.online, this.name = '', this.ip = '', this.geo = ''});
}

/// 串行探测一批设备码（服务端 ip_geo 有缓存，串行只为压平请求峰值，
/// 与 web queuedGeo 同理由）。onEach 在每台探测完回调（UI 增量刷新）。
Future<Map<String, ProbeResult>> probeDevices(
  List<String> codes,
  void Function(String code, ProbeResult r) onEach,
) async {
  final out = <String, ProbeResult>{};
  for (final code in codes) {
    if (code.length != 9) continue; // 脏记录不探测（对齐 web isBadId）
    var online = false;
    var name = '';
    var ip = '';
    var geo = '';
    try {
      final r = await findReceiverByUuid(code).timeout(const Duration(seconds: 8));
      if (r.ok && r.data is Map) {
        final d = r.data as Map;
        online = (d['receiver'] ?? '').toString().isNotEmpty;
        name = (d['name'] ?? '').toString().trim();
        ip = (d['ip'] ?? '').toString().trim();
      }
    } catch (_) {}
    if (ip.isNotEmpty && !isLanIp(ip)) {
      try {
        final g = await ipGeo(ip).timeout(const Duration(seconds: 8));
        if (g.ok && g.data is Map) {
          geo = ((g.data as Map)['geo'] ?? '').toString().trim();
        }
      } catch (_) {}
    }
    final res = ProbeResult(online: online, name: name, ip: ip, geo: geo);
    out[code] = res;
    onEach(code, res);
  }
  return out;
}

/// 位置展示串（对齐 web displayLoc）：归属地 (ip) / 裸 ip / 内网 (ip) / 空
String displayLoc(String ip, String geo) {
  if (ip.isEmpty) return '';
  if (isLanIp(ip)) return '内网 ($ip)';
  return geo.isNotEmpty ? '$geo ($ip)' : ip;
}
