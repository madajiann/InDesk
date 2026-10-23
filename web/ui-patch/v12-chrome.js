/* ============================================================================
   InDesk 外观同步贴片  v12-chrome.js  (2026-10-22)
   ----------------------------------------------------------------------------
   只做一件事：往桌面端 .sidebar 顶部注入品牌头
       [logo 方块] InDesk
                   v1.4.2 · 网页版
   对齐桌面端 2.3.47 左上角的「InDesk + 版本」锁标。

   三条纪律：
     ① **只注入 <div class="ink-brand"> 一个节点**，不碰定版任何节点、不写 style。
     ② **只在桌面注入**（html.indesk-pc）。手机端 .sidebar 是底部导航栏，
        .item 全部 flex:1 均分 —— 往它里面塞节点会把导航格挤歪
        （v11 踩过：退出登录被 append 进 .list 后独占最右一格，见 v11-base.css）。
     ③ 幂等 + 可自愈：Vue 重新渲染把节点冲掉时，下一轮轮询会补回来；
        已有则直接跳过，绝不重复插入。
   ========================================================================== */
(function () {
  'use strict';

  // 版本号里带上贴片批次（20261022b）：用户能在界面上直接确认自己拿到的是哪一版，
  // 不用再靠"看着像不像"来争论。
  // 版本号取真实网页版本（与 index.html 的 indesk-web-version、version.json 的
  // webVersion 同一个值）。**不要**再写死了几年的壳版本 —— 用户拿它核对"装没装新版"，
  // 写成 v1.4.2 只会让人以为网页还是旧版。
  var VER = '网页版 __WEBVER__';
  var MARK_CLS = 'ink-brand';

  /**
   * 客户端下载链接纠偏（2026-10-22 补）
   * ------------------------------------------------------------------------
   * 定版 `layout/index.vue` 的「客户端下载」弹窗里硬编码了两条**退役地址**：
   *   /updates/indesk-desktop.exe → 旧 Tauri 桌面端（1.4.x），不是现在的 2.3.x 安装包
   *   /updates/InDesk.apk         → 旧 10MB 胖包（1.3.0），不是 1.3.3 的 1MB 瘦包
   * 用户从官网下去就是装了个退役程序。这里在点击与渲染两处同时纠正：
   *   · 捕获阶段拦点击（弹窗刚出来、700ms 轮询还没跑到时也生效）
   *   · 轮询里顺手改写 href 与文字，让用户看到的字和实际下的文件一致
   * 值全部来自 build.py 的单点常量，不在这里硬编码。
   */
  var DL = {
    '/updates/indesk-desktop.exe': {
      href: '/updates/__DESKTOP_FILE__',
      text: '⬇ Windows 电脑端（InDesk v__DESKTOP_VER__ 安装包）'
    },
    '/updates/InDesk.apk': {
      href: '/updates/__APK_FILE__',
      text: '⬇ 安卓手机端（InDesk __APK_VER__）'
    }
  };

  function dlKey(a) {
    var h = a.getAttribute('href') || '';
    try {
      h = h.replace(location.origin, '').replace(location.host, '');
    } catch (e) {
      /* ignore */
    }
    return h.split('?')[0];
  }

  function syncDownloads() {
    var as = document.querySelectorAll('a[href*="/updates/"]');
    for (var i = 0; i < as.length; i++) {
      var a = as[i];
      var d = DL[dlKey(a)];
      if (!d) continue;
      if (a.getAttribute('href') !== d.href) {
        a.setAttribute('href', d.href);
        a.removeAttribute('download'); // 沿用定版的文件名会与实际文件不符
        a.textContent = d.text;
      }
    }
  }

  function isPc() {
    return document.documentElement.classList.contains('indesk-pc');
  }

  function build() {
    var box = document.createElement('div');
    box.className = MARK_CLS;

    var mark = document.createElement('i');
    mark.className = 'ink-brand-mark';

    var tx = document.createElement('div');
    tx.className = 'ink-brand-tx';

    var name = document.createElement('b');
    name.textContent = 'InDesk';

    var sub = document.createElement('span');
    sub.textContent = VER;

    tx.appendChild(name);
    tx.appendChild(sub);
    box.appendChild(mark);
    box.appendChild(tx);
    return box;
  }

  /**
   * 更新公告入口（2026-10-22 补）
   * ------------------------------------------------------------------------
   * 定版的公告是**进页面整屏弹**，写着过期的 20261011.16，用户以为界面没更新。
   * 现在：版本号取服务器 version.json 真实值，弹窗改成右下角浮卡（见 CSS 12.13），
   * 侧栏底部再给一个「更新公告」入口，随时能翻，有未读才显蓝点。
   *
   * 复用定版组件的 `window.__indeskAnnounce(ver, notes)`（它内部负责"看过了"的
   * 记账），不自己实现一份，避免两处状态打架。
   */
  var NOTICE_CLS = 'ink-notice';
  var REMOTE = null;   // { ver, notes, deskVer, deskFile }
  var tries = 0;

  function loadRemote(then) {
    if (REMOTE) {
      if (then) then();
      return;
    }
    fetch(location.origin + '/version.json?t=' + Date.now(), { cache: 'no-store' })
      .then(function (r) { return r.json(); })
      .then(function (j) {
        REMOTE = {
          ver: String((j && (j.webVersion || j.version)) || ''),
          notes: String((j && j.notes) || ''),
          deskVer: String((j && j.desktopVersion) || ''),
          deskFile: String((j && j.desktopFile) || '')
        };
        // 桌面端版本号以 version.json 为准。以前这里的数字是 build.py 写死的，
        // 每发一个桌面版就得重发一次网页批次；改成运行时取值后，以后只更新
        // version.json 就行，网页不会再出现"下载到上一个版本"的错位。
        var w = DL['/updates/indesk-desktop.exe'];
        if (w && REMOTE.deskVer) {
          if (REMOTE.deskFile) w.href = '/updates/' + REMOTE.deskFile;
          w.text = '⬇ Windows 电脑端（InDesk v' + REMOTE.deskVer + ' 安装包）';
        }
        if (then) then();
      })
      .catch(function () {
        /* 离线/接口不通就静默，绝不弹错误框 */
      });
  }

  function seenVer() {
    try {
      return localStorage.getItem('indesk_web_version') || '';
    } catch (e) {
      return '';
    }
  }

  function openNotice() {
    var fn = window.__indeskAnnounce;
    if (typeof fn !== 'function' || !REMOTE || !REMOTE.ver) {
      // 组件还没挂载 / 版本还没拉到：最多等 ~3.6s，之后放弃（不无限递归）
      if (tries++ > 12) {
        tries = 0;
        return;
      }
      loadRemote();
      setTimeout(openNotice, 300);
      return;
    }
    tries = 0;
    fn(REMOTE.ver, REMOTE.notes);
  }

  function syncNotice(side) {
    var foot = side.querySelector('.side-foot');
    if (!foot) return;
    var el = foot.querySelector(':scope > .' + NOTICE_CLS);
    if (!el) {
      el = document.createElement('div');
      el.className = NOTICE_CLS;
      var tx = document.createElement('span');
      tx.textContent = '更新公告';
      var dot = document.createElement('i');
      dot.className = 'ink-notice-dot';
      el.appendChild(tx);
      el.appendChild(dot);
      el.addEventListener('click', openNotice);
      foot.appendChild(el); // 跟「账号登录」同区，不挤导航
    }
    var unread = !!(REMOTE && REMOTE.ver && seenVer() !== REMOTE.ver);
    if (unread) el.classList.add('has-dot');
    else el.classList.remove('has-dot');
  }

  function tick() {
    try {
      // 下载链接纠偏要在**任何端**都跑：手机端也有「客户端下载」弹窗
      syncDownloads();
      if (!isPc()) return;
      var side = document.querySelector('.layout > .sidebar');
      if (!side) return;
      if (!side.querySelector(':scope > .' + MARK_CLS)) {
        side.insertBefore(build(), side.firstChild);
      }
      syncAvatar(side);
      syncNotice(side);
    } catch (e) {
      /* 全部吞掉：贴片异常绝不影响定版 */
    }
  }

  /**
   * 头像与桌面端一致：已登录 = 品牌蓝实底 + 昵称首字（白字）；
   * 未登录 = 人形线稿（CSS 里的 data URI），不伪造字母。
   * 只写 data-av 属性，渲染全部交给 CSS，不碰定版的内联 style。
   */
  function syncAvatar(side) {
    var user = side.querySelector('.username');
    var av = side.querySelector('.user');
    if (!user || !av) return;
    var t = (user.textContent || '').trim();
    var ch = '';
    if (t && t !== '未登录' && t !== '登录' && t !== '账号登录') {
      ch = Array.from(t)[0]; // 用 Array.from 取首字，避免把 emoji 劈成半个代理对
    }
    if (ch) {
      if (av.getAttribute('data-av') !== ch) av.setAttribute('data-av', ch);
    } else if (av.hasAttribute('data-av')) {
      av.removeAttribute('data-av');
    }
  }

  // 先拉一次版本：侧栏入口的"未读蓝点"要靠它，点开公告也靠它
  loadRemote();

  // 捕获阶段兜底：弹窗刚插进 DOM、还没轮到 700ms 轮询时点的也算得住
  document.addEventListener('click', function (e) {
    try {
      var a = e.target && e.target.closest ? e.target.closest('a[href*="/updates/"]') : null;
      if (!a) return;
      var d = DL[dlKey(a)];
      if (!d) return;
      e.preventDefault();
      e.stopPropagation();
      location.href = d.href;
    } catch (err) {
      /* 不干扰定版 */
    }
  }, true);

  if (document.readyState === 'loading') {
    document.addEventListener('DOMContentLoaded', tick);
  } else {
    tick();
  }
  setInterval(tick, 700);
})();
