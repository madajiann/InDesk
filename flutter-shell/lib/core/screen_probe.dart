/// [20261021 九轮] 抓屏能力探针 —— 只为了在**没人连进来**的时候验证
/// flutter_webrtc 的 Windows 桌面捕获在本机是否真的可用。
///
/// 背景：Windows 插件里确实编进了 `common/cpp/src/flutter_screen_capture.cc`
/// （编译宏 RTC_DESKTOP_DEVICE），但这条路径历史上从没被测过。它一旦不可用，
/// 被控端就只会静默回退到老的 DataChannel 直送 —— 也就是"糊 + 点不动"那条。
/// 所以先让探针把"能不能采、采出来几路、SDP 里是什么编码"写进 shell.log，
/// 而不是拿用户当小白鼠。
///
/// 用法（带环境变量启动壳，探针跑完直接 exit）：
///   set INDESK_PROBE_SCREEN=1 && indesk.exe
import 'dart:io';

import 'package:flutter_webrtc/flutter_webrtc.dart';

import 'shell_log.dart';

class ScreenProbe {
  static Future<void> run() async {
    final t0 = DateTime.now();
    ShellLog.w('[probe] ===== 抓屏能力探针开始 =====');
    RTCPeerConnection? pc;
    MediaStream? stream;
    try {
      final sources = await desktopCapturer.getSources(
        types: [SourceType.Screen],
        thumbnailSize: ThumbnailSize(0, 0),
      );
      ShellLog.w('[probe] 屏幕源 ${sources.length} 个');
      for (final s in sources) {
        ShellLog.w('[probe]   源 id=${s.id} name="${s.name}" '
            'type=${s.type} thumb=${s.thumbnailSize.width}x'
            '${s.thumbnailSize.height}');
      }
      if (sources.isEmpty) {
        ShellLog.w('[probe] 结论：没有屏幕源 → 视频轨方案不可用');
        return;
      }

      stream = await navigator.mediaDevices.getDisplayMedia(<String, dynamic>{
        'video': <String, dynamic>{
          'deviceId': <String, dynamic>{'exact': sources.first.id},
          'mandatory': <String, dynamic>{
            'frameRate': 30.0,
            'minFrameRate': 10.0,
          },
          'optional': <String, dynamic>{},
        },
        'audio': false,
      });
      final tracks = stream.getVideoTracks();
      ShellLog.w('[probe] getDisplayMedia OK：${tracks.length} 条视频轨');
      if (tracks.isEmpty) {
        ShellLog.w('[probe] 结论：getDisplayMedia 没给出视频轨 → 不可用');
        return;
      }
      final track = tracks.first;
      ShellLog.w('[probe] 轨道 id=${track.id} label=${track.label} '
          'kind=${track.kind} enabled=${track.enabled}');

      pc = await createPeerConnection(<String, dynamic>{
        'iceServers': <Map<String, dynamic>>[],
        'sdpSemantics': 'unified-plan',
      });
      await pc.addTrack(track, stream);
      final offer = await pc.createOffer(<String, dynamic>{});
      await pc.setLocalDescription(offer);
      final sdp = offer.sdp ?? '';
      final mVideo = RegExp(r'^m=video .*$', multiLine: true)
          .firstMatch(sdp)
          ?.group(0);
      final rtpmap = RegExp(r'^a=rtpmap:\d+ (\S+/\d+).*$', multiLine: true)
          .allMatches(sdp)
          .map((m) => m.group(1))
          .where((e) => e != null)
          .toSet()
          .join(',');
      ShellLog.w('[probe] SDP ${sdp.length}B  m=video: ${mVideo ?? '无'}');
      ShellLog.w('[probe] SDP 编码: $rtpmap');
      final senders = await pc.getSenders();
      ShellLog.w('[probe] 发送侧 ${senders.length} 个 sender');
      ShellLog.w('[probe] 结论：**视频轨方案可用** '
          '（耗时 ${DateTime.now().difference(t0).inMilliseconds}ms）');

      // mode=2：本地环回压测 —— 两个 PC 互连，只跑发送侧编码，
      // 用 outbound-rtp 的 framesEncoded/framesPerSecond 回答一个问题：
      // libwebrtc 的 VP8 软编在**这台 i5-4590** 上能出多少帧。
      // mode=3：同上但把 H264 排到编码器偏好第一位 —— 若 libwebrtc 走
      // MediaFoundation 硬编，CPU 会断崖式下降，那就是最优解。
      final mode = Platform.environment['INDESK_PROBE_SCREEN'] ?? '';
      if (mode == '2') {
        await _loopback(track, stream, preferH264: false);
      } else if (mode == '3') {
        await _loopback(track, stream, preferH264: true);
      }
      ShellLog.w('[probe] 结论：**视频轨方案可用** '
          '（耗时 ${DateTime.now().difference(t0).inMilliseconds}ms）');
    } catch (e, st) {
      ShellLog.w('[probe] 失败: $e');
      ShellLog.w('[probe] 栈: ${st.toString().split('\n').take(6).join(' | ')}');
      ShellLog.w('[probe] 结论：**视频轨方案不可用**，需回退 DataChannel 直送');
    } finally {
      try {
        await pc?.close();
      } catch (_) {}
      try {
        await stream?.dispose();
      } catch (_) {}
      ShellLog.w('[probe] ===== 探针结束 =====');
    }
  }

