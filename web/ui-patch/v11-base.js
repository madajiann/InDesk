/* ============================================================================
   InDesk 界面贴片  ui-patch.js  v11  (2026-09-24)
   ----------------------------------------------------------------------------
   产物层补丁：不修改 20260921.18 定版的任何文件；删掉 index.html 里的引用即回退。
   本脚本做六件事（全部只加 class / CSS 变量 / 外层包裹，不写 .idsb 与 .drag 的 style）：
     A. 判断"哪一端"并给 <html> 挂标记（indesk-mobile / indesk-pc）
     B. 判断"是否全屏"并给 <html> 挂 .indesk-fs
     C. 【数据栏 .idsb 自适应（v6 重写）】
          · 全屏且画面两侧留白 ≥ 72px → 挂 .indesk-sbfit，下发
            --idsb-w / --idsb-right / --idsb-k
          · 【v6 关键】卡片**左边缘固定吸附在「画面右边缘 + 6px」**上
            （--idsb-right = 留白 − 6 − 卡宽）
            ⇒ 大留白时卡片贴着画面、右侧留空，不再"吸附窗口最右侧"
            ⇒ 小留白时卡片填满留白，绝不越出到画面上
          · .47 的阈值是 176px：留白 72~175px（16:10 / 4:3 被控端全屏时很常见）
            整条退回**横排**、横着跨在"画面 + 黑边"上 —— 用户报的
            "横显数据栏直接超出屏幕到空白区域"就是这个口径。现在降到 72px。
     D. 【画面恢复浮层去黑幕（v6 新增）】
          定版自身逻辑：距上次真实出帧 >3000ms 就弹 #indesk-recovery-overlay，
          整屏 rgba(13,16,23,.96) 不透明黑 + 22px 居中大字「画面恢复中…」。
          弱网（手机流量/丢包）帧间隔动不动就 >3s ⇒ 反复黑屏，'像断线'。
          本脚本**不改定版的显隐开关（内联 display）**，只把浮层改成
          「极轻遮罩 + 底部一行小字」，让**最后一帧画面始终可见**。
     E. 【弱网自动降档（v6 新增）】
          90 秒内恢复浮层出现 ≥3 次 ⇒ 判定持续弱网 ⇒ 自动把「画质档位」
          切到「流畅」（540P / 30fps / 2Mbps，带宽需求砍到 1/4），并弹提示。
          只在跨过阈值时执行一次；用户可在「连接详情」里随时改回。
     F. 【手机端「连接详情」面板重设计（v6 新增，用户第 4 条）】
          定版手机端直接把**电脑端整套设置**塞进面板：操作(模式/鼠标/虚拟鼠标)
          + 画质(档位/码率/帧率/分辨率/视频内容/音频内容) + 剪贴板 + 关闭连接，
          面积大、且**没有任何「收起面板」的控件**（唯一的按钮是"关闭连接"＝断线）。
          本脚本在手机端：① 顶部注入「连接详情 + ✕」栏（✕ 派发点击给 .logo-btn
          收起面板，因为面板显隐由 Vue 的 we 变量控制，只能走它自己的开关）；
          ② 隐藏手机用不到的 7 项（鼠标 / 虚拟鼠标 / 码率 / 帧率 / 分辨率 /
          视频内容 / 音频内容），只留 模式 / 画质档位 / 剪贴板 / 关闭连接。
     G. 造整屏连接动画（#indesk-conn），出帧后淡出；异常全吞，绝不影响定版逻辑。

   v11 修订（用户 2026-09-24 追加的三条）：
     H. 【退出登录改位置】v8 把「退出账号」append 进 .sidebar .list，而手机端
        ≤820px 时 .sidebar 是底部导航栏、.item 全部 flex:1 均分 ⇒ 追加的第 6 项
        独占最右一格 + 高饱和红 = 用户说的"挤在底部导航栏右上角的小红块"。
        v11 **撤回**该注入，改为注入「账号信息」弹窗 .acc-body 底部。
        弹窗入口无需另造：定版 .list 第 5 项在已登录时就显示"账号信息"。
        确认改用**两次点击**（4 秒内再点一次才真退），不再依赖 window.confirm。
     I. 【设备列表页增强】定版该页只渲染一行裸 UUID + 一个细线删除叉。
        v11 在该页注入 .ink-dev-info（设备名/在线徽章/设备码/IP/最后连接），
        数据来源：
          · 在线状态 + 设备名 + IP → GET /desk_user/find_receiver_by_uuid?uuid=
            （定版 remote 页 refreshOnlineState() 用的同一个接口；
              code===200 && data.receiver 为真 ⇒ 在线；回填 data.name / data.ip）
          · 设备码 → 定版同款规则（9 位数字显示 123-456-789）
          · 最后连接时间 → **定版本地缓存没有这个字段**，故由本脚本自建台账
            localStorage['indesk___link-log']，只在**真正连上并出帧**时打点；
            首装台账为空 ⇒ 显示「—」，绝不编数据。
     J. 【高级设置页增强】按块名给 .label 注入定版同款线性图标（stroke 1.6）。
   注：任何异常都被吞掉。
   ⚠️ 只写 <html> 的 class 与 CSS 变量 —— .18 每 500ms 会用 el.style.left/top
      重写 .idsb / .drag 的内联定位，往它们身上写 style 会被抹掉。
   ⚠️ **绝不碰 #indesk-recovery-overlay 与 .info 的 display**：前者是定版的
      显隐开关（内联），后者靠 .info.show 类。碰了就会"常显不消失"。
   ============================================================================ */
