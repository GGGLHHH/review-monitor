package com.reviewmonitor.ui

import android.widget.FrameLayout
import androidx.compose.foundation.background
import androidx.compose.foundation.clickable
import androidx.compose.foundation.layout.imePadding
import androidx.compose.runtime.DisposableEffect
import androidx.compose.runtime.LaunchedEffect
import androidx.compose.runtime.collectAsState
import androidx.compose.runtime.key
import androidx.compose.ui.platform.LocalContext
import com.reviewmonitor.WebHost
import kotlinx.coroutines.delay
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.Spacer
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.width
import androidx.compose.foundation.rememberScrollState
import androidx.compose.foundation.verticalScroll
import androidx.compose.material3.AlertDialog
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.text.font.FontWeight
import androidx.compose.ui.text.style.TextAlign
import androidx.compose.ui.unit.dp
import androidx.compose.ui.unit.sp
import androidx.compose.ui.viewinterop.AndroidView
import com.reviewmonitor.MonitorService
import com.reviewmonitor.RoundStats
import com.reviewmonitor.UiState
import com.reviewmonitor.core.Settings
import com.reviewmonitor.core.defaultSettings
import com.reviewmonitor.core.startUrl
import com.reviewmonitor.core.validateSettings

/** 保持后台运行要检查的几项，界面每次回到前台时重新读 */
data class KeepAlive(
    val notifyOk: Boolean,
    val batteryOk: Boolean,
    /** null：系统版本不需要 */
    val exactAlarmOk: Boolean?,
    val isMeizu: Boolean,
)

class Actions(
    val askNotify: () -> Unit,
    val askBattery: () -> Unit,
    val askExactAlarm: () -> Unit,
    val openVendorPower: () -> Unit,
    val openAppSettings: () -> Unit,
    val quit: () -> Unit,
    val logout: () -> Unit,
)

/** 两页：评价（按网页版筛选直接查）、监控（状态 → 保持后台运行 → 参数）。需要登录时整页换成登录页 */
@Composable
fun AppScreen(
    svc: MonitorService?,
    st: UiState,
    keep: KeepAlive,
    actions: Actions,
    attachWeb: (FrameLayout) -> Unit,
    detachWeb: () -> Unit,
    focus: Focus? = null,
) {
    val needLogin = st.status?.login == 2
    val web = svc?.webFlow?.collectAsState()?.value
    Column(Modifier.fillMaxSize().background(C.bg)) {
        when {
            svc == null -> MainPane(svc, st, keep, actions)
            // 登录网页每次需要登录时新建一个：换了实例就整页重来，重新挂载
            needLogin && web != null -> key(web) { LoginPane(svc, web, attachWeb, detachWeb) }
            needLogin -> Column(Modifier.fillMaxSize(), verticalArrangement = Arrangement.Center, horizontalAlignment = Alignment.CenterHorizontally) {
                Text("正在打开登录页…", color = C.sub, fontSize = 14.sp)
            }
            else -> {
                var tab by androidx.compose.runtime.saveable.rememberSaveable { mutableStateOf(0) }
                // 点差评通知进来：切到评价页
                LaunchedEffect(focus) { if (focus != null) tab = 0 }
                Row(Modifier.fillMaxWidth().background(C.panel).padding(horizontal = 16.dp, vertical = 10.dp), verticalAlignment = Alignment.CenterVertically) {
                    Seg(listOf("评价", "监控"), tab) { tab = it }
                    Spacer(Modifier.weight(1f))
                    val s = st.status
                    Dot(if (s?.login == 1) C.good else C.faint, 8)
                    Spacer(Modifier.width(6.dp))
                    Text(if (s?.busy == true) "检查中" else if (s?.login == 1) "监控中" else "检测中", color = C.sub, fontSize = 12.sp)
                }
                Box(Modifier.weight(1f)) {
                    if (tab == 0) ReviewsPane(svc.engine, focus) else MainPane(svc, st, keep, actions)
                }
            }
        }
    }
}

// ---------------- 登录（只在需要登录时出现）----------------

/**
 * 应用自己的登录页：账号密码在这里输入，由 login.js 填进美团的表单并提交。
 * 美团的安全验证（滑块）出现时，把登录网页露出来让用户自己拖；验证不代做。
 * 表单迟迟不出现（改版、要短信验证等）时，可以改用完整网页登录。
 * 进到经营宝后台后网页把 cookie 交给 Engine，确认登录成功后服务会关掉网页，这一页也就消失了。
 */
