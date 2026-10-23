/// [20261017] 通用按下反馈组件：按下时容器叠加一层深色/高亮，
/// 松开恢复。解决白底 InkWell splash 不明显、点击"生硬"的问题。
/// 用法：Pressable(onTap: ..., radius: 6, child: ...) 或
///       Pressable.builder(builder: (pressed) => ...) 自定义按下态样式。
import 'package:flutter/material.dart';

class Pressable extends StatefulWidget {
  const Pressable({
    super.key,
    required this.child,
    this.onTap,
    this.radius = 6,
    this.pressedOverlay = const Color(0x14000000), // 按下叠加 8% 黑
    this.behavior = HitTestBehavior.opaque,
    this.builder,
  });

  /// 自定义按下态：pressed=true 时返回对应样式
  const Pressable.builder({
    super.key,
    required this.builder,
    this.onTap,
    this.radius = 6,
    this.behavior = HitTestBehavior.opaque,
  })  : child = null,
        pressedOverlay = null;

  final Widget? child;
  final Widget Function(bool pressed)? builder;
  final VoidCallback? onTap;
  final double radius;
  final Color? pressedOverlay;
  final HitTestBehavior behavior;

  @override
  State<Pressable> createState() => _PressableState();
}

class _PressableState extends State<Pressable> {
  bool _pressed = false;

  void _set(bool v) {
    if (_pressed != v && mounted) setState(() => _pressed = v);
  }

  @override
  Widget build(BuildContext context) {
    final content = widget.builder != null
        ? widget.builder!(_pressed)
        : (widget.pressedOverlay == null
            ? widget.child!
            : AnimatedContainer(
                duration: const Duration(milliseconds: 70),
                decoration: BoxDecoration(
                  borderRadius: BorderRadius.circular(widget.radius),
                  color: _pressed ? widget.pressedOverlay : null,
                ),
                child: widget.child,
              ));
    return GestureDetector(
      behavior: widget.behavior,
      onTapDown: widget.onTap == null ? null : (_) => _set(true),
      onTapUp: widget.onTap == null ? null : (_) => _set(false),
      onTapCancel: widget.onTap == null ? null : () => _set(false),
      onTap: widget.onTap,
      child: content,
    );
  }
}
