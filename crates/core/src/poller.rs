//! 轮询状态机：(门店列表) → 各平台评价列表（翻页）→ 入库 → 推送。正常一轮只有这几个请求。
//!
//! 请求由这里拼好（带上登录 cookie），经 [`Transport`] 直接发给经营宝接口，结果由宿主喂给 `on_reply`。
//! 不碰网页：网页只用来登录，登录完宿主把 cookie 交给 [`Poller::set_credentials`]，之后就可以关掉网页。
//! 掉线判断：列表 / 门店接口明说没登录（401、303）就是掉线；回了别的怪东西，再带着 cookie 问一次
//! whoami（「我是谁」），答不上来才算登录失效。掉线后只用 whoami 慢慢探测，等用户重新登录。
use crate::config::Config;
use crate::cred::Credentials;
use crate::http::{HttpReply, HttpRequest, Transport, BASE, FALLBACK_UA, REFERER};
use crate::model::Platform;
use crate::notify::{Alert, AlertKind, Dispatcher};
use crate::browse::is_login_lost;
use crate::parse::parse_list;
use crate::schedule::{backoff, next_delay, query_window};
use crate::store::Store;
use chrono::{DateTime, Local, TimeZone};
use rand::{rngs::StdRng, Rng, SeedableRng};
use std::collections::VecDeque;
use std::time::{Duration, Instant};

pub const PAGE_SIZE: u32 = 20;
const REPLY_TIMEOUT: Duration = Duration::from_secs(20);
const API_ALERT_AFTER: u32 = 3;
const API_ALERT_DEDUPE_MS: i64 = 6 * 3_600_000;
/// 掉线后，刚交来新 cookie 的这段时间里勤快地探测（登录跳转要几秒到十几秒）
const LOGIN_PROBE_FAST: Duration = Duration::from_secs(15);
const LOGIN_FAST_WINDOW: Duration = Duration::from_secs(180);
/// 之后就放慢：没人登录时别拿着失效的 cookie 一直敲接口
const LOGIN_PROBE_SLOW: Duration = Duration::from_secs(300);
const FLUSH_EVERY: Duration = Duration::from_secs(60);
const FIRST_CHECK_DELAY: Duration = Duration::from_secs(3);
/// state 表里记「已经发过需要重新登录」，空串表示没有
const LOGIN_LOST_KEY: &str = "login_lost_notified";
/// LOGIN_LOST_KEY 的特殊值：用户主动退出登录（不算掉线，不提醒，重新登录也不发「已恢复」）
const MANUAL_LOGOUT: &str = "manual";
const CRED_KEY: &str = "credentials";

/// 经营宝评价页：登录网页的起始地址（没登录会自动跳到登录页，登录完跳回这里）
pub const START_URL: &str = "https://ecom.meituan.com/meishi/?cate=6103#https://ecom.meituan.com/emis/evaluation/poi";

/// 要发的接口请求。轮询只用只读的三种；Reply 是评价页里用户亲手点「发送」才发的写操作
#[derive(Debug, Clone, PartialEq)]
pub enum Call {
    Whoami,
    Pois,
    List { platform: Platform, body: String },
    /// 回复评价（body 由 `browse::reply_body` 生成）
    Reply { body: String },
}

