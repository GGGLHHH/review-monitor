//! 直连经营宝接口：宿主从 WebView2 的 cookie 库取登录 cookie（含 httpOnly 的 bsid），
//! 在后台线程直接发 HTTP 请求，不再经页面 XHR（也就不依赖 H5guard 的 mtgsig 签名）。
//! 回传的消息与 web/poller.js 的 postMessage 同形状，直接喂给 Poller::on_msg。
//!
//! 依据（docs/接口调研.md「直连验证」）：2026-11-13 实测 whoami / 美团 / 点评列表
//! 只带 cookie、不带 mtgsig 均返回 200 + 正常数据。
use rm_core::poller::{Call, IpcMsg};
use rm_core::Platform;
use std::time::Duration;

const BASE: &str = "https://ecom.meituan.com";
const UA: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/120.0.0.0 Safari/537.36";
/// 与 poller.js 的 XHR 超时一致
const TIMEOUT: Duration = Duration::from_secs(15);

/// 一次待发的 HTTP 请求（纯数据，便于测试）
pub struct Request {
    pub method: &'static str,
    pub url: String,
    pub body: Option<String>,
    /// 回传 IpcMsg 的 kind，与 poller.js 一致：whoami / poi / list
    pub kind: &'static str,
}

/// Call → HTTP 请求（自动补 `_tm` 时间戳参数）。
pub fn build(call: &Call, now_ms: i64) -> Request {
    let tm = format!("?_tm={now_ms}");
    match call {
        Call::Whoami => Request {
            method: "GET",
            url: format!("{BASE}/emis/gw/account/biz/getUserInfo{tm}"),
            body: None,
            kind: "whoami",
        },
        Call::Pois => Request {
            method: "GET",
            url: format!("{BASE}/emis/gw/rpc/TFeedbackListNewService/getCityPoiListSimple{tm}"),
            body: None,
            kind: "poi",
        },
        Call::List { platform, body } => {
            let svc = match platform {
                Platform::Meituan => "queryMTFeedbackPCNew",
                Platform::Dianping => "queryDPFeedbackPCNew",
            };
            Request {
                method: "POST",
                url: format!("{BASE}/emis/gw/rpc/TFeedbackListNewService/{svc}{tm}"),
                body: Some(body.clone()),
                kind: "list",
            }
        }
    }
}

/// 执行请求并回传 poller.js 同形状的消息。在后台线程调用（会阻塞网络 I/O）。
/// `cookie` 是 ecom.meituan.com 的完整 Cookie 头（可能为空串=未登录）。
pub fn perform(req_id: &str, call: &Call, cookie: &str) -> IpcMsg {
    let r = build(call, chrono::Local::now().timestamp_millis());
    // 与页面 XHR 一致：4xx/5xx 不当错误，原样回传 status + body 给轮询状态机
    let config = ureq::config::Config::builder()
        .timeout_global(Some(TIMEOUT))
        .http_status_as_error(false)
        .build();
    let agent = config.new_agent();
    let outcome = match r.method {
        "GET" => agent.get(&r.url).header("User-Agent", UA).header("Cookie", cookie).call(),
        _ => agent.post(&r.url)
            .header("Content-Type", "application/json")
            .header("User-Agent", UA)
            .header("Cookie", cookie)
            .send(r.body.as_deref().unwrap_or_default()),
    };
    match outcome {
        Ok(mut res) => {
            let status = res.status().as_u16();
            let body = res.body_mut().read_to_string().unwrap_or_default();
            tracing::debug!("直连 {} {} → {status}（{} 字节）", r.method, r.url, body.len());
            IpcMsg { req_id: Some(req_id.into()), kind: r.kind.into(), status: Some(status), body: Some(body), ..Default::default() }
        }
        Err(e) => {
            tracing::warn!("直连 {} 失败: {e}", r.url);
            IpcMsg {
                req_id: Some(req_id.into()),
                kind: r.kind.into(),
                error: Some(if matches!(e, ureq::Error::Timeout(_)) { "timeout".into() } else { "network".into() }),
                ..Default::default()
            }
        }
    }
}

/// cookie 库暂时读不到时的即时失败消息（不等 20 秒超时）
pub fn cookie_unavailable(req_id: &str, call: &Call) -> IpcMsg {
    IpcMsg { req_id: Some(req_id.into()), kind: build(call, 0).kind.into(), error: Some("network".into()), ..Default::default() }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_shapes_match_poller_js() {
        let w = build(&Call::Whoami, 1_794_400_000_000);
        assert_eq!((w.method, w.kind), ("GET", "whoami"));
        assert!(w.url.ends_with("/emis/gw/account/biz/getUserInfo?_tm=1794400000000"), "{}", w.url);
        assert!(w.body.is_none());

        let p = build(&Call::Pois, 7);
        assert!(p.url.ends_with("getCityPoiListSimple?_tm=7"), "{}", p.url);
        assert_eq!(p.kind, "poi");

        let d = build(&Call::List { platform: Platform::Dianping, body: "{\"pageNo\":1}".into() }, 5);
        assert_eq!((d.method, d.kind), ("POST", "list"));
        assert!(d.url.ends_with("queryDPFeedbackPCNew?_tm=5"), "{}", d.url);
        assert_eq!(d.body.as_deref(), Some(r#"{"pageNo":1}"#));

        let m = build(&Call::List { platform: Platform::Meituan, body: "{}".into() }, 5);
        assert!(m.url.contains("queryMTFeedbackPCNew"));
    }
}
