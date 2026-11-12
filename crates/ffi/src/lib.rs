//! rm-core 的 UniFFI 门面：给安卓（Kotlin）用。
//!
//! Kotlin 提供一个 [`Host`]（在 WebView 里执行 JS、跳转、发系统通知），
//! 拿到一个 [`Engine`]，在主线程上定时调 `tick()`，把 poller.js 的回传交给 `on_message()`。
//! 业务逻辑全部在 rm-core 里，和电脑版完全同一套。
use chrono::Local;
use rm_core::activate::{self, Activation};
use rm_core::config::Config;
use rm_core::notify::{fmt_star, Alert, AlertKind, Dispatcher, Notifier};
use rm_core::poller::{self, IpcMsg, Login, Page, Poller};
use rm_core::store::Store;
use rm_core::Platform;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, Once};
use std::time::Instant;

uniffi::setup_scaffolding!();

// ---------------- Kotlin 实现的宿主 ----------------

/// 宿主能力。所有方法都可能在持锁时被调用，实现里**不要**同步回调 Engine。
#[uniffi::export(with_foreign)]
pub trait Host: Send + Sync {
    /// 在后台页 WebView 里执行 JS（异步执行即可）
    fn eval_js(&self, js: String);
    /// 让后台页打开这个地址
    fn goto_url(&self, url: String);
    /// 用户此刻是否正看着后台页
    fn user_visible(&self) -> bool;
    /// 发一条系统通知。返回 false 表示没发出去（会按发件箱规则重试）
    fn notify(&self, alert: AlertInfo) -> bool;
}

#[derive(uniffi::Record, Debug, Clone)]
pub struct AlertInfo {
    /// new_bad_review / login_lost / login_restored / api_error
    pub kind: String,
    pub title: String,
    pub body: String,
    /// 点通知打开的链接（review-monitor://...），交给 [`parse_launch`] 解析
    pub launch_uri: String,
    /// 同一条评价重复通知时用它覆盖，系统通知用空串
    pub review_key: String,
}

fn kind_name(k: AlertKind) -> &'static str {
    match k {
        AlertKind::NewBadReview => "new_bad_review",
        AlertKind::LoginLost => "login_lost",
        AlertKind::LoginRestored => "login_restored",
        AlertKind::ApiError => "api_error",
    }
}

impl From<&Alert> for AlertInfo {
    fn from(a: &Alert) -> Self {
        Self {
            kind: kind_name(a.kind).into(),
            title: a.title.clone(),
            body: a.body.clone(),
            launch_uri: activate::for_alert(a).uri(),
            review_key: a.review_key.clone().unwrap_or_default(),
        }
    }
}

/// rm-core 的 Notifier → Kotlin 的系统通知。id 沿用 "toast"（系统通知），两端配置文件通用。
struct HostNotifier(Arc<dyn Host>);
impl Notifier for HostNotifier {
    fn id(&self) -> &'static str {
        "toast"
    }
    fn send(&self, a: &Alert) -> anyhow::Result<()> {
        anyhow::ensure!(self.0.notify(a.into()), "系统通知没有发出（可能没有通知权限）");
        Ok(())
    }
}

struct HostPage<'a>(&'a Arc<dyn Host>);
impl Page for HostPage<'_> {
    fn call(&self, req_id: &str, call: &poller::Call) {
        tracing::debug!("call {req_id} {call:?}");
        self.0.eval_js(call.to_js(req_id));
    }
    fn goto_reviews(&self) {
        tracing::info!("让后台页回到评价页");
        self.0.goto_url(poller::START_URL.into());
    }
    fn user_visible(&self) -> bool {
        self.0.user_visible()
    }
}

// ---------------- 给界面的数据 ----------------

#[derive(uniffi::Record, Debug, Clone)]
pub struct StatusInfo {
    /// 0 检测中 1 已登录 2 需要登录
    pub login: i32,
    pub busy: bool,
    pub message: String,
    /// 「上次成功 11-12 13:00」，没有为空
    pub last_ok: String,
    /// 上次成功的时间戳（毫秒），没有为 0。用来统计保活情况
    pub last_ok_ms: i64,
    /// 距下次检查的秒数，-1 表示没有计划（掉线暂停等）
    pub next_in_secs: i64,
    /// 顶部提示（配置文件损坏回退、通知多次失败等），没有为空
    pub banner: String,
}

