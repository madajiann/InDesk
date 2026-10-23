/// 高级设置页 —— 板块对齐 web 端 views/setting/index.vue：
/// 界面设置（主窗口置顶）/ 开机自启 / 接口配置 / 产品信息 / 关于InDesk
import 'dart:io';

import 'package:flutter/material.dart';
import 'package:window_manager/window_manager.dart';

import '../core/ipc.dart';
import 'widgets/pressable.dart';
import '../core/prefs.dart';

const brand = Color(0xFF2F6FED);

class SettingsPage extends StatefulWidget {
  const SettingsPage({super.key});

  @override
  State<SettingsPage> createState() => _SettingsPageState();
}

class _SettingsPageState extends State<SettingsPage> {
  String _version = '';

  @override
  void initState() {
    super.initState();
    _loadVersion();
  }

  Future<void> _loadVersion() async {
    final v = await Ipc.instance.getShellVersion();
    if (!mounted) return;
    final m = RegExp(r'version: ([\d.]+)').firstMatch(v.toString());
    setState(() => _version = m?.group(1) ?? '');
  }

  /// 开机自启：写 HKCU\...\Run 注册表（发布环境生效）
  Future<void> _setAutoStart(bool enable) async {
    final exe = Platform.resolvedExecutable;
    try {
      if (enable) {
        await Process.run('reg', [
          'add', r'HKCU\Software\Microsoft\Windows\CurrentVersion\Run',
          '/v', 'InDesk', '/t', 'REG_SZ', '/d', '"$exe"', '/f',
        ]);
      } else {
        await Process.run('reg', [
          'delete', r'HKCU\Software\Microsoft\Windows\CurrentVersion\Run',
          '/v', 'InDesk', '/f',
        ]);
      }
      ShellPrefs.autoStart = enable;
      if (!mounted) return;
      setState(() {});
      _toast(enable ? '已开启开机自启' : '已关闭开机自启');
    } catch (e) {
      if (!mounted) return;
      _toast('设置失败：$e');
    }
  }

  Future<void> _setAlwaysOnTop(bool enable) async {
    ShellPrefs.alwaysOnTop = enable;
    await windowManager.setAlwaysOnTop(enable);
    if (!mounted) return;
    setState(() {});
  }

  void _toast(String msg) {
    ScaffoldMessenger.of(context)
      ..hideCurrentSnackBar()
      ..showSnackBar(SnackBar(
          content: Text(msg), behavior: SnackBarBehavior.floating));
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
        child: Column(crossAxisAlignment: CrossAxisAlignment.start,
            children: [
          _label('界面设置'),
          _radioRow('主窗口置顶', ShellPrefs.alwaysOnTop, _setAlwaysOnTop),
          _hr(),
          _label('开机自启'),
          _radioRow('开机自动启动', ShellPrefs.autoStart, _setAutoStart),
          _hr(),
          _label('接口配置'),
          _kvRow('信令服务器', 'https://desk.yomm.top:8443'),
          _hr(),
          _label('产品信息'),
          _kvRow('产品名称', 'InDesk'),
          _kvRow('核心版本', _version.isEmpty ? '…' : 'v$_version'),
          _hr(),
          _label('关于InDesk'),
          Padding(
            padding: const EdgeInsets.only(top: 8, bottom: 4),
            child: Text(
              'InDesk —— 便捷式免费远程连接软件。基于 WebRTC 的远程桌面协助工具，'
              '支持设备码直连、无人值守、隐私屏、连接画质档位等能力。',
              style: TextStyle(
                  fontSize: 12, height: 1.7, color: Colors.grey.shade600),
            ),
          ),
        ]),
      ),
    );
  }

  Widget _label(String text) => Padding(
        padding: const EdgeInsets.only(bottom: 10),
        child: Text(text,
            style: TextStyle(
                fontSize: 13,
                fontWeight: FontWeight.w700,
                color: Colors.grey.shade800)),
      );

  Widget _hr() => Padding(
        padding: const EdgeInsets.symmetric(vertical: 14),
        child: Divider(height: 1, color: Colors.grey.shade200),
      );

  Widget _radioRow(String label, bool value, ValueChanged<bool> onChange) {
    return Row(children: [
      Text('$label：',
          style: TextStyle(fontSize: 12.5, color: Colors.grey.shade600)),
      const SizedBox(width: 8),
      _radio('是', value, () => onChange(true)),
      const SizedBox(width: 18),
      _radio('否', !value, () => onChange(false)),
    ]);
  }

  Widget _radio(String label, bool checked, VoidCallback onTap) {
    return Pressable(
      onTap: onTap,
      radius: 20,
      child: Row(mainAxisSize: MainAxisSize.min, children: [
        Container(
          width: 16,
          height: 16,
          decoration: BoxDecoration(
            shape: BoxShape.circle,
            color: checked ? brand : Colors.white,
            border: Border.all(
                color: checked ? brand : Colors.grey.shade400, width: 1.6),
          ),
          child: checked
              ? const Icon(Icons.check, size: 10, color: Colors.white)
              : null,
        ),
        const SizedBox(width: 6),
        Text(label, style: const TextStyle(fontSize: 12.5)),
      ]),
    );
  }

  Widget _kvRow(String k, String v) => Padding(
        padding: const EdgeInsets.only(bottom: 8),
        child: Row(children: [
          SizedBox(
              width: 90,
              child: Text(k,
                  style: TextStyle(
                      fontSize: 12.5, color: Colors.grey.shade600))),
          Expanded(
            child: Text(v,
                style: const TextStyle(
                    fontSize: 12.5,
                    color: Color(0xFF182236),
                    fontWeight: FontWeight.w500)),
          ),
        ]),
      );
}
