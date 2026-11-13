// release 版不弹控制台窗口
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod activation;
mod direct;
mod paths;
mod tray;
mod webview;

use chrono::Local;
use rm_core::activate::{self, Activation};
use rm_core::config::Config;
use rm_core::notify::{fmt_star, Alert, AlertKind, Dispatcher};
use rm_core::poller::{Login, Poller};
use rm_core::store::Store;
use rm_core::Platform;
use slint::{ComponentHandle, Model, ModelRc, SharedString, VecModel};
use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::time::{Duration, Instant};
use webview::{Host, Inbox};

slint::include_modules!();

const TAB_BACKEND: i32 = 1;
const LIST_LIMIT: u32 = 300;
/// 定位评价时，页面没准备好最多重试几次
const REVEAL_TRIES: u8 = 3;

/// 正在后台页里定位的一条评价
struct RevealJob {
    platform: Platform,
    feedback_id: i64,
    tries: u8,
    /// 已经发给页面、等回执
    sent: bool,
    /// 页面在重新载入，等它载完再发
    wait_nav: bool,
}

fn now_ms() -> i64 {
    Local::now().timestamp_millis()
}

fn main() {
    if let Err(e) = run() {
        tracing::error!("致命错误: {e:#}");
        message_box(&format!("差评监控启动失败：\n{e:#}"));
    }
}

