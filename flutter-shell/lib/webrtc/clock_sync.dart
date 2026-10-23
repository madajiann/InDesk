/// [20261019j] 跨机时钟对齐（1:1 移植 web 主控 utils/clockSync.ts 的估计器）。
///
/// 背景：H264 直送的帧时间戳是被控端 WebView 的 `performance.now()`（页面
/// 导航原点），与主控端的钟没有任何同源关系。要算「发送端封包 → 主控端上屏」
/// 的端到端延迟，必须先量出钟差 offset（四时间戳法）：
///
///   主控 ──t1──► 被控      t1/t4 在主控时间轴，t2 在被控时间轴
///   主控 ◄──t2── 被控      rtt = t4 - t1
///   t4 = 主控收到应答       offset = t2 - (t1+t4)/2  满足 S时间 = R时间 + offset
///
/// 被控端 remote/index.vue 的 bindChannelHandlers 会对 VideoChannel 文本帧
/// `h264ClockPing` 回 `h264ClockPong{t1,t2}` —— 主控端只管发探针、收应答。
///
/// 只保留 RTT 最小的 5 个采样（往返越快路径越对称，offset 越可信），
/// 取中位数抗离群。帧延迟 = now - (frameTs - offset)，超过
/// PLAUSIBLE_MAX_MS 视为对齐失效返回 null（不显示假数）。
class ClockSync {
  /// 保留的最优采样数（对齐 web ClockSync 默认 keep=5）。
  static const int _keep = 5;

  /// RTT 超过此值的采样丢弃（通道卡顿会把 offset 带偏）。
  static const int _maxRttMs = 5000;

  /// 帧延迟上界：真链路不会出现 3 秒延迟，出现即对齐失效。
  static const int _plausibleMaxMs = 3000;

  final List<({int rttMs, double offsetMs})> _best = [];
  int total = 0;

  /// 喂一次往返观测（t1=主控发包时刻，t2=被控应答时刻，t4=主控收包时刻）。
  void addSample(int t1, int t2, int t4) {
    final rtt = t4 - t1;
    if (t1 <= 0 || t2 <= 0 || rtt < 0 || rtt > _maxRttMs) return;
    total += 1;
    _best.add((rttMs: rtt, offsetMs: t2 - (t1 + t4) / 2));
    _best.sort((a, b) => a.rttMs.compareTo(b.rttMs));
    if (_best.length > _keep) _best.removeLast();
  }

  /// 钟差中位数（ms）。样本不足 3 个返回 null。
  double? get offsetMs {
    if (_best.length < 3) return null;
    final offs = _best.map((e) => e.offsetMs).toList()..sort();
    final mid = offs.length ~/ 2;
    return offs.length.isOdd ? offs[mid] : (offs[mid - 1] + offs[mid]) / 2;
  }

  /// 有效采样数（对齐用：数据栏可显示对齐进度）。
  int get samples => _best.length;

  /// 把被控时间轴上的帧时间戳折算成端到端延迟（ms）。
  /// 返回 null = 时钟未对齐 / 结果不合理（钟差漂移、时钟被改）。
  int? latencyOf(int remoteTsMs, int nowMs) {
    if (remoteTsMs <= 0) return null;
    final off = offsetMs;
    if (off == null) return null;
    final local = remoteTsMs - off;
    final ms = (nowMs - local).round();
    if (ms < 0 || ms > _plausibleMaxMs) return null;
    return ms;
  }

  void reset() {
    _best.clear();
    total = 0;
  }
}
