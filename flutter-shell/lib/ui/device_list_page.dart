/// 设备列表页 —— 板块对齐 web 端 views/deviceManage/index.vue：
/// 「最近连接」label + 刷新探测按钮 + 列表（在线状态点/设备名/子行信息/删除）。
/// [20261019e] IP/设备名/在线状态改为真实探测：逐台调服务端
/// find_receiver_by_uuid（web 主控同一条路），结果回填 RecentStore 持久化。
import 'package:flutter/material.dart';

import '../core/device_probe.dart';
import '../core/recent.dart';
import 'widgets/pressable.dart';

const brand = Color(0xFF2F6FED);

class DeviceListPage extends StatefulWidget {
  const DeviceListPage({super.key});

  @override
  State<DeviceListPage> createState() => _DeviceListPageState();
}

class _DeviceListPageState extends State<DeviceListPage> {
  List<Map<String, dynamic>> _recent = [];
  bool _probing = false;
  // 在线状态（探测结果，内存态）：true=在线 / false=离线 / 无键=未知
  final Map<String, bool> _online = {};

  @override
  void initState() {
    super.initState();
    _recent = RecentStore.loadSync();
    _probe();
  }

  void _probe() {
    if (_probing) return;
    setState(() => _probing = true);
    probeDevices(_recent.map((e) => e['code'] as String).toList(), (code, r) {
      // 每台探完立即回填持久化并刷新界面（增量，不等整批）
      RecentStore.merge(code, {'name': r.name, 'ip': r.ip, 'geo': r.geo});
      if (!mounted) return;
      setState(() {
        _online[code] = r.online;
        _recent = RecentStore.loadSync();
      });
    }).then((_) {
      if (mounted) setState(() => _probing = false);
    });
  }

  /// 位置展示（对齐 web displayLoc）：归属地 (ip) / 裸 IP / 内网 (ip) / IP 未知
  String _loc(Map<String, dynamic> it) {
    final ip = (it['ip'] ?? '').toString();
    final geo = (it['geo'] ?? '').toString();
    if (ip.isEmpty) return 'IP 未知';
    return displayLoc(ip, geo);
  }

  @override
  Widget build(BuildContext context) {
    return SingleChildScrollView(
      padding: const EdgeInsets.fromLTRB(26, 18, 26, 18),
      child: Container(
        padding: const EdgeInsets.all(18),
        decoration: BoxDecoration(
          color: Colors.white,
          borderRadius: BorderRadius.circular(12),
          border: Border.all(color: const Color(0xFFDCE7F8)),
        ),
        child: Column(crossAxisAlignment: CrossAxisAlignment.start, children: [
          Row(children: [
            Text('最近连接',
                style: TextStyle(
                    fontSize: 13.5,
                    fontWeight: FontWeight.w700,
                    color: Colors.grey.shade800)),
            const SizedBox(width: 10),
            Pressable(
              onTap: _probing ? null : _probe,
              radius: 6,
              child: _probing
                  ? const SizedBox(
                      width: 14,
                      height: 14,
                      child: CircularProgressIndicator(strokeWidth: 2))
                  : Icon(Icons.refresh_rounded,
                      size: 17, color: Colors.grey.shade500),
            ),
          ]),
          const SizedBox(height: 14),
          if (_recent.isEmpty)
            Padding(
              padding: const EdgeInsets.symmetric(vertical: 46),
              child: Center(
                child: Column(children: [
                  Icon(Icons.devices_outlined,
                      size: 42, color: Colors.grey.shade300),
                  const SizedBox(height: 10),
                  Text('还没有设备记录',
                      style: TextStyle(
                          fontSize: 13, color: Colors.grey.shade400)),
                  const SizedBox(height: 4),
                  Text('在「远程控制」页输入对方设备码连接后，会出现在这里',
                      style:
                          TextStyle(fontSize: 11.5, color: Colors.grey.shade300)),
                ]),
              ),
            )
          else
            ..._recent.map(_deviceRow),
        ]),
      ),
    );
  }

  Widget _deviceRow(Map<String, dynamic> it) {
    final code = it['code'] as String;
    final name = (it['name'] ?? '').toString();
    final dt = DateTime.fromMillisecondsSinceEpoch(it['at'] as int);
    final ts =
        '${dt.year}/${dt.month.toString().padLeft(2, '0')}/${dt.day.toString().padLeft(2, '0')} ${dt.hour.toString().padLeft(2, '0')}:${dt.minute.toString().padLeft(2, '0')}';
    final on = _online[code];
    final dotColor = on == null
        ? Colors.grey.shade300
        : (on ? const Color(0xFF22C55E) : const Color(0xFFEF4444));
    final stateText = on == null ? '未知' : (on ? '在线' : '离线');
    final stateColor = on == null
        ? Colors.grey.shade400
        : (on ? const Color(0xFF16A34A) : const Color(0xFFDC2626));
    return Container(
      padding: const EdgeInsets.symmetric(horizontal: 4, vertical: 10),
      decoration: BoxDecoration(
          border: Border(
              bottom: BorderSide(color: Colors.grey.shade100))),
      child: Row(children: [
        // 在线状态点：真实探测（被控连着信令 WebSocket 才有 socket_id）
        Container(
          width: 9,
          height: 9,
          decoration: BoxDecoration(shape: BoxShape.circle, color: dotColor),
        ),
        const SizedBox(width: 12),
        Expanded(
          child: Column(crossAxisAlignment: CrossAxisAlignment.start,
              children: [
                Text(name.isNotEmpty ? name : code,
                    style: const TextStyle(
                        fontSize: 14,
                        fontWeight: FontWeight.w600,
                        letterSpacing: 1.2,
                        color: Color(0xFF182236))),
                const SizedBox(height: 3),
                // 副行（对齐 web dev-sub）：设备码 · 位置(IP) · 最后连接时间
                Text('码 $code · ${_loc(it)} · 最后连接 $ts',
                    maxLines: 1,
                    overflow: TextOverflow.ellipsis,
                    style: TextStyle(
                        fontSize: 11.5, color: Colors.grey.shade400)),
              ]),
        ),
        Padding(
          padding: const EdgeInsets.only(right: 12),
          child: Text(stateText,
              style: TextStyle(fontSize: 12.5, color: stateColor)),
        ),
        Pressable(
          onTap: () {
            RecentStore.removeSync(code);
            setState(() => _recent = RecentStore.loadSync());
          },
          radius: 8,
          child: Icon(Icons.delete_outline_rounded,
              size: 17, color: Colors.grey.shade400),
        ),
      ]),
    );
  }
}
