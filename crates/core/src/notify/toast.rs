//! Windows 系统通知（WinRT Toast）。
//!
//! 点通知会打开 `review-monitor://` 链接（protocol 激活）：程序退出后从操作中心点也有效。
//! 链接的注册、转发在 app 里做（`crates/app/src/activation.rs`）。
use super::{Alert, Notifier};
use crate::activate;
use std::sync::OnceLock;
use windows::Data::Xml::Dom::XmlDocument;
use windows::UI::Notifications::{NotificationSetting, ToastNotification, ToastNotificationManager};
use windows::core::HSTRING;

/// 没注册自己的应用 ID 时借用 PowerShell 的，通知照样能弹，只是标题栏显示「Windows PowerShell」
pub const POWERSHELL_APP_ID: &str = r"{1AC14E77-02E7-4E5D-B744-2EB1AE5198B7}\WindowsPowerShell\v1.0\powershell.exe";

static APP_ID: OnceLock<String> = OnceLock::new();

/// 启动时调用一次：设置通知用的应用 ID（之后新建的 ToastNotifier 都用它）
pub fn set_app_id(id: &str) {
    let _ = APP_ID.set(id.to_string());
}

/// 系统认不认这个应用 ID（注册表里登记过、用户没关掉它的通知）
pub fn app_id_enabled(id: &str) -> bool {
    ToastNotificationManager::CreateToastNotifierWithId(&HSTRING::from(id))
        .and_then(|n| n.Setting())
        .is_ok_and(|s| s == NotificationSetting::Enabled)
}

pub struct ToastNotifier {
    app_id: String,
}

impl Default for ToastNotifier {
    fn default() -> Self {
        Self { app_id: APP_ID.get().cloned().unwrap_or_else(|| POWERSHELL_APP_ID.to_string()) }
    }
}

fn esc(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;").replace('\'', "&apos;")
}

/// 通知内容（ToastGeneric 模板）
pub fn toast_xml(a: &Alert) -> String {
    let launch = activate::for_alert(a).uri();
    format!(
        r#"<toast launch="{}" activationType="protocol" duration="long"><visual><binding template="ToastGeneric"><text>{}</text><text>{}</text></binding></visual></toast>"#,
        esc(&launch), esc(&a.title), esc(&a.body)
    )
}

impl Notifier for ToastNotifier {
    fn id(&self) -> &'static str {
        "toast"
    }

    fn send(&self, a: &Alert) -> anyhow::Result<()> {
        let doc = XmlDocument::new()?;
        doc.LoadXml(&HSTRING::from(toast_xml(a)))?;
        let toast = ToastNotification::CreateToastNotification(&doc)?;
        ToastNotificationManager::CreateToastNotifierWithId(&HSTRING::from(&self.app_id))?.Show(&toast)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::notify::AlertKind;

    #[test]
    fn powershell_id_is_enabled() {
        assert!(POWERSHELL_APP_ID.ends_with(r"\WindowsPowerShell\v1.0\powershell.exe"));
        assert!(app_id_enabled(POWERSHELL_APP_ID));
        assert!(!app_id_enabled("ReviewMonitor.NoSuchApp.Test"));
    }

    #[test]
    fn xml_escapes_and_links() {
        let mut a = Alert::system(AlertKind::NewBadReview, "点评 2.5★ 新差评", "11-10 19:49  菜<咸>了 & \"贵\"");
        a.review_key = Some("dp:3550718756".into());
        let x = toast_xml(&a);
        assert!(x.contains(r#"launch="review-monitor://open?review=dp:3550718756""#), "{x}");
        assert!(x.contains("菜&lt;咸&gt;了 &amp; &quot;贵&quot;"), "{x}");
        // 能被系统的 XML 解析器读进去
        XmlDocument::new().unwrap().LoadXml(&HSTRING::from(x)).unwrap();
    }
}