fn run() -> anyhow::Result<()> {
    let dirs = paths::ensure()?;
    let _log_guard = init_logging(&dirs.logs);
    tracing::info!("启动，数据目录 {}", dirs.root.display());

    // 命令行里可能带着点通知传来的 review-monitor:// 链接
    let initial = std::env::args().skip(1).find_map(|a| activate::parse(&a));
    let Some(_instance) = single_instance() else {
        // 已经有一个在跑：把请求交给它（没带链接就是让它把窗口提到前面）
        activation::forward(&dirs.root, &initial.unwrap_or(Activation::Open).uri());
        return Ok(());
    };
    activation::clear(&dirs.root);
    if activation::register(&dirs.root) && rm_core::notify::toast::app_id_enabled(activation::APP_ID) {
        rm_core::notify::toast::set_app_id(activation::APP_ID);
        tracing::info!("通知使用自己的应用 ID {}", activation::APP_ID);
    } else {
        tracing::warn!("通知改用 PowerShell 的应用 ID");
    }

    let (cfg, cfg_warn) = Config::load_or_default(&dirs.config);
    if let Some(w) = &cfg_warn { tracing::warn!("{w}"); }
    let store = Store::open(&dirs.db)?;
    let disp = Dispatcher::build(&cfg.notify.enabled);
    let poller = Rc::new(RefCell::new(Poller::new(cfg.clone(), store, disp, Instant::now())));

    let ui = AppWindow::new()?;
    settings_to_ui(&ui, &cfg);
    let banner = Rc::new(RefCell::new(cfg_warn.unwrap_or_default()));

    let rows: Rc<VecModel<ReviewRow>> = Rc::new(VecModel::default());
    ui.set_rows(ModelRc::from(rows.clone()));

    ui.show()?;

    let inbox: Inbox = Rc::default();
    let host: Rc<RefCell<Option<Host>>> = Rc::default();
    let window_shown = Rc::new(Cell::new(true));
    let pending: Rc<RefCell<Option<Activation>>> = Rc::new(RefCell::new(initial));
    let reveal: Rc<RefCell<Option<RevealJob>>> = Rc::default();

    // 主窗口真正创建出来以后再嵌 WebView
    {
        let ui_w = ui.as_weak();
        let host = host.clone();
        let inbox = inbox.clone();
        let wv_dir = dirs.webview.clone();
        slint::Timer::single_shot(Duration::from_millis(250), move || {
            let ui = ui_w.unwrap();
            match Host::new(ui.window(), &wv_dir, inbox) {
                Ok(h) => { tracing::info!("WebView 已创建"); *host.borrow_mut() = Some(h); }
                Err(e) => {
                    tracing::error!("WebView 创建失败: {e:#}");
                    ui.set_banner(format!("后台页面创建失败：{e}。请确认系统已安装 Microsoft Edge WebView2 运行时。").into());
                }
            }
        });
    }

    // 后台页可见性：只在「后台页面」标签且窗口可见时显示
    let sync_backend = {
        let ui_w = ui.as_weak();
        let host = host.clone();
        let window_shown = window_shown.clone();
        move || {
            let Some(ui) = ui_w.upgrade() else { return };
            if let Some(h) = host.borrow().as_ref() {
                let on = ui.get_tab() == TAB_BACKEND && window_shown.get();
                h.user_visible.set(on);
                h.set_shown(on);
            }
        }
    };

    let show_window = {
        let ui_w = ui.as_weak();
        let window_shown = window_shown.clone();
        let sync = sync_backend.clone();
        // front：用户主动操作（托盘、通知）时提到最前；程序自己要人注意时不抢前台
        move |front: bool| {
            let Some(ui) = ui_w.upgrade() else { return };
            let _ = ui.show();
            ui.window().set_minimized(false);
            if front { activation::bring_to_front(ui.window()); }
            window_shown.set(true);
            sync();
        }
    };

    // 关窗 = 缩到托盘（第一次提示一下）
    {
        let window_shown = window_shown.clone();
        let sync = sync_backend.clone();
        let told = Cell::new(false);
        ui.window().on_close_requested(move || {
            window_shown.set(false);
            sync();
            if !told.replace(true) {
                Dispatcher::build(&["toast".into()]).system(&Alert::system(AlertKind::LoginRestored,
                    "差评监控仍在后台运行", "关闭窗口不会停止监控。要退出请右键托盘图标 → 退出。"));
            }
            slint::CloseRequestResponse::HideWindow
        });
    }

    // ---- 界面回调 ----
    {
        let p = poller.clone();
        ui.on_check_now(move || p.borrow_mut().check_now(Instant::now()));
    }
    {
        let sync = sync_backend.clone();
        let ui_w = ui.as_weak();
        ui.on_tab_changed(move |_| {
            if let Some(ui) = ui_w.upgrade() { ui.set_backend_hint("".into()); }
            sync();
        });
    }
    {
        let p = poller.clone();
        ui.on_filter_changed(move |_| p.borrow_mut().list_dirty = true);
    }
    {
        let p = poller.clone();
        ui.on_mark_all_read(move || {
            let mut p = p.borrow_mut();
            let _ = p.store.mark_all_read();
            p.list_dirty = true;
        });
    }
    {
        // 卡片上的「标为已读 / 未读」：只改状态，不跳转
        let p = poller.clone();
        ui.on_toggle_read(move |key, read| {
            let mut pp = p.borrow_mut();
            if let Err(e) = pp.store.set_read(&key, read) { tracing::warn!("改已读状态失败 {key}: {e:#}"); }
            pp.list_dirty = true;
        });
    }
    {
        // 点卡片和点通知走同一条路：切到后台页并定位这条评价
        let pending = pending.clone();
        ui.on_open_review(move |key| {
            let a = activate::split_key(&key)
                .map(|(platform, feedback_id)| Activation::Review { platform, feedback_id })
                .unwrap_or(Activation::Backend);
            *pending.borrow_mut() = Some(a);
        });
    }
    {
        let host = host.clone();
        ui.on_reload_backend(move || { if let Some(h) = host.borrow().as_ref() { h.goto(webview::START_URL); } });
    }
    {
        let p = poller.clone();
        let ui_w = ui.as_weak();
        let path = dirs.config.clone();
        let banner = banner.clone();
        ui.on_save_settings(move || {
            let Some(ui) = ui_w.upgrade() else { return };
            let cfg = settings_from_ui(&ui, p.borrow().config());
            match cfg.save(&path) {
                Ok(()) => {
                    let disp = Dispatcher::build(&cfg.notify.enabled);
                    p.borrow_mut().set_config(cfg, disp, Instant::now());
                    banner.borrow_mut().clear();
                    ui.set_settings_ok(true);
                    ui.set_settings_msg(format!("已保存 {}", Local::now().format("%H:%M:%S")).into());
                    tracing::info!("设置已保存");
                }
                Err(e) => {
                    ui.set_settings_ok(false);
                    ui.set_settings_msg(format!("没有保存：{e}").into());
                }
            }
        });
    }
    {
        let ui_w = ui.as_weak();
        ui.on_reset_settings(move || {
            let Some(ui) = ui_w.upgrade() else { return };
            settings_to_ui(&ui, &Config::default());
            ui.set_settings_ok(true);
            ui.set_settings_msg("已填入默认值，点「保存」后生效".into());
        });
    }

    let tray = match tray::Tray::new() {
        Ok(t) => Some(t),
        Err(e) => { tracing::error!("托盘图标创建失败: {e:#}"); None }
    };

    // ---- 主循环：每 250ms 驱动一次 ----
    let main_timer = slint::Timer::default();
    {
        let ui_w = ui.as_weak();
        let p = poller.clone();
        let host = host.clone();
        let inbox = inbox.clone();
        let rows = rows.clone();
        let banner = banner.clone();
        let show = show_window.clone();
        let sync = sync_backend.clone();
        let pending = pending.clone();
        let reveal = reveal.clone();
        let root = dirs.root.clone();
        let last_size = Cell::new((0u32, 0u32));
        let last_tip = RefCell::new(String::new());
        main_timer.start(slint::TimerMode::Repeated, Duration::from_millis(250), move || {
            let Some(ui) = ui_w.upgrade() else { return };
            for a in tray::Tray::poll() {
                match a {
                    tray::TrayAction::Open => show(true),
                    tray::TrayAction::CheckNow => p.borrow_mut().check_now(Instant::now()),
                    tray::TrayAction::Quit => { tracing::info!("从托盘退出"); let _ = slint::quit_event_loop(); return; }
                }
            }
            // 点通知 / 卡片 / 再次双击 exe 传来的跳转请求
            if let Some(u) = activation::take(&root) {
                tracing::info!("收到跳转请求: {u}");
                if let Some(a) = activate::parse(&u) { *pending.borrow_mut() = Some(a); }
            }
            let act = pending.borrow_mut().take();
            if let Some(a) = act {
                match a {
                    Activation::Open => show(true),
                    Activation::Backend => { ui.set_tab(TAB_BACKEND); show(true); }
                    Activation::Review { platform, feedback_id } => {
                        if let Some(k) = a.review_key() {
                            let mut pp = p.borrow_mut();
                            let _ = pp.store.mark_read(&k);
                            pp.list_dirty = true;
                        }
                        ui.set_tab(TAB_BACKEND);
                        ui.set_backend_hint("正在定位这条评价…".into());
                        show(true);
                        *reveal.borrow_mut() = Some(RevealJob { platform, feedback_id, tries: 0, sent: false, wait_nav: false });
                    }
                }
            }

            let hb = host.borrow();
            let Some(h) = hb.as_ref() else { return };
            if let Some(j) = reveal.borrow_mut().as_mut()
                && !j.sent
                && !j.wait_nav
            {
                h.reveal(j.platform, j.feedback_id);
                j.sent = true;
            }
            // 跟随窗口尺寸
            let s = ui.window().size();
            if last_size.get() != (s.width, s.height) { last_size.set((s.width, s.height)); h.layout(ui.window()); }
            h.pump();

            let mut pp = p.borrow_mut();
            let msgs: Vec<_> = inbox.borrow_mut().drain(..).collect();
            let now = Instant::now();
            for m in &msgs {
                if m.kind == "reveal" { on_reveal_result(&ui, h, &mut reveal.borrow_mut(), m); continue; }
                if m.kind == "nav"
                    && m.login != Some(true)
                    && m.href.as_deref().is_some_and(|x| x.contains("evaluation/poi"))
                    && let Some(j) = reveal.borrow_mut().as_mut()
                {
                    j.wait_nav = false; // 评价页载完了，下一拍重发
                }
                pp.on_msg(m, h, now, now_ms());
            }
            pp.tick(h, now, now_ms());

            if pp.attention {
                pp.attention = false;
                drop(pp);
                drop(hb);
                ui.set_tab(TAB_BACKEND);
                show(false);
                sync();
                pp = p.borrow_mut();
            }
            update_status(&ui, &pp, &banner.borrow());
            if pp.list_dirty {
                pp.list_dirty = false;
                refresh_rows(&ui, &pp, &rows);
            }
            if let Some(t) = &tray {
                let tip = format!("差评监控 · {}", pp.status.message);
                if *last_tip.borrow() != tip { t.set_tooltip(&tip); *last_tip.borrow_mut() = tip; }
            }
        });
    }

    slint::run_event_loop_until_quit()?;
    tracing::info!("退出");
    Ok(())
}

