//! rm-core 的 UniFFI 门面：给安卓（Kotlin）用。
//!
//! 接口请求由 Rust 直接发（rm-core 的 http 模块，和电脑版同一套），不经过 WebView。
//! Kotlin 只做两件事：提供一个能发系统通知的 [`Host`]；需要登录时开 WebView 让用户登录，
//! 登录完把 cookie 交给 [`Engine::set_credentials`]，然后就可以把 WebView 销毁。
//! 主线程上定时调 `tick()` 驱动轮询。业务逻辑全部在 rm-core 里。
use chrono::Local;
use rm_core::activate::{self, Activation};
use rm_core::config::Config;
use rm_core::browse;
use rm_core::http::{Client, ThreadTransport};
use std::collections::{BTreeMap, HashMap};
use rm_core::notify::{fmt_star, Alert, AlertKind, Dispatcher, Notifier};
use rm_core::poller::{self, Login, Poller};
use rm_core::store::Store;
use rm_core::Platform;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, Once};
use std::time::Instant;

uniffi::setup_scaffolding!();

// ---------------- Kotlin 实现的宿主 ----------------

/// 宿主能力。可能在持锁时被调用，实现里**不要**同步回调 Engine。
#[uniffi::export(with_foreign)]
pub trait Host: Send + Sync {
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

// ---------------- 评价页（按网页版筛选直接查经营宝）----------------

/// 一组筛选（回复状态 / 星级 / 来源 / 用户 / 时间），options 就是发给接口的值
#[derive(uniffi::Record, Debug, Clone)]
pub struct FilterGroupInfo {
    pub key: String,
    pub label: String,
    pub options: Vec<String>,
    pub default_value: String,
}

/// 网页星级档位 band（「全部」「中差评」…）里能细筛哪几星
#[uniffi::export]
pub fn review_star_options(is_mt: bool, band: String) -> Vec<u8> {
    let p = if is_mt { Platform::Meituan } else { Platform::Dianping };
    browse::stars_in_band(p, &band)
}

/// 该平台网页版上的筛选项（顺序同网页）
#[uniffi::export]
pub fn review_filters(is_mt: bool) -> Vec<FilterGroupInfo> {
    let p = if is_mt { Platform::Meituan } else { Platform::Dianping };
    browse::filter_groups(p).into_iter().map(|g| FilterGroupInfo {
        key: g.key.into(),
        label: g.label.into(),
        options: g.options.into_iter().map(String::from).collect(),
        default_value: g.default.into(),
    }).collect()
}

#[derive(uniffi::Record, Debug, Clone)]
pub struct ReviewQuery {
    pub is_mt: bool,
    /// 从 1 开始
    pub page: u32,
    /// filterKey → 选中的值；没给的按默认
    pub filters: HashMap<String, String>,
    /// 只看这几星（1~5，半星归低一档）；空 = 不细筛。网页版没有这个，拉回来后本地筛
    pub stars: Vec<u8>,
}

#[derive(uniffi::Record, Debug, Clone)]
pub struct ReplyInfo {
    pub reply_id: i64,
    pub follow_id: i64,
    pub from_merchant: bool,
    pub name: String,
    pub content: String,
    pub time: String,
}

#[derive(uniffi::Record, Debug, Clone)]
pub struct FeedbackInfo {
    pub key: String,
    pub is_mt: bool,
    pub feedback_id: i64,
    pub user_name: String,
    /// 「2.5★」
    pub star: String,
    pub star_value: f32,
    pub sub_scores: String,
    pub time: String,
    pub content: String,
    /// 缩略图
    pub pictures: Vec<String>,
    /// 大图（和 pictures 一一对应）
    pub pictures_full: Vec<String>,
    pub video_count: u32,
    pub source: String,
    pub deal_name: String,
    pub returned_customer: bool,
    pub replied: bool,
    pub replies: Vec<ReplyInfo>,
}

#[derive(uniffi::Record, Debug, Clone)]
pub struct FeedbackPageInfo {
    pub total: u32,
    pub page: u32,
    pub page_size: u32,
    /// 这一页接口实际返回了几条（细筛后 items 可能更少，翻页按这个算）
    pub scanned: u32,
    pub items: Vec<FeedbackInfo>,
}

impl From<browse::Feedback> for FeedbackInfo {
    fn from(f: browse::Feedback) -> Self {
        Self {
            key: f.key,
            is_mt: f.platform == Platform::Meituan,
            feedback_id: f.feedback_id,
            user_name: f.user_name,
            star: format!("{}★", fmt_star(f.star)),
            star_value: f.star,
            sub_scores: f.sub_scores,
            time: f.time,
            content: f.content,
            pictures: f.pictures,
            pictures_full: f.pictures_full,
            video_count: f.video_count,
            source: f.source,
            deal_name: f.deal_name,
            returned_customer: f.returned_customer,
            replied: f.replied,
            replies: f.replies.into_iter().map(|r| ReplyInfo {
                reply_id: r.reply_id, follow_id: r.follow_id, from_merchant: r.from_merchant,
                name: r.name, content: r.content, time: r.time,
            }).collect(),
        }
    }
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
        Activation::Open => Launch { kind: "open".into(), review_key: String::new() },
        // backend：去登录
        Activation::Backend => Launch { kind: "backend".into(), review_key: String::new() },
        Activation::Review { .. } => Launch { kind: "review".into(), review_key: a.review_key().unwrap_or_default() },
    }
}