#[derive(uniffi::Record, Debug, Clone)]
pub struct ReviewRow {
    pub key: String,
    /// 美团 / 点评
    pub platform: String,
    pub is_mt: bool,
    /// 「2.5★」
    pub star: String,
    pub time: String,
    pub content: String,
    pub source: String,
    pub replied: bool,
    pub bad: bool,
    pub read: bool,
}

/// 设置页的全部配置项（和电脑版设置页一一对应）
#[derive(uniffi::Record, Debug, Clone, PartialEq)]
pub struct Settings {
    pub interval_minutes: u32,
    pub jitter_percent: u32,
    pub overlap_minutes: u32,
    pub first_run_lookback_hours: u32,
    pub max_pages: u32,
    pub meituan_max_star: f32,
    pub dianping_max_star: f32,
    pub meituan_on: bool,
    pub dianping_on: bool,
    pub only_unreplied: bool,
    pub alert_existing_on_first_run: bool,
    pub notify_on: bool,
}

impl From<&Config> for Settings {
    fn from(c: &Config) -> Self {
        Self {
            interval_minutes: c.poll.interval_minutes,
            jitter_percent: c.poll.jitter_percent,
            overlap_minutes: c.poll.overlap_minutes,
            first_run_lookback_hours: c.poll.first_run_lookback_hours,
            max_pages: c.poll.max_pages,
            meituan_max_star: c.rules.meituan_max_star,
            dianping_max_star: c.rules.dianping_max_star,
            meituan_on: c.rules.platforms.contains(&Platform::Meituan),
            dianping_on: c.rules.platforms.contains(&Platform::Dianping),
            only_unreplied: c.rules.only_unreplied,
            alert_existing_on_first_run: c.rules.alert_existing_on_first_run,
            notify_on: c.notify.enabled.iter().any(|n| n == "toast"),
        }
    }
}

fn apply_settings(base: &Config, s: &Settings) -> Config {
    let mut c = base.clone();
    c.poll.interval_minutes = s.interval_minutes;
    c.poll.jitter_percent = s.jitter_percent;
    c.poll.overlap_minutes = s.overlap_minutes;
    c.poll.first_run_lookback_hours = s.first_run_lookback_hours;
    c.poll.max_pages = s.max_pages;
    c.rules.meituan_max_star = (s.meituan_max_star * 2.0).round() / 2.0;
    c.rules.dianping_max_star = (s.dianping_max_star * 2.0).round() / 2.0;
    c.rules.platforms = Platform::ALL
        .into_iter()
        .filter(|p| match p { Platform::Meituan => s.meituan_on, Platform::Dianping => s.dianping_on })
        .collect();
    c.rules.only_unreplied = s.only_unreplied;
    c.rules.alert_existing_on_first_run = s.alert_existing_on_first_run;
    c.notify.enabled.retain(|n| n != "toast");
    if s.notify_on { c.notify.enabled.insert(0, "toast".into()); }
    c
}

/// 点通知打开的链接解析结果
#[derive(uniffi::Record, Debug, Clone, PartialEq)]
pub struct Launch {
    /// open / backend / review
    pub kind: String,
    /// kind=review 时是评价 key（dp:123），否则为空
    pub review_key: String,
    /// kind=review 时，在后台页里执行它就会定位并高亮这条评价
    pub reveal_js: String,
}

/// 解析 review-monitor:// 链接；不是本程序的链接返回 None
#[uniffi::export]
pub fn parse_launch(uri: String) -> Option<Launch> {
    Some(launch_of(&activate::parse(&uri)?))
}

/// 列表里点卡片：和点通知走同一条路
#[uniffi::export]
pub fn launch_for_review(key: String) -> Option<Launch> {
    let (platform, feedback_id) = activate::split_key(&key)?;
    Some(launch_of(&Activation::Review { platform, feedback_id }))
}

fn launch_of(a: &Activation) -> Launch {
    match a {
        Activation::Open => Launch { kind: "open".into(), review_key: String::new(), reveal_js: String::new() },
        Activation::Backend => Launch { kind: "backend".into(), review_key: String::new(), reveal_js: String::new() },
        Activation::Review { platform, feedback_id } => Launch {
            kind: "review".into(),
            review_key: a.review_key().unwrap_or_default(),
            reveal_js: poller::reveal_js(*platform, *feedback_id),
        },
    }
}

