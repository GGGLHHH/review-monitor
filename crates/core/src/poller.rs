//! 轮询状态机：whoami → (门店列表) → 各平台评价列表（翻页）→ 入库 → 推送。
//! 不直接接触 WebView：通过 `Page` 让宿主代发请求，宿主把 IPC 回传喂给 `on_msg`。
use crate::config::Config;
use crate::model::Platform;
use crate::notify::{Alert, AlertKind, Dispatcher};
use crate::parse::parse_list;
use crate::schedule::{backoff, next_delay, query_window};
use crate::store::Store;
use chrono::{DateTime, Local, TimeZone};
use rand::{rngs::StdRng, Rng, SeedableRng};
use serde::Deserialize;
use std::collections::VecDeque;
use std::time::{Duration, Instant};

pub const PAGE_SIZE: u32 = 20;
const REPLY_TIMEOUT: Duration = Duration::from_secs(20);
const API_ALERT_AFTER: u32 = 3;
const API_ALERT_DEDUPE_MS: i64 = 6 * 3_600_000;
const NO_FRAME_RETRY: Duration = Duration::from_secs(10);
const NO_FRAME_LIMIT: u32 = 6;
const LOGIN_PROBE_EVERY: Duration = Duration::from_secs(15);
const FLUSH_EVERY: Duration = Duration::from_secs(60);
const FIRST_CHECK_DELAY: Duration = Duration::from_secs(6);
/// state 表里记「已经发过需要重新登录」，空串表示没有
const LOGIN_LOST_KEY: &str = "login_lost_notified";
/// LOGIN_LOST_KEY 的特殊值：用户主动退出登录（不算掉线，不提醒，重新登录也不发「已恢复」）
const MANUAL_LOGOUT: &str = "manual";

/// 宿主要代发的请求
#[derive(Debug, Clone, PartialEq)]
pub enum Call {
    Whoami,
    Pois,
    List { platform: Platform, body: String },
}

/// 经营宝评价页（各端 WebView 的起始地址）
pub const START_URL: &str = "https://ecom.meituan.com/meishi/?cate=6103#https://ecom.meituan.com/emis/evaluation/poi";

impl Call {
    /// 让页面里的 poller.js 代发这个请求的 JS。`__rm` 还没注入（页面没载完）时，
    /// 按「不在评价页」回报，由 poller 稍后重试。电脑版和安卓版共用。
    pub fn to_js(&self, req_id: &str) -> String {
        let req = serde_json::to_string(req_id).unwrap_or_default();
        let inner = match self {
            Call::Whoami => format!("window.__rm.whoami({req})"),
            Call::Pois => format!("window.__rm.poiList({req})"),
            Call::List { platform, body } => {
                format!("window.__rm.fetchList({req},{},{body})", serde_json::to_string(platform.js_name()).unwrap_or_default())
            }
        };
        format!(
            "if(window.__rm){{{inner}}}else{{try{{window.ipc.postMessage(JSON.stringify({{reqId:{req},kind:'none',error:'no_iframe',href:location.href}}))}}catch(e){{}}}}"
        )
    }
}

/// 在评价页里定位一条评价并高亮（结果以 kind=reveal 的消息回传）
pub fn reveal_js(platform: Platform, feedback_id: i64) -> String {
    format!(
        "if(window.__rm&&window.__rm.reveal){{window.__rm.reveal('{}','{feedback_id}')}}\
         else{{window.ipc.postMessage(JSON.stringify({{kind:'reveal',error:'no_rm',href:location.href}}))}}",
        platform.js_name()
    )
}

/// 宿主（WebView）能力
pub trait Page {
    fn call(&self, req_id: &str, call: &Call);
    /// 让页面回到评价页
    fn goto_reviews(&self);
    /// 用户此刻是否正看着后台页（看着就别乱跳转）
    fn user_visible(&self) -> bool;
}