/// 页面回传的定位结果
fn on_reveal_result(ui: &AppWindow, h: &Host, job: &mut Option<RevealJob>, m: &rm_core::poller::IpcMsg) {
    let Some(j) = job.as_mut() else { return };
    let err = m.error.as_deref();
    tracing::info!("定位评价 {}:{} 结果: {}", j.platform.key_prefix(), j.feedback_id, err.unwrap_or("ok"));
    match err {
        None => { ui.set_backend_hint("".into()); *job = None; }
        // 页面还没加载好 / 不在评价页：载入评价页，载完再试
        Some("no_rm" | "no_iframe") if j.tries + 1 < REVEAL_TRIES => {
            j.tries += 1;
            j.sent = false;
            j.wait_nav = true;
            let on_reviews = m.href.as_deref().is_some_and(|x| x.contains("evaluation/poi"));
            if !(err == Some("no_rm") && on_reviews) { h.goto(webview::START_URL); }
        }
        Some("not_found") => {
            ui.set_backend_hint("当前列表里没找到这条评价（可能被筛选条件挡住，或不在第一页），请在下面手动翻找".into());
            *job = None;
        }
        Some(_) => {
            ui.set_backend_hint("没能自动定位到这条评价，请在下面手动查找".into());
            *job = None;
        }
    }
}