@Composable
private fun LoginPane(svc: MonitorService, web: WebHost, attach: (FrameLayout) -> Unit, detach: () -> Unit) {
    val form by web.loginForm.collectAsState()
    val prefs = LocalContext.current.getSharedPreferences("login", android.content.Context.MODE_PRIVATE)
    var user by remember { mutableStateOf(prefs.getString("user", "") ?: "") }
    var pass by remember { mutableStateOf("") }
    var agree by remember { mutableStateOf(false) }
    var showPass by remember { mutableStateOf(false) }
    var useWeb by remember { mutableStateOf(false) }
    /** 点了登录、还没出结果 */
    var waiting by remember { mutableStateOf(false) }
    var msg by remember { mutableStateOf("") }
    var slow by remember { mutableStateOf(false) }
    /** 提交过、并且见过美团的验证（滑块 / 短信）：之后表单消失就说明验证通过、页面正在跳转 */
    var verified by remember { mutableStateOf(false) }
    if (waiting && (form.captcha || form.sms)) verified = true
    val entering = (verified || waiting) && !form.ready && !form.captcha && !form.sms

    // 表单 12 秒还没准备好：提示可以改用网页
    LaunchedEffect(form.ready) { slow = false; if (!form.ready) { delay(12_000); slow = true } }
    // 提交后：美团报错就显示出来；20 秒没有任何结果也停止等待
    LaunchedEffect(waiting, form.error, form.captcha) {
        if (!waiting) return@LaunchedEffect
        if (form.error.isNotEmpty() && !form.captcha) { msg = form.error; waiting = false; return@LaunchedEffect }
        if (!form.captcha && !form.sms && !entering) { delay(20_000); msg = "没有等到登录结果，可以再试一次，或改用网页登录"; waiting = false }
    }
    // 验证通过后页面在跳转：每 5 秒催一次登录探测，别让用户干等；又回到登录表单说明没成功
    LaunchedEffect(entering) {
        if (!entering) return@LaunchedEffect
        repeat(18) { delay(5_000); svc.checkNow() }
        verified = false; waiting = false; msg = "登录没有完成，请再试一次，或改用网页登录"
    }
    LaunchedEffect(form.ready) { if (form.ready && verified && !form.captcha && !form.sms) { verified = false; waiting = false } }

    if (useWeb) {
        WebPane("网页登录", "这是美团经营宝的登录网页，滑块需要你自己拖。登录成功后自动收起。", "返回", { useWeb = false }, { web.goto(startUrl()) }, attach, detach)
        return
    }
    if (form.captcha) {
        // 滑块：露出美团的验证本身（页面其余部分用样式遮掉），用户自己拖
        DisposableEffect(Unit) { web.focusCaptcha(true); onDispose { web.focusCaptcha(false) } }
        WebPane("安全验证", "请拖动下面的滑块，完成美团的安全验证。完成后自动继续。", null, {}, null, attach, detach)
        return
    }
    if (form.sms) {
        SmsPane(web, form, onUseWeb = { useWeb = true })
        return
    }
    if (entering) {
        Column(Modifier.fillMaxSize().padding(24.dp), verticalArrangement = Arrangement.Center, horizontalAlignment = Alignment.CenterHorizontally) {
            Text("验证通过", color = C.text, fontSize = 22.sp, fontWeight = FontWeight.Bold)
            VSpace(10)
            Text("正在进入经营宝后台，通常需要十几秒…", color = C.sub, fontSize = 14.sp)
        }
        return
    }

    Column(
        Modifier.fillMaxSize().verticalScroll(rememberScrollState()).imePadding().padding(horizontal = 24.dp, vertical = 32.dp),
        verticalArrangement = Arrangement.spacedBy(14.dp),
    ) {
        Text("登录美团经营宝", color = C.text, fontSize = 24.sp, fontWeight = FontWeight.Bold)
        Text("用监控专用的子账号登录。登录后在后台定时检查新差评。", color = C.sub, fontSize = 13.sp, lineHeight = 19.sp)
        VSpace(6)
        Field(user, { user = it; msg = "" }, "账号", enabled = !waiting)
        Field(pass, { pass = it; msg = "" }, "密码", enabled = !waiting, password = !showPass,
            trailing = { Text(if (showPass) "隐藏" else "显示", color = C.sub, fontSize = 13.sp, modifier = Modifier.clickable { showPass = !showPass }.padding(6.dp)) })
        Row(Modifier.fillMaxWidth().clickable(enabled = !waiting) { agree = !agree; msg = "" }.padding(vertical = 4.dp), verticalAlignment = Alignment.Top) {
            Check(agree)
            Spacer(Modifier.width(10.dp))
            Text("我已阅读并同意美团的《经营宝隐私政策》《经营宝商户服务协议》", color = C.sub, fontSize = 13.sp, lineHeight = 19.sp)
        }
        val can = form.ready && !waiting && user.isNotBlank() && pass.isNotEmpty() && agree
        BigButton(if (waiting) "正在登录…" else if (!form.ready) "正在连接美团…" else "登录", enabled = can) {
            prefs.edit().putString("user", user.trim()).apply()
            msg = ""
            if (web.submitLogin(user.trim(), pass)) waiting = true
            else msg = "登录页还没准备好，请稍等几秒再试"
        }
        if (msg.isNotEmpty()) Note(msg, C.bad)
        if (!form.ready && slow) Note("一直连不上美团的登录页：检查网络，或改用网页登录。", C.warn)
        Text("下一步可能出现滑块验证，需要你自己拖。密码只用于这次登录，不会保存。", color = C.faint, fontSize = 12.sp, lineHeight = 17.sp)
        Row(Modifier.fillMaxWidth(), horizontalArrangement = Arrangement.Center) {
            Text("改用网页登录", color = C.sub, fontSize = 13.sp, modifier = Modifier.clickable { useWeb = true }.padding(10.dp))
        }
    }
}

