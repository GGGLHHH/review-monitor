package com.reviewmonitor

import android.annotation.SuppressLint
import android.content.Context
import android.content.MutableContextWrapper
import android.os.Handler
import android.os.Looper
import android.util.Log
import android.view.ViewGroup
import android.webkit.CookieManager
import android.webkit.JavascriptInterface
import android.webkit.WebSettings
import android.webkit.WebStorage
import android.webkit.WebView
import android.webkit.WebViewClient
import androidx.webkit.JavaScriptReplyProxy
import androidx.webkit.WebViewCompat
import androidx.webkit.WebViewFeature
import kotlinx.coroutines.flow.MutableStateFlow
import org.json.JSONObject

/** 美团登录表单的状态（由 login.js 上报）。ready：表单已出现，可以代填；captcha：安全验证（滑块）正显示着，要用户自己拖 */
data class LoginForm(
    val ready: Boolean = false, val captcha: Boolean = false, val error: String = "",
    /** 短信验证正等着：手机号（打码）、「获取验证码」按钮上的字（倒计时也在这）和能不能点 */
    val sms: Boolean = false, val phone: String = "", val sendText: String = "", val sendEnabled: Boolean = false,
)

/**
 * 后台页面：一个常驻的 WebView，由前台服务持有。
 * 平时不显示，只用来在登录态下请求接口；掉线需要登录时才挂到界面上让用户登录，登录完摘下来，页面一直活着。
 * 和电脑版共用同一份 web/poller.js：脚本通过 window.ipc.postMessage(String) 回传，这里用 addJavascriptInterface 提供同名接口。
 */
@SuppressLint("SetJavaScriptEnabled")
class WebHost(app: Context, private val startUrl: String, private val onMessage: (String) -> Unit) {
    companion object {
        private const val TAG = "RM.Web"
        // 不在界面上时，按桌面窗口大小排版，后台页的 iframe 才会正常加载
        private const val OFF_W = 1280
        private const val OFF_H = 900
        private val LOGIN_ORIGIN = setOf("https://epassport.meituan.com")
    }

    private val appCtx = app.applicationContext
    private val ctx = MutableContextWrapper(appCtx)
    private val main = Handler(Looper.getMainLooper())
    private val pollerJs: String = appCtx.assets.open("poller.js").bufferedReader().use { it.readText() }
    private val loginJs: String = appCtx.assets.open("login.js").bufferedReader().use { it.readText() }

    /** 应用自己的登录页靠它知道：能不能代填、要不要把滑块露出来 */
    val loginForm = MutableStateFlow(LoginForm())
    private var loginProxy: JavaScriptReplyProxy? = null

    /** 界面是否正显示着后台页（和电脑版 user_visible 同义：用户在看时不要自动刷新页面） */
    @Volatile var userVisible = false

