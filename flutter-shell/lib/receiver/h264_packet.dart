/// H264 直送分包协议 —— 逐行移植 web/src/utils/h264Packet.ts。
/// 24 字节小端包头 + 16KB 分片；与主控端 h264_direct.dart 的组帧器互为镜像。
import 'dart:typed_data';

const int pktHeader = 24;

/// 单分片最大载荷（SCTP MTU≈1200B 下 16KB 的取舍见 web 版注释）
const int pktChunk = 16 * 1024;

const int pktFlagKey = 1;

/// 起始码长度：3（001）或 4（0001），非起始码 0
int _startCodeLen(Uint8List b, int i) {
  if (b[i] != 0 || b[i + 1] != 0) return 0;
  if (b[i + 2] == 1) return 3;
  if (b[i + 2] == 0 && b[i + 3] == 1) return 4;
  return 0;
}

class FrameScan {
  FrameScan({required this.key, required this.hasSps, required this.hasPps});
  final bool key;
  final bool hasSps;
  final bool hasPps;
}

/// 扫 Annex-B 码流判断是否含 IDR（NAL type 5）
FrameScan scanFrame(Uint8List b) {
  var key = false, hasSps = false, hasPps = false;
  final n = b.length;
  var i = 0;
  while (i + 4 < n) {
    final sl = _startCodeLen(b, i);
    if (sl == 0) {
      i += 1;
      continue;
    }
    final nri = i + sl;
    final type = b[nri] & 0x1f;
    if (type == 5) {
      key = true;
    } else if (type == 7) {
      hasSps = true;
    } else if (type == 8) {
      hasPps = true;
    }
    i = nri + 1;
  }
  return FrameScan(key: key, hasSps: hasSps, hasPps: hasPps);
}

/// 把一帧 Annex-B H264 切成 24B 包头 + 载荷的分片
List<Uint8List> packetize(
    Uint8List bytes, int frameId, int tsMs, int width, int height, bool key) {
  var count = (bytes.length + pktChunk - 1) ~/ pktChunk;
  if (count < 1) count = 1;
  if (count > 0xffff) return const [];
  final out = <Uint8List>[];
  for (var ci = 0; ci < count; ci++) {
    final s = ci * pktChunk;
    final e = (s + pktChunk > bytes.length) ? bytes.length : s + pktChunk;
    final pkt = Uint8List(pktHeader + (e - s));
    final dv = ByteData.sublistView(pkt);
    dv.setUint32(0, frameId & 0xFFFFFFFF, Endian.little);
    dv.setUint32(4, tsMs & 0xFFFFFFFF, Endian.little);
    dv.setUint16(8, ci, Endian.little);
    dv.setUint16(10, count, Endian.little);
    dv.setUint32(12, bytes.length & 0xFFFFFFFF, Endian.little);
    dv.setUint16(16, width & 0xFFFF, Endian.little);
    dv.setUint16(18, height & 0xFFFF, Endian.little);
    pkt[20] = key ? pktFlagKey : 0;
    pkt[21] = 0;
    dv.setUint16(22, 0, Endian.little);
    pkt.setRange(pktHeader, pktHeader + (e - s), bytes, s);
    out.add(pkt);
  }
  return out;
}

/// frameId u32 环形比较：a 是否比 b 新
bool isNewer(int a, int b) => ((a - b) & 0xFFFFFFFF) > 0 && (a - b) < 0x80000000;