impl Call {
    /// 拼成 HTTP 请求。`_tm` 和页面里的请求一样带毫秒时间戳。
    pub fn to_http(&self, cred: &Credentials, now_ms: i64) -> HttpRequest {
        let (method, path, body) = match self {
            Call::Whoami => ("GET", "/emis/gw/account/biz/getUserInfo", None),
            Call::Pois => ("GET", "/emis/gw/rpc/TFeedbackListNewService/getCityPoiListSimple", None),
            Call::List { platform, body } => {
                let svc = match platform {
                    Platform::Meituan => "/emis/gw/rpc/TFeedbackListNewService/queryMTFeedbackPCNew",
                    Platform::Dianping => "/emis/gw/rpc/TFeedbackListNewService/queryDPFeedbackPCNew",
                };
                ("POST", svc, Some(body.clone()))
            }
            Call::Reply { body } => ("POST", "/emis/gw/rpc/TFeedbackListNewService/followReplyPCNew", Some(body.clone())),
        };
        let ua = if cred.user_agent.is_empty() { FALLBACK_UA.to_string() } else { cred.user_agent.clone() };
        let mut headers = vec![
            ("Cookie", cred.header()),
            ("User-Agent", ua),
            ("Referer", REFERER.to_string()),
            ("Accept", "application/json, text/plain, */*".to_string()),
        ];
        if body.is_some() {
            headers.push(("Content-Type", "application/json".to_string()));
            headers.push(("Origin", BASE.to_string()));
        }
        HttpRequest { method, url: format!("{BASE}{path}?_tm={now_ms}"), headers, body }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Login {
    Unknown,
    In,
    Out,
}

#[derive(Debug, Clone)]
pub struct Status {
    pub login: Login,
    pub busy: bool,
    pub last_ok: Option<DateTime<Local>>,
    pub next_at: Option<Instant>,
    pub message: String,
}

#[derive(Debug, Clone, PartialEq)]
enum Step {
    Whoami,
    Pois,
    List { platform: Platform, poi: String, page: u32 },
}

struct Waiting {
    req: String,
    step: Step,
    deadline: Instant,
}

struct Cycle {
    started_ms: i64,
    window: (i64, i64),
    baseline: bool,
    steps: VecDeque<Step>,
    waiting: Option<Waiting>,
    gap_until: Instant,
    new_bad: u32,
    /// 列表 / 门店接口出了看不懂的错，正在用 whoami 确认是不是掉线；登录还在就按这个记失败
    verify: Option<(String, String)>,
}

enum State {
    Idle { next: Instant },
    Running(Cycle),
    LoginLost { next_probe: Instant, probe: Option<String> },
}

pub struct Poller {
    cfg: Config,
    pub store: Store,
    disp: Dispatcher,
    rng: StdRng,
    state: State,
    failures: u32,
    seq: u64,
    last_flush: Instant,
    cred: Credentials,
    /// 上次交来新 cookie 的时间（掉线探测据此决定快慢）
    cred_at: Option<Instant>,
    pub status: Status,
    /// 列表有变化，界面需要刷新
    pub list_dirty: bool,
    /// 需要用户处理（掉线），界面应切到登录页
    pub attention: bool,
}

impl Poller {
    pub fn new(cfg: Config, store: Store, disp: Dispatcher, now: Instant) -> Self {
        let last_ok = store.get_state("last_success").ok().flatten().and_then(|v| v.parse::<i64>().ok())
            .and_then(|ms| Local.timestamp_millis_opt(ms).single());
        let cred = store.get_state(CRED_KEY).ok().flatten().filter(|s| !s.is_empty())
            .and_then(|s| Credentials::unseal(&s)).unwrap_or_default();
        Self {
            cfg, store, disp,
            rng: StdRng::from_os_rng(),
            state: State::Idle { next: now + FIRST_CHECK_DELAY },
            failures: 0, seq: 0,
            last_flush: now,
            cred, cred_at: None,
            status: Status { login: Login::Unknown, busy: false, last_ok, next_at: Some(now + FIRST_CHECK_DELAY), message: "正在启动…".into() },
            list_dirty: true,
            attention: false,
        }
    }

    pub fn config(&self) -> &Config {
        &self.cfg
    }

    pub fn has_credentials(&self) -> bool {
        !self.cred.is_empty()
    }

    /// 当前的登录 cookie（评价页自己发查询时拿一份去用）
    pub fn credentials(&self) -> &Credentials {
        &self.cred
    }

    /// 宿主从登录网页里取到了 cookie（`a=1; b=2` 形式，要含 httpOnly 的）和网页的 UA。
    /// 掉线状态下会马上用它探测一次；返回 false 表示是空的、没收下。
    pub fn set_credentials(&mut self, cookie_header: &str, user_agent: &str, now: Instant) -> bool {
        let mut c = Credentials::from_header(cookie_header, user_agent);
        if c.is_empty() { return false; }
        if c.user_agent.is_empty() { c.user_agent = self.cred.user_agent.clone(); }
        if c != self.cred {
            tracing::info!("收到登录 cookie {} 个", c.cookies.len());
            self.cred = c;
            self.save_credentials();
        }
        self.cred_at = Some(now);
        if let State::LoginLost { next_probe, probe } = &mut self.state {
            *next_probe = now;
            *probe = None;
        }
        true
    }

    fn save_credentials(&self) {
        let v = if self.cred.is_empty() { String::new() } else { self.cred.seal() };
        if let Err(e) = self.store.set_state(CRED_KEY, &v) { tracing::error!("保存登录 cookie 失败: {e:#}"); }
    }

    /// 应用新配置（已校验）。阈值变化会重新标记列表；下一轮按新间隔排期。
    pub fn set_config(&mut self, cfg: Config, disp: Dispatcher, now: Instant) {
        if cfg.rules() != self.cfg.rules() {
            if let Err(e) = self.store.reclassify(&cfg.rules()) { tracing::warn!("重新标记失败: {e:#}"); }
            self.list_dirty = true;
        }
        self.cfg = cfg;
        self.disp = disp;
        if let State::Idle { next } = &mut self.state {
            *next = now + next_delay(self.cfg.poll.interval_minutes, self.cfg.poll.jitter_percent, &mut self.rng);
            self.status.next_at = Some(*next);
        }
    }

    pub fn check_now(&mut self, now: Instant) {
        match &mut self.state {
            State::Idle { next } => { *next = now; }
            State::LoginLost { next_probe, probe } => { *next_probe = now; *probe = None; }
            State::Running(_) => {}
        }
    }

    /// 用户主动退出登录：丢掉保存的 cookie（网页里的由宿主清），监控暂停到重新登录；不发任何提醒。
    pub fn logout(&mut self, now: Instant, _now_ms: i64) {
        let _ = self.store.set_state(LOGIN_LOST_KEY, MANUAL_LOGOUT);
        self.cred = Credentials::default();
        self.cred_at = None;
        self.save_credentials();
        self.state = State::LoginLost { next_probe: now + LOGIN_PROBE_SLOW, probe: None };
        self.failures = 0;
        self.status.login = Login::Out;
        self.status.busy = false;
        self.status.next_at = None;
        self.status.message = "已退出登录，登录后继续监控".into();
        self.attention = true;
    }

    fn next_req(&mut self) -> String {
        self.seq += 1;
        format!("r{}", self.seq)
    }

    fn probe_every(&self, now: Instant) -> Duration {
        match self.cred_at {
            Some(t) if now.saturating_duration_since(t) < LOGIN_FAST_WINDOW => LOGIN_PROBE_FAST,
            _ => LOGIN_PROBE_SLOW,
        }
    }

    pub fn tick(&mut self, net: &dyn Transport, now: Instant, now_ms: i64) {
        if now.duration_since(self.last_flush) >= FLUSH_EVERY {
            self.flush(now, now_ms);
        }
        match &mut self.state {
            State::Idle { next } => {
                if now >= *next { self.start_cycle(now, now_ms); }
            }
            State::LoginLost { next_probe, probe } => {
                // 没有 cookie 就没什么可探测的，等宿主交来
                if !self.cred.is_empty() && now >= *next_probe {
                    if probe.is_none() {
                        let every = self.probe_every(now);
                        let req = self.next_req();
                        if let State::LoginLost { next_probe, probe } = &mut self.state {
                            *probe = Some(req.clone());
                            *next_probe = now + every;
                        }
                        net.send(&req, Call::Whoami.to_http(&self.cred, now_ms));
                    } else if let State::LoginLost { probe, .. } = &mut self.state {
                        // 上一次探测没有回音，清掉，下一拍重来
                        *probe = None;
                    }
                }
            }
            State::Running(_) => {}
        }
        // 运行中：超时 / 发下一步
        let mut timed_out = false;
        let mut to_send: Option<Step> = None;
        if let State::Running(c) = &mut self.state {
            if let Some(w) = &c.waiting {
                if now >= w.deadline { timed_out = true; }
            } else if now >= c.gap_until
                && let Some(step) = c.steps.pop_front()
            {
                to_send = Some(step);
            }
        }
        if timed_out {
            self.fail(now, now_ms, "timeout", "接口没有响应（超时）");
            return;
        }
        if let Some(step) = to_send {
            let req = self.next_req();
            let call = self.call_for(&step);
            if let State::Running(c) = &mut self.state {
                c.waiting = Some(Waiting { req: req.clone(), step, deadline: now + REPLY_TIMEOUT });
            }
            net.send(&req, call.to_http(&self.cred, now_ms));
        } else if let State::Running(c) = &self.state
            && c.waiting.is_none()
            && c.steps.is_empty()
            && now >= c.gap_until
        {
            self.finish_ok(now, now_ms);
        }
    }

    fn call_for(&self, step: &Step) -> Call {
        match step {
            Step::Whoami => Call::Whoami,
            Step::Pois => Call::Pois,
            Step::List { platform, poi, page } => {
                let (start, end) = match &self.state { State::Running(c) => c.window, _ => (0, 0) };
                let body = serde_json::json!({
                    "pageNo": page, "pageSize": PAGE_SIZE, "poiId": poi,
                    "startTime": start, "endTime": end,
                    "replyType": "全部", "starType": "全部", "sourceType": "全部", "userType": "全部",
                    "timeType": "全部",
                });
                Call::List { platform: *platform, body: body.to_string() }
            }
        }
    }

    fn last_success_ms(&self) -> Option<i64> {
        self.store.get_state("last_success").ok().flatten().and_then(|v| v.parse().ok())
    }

    /// 上一次取到的门店（评价页查询用第一家）
    pub fn cached_pois(&self) -> Vec<String> {
        self.store.get_state("pois").ok().flatten()
            .and_then(|v| serde_json::from_str::<Vec<String>>(&v).ok()).unwrap_or_default()
    }

    fn list_steps(&self, pois: &[String]) -> Vec<Step> {
        let mut v = Vec::new();
        for p in Platform::ALL {
            if !self.cfg.rules.platforms.contains(&p) { continue; }
            for poi in pois { v.push(Step::List { platform: p, poi: poi.clone(), page: 1 }); }
        }
        v
    }

    fn start_cycle(&mut self, now: Instant, now_ms: i64) {
        if self.cred.is_empty() {
            // 还没登录过（新装、升级后第一次、主动退出后）：直接等登录，不算掉线
            self.enter_login_lost(now, now_ms);
            return;
        }
        let p = &self.cfg.poll;
        let window = query_window(self.last_success_ms(), now_ms, p.overlap_minutes, p.first_run_lookback_hours);
        let initialized = self.store.get_state("initialized").ok().flatten().is_some();
        let baseline = !initialized && !self.cfg.rules.alert_existing_on_first_run;
        // 不再每轮先问 whoami：列表接口没登录时自己会说（401 / 「您尚未登录」），出了别的怪错再问。每轮少一个请求
        let mut steps = VecDeque::new();
        let pois = self.cached_pois();
        if pois.is_empty() { steps.push_back(Step::Pois); } else { steps.extend(self.list_steps(&pois)); }
        self.state = State::Running(Cycle { started_ms: now_ms, window, baseline, steps, waiting: None, gap_until: now, new_bad: 0, verify: None });
        self.status.busy = true;
        self.status.next_at = None;
        self.status.message = "正在检查…".into();
    }

    pub fn on_reply(&mut self, r: &HttpReply, now: Instant, now_ms: i64) {
        // 和浏览器一样：接口续发的 cookie 记下来
        if !r.set_cookie.is_empty() && !self.cred.is_empty() {
            let mut changed = false;
            for l in &r.set_cookie { changed |= self.cred.apply_set_cookie(l, now_ms); }
            if changed { self.save_credentials(); }
        }
        // 掉线状态下的探测回音
        if let State::LoginLost { probe, .. } = &mut self.state {
            if probe.is_some() && probe.as_deref() == Some(r.req_id.as_str()) {
                *probe = None;
                if whoami_ok(r) { self.login_restored(now, now_ms); }
            }
            return;
        }
        let State::Running(c) = &mut self.state else { return };
        let Some(w) = &c.waiting else { return };
        if r.req_id != w.req { return; }
        let step = w.step.clone();
        c.waiting = None;
        c.gap_until = now + Duration::from_millis(self.rng.random_range(1000..=3000));

        if let Some(err) = &r.error {
            let what = if err == "timeout" { "接口没有响应（超时）".to_string() } else { "连不上经营宝（网络错误）".to_string() };
            self.fail(now, now_ms, err, &what);
            return;
        }
        let status = r.status.unwrap_or(0);
        let body = r.body.as_str();
        match step {
            Step::Whoami => {
                if !whoami_ok(r) { self.enter_login_lost(now, now_ms); return; }
                self.status.login = Login::In;
                self.settle_login_lost();
                // 登录还在：刚才那个错就是接口本身的问题
                let pending = match &mut self.state { State::Running(c) => c.verify.take(), _ => None };
                if let Some((kind, detail)) = pending { self.fail(now, now_ms, &kind, &detail); }
            }
            Step::Pois => {
                let pois = extract_pois(body);
                if status != 200 || pois.is_empty() {
                    self.step_failed(now, now_ms, status, body, "pois", "没取到门店列表".into());
                    return;
                }
                let _ = self.store.set_state("pois", &serde_json::to_string(&pois).unwrap_or_default());
                let steps = self.list_steps(&pois);
                if let State::Running(c) = &mut self.state { c.steps.extend(steps); }
            }
            Step::List { platform, poi, page: pno } => {
                if status != 200 {
                    self.step_failed(now, now_ms, status, body, "http", format!("{} 评价接口返回 HTTP {status}", platform.label()));
                    return;
                }
                let parsed = match parse_list(platform, body) {
                    Ok(p) => p,
                    Err(e) => { self.step_failed(now, now_ms, status, body, "shape", format!("{} 评价接口：{e}", platform.label())); return; }
                };
                // 拿到了正常数据，登录当然还在（掉线后恢复的「已恢复」也在这里补发）
                self.status.login = Login::In;
                self.settle_login_lost();
                let rules = self.cfg.rules();
                let ids = self.disp.ids();
                let State::Running(c) = &mut self.state else { return };
                match self.store.ingest(&parsed.reviews, &rules, &ids, c.baseline, now_ms) {
                    Ok(fresh) => { if !c.baseline { c.new_bad += fresh.len() as u32; } }
                    Err(e) => tracing::error!("入库失败: {e:#}"),
                }
                self.list_dirty = true;
                if pno * PAGE_SIZE < parsed.total && pno < self.cfg.poll.max_pages && !parsed.reviews.is_empty() {
                    c.steps.push_front(Step::List { platform, poi, page: pno + 1 });
                }
            }
        }
    }

    fn finish_ok(&mut self, now: Instant, now_ms: i64) {
        let State::Running(c) = &self.state else { return };
        let (started, new_bad, baseline) = (c.started_ms, c.new_bad, c.baseline);
        let _ = self.store.set_state("last_success", &started.to_string());
        let _ = self.store.set_state("initialized", "1");
        self.failures = 0;
        self.status.login = Login::In;
        self.status.last_ok = Local.timestamp_millis_opt(now_ms).single();
        self.status.message = if baseline { "首次检查完成（已有评价只入列表，不提醒）".into() }
                              else if new_bad > 0 { format!("检查完成：发现 {new_bad} 条新差评") }
                              else { "检查完成：没有新差评".into() };
        self.flush(now, now_ms);
        let d = next_delay(self.cfg.poll.interval_minutes, self.cfg.poll.jitter_percent, &mut self.rng);
        self.schedule_next(now, d);
    }

    fn schedule_next(&mut self, now: Instant, d: Duration) {
        self.state = State::Idle { next: now + d };
        self.status.busy = false;
        self.status.next_at = Some(now + d);
    }

    fn fail(&mut self, now: Instant, now_ms: i64, kind: &str, detail: &str) {
        self.failures += 1;
        tracing::warn!("检查失败 [{kind}] 第 {} 次: {detail}", self.failures);
        self.status.message = format!("检查失败：{detail}");
        if self.failures >= API_ALERT_AFTER {
            let k = format!("apierr:{kind}");
            let last: i64 = self.store.get_state(&k).ok().flatten().and_then(|v| v.parse().ok()).unwrap_or(0);
            if now_ms - last >= API_ALERT_DEDUPE_MS {
                let _ = self.store.set_state(&k, &now_ms.to_string());
                self.disp.system(&Alert::system(AlertKind::ApiError, "差评监控：检查连续失败",
                    format!("{detail}。接口可能有变化，请打开程序看看。")));
            }
        }
        self.schedule_next(now, backoff(self.failures, self.cfg.poll.interval_minutes));
    }

    /// 门店 / 列表接口没给出正常数据。明说没登录（401、303）就是掉线；
    /// 别的错（比如会话过期被跳到一个 HTML 页面）先问一次 whoami，登录还在才按接口出错算。
    fn step_failed(&mut self, now: Instant, now_ms: i64, status: u16, body: &str, kind: &str, detail: String) {
        if is_login_lost(Some(status), body) {
            self.enter_login_lost(now, now_ms);
            return;
        }
        if let State::Running(c) = &mut self.state {
            c.verify = Some((kind.to_string(), detail));
            c.steps.clear();
            c.steps.push_front(Step::Whoami);
            return;
        }
        self.fail(now, now_ms, kind, &detail);
    }

    fn enter_login_lost(&mut self, now: Instant, now_ms: i64) {
        if matches!(self.state, State::LoginLost { .. }) { return; }
        let was = self.status.login;
        let every = self.probe_every(now);
        self.state = State::LoginLost { next_probe: now + every, probe: None };
        self.status.login = Login::Out;
        self.status.busy = false;
        self.status.next_at = None;
        self.status.message = "需要登录：请打开程序登录经营宝（滑块请手动拖）".into();
        self.attention = true;
        // 一次掉线只提醒一次：标记存库里，程序被系统杀掉重启也不会再弹（手机上很常见）
        let notified = self.store.get_state(LOGIN_LOST_KEY).ok().flatten().is_some_and(|v| !v.is_empty());
        // 从没成功检查过（刚装好、还没登录过）：本来就要登录，不算「登录失效」，不提醒
        let ever = self.store.get_state("initialized").ok().flatten().is_some();
        // 手上根本没有 cookie（升级后第一次、刚退出登录）：也不是掉线
        if was != Login::Out && !notified && ever && !self.cred.is_empty() {
            let _ = self.store.set_state(LOGIN_LOST_KEY, &now_ms.to_string());
            self.disp.system(&Alert::system(AlertKind::LoginLost, "差评监控：需要重新登录",
                "后台登录已失效，监控已暂停。请打开程序登录。"));
        }
    }

    fn login_restored(&mut self, now: Instant, _now_ms: i64) {
        self.status.login = Login::In;
        self.attention = false;
        self.settle_login_lost();
        // 立刻检查；窗口从上次成功时间开始，补上掉线期间
        self.schedule_next(now, Duration::from_secs(2));
        self.status.message = "已登录，马上检查…".into();
    }

    /// 确认已登录：清掉「已提醒掉线」的标记，下次掉线才会再提醒。
    /// 只有发过「需要重新登录」才发「已恢复」，两条成对出现——包括掉线后程序被杀、重启时已经登录好了的情况。
    fn settle_login_lost(&mut self) {
        let notified = self.store.get_state(LOGIN_LOST_KEY).ok().flatten().is_some_and(|v| !v.is_empty());
        if !notified { return; }
        let manual = self.store.get_state(LOGIN_LOST_KEY).ok().flatten().as_deref() == Some(MANUAL_LOGOUT);
        let _ = self.store.set_state(LOGIN_LOST_KEY, "");
        if !manual && self.store.get_state("initialized").ok().flatten().is_some() {
            self.disp.system(&Alert::system(AlertKind::LoginRestored, "差评监控：已恢复", "登录成功，监控已恢复，正在补查掉线期间的评价。"));
        }
    }

    fn flush(&mut self, now: Instant, now_ms: i64) {
        self.last_flush = now;
        if let Err(e) = self.disp.flush(&mut self.store, now_ms) { tracing::error!("推送出错: {e:#}"); }
    }
}

/// getUserInfo 是否表示已登录。实测已登录时返回
/// `{"user":{"id":100000001,"login":"demo_sub",...}}`；兼容 `data` 包一层的格式。
/// 登录失效时接口会跳转到登录页（拿到的是 HTML）或返回错误码，都判为没登录。
pub fn whoami_ok(r: &HttpReply) -> bool {
    if r.error.is_some() || r.status != Some(200) { return false; }
    let Ok(v) = serde_json::from_str::<serde_json::Value>(&r.body) else { return false };
    let code_bad = ["code", "status"].iter().any(|k| {
        v.get(*k).and_then(|c| c.as_i64()).is_some_and(|c| c != 0 && c != 200)
    });
    let user = v.get("user").or_else(|| v.get("data").and_then(|d| d.get("user"))).or_else(|| v.get("data"));
    let has_user = user.is_some_and(|u| {
        u.is_object() && ["login", "id", "userId", "accountName"].iter().any(|k| u.get(*k).is_some_and(|x| !x.is_null()))
    });
    !code_bad && has_user
}

/// 从 getCityPoiListSimple 响应里递归收集门店 ID（poiId / poiIdStr）
pub fn extract_pois(body: &str) -> Vec<String> {
    fn walk(v: &serde_json::Value, out: &mut Vec<String>) {
        match v {
            serde_json::Value::Object(m) => {
                for (k, x) in m {
                    if k == "poiId" || k == "poiIdStr" {
                        let id = match x { serde_json::Value::Number(n) => n.to_string(), serde_json::Value::String(s) => s.clone(), _ => String::new() };
                        if !id.is_empty() && id != "0" && !out.contains(&id) { out.push(id); }
                    } else { walk(x, out); }
                }
            }
            serde_json::Value::Array(a) => a.iter().for_each(|x| walk(x, out)),
            _ => {}
        }
    }
    let mut out = Vec::new();
    if let Ok(v) = serde_json::from_str(body) { walk(&v, &mut out); }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::notify::Notifier;
    use std::cell::RefCell;
    use std::sync::{Arc, Mutex};

    #[derive(Default)]
    struct FakeNet { sent: RefCell<Vec<(String, HttpRequest)>> }
    impl Transport for FakeNet {
        fn send(&self, req_id: &str, req: HttpRequest) { self.sent.borrow_mut().push((req_id.into(), req)); }
    }
    impl FakeNet {
        fn last(&self) -> (String, HttpRequest) { self.sent.borrow().last().cloned().unwrap() }
        fn n(&self) -> usize { self.sent.borrow().len() }
        /// 最后一个请求是哪个接口
        fn api(&self) -> &'static str {
            let u = self.last().1.url;
            if u.contains("getUserInfo") { "whoami" } else if u.contains("getCityPoiListSimple") { "poi" }
            else if u.contains("queryMTFeedback") { "mt" } else if u.contains("queryDPFeedback") { "dp" } else { "?" }
        }
    }

    struct Rec(Arc<Mutex<Vec<Alert>>>);
    impl Notifier for Rec {
        fn id(&self) -> &'static str { "toast" }
        fn send(&self, a: &Alert) -> anyhow::Result<()> { self.0.lock().unwrap().push(a.clone()); Ok(()) }
    }

    const MT: &str = include_str!("../tests/fixtures/mt_list.json");
    const DP: &str = include_str!("../tests/fixtures/dp_list.json");
    // 实测的 getUserInfo 已登录响应
    const WHO: &str = r#"{"user":{"id":100000001,"login":"demo_sub","bizacctid":100000001,"aclUserID":null,"userId":100000001}}"#;
    const POIS: &str = r#"{"data":{"data":[{"cityName":"武汉","poiList":[{"poiId":10000001,"poiName":"店"}]}]}}"#;
    /// 登录失效时 whoami 被跳到登录页
    const LOGIN_HTML: &str = "<!doctype html><title>登录</title>";
    const COOKIE: &str = "com.sankuai.meishi.fe.kdb-bsid=abc; _lxsdk=1";

    fn reply(req: &str, body: &str) -> HttpReply {
        HttpReply { req_id: req.into(), status: Some(200), body: body.into(), ..Default::default() }
    }

    struct H { p: Poller, net: FakeNet, t: Instant, ms: i64, alerts: Arc<Mutex<Vec<Alert>>> }
    impl H {
        fn with_store(store: Store, alerts: Arc<Mutex<Vec<Alert>>>) -> H {
            let t = Instant::now();
            let disp = Dispatcher::new(vec![Box::new(Rec(alerts.clone()))]);
            H { p: Poller::new(Config::default(), store, disp, t), net: FakeNet::default(), t, ms: 1_794_400_000_000, alerts }
        }
        /// 已经登录过（有 cookie）
        fn new() -> H {
            let mut h = H::with_store(Store::open_in_memory().unwrap(), Arc::new(Mutex::new(vec![])));
            assert!(h.p.set_credentials(COOKIE, "UA/1", h.t));
            h
        }
        fn adv(&mut self, s: u64) { self.t += Duration::from_secs(s); self.ms += s as i64 * 1000; self.p.tick(&self.net, self.t, self.ms); }
        fn feed(&mut self, r: HttpReply) { self.p.on_reply(&r, self.t, self.ms); }
        /// 回应最后一个请求，再走 4 秒
        fn answer(&mut self, body: &str) { let (r, _) = self.net.last(); self.feed(reply(&r, body)); self.adv(4); }
        fn titles(&self) -> Vec<String> { self.alerts.lock().unwrap().iter().map(|a| a.title.clone()).collect() }
        /// 回应最后一个请求一个 HTTP 状态码（正文为空），再走 4 秒
        fn answer_status(&mut self, code: u16) {
            let (r, _) = self.net.last();
            self.feed(HttpReply { status: Some(code), ..reply(&r, "") });
            self.adv(4);
        }
        /// 会话失效：接口全被跳到登录页（拿到 HTML）。列表 / 门店出错后会补问 whoami，也是 HTML → 掉线
        fn lose(&mut self) {
            for _ in 0..2 {
                if self.p.status.login == Login::Out { return; }
                self.answer(LOGIN_HTML);
            }
            assert_eq!(self.p.status.login, Login::Out);
        }
        fn full_cycle(&mut self, mt: &str, dp: &str) {
            assert_ne!(self.net.api(), "whoami", "正常一轮不问 whoami");
            if self.net.api() == "poi" { self.answer(POIS); }
            assert_eq!(self.net.api(), "mt");
            self.answer(mt);
            assert_eq!(self.net.api(), "dp");
            self.answer(dp);
            // 点评样本 total=24 → 还有第 2 页，回一个空页结束翻页
            while self.p.status.busy {
                assert!(self.net.last().1.body.unwrap().contains("\"pageNo\":2"));
                self.answer(r#"{"data":{"data":{"total":24,"feedbacks":[]}}}"#);
            }
            self.adv(1);
        }
    }

    fn header<'a>(r: &'a HttpRequest, k: &str) -> Option<&'a str> {
        r.headers.iter().find(|(n, _)| *n == k).map(|(_, v)| v.as_str())
    }

