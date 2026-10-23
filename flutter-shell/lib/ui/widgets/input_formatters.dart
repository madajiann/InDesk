/// [20261017b] 密码类输入清洗：字母强制小写 + 只保留 ASCII 可打印字符。
/// 解决：没切输入法直接打字上屏汉字（密码含汉字永远错误）。
///
/// ⚠️ 为什么光靠 TextInputFormatter 不够：Windows 中文输入法的组词上屏
/// （composition commit）在部分路径上绕过 inputFormatters（Flutter 引擎
/// 已知行为），实测用户输入仍然出现汉字。所以必须再用
/// [sanitizeController] 给 TextEditingController 挂监听做**双保险**——
/// 监听覆盖一切来源的文本变更（IME 提交、粘贴、格式化器放行的），
/// 是 100% 生效的兜底。
import 'package:flutter/material.dart';
import 'package:flutter/services.dart';

import '../../core/shell_log.dart';

/// [20261017c] 密码框禁用系统输入法（真正的根治）：
/// Windows runner 侧 MethodChannel("indesk/window") → ImmAssociateContext(hwnd, NULL)。
/// 聚焦密码框时挂掉本窗口 IME（打字直接上屏英文小写，**无需切换输入法**），
/// 失焦立即恢复 —— 中文聊天/搜索等其它输入框不受影响。
///
/// 用法：`TextField(focusNode: createNoImeFocusNode())`；
/// dispose 该字段前调用 `disposeNoImeFocusNode(fn)` 恢复 IME。
final MethodChannel _imeChannel = MethodChannel('indesk/window');

FocusNode createNoImeFocusNode() {
  final fn = FocusNode(debugLabel: 'noImeField');
  fn.addListener(() {
    // 有焦点 → 禁 IME（打字直接英文上屏）；失焦 → 恢复系统输入法
    ShellLog.w('[ime] 聚焦=${fn.hasFocus} → ${fn.hasFocus ? "禁用输入法" : "恢复输入法"}');
    _imeChannel
        .invokeMethod<bool>('setImeEnabled', !fn.hasFocus)
        .catchError((e) {
      ShellLog.w('[ime] setImeEnabled 失败: $e');
      return false;
    });
  });
  return fn;
}

void disposeNoImeFocusNode(FocusNode fn) {
  // [20261017d] 铁律：销毁前先把主焦点移走。带着 primaryFocus 直接 dispose
  // 会让 FocusManager 卡在已销毁节点上，之后全局 requestFocus 全部哑火
  // （实测症状：远控画面里点击/拖拽/键盘全部无响应）。
  if (fn.hasFocus) {
    try {
      fn.unfocus();
    } catch (_) {}
    _imeChannel
        .invokeMethod<bool>('setImeEnabled', true)
        .catchError((e) => false);
  }
  fn.dispose();
}

/// 清洗函数：只留 ASCII 可打印（0x21~0x7E），字母小写。
String sanitizePasswordText(String input) {
  final sb = StringBuffer();
  for (final rune in input.runes) {
    if (rune >= 0x21 && rune <= 0x7E) {
      final ch = String.fromCharCode(rune);
      sb.write(ch == ch.toUpperCase() ? ch.toLowerCase() : ch);
    }
  }
  return sb.toString();
}

class LowercaseAsciiFormatter extends TextInputFormatter {
  const LowercaseAsciiFormatter();

  @override
  TextEditingValue formatEditUpdate(
      TextEditingValue oldValue, TextEditingValue newValue) {
    final filtered = sanitizePasswordText(newValue.text);
    if (filtered == newValue.text) return newValue;
    return TextEditingValue(
      text: filtered,
      selection: TextSelection.collapsed(offset: filtered.length),
    );
  }
}

/// 给密码类输入框的 controller 挂上兜底清洗：
/// IME 组词上屏绕过 formatter 时，这里仍会把汉字/大写字母清掉。
/// 返回原 controller 便于链式使用。
TextEditingController sanitizePasswordController(
    TextEditingController ctrl) {
  ctrl.addListener(() {
    final clean = sanitizePasswordText(ctrl.text);
    if (clean == ctrl.text) return;
    final sel = ctrl.selection;
    ctrl.value = TextEditingValue(
      text: clean,
      selection: TextSelection.collapsed(
          offset: sel.baseOffset.clamp(0, clean.length)),
    );
  });
  return ctrl;
}
