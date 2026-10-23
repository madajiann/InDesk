package com.indesk.mobile;

import android.app.Activity;
import android.app.AlertDialog;
import android.content.Intent;
import android.content.SharedPreferences;
import android.content.pm.ActivityInfo;
import android.content.pm.PackageManager;
import android.graphics.Bitmap;
import android.net.Uri;
import android.os.Build;
import android.os.Bundle;
import android.os.Handler;
import android.os.Looper;
import android.os.SystemClock;
import android.provider.Settings;
import android.view.KeyEvent;
import android.view.View;
import android.webkit.JavascriptInterface;
import android.webkit.WebChromeClient;
import android.webkit.WebResourceRequest;
import android.webkit.WebSettings;
import android.webkit.WebView;
import android.webkit.WebViewClient;
import android.widget.EditText;
import android.widget.FrameLayout;
import android.widget.LinearLayout;
import android.widget.ProgressBar;
import android.widget.TextView;
import android.widget.Toast;

import org.json.JSONObject;

import java.io.BufferedReader;
import java.io.File;
import java.io.FileOutputStream;
import java.io.InputStream;
import java.io.InputStreamReader;
import java.net.HttpURLConnection;
import java.net.URL;
import java.nio.charset.StandardCharsets;

/**
 * InDesk mobile shell.
 * Web UI is hot-updated from the server. APK is only the shell:
 * fullscreen WebView + a minimal launch screen. No extra chrome.
 */
public class MainActivity extends Activity {

    public static final String APP_NAME = "InDesk";
    /** 仅作读取失败时的兜底；真实版本一律用 selfVersionCode()/selfVersionName() 从系统读 */
    public static final int APP_VERSION_CODE = 19;
    public static final String APP_VERSION = "1.3.3";

    /**
     * [1.2.10 关键修复] 真实已安装版本：从 PackageManager 读，**不再信任写死的常量**。
     * 事故：常量曾漏改（APK 实际 versionCode=12，常量还是 11）→ 装上"最新版"后
     * APP 自认比服务器旧 → **每次打开都提示更新**（用户实测骂过）。读系统值就永不脱节。
     */
    private int selfVersionCode() {
        try {
            android.content.pm.PackageInfo pi =
                    getPackageManager().getPackageInfo(getPackageName(), 0);
            if (Build.VERSION.SDK_INT >= 28) return (int) pi.getLongVersionCode();
            return pi.versionCode;
        } catch (Throwable t) {
            return APP_VERSION_CODE;
        }
    }

    private String selfVersionName() {
        try {
            String n = getPackageManager().getPackageInfo(getPackageName(), 0).versionName;
            return (n == null || n.isEmpty()) ? APP_VERSION : n;
        } catch (Throwable t) {
            return APP_VERSION;
        }
    }

    private static final String DEFAULT_DESK_URL = "https://desk.yomm.top:8443";
    private static final String DEFAULT_VERSION_URL = "https://desk.yomm.top:8443/version.json";

    private static final String PREFS_NAME = "indesk";
    private static final String KEY_URL = "desk_url";
    private static final String KEY_LAST_NOTES = "last_notes";
    private static final String KEY_LAST_REMOTE = "last_remote_version";
    private static final String KEY_PENDING_INSTALL = "pending_install";

    private WebView webView;
    private FrameLayout fullscreenContainer;
    private View customView;
    private WebChromeClient.CustomViewCallback customViewCallback;
    private LinearLayout loadingPanel;
    private ProgressBar progressBar;
    private TextView tvLoadingNote;
    private TextView tvLoadingVer;
    private Handler mainHandler;
    private String deskUrl;
    // [1.2.6] 渲染进程崩溃计数 / 版本检查节流 / 公告弹窗防叠
    private int crashCount = 0;
    private long lastCrashAt = 0;
    private long lastVerCheckAt = 0;
    private boolean launchDialogShowing = false;
    // [1.2.7] 离线兜底是否已触发（触发后不再重复切换）
    private boolean offlineMode = false;
    // [1.2.10] 画面心跳看门狗：网页每 2s 报一次画面健康度，壳据此强拉恢复
    private volatile long lastBeatAt = 0;
    private volatile int beatBad = 0;
    private volatile boolean beatLive = false;
    // [20260921.6] 会话闸：只有网页明确「正在远控会话中」才守护画面。首页/设备列表页
    // 本来就没有画面可救，绝不能因为「没心跳」去重载页面 —— 那正是用户实测的误报来源
    // （1.2.10/1.2.11 壳 + 旧网页：停在首页 >8s 就 Toast「画面无响应」+ reload 死循环）。
    private volatile boolean beatSess = false;
    // [20260921.6] 更新下载/安装期间静默看门狗：此刻页面被对话框压着、用户也没在看画面，
    // 不该弹「正在恢复」打扰；回前台（onResume）自动解除。
    private volatile boolean beatPaused = false;
    private boolean foreground = false;
    // [1.2.12] ★ 启动面板首屏闸门（用户实测："手机端下面导航按钮，没按一次有个过渡画面"）
    // 真因：WebView 每次导航（含 App 内页面切换）都会回调 onPageStarted/onPageFinished，
    // 而这两处**无条件**把 loading_panel（#F7FAFC + 92dp LOGO + 进度条）置 VISIBLE、
    // 250ms 后才 GONE ⇒ 每切一次页面闪一次"白底 LOGO 过渡画面"。
    // 修：loading_panel 只允许在冷启动首次主文档加载完成前出现；之后一律不碰。
    private boolean firstLoadDone = false;
    private int beatReloads = 0;
    private long beatReloadWindowAt = 0;
    // [1.3.1] 已下发给 WebView 的地址。首屏已提前加载过一次，
    // version.json 回来后只有"地址真的变了"才允许再 loadUrl，避免二次白屏。
    private String lastLoadedUrl = null;

