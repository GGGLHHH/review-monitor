package com.reviewmonitor

import android.app.AlarmManager
import android.app.Notification
import android.app.NotificationChannel
import android.app.NotificationManager
import android.app.PendingIntent
import android.app.Service
import android.content.BroadcastReceiver
import android.content.Context
import android.content.Intent
import android.content.pm.ServiceInfo
import android.os.Binder
import android.os.Build
import android.os.Handler
import android.os.IBinder
import android.os.Looper
import android.os.PowerManager
import android.os.SystemClock
import android.util.Log
import androidx.core.app.NotificationCompat
import androidx.core.app.NotificationManagerCompat
import androidx.core.app.ServiceCompat
import com.reviewmonitor.core.AlertInfo
import com.reviewmonitor.core.Engine
import com.reviewmonitor.core.Host
import com.reviewmonitor.core.StatusInfo
import com.reviewmonitor.core.startUrl
import kotlinx.coroutines.flow.MutableStateFlow
import kotlin.math.abs

/** 界面要的全部状态，服务每秒刷新一次 */
data class UiState(
    val fatal: String = "",
    val status: StatusInfo? = null,
    val keep: RoundStats = RoundStats(0, 0, 0),
)

/** 保活记录：过去 24 小时成功检查了几轮、两轮之间最长隔了多久 */
data class RoundStats(val rounds24h: Int, val maxGapMin: Int, val coveredMin: Int)

/** 每轮成功的时间记在 files/rounds.log（一行一个毫秒时间戳），服务被杀重启也接得上 */
class RoundLog(private val file: java.io.File) {
    private val times = ArrayDeque<Long>()
    init {
        runCatching { file.readLines().mapNotNull { it.trim().toLongOrNull() }.forEach { times.addLast(it) } }
        trim(System.currentTimeMillis())
    }
    // 负数是「没登录」的标记（掉线或主动退出时记一条）：它前后的空档不是保活失败，不计入最长间隔
    private fun trim(now: Long) { while (times.isNotEmpty() && kotlin.math.abs(times.first()) < now - 86_400_000L) times.removeFirst() }
    private fun save() { runCatching { file.writeText(times.joinToString(System.lineSeparator())) } }
    fun add(t: Long) {
        if (t <= kotlin.math.abs(times.lastOrNull() ?: 0L)) return
        times.addLast(t)
        trim(System.currentTimeMillis())
        save()
    }
    /** 进入「没登录」状态时调用；连续调用只记一条 */
    fun markLoggedOut(now: Long) {
        if ((times.lastOrNull() ?: 0) < 0) return
        times.addLast(-now)
        save()
    }
    fun stats(now: Long): RoundStats {
        trim(now)
        val rounds = times.count { it > 0 }
        if (rounds == 0) return RoundStats(0, 0, 0)
        var gap = if (times.last() > 0) now - times.last() else 0L
        for (i in 1 until times.size) if (times[i] > 0 && times[i - 1] > 0) gap = maxOf(gap, times[i] - times[i - 1])
        return RoundStats(rounds, (gap / 60_000).toInt(), ((now - times.first { it > 0 }) / 60_000).toInt())
    }
}

/**
 * 常驻监控服务：持有后台页 WebView 和 rm-core 的 Engine，每秒驱动一次。
 * 熄屏后 Handler 会跟着 CPU 睡着，所以每轮检查前用闹钟把手机叫醒，检查期间持有 wake lock，查完就放。
 */
class MonitorService : Service() {
    companion object {
        const val CH_RUNNING = "running"
        const val CH_ALERTS = "alerts"
        const val ID_RUNNING = 1
        const val ACTION_STOP = "com.reviewmonitor.STOP"
        private const val TAG = "RM.Svc"

        @Volatile var instance: MonitorService? = null
            private set

        /** 从后台拉起可能被系统拒绝（Android 12+ 的后台启动限制），失败就等下一次看门狗 */
        fun start(ctx: Context): Boolean = try {
            ctx.startForegroundService(Intent(ctx, MonitorService::class.java)); true
        } catch (e: Exception) {
            Log.w(TAG, "启动服务失败: $e"); false
        }

        private const val WATCHDOG_MS = 15 * 60_000L

        /** 看门狗：每 15 分钟看一眼服务还在不在（被杀时也许恰好没有挂着的检查闹钟） */
        fun armWatchdog(ctx: Context) {
            val pi = PendingIntent.getBroadcast(
                ctx, 2, Intent(ctx, AlarmReceiver::class.java).setAction(AlarmReceiver.WATCHDOG),
                PendingIntent.FLAG_IMMUTABLE or PendingIntent.FLAG_UPDATE_CURRENT,
            )
            ctx.getSystemService(AlarmManager::class.java).setInexactRepeating(
                AlarmManager.ELAPSED_REALTIME_WAKEUP, SystemClock.elapsedRealtime() + WATCHDOG_MS, WATCHDOG_MS, pi,
            )
        }

        fun disarmWatchdog(ctx: Context) {
            val pi = PendingIntent.getBroadcast(
                ctx, 2, Intent(ctx, AlarmReceiver::class.java).setAction(AlarmReceiver.WATCHDOG),
                PendingIntent.FLAG_IMMUTABLE or PendingIntent.FLAG_NO_CREATE,
            ) ?: return
            ctx.getSystemService(AlarmManager::class.java).cancel(pi)
        }
    }