(function () {
  'use strict';

  /* ========================= A. 三端标记 ========================= */
  try {
    var ua = navigator.userAgent || '';
    var isIPad = /iPad/i.test(ua) || (/Macintosh/i.test(ua) && navigator.maxTouchPoints > 1);
    var isMobile = /Android|iPhone|iPod|Windows Phone|HarmonyOS|Mobile/i.test(ua) && !isIPad;
    /* 三端互斥标记：
         · 手机端    → html.indesk-mobile（CSS 里只配合 @media(max-width:820px) 用）
         · 电脑端/网页版 → html.indesk-pc
       手机端**不**挂 indesk-pc ⇒ 数据栏规则永远不会作用到手机上。 */
    if (isMobile) document.documentElement.classList.add('indesk-mobile');
    else document.documentElement.classList.add('indesk-pc');
  } catch (e) { /* 忽略 */ }

  /* ==================== B. 全屏标记 ==================== */
  function fsLike() {
    try {
      /* 信号 1：标准全屏 API（PC 工具栏「开启全屏」= documentElement.requestFullscreen） */
      if (document.fullscreenElement || document.webkitFullscreenElement) return true;
      var s = window.screen || {};
      var sw = +s.width || 0, sh = +s.height || 0;                 // 整屏
      var aw = +s.availWidth || sw, ah = +s.availHeight || sh;     // 去掉任务栏
      var iw = window.innerWidth || 0, ih = window.innerHeight || 0;
      var ow = window.outerWidth || 0, oh = window.outerHeight || 0;
      if (!iw || !ih) return false;
      /* 宽度铺满：外框贴住可用宽度，或视口贴住屏幕宽度 */
      var wide = (ow && aw && ow >= aw - 2) || (sw && iw >= sw - 2) || (aw && iw >= aw - 2);
      /* 高度铺满：真全屏(=screen.height) / 最大化(=availHeight) /
         容错 60px 标题栏·任务栏 / 屏高 90% 兜底 —— 覆盖「窗口最大化」也算全屏 */
      var tall = (oh && ah && oh >= ah - 2)
              || (sh && ih >= sh - 2)
              || (ah && ih >= ah - 60)
              || (sh && ih >= sh * 0.90);
      return !!(wide && tall);
    } catch (e) { return false; }
  }

  /* 数据栏 .idsb：全屏时套用 .18 自带的竖列样式（data-o="v"），非全屏交回原样。
     ⚠️ 只动 data-o 属性，**绝不写 .idsb 的 style** —— 见文件头说明。 */
  function applySb(fs) {
    try {
      var el = document.querySelector('.idsb');
      if (!el) return;
      if (fs) {
        if (el.getAttribute('data-o') !== 'v') el.setAttribute('data-o', 'v');
      } else if (el.hasAttribute('data-o')) {
        el.removeAttribute('data-o');
      }
    } catch (e) { /* 忽略 */ }
  }

  /* ============ C. 实测画面右侧留白 → 数据栏竖列卡的自适应尺寸 ============
     远程画面被 .18 以 contain 方式居中显示（.webrtc-wrap 是 flex center）。
     画面的真实显示宽度：
        · 有 <video> 且 videoWidth/Height 有效 → contain 换算 min(容器宽, 容器高*比例)
        · <video> 元素本身已被裁成画面尺寸（比容器窄）→ 用它的 rect（更准）
     右侧留白 = (视口宽 - 画面宽) / 2  （居中布局 ⇒ 左右留白相等）

     【v6 定位口径】卡片**左边缘固定贴在「画面右边缘 + GAP_PIC」**上：
         --idsb-right = 留白 − GAP_PIC − 卡宽
       ⇒ 大留白（>246px）时卡宽封顶 240，右距随之变大 → 卡片紧贴画面、右侧留空，
         这就是用户要的"吸附到画面右边框上，不要吸附在窗口最右侧"；
       ⇒ 小留白时卡宽 = 留白 − 12 → 右距恒为 6px，卡片整块待在留白里，绝不压画面。
     阈值 MIN_BLANK = 72px（.47 是 176，导致 72~175px 的常见场景整条退回横排）。 */
  var MIN_BLANK = 72;    // 留白低于此值 → 退回右上角横排（真·铺满，横排也就占画面角）
  var MAX_CARD = 240;    // 竖列卡宽度上限
  var GAP_PIC = 6;       // 卡片左边缘与画面右边缘的固定间隙
  var MIN_CARD = 60;     // 竖列卡宽度下限

  function sbMetrics() {
    var box = document.querySelector('.remote-video');
    if (!box) return null;
    var br = box.getBoundingClientRect();
    if (!br.width || !br.height) return null;
    var picW = br.width;
    var v = box.querySelector('video');
    if (v) {
      var vw = +v.videoWidth || 0, vh = +v.videoHeight || 0;
      if (vw > 0 && vh > 0) picW = Math.min(br.width, br.height * vw / vh);
      var vr = v.getBoundingClientRect();
      if (vr.width > 0 && vr.width < br.width - 2) picW = Math.max(picW, Math.min(br.width, vr.width));
    }
    var blank = (window.innerWidth - picW) / 2;
    if (!(blank > 0)) blank = 0;
    return { blank: blank, picW: picW };
  }

  function clearSbfit(html) {
    if (html.classList.contains('indesk-sbfit')) html.classList.remove('indesk-sbfit');
    html.style.removeProperty('--idsb-w');
    html.style.removeProperty('--idsb-right');
    html.style.removeProperty('--idsb-k');
  }

  function layoutSb() {
    try {
      var html = document.documentElement;
      if (!html.classList.contains('indesk-pc')) return;   // 手机端不参与
      if (!html.classList.contains('indesk-fs')) { clearSbfit(html); return; }
      var m = sbMetrics();
      if (!m) { clearSbfit(html); return; }
      var blank = m.blank;
      if (blank >= MIN_BLANK) {
        var w = Math.round(Math.max(MIN_CARD, Math.min(MAX_CARD, blank - 12)));
        /* 卡片左边缘 = 画面右边缘 + GAP_PIC（恒等式，见文件头 C 段说明） */
        var right = Math.round(Math.max(4, blank - GAP_PIC - w));
        var k = 0.70 + 0.30 * Math.max(0, Math.min(1, (w - 64) / 120));
        html.classList.add('indesk-sbfit');
        html.style.setProperty('--idsb-w', w + 'px');
        html.style.setProperty('--idsb-right', right + 'px');
        html.style.setProperty('--idsb-k', k.toFixed(3));
      } else {
        /* 留白不够（本地 16:9 看远程 16:9，满屏无黑边）
           → 摘掉 sbfit，CSS 退回右上角横排，绝不压住画面 */
        clearSbfit(html);
      }
    } catch (e) { /* 忽略 */ }
  }

  /* ============ C-2. 手机端：面板避开数据栏所在的一侧 ============
     数据栏中心在左半屏 → 面板靠右；否则（含数据栏不存在）面板靠左
     （手机端 LOGO 按钮固定在右上角，默认靠左也最不容易互相压）。 */
  function layoutPanel() {
    try {
      var html = document.documentElement;
      if (!html.classList.contains('indesk-mobile')) return;
      var vw = window.innerWidth || 0;
      if (!vw) return;
      var barLeft = false;
      var sb = document.querySelector('.idsb');
      if (sb) {
        var r = sb.getBoundingClientRect();
        if (r.width > 0 && r.height > 0) barLeft = (r.left + r.width / 2) < vw * 0.5;
      }
      if (barLeft) {
        html.style.setProperty('--idsp-left', 'auto');
        html.style.setProperty('--idsp-right', '10px');
      } else {
        html.style.setProperty('--idsp-left', '10px');
        html.style.setProperty('--idsp-right', 'auto');
      }
    } catch (e) { /* 忽略 */ }
  }

  /* ========================= E. 轻提示 toast ========================= */
  var toastTimer = null;
  function toast(msg) {
    try {
      var old = document.getElementById('indesk-toast');
      if (old && old.parentNode) old.parentNode.removeChild(old);
      if (toastTimer) { clearTimeout(toastTimer); toastTimer = null; }
      var t = document.createElement('div');
      t.id = 'indesk-toast';
      t.textContent = msg;
      (document.body || document.documentElement).appendChild(t);
      setTimeout(function () { t.classList.add('indesk-toast-in'); }, 20);
      toastTimer = setTimeout(function () {
        t.classList.remove('indesk-toast-in');
        setTimeout(function () { if (t.parentNode) t.parentNode.removeChild(t); }, 420);
      }, 6500);
    } catch (e) { /* 忽略 */ }
  }

  /* ==================== E. 弱网自动降档到「流畅」 ====================
     档位按钮在定版 .link-config 里（n-radio-group，顺序 = QUALITY_TIERS：
       流畅 / 均衡 / 高清）。DOM 常驻（.info 只切 .show 类，不销毁子节点），
     所以面板**关着也能点到**。
     ⇒ 找到「画质档位」那一行，点它的**第 1 个** radio-button（＝流畅）。 */
  var netBadTimes = [];
  var didAutoDowngrade = false;
  var lastOvShowing = false;

  function findTierRow() {
    var rows = document.querySelectorAll('.link-config .lc-row');
    for (var i = 0; i < rows.length; i++) {
      var lb = rows[i].querySelector('.lc-label');
      if (lb && (lb.textContent || '').replace(/\s/g, '') === '画质档位') return rows[i];
    }
    return null;
  }

  function applySmoothTier() {
    try {
      var row = findTierRow();
      if (!row) return false;
      /* naive-ui 的 n-radio-button 内部是 <input type="radio">，点它最稳；
         再加容器点击兜底（重复点击同值不会二次触发 change）。 */
      var inp = row.querySelectorAll('input[type="radio"]');
      if (inp && inp.length) { inp[0].click(); return true; }
      var btns = row.querySelectorAll('.n-radio-button');
      if (btns && btns.length) { btns[0].click(); return true; }
      var labels = row.querySelectorAll('label');
      if (labels && labels.length) { labels[0].click(); return true; }
    } catch (e) { /* 忽略 */ }
    return false;
  }

  function weakNetGuard() {
    try {
      var ov = document.getElementById('indesk-recovery-overlay');
      var showing = !!(ov && ov.style && ov.style.display && ov.style.display !== 'none');
      if (showing === lastOvShowing) return;
      lastOvShowing = showing;
      if (!showing) return;
      /* 恢复浮层刚出现 ⇒ 记一次"画面掉帧"事件 */
      var now = Date.now();
      netBadTimes.push(now);
      while (netBadTimes.length && now - netBadTimes[0] > 90000) netBadTimes.shift();
      if (!didAutoDowngrade && netBadTimes.length >= 3) {
        didAutoDowngrade = true;
        var ok = applySmoothTier();
        toast(ok
          ? '网络较弱，已自动切换到「流畅」画质（540P·2Mbps）以保持画面连贯；可在「连接详情」里改回'
          : '网络较弱，建议在「连接详情」里把画质切到「流畅」以减少卡顿');
      }
    } catch (e) { /* 忽略 */ }
  }

  /* ============ F. 手机端「连接详情」面板重设计 ============
     · 隐藏手机用不到的项（按 .lc-label 文本精确匹配，不靠 nth-child 猜顺序）
     · 注入顶部「连接详情 + ✕」栏；✕ 派发点击给 .logo-btn 收起面板
       （面板显隐 = Vue 的 we 变量 → class .info.show，只能走它自己的开关） */
  var HIDE_LABELS = { '鼠标': 1, '虚拟鼠标': 1, '码率': 1, '帧率': 1, '分辨率': 1,
                      '视频内容': 1, '音频内容': 1 };

  function tidyMobilePanel() {
    try {
      var html = document.documentElement;
      if (!html.classList.contains('indesk-mobile')) return;
      var cfg = document.querySelector('.webrtc-wrap .drag .info .link-config');
      if (!cfg) return;
      var rows = cfg.querySelectorAll('.lc-row');
      for (var i = 0; i < rows.length; i++) {
        var lb = rows[i].querySelector('.lc-label');
        var txt = lb ? (lb.textContent || '').replace(/\s/g, '') : '';
        if (HIDE_LABELS[txt]) {
          if (rows[i].getAttribute('data-ink-hide') !== '1') rows[i].setAttribute('data-ink-hide', '1');
        }
      }
      if (!cfg.querySelector('.ink-mbar')) {
        var bar = document.createElement('div');
        bar.className = 'ink-mbar';
        var tt = document.createElement('span');
        tt.className = 'ink-mtitle';
        tt.textContent = '连接详情';
        var x = document.createElement('button');
        x.type = 'button';
        x.className = 'ink-mclose';
        x.setAttribute('aria-label', '收起面板');
        x.textContent = '✕';
        x.addEventListener('click', function (ev) {
          try {
            if (ev) { ev.preventDefault(); ev.stopPropagation(); }
            var logo = document.querySelector('.webrtc-wrap .drag .logo-btn');
            if (logo && logo.click) logo.click();
          } catch (e) { /* 忽略 */ }
        });
        bar.appendChild(tt);
        bar.appendChild(x);
        cfg.insertBefore(bar, cfg.firstChild);
      }
    } catch (e) { /* 忽略 */ }
  }

  function syncAll() {
    try {
      var el = document.documentElement, on = fsLike();
      if (el.classList.contains('indesk-fs') !== on) {
        if (on) el.classList.add('indesk-fs'); else el.classList.remove('indesk-fs');
      }
      if (el.classList.contains('indesk-pc')) {
        try { applySb(on); } catch (e) { /* 忽略 */ }
      }
    } catch (e) { /* 忽略 */ }
    try { layoutSb(); } catch (e) { /* 忽略 */ }
    try { layoutPanel(); } catch (e) { /* 忽略 */ }
    try { tidyMobilePanel(); } catch (e) { /* 忽略 */ }
    try { weakNetGuard(); } catch (e) { /* 忽略 */ }
  }
  try {
    setInterval(syncAll, 400);
    window.addEventListener('resize', syncAll);
    window.addEventListener('orientationchange', syncAll);
    document.addEventListener('fullscreenchange', syncAll);
    document.addEventListener('webkitfullscreenchange', syncAll);
    syncAll();
  } catch (e) { /* 忽略 */ }

  /* ========================= G. 整屏连接动画 ========================= */
  var PHRASES = ['正在建立加密信道', '正在协商画质参数', '即将进入远程桌面'];
  var MAXP = 92;          // 未出帧前最多爬到 92%
  var MIN_SHOW_MS = 500;  // 至少显示这么久，避免"闪一下"
  var HARD_TIMEOUT = 25000; // 兜底：25 秒后强制淡出，绝不永久遮挡

  var wrap = null, numEl = null, barEl = null, fgEl = null, txtEl = null;
  var shown = false, startedAt = 0, finished = false, timer = null;

  function build() {
    if (wrap) return;
    wrap = document.createElement('div');
    wrap.id = 'indesk-conn';
    wrap.innerHTML =
      '<div class="ic-ring">' +
        '<svg viewBox="0 0 100 100" aria-hidden="true">' +
          '<circle class="ic-bg" cx="50" cy="50" r="45"></circle>' +
          '<circle class="ic-fg" cx="50" cy="50" r="45"></circle>' +
        '</svg>' +
        '<div class="ic-num"><span>0</span><i>%</i></div>' +
      '</div>' +
      '<div class="ic-phrase">' + PHRASES[0] + '</div>' +
      '<div class="ic-bar"><i></i></div>';
    (document.body || document.documentElement).appendChild(wrap);
    numEl = wrap.querySelector('.ic-num span');
    barEl = wrap.querySelector('.ic-bar i');
    fgEl = wrap.querySelector('.ic-fg');
    txtEl = wrap.querySelector('.ic-phrase');
  }

  function paint(p) {
    if (!wrap) return;
    if (numEl) numEl.textContent = Math.floor(p);
    if (barEl) barEl.style.width = p + '%';
    if (fgEl) fgEl.style.strokeDashoffset = String(283 - 283 * p / 100);
    if (txtEl) {
      var step = MAXP / PHRASES.length;
      var i = Math.min(PHRASES.length - 1, Math.floor(p / step));
      if (txtEl.textContent !== PHRASES[i]) txtEl.textContent = PHRASES[i];
    }
  }

  function show() {
    if (shown) return;
    if (!document.body) return;
    build();
    /* [20261001] 每轮重新爬。
       缺陷：`p` 一旦爬到 MAXP(92) 就再也进不了 `if (p < MAXP)` 分支，
       而 `show()` 又固定 `paint(0)` ⇒ 25s 硬超时隐藏后重建的遮罩会
       **永远停在 0%**（只有每 25s flash 一下 100% 再跳回 0%）。
       手机端用户实测截图就是「0% 正在建立加密信道」纹丝不动。
       修法：本函数即"新一轮开始"，把进度归零，让它重新爬。 */
    p = 0;
    paint(0);
    shown = true;
    startedAt = Date.now();
    finished = false;
    /* [20260928.3] 整屏独占：动画在，同一屏上不许再有数据栏 / 顶栏等任何 UI。
       只挂一个类，具体藏哪些交给 CSS 的 html.indesk-connecting 段。 */
    try { document.documentElement.classList.add('indesk-connecting'); } catch (e) { }
    if (timer) { clearTimeout(timer); timer = null; }
    timer = setTimeout(forceHide, HARD_TIMEOUT);
  }

  function hide() {
    if (!shown || !wrap) return;
    shown = false;
    finished = true;
    /* [20260928.3] 与 show() 对称：动画一消失，被藏起来的 UI 立刻复原。 */
    try { document.documentElement.classList.remove('indesk-connecting'); } catch (e) { }
    if (timer) { clearTimeout(timer); timer = null; }
    var node = wrap;
    wrap = null; numEl = null; barEl = null; fgEl = null; txtEl = null;
    node.classList.add('ic-hide');
    setTimeout(function () {
      if (node && node.parentNode) node.parentNode.removeChild(node);
    }, 420);
  }

  function forceHide() {
    if (!shown) return;
    if (barEl) barEl.style.width = '100%';
    if (numEl) numEl.textContent = '100';
    if (fgEl) fgEl.style.strokeDashoffset = '0';
    setTimeout(hide, 260);
  }

  /* 出帧判定：只认 .remote-video 里真正有尺寸的 <video>（保守，避免误判提前露出黑屏） */
  function hasFrame() {
    var box = document.querySelector('.remote-video');
    if (!box) return false;
    var v = box.querySelector('video');
    if (v && v.videoWidth > 0 && v.videoHeight > 0) return true;
    return false;
  }

  var p = 0;

  function tick() {
    var inRoom = !!document.querySelector('.remote-video');
    if (!inRoom) {
      if (shown) hide(); else p = 0;
      return;
    }
    if (hasFrame()) {
      if (shown) {
        if (Date.now() - startedAt >= MIN_SHOW_MS) {
          p = 100; paint(100);
          setTimeout(hide, 220);
        }
      }
      return;
    }
    if (document.hidden) return;
    show();
    if (p < MAXP) {
      p += Math.max(0.35, (MAXP - p) * 0.018);
      if (p > MAXP) p = MAXP;
      paint(p);
    }
  }

  function boot() {
    setInterval(tick, 130);
    tick();
  }

  if (document.readyState === 'loading') {
    document.addEventListener('DOMContentLoaded', function () { setTimeout(boot, 200); });
  } else {
    setTimeout(boot, 200);
  }
})();