/// poller.js 回传的消息
#[derive(Debug, Clone, Default, Deserialize, PartialEq)]
pub struct IpcMsg {
    #[serde(rename = "reqId")]
    pub req_id: Option<String>,
    pub kind: String,
    pub status: Option<u16>,
    pub body: Option<String>,
    pub error: Option<String>,
    pub href: Option<String>,
    pub login: Option<bool>,
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
    no_frame: u32,
    seq: u64,
    last_flush: Instant,
    pub status: Status,
    /// 列表有变化，界面需要刷新
    pub list_dirty: bool,
    /// 需要用户处理（掉线），界面应切到后台页
    pub attention: bool,
}

impl Poller {
    pub fn new(cfg: Config, store: Store, disp: Dispatcher, now: Instant) -> Self {
        let last_ok = store.get_state("last_success").ok().flatten().and_then(|v| v.parse::<i64>().ok())
            .and_then(|ms| Local.timestamp_millis_opt(ms).single());
        Self {
            cfg, store, disp,
            rng: StdRng::from_os_rng(),
            state: State::Idle { next: now + FIRST_CHECK_DELAY },
            failures: 0, no_frame: 0, seq: 0,
            last_flush: now,
            status: Status { login: Login::Unknown, busy: false, last_ok, next_at: Some(now + FIRST_CHECK_DELAY), message: "等待后台页面载入…".into() },
            list_dirty: true,
            attention: false,
        }
    }

