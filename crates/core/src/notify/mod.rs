//! 推送抽象层。新增渠道 = 新写一个 `impl Notifier`，在 config::KNOWN_NOTIFIERS 和 `build` 里登记。
use crate::model::{Platform, Review};
use crate::store::Store;
use chrono::{DateTime, Local};

#[cfg(windows)]
pub mod toast;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AlertKind {
    NewBadReview,
    LoginLost,
    LoginRestored,
    ApiError,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Alert {
    pub kind: AlertKind,
    pub platform: Option<Platform>,
    pub star: Option<f32>,
    pub time: Option<DateTime<Local>>,
    pub title: String,
    pub body: String,
    pub review_key: Option<String>,
}

impl Alert {
    pub fn system(kind: AlertKind, title: impl Into<String>, body: impl Into<String>) -> Self {
        Self { kind, platform: None, star: None, time: None, title: title.into(), body: body.into(), review_key: None }
    }
}

pub trait Notifier: Send + Sync {
    fn id(&self) -> &'static str;
    fn send(&self, alert: &Alert) -> anyhow::Result<()>;
}

const BODY_CHARS: usize = 80;

/// 星级显示：整数不带小数（1★），半星保留一位（2.5★）
pub fn fmt_star(s: f32) -> String {
    if (s - s.round()).abs() < 1e-3 { format!("{}", s.round() as i32) } else { format!("{s:.1}") }
}

pub fn alert_for(r: &Review) -> Alert {
    let text: String = r.content.chars().take(BODY_CHARS).collect();
    let text = if text.is_empty() { "（无文字）".to_string() }
               else if r.content.chars().count() > BODY_CHARS { format!("{text}…") } else { text };
    Alert {
        kind: AlertKind::NewBadReview,
        platform: Some(r.platform),
        star: Some(r.star),
        time: Some(r.add_time),
        title: format!("{} {}★ 新差评", r.platform.label(), fmt_star(r.star)),
        body: format!("{}  {}", r.add_time.format("%m-%d %H:%M"), text),
        review_key: Some(r.key.clone()),
    }
}

pub struct Dispatcher {
    notifiers: Vec<Box<dyn Notifier>>,
}

#[derive(Debug, Default, PartialEq)]
pub struct FlushReport {
    pub sent: u32,
    pub failed: u32,
}

impl Dispatcher {
    pub fn new(notifiers: Vec<Box<dyn Notifier>>) -> Self {
        Self { notifiers }
    }

    /// 按配置里启用的 id 构建渠道
    pub fn build(enabled: &[String]) -> Self {
        let mut v: Vec<Box<dyn Notifier>> = Vec::new();
        for id in enabled {
            match id.as_str() {
                #[cfg(windows)]
                "toast" => v.push(Box::new(toast::ToastNotifier::default())),
                other => tracing::warn!("未知推送渠道 {other}，已忽略"),
            }
        }
        Self::new(v)
    }

    pub fn ids(&self) -> Vec<&'static str> {
        self.notifiers.iter().map(|n| n.id()).collect()
    }

    /// 发出所有到期的发件项；某个渠道失败不影响其他渠道
    pub fn flush(&self, store: &mut Store, now_ms: i64) -> anyhow::Result<FlushReport> {
        let mut rep = FlushReport::default();
        for item in store.due_outbox(now_ms)? {
            let Some(n) = self.notifiers.iter().find(|n| n.id() == item.notifier) else {
                continue; // 渠道已被关闭：保留在发件箱，重新启用后继续发
            };
            let Some(review) = store.get_review(&item.review_key)? else {
                store.mark_failed(item.id, now_ms, "评价不存在")?;
                continue;
            };
            match n.send(&alert_for(&review)) {
                Ok(()) => { store.mark_sent(item.id)?; rep.sent += 1; }
                Err(e) => {
                    tracing::warn!("推送失败 {} {}: {e:#}", item.notifier, item.review_key);
                    store.mark_failed(item.id, now_ms, &format!("{e:#}"))?;
                    rep.failed += 1;
                }
            }
        }
        Ok(rep)
    }

    /// 系统消息（掉线、恢复、接口异常）：直接发给所有渠道，不进发件箱
    pub fn system(&self, a: &Alert) {
        for n in &self.notifiers {
            if let Err(e) = n.send(a) { tracing::warn!("系统消息推送失败 {}: {e:#}", n.id()); }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rules::Rules;
    use chrono::TimeZone;
    use std::sync::{Arc, Mutex};

    fn rv(content: &str, star: f32) -> Review {
        Review { key: "dp:1".into(), platform: Platform::Dianping, feedback_id: 1,
                 add_time: Local.with_ymd_and_hms(2026, 11, 10, 19, 49, 0).unwrap(), star,
                 content: content.into(), replied: false, source: String::new(), poi_name: String::new(), user_name: String::new() }
    }

    #[test]
    fn alert_title_format() {
        let a = alert_for(&rv("好难吃", 2.5));
        assert_eq!(a.title, "点评 2.5★ 新差评");
        assert_eq!(a.body, "11-10 19:49  好难吃");
        assert_eq!(alert_for(&rv("x", 1.0)).title, "点评 1★ 新差评");
        let long = "字".repeat(100);
        assert!(alert_for(&rv(&long, 1.0)).body.ends_with('…'));
    }

    #[test]
    fn alert_empty_body() {
        assert!(alert_for(&rv("", 1.0)).body.ends_with("（无文字）"));
    }

    struct Fake { id: &'static str, ok: bool, got: Arc<Mutex<Vec<String>>> }
    impl Notifier for Fake {
        fn id(&self) -> &'static str { self.id }
        fn send(&self, a: &Alert) -> anyhow::Result<()> {
            self.got.lock().unwrap().push(a.title.clone());
            if self.ok { Ok(()) } else { anyhow::bail!("down") }
        }
    }

    #[test]
    fn flush_isolates_failures() {
        let got = Arc::new(Mutex::new(vec![]));
        let d = Dispatcher::new(vec![
            Box::new(Fake { id: "bad", ok: false, got: got.clone() }),
            Box::new(Fake { id: "good", ok: true, got: got.clone() }),
        ]);
        let mut s = Store::open_in_memory().unwrap();
        let rules = Rules { meituan_max_star: 3.5, dianping_max_star: 3.5, platforms: Platform::ALL.to_vec(), only_unreplied: false };
        s.ingest(&[rv("难吃", 1.0)], &rules, &d.ids(), false, 0).unwrap();
        let rep = d.flush(&mut s, 0).unwrap();
        assert_eq!(rep, FlushReport { sent: 1, failed: 1 });
        assert_eq!(got.lock().unwrap().len(), 2);
        let due = s.due_outbox(i64::MAX).unwrap();
        assert_eq!(due.len(), 1, "只剩失败的那条待重试");
        assert_eq!((due[0].notifier.as_str(), due[0].attempts), ("bad", 1));
    }
}
