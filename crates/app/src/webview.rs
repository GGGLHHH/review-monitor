//! 后台页面：wry(WebView2) 作为子窗口嵌在 Slint 主窗口的内容区，注入 web/poller.js。
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
}

impl Host {
    pub fn new(window: &slint::Window, data_dir: &Path, inbox: Inbox) -> anyhow::Result<Self> {
        // WebContext 要活得比 WebView 久：整个进程只建一个
        let ctx: &'static mut WebContext = Box::leak(Box::new(WebContext::new(Some(data_dir.to_path_buf()))));
        let handle = window.window_handle();
        let pending_urls: Rc<RefCell<VecDeque<String>>> = Rc::default();
        let pu = pending_urls.clone();
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
                        inbox.borrow_mut().push_back(m);
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
        Ok(Self { wv, user_visible: Rc::new(Cell::new(false)), pending_urls })
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
        let js = call.to_js(req_id);
        tracing::debug!("call {req_id} {call:?}");
        if let Err(e) = self.wv.evaluate_script(&js) {
            tracing::warn!("evaluate_script 失败: {e}");
        }
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
