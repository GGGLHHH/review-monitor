package com.reviewmonitor

import android.Manifest
import android.app.AlarmManager
import android.content.ComponentName
import android.content.Context
import android.content.Intent
import android.content.ServiceConnection
import android.content.pm.PackageManager
import android.net.Uri
import android.os.Build
import android.os.Bundle
import android.os.IBinder
import android.os.PowerManager
import android.provider.Settings
import android.widget.FrameLayout
import androidx.activity.ComponentActivity
import androidx.activity.SystemBarStyle
import androidx.activity.compose.setContent
import androidx.activity.enableEdgeToEdge
import androidx.activity.result.contract.ActivityResultContracts
import androidx.compose.foundation.background
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.systemBarsPadding
import androidx.compose.runtime.collectAsState
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.setValue
import androidx.compose.ui.Modifier
import androidx.compose.ui.graphics.toArgb
import androidx.core.app.NotificationManagerCompat
import com.reviewmonitor.ui.Actions
import com.reviewmonitor.ui.AppScreen
import com.reviewmonitor.ui.C
import com.reviewmonitor.ui.KeepAlive
import kotlinx.coroutines.flow.MutableStateFlow

class MainActivity : ComponentActivity() {
    private var service by mutableStateOf<MonitorService?>(null)
    private var keep by mutableStateOf(KeepAlive(true, true, null, false))
    private var manualLogin by mutableStateOf(false)
    private var webShown = false
    private var resumed = false

    private val conn = object : ServiceConnection {
        override fun onServiceConnected(name: ComponentName?, b: IBinder?) {
            service = (b as MonitorService.LocalBinder).service
        }
        override fun onServiceDisconnected(name: ComponentName?) { service = null }
    }

    private val askNotifyPerm = registerForActivityResult(ActivityResultContracts.RequestPermission()) { refreshKeep() }

    override fun onCreate(savedInstanceState: Bundle?) {
        enableEdgeToEdge(SystemBarStyle.dark(C.panel.toArgb()), SystemBarStyle.dark(C.panel.toArgb()))
        super.onCreate(savedInstanceState)
        if (Build.VERSION.SDK_INT >= 33 && checkSelfPermission(Manifest.permission.POST_NOTIFICATIONS) != PackageManager.PERMISSION_GRANTED) {
            askNotifyPerm.launch(Manifest.permission.POST_NOTIFICATIONS)
        }
        MonitorService.start(this)
        val empty = MutableStateFlow(UiState())
        setContent {
            val st by (service?.ui ?: empty).collectAsState()
            Box(Modifier.fillMaxSize().background(C.panel).systemBarsPadding()) {
                AppScreen(
                    svc = service, st = st, keep = keep,
                    actions = Actions(
                        askNotify = ::askNotify,
                        askBattery = ::askBattery,
                        askExactAlarm = ::askExactAlarm,
                        openVendorPower = ::openVendorPower,
                        openAppSettings = ::openAppDetails,
                        quit = ::quit,
                        logout = { service?.logout() },
                    ),
                    manualLogin = manualLogin,
                    setManualLogin = { manualLogin = it },
                    attachWeb = ::attachWeb,
                    detachWeb = ::detachWeb,
                )
            }
        }
    }

    // ---------------- 登录页挂载 ----------------

    private fun attachWeb(parent: FrameLayout) {
        val s = service ?: return
        s.web.attach(this, parent)
        webShown = true
        s.web.userVisible = resumed
    }

    private fun detachWeb() {
        webShown = false
        service?.web?.let { it.userVisible = false; it.detach() }
    }

    override fun onStart() {
        super.onStart()
        bindService(Intent(this, MonitorService::class.java), conn, Context.BIND_AUTO_CREATE)
    }

    override fun onResume() {
        super.onResume()
        resumed = true
        service?.web?.userVisible = webShown
        refreshKeep()
    }

    override fun onPause() {
        resumed = false
        service?.web?.userVisible = false
        super.onPause()
    }

    override fun onStop() {
        unbindService(conn)
        super.onStop()
    }

    override fun onDestroy() {
        detachWeb()
        super.onDestroy()
    }

    // ---------------- 保活引导 ----------------

    private fun refreshKeep() {
        val pm = getSystemService(PowerManager::class.java)
        val am = getSystemService(AlarmManager::class.java)
        keep = KeepAlive(
            notifyOk = NotificationManagerCompat.from(this).areNotificationsEnabled(),
            batteryOk = pm.isIgnoringBatteryOptimizations(packageName),
            exactAlarmOk = if (Build.VERSION.SDK_INT >= 31) am.canScheduleExactAlarms() else null,
            isMeizu = Build.MANUFACTURER.equals("meizu", ignoreCase = true),
        )
    }

    private fun askNotify() {
        if (Build.VERSION.SDK_INT >= 33 && checkSelfPermission(Manifest.permission.POST_NOTIFICATIONS) != PackageManager.PERMISSION_GRANTED
            && shouldShowRequestPermissionRationale(Manifest.permission.POST_NOTIFICATIONS)) {
            askNotifyPerm.launch(Manifest.permission.POST_NOTIFICATIONS)
            return
        }
        tryStart(Intent(Settings.ACTION_APP_NOTIFICATION_SETTINGS).putExtra(Settings.EXTRA_APP_PACKAGE, packageName))
    }

    private fun askBattery() {
        tryStart(Intent(Settings.ACTION_REQUEST_IGNORE_BATTERY_OPTIMIZATIONS, Uri.parse("package:$packageName")))
    }

    private fun askExactAlarm() {
        if (Build.VERSION.SDK_INT >= 31) tryStart(Intent(Settings.ACTION_REQUEST_SCHEDULE_EXACT_ALARM, Uri.parse("package:$packageName")))
    }

    /** 魅族：手机管家。找不到就退回应用信息页 */
    private fun openVendorPower() {
        val tries = listOf(
            Intent("com.meizu.safe.security.SHOW_APPSEC").addCategory(Intent.CATEGORY_DEFAULT).putExtra("packageName", packageName),
            packageManager.getLaunchIntentForPackage("com.meizu.safe"),
        )
        for (i in tries) if (i != null && tryStart(i)) return
        openAppDetails()
    }

    private fun openAppDetails() {
        tryStart(Intent(Settings.ACTION_APPLICATION_DETAILS_SETTINGS, Uri.parse("package:$packageName")))
    }

    private fun tryStart(i: Intent): Boolean = try { startActivity(i); true } catch (_: Exception) { false }

    private fun quit() {
        service?.stopAll()
        finishAndRemoveTask()
    }
}
