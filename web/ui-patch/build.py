"""
ui-patch 构建脚本
----------------------------------------------------------------------------
把 v11 定版补丁 + v12 外观同步合并成一个 css / 一个 js，输出到
web/dist/assets/indesk-ui-patch.{css,js}，供本地预览与线上部署使用。

用法：
    python build.py            # 合并到 web/dist/assets/
    python build.py --deploy   # 额外打印部署用文件名（带批次后缀）

设计原则：
    · v11-base.* 是线上定版原样，**永远不要编辑它**（它是回退基线）。
    · 新增改动一律写在 v1X-*.css / v1X-*.js，按版本号顺序拼接。
    · 批次号只在这一处改：TAG 同时决定补丁资源名与 CSS/JS 里的引用。
"""
import os
import sys

import make_logo

HERE = os.path.dirname(os.path.abspath(__file__))
DIST = os.path.normpath(os.path.join(HERE, '..', 'dist', 'assets'))

# 批次号：改这里 → 新文件名 → 破掉一年的 immutable 缓存
TAG = '20261022g'
LOGO = 'logo-%s.png' % TAG

# 网页版版本号：index.html 的 <meta name="indesk-web-version"> 与服务器
# version.json 的 webVersion **必须完全一致**。不一致会触发定版的"缓存纠错"
# 强制刷新（WebAnnounce 里那段 pageVer !== remote 的逻辑），每次新会话多刷一次。
WEB_VERSION = '20261022.2'

# 客户端下载的真实文件。桌面端版本号现在由 v12-chrome.js 在运行时读
# version.json 的 desktopVersion 覆盖，这里的值只是"接口不通时的兜底"。
DESKTOP_VER = '2.3.49'
DESKTOP_FILE = 'InDesk-%s-x64-setup.exe' % DESKTOP_VER
APK_VER = '1.3.3'
APK_FILE = 'InDesk_%s.apk' % APK_VER

# 这次更新写进公告的内容（会写进 version.json 的 notes，公告按「；」拆行）
RELEASE_NOTES = (
    '桌面端 2.3.49 支持一键自更新：发现新版本后直接在程序里下载安装，装完自动重启，'
    '不用再开浏览器去官网找安装包；'
    '同时修复版本检测一直显示"已是最新"的老问题（原来比的是已退役壳的版本号）；'
    '官网下载链接改为跟随 version.json 实时版本号，不会再出现"页面写新、下载到旧"'
)

CSS_PARTS = ['v11-base.css', 'v12-chrome.css']
JS_PARTS = ['v11-base.js', 'v12-chrome.js']

# 文本里的占位符替换（CSS/JS 里不要硬编码批次）
SUBS = {
    '__LOGO__': LOGO,
    '__TAG__': TAG,
    '__WEBVER__': WEB_VERSION,
    '__DESKTOP_VER__': DESKTOP_VER,
    '__DESKTOP_FILE__': DESKTOP_FILE,
    '__APK_VER__': APK_VER,
    '__APK_FILE__': APK_FILE,
}


def merge(parts, out_name):
    buf = []
    for p in parts:
        path = os.path.join(HERE, p)
        with open(path, 'r', encoding='utf-8') as f:
            buf.append('/* ==== merged: %s ==== */\n' % p)
            buf.append(f.read())
    text = '\n'.join(buf)
    for k, v in SUBS.items():
        text = text.replace(k, v)
    out = os.path.join(DIST, out_name)
    with open(out, 'w', encoding='utf-8', newline='\n') as f:
        f.write(text)
    return out, len(text)


def main():
    if not os.path.isdir(DIST):
        print('[!] 找不到 dist/assets: %s' % DIST)
        return 1

    # logo 永远由桌面端品牌图生成，不从 src/assets/img/logo.png 复制
    name, size = make_logo.generate(TAG)
    print('[ok] %s  %d B  <- flutter-shell/assets/brand/indesk.png' % (name, size))

    for name, size in (merge(CSS_PARTS, 'indesk-ui-patch.css'),
                       merge(JS_PARTS, 'indesk-ui-patch.js')):
        print('[ok] %s  %d chars' % (os.path.basename(name), size))

    if '--deploy' in sys.argv:
        print('\n部署时 index.html 里应换成：')
        print('  <link rel="stylesheet" href="./assets/indesk-ui-patch-%s.css?v=%s"/>' % (TAG, TAG))
        print('  <script src="./assets/indesk-ui-patch-%s.js?v=%s"></script>' % (TAG, TAG))
    return 0


if __name__ == '__main__':
    raise SystemExit(main())
