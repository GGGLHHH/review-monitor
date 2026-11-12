//! 点通知跳转的系统集成。
//!
//! - 在 HKCU 下注册 `review-monitor://` 协议（指向当前 exe）和自己的通知应用 ID（显示名「差评监控」+ 图标）。
//!   每次启动都重写一遍，exe 挪了位置也能跟上；只写当前用户，不需要管理员权限。
//! - 点通知时系统会再启动一个 exe 并把链接当参数传进来。如果已经有实例在跑，新进程把链接写进
//!   「信箱」文件后立刻退出，由正在运行的实例在主循环里取走处理。
use std::path::{Path, PathBuf};
use windows_sys::Win32::Foundation::{ERROR_SUCCESS, HWND};
use windows_sys::Win32::System::Registry::{RegSetKeyValueW, HKEY_CURRENT_USER, REG_SZ};

pub const APP_ID: &str = "ReviewMonitor.App";
const ICON_PNG: &[u8] = include_bytes!("../assets/icon.png");

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain([0]).collect()
}

/// 写 HKCU\<subkey> 下的字符串值（name=None 是默认值），键不存在会自动创建
fn set_str(subkey: &str, name: Option<&str>, value: &str) -> anyhow::Result<()> {
    let k = wide(subkey);
    let n = name.map(wide);
    let v = wide(value);
    let rc = unsafe {
        RegSetKeyValueW(
            HKEY_CURRENT_USER,
            k.as_ptr(),
            n.as_ref().map_or(std::ptr::null(), |n| n.as_ptr()),
            REG_SZ,
            v.as_ptr().cast(),
            (v.len() * 2) as u32,
        )
    };
    anyhow::ensure!(rc == ERROR_SUCCESS, "写注册表 {subkey} 失败（{rc}）");
    Ok(())
}

/// 注册协议和通知应用 ID。返回 true 表示自己的应用 ID 可用。
pub fn register(data_root: &Path) -> bool {
    let exe = match std::env::current_exe() {
        Ok(p) => p,
        Err(e) => { tracing::warn!("拿不到 exe 路径，点通知跳转不可用: {e}"); return false; }
    };
    let exe = exe.display().to_string();
    let scheme = rm_core::activate::SCHEME;
    let base = format!(r"Software\Classes\{scheme}");
    let proto = (|| -> anyhow::Result<()> {
        set_str(&base, None, "URL:差评监控")?;
        set_str(&base, Some("URL Protocol"), "")?;
        set_str(&format!(r"{base}\DefaultIcon"), None, &format!("\"{exe}\",0"))?;
        set_str(&format!(r"{base}\shell\open\command"), None, &format!("\"{exe}\" \"%1\""))?;
        Ok(())
    })();
    if let Err(e) = &proto { tracing::warn!("注册 {scheme}:// 协议失败，点通知不会跳转: {e:#}"); }

    let icon = data_root.join("icon.png");
    let aumid = (|| -> anyhow::Result<()> {
        if std::fs::read(&icon).ok().as_deref() != Some(ICON_PNG) { std::fs::write(&icon, ICON_PNG)?; }
        let key = format!(r"Software\Classes\AppUserModelId\{APP_ID}");
        set_str(&key, Some("DisplayName"), "差评监控")?;
        set_str(&key, Some("IconUri"), &icon.display().to_string())?;
        Ok(())
    })();
    match aumid {
        Ok(()) => true,
        Err(e) => { tracing::warn!("注册通知应用 ID 失败，改用 PowerShell 的: {e:#}"); false }
    }
}

fn mailbox(data_root: &Path) -> PathBuf {
    data_root.join("activate.txt")
}

/// 第二个实例：把链接交给正在运行的实例。先放行它抢前台，否则窗口只会在任务栏闪。
pub fn forward(data_root: &Path, uri: &str) {
    use windows_sys::Win32::UI::WindowsAndMessaging::{AllowSetForegroundWindow, ASFW_ANY};
    unsafe { AllowSetForegroundWindow(ASFW_ANY) };
    let tmp = data_root.join("activate.txt.tmp");
    let r = std::fs::write(&tmp, uri).and_then(|_| std::fs::rename(&tmp, mailbox(data_root)));
    match r {
        Ok(()) => tracing::info!("已转交给正在运行的实例: {uri}"),
        Err(e) => tracing::warn!("转交失败: {e}"),
    }
}

/// 正在运行的实例：取走信箱里的链接
pub fn take(data_root: &Path) -> Option<String> {
    let p = mailbox(data_root);
    let s = std::fs::read_to_string(&p).ok()?;
    let _ = std::fs::remove_file(&p);
    Some(s)
}

/// 启动时清掉上次残留的请求
pub fn clear(data_root: &Path) {
    let _ = std::fs::remove_file(mailbox(data_root));
}

/// 把主窗口提到最前（需要当前进程有前台权限：用户点了托盘 / 通知时才有）
pub fn bring_to_front(w: &slint::Window) {
    use windows_sys::Win32::UI::WindowsAndMessaging::SetForegroundWindow;
    use wry::raw_window_handle::{HasWindowHandle, RawWindowHandle};
    if let Ok(h) = w.window_handle().window_handle()
        && let RawWindowHandle::Win32(h) = h.as_raw()
    {
        unsafe { SetForegroundWindow(h.hwnd.get() as HWND) };
    }
}