    #[test]
    fn requests_carry_credentials() {
        let mut h = H::new();
        h.adv(4);
        let (_, w) = h.net.last();
        assert_eq!(w.method, "GET");
        assert_eq!(w.url, format!("{BASE}/emis/gw/rpc/TFeedbackListNewService/getCityPoiListSimple?_tm={}", h.ms));
        assert_eq!(header(&w, "Cookie"), Some("_lxsdk=1; com.sankuai.meishi.fe.kdb-bsid=abc"));
        assert_eq!(header(&w, "User-Agent"), Some("UA/1"));
        assert_eq!(header(&w, "Referer"), Some(REFERER));
        assert_eq!(header(&w, "Content-Type"), None);
        h.answer(POIS);
        let (_, l) = h.net.last();
        assert_eq!(l.method, "POST");
        assert!(l.url.contains("queryMTFeedbackPCNew?_tm="));
        assert_eq!(header(&l, "Content-Type"), Some("application/json"));
        assert_eq!(header(&l, "Origin"), Some(BASE));
    }

    #[test]
    fn first_cycle_is_baseline_then_new_review_alerts() {
        let mut h = H::new();
        h.adv(4);
        h.full_cycle(MT, DP);
        assert!(h.titles().is_empty(), "首次只做基线：{:?}", h.titles());
        assert_eq!(h.p.status.login, Login::In);
        assert!(!h.p.status.busy);
        assert_eq!(h.p.store.recent(100, true).unwrap().len(), 4, "mt 3 条差评 + dp 1 条（2.5）");

        // 第二轮：点评多了一条 1 星
        h.adv(13 * 60);
        let dp2 = DP.replace("\"feedbacks\":[", "\"feedbacks\":[{\"addTime\":1794400000000,\"score\":1,\"feedbackId\":99,\"content\":\"太差\",\"replyCount\":0},");
        h.full_cycle(MT, &dp2);
        assert_eq!(h.titles(), vec!["点评 1★ 新差评".to_string()]);
        // 第三轮重复数据不再提醒
        h.adv(13 * 60);
        h.full_cycle(MT, &dp2);
        assert_eq!(h.titles().len(), 1);
    }

