/// Flutter LogicalKeyboardKey → nut 键码（被控端 nut_key_to_vk 消费）。
/// 数值来源：@nut-tree-fork/shared Key 枚举（与 web/src/constant.ts NUT_KEY_MAP 同源）。
import 'package:flutter/services.dart';

import '../signal/protocol.dart' show NutKey;

int? logicalToNut(LogicalKeyboardKey k) {
  final kl = k.keyLabel;
  // 字母（keyLabel 单个大写字母）
  if (kl.length == 1) {
    final c = kl.toUpperCase().codeUnitAt(0);
    if (c >= 0x41 && c <= 0x5A) return NutKey.a + (c - 0x41);
    // 数字行
    if (c >= 0x30 && c <= 0x39) {
      const first = <int, int>{
        0x30: NutKey.num0,
        0x31: NutKey.num1,
        0x32: NutKey.num2,
        0x33: NutKey.num3,
        0x34: NutKey.num4,
        0x35: NutKey.num5,
        0x36: NutKey.num6,
        0x37: NutKey.num7,
        0x38: NutKey.num8,
        0x39: NutKey.num9,
      };
      return first[c];
    }
    // 标点（US 105 布局）
    switch (kl) {
      case ';':
        return NutKey.semicolon;
      case "'":
        return NutKey.quote;
      case ',':
        return NutKey.comma;
      case '.':
        return NutKey.period;
      case '/':
        return NutKey.slash;
      case '\\':
        return NutKey.backslash;
      case '[':
        return NutKey.leftBracket;
      case ']':
        return NutKey.rightBracket;
      case '`':
        return NutKey.grave;
      case '-':
        return NutKey.minus;
      case '=':
        return NutKey.equal;
      case ' ':
        return NutKey.space;
    }
  }
  switch (k) {
    case LogicalKeyboardKey.escape:
      return NutKey.escape;
    case LogicalKeyboardKey.enter:
    case LogicalKeyboardKey.numpadEnter:
      return NutKey.enter;
    case LogicalKeyboardKey.tab:
      return NutKey.tab;
    case LogicalKeyboardKey.backspace:
      return NutKey.backspace;
    case LogicalKeyboardKey.delete:
      return NutKey.delete;
    case LogicalKeyboardKey.insert:
      return NutKey.insert;
    case LogicalKeyboardKey.home:
      return NutKey.home;
    case LogicalKeyboardKey.end:
      return NutKey.end;
    case LogicalKeyboardKey.pageUp:
      return NutKey.pageUp;
    case LogicalKeyboardKey.pageDown:
      return NutKey.pageDown;
    case LogicalKeyboardKey.arrowUp:
      return NutKey.up;
    case LogicalKeyboardKey.arrowDown:
      return NutKey.down;
    case LogicalKeyboardKey.arrowLeft:
      return NutKey.left;
    case LogicalKeyboardKey.arrowRight:
      return NutKey.right;
    case LogicalKeyboardKey.space:
      return NutKey.space;
    case LogicalKeyboardKey.capsLock:
      return NutKey.capsLock;
    case LogicalKeyboardKey.numLock:
      return NutKey.numLock;
    case LogicalKeyboardKey.controlLeft:
      return NutKey.leftControl;
    case LogicalKeyboardKey.controlRight:
      return NutKey.rightControl;
    case LogicalKeyboardKey.shiftLeft:
      return NutKey.leftShift;
    case LogicalKeyboardKey.shiftRight:
      return NutKey.rightShift;
    case LogicalKeyboardKey.altLeft:
      return NutKey.leftAlt;
    case LogicalKeyboardKey.altRight:
      return NutKey.rightAlt;
    case LogicalKeyboardKey.metaLeft:
      return NutKey.leftWin;
    case LogicalKeyboardKey.metaRight:
      return NutKey.rightWin;
    case LogicalKeyboardKey.contextMenu:
      return NutKey.menu;
    case LogicalKeyboardKey.f1:
      return NutKey.f1;
    case LogicalKeyboardKey.f2:
      return NutKey.f2;
    case LogicalKeyboardKey.f3:
      return NutKey.f3;
    case LogicalKeyboardKey.f4:
      return NutKey.f4;
    case LogicalKeyboardKey.f5:
      return NutKey.f5;
    case LogicalKeyboardKey.f6:
      return NutKey.f6;
    case LogicalKeyboardKey.f7:
      return NutKey.f7;
    case LogicalKeyboardKey.f8:
      return NutKey.f8;
    case LogicalKeyboardKey.f9:
      return NutKey.f9;
    case LogicalKeyboardKey.f10:
      return NutKey.f10;
    case LogicalKeyboardKey.f11:
      return NutKey.f11;
    case LogicalKeyboardKey.f12:
      return NutKey.f12;
  }
  return null;
}