    @Override
    protected void onCreate(Bundle savedInstanceState) {
        super.onCreate(savedInstanceState);
        setContentView(R.layout.activity_main);
        mainHandler = new Handler(Looper.getMainLooper());

        fullscreenContainer = findViewById(R.id.fullscreen_container);
        webView = findViewById(R.id.webview);
        loadingPanel = findViewById(R.id.loading_panel);
        progressBar = findViewById(R.id.progress);
        tvLoadingNote = findViewById(R.id.tv_loading_note);
        tvLoadingVer = findViewById(R.id.tv_loading_ver);

        SharedPreferences prefs = getSharedPreferences(PREFS_NAME, MODE_PRIVATE);
        deskUrl = prefs.getString(KEY_URL, DEFAULT_DESK_URL);

        tvLoadingVer.setText("v" + selfVersionName());
        setupWebView();
        setStatus(5, "正在连接");
        // [1.3.1] 首屏不再等 version.json。
        // 旧顺序是"先同步拉 version.json（连接+读取各 12s 超时）→ 才 loadUrl"，
        // 弱网/自签证书握手慢时用户就干等十几秒。现在先用上次记住的地址立刻
        // 开始加载，version.json 放后台并行，回来发现地址变了才重新加载。
        lastLoadedUrl = deskUrl;
        webView.loadUrl(deskUrl);
        new Thread(() -> checkRemoteAndLoad(prefs)).start();
        // [1.2.10] 启动画面心跳看门狗（前台运行时每 3s 检查一次网页心跳）
        mainHandler.postDelayed(beatWatchdog, 5000);

        // [1.3.0 前台保活] 常驻前台服务：游戏抢前台时进程不被冻结/回收（根治切游戏掉线）。
        // 必须在应用处于前台时启动（Android 12+ FGS 限制），onCreate 恰好满足。
        startKeepAliveService();
        requestNotificationPermissionIfNeeded();
        promptBatteryOptimizationOnce(prefs);
    }

    /** [1.3.0] 启动前台保活服务（KeepAliveService） */
    private void startKeepAliveService() {
        try {
            Intent svc = new Intent(this, KeepAliveService.class);
            if (Build.VERSION.SDK_INT >= 26) {
                startForegroundService(svc);
            } else {
                startService(svc);
            }
        } catch (Exception ignored) {
            // 个别 ROM 限制后台启动：最坏退回 web 层保活（1.2.12 行为），不崩溃
        }
    }

    /** [1.3.0] Android 13+ 通知权限：不弹授权的话前台服务的常驻通知不显示（服务本身照跑） */
    private static final int REQ_POST_NOTIFICATIONS = 20261;

    private void requestNotificationPermissionIfNeeded() {
        if (Build.VERSION.SDK_INT >= 33) {
            if (checkSelfPermission("android.permission.POST_NOTIFICATIONS")
                    != PackageManager.PERMISSION_GRANTED) {
                requestPermissions(new String[]{"android.permission.POST_NOTIFICATIONS"},
                        REQ_POST_NOTIFICATIONS);
            }
        }
    }

    /** [1.3.0] 电池优化白名单：一次性引导（激进 ROM 杀后台的最后一道保险），用户可拒绝 */
    private void promptBatteryOptimizationOnce(SharedPreferences prefs) {
        final String KEY = "battery_opt_asked";
        if (prefs.getBoolean(KEY, false)) return;
        prefs.edit().putBoolean(KEY, true).apply();
        try {
            android.os.PowerManager pm = (android.os.PowerManager) getSystemService(POWER_SERVICE);
            if (pm == null || pm.isIgnoringBatteryOptimizations(getPackageName())) return;
            new AlertDialog.Builder(this)
                    .setTitle("保持连接不断开")
                    .setMessage("为了让远程桌面在切到游戏/其他应用时保持连接不断开，\n"
                            + "建议把 InDesk 加入电池优化白名单（忽略电池优化）。\n\n"
                            + "下一步弹出的系统窗口请选择「允许」。")
                    .setPositiveButton("去设置", (d, w) -> {
                        try {
                            startActivity(new Intent(Settings.ACTION_REQUEST_IGNORE_BATTERY_OPTIMIZATIONS,
                                    Uri.parse("package:" + getPackageName())));
                        } catch (Exception ignored2) {
                            // 个别 ROM 没有该页面：用户可手动去系统设置
                        }
                    })
                    .setNegativeButton("暂不", null)
                    .show();
        } catch (Exception ignored) {
        }
    }