/// 后台页的起始地址
#[uniffi::export]
pub fn start_url() -> String {
    poller::START_URL.into()
}

/// 不改配置，只校验；返回中文错误，合法返回空列表
#[uniffi::export]
pub fn validate_settings(s: Settings) -> Vec<String> {
    apply_settings(&Config::default(), &s).validate().err().unwrap_or_default()
}

#[uniffi::export]
pub fn default_settings() -> Settings {
    (&Config::default()).into()
}

// ---------------- Engine ----------------

#[derive(Debug, uniffi::Error)]
pub enum EngineError {
    Failed { msg: String },
}

impl std::error::Error for EngineError {}
impl std::fmt::Display for EngineError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self { EngineError::Failed { msg } => f.write_str(msg) }
    }
}
impl From<anyhow::Error> for EngineError {
    fn from(e: anyhow::Error) -> Self {
        EngineError::Failed { msg: format!("{e:#}") }
    }
}

struct Inner {
    poller: Poller,
    host: Arc<dyn Host>,
    config_path: PathBuf,
    cfg_warn: Option<String>,
}

#[derive(uniffi::Object)]
pub struct Engine {
    inner: Mutex<Inner>,
}

/// 轮询用的单调时钟。安卓上 Instant 走 CLOCK_MONOTONIC，深睡时不走；
/// 这里按 CLOCK_BOOTTIME（含睡眠时间）推算，熄屏几分钟后醒来，到点的检查马上就会开始。
fn mono_now() -> Instant {
    use std::sync::OnceLock;
    use std::time::Duration;
    static BASE: OnceLock<(Instant, Option<Duration>)> = OnceLock::new();
    fn boottime() -> Option<Duration> {
        #[cfg(target_os = "android")]
        {
            let mut ts = libc::timespec { tv_sec: 0, tv_nsec: 0 };
            if unsafe { libc::clock_gettime(libc::CLOCK_BOOTTIME, &mut ts) } == 0 {
                return Some(Duration::new(ts.tv_sec as u64, ts.tv_nsec as u32));
            }
        }
        None
    }
    let (t0, b0) = *BASE.get_or_init(|| (Instant::now(), boottime()));
    match (b0, boottime()) {
        (Some(b0), Some(b)) => t0 + b.saturating_sub(b0),
        _ => Instant::now(),
    }
}

fn now_ms() -> i64 {
    Local::now().timestamp_millis()
}

fn dispatcher(cfg: &Config, host: &Arc<dyn Host>) -> Dispatcher {
    let mut v: Vec<Box<dyn Notifier>> = Vec::new();
    if cfg.notify.enabled.iter().any(|n| n == "toast") { v.push(Box::new(HostNotifier(host.clone()))); }
    Dispatcher::new(v)
}

static LOG: Once = Once::new();

fn init_logging(dir: &std::path::Path) {
    LOG.call_once(|| {
        use tracing_subscriber::{fmt, prelude::*, EnvFilter};
        let filter = EnvFilter::new("info,rm_core=debug,rm_ffi=debug");
        let file = tracing_appender::rolling::Builder::new()
            .rotation(tracing_appender::rolling::Rotation::DAILY)
            .filename_prefix("review-monitor")
            .filename_suffix("log")
            .max_log_files(14)
            .build(dir.join("logs"))
            .ok()
            .map(|a| fmt::layer().with_writer(std::sync::Mutex::new(a)).with_ansi(false));
        let reg = tracing_subscriber::registry().with(filter).with(file);
        #[cfg(target_os = "android")]
        let reg = reg.with(paranoid_android::layer("RM.Core").with_ansi(false).without_time());
        let _ = reg.try_init();
    });
}

impl Engine {
    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        // 某次调用 panic 了也别让整个程序跟着锁死
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }
}