fn update_status(ui: &AppWindow, p: &Poller, banner: &str) {
    let st = &p.status;
    ui.set_logged_in(match st.login { Login::Unknown => 0, Login::In => 1, Login::Out => 2 });
    ui.set_busy(st.busy);
    ui.set_status_text(st.message.clone().into());
    let next = match (st.busy, st.next_at, st.login) {
        (true, _, _) => String::new(),
        (_, _, Login::Out) => "监控已暂停，登录后自动恢复".into(),
        (_, Some(t), _) => {
            let left = t.saturating_duration_since(Instant::now()).as_secs();
            if left >= 60 { format!("{} 分 {} 秒后再次检查", left / 60, left % 60) } else { format!("{left} 秒后再次检查") }
        }
        _ => String::new(),
    };
    let last = st.last_ok.map(|t| format!("上次成功 {}", t.format("%m-%d %H:%M"))).unwrap_or_default();
    let sep = if last.is_empty() || next.is_empty() { "" } else { " · " };
    ui.set_next_text(format!("{last}{sep}{next}").into());
    let mut b = banner.to_string();
    if let Ok(n) = p.store.gave_up_count()
        && n > 0
    {
        if !b.is_empty() { b.push_str("  "); }
        b.push_str(&format!("有 {n} 条通知多次发送失败已放弃，详见日志。"));
    }
    ui.set_banner(b.into());
}

fn refresh_rows(ui: &AppWindow, p: &Poller, rows: &VecModel<ReviewRow>) {
    let only_bad = ui.get_only_bad();
    let list = p.store.recent(LIST_LIMIT, only_bad).unwrap_or_default();
    let v: Vec<ReviewRow> = list.iter().map(|s| {
        let r = &s.review;
        ReviewRow {
            key: r.key.clone().into(),
            platform: r.platform.label().into(),
            is_mt: r.platform == Platform::Meituan,
            star: format!("{}★", fmt_star(r.star)).into(),
            star_value: r.star,
            time: r.add_time.format("%m-%d %H:%M").to_string().into(),
            content: r.content.clone().into(),
            source: r.source.clone().into(),
            replied: r.replied,
            bad: s.is_bad,
            read: s.read,
        }
    }).collect();
    rows.set_vec(v);
    let unread = p.store.unread_bad_count().unwrap_or(0);
    ui.set_unread(unread as i32);
    let bad_total = list.iter().filter(|s| s.is_bad).count();
    ui.set_list_summary(SharedString::from(if only_bad {
        format!("共 {bad_total} 条差评，未读 {unread} 条")
    } else {
        format!("最近 {} 条评价（其中差评 {bad_total} 条）", rows.row_count())
    }));
}