    private void setupWebView() {
        // [1.3.1] 冷启动不再整包清 HTTP 缓存。
        //
        // 1.2.6 时加 clearCache(true) 是为了防"服务器热更后手机仍显示老界面"，
        // 但线上 Caddy 已经对 index.html 返回 `Cache-Control: no-cache`（每次校验
        // ETag），而 JS/CSS 是内容哈希文件名 + `immutable` 长缓存 —— 该失效的会失效。
        // 整包清掉的代价是每次冷启动重下约 1.5MB 资源，弱网下首屏要等好几秒，
        // 正是用户反馈"点开要等很久"的主因之一。
        // 若将来换掉 Caddy 配置，请先确认 index.html 仍是 no-cache 再评估。
        // [1.2.12] WebView 自身铺不透明底色（与壳底衬/layout 同色 #F7FAFC）：
        // 内容区某一帧没绘制时也不会露出别的颜色，避免"闪一下"的观感。
        webView.setBackgroundColor(0xFFF7FAFC);
        WebSettings ws = webView.getSettings();
        ws.setJavaScriptEnabled(true);
        ws.setDomStorageEnabled(true);
        ws.setDatabaseEnabled(true);
        ws.setCacheMode(WebSettings.LOAD_DEFAULT);
        ws.setMediaPlaybackRequiresUserGesture(false);
        ws.setAllowFileAccess(true);
        ws.setAllowContentAccess(true);
        ws.setLoadWithOverviewMode(true);
        ws.setUseWideViewPort(true);
        ws.setBuiltInZoomControls(false);
        ws.setDisplayZoomControls(false);
        ws.setMixedContentMode(WebSettings.MIXED_CONTENT_ALWAYS_ALLOW);
        // [1.2.7] 内嵌离线版（file:///android_asset/web）需要跨域访问自家接口
        ws.setAllowUniversalAccessFromFileURLs(true);
        ws.setUserAgentString(ws.getUserAgentString() + " InDesk/" + APP_VERSION);
        // [1.2.5] JS 桥：网页转屏走原生 setRequestedOrientation（不再用
        // requestFullscreen，规避 WebView 全屏重挂页面导致触控失效）
        webView.addJavascriptInterface(new OrientationBridge(), "IndeskBridge");
        // [1.2.6] 白屏自救：渲染进程崩溃（内存不足/解码压力大时被系统杀）在
        // 用户眼里就是"连着连着白屏了"。监听并自动重建页面，不用手动断开重连。
        // 本地镜像 android.jar 缺该 API，走反射注册（真机 API 26+ 生效）。
        installRenderProcessGoneHandler();

        webView.setWebViewClient(new WebViewClient() {
            @Override
            public boolean shouldOverrideUrlLoading(WebView view, WebResourceRequest request) {
                return false;
            }

            @Override
            public void onReceivedSslError(WebView view, android.webkit.SslErrorHandler handler,
                                           android.net.http.SslError error) {
                handler.proceed();
            }

            @Override
            public void onReceivedError(WebView view, android.webkit.WebResourceRequest request,
                                        android.webkit.WebResourceError error) {
                // [1.2.7] 离线兜底：主文档加载失败（服务器挂了/断网/DNS 故障）时
                // 切到 APK 内嵌的完整网页版（assets/web，与线上同构），保证 APP 永远能开。
                if (request != null && request.isForMainFrame() && !offlineMode) {
                    offlineMode = true;
                    Toast.makeText(MainActivity.this, "网络异常，已切换离线模式", Toast.LENGTH_LONG).show();
                    view.loadUrl("file:///android_asset/web/index.html");
                }
            }

            @Override
            public void onPageStarted(WebView view, String url, Bitmap favicon) {
                // [1.2.12] 只有冷启动首屏才显示启动面板；App 内导航（点底部导航切页面等）
                // 一律不再弹，否则每切一次就闪一次"白底 + LOGO + 进度条"。
                if (!firstLoadDone) setStatus(70, "加载界面");
            }

            @Override
            public void onPageFinished(WebView view, String url) {
                // [1.2.12] 同理：首屏加载完这一次才收面板 + 置就绪，之后的导航完全不碰面板。
                if (!firstLoadDone) {
                    setStatus(100, "就绪");
                    mainHandler.postDelayed(() -> loadingPanel.setVisibility(View.GONE), 250);
                }
                firstLoadDone = true;
                // [1.2.10] 页面就绪：给心跳看门狗重新计时（新页面刚开始还没报心跳）
                lastBeatAt = SystemClock.elapsedRealtime();
                beatBad = 0;
            }
        });

        webView.setWebChromeClient(new WebChromeClient() {
            @Override
            public void onProgressChanged(WebView view, int newProgress) {
                if (loadingPanel.getVisibility() == View.VISIBLE) {
                    int p = Math.max(20, Math.min(99, newProgress));
                    setStatus(p, "加载界面");
                }
            }

            @Override
            public void onShowCustomView(View view, CustomViewCallback callback) {
                customView = view;
                customViewCallback = callback;
                fullscreenContainer.addView(view);
                fullscreenContainer.setVisibility(View.VISIBLE);
            }

            @Override
            public void onHideCustomView() {
                if (customView != null) {
                    fullscreenContainer.removeView(customView);
                    customView = null;
                }
                fullscreenContainer.setVisibility(View.GONE);
                if (customViewCallback != null) {
                    customViewCallback.onCustomViewHidden();
                    customViewCallback = null;
                }
            }
        });
    }

