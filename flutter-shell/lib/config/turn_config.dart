// TURN 服务器配置
//
// 仓库里不含任何真实凭据：官方发行版在构建时用 --dart-define 注入，
// 你自己编译时也照这个来，把值换成你自己的 TURN 服务：
//
//   flutter build windows --release \
//     --dart-define=TURN_URL=turn:你的域名:3478 \
//     --dart-define=TURN_URL_TCP='turn:你的域名:3478?transport=tcp' \
//     --dart-define=TURN_USERNAME=... \
//     --dart-define=TURN_CREDENTIAL=...
//
// 强烈建议用 coturn 的 use-auth-secret 临时凭证（每次连接前向自己的
// 服务端申请、短时有效），不要把长期静态账号打进客户端——静态账号
// 一旦被从二进制里扒出来，带宽账单就是你的。
class TurnConfig {
  static const String url = String.fromEnvironment('TURN_URL', defaultValue: '');
  static const String urlTcp =
      String.fromEnvironment('TURN_URL_TCP', defaultValue: '');
  static const String username = String.fromEnvironment('TURN_USERNAME', defaultValue: '');
  static const String credential =
      String.fromEnvironment('TURN_CREDENTIAL', defaultValue: '');

  /// 没配置 TURN 时只走 STUN 直连；跨 NAT 严格的网络可能连不上。
  static bool get isConfigured => url.isNotEmpty && credential.isNotEmpty;
}