/** 短信验证：美团在新设备登录时要求。按钮和输入都在应用里，由 login.js 照着操作美团的页面 */
@Composable
private fun SmsPane(web: WebHost, form: com.reviewmonitor.LoginForm, onUseWeb: () -> Unit) {
    var code by remember { mutableStateOf("") }
    var sent by remember { mutableStateOf(false) }
    var checking by remember { mutableStateOf(false) }
    // 美团报错（验证码不对等）：停止等待，让用户重输
    LaunchedEffect(form.error) { if (form.error.isNotEmpty()) checking = false }
    LaunchedEffect(checking) { if (checking) { delay(15_000); checking = false } }
    Column(
        Modifier.fillMaxSize().verticalScroll(rememberScrollState()).imePadding().padding(horizontal = 24.dp, vertical = 32.dp),
        verticalArrangement = Arrangement.spacedBy(14.dp),
    ) {
        Text("短信验证", color = C.text, fontSize = 24.sp, fontWeight = FontWeight.Bold)
        Text("为保护账户安全，美团要求用短信验证码确认这次登录。", color = C.sub, fontSize = 13.sp, lineHeight = 19.sp)
        VSpace(6)
        Row(verticalAlignment = Alignment.CenterVertically) {
            Column(Modifier.weight(1f)) {
                Text("手机号", color = C.sub, fontSize = 12.sp)
                Text(form.phone.ifEmpty { "—" }, color = C.text, fontSize = 18.sp, fontWeight = FontWeight.Medium)
            }
            Pill(form.sendText.ifEmpty { "获取验证码" }, primary = !sent, enabled = form.sendEnabled) { sent = true; web.smsSend() }
        }
        Field(code, { v -> code = v.filter { it.isDigit() }.take(6) }, "验证码", enabled = !checking, numeric = true)
        BigButton(if (checking) "正在验证…" else "验证", enabled = code.length >= 4 && !checking) {
            if (web.smsVerify(code)) checking = true
        }
        if (form.error.isNotEmpty()) Note(form.error, C.bad)
        Text("点「获取验证码」后可能再出现一次滑块，需要你自己拖。验证码只用于这次验证，不会保存。", color = C.faint, fontSize = 12.sp, lineHeight = 17.sp)
        Row(Modifier.fillMaxWidth(), horizontalArrangement = Arrangement.Center) {
            Text("收不到验证码？改用网页处理", color = C.sub, fontSize = 13.sp, modifier = Modifier.clickable(onClick = onUseWeb).padding(10.dp))
        }
    }
}

/** 把登录网页露出来：安全验证、网页登录时用 */
@Composable
private fun WebPane(title: String, hint: String, closeLabel: String?, onClose: () -> Unit, onReload: (() -> Unit)?, attach: (FrameLayout) -> Unit, detach: () -> Unit) {
    Column(Modifier.fillMaxSize()) {
        Column(Modifier.fillMaxWidth().background(C.panel).padding(16.dp), verticalArrangement = Arrangement.spacedBy(8.dp)) {
            Row(verticalAlignment = Alignment.CenterVertically) {
                Text(title, color = C.text, fontSize = 17.sp, fontWeight = FontWeight.Bold, modifier = Modifier.weight(1f))
                if (onReload != null) Pill("重新载入", onClick = onReload)
                if (closeLabel != null) { Spacer(Modifier.width(8.dp)); Pill(closeLabel, primary = true, onClick = onClose) }
            }
            Text(hint, color = C.sub, fontSize = 12.sp, lineHeight = 17.sp)
        }
        AndroidView(
            factory = { ctx -> FrameLayout(ctx).also(attach) },
            onRelease = { detach() },
            modifier = Modifier.fillMaxSize(),
        )
    }
}