    #[test]
    fn list_request_uses_window_and_poi() {
        let mut h = H::new();
        h.adv(4);
        h.answer(POIS);
        let body = h.net.last().1.body.unwrap();
        let v: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(v["poiId"], "10000001");
        assert_eq!(v["starType"], "全部");
        let span = v["endTime"].as_i64().unwrap() - v["startTime"].as_i64().unwrap();
        assert_eq!(span, 48 * 3_600_000, "首次回看 48 小时");
    }

    #[test]
    fn paginates_by_total() {
        let mut h = H::new();
        h.adv(4);
        h.answer(POIS);
        // 美团 total=45 → 需要 3 页
        let mt = MT.replace("\"total\":7", "\"total\":45");
        h.answer(&mt);
        assert_eq!(h.net.api(), "mt");
        assert!(h.net.last().1.body.unwrap().contains("\"pageNo\":2"));
        h.answer(&mt);
        assert!(h.net.last().1.body.unwrap().contains("\"pageNo\":3"));
        h.answer(&mt);
        assert_eq!(h.net.api(), "dp");
    }

    #[test]
    fn without_credentials_waits_silently_then_starts() {
        // 新装 / 升级后第一次：没有 cookie → 直接等登录，不发请求、不提醒（哪怕以前成功检查过）
        let mut h = H::with_store(Store::open_in_memory().unwrap(), Arc::new(Mutex::new(vec![])));
        let _ = h.p.store.set_state("initialized", "1");
        h.adv(4);
        assert_eq!(h.p.status.login, Login::Out);
        assert!(h.p.attention);
        h.adv(30 * 60);
        assert_eq!(h.net.n(), 0, "没有 cookie 不敲接口");
        assert!(h.titles().is_empty());
        // 宿主从登录网页取到 cookie → 马上探测 → 恢复 → 开始检查
        assert!(!h.p.set_credentials("  ", "UA", h.t), "空的不收");
        assert!(h.p.set_credentials(COOKIE, "UA/2", h.t));
        h.adv(1);
        assert_eq!(h.net.api(), "whoami");
        assert_eq!(header(&h.net.last().1, "User-Agent"), Some("UA/2"));
        h.answer(WHO);
        assert_eq!(h.p.status.login, Login::In);
        assert!(h.titles().is_empty(), "不是掉线恢复，不发「已恢复」");
        h.adv(1);
        assert_eq!(h.net.api(), "poi", "开始新一轮（还没门店缓存，先取门店）");
        assert!(h.p.status.busy);
    }