  /// 本地环回压测：tx 发抓屏轨 → rx 收，只统计**发送侧编码**（CPU 大头）。
  /// 目的：验证 libwebrtc 的 VP8 软编在这台 i5-4590 上到底能出多少帧、
  /// 什么分辨率 —— 这是决定"换轨道会不会把 CPU 打爆"的唯一硬数据。
  static Future<void> _loopback(
      MediaStreamTrack track, MediaStream stream,
      {bool preferH264 = false}) async {
    RTCPeerConnection? tx;
    RTCPeerConnection? rx;
    try {
      final cfg = <String, dynamic>{
        'iceServers': <Map<String, dynamic>>[],
        'sdpSemantics': 'unified-plan',
      };
      tx = await createPeerConnection(cfg);
      rx = await createPeerConnection(cfg);
      tx.onIceCandidate = (RTCIceCandidate c) async {
        if ((c.candidate ?? '').isEmpty) return;
        try {
          await rx?.addCandidate(c);
        } catch (_) {}
      };
      rx.onIceCandidate = (RTCIceCandidate c) async {
        if ((c.candidate ?? '').isEmpty) return;
        try {
          await tx?.addCandidate(c);
        } catch (_) {}
      };

      final caps = await getRtpSenderCapabilities('video');
      final codecs = caps.codecs ?? <RTCRtpCodecCapability>[];
      ShellLog.w('[probe] 编码能力: '
          '${codecs.map((c) => c.mimeType).toSet().join(', ')}');
      // 帧率上限从环境变量读，免去"改一次参数重建一次"的往返
      final wantFps =
          int.tryParse(Platform.environment['INDESK_PROBE_FPS'] ?? '') ?? 30;
      ShellLog.w('[probe] 本次压测帧率上限 = $wantFps');

      final sender = await tx.addTrack(track, stream);
      final params = sender.parameters;
      final enc = (params.encodings != null && params.encodings!.isNotEmpty)
          ? params.encodings!.first
          : RTCRtpEncoding();
      enc.active = true;
      enc.maxBitrate = 8000 * 1000;
      enc.maxFramerate = wantFps;
      params.encodings = <RTCRtpEncoding>[enc];
      params.degradationPreference = RTCDegradationPreference.MAINTAIN_RESOLUTION;
      await sender.setParameters(params);

      if (preferH264) {
        final trans = await tx.getTransceivers();
        for (final t in trans) {
          if (t.sender.track?.kind != 'video') continue;
          final h264 = codecs
              .where((c) => (c.mimeType ?? '').toLowerCase().contains('h264'))
              .toList();
          final rest = codecs
              .where((c) => !(c.mimeType ?? '').toLowerCase().contains('h264'))
              .toList();
          if (h264.isEmpty) {
            ShellLog.w('[probe] 发送能力里没有 H264，偏好设置跳过');
            break;
          }
          await t.setCodecPreferences(<RTCRtpCodecCapability>[
            ...h264,
            ...rest,
          ]);
          ShellLog.w('[probe] 已把 H264（${h264.length} 个 profile）排到首位');
          break;
        }
      }

      final offer = await tx.createOffer(<String, dynamic>{});
      await tx.setLocalDescription(offer);
      await rx.setRemoteDescription(offer);
      final answer = await rx.createAnswer(<String, dynamic>{});
      await rx.setLocalDescription(answer);
      await tx.setRemoteDescription(answer);
      ShellLog.w('[probe] 环回 PC 已协商，开始采样（30s）');

      for (var i = 1; i <= 6; i++) {
        await Future<void>.delayed(const Duration(seconds: 5));
        final stats = await tx.getStats();
        for (final s in stats) {
          if (s.type == 'outbound-rtp' && s.values['kind'] == 'video') {
            ShellLog.w('[probe] t=${i * 5}s 编码帧=${s.values['framesEncoded']} '
                'fps=${s.values['framesPerSecond']} '
                '发送=${s.values['bytesSent']}B '
                '尺寸=${s.values['frameWidth']}x${s.values['frameHeight']} '
                '总编码秒=${s.values['totalEncodeTime']} '
                '质量限制=${s.values['qualityLimitationReason']}');
          }
        }
      }
      ShellLog.w('[probe] 环回采样结束');
    } catch (e) {
      ShellLog.w('[probe] 环回失败: $e');
    } finally {
      try {
        await tx?.close();
      } catch (_) {}
      try {
        await rx?.close();
      } catch (_) {}
    }
  }
}