// ---------------- 主页 ----------------

@Composable
private fun MainPane(svc: MonitorService?, st: UiState, keep: KeepAlive, a: Actions) {
    var s by remember(svc) { mutableStateOf(svc?.engine?.settings() ?: defaultSettings()) }
    var msg by remember { mutableStateOf("") }
    var ok by remember { mutableStateOf(true) }
    fun upd(f: Settings.() -> Settings) { s = s.f(); msg = "" }
    var askLogout by remember { mutableStateOf(false) }
    if (askLogout) {
        AlertDialog(
            onDismissRequest = { askLogout = false },
            containerColor = C.panel, titleContentColor = C.text, textContentColor = C.sub,
            title = { Text("退出登录？") },
            text = { Text("退出后监控暂停，要重新登录（滑块要手动拖）才会继续。电脑上的登录不受影响。") },
            confirmButton = { Pill("退出登录", primary = true) { askLogout = false; a.logout() } },
            dismissButton = { Pill("取消") { askLogout = false } },
        )
    }

    Column(Modifier.fillMaxSize().verticalScroll(rememberScrollState()).padding(16.dp), verticalArrangement = Arrangement.spacedBy(20.dp)) {
        StatusCard(svc, st)
        st.fatal.ifEmpty { null }?.let { Note(it, C.bad) }
        st.status?.banner?.ifEmpty { null }?.let { Note(it, C.warn) }

        Section("保持后台运行" + if (keep.isMeizu) "（魅族必看）" else "") {
            SettingRow("通知权限", if (keep.notifyOk) "已开启" else "没开就收不到新差评提醒") {
                if (keep.notifyOk) Tag("已开启", C.good) else Pill("去开启", primary = true, onClick = a.askNotify)
            }
            SettingRow("忽略电池优化", if (keep.batteryOk) "已加入白名单" else "不加的话，熄屏后系统会断掉网络、推迟检查") {
                if (keep.batteryOk) Tag("已开启", C.good) else Pill("去开启", primary = true, onClick = a.askBattery)
            }
            keep.exactAlarmOk?.let { okA ->
                SettingRow("闹钟和提醒", if (okA) "已允许，熄屏后也能准点检查" else "不开的话，熄屏后检查会被推迟") {
                    if (okA) Tag("已允许", C.good) else Pill("去允许", primary = true, onClick = a.askExactAlarm)
                }
            }
            if (keep.isMeizu) {
                SettingRow("允许后台运行", "手机管家 → 权限管理 → 后台管理 → 差评监控 → 允许后台运行；再在最近任务里下拉卡片「锁定」") {
                    Pill("打开手机管家", onClick = a.openVendorPower)
                }
            } else {
                SettingRow("厂商省电设置", "部分手机还要在系统的「后台运行 / 自启动」里允许本应用") {
                    Pill("应用信息", onClick = a.openAppSettings)
                }
            }
            KeepRecord(st.keep, s.intervalMinutes.toInt())
        }

        Section("参数") {
            SettingRow("检查间隔", "每隔多久去后台看一次") {
                Stepper(s.intervalMinutes.toFloat(), 1f, 120f, 1f, "分钟") { v -> upd { copy(intervalMinutes = v.toUInt()) } }
            }
            SettingRow("美团差评阈值", "评分 ≤ 该值即算差评") {
                Stepper(s.meituanMaxStar, 0.5f, 5f, 0.5f, "星", 1) { v -> upd { copy(meituanMaxStar = v) } }
            }
            SettingRow("点评差评阈值", "点评「差评」为 ≤2，「中评」为 2.5–3.5") {
                Stepper(s.dianpingMaxStar, 0.5f, 5f, 0.5f, "星", 1) { v -> upd { copy(dianpingMaxStar = v) } }
            }
            SettingRow("监控美团评价") { Toggle(s.meituanOn) { v -> upd { copy(meituanOn = v) } } }
            SettingRow("监控点评评价") { Toggle(s.dianpingOn) { v -> upd { copy(dianpingOn = v) } } }
            SettingRow("只提醒未回复的", "已经回复过的差评不再提醒") { Toggle(s.onlyUnreplied) { v -> upd { copy(onlyUnreplied = v) } } }
            Row(Modifier.fillMaxWidth().padding(horizontal = 16.dp, vertical = 10.dp), verticalAlignment = Alignment.CenterVertically) {
                Text("保存后从下一轮开始生效", color = C.faint, fontSize = 12.sp, modifier = Modifier.weight(1f))
                Pill("恢复默认") { s = defaultSettings(); msg = "已恢复默认值，点「保存」生效"; ok = true }
                Spacer(Modifier.width(8.dp))
                Pill("保存", primary = true) {
                    // 保留界面上没露出来的高级参数（抖动、回看等）
                    val errs = validateSettings(s)
                    if (errs.isNotEmpty()) { msg = errs.joinToString("；"); ok = false; return@Pill }
                    val e = svc?.engine?.saveSettings(s) ?: "监控服务没有运行"
                    if (e.isEmpty()) { msg = "已保存"; ok = true } else { msg = e; ok = false }
                }
            }
            if (msg.isNotEmpty()) Box(Modifier.padding(start = 16.dp, end = 16.dp, bottom = 12.dp)) { Note(msg, if (ok) C.good else C.bad) }
        }

        Section("其他") {
            SettingRow("退出登录", "清掉这台手机上的登录状态，监控暂停到重新登录；已提醒过的差评不会再提醒") {
                Pill("退出登录") { askLogout = true }
            }
            SettingRow("停止监控并退出", "停掉后台检查，下次打开应用时重新开始") { Pill("退出", onClick = a.quit) }
        }
        val ctx = LocalContext.current
        val version = remember { runCatching { ctx.packageManager.getPackageInfo(ctx.packageName, 0).versionName }.getOrNull() ?: "" }
        Text("数据只存在这台手机上，不上传任何服务器。\n版本 $version", color = C.faint, fontSize = 12.sp, lineHeight = 18.sp, textAlign = TextAlign.Center, modifier = Modifier.fillMaxWidth())
    }
}