    private void checkRemoteAndLoad(final SharedPreferences prefs) {
        setStatus(12, "检查更新");
        String remoteVer = null;
        String remoteNotes = null;
        String remoteWeb = null;
        int remoteApkCode = 0;
        String remoteApkVer = null;
        String remoteApkUrl = null;
        try {
            String body = httpGet(DEFAULT_VERSION_URL);
            JSONObject json = new JSONObject(body);
            remoteVer = json.optString("version", "");
            remoteNotes = json.optString("notes", "");
            remoteWeb = json.optString("webUrl", deskUrl);
            remoteApkCode = json.optInt("apkVersionCode", 0);
            remoteApkVer = json.optString("apkVersion", "");
            remoteApkUrl = json.optString("apkUrl", "");
            final String fVer = remoteVer;
            final String fNotes = remoteNotes;
            final String fWeb = remoteWeb;
            final String lastRemote = prefs.getString(KEY_LAST_REMOTE, "");
            final int fApkCode = remoteApkCode;
            final String fApkVer = remoteApkVer;
            final String fApkUrl = remoteApkUrl;
            mainHandler.post(() -> {
                setStatus(40, "检查更新");
                prefs.edit()
                        .putString(KEY_LAST_REMOTE, fVer)
                        .putString(KEY_LAST_NOTES, fNotes)
                        .apply();
                String target = (fWeb != null && !fWeb.isEmpty()) ? fWeb : deskUrl;
                deskUrl = target;
                prefs.edit().putString(KEY_URL, target).apply();
                // [1.3.1] 首屏已经按上次地址开始加载了；只有服务器换了 webUrl 才重载。
                if (!target.equals(lastLoadedUrl)) {
                    lastLoadedUrl = target;
                    setStatus(55, "加载界面");
                    webView.loadUrl(target);
                }
                // [1.2.5] 每次启动都弹：版本号 + 公告；有安卓新版时带「立即更新」
                showLaunchDialog(fVer, fNotes, fApkCode, fApkVer, fApkUrl);
                // [1.2.10] 残留待装包只在"服务器确实还有比本机更高的版本"时才补弹；
                // 本机已装到该版本（或更高）→ 标志与残留包一并作废。
                // 旧版漏清标志，导致"覆盖安装完新版本、每次打开还提示安装"（用户实测骂过）。
                if (prefs.getBoolean(KEY_PENDING_INSTALL, false) && updateFile().exists()) {
                    if (fApkCode > selfVersionCode()) {
                        offerInstall((fApkVer == null || fApkVer.isEmpty()) ? ("build " + fApkCode) : fApkVer);
                    } else {
                        prefs.edit().putBoolean(KEY_PENDING_INSTALL, false).apply();
                        try {
                            updateFile().delete();
                        } catch (Throwable ignored) {
                        }
                    }
                }
            });
        } catch (Exception e) {
            mainHandler.post(() -> {
                // [1.3.1] 首屏已发起加载就不要再 loadUrl，否则等于重来一次
                if (!deskUrl.equals(lastLoadedUrl)) {
                    lastLoadedUrl = deskUrl;
                    setStatus(55, "正在连接");
                    webView.loadUrl(deskUrl);
                }
            });
        }
    }

