//! 直连经营宝接口：Poller 拼好请求（带登录 cookie），由 [`Transport`] 发出去，结果以 [`HttpReply`] 交回。
//!
//! 依据（docs/接口调研.md「直连」）：2026-11-13 实测 whoami / 美团 / 点评列表只带 cookie、
//! 不带页面 H5guard 生成的 `mtgsig` 签名，都返回 200 和正常数据。所以登录以后不再需要网页。
//! 电脑版和安卓版共用这一份（安卓经 rm-ffi 调用）。
use std::sync::mpsc::{channel, Receiver, Sender};
use std::time::Duration;

pub const BASE: &str = "https://ecom.meituan.com";
/// 网页里评价模块所在的地址；直连请求的 Referer 用它，和页面里发出的请求一致
pub const REFERER: &str = "https://ecom.meituan.com/emis/evaluation/poi";
/// 还没从登录网页拿到真实 UA 时的兜底值
pub const FALLBACK_UA: &str =
    "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/140.0.0.0 Safari/537.36";
const TIMEOUT: Duration = Duration::from_secs(15);

#[derive(Debug, Clone, PartialEq)]
pub struct HttpRequest {
    pub method: &'static str,
    pub url: String,
    pub headers: Vec<(&'static str, String)>,
    pub body: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct HttpReply {
    pub req_id: String,
    pub status: Option<u16>,
    pub body: String,
    /// 响应里的 Set-Cookie（原样），Poller 用它更新保存的登录 cookie
    pub set_cookie: Vec<String>,
    /// "timeout" / "network"；有它时 status、body 无意义
    pub error: Option<String>,
}

/// 发请求的能力。实现方必须异步：`send` 立刻返回，结果稍后交回 Poller。
pub trait Transport {
    fn send(&self, req_id: &str, req: HttpRequest);
}

/// 同步发一个请求（阻塞，放在后台线程里调）。4xx/5xx 不算错误，原样带回状态码和正文。
pub fn perform(agent: &ureq::Agent, req_id: &str, r: &HttpRequest) -> HttpReply {
    let outcome = if r.method == "GET" {
        let mut b = agent.get(&r.url);
        for (k, v) in &r.headers { b = b.header(*k, v); }
        b.call()
    } else {
        let mut b = agent.post(&r.url);
        for (k, v) in &r.headers { b = b.header(*k, v); }
        b.send(r.body.as_deref().unwrap_or_default())
    };
    match outcome {
        Ok(mut res) => {
            let status = res.status().as_u16();
            let set_cookie = res.headers().get_all("set-cookie").iter()
                .filter_map(|v| v.to_str().ok().map(String::from)).collect();
            let body = res.body_mut().read_to_string().unwrap_or_default();
            tracing::debug!("直连 {} {} → {status}（{} 字节）", r.method, strip_query(&r.url), body.len());
            HttpReply { req_id: req_id.into(), status: Some(status), body, set_cookie, error: None }
        }
        Err(e) => {
            tracing::warn!("直连 {} 失败: {e}", strip_query(&r.url));
            let kind = if matches!(e, ureq::Error::Timeout(_)) { "timeout" } else { "network" };
            HttpReply { req_id: req_id.into(), error: Some(kind.into()), ..Default::default() }
        }
    }
}

fn strip_query(u: &str) -> &str {
    u.split('?').next().unwrap_or(u)
}

/// 每个请求开一个后台线程发；结果攒在通道里，宿主在主循环里 [`drain`](Self::drain) 交给 Poller。
pub struct ThreadTransport {
    agent: ureq::Agent,
    tx: Sender<HttpReply>,
    rx: Receiver<HttpReply>,
}

/// 统一的 HTTP 客户端设置：15 秒总超时，4xx/5xx 不当错误
pub fn agent() -> ureq::Agent {
    ureq::config::Config::builder()
        .timeout_global(Some(TIMEOUT))
        .http_status_as_error(false)
        .build()
        .new_agent()
}

/// 同步客户端：调用方自己保证在后台线程里用（安卓评价页在 IO 线程里调）
#[derive(Clone)]
pub struct Client(ureq::Agent);

impl Default for Client {
    fn default() -> Self {
        Self(agent())
    }
}

impl Client {
    pub fn call(&self, req_id: &str, r: &HttpRequest) -> HttpReply {
        perform(&self.0, req_id, r)
    }
}

impl Default for ThreadTransport {
    fn default() -> Self {
        let (tx, rx) = channel();
        Self { agent: agent(), tx, rx }
    }
}

impl ThreadTransport {
    /// 已经回来的结果（不阻塞）
    pub fn drain(&self) -> Vec<HttpReply> {
        self.rx.try_iter().collect()
    }
}

impl Transport for ThreadTransport {
    fn send(&self, req_id: &str, req: HttpRequest) {
        let (agent, tx, req_id) = (self.agent.clone(), self.tx.clone(), req_id.to_string());
        std::thread::spawn(move || { let _ = tx.send(perform(&agent, &req_id, &req)); });
    }
}