fn settings_to_ui(ui: &AppWindow, c: &Config) {
    ui.set_s_interval(c.poll.interval_minutes as f32);
    ui.set_s_jitter(c.poll.jitter_percent as f32);
    ui.set_s_overlap(c.poll.overlap_minutes as f32);
    ui.set_s_lookback(c.poll.first_run_lookback_hours as f32);
    ui.set_s_max_pages(c.poll.max_pages as f32);
    ui.set_s_mt_star(c.rules.meituan_max_star);
    ui.set_s_dp_star(c.rules.dianping_max_star);
    ui.set_s_mt_on(c.rules.platforms.contains(&Platform::Meituan));
    ui.set_s_dp_on(c.rules.platforms.contains(&Platform::Dianping));
    ui.set_s_only_unreplied(c.rules.only_unreplied);
    ui.set_s_alert_existing(c.rules.alert_existing_on_first_run);
    ui.set_s_toast_on(c.notify.enabled.iter().any(|n| n == "toast"));
}

fn settings_from_ui(ui: &AppWindow, base: &Config) -> Config {
    let mut c = base.clone();
    c.poll.interval_minutes = ui.get_s_interval().round() as u32;
    c.poll.jitter_percent = ui.get_s_jitter().round() as u32;
    c.poll.overlap_minutes = ui.get_s_overlap().round() as u32;
    c.poll.first_run_lookback_hours = ui.get_s_lookback().round() as u32;
    c.poll.max_pages = ui.get_s_max_pages().round() as u32;
    c.rules.meituan_max_star = (ui.get_s_mt_star() * 2.0).round() / 2.0;
    c.rules.dianping_max_star = (ui.get_s_dp_star() * 2.0).round() / 2.0;
    c.rules.platforms = Platform::ALL.into_iter()
        .filter(|p| match p { Platform::Meituan => ui.get_s_mt_on(), Platform::Dianping => ui.get_s_dp_on() })
        .collect();
    c.rules.only_unreplied = ui.get_s_only_unreplied();
    c.rules.alert_existing_on_first_run = ui.get_s_alert_existing();
    c.notify.enabled.retain(|n| n != "toast");
    if ui.get_s_toast_on() { c.notify.enabled.insert(0, "toast".into()); }
    c
}

fn init_logging(dir: &std::path::Path) -> Option<tracing_appender::non_blocking::WorkerGuard> {
    use tracing_subscriber::{fmt, prelude::*, EnvFilter};
    let appender = tracing_appender::rolling::Builder::new()
        .rotation(tracing_appender::rolling::Rotation::DAILY)
        .filename_prefix("review-monitor")
        .filename_suffix("log")
        .max_log_files(14)
        .build(dir)
        .ok()?;
    let (nb, guard) = tracing_appender::non_blocking(appender);
    let filter = EnvFilter::try_from_env("RM_LOG").unwrap_or_else(|_| EnvFilter::new("info,review_monitor=debug,rm_core=debug"));
    tracing_subscriber::registry()
        .with(filter)
        .with(fmt::layer().with_writer(nb).with_ansi(false))
        .with(fmt::layer().with_writer(std::io::stderr))
        .init();
    Some(guard)
}

/// 全局命名互斥量：同一时间只允许一个实例（避免两份程序同时用一个登录态轮询）
fn single_instance() -> Option<isize> {
    use windows_sys::Win32::Foundation::{GetLastError, ERROR_ALREADY_EXISTS};
    use windows_sys::Win32::System::Threading::CreateMutexW;
    let name: Vec<u16> = "Local\\review-monitor-single-instance\0".encode_utf16().collect();
    let h = unsafe { CreateMutexW(std::ptr::null(), 1, name.as_ptr()) };
    if h.is_null() { return Some(0); }
    if unsafe { GetLastError() } == ERROR_ALREADY_EXISTS { return None; }
    Some(h as isize)
}

fn message_box(text: &str) {
    use windows_sys::Win32::UI::WindowsAndMessaging::{MessageBoxW, MB_ICONINFORMATION, MB_OK};
    let t: Vec<u16> = text.encode_utf16().chain([0]).collect();
    let c: Vec<u16> = "差评监控".encode_utf16().chain([0]).collect();
    unsafe { MessageBoxW(std::ptr::null_mut(), t.as_ptr(), c.as_ptr(), MB_OK | MB_ICONINFORMATION) };
}