/* ============================================================================
   H. 【v11 重做】「退出登录」入口 —— 注入「账号信息」弹窗，不再碰侧栏/底栏
   ----------------------------------------------------------------------------
   用户反馈："你的退出账号为什么挤在底部导航栏的右上角？很难看。
               你把它放到账号信息里啊，点开账号信息，里面加上退出登录不行吗？"

   定版事实（对 dist 实测）：
     · .sidebar .list 只有 5 项，第 5 项 = {{ token ? '账号信息' : '账号登录' }}，
       其 onClick 即 handleAccountEntry（已登录→弹出账号信息弹窗）。
     · 账号弹窗结构：.acc-overlay > .acc-mask + .acc-card > .acc-top + .acc-body，
       .acc-body 定版 padding 14px 18px 18px，内部只有 .row/.hr/.bind-tip/.bind-row
       ⇒ 底部干净，接一块 .ink-acc-foot 不打架。
     · 弹窗是 v-if 渲染 ⇒ 每次打开都是新节点，所以这里必须**持续轮询补挂**。

   ⚠️ 为什么不用 window.confirm：
     安卓 WebView 壳若没实现 WebChromeClient.onJsConfirm，confirm() 会**直接返回
     false**（不弹框）⇒ 用户点了按钮"完全没反应"。v8 就是这么写的。
     v11 改为**按钮内两次点击确认**（4 秒内在同一按钮上再点一次才真退），
     零宿主依赖。
   ============================================================================
   为什么不用 store.logout()：贴片运行时拿不到 Pinia 实例。查源码知其等价物是
   store/user/index.ts 的 logout() → cache.clearStorage('token')，而
   cache = new CacheModel(LS_KEY_PREFIX)，LS_KEY_PREFIX = 'indesk___'、
   LS_KEY.token = 'token' ⇒ 实际键名 indesk___token；另外 pinia 持久化缓存
   （indesk___pinia-cache）与「记住密码」（indesk___password）里有同一份登录态，
   一并清掉才算真登出。
   ============================================================================ */