    /**
     * [1.2.6] 反射注册渲染进程崩溃监听（WebView.setOnRenderProcessGoneHandler，
     * API 26+）。崩溃时：轻则 recreate() 重建页面，2 分钟内 3 次则整进程重启。
     * 崩溃瞬间 WebView 已死，recreate 重新走 onCreate 最省事且必然干净。
     */
    private void installRenderProcessGoneHandler() {
        try {
            final Class<?> goneHandler = Class.forName("android.webkit.WebView$RenderProcessGoneHandler");
            java.lang.reflect.Method setter = WebView.class.getMethod(
                    "setOnRenderProcessGoneHandler", WebView.class, goneHandler);
            Object listener = java.lang.reflect.Proxy.newProxyInstance(
                    goneHandler.getClassLoader(), new Class<?>[]{goneHandler},
                    (proxy, method, args) -> {
                        crashCount++;
                        long now = SystemClock.elapsedRealtime();
                        if (now - lastCrashAt > 120_000) crashCount = 1;
                        lastCrashAt = now;
                        Toast.makeText(MainActivity.this, "画面进程异常，正在自动恢复…", Toast.LENGTH_SHORT).show();
                        if (crashCount >= 3) {
                            restartApp();
                        } else {
                            recreate();
                        }
                        return Boolean.TRUE; // 已处理，避免系统再把 APP 整个杀掉
                    });
            setter.invoke(webView, webView, listener);
        } catch (Throwable ignored) {
            // 设备 <API26 或反射失败：退化为旧行为（用户手动重开）
        }
    }

    /**
     * [1.2.9 重写] 更新弹窗，用户明确的交互：
     * - 没有更新 → 不弹任何框，直接进（不再弹"只有进入按钮的公告框"）；
     * - 有更新（apkVersionCode > 当前）→ 弹「发现新版本 vX.Y.Z」，正文就是更新内容，
     *   按钮【立即更新】/【取消】：立即更新 → 进度条（百分比+MB）→ 下载完弹一键安装。
     */
    private void showLaunchDialog(String webVer, String notes, int apkCode, String apkVer, String apkUrl) {
        if (launchDialogShowing) return; // [1.2.6] 防冷启动+onResume 双弹叠窗
        boolean hasApk = apkCode > selfVersionCode() && apkUrl != null && !apkUrl.isEmpty();
        if (!hasApk) return; // [1.2.9] 没更新不弹，直接进
        launchDialogShowing = true;
        String latest = (apkVer == null || apkVer.isEmpty()) ? ("build " + apkCode) : apkVer;
        String msg = "当前版本 v" + selfVersionName() + " → 最新版本 v" + latest + "\n\n"
                + "更新内容：\n"
                + ((notes == null || notes.isEmpty()) ? "体验优化与问题修复。" : notes);
        AlertDialog ad = new AlertDialog.Builder(this)
                .setTitle("发现新版本 v" + latest)
                .setMessage(msg)
                .setCancelable(false)
                .setPositiveButton("立即更新", (d, w) -> startApkDownload(latest, apkUrl))
                .setNegativeButton("取消", null)
                .show();
        ad.setOnDismissListener((d) -> launchDialogShowing = false);
    }

    /** 由启动弹窗「立即更新」触发：下载（进度走启动页进度条）→ 完成弹一键安装 */
    /** 下载进度回调（后台线程调用，实现方自行切主线程） */
    private interface ProgressCb { void on(int pct, long doneBytes, long totalBytes); }

    private android.app.AlertDialog downloadDialog;
    private android.widget.ProgressBar downloadBar;
    private android.widget.TextView downloadNote;

    private void startApkDownload(String apkVer, String apkUrl) {
        final String ver = (apkVer == null || apkVer.isEmpty()) ? "新版" : apkVer;
        // [20260921.6] 下载/安装期间看门狗静默：用户正盯着进度条，不该被
        // 「画面异常，正在自动恢复」的 Toast 打扰（且此刻页面被对话框压着）→ onResume 解除。
        beatPaused = true;
        // [1.2.8] 下载进度对话框：用户点「立即更新」后实时看到百分比进度，
        // 不再是"点了没反应不知道成没成功"
        android.widget.LinearLayout box = new android.widget.LinearLayout(this);
        box.setOrientation(android.widget.LinearLayout.VERTICAL);
        int pad = (int) (getResources().getDisplayMetrics().density * 20);
        box.setPadding(pad, pad / 2, pad, 0);
        downloadNote = new android.widget.TextView(this);
        downloadNote.setText("正在下载 v" + ver + "… 0%");
        downloadBar = new android.widget.ProgressBar(this, null, android.R.attr.progressBarStyleHorizontal);
        downloadBar.setMax(100);
        downloadBar.setProgress(0);
        box.addView(downloadNote);
        box.addView(downloadBar);
        downloadDialog = new AlertDialog.Builder(this)
                .setTitle("下载更新 v" + ver)
                .setView(box)
                .setNegativeButton("后台下载", null) // 关弹窗不取消下载，完成仍弹安装
                .show();
        new Thread(() -> {
            try {
                setStatus(50, "下载新版 " + ver);
                downloadApk(apkUrl, (pct, done, total) -> mainHandler.post(() -> {
                    if (downloadBar != null) downloadBar.setProgress(pct);
                    if (downloadNote != null) {
                        long mb = total > 0 ? total / (1024 * 1024) : 0;
                        downloadNote.setText("正在下载 v" + ver + "… " + pct + "%"
                                + (mb > 0 ? "  (" + (done / (1024 * 1024)) + "/" + mb + " MB)" : ""));
                    }
                }));
                mainHandler.post(() -> {
                    if (downloadDialog != null) { downloadDialog.dismiss(); downloadDialog = null; }
                    offerInstall(ver);
                });
            } catch (Exception e) {
                mainHandler.post(() -> {
                    if (downloadDialog != null) { downloadDialog.dismiss(); downloadDialog = null; }
                    if (loadingPanel.getVisibility() == View.VISIBLE) setStatus(60, "加载界面");
                    Toast.makeText(this, "下载失败：" + e.getMessage() + "，请稍后重试", Toast.LENGTH_LONG).show();
                });
            }
        }).start();
    }

