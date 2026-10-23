/// InDesk 信令协议常量 —— 与 web/src/types/websocket.ts 逐字段对齐。
///
/// ⚠️ 铁律（用户 2026-10-15 明确）：
/// 1. WsMsgTypeEnum 的字符串值、DeskBehaviorEnum 的**数值**，两端必须完全一致。
///    DeskBehaviorEnum 是数字枚举、走 WS 原样传输，**只能追加，绝不可插入中间**，
///    否则老版本被控端会把"隐私屏"当成"滚轮下滚"之类的错乱指令。
/// 2. 键码 = @nut-tree-fork/shared 的 Key 枚举（被控端壳侧 nut_key_to_vk 消费）。
library;

/// WsMsgTypeEnum（字符串值与 web 完全一致，只列远控用到的子集）
class WsMsg {
  static const deskJoin = 'deskJoin';
  static const deskJoined = 'deskJoined';
  static const deskUpdateUser = 'deskUpdateUser';
  static const deskStartRemote = 'deskStartRemote';
  static const deskStartRemoteResult = 'deskStartRemoteResult';
  static const deskBehavior = 'deskBehavior';
  static const deskCloseRemote = 'deskCloseRemote';
  static const nativeWebRtcOffer = 'nativeWebRtcOffer';
  static const nativeWebRtcAnswer = 'nativeWebRtcAnswer';
  static const nativeWebRtcCandidate = 'nativeWebRtcCandidate';
  static const heartbeat = 'heartbeat';

  /// ---- 画质档位（对齐上游 remote/index.vue 与服务端 WsMsgTypeEnum）----
  /// 三类消息 data 均为 {live_room_id, val}；被控端 web 收到后走
  /// changeMaxBitrate→setMaxBitrate / changeMaxFramerate→handlConstraints /
  /// changeResolutionRatio→记档位（静默取屏档位联动 core 参数）。
  static const changeMaxBitrate = 'changeMaxBitrate';
  static const changeMaxFramerate = 'changeMaxFramerate';
  static const changeResolutionRatio = 'changeResolutionRatio';
  static const smartFpsState = 'smartFpsState';
}

/// DeskBehaviorEnum（数值与 web 完全一致；追加在末尾，禁止插入）
class DeskBehavior {
  static const mouseMove = 0;
  static const mouseDrag = 1;
  static const pressButtonLeft = 2;
  static const pressButtonRight = 3;
  static const releaseButtonLeft = 4;
  static const releaseButtonRight = 5;
  static const setPosition = 6;
  static const doubleClick = 7;
  static const leftClick = 8;
  static const rightClick = 9;
  static const scrollDown = 10;
  static const scrollUp = 11;
  static const scrollLeft = 12;
  static const scrollRight = 13;
  static const keyboardType = 14;
  static const keyboardPressKey = 15;
  static const keyboardReleaseKey = 16;
  static const switchIme = 17;
  static const releaseAllModifiers = 18;
  static const privacyScreenOn = 19;
  static const privacyScreenOff = 20;
  static const textChat = 21;
  static const sendCtrlAltDel = 22;
  static const openTaskManager = 23;
  static const lockScreen = 24;
}

/// @nut-tree-fork/shared Key 枚举（数值与被控端 nut_key_to_vk 对齐）
class NutKey {
  static const escape = 0;
  static const f1 = 1;
  static const f2 = 2;
  static const f3 = 3;
  static const f4 = 4;
  static const f5 = 5;
  static const f6 = 6;
  static const f7 = 7;
  static const f8 = 8;
  static const f9 = 9;
  static const f10 = 10;
  static const f11 = 11;
  static const f12 = 12;
  static const print = 25;
  static const scrollLock = 26;
  static const pause = 27;
  static const grave = 28;
  static const num1 = 29;
  static const num2 = 30;
  static const num3 = 31;
  static const num4 = 32;
  static const num5 = 33;
  static const num6 = 34;
  static const num7 = 35;
  static const num8 = 36;
  static const num9 = 37;
  static const num0 = 38;
  static const minus = 39;
  static const equal = 40;
  static const backspace = 41;
  static const insert = 42;
  static const home = 43;
  static const pageUp = 44;
  static const numLock = 45;
  static const divide = 46;
  static const multiply = 47;
  static const subtract = 48;
  static const tab = 49;
  static const q = 50;
  static const w = 51;
  static const e = 52;
  static const r = 53;
  static const t = 54;
  static const y = 55;
  static const u = 56;
  static const i = 57;
  static const o = 58;
  static const p = 59;
  static const leftBracket = 60;
  static const rightBracket = 61;
  static const backslash = 62;
  static const delete = 63;
  static const end = 64;
  static const pageDown = 65;
  static const numPad7 = 66;
  static const numPad8 = 67;
  static const numPad9 = 68;
  static const add = 69;
  static const capsLock = 70;
  static const a = 71;
  static const s = 72;
  static const d = 73;
  static const f = 74;
  static const g = 75;
  static const h = 76;
  static const j = 77;
  static const k = 78;
  static const l = 79;
  static const semicolon = 80;
  static const quote = 81;
  static const ret = 82;
  static const numPad4 = 83;
  static const numPad5 = 84;
  static const numPad6 = 85;
  static const leftShift = 86;
  static const z = 87;
  static const x = 88;
  static const c = 89;
  static const v = 90;
  static const b = 91;
  static const n = 92;
  static const m = 93;
  static const comma = 94;
  static const period = 95;
  static const slash = 96;
  static const rightShift = 97;
  static const up = 98;
  static const numPad1 = 99;
  static const numPad2 = 100;
  static const numPad3 = 101;
  static const enter = 102;
  static const leftControl = 103;
  static const leftSuper = 104;
  static const leftWin = 105;
  static const leftCmd = 106;
  static const leftAlt = 107;
  static const space = 108;
  static const rightAlt = 109;
  static const rightSuper = 110;
  static const rightWin = 111;
  static const rightCmd = 112;
  static const menu = 113;
  static const rightControl = 114;
  static const fn = 115;
  static const left = 116;
  static const down = 117;
  static const right = 118;
  static const numPad0 = 119;
  static const decimal = 120;
  static const clear = 121;
}

/// 信令 ws 地址（web 的 WEBSOCKET_URL 生产值；本地开发可被 --dart-define 覆盖）
const kSignalingUrl = String.fromEnvironment(
  'INDESK_SIGNALING_URL',
  defaultValue: 'wss://desk.yomm.top:8443',
);

/// 随机 8 位 request_id（对齐 getRandomString(8)）
String randomRequestId() {
  const chars = 'ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789';
  final rnd = DateTime.now().microsecondsSinceEpoch;
  var seed = rnd;
  return String.fromCharCodes(List.generate(8, (_) {
    seed = (seed * 1103515245 + 12345) & 0x7fffffff;
    return chars.codeUnitAt(seed % chars.length);
  }));
}

/// 信令信封（IReqWsFormat<T>）
/// {request_id, socket_id, user_info, user_token, time, data}
Map<String, dynamic> envelope({
  required String? socketId,
  required Map<String, dynamic> data,
}) {
  return <String, dynamic>{
    'request_id': randomRequestId(),
    'socket_id': socketId ?? '',
    'user_info': <String, dynamic>{},
    'user_token': '',
    'time': DateTime.now().millisecondsSinceEpoch,
    'data': data,
  };
}