(function () {
  var TOKEN_KEY = 'indesk___token';
  var PINIA_KEY = 'indesk___pinia-cache';
  var PWD_KEY = 'indesk___password';
  var FOOT_CLS = 'ink-acc-foot';
  var ARM_MS = 4000;      /* 二次确认的有效窗口 */

  function lsKeys() {
    var out = [];
    try {
      for (var i = 0; i < localStorage.length; i += 1) out.push(localStorage.key(i));
    } catch (e) { /* 隐私模式等：当作未登录 */ }
    return out;
  }

  function loggedIn() {
    try {
      var ks = lsKeys();
      for (var i = 0; i < ks.length; i += 1) {
        var k = ks[i] || '';
        if (k === TOKEN_KEY) return true;
        if (k.indexOf('indesk___') === 0 && /token/i.test(k)) return true;
      }
    } catch (e) { }
    return false;
  }

  function doLogout() {
    try {
      lsKeys().forEach(function (k) {
        if (!k) return;
        if (k === TOKEN_KEY || k === PINIA_KEY || k === PWD_KEY || /token/i.test(k)) {
          localStorage.removeItem(k);
        }
      });
      sessionStorage.clear();
    } catch (e) { }
    /* 必须 reload：Pinia 的内存态还在，只跳路由的话底栏仍会显示"账号信息" */
    try { window.location.hash = '#/login'; } catch (e) { }
    window.setTimeout(function () { try { window.location.reload(); } catch (e) { } }, 80);
  }

  var armed = false;
  var armTimer = null;

  function resetArm(btn) {
    armed = false;
    if (armTimer) { window.clearTimeout(armTimer); armTimer = null; }
    if (!btn) return;
    btn.classList.remove('ink-acc-danger');
    btn.textContent = '退出登录';
  }

  function onLogoutClick(btn) {
    if (armed) { resetArm(btn); doLogout(); return; }
    armed = true;
    btn.classList.add('ink-acc-danger');
    btn.textContent = '再点一次确认退出';
    if (armTimer) window.clearTimeout(armTimer);
    armTimer = window.setTimeout(function () { resetArm(btn); }, ARM_MS);
  }

  function buildFoot() {
    var foot = document.createElement('div');
    foot.className = FOOT_CLS;
    var btn = document.createElement('button');
    btn.type = 'button';
    btn.className = 'ink-acc-logout';
    btn.textContent = '退出登录';
    btn.addEventListener('click', function (ev) {
      try { if (ev) { ev.preventDefault(); ev.stopPropagation(); } } catch (e) { }
      onLogoutClick(btn);
    });
    foot.appendChild(btn);
    return foot;
  }

  function sync() {
    var body = document.querySelector('.acc-overlay .acc-body');
    if (!body) return;                       /* 弹窗未打开 */
    var foot = body.querySelector('.' + FOOT_CLS);
    if (!loggedIn()) {
      if (foot && foot.parentNode) foot.parentNode.removeChild(foot);
      return;
    }
    if (!foot) { foot = buildFoot(); body.appendChild(foot); return; }
    if (foot.parentNode !== body) body.appendChild(foot);
  }

  function boot() {
    window.setInterval(sync, 900);
    sync();
  }

  if (document.readyState === 'loading') {
    document.addEventListener('DOMContentLoaded', function () { window.setTimeout(boot, 400); });
  } else {
    window.setTimeout(boot, 400);
  }
})();