    inner class LocalBinder : Binder() { val service get() = this@MonitorService }
    private val binder = LocalBinder()
    private val main = Handler(Looper.getMainLooper())

    lateinit var web: WebHost
        private set
    var engine: Engine? = null
        private set
    val ui = MutableStateFlow(UiState())

    private lateinit var wake: PowerManager.WakeLock
    private var alarmAt = 0L
    private var lastOngoing = ""
    private var stopped = false
    private lateinit var rounds: RoundLog

    override fun onCreate() {
        super.onCreate()
        instance = this
        createChannels(this)
        ServiceCompat.startForeground(this, ID_RUNNING, runningNotification("正在启动…"), ServiceInfo.FOREGROUND_SERVICE_TYPE_SPECIAL_USE)
        wake = getSystemService(PowerManager::class.java)
            .newWakeLock(PowerManager.PARTIAL_WAKE_LOCK, "review-monitor:round")
            .apply { setReferenceCounted(false) }
        rounds = RoundLog(java.io.File(filesDir, "rounds.log"))
        web = WebHost(this, startUrl()) { json -> engine?.onMessage(json) }
        try {
            engine = Engine(filesDir.absolutePath, EngineHost())
        } catch (e: Throwable) {
            Log.e(TAG, "Engine 启动失败", e)
            ui.value = UiState(fatal = "启动失败：${e.message}")
        }
        main.post(ticker)
        armWatchdog(this)
    }

    override fun onStartCommand(intent: Intent?, flags: Int, startId: Int): Int {
        if (intent?.action == ACTION_STOP) {
            stopAll()
            return START_NOT_STICKY
        }
        return START_STICKY
    }

    override fun onBind(intent: Intent?): IBinder = binder

    override fun onDestroy() {
        main.removeCallbacks(ticker)
        cancelAlarm()
        if (wake.isHeld) wake.release()
        web.destroy()
        engine?.close()
        engine = null
        instance = null
        super.onDestroy()
    }

    /** 「停止监控并退出」：不再自动重启 */
    fun stopAll() {
        stopped = true
        disarmWatchdog(this)
        ServiceCompat.stopForeground(this, ServiceCompat.STOP_FOREGROUND_REMOVE)
        stopSelf()
    }

    // ---------------- 驱动 ----------------

    private val ticker = object : Runnable {
        override fun run() {
            if (stopped) return
            try { onTick() } catch (e: Throwable) { Log.e(TAG, "tick 出错", e) }
            main.postDelayed(this, 1000)
        }
    }

    private fun onTick() {
        val e = engine ?: return
        e.tick()
        val st = e.status()
        keepAwake(st)
        e.takeListDirty()
        e.takeAttention() // 掉线：core 已经发了「需要登录」通知，界面打开时会直接显示登录页
        if (st.login == 2) rounds.markLoggedOut(System.currentTimeMillis())
        else if (st.lastOkMs > 0) rounds.add(st.lastOkMs)
        ui.value = ui.value.copy(status = st, keep = rounds.stats(System.currentTimeMillis()))
        updateOngoing(st)
    }

    /** 「退出登录」：先让监控暂停（之后到的回音一律丢掉），再清登录状态 */
    fun logout() {
        engine?.logout()
        web.clearLogin()
        main.removeCallbacks(ticker)
        main.post(ticker)
    }

    fun checkNow() {
        engine?.checkNow()
        main.removeCallbacks(ticker)
        main.post(ticker)
    }

    private fun keepAwake(st: StatusInfo) {
        val next = st.nextInSecs
        // 快到点了或正在查：撑住 CPU 别睡
        if (st.busy || next in 0..15) wake.acquire(60_000)
        else if (wake.isHeld) wake.release()
        // 下一轮的闹钟（深睡时靠它叫醒）
        if (next > 15) {
            val at = SystemClock.elapsedRealtime() + next * 1000 - 5_000
            if (abs(at - alarmAt) > 10_000) scheduleAlarm(at)
        } else if (next < 0) {
            cancelAlarm()
        }
    }

    private fun alarmIntent() = PendingIntent.getBroadcast(
        this, 1, Intent(this, AlarmReceiver::class.java),
        PendingIntent.FLAG_IMMUTABLE or PendingIntent.FLAG_UPDATE_CURRENT,
    )

    private fun scheduleAlarm(at: Long) {
        val am = getSystemService(AlarmManager::class.java)
        alarmAt = at
        if (Build.VERSION.SDK_INT < 31 || am.canScheduleExactAlarms()) {
            am.setExactAndAllowWhileIdle(AlarmManager.ELAPSED_REALTIME_WAKEUP, at, alarmIntent())
        } else {
            am.setAndAllowWhileIdle(AlarmManager.ELAPSED_REALTIME_WAKEUP, at, alarmIntent())
        }
    }