    val webView: WebView = WebView(ctx).apply {
        settings.apply {
            javaScriptEnabled = true
            domStorageEnabled = true
            useWideViewPort = true
            loadWithOverviewMode = true
            setSupportZoom(true)
            builtInZoomControls = true
            displayZoomControls = false
            mixedContentMode = WebSettings.MIXED_CONTENT_NEVER_ALLOW
            // 经营宝网页版是桌面站：用桌面 Chrome 的 UA，版本号沿用本机 WebView 的
            val chrome = Regex("Chrome/[\\d.]+").find(WebSettings.getDefaultUserAgent(appCtx))?.value ?: "Chrome/140.0.0.0"
            userAgentString = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) $chrome Safari/537.36"
        }
        CookieManager.getInstance().setAcceptCookie(true)
        CookieManager.getInstance().setAcceptThirdPartyCookies(this, true)
        addJavascriptInterface(Bridge(), "ipc")
        webViewClient = object : WebViewClient() {
            override fun onPageStarted(view: WebView, url: String?, favicon: android.graphics.Bitmap?) {
                // 换页了：旧表单作废，等新页面里的 login.js 重新上报
                loginProxy = null
                loginForm.value = LoginForm()
                if (!docStartSupported) view.evaluateJavascript(pollerJs, null)
            }
            override fun onPageFinished(view: WebView, url: String?) {
                if (!docStartSupported) view.evaluateJavascript(pollerJs, null)
                CookieManager.getInstance().flush()
            }
        }
    }

    private val docStartSupported = WebViewFeature.isFeatureSupported(WebViewFeature.DOCUMENT_START_SCRIPT)

    init {
        if (docStartSupported) {
            WebViewCompat.addDocumentStartJavaScript(webView, pollerJs, setOf("https://ecom.meituan.com"))
        } else {
            Log.w(TAG, "WebView 不支持 DOCUMENT_START_SCRIPT，改为页面加载时注入")
        }
        // 登录表单在 epassport 的跨域帧里：只给这个来源注入 login.js，也只接受它发来的消息
        if (docStartSupported && WebViewFeature.isFeatureSupported(WebViewFeature.WEB_MESSAGE_LISTENER)) {
            WebViewCompat.addWebMessageListener(webView, "rmLogin", LOGIN_ORIGIN) { _, message, _, _, reply ->
                val o = try { JSONObject(message.data ?: return@addWebMessageListener) } catch (_: Exception) { return@addWebMessageListener }
                if (o.optString("t") == "state") {
                    loginProxy = reply
                    loginForm.value = LoginForm(o.optBoolean("form"), o.optBoolean("captcha"), o.optString("error"),
                        o.optBoolean("sms"), o.optString("phone"), o.optString("sendText"), o.optBoolean("sendEnabled"))
                }
            }
            WebViewCompat.addDocumentStartJavaScript(webView, loginJs, LOGIN_ORIGIN)
        } else {
            Log.w(TAG, "WebView 不支持消息通道，登录只能用网页")
        }
        layoutOffscreen()
        webView.loadUrl(startUrl)
    }

    /** 把用户在应用登录页输入的账号密码填进美团的表单并提交（只在内存里经过，不保存）。表单不在时返回 false */
    fun submitLogin(user: String, pass: String): Boolean {
        if (!loginForm.value.ready) return false
        return loginCmd(JSONObject().put("t", "submit").put("user", user).put("pass", pass))
    }

    /** 短信验证：替用户点美团页面上的「获取验证码」 */
    fun smsSend() = loginCmd(JSONObject().put("t", "smsSend"))

    /** 短信验证：把用户输入的验证码填进美团页面并点「验证」 */
    fun smsVerify(code: String) = loginCmd(JSONObject().put("t", "smsVerify").put("code", code))

    private fun loginCmd(o: JSONObject): Boolean {
        val p = loginProxy ?: return false
        return try { p.postMessage(o.toString()); true } catch (e: Exception) { Log.w(TAG, "登录页指令失败", e); false }
    }

    /**
     * 露出页面做滑块验证时：让登录表单所在的帧铺满整个页面、背景换成深色，配合 login.js 里的不透明遮罩，
     * 屏幕上只剩验证弹窗。只改外观，验证本身仍是美团原样的，由用户自己拖。on=false 还原。
     */
    fun focusCaptcha(on: Boolean) = eval(
        "(function(){var s=document.getElementById('rm-focus');" +
            "if(!$on){if(s)s.remove();return;}" +
            "if(s||!document.head)return;s=document.createElement('style');s.id='rm-focus';" +
            "s.textContent='html,body{background:#14161B!important;overflow:hidden!important}" +
            "iframe[src*=\"epassport.meituan.com\"]{position:fixed!important;left:0!important;top:0!important;width:100vw!important;height:100vh!important;z-index:2147483000!important;border:0!important;margin:0!important;max-width:none!important;max-height:none!important;background:#14161B!important}.loginBox{background:transparent!important}';" +
            "document.head.appendChild(s);})()"
    )

    private fun layoutOffscreen() {
        val ms = { n: Int -> android.view.View.MeasureSpec.makeMeasureSpec(n, android.view.View.MeasureSpec.EXACTLY) }
        webView.measure(ms(OFF_W), ms(OFF_H))
        webView.layout(0, 0, OFF_W, OFF_H)
    }

    private inner class Bridge {
        // 在 JavaBridge 线程上被调用，转到主线程处理
        @JavascriptInterface
        fun postMessage(s: String) { main.post { onMessage(s) } }
    }

    fun eval(js: String) = main.post { webView.evaluateJavascript(js, null) }

    fun goto(url: String) = main.post { webView.loadUrl(url) }

    /** 挂到界面上（传 Activity 作为 Context，滑块等交互才正常） */
    fun attach(activity: Context, parent: ViewGroup) {
        (webView.parent as? ViewGroup)?.removeView(webView)
        ctx.baseContext = activity
        parent.addView(webView, ViewGroup.LayoutParams(ViewGroup.LayoutParams.MATCH_PARENT, ViewGroup.LayoutParams.MATCH_PARENT))
        webView.onResume()
        webView.resumeTimers()
    }

    /** 从界面上摘下来，页面和 JS 继续在服务里跑 */
    fun detach() {
        (webView.parent as? ViewGroup)?.removeView(webView)
        ctx.baseContext = appCtx
        // 注意：不调用 onPause / pauseTimers，否则后台取数会停
        webView.resumeTimers()
        layoutOffscreen()
    }

    /** 退出登录：清掉这台手机上的登录状态（cookie、页面存储、缓存），再打开起始页（会跳到登录页）。不调用美团的退出接口，不影响电脑上的登录 */
    fun clearLogin() = main.post {
        val cm = CookieManager.getInstance()
        cm.removeAllCookies {
            cm.flush()
            WebStorage.getInstance().deleteAllData()
            webView.clearCache(true)
            webView.clearHistory()
            webView.loadUrl(startUrl)
            Log.i(TAG, "已清除登录状态")
        }
    }

    fun destroy() {
        (webView.parent as? ViewGroup)?.removeView(webView)
        webView.destroy()
    }
}