#[uniffi::export]
impl Engine {
    /// data_dir：应用私有目录（Context.filesDir），里面放 config.toml、data.db、logs/
    #[uniffi::constructor]
    pub fn new(data_dir: String, host: Arc<dyn Host>) -> Result<Arc<Self>, EngineError> {
        let dir = PathBuf::from(data_dir);
        std::fs::create_dir_all(&dir).map_err(anyhow::Error::from)?;
        init_logging(&dir);
        let config_path = dir.join("config.toml");
        let (cfg, cfg_warn) = Config::load_or_default(&config_path);
        if let Some(w) = &cfg_warn { tracing::warn!("{w}"); }
        let store = Store::open(&dir.join("data.db"))?;
        let disp = dispatcher(&cfg, &host);
        tracing::info!("Engine 启动，数据目录 {}", dir.display());
        let poller = Poller::new(cfg, store, disp, mono_now());
        Ok(Arc::new(Self { inner: Mutex::new(Inner { poller, host, config_path, cfg_warn }) }))
    }

    /// 主线程上每 0.5–1 秒调一次
    pub fn tick(&self) {
        let mut g = self.lock();
        let Inner { poller, host, .. } = &mut *g;
        poller.tick(&HostPage(host), mono_now(), now_ms());
    }

    /// poller.js 通过 window.ipc.postMessage 回传的原始 JSON
    pub fn on_message(&self, json: String) {
        let msg: IpcMsg = match serde_json::from_str(&json) {
            Ok(m) => m,
            Err(e) => { tracing::warn!("IPC 解析失败: {e}"); return; }
        };
        let mut g = self.lock();
        let Inner { poller, host, .. } = &mut *g;
        poller.on_msg(&msg, &HostPage(host), mono_now(), now_ms());
    }

    pub fn check_now(&self) {
        self.lock().poller.check_now(mono_now());
    }

    /// 主动退出登录：登录状态由界面清掉（cookie、页面存储），这里让监控暂停、不发提醒
    pub fn logout(&self) {
        self.lock().poller.logout(mono_now(), now_ms());
    }

    pub fn status(&self) -> StatusInfo {
        let g = self.lock();
        let st = &g.poller.status;
        let mut banner = g.cfg_warn.clone().unwrap_or_default();
        if let Ok(n) = g.poller.store.gave_up_count()
            && n > 0
        {
            if !banner.is_empty() { banner.push_str("  "); }
            banner.push_str(&format!("有 {n} 条通知多次发送失败已放弃。"));
        }
        StatusInfo {
            login: match st.login { Login::Unknown => 0, Login::In => 1, Login::Out => 2 },
            busy: st.busy,
            message: st.message.clone(),
            last_ok: st.last_ok.map(|t| format!("上次成功 {}", t.format("%m-%d %H:%M"))).unwrap_or_default(),
            last_ok_ms: st.last_ok.map(|t| t.timestamp_millis()).unwrap_or(0),
            next_in_secs: match (st.login, st.next_at) {
                (Login::Out, _) | (_, None) => -1,
                (_, Some(t)) => t.saturating_duration_since(mono_now()).as_secs() as i64,
            },
            banner,
        }
    }

    /// 列表；only_bad=true 只看差评
    pub fn reviews(&self, only_bad: bool, limit: u32) -> Vec<ReviewRow> {
        let g = self.lock();
        g.poller.store.recent(limit, only_bad).unwrap_or_default().into_iter().map(|s| {
            let r = s.review;
            ReviewRow {
                key: r.key,
                platform: r.platform.label().into(),
                is_mt: r.platform == Platform::Meituan,
                star: format!("{}★", fmt_star(r.star)),
                time: r.add_time.format("%m-%d %H:%M").to_string(),
                content: r.content,
                source: r.source,
                replied: r.replied,
                bad: s.is_bad,
                read: s.read,
            }
        }).collect()
    }

    pub fn unread_count(&self) -> u32 {
        self.lock().poller.store.unread_bad_count().unwrap_or(0)
    }

    pub fn set_read(&self, key: String, read: bool) {
        let mut g = self.lock();
        if let Err(e) = g.poller.store.set_read(&key, read) { tracing::warn!("改已读失败 {key}: {e:#}"); }
        g.poller.list_dirty = true;
    }

    pub fn mark_all_read(&self) {
        let mut g = self.lock();
        let _ = g.poller.store.mark_all_read();
        g.poller.list_dirty = true;
    }

    /// 列表要不要刷新（取一次就清掉）
    pub fn take_list_dirty(&self) -> bool {
        std::mem::take(&mut self.lock().poller.list_dirty)
    }