    private File updateFile() {
        return new File(getExternalFilesDir(null), UpdateProvider.FILE_NAME);
    }

    private void downloadApk(String urlStr) throws Exception {
        downloadApk(urlStr, null);
    }

    private void downloadApk(String urlStr, ProgressCb cb) throws Exception {
        URL url = new URL(urlStr);
        HttpURLConnection conn = open(urlStr, 15000, 30000);
        conn.setRequestMethod("GET");
        int total = conn.getContentLength();
        InputStream in = conn.getInputStream();
        File dst = updateFile();
        FileOutputStream out = new FileOutputStream(dst);
        byte[] buf = new byte[16384];
        int n, done = 0, lastPct = -1;
        while ((n = in.read(buf)) > 0) {
            out.write(buf, 0, n);
            done += n;
            if (total > 0) {
                final int pct = Math.min(100, done * 100 / total);
                if (pct != lastPct) {
                    lastPct = pct;
                    if (cb != null) cb.on(pct, done, total);
                    if (loadingPanel.getVisibility() == View.VISIBLE) {
                        final int p2 = Math.min(99, pct);
                        setStatus(50 + p2 / 4, "下载新版 " + p2 + "%");
                    }
                }
            }
        }
        out.close();
        in.close();
        conn.disconnect();
    }

    private void offerInstall(String ver) {
        getSharedPreferences(PREFS_NAME, MODE_PRIVATE).edit().putBoolean(KEY_PENDING_INSTALL, true).apply();
        new AlertDialog.Builder(this)
                .setTitle("InDesk 新版 " + ver + " 已就绪")
                .setMessage("是否立即安装？安装不会丢失任何数据。")
                .setCancelable(false)
                .setPositiveButton("立即安装", (d, w) -> tryInstall())
                .setNegativeButton("下次启动再说", null)
                .show();
    }

    private void tryInstall() {
        if (Build.VERSION.SDK_INT >= 26
                && !getPackageManager().canRequestPackageInstalls()) {
            // 跳到授权页，回来后 onResume 继续安装
            try {
                Intent i = new Intent(Settings.ACTION_MANAGE_UNKNOWN_APP_SOURCES,
                        Uri.parse("package:" + getPackageName()));
                startActivity(i);
            } catch (Exception e) {
                startActivity(new Intent(Settings.ACTION_APPLICATION_DETAILS_SETTINGS,
                        Uri.parse("package:" + getPackageName())));
            }
            return;
        }
        startInstall();
    }

    private void startInstall() {
        // [1.2.10] 已交给系统安装器 → 立刻清掉"待安装"标志，避免装完还反复弹（用户实测骂过）
        getSharedPreferences(PREFS_NAME, MODE_PRIVATE).edit()
                .putBoolean(KEY_PENDING_INSTALL, false).apply();
        Uri uri = Uri.parse("content://" + UpdateProvider.AUTHORITY + "/" + UpdateProvider.FILE_NAME);
        Intent i = new Intent(Intent.ACTION_INSTALL_PACKAGE, uri);
        i.addFlags(Intent.FLAG_GRANT_READ_URI_PERMISSION);
        startActivity(i);
    }

    @Override
    protected void onPause() {
        foreground = false;
        super.onPause();
    }

    @Override
    protected void onResume() {
        super.onResume();
        foreground = true;
        lastBeatAt = SystemClock.elapsedRealtime(); // 后台期间心跳本就停：回前台重新计时
        beatBad = 0;
        beatPaused = false; // [20260921.6] 回前台解除「更新期静默」
        SharedPreferences prefs = getSharedPreferences(PREFS_NAME, MODE_PRIVATE);
        if (prefs.getBoolean(KEY_PENDING_INSTALL, false) && updateFile().exists()
                && Build.VERSION.SDK_INT >= 26
                && getPackageManager().canRequestPackageInstalls()) {
            prefs.edit().putBoolean(KEY_PENDING_INSTALL, false).apply();
            startInstall();
            return;
        }
        // [1.2.6] 从最近任务/切回前台也检查一次更新（版本检查原本只在冷启动跑，
        // 用户"打开 APP"往往是恢复后台进程 → 永远看不到更新提示）。60s 节流。
        if (SystemClock.elapsedRealtime() - lastVerCheckAt > 60_000) {
            new Thread(this::quickVerCheck).start();
        }
        // [1.2.9] 白屏自救第二级：回前台立刻通知网页做一次画面体检（恢复播放 +
        // 跑一轮守护判定），安卓 WebView 切后台再回来最容易出现"连接活着画面死了"。
        try {
            webView.evaluateJavascript("window.__indeskKick && window.__indeskKick();", null);
        } catch (Throwable ignored) {
        }
    }

