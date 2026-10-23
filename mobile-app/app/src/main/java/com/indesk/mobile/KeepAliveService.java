package com.indesk.mobile;

import android.app.Notification;
import android.app.NotificationChannel;
import android.app.NotificationManager;
import android.app.PendingIntent;
import android.app.Service;
import android.content.Context;
import android.content.Intent;
import android.content.pm.ServiceInfo;
import android.os.Build;
import android.os.IBinder;
import android.os.PowerManager;

/**
 * [1.3.0 前台保活] 前台服务——根治"切到游戏 = InDesk 被安卓冻结/杀死"。
 *
 * 机理：挂上前台通知后，本进程的系统优先级提升到前台档（oom_adj 与可见应用同级），
 * 游戏抢前台时系统不再冻结/回收本进程，WebView 里的 WebSocket / WebRTC 会话
 * 全程存活。这是 web 层"不可闻音频保活"（受自动播放策略/ROM 差异限制）的
 * 系统级根治版。
 *
 * 生命周期：
 *  - MainActivity.onCreate 启动（前台状态启动，符合 Android 12+ FGS 限制）；
 *  - 用户从最近任务划掉应用（onTaskRemoved）→ stopSelf，不留僵尸通知；
 *  - 系统内存不足强杀 → START_STICKY 自动拉起。
 */
public class KeepAliveService extends Service {

    private static final String CHANNEL_ID = "indesk_keepalive";
    private static final int NOTIFY_ID = 2026;

    private PowerManager.WakeLock mWakeLock;

    @Override
    public void onCreate() {
        super.onCreate();
        startForegroundWithType();
        acquireWakeLock();
    }

    @Override
    public int onStartCommand(Intent intent, int flags, int startId) {
        // onTaskRemoved 后系统重建时也会走到这里，重新挂前台通知即可
        startForegroundWithType();
        acquireWakeLock();
        return START_STICKY;
    }

    @Override
    public IBinder onBind(Intent intent) {
        return null;
    }

    /** 用户从最近任务划掉壳 = 明确退出，停掉保活，不留僵尸通知 */
    @Override
    public void onTaskRemoved(Intent rootIntent) {
        stopForeground(true);
        stopSelf();
        super.onTaskRemoved(rootIntent);
    }

    @Override
    public void onDestroy() {
        releaseWakeLock();
        super.onDestroy();
    }

    private void startForegroundWithType() {
        NotificationManager nm = (NotificationManager) getSystemService(Context.NOTIFICATION_SERVICE);
        NotificationChannel ch = new NotificationChannel(
                CHANNEL_ID, "连接保活",
                NotificationManager.IMPORTANCE_MIN); // 无声音无横幅，只占通知栏一行
        ch.setDescription("保持远程桌面连接不断开");
        ch.setShowBadge(false);
        nm.createNotificationChannel(ch);

        Intent contentIntent = new Intent(this, MainActivity.class);
        contentIntent.setFlags(Intent.FLAG_ACTIVITY_SINGLE_TOP);
        PendingIntent pi = PendingIntent.getActivity(
                this, 0, contentIntent,
                PendingIntent.FLAG_UPDATE_CURRENT | PendingIntent.FLAG_IMMUTABLE);

        Notification.Builder b;
        if (Build.VERSION.SDK_INT >= 26) {
            b = new Notification.Builder(this, CHANNEL_ID);
        } else {
            b = new Notification.Builder(this);
            b.setPriority(Notification.PRIORITY_MIN);
        }
        Notification n = b
                .setSmallIcon(android.R.drawable.ic_menu_compass)
                .setContentTitle("InDesk 远程桌面")
                .setContentText("连接保活中 · 切到游戏也不掉线")
                .setOngoing(true)
                .setContentIntent(pi)
                .build();

        // API 34 起必须声明 foregroundServiceType；DATA_SYNC 需要 FOREGROUND_SERVICE_DATA_SYNC 权限
        if (Build.VERSION.SDK_INT >= 34) {
            startForeground(NOTIFY_ID, n, ServiceInfo.FOREGROUND_SERVICE_TYPE_DATA_SYNC);
        } else {
            startForeground(NOTIFY_ID, n);
        }
    }

    /** 部分 WakeLock：Doze 下保住 CPU 与网络心跳（WS 探活/RTC 心跳不被掐） */
    private void acquireWakeLock() {
        if (mWakeLock != null && mWakeLock.isHeld()) return;
        try {
            PowerManager pm = (PowerManager) getSystemService(Context.POWER_SERVICE);
            mWakeLock = pm.newWakeLock(PowerManager.PARTIAL_WAKE_LOCK, "InDesk:keepalive");
            mWakeLock.setReferenceCounted(false);
            mWakeLock.acquire();
        } catch (Exception ignored) {
            // 个别 ROM 禁 WakeLock：前台优先级兜底仍然生效
        }
    }

    private void releaseWakeLock() {
        try {
            if (mWakeLock != null && mWakeLock.isHeld()) mWakeLock.release();
        } catch (Exception ignored) {
        }
        mWakeLock = null;
    }
}