/* ============================================================================
   I / J. 【v11 新增】页面增强模块
   ----------------------------------------------------------------------------
   I. 设备列表页（#/deviceManage）：把"一行裸 UUID"变成信息卡片
      定版 DOM：.setting-wrap > .container > .label + .link-device-list
                > .link-device-item > .left{裸 UUID} + .right > .del{删除}
      · 只**追加**节点（.ink-dev-info）与**追加**统计条，不改定版结构、
        不重排、不删节点 ⇒ Vue 的 key/事件绑定（删除按钮）全部保持原样；
      · .left 由 CSS 整块隐藏（它是静态文本节点，无显隐切换，隐藏安全）；
      · 在线的判断与定版 remote 页完全一致：
          fetch /desk_user/find_receiver_by_uuid?uuid=xxx
          res.code===200 && res.data.receiver ⇒ 在线；并回填 res.data.name / ip
      · 最后连接时间：定版缓存里没有该字段 ⇒ 本脚本自建台账
        localStorage['indesk___link-log']，**只在 #/webrtc 真正出帧时**打点。
        没有记录就显示「—」，绝不编造历史。

   J. 高级设置页（#/setting）：给每个块的 .label 注入定版同款线性图标
      （stroke 1.6 / currentColor / viewBox 24，与定版侧栏图标同一套画法）

   ⚠️ 全部只作用于 html.indesk-mobile（电脑端/网页版一个字节都不碰）。
   ============================================================================ */
