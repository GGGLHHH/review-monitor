//! 后台页面：wry(WebView2) 作为子窗口嵌在 Slint 主窗口的内容区，注入 web/poller.js。
//! 只负责登录界面与定位评价；评价数据由 Poller 经 [`crate::direct`] 直连接口拉取
//!（cookie 从 WebView2 的 cookie 库取，含 httpOnly 的 bsid）。
use rm_core::poller::{Call, IpcMsg, Page};
use std::cell::{Cell, RefCell};
use std::collections::VecDeque;
use std::path::Path;
use std::rc::Rc;
use wry::{dpi, NewWindowResponse, Rect, WebContext, WebView, WebViewBuilder};

pub use rm_core::poller::START_URL;
const POLLER_JS: &str = include_str!("../../../web/poller.js");

/// 与 ui/app.slint 里 Layout.sidebar-w / topbar-h 一致（逻辑像素）
pub const SIDEBAR_W: f64 = 208.0;
pub const TOPBAR_H: f64 = 64.0;

pub type Inbox = Rc<RefCell<VecDeque<IpcMsg>>>;

pub struct Host {
    wv: WebView,
    /// 用户是否正看着后台页（主窗口可见且在「后台页面」标签）
    pub user_visible: Rc<Cell<bool>>,
    pending_urls: Rc<RefCell<VecDeque<String>>>,
    inbox: Inbox,
    /// 直连请求在后台线程跑完，结果经这个 channel 回主线程并入 inbox
    direct_tx: std::sync::mpsc::Sender<IpcMsg>,
    direct_rx: std::sync::mpsc::Receiver<IpcMsg>,
}

impl Host {
    pub fn new(window: &slint::Window, data_dir: &Path, inbox: Inbox) -> anyhow::Result<Self> {
        // WebContext 要活得比 WebView 久：整个进程只建一个
        let ctx: &'static mut WebContext = Box::leak(Box::new(WebContext::new(Some(data_dir.to_path_buf()))));
        let handle = window.window_handle();
        let pending_urls: Rc<RefCell<VecDeque<String>>> = Rc::default();
        let pu = pending_urls.clone();
        let ipc_inbox = inbox.clone();
        let (direct_tx, direct_rx) = std::sync::mpsc::channel();
        let mut builder = WebViewBuilder::new_with_web_context(ctx);
        // 仅调试版：设了 RM_CDP_PORT 就开远程调试端口，方便用 tools/cdp.mjs 看已登录的页面
        #[cfg(debug_assertions)]
        if let Ok(port) = std::env::var("RM_CDP_PORT") {
            use wry::WebViewBuilderExtWindows;
            builder = builder.with_additional_browser_args(format!(
                "--disable-features=msWebOOUI,msPdfOOUI,msSmartScreenProtection --remote-debugging-port={port}"
            ));
        }
        let wv = builder
            .with_url(START_URL)
            .with_initialization_script(POLLER_JS)
            .with_devtools(cfg!(debug_assertions))
            .with_visible(false)
            .with_bounds(Self::bounds_for(window))
            .with_ipc_handler(move |req| {
                let body = req.body();
                match serde_json::from_str::<IpcMsg>(body) {
                    Ok(m) => {
                        tracing::debug!("IPC <- {}", truncate(body, 300));
                        ipc_inbox.borrow_mut().push_back(m);
                    }
                    Err(e) => tracing::warn!("无法解析的 IPC 消息 ({e}): {}", truncate(body, 200)),
                }
            })
            // 后台里 target=_blank 的链接：在同一个 WebView 里打开，不弹新窗口
            .with_new_window_req_handler(move |url, _| {
                pu.borrow_mut().push_back(url);
                NewWindowResponse::Deny
            })
            .build_as_child(&handle)?;
        Ok(Self { wv, user_visible: Rc::new(Cell::new(false)), pending_urls, inbox, direct_tx, direct_rx })
    }

    /// ecom.meituan.com 的完整 cookie 头（含 httpOnly 的 bsid）。直接读 WebView2 的
    /// cookie 库，与当前页面无关；cookie 全空（未登录）时返回 Some("")。
    fn cookie_header(&self) -> Option<String> {
        let cs = self.wv.cookies_for_url("https://ecom.meituan.com/").ok()?;
        Some(cs.iter().map(|c| format!("{}={}", c.name(), c.value())).collect::<Vec<_>>().join("; "))
    }

    fn bounds_for(window: &slint::Window) -> Rect {
        let s = window.size();
        let k = window.scale_factor() as f64;
        let x = (SIDEBAR_W * k).round() as u32;
        let y = (TOPBAR_H * k).round() as u32;
        Rect {
            position: dpi::PhysicalPosition::new(x as i32, y as i32).into(),
            size: dpi::PhysicalSize::new(s.width.saturating_sub(x), s.height.saturating_sub(y)).into(),
        }
    }

    pub fn layout(&self, window: &slint::Window) {
        let _ = self.wv.set_bounds(Self::bounds_for(window));
    }

    pub fn set_shown(&self, on: bool) {
        let _ = self.wv.set_visible(on);
        if on { let _ = self.wv.focus(); }
    }

    /// 处理页面里请求打开的新窗口链接
    pub fn pump(&self) {
        // 后台线程完成的直连请求结果并入 IPC inbox，走同一条 on_msg 通道
        while let Ok(m) = self.direct_rx.try_recv() {
            tracing::debug!("直连回传 {} kind={}", m.req_id.as_deref().unwrap_or_default(), m.kind);
            self.inbox.borrow_mut().push_back(m);
        }
        while let Some(u) = self.pending_urls.borrow_mut().pop_front() {
            let _ = self.wv.load_url(&u);
        }
    }

    pub fn goto(&self, url: &str) {
        let _ = self.wv.load_url(url);
    }

    /// 在评价页里定位一条评价（结果以 kind=reveal 的消息回传）
    pub fn reveal(&self, platform: rm_core::Platform, feedback_id: i64) {
        let _ = self.wv.evaluate_script(&rm_core::poller::reveal_js(platform, feedback_id));
    }
}

impl Page for Host {
    fn call(&self, req_id: &str, call: &Call) {
        tracing::debug!("call {req_id} {call:?}（直连）");
        let Some(cookie) = self.cookie_header() else {
            self.inbox.borrow_mut().push_back(crate::direct::cookie_unavailable(req_id, call));
            return;
        };
        let tx = self.direct_tx.clone();
        // 网络 I/O 不进 UI 线程；结果经 direct_rx → inbox 回给 Poller
        let (req_id, call) = (req_id.to_string(), call.clone());
        std::thread::spawn(move || { let _ = tx.send(crate::direct::perform(&req_id, &call, &cookie)); });
    }

    fn goto_reviews(&self) {
        tracing::info!("让后台页回到评价页");
        self.goto(START_URL);
    }

    fn user_visible(&self) -> bool {
        self.user_visible.get()
    }
}

fn truncate(s: &str, n: usize) -> String {
    let t: String = s.chars().take(n).collect();
    if s.chars().count() > n { format!("{t}…") } else { t }
}