    #[test]
    fn list_401_pauses_alerts_once_then_restores() {
        let mut h = H::new();
        h.adv(4);
        h.full_cycle(MT, DP);
        h.adv(13 * 60);
        assert_eq!(h.net.api(), "mt", "有门店缓存：一上来就查列表");
        let n = h.net.n();
        h.answer_status(401); // 会话失效：列表接口直接说没登录
        assert_eq!(h.net.n(), n, "明说没登录就不用再问 whoami");
        assert_eq!(h.p.status.login, Login::Out);
        assert!(h.p.attention);
        assert_eq!(h.titles(), vec!["差评监控：需要重新登录".to_string()]);
        // 掉线期间只探测 whoami，而且慢慢来
        let n = h.net.n();
        for _ in 0..60 { h.adv(60); }
        let probes = h.net.sent.borrow()[n..].to_vec();
        assert!(probes.iter().all(|(_, r)| r.url.contains("getUserInfo")));
        assert!((10..=13).contains(&probes.len()), "一小时约 12 次（5 分钟一次）：{}", probes.len());
        // 用户重新登录，宿主交来新 cookie → 马上探测 → 恢复并立即检查
        h.p.set_credentials("com.sankuai.meishi.fe.kdb-bsid=new", "", h.t);
        h.adv(1);
        assert_eq!(h.net.api(), "whoami");
        assert_eq!(header(&h.net.last().1, "Cookie"), Some("com.sankuai.meishi.fe.kdb-bsid=new"));
        assert_eq!(header(&h.net.last().1, "User-Agent"), Some("UA/1"), "没给新 UA 就沿用旧的");
        h.answer(WHO);
        assert_eq!(h.p.status.login, Login::In);
        assert!(h.titles().contains(&"差评监控：已恢复".to_string()));
        h.adv(3);
        assert_eq!(h.net.api(), "mt", "恢复后马上开始新一轮");
        assert!(h.p.status.busy);
    }