/// 登录网页的起始地址（没登录会跳到登录页，登录完跳回经营宝后台）
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
    net: ThreadTransport,
    host: Arc<dyn Host>,
    config_path: PathBuf,
    cfg_warn: Option<String>,
}

#[derive(uniffi::Object)]
pub struct Engine {
    inner: Mutex<Inner>,
    /// 评价页的查询用：在调用方的线程里同步发，不经过轮询，也不占着 inner 的锁
    client: Client,
}

const BROWSE_PAGE_SIZE: u32 = 20;

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

fn fail(m: &str) -> EngineError {
    EngineError::Failed { msg: m.into() }
}

impl Engine {
    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        // 某次调用 panic 了也别让整个程序跟着锁死
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// 评价页自己发的请求：拿一份当前 cookie，在调用方线程里同步发（不占着锁），
    /// 回来后把续发的 cookie 记下；没登录、登录失效、网络错误都在这里变成中文错误。
    fn direct(
        &self,
        req_id: &str,
        make: impl FnOnce(&Inner) -> Result<poller::Call, EngineError>,
    ) -> Result<rm_core::http::HttpReply, EngineError> {
        let req = {
            let g = self.lock();
            if !g.poller.has_credentials() { return Err(fail("还没登录")); }
            make(&g)?.to_http(g.poller.credentials(), now_ms())
        };
        let reply = self.client.call(req_id, &req);
        {
            // 请求编号对不上轮询的，on_reply 只会记 cookie，不影响轮询
            let mut g = self.lock();
            g.poller.on_reply(&reply, mono_now(), now_ms());
            if reply.error.is_none() && browse::is_login_lost(reply.status, &reply.body) {
                g.poller.check_now(mono_now());
                return Err(fail("登录已失效，正在重新检查登录"));
            }
        }
        match &reply.error {
            Some(e) if e == "timeout" => Err(fail("经营宝没有响应（超时），稍后再试")),
            Some(_) => Err(fail("连不上经营宝，检查一下网络")),
            None => Ok(reply),
        }
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
        Ok(Arc::new(Self {
            inner: Mutex::new(Inner { poller, net: ThreadTransport::default(), host, config_path, cfg_warn }),
            client: Client::default(),
        }))
    }

    /// 评价页：按网页版的筛选查一页评价（只读）。**会阻塞到接口返回（最长 15 秒），要在后台线程里调。**
    /// 没登录、登录失效、网络错误都以中文错误返回；登录失效时顺便让监控马上复查登录。
    pub fn fetch_reviews(&self, q: ReviewQuery) -> Result<FeedbackPageInfo, EngineError> {
        let platform = if q.is_mt { Platform::Meituan } else { Platform::Dianping };
        let page = q.page.max(1);
        let mut stars = q.stars.clone();
        stars.sort_unstable();
        stars.dedup();
        stars.retain(|s| (1..=5).contains(s));
        let reply = self.direct("browse", |g| {
            let Some(poi) = g.poller.cached_pois().into_iter().next() else {
                return Err(fail("还没取到门店，等第一次检查完成再试"));
            };
            let mut picked: BTreeMap<String, String> = q.filters.into_iter().collect();
            let band = picked.get("starType").cloned().unwrap_or_else(|| "全部".into());
            picked.insert("starType".into(), browse::star_band_for(platform, &band, &stars));
            let body = browse::list_body(platform, &poi, page, BROWSE_PAGE_SIZE, &picked, Local::now());
            Ok(poller::Call::List { platform, body })
        })?;
        if reply.status != Some(200) {
            return Err(fail(&format!("经营宝返回错误（HTTP {}）", reply.status.unwrap_or(0))));
        }
        let p = browse::parse_page(platform, &reply.body, Local::now()).map_err(|e| fail(&e.to_string()))?;
        let scanned = p.items.len() as u32;
        let items = p.items.into_iter().filter(|f| stars.is_empty() || stars.contains(&browse::star_bucket(f.star)));
        Ok(FeedbackPageInfo { total: p.total, page, page_size: BROWSE_PAGE_SIZE, scanned, items: items.map(Into::into).collect() })
    }