    /// 掉线等需要用户处理（取一次就清掉）：界面应切到后台页
    pub fn take_attention(&self) -> bool {
        std::mem::take(&mut self.lock().poller.attention)
    }

    pub fn settings(&self) -> Settings {
        self.lock().poller.config().into()
    }

    /// 保存设置；成功返回空串，失败返回中文原因（不保存）
    pub fn save_settings(&self, s: Settings) -> String {
        let mut g = self.lock();
        let cfg = apply_settings(g.poller.config(), &s);
        match cfg.save(&g.config_path) {
            Ok(()) => {
                let disp = dispatcher(&cfg, &g.host);
                g.poller.set_config(cfg, disp, mono_now());
                g.cfg_warn = None;
                tracing::info!("设置已保存");
                String::new()
            }
            Err(e) => format!("{e:#}"),
        }
    }

    /// 发一条系统提醒（不走发件箱），比如「仍在后台运行」
    pub fn system_alert(&self, title: String, body: String) {
        let g = self.lock();
        let a = Alert::system(AlertKind::LoginRestored, title, body);
        let _ = HostNotifier(g.host.clone()).send(&a);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex as M;

    #[derive(Default)]
    struct FakeHost {
        js: M<Vec<String>>,
        notes: M<Vec<AlertInfo>>,
    }
    impl Host for FakeHost {
        fn eval_js(&self, js: String) { self.js.lock().unwrap().push(js); }
        fn goto_url(&self, _url: String) {}
        fn user_visible(&self) -> bool { false }
        fn notify(&self, alert: AlertInfo) -> bool { self.notes.lock().unwrap().push(alert); true }
    }

    #[test]
    fn settings_roundtrip_and_validate() {
        let d = default_settings();
        assert_eq!(d.interval_minutes, 3);
        assert!(validate_settings(d.clone()).is_empty());
        let mut bad = d.clone();
        bad.interval_minutes = 0;
        bad.meituan_on = false;
        bad.dianping_on = false;
        assert!(validate_settings(bad).len() >= 2);
        let mut s = d;
        s.dianping_max_star = 2.3; // 按 0.5 取整
        assert_eq!(apply_settings(&Config::default(), &s).rules.dianping_max_star, 2.5);
    }

    #[test]
    fn launch_parsing() {
        let l = parse_launch("review-monitor://open?review=dp:3550718756".into()).unwrap();
        assert_eq!((l.kind.as_str(), l.review_key.as_str()), ("review", "dp:3550718756"));
        assert!(l.reveal_js.contains("reveal('dianping','3550718756')"));
        assert_eq!(parse_launch("review-monitor://open?page=backend".into()).unwrap().kind, "backend");
        assert!(parse_launch("https://x".into()).is_none());
        assert_eq!(launch_for_review("mt:1".into()).unwrap().kind, "review");
    }

    #[test]
    fn engine_drives_host() {
        let dir = tempfile::tempdir().unwrap();
        let host = Arc::new(FakeHost::default());
        let e = Engine::new(dir.path().display().to_string(), host.clone()).unwrap();
        e.check_now();
        e.tick();
        let js = host.js.lock().unwrap().clone();
        assert!(js.iter().any(|j| j.contains("__rm.whoami")), "{js:?}");
        // 坏 JSON 不会 panic
        e.on_message("not json".into());
        assert_eq!(e.status().login, 0);
        assert!(e.reviews(true, 10).is_empty());
        // 保存非法设置被拒
        let mut s = e.settings();
        s.interval_minutes = 999;
        assert!(!e.save_settings(s).is_empty());
        assert!(dir.path().join("data.db").exists());
    }

    #[test]
    fn alert_info_has_launch_uri() {
        let mut a = Alert::system(AlertKind::NewBadReview, "t", "b");
        a.review_key = Some("dp:7".into());
        let i: AlertInfo = (&a).into();
        assert_eq!(i.launch_uri, "review-monitor://open?review=dp:7");
        assert_eq!(i.kind, "new_bad_review");
        let l: AlertInfo = (&Alert::system(AlertKind::LoginLost, "t", "b")).into();
        assert_eq!(l.launch_uri, "review-monitor://open?page=backend");
    }
}