    #[test]
    fn odd_list_error_asks_whoami_before_blaming_api() {
        let mut h = H::new();
        h.adv(4);
        h.full_cycle(MT, DP);
        // 接口回了个看不懂的东西：先问 whoami；登录还在 → 算接口出错，不算掉线
        h.adv(13 * 60);
        h.answer(r#"{"code":500,"msg":"系统繁忙"}"#);
        assert_eq!(h.net.api(), "whoami");
        h.answer(WHO);
        assert_eq!(h.p.status.login, Login::In);
        assert!(!h.p.status.busy, "这一轮作废，等退避后重试");
        assert!(h.p.status.message.contains("接口"));
        // 会话过期被跳到登录页（200 + HTML）：whoami 也是 HTML → 掉线
        h.p.check_now(h.t);
        h.adv(1);
        h.lose();
        assert_eq!(h.titles(), vec!["差评监控：需要重新登录".to_string()]);
    }

    #[test]
    fn failed_probe_retries_fast_after_new_cookie() {
        let mut h = H::new();
        h.adv(4);
        h.lose();
        h.p.set_credentials("bsid=half-way", "", h.t);
        h.adv(1);
        h.answer(LOGIN_HTML); // 登录跳转还没走完，cookie 暂时无效
        let n = h.net.n();
        h.adv(16);
        assert_eq!(h.net.n(), n + 1, "刚交来 cookie 的几分钟里 15 秒重试一次");
    }

    #[test]
    fn login_lost_alert_survives_restart() {
        // 掉线 → 进程被杀 → 重启后 cookie 仍失效：不再重复提醒
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("d.db");
        let alerts = Arc::new(Mutex::new(vec![]));
        let mk = || H::with_store(Store::open(&db).unwrap(), alerts.clone());
        let mut h = mk();
        let _ = h.p.store.set_state("initialized", "1");
        h.p.set_credentials(COOKIE, "UA", h.t);
        h.adv(4);
        h.lose();
        assert_eq!(alerts.lock().unwrap().len(), 1);
        drop(h);
        for _ in 0..3 {
            let mut h = mk();
            assert!(h.p.has_credentials(), "cookie 存在库里，重启后还在");
            h.adv(4);
            h.lose();
        }
        assert_eq!(alerts.lock().unwrap().len(), 1, "重启 3 次只提醒过 1 次");

        // 再重启时已经登录好了 → 发「已恢复」并清标记；之后再掉线要重新提醒
        let mut h = mk();
        h.adv(4);
        h.full_cycle(MT, DP);
        assert_eq!(h.p.status.login, Login::In);
        h.adv(13 * 60);
        h.lose();
        let titles: Vec<String> = alerts.lock().unwrap().iter().map(|a| a.title.clone()).filter(|t| t.starts_with("差评监控")).collect();
        assert_eq!(titles, ["差评监控：需要重新登录", "差评监控：已恢复", "差评监控：需要重新登录"]);
    }

    #[test]
    fn manual_logout_is_silent_and_rearms() {
        // 主动退出：丢掉 cookie、不提醒，重启后也不提醒；重新登录不弹「已恢复」；之后真掉线照常提醒
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("d.db");
        let alerts = Arc::new(Mutex::new(vec![]));
        let mut h = H::with_store(Store::open(&db).unwrap(), alerts.clone());
        let _ = h.p.store.set_state("initialized", "1");
        h.p.set_credentials(COOKIE, "UA", h.t);
        h.p.logout(h.t, 0);
        assert_eq!(h.p.status.login, Login::Out);
        assert!(!h.p.has_credentials());
        h.adv(30 * 60);
        assert_eq!(h.net.n(), 0);
        drop(h);
        let mut h = H::with_store(Store::open(&db).unwrap(), alerts.clone());
        assert!(!h.p.has_credentials(), "退出后库里的 cookie 也清掉了");
        h.adv(4);
        assert!(alerts.lock().unwrap().is_empty(), "主动退出不提醒，重启后也不提醒");

        h.p.set_credentials(COOKIE, "UA", h.t);
        h.adv(1);
        h.answer(WHO);
        assert_eq!(h.p.status.login, Login::In);
        assert!(alerts.lock().unwrap().is_empty(), "重新登录不发「已恢复」");

        h.adv(1);
        h.lose();
        let titles: Vec<String> = alerts.lock().unwrap().iter().map(|a| a.title.clone()).collect();
        assert_eq!(titles, ["差评监控：需要重新登录"]);
    }

    #[test]
    fn logout_while_running_drops_pending_reply() {
        let mut h = H::new();
        h.adv(4);
        let (req, _) = h.net.last();
        h.p.logout(h.t, 0);
        // 退出前发出的请求晚到了：不应把状态改回已登录
        h.feed(reply(&req, WHO));
        assert_eq!(h.p.status.login, Login::Out);
    }

    #[test]
    fn set_cookie_in_replies_is_kept() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("d.db");
        let alerts = Arc::new(Mutex::new(vec![]));
        let mut h = H::with_store(Store::open(&db).unwrap(), alerts.clone());
        h.p.set_credentials(COOKIE, "UA", h.t);
        h.adv(4);
        let (r, _) = h.net.last();
        h.feed(HttpReply { set_cookie: vec!["_lxsdk=2; Path=/".into(), "fresh=1; Max-Age=86400".into()], ..reply(&r, POIS) });
        h.adv(4);
        assert_eq!(header(&h.net.last().1, "Cookie"), Some("_lxsdk=2; com.sankuai.meishi.fe.kdb-bsid=abc; fresh=1"));
        drop(h);
        let h = H::with_store(Store::open(&db).unwrap(), alerts);
        assert!(h.p.cred.header().contains("fresh=1"), "续发的 cookie 存进了库");
        assert!(!h.p.store.get_state(CRED_KEY).unwrap().unwrap().contains("bsid"), "库里不是明文（Windows 上）或至少是编码过的");
    }

