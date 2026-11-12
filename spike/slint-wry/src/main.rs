// Task 0 原型：Slint 窗口内嵌 wry(WebView2)，验证 登录 → 页面内取数 → IPC 回传。用完即弃。
// 环境变量：
//   SPIKE_POLL_SECS=60   自动取数间隔（0=只手动）
//   SPIKE_HIDE_AFTER=30  启动 N 秒后隐藏窗口（测试隐藏后取数）
//   SPIKE_SHOW_AFTER=1800 隐藏 N 秒后重新显示
use std::{cell::RefCell, rc::Rc, time::Duration};
use wry::{dpi, Rect, WebContext, WebView, WebViewBuilder};

slint::slint! {
    import { Button } from "std-widgets.slint";
    export component Spike inherits Window {
        title: "spike: slint + wry";
        preferred-width: 1280px;
        preferred-height: 860px;
        in-out property <string> status: "启动中";
        callback fetch();
        callback devtools();
        HorizontalLayout {
            height: 44px; y: 0; padding: 6px; spacing: 8px; alignment: start;
            Button { text: "立即取数"; clicked => { root.fetch(); } }
            Button { text: "DevTools"; clicked => { root.devtools(); } }
            Text { text: root.status; vertical-alignment: center; }
        }
    }
}

const TOOLBAR_LOGICAL: f64 = 44.0;
const START_URL: &str = "https://ecom.meituan.com/meishi/?cate=6103#https://ecom.meituan.com/emis/evaluation/poi";

fn now_ms() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_millis() as u64
}

fn log(s: &str) {
    let t = chrono_like_now();
    println!("[{t}] {s}");
}
fn chrono_like_now() -> String {
    let secs = now_ms() / 1000;
    format!("{secs}")
}