    /// 回复一条评价（key 形如 `dp:3550270840`）。**写操作，回复会公开显示在美团 / 点评上**，
    /// 只能由用户在评价页亲手点「发送」触发。会阻塞到接口返回，要在后台线程里调。成功返回空，失败返回中文原因。
    pub fn reply_review(&self, key: String, content: String) -> Result<(), EngineError> {
        let (platform, feedback_id) = activate::split_key(&key).ok_or_else(|| fail("评价编号不对"))?;
        let body = browse::reply_body(platform, feedback_id, &content).map_err(|e| fail(&e))?;
        let reply = self.direct("reply", |_| Ok(poller::Call::Reply { body }))?;
        browse::reply_outcome(reply.status, &reply.body).map_err(|e| fail(&e))?;
        tracing::info!("已回复评价 {key}（{} 字）", content.trim().chars().count());
        Ok(())
    }

    /// 主线程上每 0.5–1 秒调一次：收已经回来的接口结果，到点了就发下一个请求（请求在后台线程里跑，不阻塞）
    pub fn tick(&self) {
        let mut g = self.lock();
        let Inner { poller, net, .. } = &mut *g;
        let (now, ms) = (mono_now(), now_ms());
        for r in net.drain() { poller.on_reply(&r, now, ms); }
        poller.tick(net, now, ms);
    }

    /// 登录网页里取到的 cookie（`CookieManager.getCookie` 的原样输出，含 httpOnly）和网页的 UA。
    /// 空的不收，返回 false。掉线状态下会马上用它探测登录。
    pub fn set_credentials(&self, cookie_header: String, user_agent: String) -> bool {
        self.lock().poller.set_credentials(&cookie_header, &user_agent, mono_now())
    }

    /// 有没有保存的登录 cookie（没有就得开登录网页）
    pub fn has_credentials(&self) -> bool {
        self.lock().poller.has_credentials()
    }

    pub fn check_now(&self) {
        self.lock().poller.check_now(mono_now());
    }

    /// 主动退出登录：丢掉保存的 cookie，监控暂停、不发提醒。网页里的 cookie 由界面清掉
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

    /// 掉线等需要用户处理（取一次就清掉）：界面应切到登录页
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
        notes: M<Vec<AlertInfo>>,
    }
    impl Host for FakeHost {
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
        assert_eq!(parse_launch("review-monitor://open?page=backend".into()).unwrap().kind, "backend");
        assert!(parse_launch("https://x".into()).is_none());
        assert_eq!(launch_for_review("mt:1".into()).unwrap().kind, "review");
    }

    #[test]
    fn engine_drives_host() {
        let dir = tempfile::tempdir().unwrap();
        let host = Arc::new(FakeHost::default());
        let e = Engine::new(dir.path().display().to_string(), host.clone()).unwrap();
        // 还没登录过：直接进「需要登录」，不发请求、不提醒
        assert!(!e.has_credentials());
        e.check_now();
        e.tick();
        assert_eq!(e.status().login, 2);
        assert!(e.take_attention());
        assert!(host.notes.lock().unwrap().is_empty());
        assert!(!e.set_credentials(" ".into(), "UA".into()));
        // 评价页：没登录时直接报错，不发请求
        let q = ReviewQuery { is_mt: true, page: 1, filters: HashMap::new(), stars: vec![1] };
        assert!(e.fetch_reviews(q).unwrap_err().to_string().contains("还没登录"));
        // 回复：先校验内容和编号，再看登录；都不会真的发出去
        assert!(e.reply_review("dp:1".into(), "  ".into()).unwrap_err().to_string().contains("不能为空"));
        assert!(e.reply_review("xx".into(), "谢谢".into()).unwrap_err().to_string().contains("编号"));
        assert!(e.reply_review("dp:1".into(), "谢谢".into()).unwrap_err().to_string().contains("还没登录"));
        assert_eq!(review_filters(false).len(), 5);
        assert_eq!(review_filters(true)[0].default_value, "未回复");
        // 交来 cookie 后存进库，重开 Engine 还在；退出登录就清掉
        assert!(e.set_credentials("bsid=1; a=2".into(), "UA".into()));
        assert!(e.has_credentials());
        drop(e);
        let e = Engine::new(dir.path().display().to_string(), host.clone()).unwrap();
        assert!(e.has_credentials());
        e.logout();
        assert!(!e.has_credentials());
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