    #[test]
    fn timeouts_back_off_and_alert_after_three() {
        let mut h = H::new();
        h.adv(4);
        for i in 0..3 {
            assert_eq!(h.net.api(), "poi");
            h.adv(25); // 超时
            assert!(!h.p.status.busy, "第 {i} 次超时后回到空闲");
            h.p.check_now(h.t);
            h.adv(1);
        }
        assert_eq!(h.titles(), vec!["差评监控：检查连续失败".to_string()]);
        // 6 小时内同类错误不重复提醒
        for _ in 0..3 { h.adv(25); h.p.check_now(h.t); h.adv(1); }
        assert_eq!(h.titles().len(), 1);
    }

    #[test]
    fn network_error_is_failure_not_logout() {
        let mut h = H::new();
        h.adv(4);
        let (r, _) = h.net.last();
        h.feed(HttpReply { req_id: r, error: Some("network".into()), ..Default::default() });
        assert!(h.p.status.message.contains("网络"));
        assert_ne!(h.p.status.login, Login::Out, "断网不等于掉线");
        assert!(!h.p.status.busy);
    }

    #[test]
    fn bad_shape_counts_as_failure() {
        let mut h = H::new();
        h.adv(4);
        h.answer(POIS);
        h.answer(r#"{"code":500,"msg":"x"}"#);
        h.answer(WHO);
        assert!(h.p.status.message.contains("接口"));
        assert!(!h.p.status.busy);
        // 正文里明说没登录（303 / 401）：直接算掉线
        h.p.check_now(h.t);
        h.adv(1);
        h.answer(r#"{"status":303,"message":"您尚未登录"}"#);
        assert_eq!(h.p.status.login, Login::Out);
    }

    #[test]
    fn stale_reply_ignored() {
        let mut h = H::new();
        h.adv(4);
        h.feed(reply("r999", WHO));
        assert!(h.p.status.busy, "不认识的 reqId 不影响当前轮");
    }

    #[test]
    fn whoami_and_pois_parsing() {
        assert!(whoami_ok(&reply("x", WHO)));
        assert!(whoami_ok(&reply("x", r#"{"data":{"accountName":"a"},"code":0}"#)));
        assert!(!whoami_ok(&reply("x", r#"{"code":401,"data":null}"#)));
        assert!(!whoami_ok(&reply("x", r#"{"user":null}"#)));
        assert!(!whoami_ok(&reply("x", r#"{"user":{}}"#)));
        assert!(!whoami_ok(&reply("x", LOGIN_HTML)));
        assert!(!whoami_ok(&HttpReply { status: Some(302), ..reply("x", WHO) }));
        assert_eq!(extract_pois(POIS), vec!["10000001".to_string()]);
        assert_eq!(extract_pois(r#"{"a":[{"poiIdStr":"1"},{"poiId":1},{"poiId":2}]}"#), vec!["1".to_string(), "2".to_string()]);
    }
}