    /** [1.2.6] 前台恢复时的轻量版本检查：只关心有没有 APK 新版，有才弹（不打扰） */
    private void quickVerCheck() {
        try {
            JSONObject json = new JSONObject(httpGet(DEFAULT_VERSION_URL));
            lastVerCheckAt = SystemClock.elapsedRealtime();
            final int apkCode = json.optInt("apkVersionCode", 0);
            final String apkVer = json.optString("apkVersion", "");
            final String apkUrl = json.optString("apkUrl", "");
            final String notes = json.optString("notes", "");
            final String webVer = json.optString("version", "");
            if (apkCode > selfVersionCode() && !apkUrl.isEmpty()) {
                mainHandler.post(() -> showLaunchDialog(webVer, notes, apkCode, apkVer, apkUrl));
            }
        } catch (Exception ignored) {
            // 静默：网络不可用就算了，冷启动还有一次完整检查
        }
    }

    /** [1.2.6] 渲染进程频繁崩溃时的兜底：定时器拉起 + 杀掉自己进程，等效冷启动 */
    private void restartApp() {
        try {
            Intent i = getPackageManager().getLaunchIntentForPackage(getPackageName());
            if (i == null) return;
            i.addFlags(Intent.FLAG_ACTIVITY_NEW_TASK | Intent.FLAG_ACTIVITY_CLEAR_TASK);
            android.app.PendingIntent pi = android.app.PendingIntent.getActivity(
                    this, 1001, i,
                    android.app.PendingIntent.FLAG_ONE_SHOT | android.app.PendingIntent.FLAG_IMMUTABLE);
            android.app.AlarmManager am = (android.app.AlarmManager) getSystemService(ALARM_SERVICE);
            am.setExact(android.app.AlarmManager.ELAPSED_REALTIME_WAKEUP,
                    SystemClock.elapsedRealtime() + 400, pi);
        } catch (Exception ignored) {
        }
        finishAffinity();
        Runtime.getRuntime().exit(0);
    }

    /** [1.2.5] 网页 → 原生转屏桥：绕开 requestFullscreen 的 WebView 全屏重挂问题 */
    public class OrientationBridge {
        @JavascriptInterface
        public void setOrientation(final String o) {
            mainHandler.post(() -> {
                if ("landscape".equals(o)) {
                    setRequestedOrientation(ActivityInfo.SCREEN_ORIENTATION_SENSOR_LANDSCAPE);
                } else if ("portrait".equals(o)) {
                    setRequestedOrientation(ActivityInfo.SCREEN_ORIENTATION_SENSOR_PORTRAIT);
                } else {
                    setRequestedOrientation(ActivityInfo.SCREEN_ORIENTATION_UNSPECIFIED);
                }
            });
        }

        @JavascriptInterface
        public String appVersion() {
            return selfVersionName();
        }

        /**
         * [1.2.10] 网页画面心跳：网页每 2s 调用一次，报画面健康度。
         * 壳侧看门狗据此判定"网页已挂/画面病态"，强拉恢复——白屏无论什么根因
         * 都不可能长时间停留（网页自愈失败也有壳兜底）。
         */
        @JavascriptInterface
        public void heartbeat(final String json) {
            try {
                JSONObject o = new JSONObject(json);
                lastBeatAt = SystemClock.elapsedRealtime();
                beatLive = o.optInt("live", 0) == 1;
                // sess：网页是否正在远控会话中（看门狗的唯一开关，见 beatSess 字段说明）
                beatSess = o.optInt("sess", 0) == 1;
                beatBad = o.optInt("bad", 0) == 1 ? beatBad + 1 : 0;
            } catch (Throwable ignored) {
                lastBeatAt = SystemClock.elapsedRealtime();
            }
        }
    }