(function () {
  var html = null;
  try { html = document.documentElement; } catch (e) { return; }
  if (!html || !html.classList.contains('indesk-mobile')) return;

  /* ---------------- 本地存储小工具（兼容两种序列化格式） ---------------- */
  /* CacheModel.setStorageExp/setStorage 写的是 {value,createTime[,expireTime]}
     而 pinia 持久化插件直接把 state 序列化（无包装）。两者都要能读。 */
  function readAny(key) {
    try {
      var raw = localStorage.getItem(key);
      if (!raw) return null;
      var o = JSON.parse(raw);
      if (o && typeof o === 'object' && !Array.isArray(o)) {
        if (Object.prototype.hasOwnProperty.call(o, 'value')) return o.value;
      }
      return o;
    } catch (e) { return null; }
  }

  var LINK_LOG = 'indesk___link-log';

  function logRead() {
    try {
      var o = JSON.parse(localStorage.getItem(LINK_LOG) || '{}');
      return (o && typeof o === 'object') ? o : {};
    } catch (e) { return {}; }
  }
  function logWrite(o) {
    try { localStorage.setItem(LINK_LOG, JSON.stringify(o)); } catch (e) { }
  }

  /* ---------------- 接口访问（与定版同域/同 token 口径） ---------------- */
  function apiBase() {
    var v = readAny('indesk___axiosBaseUrl');
    if (v) return String(v).replace(/\/+$/, '');
    return '';                       /* 空 = 同源相对路径（定版生产即同域部署） */
  }
  function authToken() {
    var t = readAny('indesk___token');
    return t ? String(t) : '';
  }
  function apiGet(path) {
    var headers = {};
    var t = authToken();
    if (t) headers.Authorization = 'Bearer ' + t;
    return fetch(apiBase() + path, {
      method: 'GET',
      headers: headers,
      credentials: 'include'
    }).then(function (r) { return r.json(); });
  }

  /* ---------------- 设备码：与定版 formatDeviceCode 逐字一致 ---------------- */
  function deviceCode(v) {
    var d = String(v || '').replace(/\D/g, '');
    if (d.length !== 9) return String(v || '');
    return d.slice(0, 3) + '-' + d.slice(3, 6) + '-' + d.slice(6, 9);
  }

  /* ---------------- 时间：相对口径 ---------------- */
  function pad2(n) { return n < 10 ? '0' + n : String(n); }
  function agoText(ts) {
    if (!ts) return '暂无记录';
    var d = Date.now() - ts;
    if (d < 0) d = 0;
    if (d < 60000) return '刚刚';
    if (d < 3600000) return Math.floor(d / 60000) + ' 分钟前';
    if (d < 86400000) return Math.floor(d / 3600000) + ' 小时前';
    if (d < 2592000000) return Math.floor(d / 86400000) + ' 天前';
    var t = new Date(ts);
    return t.getFullYear() + '-' + pad2(t.getMonth() + 1) + '-' + pad2(t.getDate());
  }

  /* ---------------- 当前设备码（用于最后连接打点） ---------------- */
  function currentUuid() {
    try {
      var m = /(?:roomId|remoteDeskUserUuid|uuid)=([0-9A-Za-z_-]{6,})/.exec(String(location.hash || ''));
      if (m) return m[1];
    } catch (e) { }
    var pc = readAny('indesk___pinia-cache');
    if (pc && pc.remoteDeskUserUuid) return String(pc.remoteDeskUserUuid);
    return '';
  }

  /* ---------------- 在线状态（节流 60s / 每设备） ---------------- */
  var onlineMap = {};    /* uuid -> true/false/undefined(未知) */
  var onlineAt = {};     /* uuid -> 上次查询完成时间 */
  var asking = {};       /* uuid -> 请求中 */
  var metaMap = {};      /* uuid -> {name, ip}（接口回填） */
  var TTL = 60000;

  function ensureOnline(uuid) {
    if (!uuid) return;
    var t = onlineAt[uuid] || 0;
    if (asking[uuid] || (t && Date.now() - t < TTL)) return;
    asking[uuid] = 1;
    function done() { asking[uuid] = 0; }
    apiGet('/desk_user/find_receiver_by_uuid?uuid=' + encodeURIComponent(uuid))
      .then(function (res) {
        var ok = res && res.code === 200;
        onlineMap[uuid] = !!(ok && res.data && res.data.receiver);
        var d = (res && res.data) || {};
        if (d.name || d.ip) {
          var m = metaMap[uuid] || (metaMap[uuid] = {});
          if (d.name) m.name = String(d.name);
          if (d.ip) m.ip = String(d.ip);
        }
        onlineAt[uuid] = Date.now();
        done();
      })['catch'](function () {
        /* 网络异常：**不改写结论**（保持"检测中"），15 秒后重试，绝不谎报离线 */
        onlineAt[uuid] = Date.now() - TTL + 15000;
        done();
      });
  }

  /* ---------------- 设备卡片：构建 + 增量更新 ---------------- */
  var CLOCK_SVG =
    '<svg class="ink-dev-clock" viewBox="0 0 24 24" aria-hidden="true">' +
      '<circle cx="12" cy="12" r="8.4" fill="none" stroke="currentColor" stroke-width="1.5"/>' +
      '<path d="M12 7.6V12l3 1.8" fill="none" stroke="currentColor" stroke-width="1.5" ' +
      'stroke-linecap="round" stroke-linejoin="round"/>' +
    '</svg>';

  function buildInfo() {
    var info = document.createElement('div');
    info.className = 'ink-dev-info';
    info.innerHTML =
      '<div class="ink-dev-top">' +
        '<span class="ink-dev-name"></span>' +
        '<span class="ink-dev-badge"></span>' +
      '</div>' +
      '<div class="ink-dev-sub">' +
        '<span class="ink-dev-code"></span>' +
        '<span class="ink-dev-sep">·</span>' +
        '<span class="ink-dev-ip"></span>' +
      '</div>' +
      '<div class="ink-dev-sub ink-dev-last">' + CLOCK_SVG +
        '<span class="ink-dev-last-tx"></span>' +
      '</div>';
    return info;
  }

  function setText(el, txt) {
    if (el && el.textContent !== txt) el.textContent = txt;
  }

  /* 定版空态：该页没有渲染 .link-device-list 时（v-else 分支），.container 里是
     一个**无类名的裸 div**（文本"暂无记录"）。它也得被打标，否则样式进不去。
     ⚠️ 必须与高级设置页区分：设置页 .container 的直接子里只有 .item / .hr
        （都带类名），且没有"最近连接"这个直接子 .label。
     ⚠️ 不用 :scope 选择器（部分 WebView 版本支持不好），直接遍历 children 判类名。 */
  function decorateEmptyIn(cont) {
    if (!cont) return;
    if (cont.querySelector('.link-device-list')) return;   /* 这个容器有列表 → 不空 */
    var kids = cont.children;
    var lb = null;
    for (var i = 0; i < kids.length; i++) {
      var cls = ' ' + (kids[i].className || '') + ' ';
      if (cls.indexOf(' label ') >= 0) { lb = kids[i]; break; }
    }
    if (!lb) return;                      /* 设置页：直接子里没有 .label */
    var t = (lb.textContent || '').replace(/\s/g, '');
    if (t.indexOf('最近连接') < 0 && t.indexOf('我的设备') < 0) return;
    for (var j = 0; j < kids.length; j++) {
      var e = kids[j];
      if (e === lb) continue;
      if (e.className) continue;          /* 有类名的一律不碰 */
      if (!(e.textContent || '').trim()) continue;
      if (e.classList && !e.classList.contains('ink-dev-empty')) e.classList.add('ink-dev-empty');
    }
  }

  /* 逐个容器判断，而不是"全页只有一个列表" —— 否则同页存在有列表的容器时，
     空态容器会被永久短路（jsdom 验证 E1 就是踩这个）。 */
  function decorateEmpty() {
    var cs = document.querySelectorAll('.setting-wrap .container');
    for (var i = 0; i < cs.length; i++) decorateEmptyIn(cs[i]);
  }

  function decorateDeviceList() {
    decorateEmpty();                       /* 空态容器先各自打标 */
    var lists = document.querySelectorAll('.link-device-list');
    for (var L = 0; L < lists.length; L++) decorateOneList(lists[L]);
  }

  function decorateOneList(list) {
    var items = list.querySelectorAll('.link-device-item');
    var log = logRead();
    var total = items.length;
    var on = 0;

    for (var i = 0; i < items.length; i++) {
      var it = items[i];
      var left = it.querySelector('.left');
      if (!left) continue;

      /* 定版 .left 里就是裸的 remoteDeskUserUuid —— 缓存到 data 上，
         避免 Vue 重渲染文本后我们又读错（实际上它是静态文本，不会变） */
      var raw = it.getAttribute('data-ink-code');
      if (!raw) {
        raw = (left.textContent || '').replace(/\s+/g, '');
        if (!raw) continue;
        it.setAttribute('data-ink-code', raw);
      }
      var uuid = raw;

      var info = it.querySelector('.ink-dev-info');
      if (!info) {
        info = buildInfo();
        it.insertBefore(info, it.firstChild);
      }

      var meta = metaMap[uuid] || {};
      var st = onlineMap[uuid];
      if (st === true) on += 1;

      setText(info.querySelector('.ink-dev-name'), meta.name || '未知设备');
      setText(info.querySelector('.ink-dev-code'), deviceCode(uuid));
      setText(info.querySelector('.ink-dev-ip'), meta.ip || 'IP 未知');
      setText(info.querySelector('.ink-dev-last-tx'),
              '最后连接 ' + agoText(log[uuid]));

      var bd = info.querySelector('.ink-dev-badge');
      if (bd) {
        var cls = 'ink-dev-badge';
        var tx = '检测中';
        if (st === true) { cls += ' ink-on'; tx = '在线'; }
        else if (st === false) { cls += ' ink-off'; tx = '离线'; }
        if (bd.className !== cls) bd.className = cls;
        setText(bd, tx);
      }

      ensureOnline(uuid);
    }

    /* 统计条：插在列表前面（.label 之后） */
    var bar = list.parentNode ? list.parentNode.querySelector('.ink-dev-bar') : null;
    if (!bar) {
      bar = document.createElement('div');
      bar.className = 'ink-dev-bar';
      bar.innerHTML = '共 <b class="ink-dev-total"></b> 台设备 · <b class="ink-dev-on"></b> 台在线';
      if (list.parentNode) list.parentNode.insertBefore(bar, list);
    }
    setText(bar.querySelector('.ink-dev-total'), String(total));
    setText(bar.querySelector('.ink-dev-on'), String(on));
  }

  /* ---------------- 设置页：图标 ---------------- */
  var S = 'fill="none" stroke="currentColor" stroke-width="1.6" stroke-linecap="round" stroke-linejoin="round"';
  var SET_ICONS = [
    /* 接口配置：插头/网口 */
    ['接口配置', '<path d="M4 8.5h16v7a2 2 0 0 1-2 2H6a2 2 0 0 1-2-2v-7Z" ' + S + '/>' +
                 '<path d="M8 8.5V6M12 8.5V6M16 8.5V6" ' + S + '/>'],
    /* 产品信息：圆 i */
    ['产品信息', '<circle cx="12" cy="12" r="8.6" ' + S + '/>' +
                 '<path d="M12 10.8v6" ' + S + '/><circle cx="12" cy="7.8" r="0.95" fill="currentColor"/>'],
    /* 网页版体验：浏览器 */
    ['网页版体验', '<rect x="3" y="4.5" width="18" height="15" rx="2.2" ' + S + '/>' +
                   '<path d="M3 9h18" ' + S + '/><circle cx="6.4" cy="6.8" r="0.9" fill="currentColor"/>' +
                   '<circle cx="9.2" cy="6.8" r="0.9" fill="currentColor"/>'],
    /* 私有化部署：服务器 */
    ['私有化部署', '<rect x="3.5" y="4" width="17" height="7" rx="1.8" ' + S + '/>' +
                   '<rect x="3.5" y="13" width="17" height="7" rx="1.8" ' + S + '/>' +
                   '<path d="M7 7.5h.01M7 16.5h.01" ' + S + '/>'],
    /* 下载客户端：下载箭头（与定版侧栏"客户端下载"同款画法） */
    ['下载客户端', '<path d="M12 3.5v11" ' + S + '/><path d="M7.8 10.6 12 14.8l4.2-4.2" ' + S + '/>' +
                   '<path d="M4.5 17.2v1.4a1.8 1.8 0 0 0 1.8 1.8h11.4a1.8 1.8 0 0 0 1.8-1.8v-1.4" ' + S + '/>'],
    /* 关于 InDesk：产品标识 */
    ['关于', '<circle cx="12" cy="12" r="8.6" ' + S + '/>' +
             '<path d="M12 11v6" ' + S + '/><circle cx="12" cy="7.9" r="0.95" fill="currentColor"/>'],
    /* 电脑端才有：界面设置 / 开机自启 */
    ['界面设置', '<rect x="3" y="4.5" width="18" height="12" rx="1.8" ' + S + '/>' +
                 '<path d="M8.5 20h7M12 16.5V20" ' + S + '/>'],
    ['开机自启', '<path d="M12 3.6v7.4" ' + S + '/>' +
                 '<path d="M6.9 6.4a7.5 7.5 0 1 0 10.2 0" ' + S + '/>']
  ];

  function decorateSetting() {
    var labels = document.querySelectorAll('.setting-wrap .container > .item > .label');
    for (var i = 0; i < labels.length; i++) {
      var lb = labels[i];
      if (lb.querySelector('.ink-set-ic')) continue;
      var tx = (lb.textContent || '').replace(/\s/g, '');
      if (!tx) continue;
      var svg = null;
      for (var j = 0; j < SET_ICONS.length; j++) {
        if (tx.indexOf(SET_ICONS[j][0]) === 0) { svg = SET_ICONS[j][1]; break; }
      }
      if (!svg) continue;
      var span = document.createElement('span');
      span.className = 'ink-set-ic';
      span.innerHTML = '<svg viewBox="0 0 24 24" width="16" height="16" aria-hidden="true">' + svg + '</svg>';
      lb.insertBefore(span, lb.firstChild);
    }
  }

  /* ---------------- 最后连接时间：只在真正出帧时打点 ---------------- */
  var lastHit = {};

  function recordLink() {
    var box = document.querySelector('.remote-video');
    if (!box) return;
    var v = box.querySelector('video');
    if (!v || !(v.videoWidth > 0) || !(v.videoHeight > 0)) return;   /* 未出帧 */
    var uuid = currentUuid();
    if (!uuid) return;
    var now = Date.now();
    if (lastHit[uuid] && now - lastHit[uuid] < 60000) return;
    lastHit[uuid] = now;
    var log = logRead();
    log[uuid] = now;
    logWrite(log);
  }

  /* ---------------- 主循环 ---------------- */
  function syncAll() {
    try { recordLink(); } catch (e) { }
    try { decorateDeviceList(); } catch (e) { }
    try { decorateSetting(); } catch (e) { }
  }

  function boot() {
    window.setInterval(syncAll, 800);
    syncAll();
  }

  if (document.readyState === 'loading') {
    document.addEventListener('DOMContentLoaded', function () { window.setTimeout(boot, 600); });
  } else {
    window.setTimeout(boot, 600);
  }
})();