fn fetch_js(seq: u64) -> String {
    let end = now_ms();
    let start = end - 2 * 86_400_000;
    let body = |_: &str| {
        format!(r#"{{"pageNo":1,"pageSize":20,"poiId":"10000001","startTime":{start},"endTime":{end},"replyType":"全部","starType":"全部","sourceType":"全部","userType":"全部","timeType":"近7天"}}"#)
    };
    format!(
        "window.__rm ? (window.__rm.whoami('w{seq}'), window.__rm.fetchList('m{seq}','meituan',{}), setTimeout(()=>window.__rm.fetchList('d{seq}','dianping',{}),1500)) : window.ipc.postMessage(JSON.stringify({{kind:'no_rm'}}))",
        body("mt"), body("dp")
    )
}

fn main() {
    let ui = Spike::new().unwrap();
    let data_dir = std::env::var("APPDATA").map(|a| std::path::PathBuf::from(a).join("review-monitor").join("webview")).unwrap();
    std::fs::create_dir_all(&data_dir).unwrap();
    log(&format!("webview data dir: {}", data_dir.display()));

    ui.show().unwrap();
    let webview: Rc<RefCell<Option<WebView>>> = Rc::new(RefCell::new(None));
    // WebContext 必须活得比 WebView 久
    let ctx: &'static mut WebContext = Box::leak(Box::new(WebContext::new(Some(data_dir))));

    {
        let ui_weak = ui.as_weak();
        let wv = webview.clone();
        // 等窗口真正创建出来再嵌 webview
        slint::Timer::single_shot(Duration::from_millis(300), move || {
            let ui = ui_weak.unwrap();
            let handle = ui.window().window_handle();
            let scale = ui.window().scale_factor() as f64;
            let size = ui.window().size();
            let ui_weak2 = ui.as_weak();
            let built = WebViewBuilder::new_with_web_context(ctx)
                .with_url(START_URL)
                .with_initialization_script(include_str!("../../../web/poller.js"))
                .with_devtools(true)
                .with_bounds(Rect {
                    position: dpi::LogicalPosition::new(0.0, TOOLBAR_LOGICAL).into(),
                    size: dpi::PhysicalSize::new(size.width, size.height.saturating_sub((TOOLBAR_LOGICAL * scale) as u32)).into(),
                })
                .with_ipc_handler(move |req| {
                    let msg = req.body();
                    let short: String = msg.chars().take(600).collect();
                    log(&format!("IPC <- {short}"));
                    // 粗略解析，显示在状态栏
                    let summary = summarize(msg);
                    if let Some(ui) = ui_weak2.upgrade() { ui.set_status(summary.into()); }
                })
                .build_as_child(&handle);
            match built {
                Ok(v) => { log("webview built as child OK"); *wv.borrow_mut() = Some(v); ui.set_status("webview 已创建".into()); }
                Err(e) => { log(&format!("webview build FAILED: {e}")); ui.set_status(format!("失败: {e}").into()); }
            }
        });
    }

    // 跟随窗口缩放
    let resize_timer = slint::Timer::default();
    {
        let ui_weak = ui.as_weak();
        let wv = webview.clone();
        let last = RefCell::new((0u32, 0u32));
        resize_timer.start(slint::TimerMode::Repeated, Duration::from_millis(200), move || {
            let Some(ui) = ui_weak.upgrade() else { return };
            let s = ui.window().size();
            if *last.borrow() == (s.width, s.height) { return; }
            *last.borrow_mut() = (s.width, s.height);
            let scale = ui.window().scale_factor() as f64;
            if let Some(v) = wv.borrow().as_ref() {
                let _ = v.set_bounds(Rect {
                    position: dpi::LogicalPosition::new(0.0, TOOLBAR_LOGICAL).into(),
                    size: dpi::PhysicalSize::new(s.width, s.height.saturating_sub((TOOLBAR_LOGICAL * scale) as u32)).into(),
                });
            }
        });
    }

    let seq = Rc::new(RefCell::new(0u64));
    let do_fetch = {
        let wv = webview.clone();
        let seq = seq.clone();
        move || {
            *seq.borrow_mut() += 1;
            let n = *seq.borrow();
            if let Some(v) = wv.borrow().as_ref() {
                log(&format!("fetch #{n}"));
                if let Err(e) = v.evaluate_script(&fetch_js(n)) { log(&format!("eval err {e}")); }
            }
        }
    };
    { let f = do_fetch.clone(); ui.on_fetch(move || f()); }
    { let wv = webview.clone(); ui.on_devtools(move || { if let Some(v) = wv.borrow().as_ref() { v.open_devtools(); } }); }

    let poll_timer = slint::Timer::default();
    let poll_secs: u64 = std::env::var("SPIKE_POLL_SECS").ok().and_then(|s| s.parse().ok()).unwrap_or(0);
    if poll_secs > 0 {
        let f = do_fetch.clone();
        poll_timer.start(slint::TimerMode::Repeated, Duration::from_secs(poll_secs), move || f());
    }

    let hide_timer = slint::Timer::default();
    let show_timer = Rc::new(slint::Timer::default());
    if let Some(h) = std::env::var("SPIKE_HIDE_AFTER").ok().and_then(|s| s.parse::<u64>().ok()) {
        let ui_weak = ui.as_weak();
        let show_after: u64 = std::env::var("SPIKE_SHOW_AFTER").ok().and_then(|s| s.parse().ok()).unwrap_or(1800);
        let st = show_timer.clone();
        hide_timer.start(slint::TimerMode::SingleShot, Duration::from_secs(h), move || {
            let ui = ui_weak.unwrap();
            log("hiding window");
            ui.hide().unwrap();
            let ui_weak = ui.as_weak();
            st.start(slint::TimerMode::SingleShot, Duration::from_secs(show_after), move || {
                log("showing window again");
                ui_weak.unwrap().show().unwrap();
            });
        });
    }

    slint::run_event_loop_until_quit().unwrap();
}

fn summarize(msg: &str) -> String {
    let kind = extract(msg, "\"kind\":\"").unwrap_or_default();
    let req = extract(msg, "\"reqId\":\"").unwrap_or_default();
    let status = msg.split("\"status\":").nth(1).map(|s| s.chars().take_while(|c| c.is_ascii_digit()).collect::<String>()).unwrap_or_default();
    let total = msg.split("\\\"total\\\":").nth(1).map(|s| s.chars().take_while(|c| c.is_ascii_digit()).collect::<String>()).unwrap_or_default();
    format!("{kind} {req} status={status} total={total}")
}
fn extract(s: &str, key: &str) -> Option<String> {
    s.split(key).nth(1).map(|r| r.chars().take_while(|c| *c != '"').collect())
}
