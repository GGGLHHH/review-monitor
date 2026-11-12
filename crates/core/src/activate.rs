//! 点通知后的跳转：通知里带一个 `review-monitor://` 链接，系统拉起程序时把它当参数传进来。
//!
//! 这个协议任何网页都能触发，所以解析要严格：只认三种动作，评价 id 必须是数字。
use crate::model::Platform;
use crate::notify::{Alert, AlertKind};

pub const SCHEME: &str = "review-monitor";

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Activation {
    /// 只打开主窗口
    Open,
    /// 打开「后台页面」（比如去重新登录）
    Backend,
    /// 打开后台评价页并定位到这条评价
    Review { platform: Platform, feedback_id: i64 },
}

impl Activation {
    pub fn uri(&self) -> String {
        match self {
            Activation::Open => format!("{SCHEME}://open"),
            Activation::Backend => format!("{SCHEME}://open?page=backend"),
            Activation::Review { platform, feedback_id } => {
                format!("{SCHEME}://open?review={}:{feedback_id}", platform.key_prefix())
            }
        }
    }

    /// 对应的去重键（`dp:123`），只有 Review 有
    pub fn review_key(&self) -> Option<String> {
        match self {
            Activation::Review { platform, feedback_id } => Some(format!("{}:{feedback_id}", platform.key_prefix())),
            _ => None,
        }
    }
}

/// `dp:3550718756` → (点评, 3550718756)
pub fn split_key(key: &str) -> Option<(Platform, i64)> {
    let (p, id) = key.split_once(':')?;
    if id.is_empty() || !id.bytes().all(|b| b.is_ascii_digit()) { return None; }
    Some((Platform::from_prefix(p)?, id.parse().ok()?))
}

/// 一条通知点开后应该去哪
pub fn for_alert(a: &Alert) -> Activation {
    if let Some((platform, feedback_id)) = a.review_key.as_deref().and_then(split_key) {
        return Activation::Review { platform, feedback_id };
    }
    match a.kind {
        AlertKind::LoginLost => Activation::Backend,
        _ => Activation::Open,
    }
}

/// 解析命令行里的链接。不是本程序的链接返回 None；是本程序的但看不懂，按 Open 处理。
pub fn parse(arg: &str) -> Option<Activation> {
    let arg = arg.trim().trim_matches('"');
    let (scheme, rest) = arg.split_once(':')?;
    if !scheme.eq_ignore_ascii_case(SCHEME) { return None; }
    let query = rest.split_once('?').map(|(_, q)| q).unwrap_or("");
    for pair in query.split('&') {
        let (k, v) = pair.split_once('=').unwrap_or((pair, ""));
        let v = v.trim_end_matches('/');
        match k {
            "review" => {
                if let Some((platform, feedback_id)) = split_key(&v.replace("%3A", ":").replace("%3a", ":")) {
                    return Some(Activation::Review { platform, feedback_id });
                }
            }
            "page" if v == "backend" => return Some(Activation::Backend),
            _ => {}
        }
    }
    Some(Activation::Open)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_all_kinds() {
        for a in [
            Activation::Open,
            Activation::Backend,
            Activation::Review { platform: Platform::Dianping, feedback_id: 3550718756 },
            Activation::Review { platform: Platform::Meituan, feedback_id: 10580084627 },
        ] {
            assert_eq!(parse(&a.uri()), Some(a.clone()), "{}", a.uri());
        }
    }

    #[test]
    fn review_uri_format() {
        let a = Activation::Review { platform: Platform::Dianping, feedback_id: 3550718756 };
        assert_eq!(a.uri(), "review-monitor://open?review=dp:3550718756");
        assert_eq!(a.review_key().as_deref(), Some("dp:3550718756"));
    }

    #[test]
    fn tolerates_shell_variants() {
        // 系统可能补斜杠、加引号、把冒号转义、改大小写
        let want = Some(Activation::Review { platform: Platform::Dianping, feedback_id: 42 });
        assert_eq!(parse("\"review-monitor://open/?review=dp:42\""), want);
        assert_eq!(parse("review-monitor://open?review=dp%3A42/"), want);
        assert_eq!(parse("REVIEW-MONITOR://open?review=dp:42"), want);
    }

    #[test]
    fn rejects_foreign_and_bad_input() {
        assert_eq!(parse("--flag"), None);
        assert_eq!(parse("https://example.com/?review=dp:1"), None);
        assert_eq!(parse("C:\\path\\file.txt"), None);
        // 本程序的链接但参数不合法：只打开主窗口
        assert_eq!(parse("review-monitor://open?review=dp:12;rm"), Some(Activation::Open));
        assert_eq!(parse("review-monitor://open?review=xx:12"), Some(Activation::Open));
        assert_eq!(parse("review-monitor://open?review=dp:"), Some(Activation::Open));
        assert_eq!(parse("review-monitor://whatever"), Some(Activation::Open));
    }

    #[test]
    fn split_key_cases() {
        assert_eq!(split_key("mt:10578232696"), Some((Platform::Meituan, 10578232696)));
        assert_eq!(split_key("dp:-1"), None);
        assert_eq!(split_key("dp"), None);
        assert_eq!(split_key("dp:99999999999999999999999"), None);
    }

    #[test]
    fn alert_targets() {
        let mut a = Alert::system(AlertKind::NewBadReview, "t", "b");
        a.review_key = Some("dp:7".into());
        assert_eq!(for_alert(&a), Activation::Review { platform: Platform::Dianping, feedback_id: 7 });
        assert_eq!(for_alert(&Alert::system(AlertKind::LoginLost, "t", "b")), Activation::Backend);
        assert_eq!(for_alert(&Alert::system(AlertKind::ApiError, "t", "b")), Activation::Open);
        assert_eq!(for_alert(&Alert::system(AlertKind::LoginRestored, "t", "b")), Activation::Open);
    }
}