    /** [1.2.10] 看门狗：每 3s 检查一次网页心跳，必要时强拉恢复 */
    private final Runnable beatWatchdog = new Runnable() {
        @Override
        public void run() {
            try {
                // [20260921.6] 三道闸：前台 + 不在更新期 + 网页明确处于远控会话中。
                // 后两条缺一不可 —— 「不在远控页」不等于「网页挂了」，旧版正是靠这一条
                // 在首页反复误触发（Toast「画面异常/画面无响应」+ reload）。
                if (foreground && !beatPaused && beatSess) {
                    long now = SystemClock.elapsedRealtime();
                    if (lastBeatAt > 0 && now - lastBeatAt > 8000) {
                        // 心跳断了：渲染进程挂死 / JS 完全不跑（白屏最恶劣形态）
                        forceRecover("画面无响应");
                    } else if (beatLive && beatBad >= 3) {
                        // JS 活着但画面持续病态（网页自愈阶梯也没救回来）
                        beatBad = 0;
                        forceRecover("画面异常");
                    }
                }
            } catch (Throwable ignored) {
            }
            mainHandler.postDelayed(this, 3000);
        }
    };

    /** [1.2.10] 强拉恢复：先重载页面（连接会重建）；2 分钟内 3 次仍不行则整进程重启 */
    private void forceRecover(String why) {
        long now = SystemClock.elapsedRealtime();
        if (now - beatReloadWindowAt > 120_000) beatReloads = 0;
        beatReloadWindowAt = now;
        beatReloads++;
        lastBeatAt = now; // 防重入：给新页面留出加载时间
        beatBad = 0;
        Toast.makeText(this, "画面异常（" + why + "），正在自动恢复…", Toast.LENGTH_SHORT).show();
        if (beatReloads >= 3) {
            restartApp();
            return;
        }
        try {
            if (offlineMode) {
                webView.loadUrl("file:///android_asset/web/index.html");
            } else {
                webView.reload();
            }
        } catch (Throwable ignored) {
        }
    }

    private void setStatus(int pct, String note) {
        runOnUiThread(() -> {
            loadingPanel.setVisibility(View.VISIBLE);
            progressBar.setProgress(pct);
            tvLoadingNote.setText(note);
        });
    }

    /**
     * [1.2.6] 自签证书放行。desk.yomm.top:8443 用自签 HTTPS，WebView 层已放行
     * （onReceivedSslError），但 HttpURLConnection 默认严格校验 → version.json
     * 拉取和 APK 下载全部 TLS 握手失败 → "打开 APP 从不弹更新框"的根因。
     * 统一走 open()，对 HTTPS 关闭证书校验（自家服务器，风险可控）。
     */
    private static javax.net.ssl.SSLSocketFactory trustAllFactory() {
        try {
            javax.net.ssl.TrustManager[] tm = new javax.net.ssl.TrustManager[]{
                    new javax.net.ssl.X509TrustManager() {
                        public void checkClientTrusted(java.security.cert.X509Certificate[] c, String a) {}
                        public void checkServerTrusted(java.security.cert.X509Certificate[] c, String a) {}
                        public java.security.cert.X509Certificate[] getAcceptedIssuers() {
                            return new java.security.cert.X509Certificate[0];
                        }
                    }};
            javax.net.ssl.SSLContext sc = javax.net.ssl.SSLContext.getInstance("TLS");
            sc.init(null, tm, new java.security.SecureRandom());
            return sc.getSocketFactory();
        } catch (Exception e) {
            return javax.net.ssl.HttpsURLConnection.getDefaultSSLSocketFactory();
        }
    }

    private static HttpURLConnection open(String urlStr, int connectMs, int readMs) throws Exception {
        URL url = new URL(urlStr);
        HttpURLConnection conn = (HttpURLConnection) url.openConnection();
        if (conn instanceof javax.net.ssl.HttpsURLConnection) {
            javax.net.ssl.HttpsURLConnection h = (javax.net.ssl.HttpsURLConnection) conn;
            h.setSSLSocketFactory(trustAllFactory());
            h.setHostnameVerifier((hostname, session) -> true);
        }
        conn.setConnectTimeout(connectMs);
        conn.setReadTimeout(readMs);
        conn.setRequestProperty("User-Agent", "InDesk/" + APP_VERSION);
        return conn;
    }

    private String httpGet(String urlStr) throws Exception {
        URL url = new URL(urlStr);
        HttpURLConnection conn = open(urlStr, 12000, 12000);
        conn.setRequestMethod("GET");
        int code = conn.getResponseCode();
        InputStream in = code >= 400 ? conn.getErrorStream() : conn.getInputStream();
        BufferedReader br = new BufferedReader(new InputStreamReader(in, StandardCharsets.UTF_8));
        StringBuilder sb = new StringBuilder();
        String line;
        while ((line = br.readLine()) != null) sb.append(line).append('\n');
        br.close();
        conn.disconnect();
        if (code >= 400) throw new Exception("HTTP " + code);
        return sb.toString();
    }

    @Override
    public boolean onKeyDown(int keyCode, KeyEvent event) {
        if (keyCode == KeyEvent.KEYCODE_BACK && webView.canGoBack()) {
            webView.goBack();
            return true;
        }
        return super.onKeyDown(keyCode, event);
    }

    @Override
    protected void onDestroy() {
        if (webView != null) webView.destroy();
        super.onDestroy();
    }
}