    private fun cancelAlarm() {
        if (alarmAt == 0L) return
        getSystemService(AlarmManager::class.java).cancel(alarmIntent())
        alarmAt = 0
    }

    fun onAlarm() {
        Log.i(TAG, "闹钟叫醒")
        alarmAt = 0
        wake.acquire(60_000)
        main.removeCallbacks(ticker)
        main.post(ticker)
    }

    // ---------------- 通知 ----------------

    private fun updateOngoing(st: StatusInfo) {
        val text = when (st.login) {
            2 -> "需要登录：点开应用登录美团经营宝"
            else -> listOf(st.message.ifEmpty { "运行中" }, st.lastOk).filter { it.isNotEmpty() }.joinToString(" · ")
        }
        if (text == lastOngoing) return
        lastOngoing = text
        NotificationManagerCompat.from(this).let {
            if (it.areNotificationsEnabled()) runCatching { it.notify(ID_RUNNING, runningNotification(text)) }
        }
    }

    fun runningNotification(text: String): Notification {
        val open = PendingIntent.getActivity(this, 0, Intent(this, MainActivity::class.java),
            PendingIntent.FLAG_IMMUTABLE or PendingIntent.FLAG_UPDATE_CURRENT)
        return NotificationCompat.Builder(this, CH_RUNNING)
            .setSmallIcon(R.drawable.ic_stat)
            .setContentTitle("差评监控运行中")
            .setContentText(text)
            .setOngoing(true)
            .setOnlyAlertOnce(true)
            .setContentIntent(open)
            .setPriority(NotificationCompat.PRIORITY_LOW)
            .build()
    }

    /** rm-core 回调到这里。都在主线程、Engine 持锁时调用，不要在里面回调 Engine */
    private inner class EngineHost : Host {
        override fun evalJs(js: String) { web.eval(js) }
        override fun gotoUrl(url: String) { web.goto(url) }
        override fun userVisible(): Boolean = web.userVisible
        override fun notify(alert: AlertInfo): Boolean = showAlert(this@MonitorService, alert)
    }
}

/** 新差评 / 掉线提醒。点开进入应用，新差评会定位到那一条 */
fun showAlert(ctx: Context, a: AlertInfo): Boolean {
    val nm = NotificationManagerCompat.from(ctx)
    if (!nm.areNotificationsEnabled()) return false
    val id = if (a.reviewKey.isNotEmpty()) a.reviewKey.hashCode() else a.kind.hashCode()
    val intent = Intent(Intent.ACTION_VIEW, android.net.Uri.parse(a.launchUri), ctx, MainActivity::class.java)
        .addFlags(Intent.FLAG_ACTIVITY_NEW_TASK or Intent.FLAG_ACTIVITY_SINGLE_TOP)
    val pi = PendingIntent.getActivity(ctx, id, intent, PendingIntent.FLAG_IMMUTABLE or PendingIntent.FLAG_UPDATE_CURRENT)
    val n = NotificationCompat.Builder(ctx, MonitorService.CH_ALERTS)
        .setSmallIcon(R.drawable.ic_stat)
        .setColor(0xFFFF5F57.toInt())
        .setContentTitle(a.title)
        .setContentText(a.body)
        .setStyle(NotificationCompat.BigTextStyle().bigText(a.body))
        .setAutoCancel(true)
        .setOnlyAlertOnce(true) // 同一条评价万一重发，只替换不再响
        .setContentIntent(pi)
        .setPriority(NotificationCompat.PRIORITY_HIGH)
        .setCategory(NotificationCompat.CATEGORY_MESSAGE)
        .build()
    return try { nm.notify(id, n); true } catch (e: SecurityException) { false }
}

fun createChannels(ctx: Context) {
    val nm = ctx.getSystemService(NotificationManager::class.java)
    nm.createNotificationChannel(NotificationChannel(MonitorService.CH_RUNNING, "后台运行状态", NotificationManager.IMPORTANCE_LOW).apply {
        description = "常驻通知，表示监控正在运行"
        setShowBadge(false)
    })
    nm.createNotificationChannel(NotificationChannel(MonitorService.CH_ALERTS, "新差评提醒", NotificationManager.IMPORTANCE_HIGH).apply {
        description = "出现新差评、需要重新登录时提醒"
    })
}

/** 每轮检查前的闹钟 + 看门狗 */
class AlarmReceiver : BroadcastReceiver() {
    companion object { const val WATCHDOG = "com.reviewmonitor.WATCHDOG" }
    override fun onReceive(ctx: Context, intent: Intent) {
        val s = MonitorService.instance
        when {
            s == null -> { Log.i("RM.Svc", "服务不在，重新拉起（${intent.action ?: "检查闹钟"}）"); MonitorService.start(ctx) }
            intent.action != WATCHDOG -> s.onAlarm()
        }
    }
}
