# -*- coding: utf-8 -*-
"""
从桌面端品牌图生成网页版 logo 资源。

源必须是 flutter-shell/assets/brand/indesk.png（桌面端侧栏/顶栏/关于页用的同一张），
绝不自己画、绝不从线上旧图取样 —— 两端必须同一张图，否则改完还是"两个产品"。

**教训（2026-10-22）**：build.py 原来把 web/src/assets/img/logo.png 复制成
补丁资源，而那张图是上游的「淡蓝双显示器 + 绿色箭头」旧图标 —— 所以补丁做完
侧栏还是旧图标。图片一律从 brand/indesk.png 出，不碰 src/assets/img/logo.png。
"""
import os
import sys

from PIL import Image

HERE = os.path.dirname(os.path.abspath(__file__))
SRC = r'G:\InDesk\flutter-shell\assets\brand\indesk.png'
OUT_DIR = os.path.normpath(os.path.join(HERE, '..', 'dist', 'assets'))
PX = 256          # 侧栏 28px / 关于页 100px，256 够 2x 清晰


def generate(tag):
    """生成 logo-<tag>.png，返回 (文件名, 字节数)。"""
    if not os.path.exists(SRC):
        raise SystemExit('[!] 品牌源图不存在: %s' % SRC)
    src = Image.open(SRC).convert('RGBA')
    # 贴边裁掉透明边（图本身是圆角方块，四角透明），contain 后不缩水
    bbox = src.split()[3].getbbox()
    if bbox:
        src = src.crop(bbox)
    name = 'logo-%s.png' % tag
    out = os.path.join(OUT_DIR, name)
    src.resize((PX, PX), Image.LANCZOS).save(out, optimize=True)
    return name, os.path.getsize(out)


def main():
    tag = sys.argv[1] if len(sys.argv) > 1 else '20261022d'
    name, size = generate(tag)
    print('源图 %s' % SRC)
    print('  -> %s  %d B' % (name, size))
    return 0


if __name__ == '__main__':
    sys.exit(main())