    pub fn config(&self) -> &Config {
        &self.cfg
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
            State::LoginLost { next_probe, .. } => { *next_probe = now; }
            State::Running(_) => {}
        }
    }

    /// 用户主动退出登录（界面已清掉登录状态）。监控暂停，等重新登录；不发任何提醒。
    pub fn logout(&mut self, now: Instant, _now_ms: i64) {
        let _ = self.store.set_state(LOGIN_LOST_KEY, MANUAL_LOGOUT);
        self.state = State::LoginLost { next_probe: now + LOGIN_PROBE_EVERY, probe: None };
        self.failures = 0;
        self.no_frame = 0;
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

    pub fn tick(&mut self, page: &dyn Page, now: Instant, now_ms: i64) {
        if now.duration_since(self.last_flush) >= FLUSH_EVERY {
            self.flush(now, now_ms);
        }
        match &mut self.state {
            State::Idle { next } => {
                if now >= *next { self.start_cycle(now, now_ms); }
            }
            State::LoginLost { next_probe, probe } => {
                if now >= *next_probe && probe.is_none() {
                    let req = self.next_req();
                    if let State::LoginLost { next_probe, probe } = &mut self.state {
                        *probe = Some(req.clone());
                        *next_probe = now + LOGIN_PROBE_EVERY;
                    }
                    page.call(&req, &Call::Whoami);
                } else if now >= *next_probe {
                    // 上一次探测没有回音（多半还在登录页），清掉重来
                    if let State::LoginLost { probe, .. } = &mut self.state { *probe = None; }
                }
            }
            State::Running(_) => {}
        }
        // 运行中：超时 / 发下一步
        let mut timed_out = false;
        let mut to_send: Option<(String, Step)> = None;
        if let State::Running(c) = &mut self.state {
            if let Some(w) = &c.waiting {
                if now >= w.deadline { timed_out = true; }
            } else if now >= c.gap_until
                && let Some(step) = c.steps.pop_front()
            {
                to_send = Some((String::new(), step));
            }
        }
        if timed_out {
            self.fail(now, now_ms, "timeout", "后台页面没有响应（超时）");
            return;
        }
        if let Some((_, step)) = to_send {
            let req = self.next_req();
            let call = self.call_for(&step);
            if let State::Running(c) = &mut self.state {
                c.waiting = Some(Waiting { req: req.clone(), step, deadline: now + REPLY_TIMEOUT });
            }
            page.call(&req, &call);
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

    fn cached_pois(&self) -> Vec<String> {
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
        let p = &self.cfg.poll;
        let window = query_window(self.last_success_ms(), now_ms, p.overlap_minutes, p.first_run_lookback_hours);
        let initialized = self.store.get_state("initialized").ok().flatten().is_some();
        let baseline = !initialized && !self.cfg.rules.alert_existing_on_first_run;
        let mut steps = VecDeque::from([Step::Whoami]);
        let pois = self.cached_pois();
        if pois.is_empty() { steps.push_back(Step::Pois); } else { steps.extend(self.list_steps(&pois)); }
        self.state = State::Running(Cycle { started_ms: now_ms, window, baseline, steps, waiting: None, gap_until: now, new_bad: 0 });
        self.status.busy = true;
        self.status.next_at = None;
        self.status.message = "正在检查…".into();
    }

    pub fn on_msg(&mut self, msg: &IpcMsg, page: &dyn Page, now: Instant, now_ms: i64) {
        if msg.kind == "nav" {
            let on_login = msg.login == Some(true);
            if on_login { self.enter_login_lost(now, now_ms); }
            else if matches!(self.state, State::LoginLost { .. }) {
                // 离开登录页了：几秒后探测是否已登录
                if let State::LoginLost { next_probe, probe } = &mut self.state {
                    *next_probe = now + Duration::from_secs(4);
                    *probe = None;
                }
            }
            return;
        }
        // 掉线状态下的探测回音
        if let State::LoginLost { probe, .. } = &self.state {
            if probe.is_some() && probe.as_deref() == msg.req_id.as_deref() {
                if msg.kind == "whoami" && whoami_ok(msg) {
                    self.login_restored(now, now_ms);
                } else {
                    if msg.error.as_deref() == Some("no_iframe") && !is_login_href(msg) && !page.user_visible() {
                        page.goto_reviews();
                    }
                    if let State::LoginLost { probe, .. } = &mut self.state { *probe = None; }
                }
            }
            return;
        }
        let State::Running(c) = &mut self.state else { return };
        let Some(w) = &c.waiting else { return };
        if msg.req_id.as_deref() != Some(w.req.as_str()) { return; }
        let step = w.step.clone();
        c.waiting = None;
        c.gap_until = now + Duration::from_millis(self.rng.random_range(1000..=3000));

        if let Some(err) = &msg.error {
            if err == "no_iframe" {
                if is_login_href(msg) { self.enter_login_lost(now, now_ms); return; }
                self.no_frame += 1;
                if self.no_frame <= NO_FRAME_LIMIT {
                    // 已经在评价页（iframe 多半还在加载）就先等；连等 3 次还没有才刷新
                    let on_reviews = msg.href.as_deref().is_some_and(|h| h.contains("evaluation/poi"));
                    if !page.user_visible() && (!on_reviews || self.no_frame.is_multiple_of(3)) { page.goto_reviews(); }
                    self.state = State::Idle { next: now + NO_FRAME_RETRY };
                    self.status.busy = false;
                    self.status.next_at = Some(now + NO_FRAME_RETRY);
                    self.status.message = if page.user_visible() { "后台页面不在评价页，稍后重试（点「回到评价页」可恢复）".into() }
                                          else { "等待评价页载入…".into() };
                    return;
                }
                self.fail(now, now_ms, "no_iframe", "后台页面一直不在评价页");
            } else {
                self.fail(now, now_ms, "network", &format!("请求失败：{err}"));
            }
            return;
        }
        self.no_frame = 0;
        let status = msg.status.unwrap_or(0);
        let body = msg.body.as_deref().unwrap_or("");
        match step {
            Step::Whoami => {
                if whoami_ok(msg) { self.status.login = Login::In; self.settle_login_lost(); }
                else { self.enter_login_lost(now, now_ms); }
            }
            Step::Pois => {
                let pois = extract_pois(body);
                if status != 200 || pois.is_empty() {
                    self.fail(now, now_ms, "pois", "没取到门店列表");
                    return;
                }
                let _ = self.store.set_state("pois", &serde_json::to_string(&pois).unwrap_or_default());
                let steps = self.list_steps(&pois);
                if let State::Running(c) = &mut self.state { c.steps.extend(steps); }
            }
            Step::List { platform, poi, page: pno } => {
                if status != 200 {
                    self.fail(now, now_ms, "http", &format!("{} 评价接口返回 HTTP {status}", platform.label()));
                    return;
                }
                let parsed = match parse_list(platform, body) {
                    Ok(p) => p,
                    Err(e) => { self.fail(now, now_ms, "shape", &format!("{} 评价接口：{e}", platform.label())); return; }
                };
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

    fn enter_login_lost(&mut self, now: Instant, _now_ms: i64) {
        if matches!(self.state, State::LoginLost { .. }) { return; }
        let was = self.status.login;
        self.state = State::LoginLost { next_probe: now + LOGIN_PROBE_EVERY, probe: None };
        self.status.login = Login::Out;
        self.status.busy = false;
        self.status.next_at = None;
        self.status.message = "需要登录：请在「后台页面」里登录（滑块请手动拖）".into();
        self.attention = true;
        // 一次掉线只提醒一次：标记存库里，程序被系统杀掉重启也不会再弹（手机上很常见）
        let notified = self.store.get_state(LOGIN_LOST_KEY).ok().flatten().is_some_and(|v| !v.is_empty());
        // 从没成功检查过（刚装好、还没登录过）：界面本来就停在登录页，不算「登录失效」，不提醒
        let ever = self.store.get_state("initialized").ok().flatten().is_some();
        if was != Login::Out && !notified && ever {
            let _ = self.store.set_state(LOGIN_LOST_KEY, &_now_ms.to_string());
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

fn is_login_href(m: &IpcMsg) -> bool {
    m.href.as_deref().is_some_and(|h| h.contains("/login.html") || h.contains("epassport"))
}

/// getUserInfo 是否表示已登录。实测已登录时返回
/// `{"user":{"id":100000001,"login":"demo_sub",...}}`；兼容 `data` 包一层的格式。
pub fn whoami_ok(m: &IpcMsg) -> bool {
    if m.error.is_some() || m.status != Some(200) { return false; }
    let Ok(v) = serde_json::from_str::<serde_json::Value>(m.body.as_deref().unwrap_or("")) else { return false };
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
mod js_tests {
    use super::*;

    #[test]
    fn call_js_shapes() {
        let w = Call::Whoami.to_js("r1");
        assert!(w.contains(r#"window.__rm.whoami("r1")"#), "{w}");
        assert!(w.contains(r#"reqId:"r1",kind:'none',error:'no_iframe'"#), "{w}");
        let l = Call::List { platform: Platform::Dianping, body: r#"{"pageNo":1}"#.into() }.to_js("r2");
        assert!(l.contains(r#"window.__rm.fetchList("r2","dianping",{"pageNo":1})"#), "{l}");
        // reqId 里的引号会被转义，不会拼出坏 JS
        assert!(Call::Pois.to_js("a\"b").contains(r#"poiList("a\"b")"#));
    }

    #[test]
    fn reveal_js_shape() {
        let j = reveal_js(Platform::Meituan, 10578232696);
        assert!(j.contains("window.__rm.reveal('meituan','10578232696')"), "{j}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::notify::Notifier;
    use std::cell::RefCell;
    use std::sync::{Arc, Mutex};

    #[derive(Default)]
    struct FakePage { calls: RefCell<Vec<(String, Call)>>, gotos: RefCell<u32> }
    impl Page for FakePage {
        fn call(&self, req_id: &str, call: &Call) { self.calls.borrow_mut().push((req_id.into(), call.clone())); }
        fn goto_reviews(&self) { *self.gotos.borrow_mut() += 1; }
        fn user_visible(&self) -> bool { false }
    }
    impl FakePage {
        fn last(&self) -> (String, Call) { self.calls.borrow().last().cloned().unwrap() }
        fn n(&self) -> usize { self.calls.borrow().len() }
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

    fn reply(req: &str, kind: &str, body: &str) -> IpcMsg {
        IpcMsg { req_id: Some(req.into()), kind: kind.into(), status: Some(200), body: Some(body.into()), ..Default::default() }
    }

    struct H { p: Poller, page: FakePage, t: Instant, ms: i64, alerts: Arc<Mutex<Vec<Alert>>> }
    impl H {
        fn new() -> H {
            let alerts = Arc::new(Mutex::new(vec![]));
            let t = Instant::now();
            let disp = Dispatcher::new(vec![Box::new(Rec(alerts.clone()))]);
            H { p: Poller::new(Config::default(), Store::open_in_memory().unwrap(), disp, t), page: FakePage::default(), t, ms: 1_794_400_000_000, alerts }
        }
        fn adv(&mut self, s: u64) { self.t += Duration::from_secs(s); self.ms += s as i64 * 1000; self.p.tick(&self.page, self.t, self.ms); }
        fn feed(&mut self, m: IpcMsg) { self.p.on_msg(&m, &self.page, self.t, self.ms); }
        /// 回应当前请求；返回发生的调用
        fn answer(&mut self, kind: &str, body: &str) { let (r, _) = self.page.last(); self.feed(reply(&r, kind, body)); self.adv(4); }
        fn titles(&self) -> Vec<String> { self.alerts.lock().unwrap().iter().map(|a| a.title.clone()).collect() }
        fn full_cycle(&mut self, mt: &str, dp: &str) {
            assert_eq!(self.page.last().1, Call::Whoami);
            self.answer("whoami", WHO);
            if self.page.last().1 == Call::Pois { self.answer("poi", POIS); }
            assert!(matches!(self.page.last().1, Call::List { platform: Platform::Meituan, .. }));
            self.answer("list", mt);
            assert!(matches!(self.page.last().1, Call::List { platform: Platform::Dianping, .. }));
            self.answer("list", dp);
            // 点评样本 total=24 → 还有第 2 页，回一个空页结束翻页
            while self.p.status.busy {
                assert!(matches!(&self.page.last().1, Call::List { body, .. } if body.contains("\"pageNo\":2")));
                self.answer("list", r#"{"data":{"data":{"total":24,"feedbacks":[]}}}"#);
            }
            self.adv(1);
        }
    }

    #[test]
    fn first_cycle_is_baseline_then_new_review_alerts() {
        let mut h = H::new();
        h.adv(7);
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
        h.adv(7);
        h.answer("whoami", WHO);
        h.answer("poi", POIS);
        let Call::List { body, .. } = h.page.last().1 else { panic!() };
        let v: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(v["poiId"], "10000001");
        assert_eq!(v["starType"], "全部");
        let span = v["endTime"].as_i64().unwrap() - v["startTime"].as_i64().unwrap();
        assert_eq!(span, 48 * 3_600_000, "首次回看 48 小时");
    }

    #[test]
    fn paginates_by_total() {
        let mut h = H::new();
        h.adv(7);
        h.answer("whoami", WHO);
        h.answer("poi", POIS);
        // 美团 total=45 → 需要 3 页
        let mt = MT.replace("\"total\":7", "\"total\":45");
        h.answer("list", &mt);
        let Call::List { platform, body } = h.page.last().1 else { panic!() };
        assert_eq!(platform, Platform::Meituan);
        assert!(body.contains("\"pageNo\":2"));
        h.answer("list", &mt);
        assert!(matches!(&h.page.last().1, Call::List { body, .. } if body.contains("\"pageNo\":3")));
        h.answer("list", &mt);
        assert!(matches!(h.page.last().1, Call::List { platform: Platform::Dianping, .. }));
    }

    #[test]
    fn login_page_pauses_and_alerts_once_then_restores() {
        let mut h = H::new();
        h.adv(7);
        h.full_cycle(MT, DP);
        h.feed(IpcMsg { kind: "nav".into(), login: Some(true), href: Some("https://ecom.meituan.com/bizaccount/login.html".into()), ..Default::default() });
        h.feed(IpcMsg { kind: "nav".into(), login: Some(true), ..Default::default() });
        assert_eq!(h.p.status.login, Login::Out);
        assert!(h.p.attention);
        assert_eq!(h.titles(), vec!["差评监控：需要重新登录".to_string()]);
        let n = h.page.n();
        h.adv(13 * 60);
        // 掉线期间不发列表请求
        assert!(h.page.calls.borrow()[n..].iter().all(|(_, c)| *c == Call::Whoami));
        // 用户登录后离开登录页 → 探测 whoami → 恢复并立即检查
        h.feed(IpcMsg { kind: "nav".into(), login: Some(false), ..Default::default() });
        h.adv(5);
        assert_eq!(h.page.last().1, Call::Whoami);
        h.answer("whoami", WHO);
        assert_eq!(h.p.status.login, Login::In);
        assert!(h.titles().contains(&"差评监控：已恢复".to_string()));
        h.adv(3);
        assert_eq!(h.page.last().1, Call::Whoami, "恢复后马上开始新一轮");
    }

    #[test]
    fn login_lost_alert_survives_restart() {
        // 掉线 → 进程被杀 → 重启后仍在登录页：不再重复提醒
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("d.db");
        let login_nav = || IpcMsg { kind: "nav".into(), login: Some(true), ..Default::default() };
        let alerts = Arc::new(Mutex::new(vec![]));
        let mk = |a: &Arc<Mutex<Vec<Alert>>>| {
            Poller::new(Config::default(), Store::open(&db).unwrap(), Dispatcher::new(vec![Box::new(Rec(a.clone()))]), Instant::now())
        };
        let page = FakePage::default();
        let mut p = mk(&alerts);
        let _ = p.store.set_state("initialized", "1");
        p.on_msg(&login_nav(), &page, Instant::now(), 0);
        assert_eq!(alerts.lock().unwrap().len(), 1);
        drop(p);
        for _ in 0..3 {
            let mut p = mk(&alerts);
            p.on_msg(&login_nav(), &page, Instant::now(), 0);
            assert_eq!(p.status.login, Login::Out);
        }
        assert_eq!(alerts.lock().unwrap().len(), 1, "重启 3 次只提醒过 1 次");

        // 再重启时已经登录好了（正常检查里 whoami 成功）→ 发「已恢复」并清标记；之后再掉线要重新提醒
        let mut p = mk(&alerts);
        let t = Instant::now() + Duration::from_secs(10);
        p.tick(&page, t, 0);
        let (req, call) = page.last();
        assert_eq!(call, Call::Whoami);
        p.on_msg(&reply(&req, "whoami", WHO), &page, t, 0);
        assert_eq!(p.status.login, Login::In);
        p.on_msg(&login_nav(), &page, t, 0);
        let titles: Vec<String> = alerts.lock().unwrap().iter().map(|a| a.title.clone()).collect();
        assert_eq!(titles, ["差评监控：需要重新登录", "差评监控：已恢复", "差评监控：需要重新登录"]);
    }

    #[test]
    fn fresh_install_does_not_alert_login_lost() {
        // 全新安装、从没登录过：停在登录页不弹「需要重新登录」；登录成功后也不弹「已恢复」
        let mut h = H::new();
        h.feed(IpcMsg { kind: "nav".into(), login: Some(true), ..Default::default() });
        assert_eq!(h.p.status.login, Login::Out);
        assert!(h.alerts.lock().unwrap().is_empty());
    }

    #[test]
    fn manual_logout_is_silent_and_rearms() {
        // 主动退出：不弹「需要重新登录」，重启后也不弹；重新登录不弹「已恢复」；之后真掉线照常提醒
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("d.db");
        let login_nav = || IpcMsg { kind: "nav".into(), login: Some(true), ..Default::default() };
        let alerts = Arc::new(Mutex::new(vec![]));
        let mk = |a: &Arc<Mutex<Vec<Alert>>>| {
            Poller::new(Config::default(), Store::open(&db).unwrap(), Dispatcher::new(vec![Box::new(Rec(a.clone()))]), Instant::now())
        };
        let page = FakePage::default();
        let mut p = mk(&alerts);
        let _ = p.store.set_state("initialized", "1");
        let t = Instant::now();
        p.logout(t, 0);
        assert_eq!(p.status.login, Login::Out);
        assert!(!p.status.busy);
        p.on_msg(&login_nav(), &page, t, 0);
        drop(p);
        let mut p = mk(&alerts);
        p.on_msg(&login_nav(), &page, t, 0);
        assert!(alerts.lock().unwrap().is_empty(), "主动退出不提醒，重启后也不提醒");

        // 用户重新登录：探测 whoami 成功 → 恢复检查，不发「已恢复」
        p.on_msg(&IpcMsg { kind: "nav".into(), login: Some(false), ..Default::default() }, &page, t, 0);
        let t = t + Duration::from_secs(5);
        p.tick(&page, t, 0);
        let (req, call) = page.last();
        assert_eq!(call, Call::Whoami);
        p.on_msg(&reply(&req, "whoami", WHO), &page, t, 0);
        assert_eq!(p.status.login, Login::In);
        assert!(alerts.lock().unwrap().is_empty(), "重新登录不发「已恢复」");

        // 之后真掉线：照常提醒
        p.on_msg(&login_nav(), &page, t, 0);
        let titles: Vec<String> = alerts.lock().unwrap().iter().map(|a| a.title.clone()).collect();
        assert_eq!(titles, ["差评监控：需要重新登录"]);
    }

    #[test]
    fn logout_while_running_drops_pending_reply() {
        let mut h = H::new();
        h.adv(7);
        let (req, call) = h.page.last();
        assert_eq!(call, Call::Whoami);
        h.p.logout(h.t, 0);
        // 退出前发出的请求晚到了：不应把状态改回已登录
        h.p.on_msg(&reply(&req, "whoami", WHO), &h.page, h.t, 0);
        assert_eq!(h.p.status.login, Login::Out);
    }

    #[test]
    fn timeouts_back_off_and_alert_after_three() {
        let mut h = H::new();
        h.adv(7);
        for i in 0..3 {
            assert_eq!(h.page.last().1, Call::Whoami);
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
    fn bad_shape_counts_as_failure() {
        let mut h = H::new();
        h.adv(7);
        h.answer("whoami", WHO);
        h.answer("poi", POIS);
        let (r, _) = h.page.last();
        h.feed(reply(&r, "list", r#"{"code":401,"msg":"未登录"}"#));
        assert!(h.p.status.message.contains("接口"));
        assert!(!h.p.status.busy);
    }

    #[test]
    fn no_iframe_redirects_and_retries() {
        let mut h = H::new();
        h.adv(7);
        let (r, _) = h.page.last();
        h.feed(IpcMsg { req_id: Some(r), kind: "whoami".into(), error: Some("no_iframe".into()), href: Some("https://ecom.meituan.com/meishi/".into()), ..Default::default() });
        assert_eq!(*h.page.gotos.borrow(), 1);
        h.adv(11);
        assert_eq!(h.page.last().1, Call::Whoami, "10 秒后重试");
        assert!(h.titles().is_empty());
    }

    #[test]
    fn no_iframe_on_reviews_page_waits_before_reload() {
        let mut h = H::new();
        h.adv(7);
        for i in 1..=3 {
            let (r, _) = h.page.last();
            h.feed(IpcMsg { req_id: Some(r), kind: "whoami".into(), error: Some("no_iframe".into()),
                            href: Some(crate_start_url().into()), ..Default::default() });
            assert_eq!(*h.page.gotos.borrow(), if i < 3 { 0 } else { 1 }, "第 {i} 次");
            h.adv(11);
        }
    }
    fn crate_start_url() -> &'static str { "https://ecom.meituan.com/meishi/?cate=6103#https://ecom.meituan.com/emis/evaluation/poi" }

    #[test]
    fn stale_reply_ignored() {
        let mut h = H::new();
        h.adv(7);
        h.feed(reply("r999", "whoami", WHO));
        assert!(h.p.status.busy, "不认识的 reqId 不影响当前轮");
    }

    #[test]
    fn whoami_and_pois_parsing() {
        assert!(whoami_ok(&reply("x", "whoami", WHO)));
        assert!(whoami_ok(&reply("x", "whoami", r#"{"data":{"accountName":"a"},"code":0}"#)));
        assert!(!whoami_ok(&reply("x", "whoami", r#"{"code":401,"data":null}"#)));
        assert!(!whoami_ok(&reply("x", "whoami", r#"{"user":null}"#)));
        assert!(!whoami_ok(&reply("x", "whoami", r#"{"user":{}}"#)));
        assert!(!whoami_ok(&reply("x", "whoami", "<html>")));
        assert_eq!(extract_pois(POIS), vec!["10000001".to_string()]);
        assert_eq!(extract_pois(r#"{"a":[{"poiIdStr":"1"},{"poiId":1},{"poiId":2}]}"#), vec!["1".to_string(), "2".to_string()]);
    }
}