@Composable
private fun StatusCard(svc: MonitorService?, st: UiState) {
    val s = st.status
    val (dot, label) = when (s?.login) {
        1 -> C.good to "已登录，监控中"
        2 -> C.bad to "需要登录"
        else -> C.faint to "检测中"
    }
    Section("状态") {
        Column(Modifier.fillMaxWidth().padding(horizontal = 16.dp, vertical = 12.dp), verticalArrangement = Arrangement.spacedBy(6.dp)) {
            Row(verticalAlignment = Alignment.CenterVertically) {
                Dot(dot, 10)
                Spacer(Modifier.width(8.dp))
                Text(label, color = C.text, fontSize = 17.sp, fontWeight = FontWeight.Bold, modifier = Modifier.weight(1f))
                Pill("立即检查", primary = true, enabled = s?.login == 1 && !s.busy) { svc?.checkNow() }
            }
            Text(s?.message?.ifEmpty { null } ?: "启动中…", color = C.sub, fontSize = 13.sp, lineHeight = 18.sp)
            val next = s?.nextInSecs ?: -1
            val tail = listOfNotNull(
                s?.lastOk?.ifEmpty { null },
                if (s?.busy == true) "正在检查…" else if (next >= 0) "${next / 60} 分 ${next % 60} 秒后再次检查" else null,
            ).joinToString(" · ")
            if (tail.isNotEmpty()) Text(tail, color = C.faint, fontSize = 12.sp)
        }
    }
}

/** 保活是否真的有效，看这一行就知道：最长间隔明显超过检查间隔，说明被系统停过 */
@Composable
private fun KeepRecord(r: RoundStats, interval: Int) {
    val expectMax = interval * 2 + 2
    val good = r.rounds24h > 0 && r.maxGapMin <= expectMax
    val hint = when {
        r.rounds24h == 0 -> "还没有成功检查过"
        else -> "最近 ${fmtMin(r.coveredMin)} 里成功检查 ${r.rounds24h} 轮，最长间隔 ${fmtMin(r.maxGapMin)}" +
            if (good) "" else "（超过 ${expectMax} 分钟，说明被系统暂停过）"
    }
    SettingRow("保活记录", hint) { Tag(if (r.rounds24h == 0) "—" else if (good) "正常" else "有中断", if (r.rounds24h == 0) C.faint else if (good) C.good else C.warn) }
}

private fun fmtMin(m: Int) = if (m < 60) "$m 分钟" else "${m / 60} 小时 ${m % 60} 分"
