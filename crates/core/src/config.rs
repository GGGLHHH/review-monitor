//! 配置文件 config.toml：默认值、校验、损坏回退、原子保存。
use crate::model::Platform;
use crate::rules::Rules;
use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::path::Path;

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    pub poll: PollCfg,
    pub rules: RulesCfg,
    pub notify: NotifyCfg,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct PollCfg {
    pub interval_minutes: u32,
    pub jitter_percent: u32,
    pub overlap_minutes: u32,
    pub first_run_lookback_hours: u32,
    pub max_pages: u32,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct RulesCfg {
    pub meituan_max_star: f32,
    pub dianping_max_star: f32,
    pub platforms: Vec<Platform>,
    pub only_unreplied: bool,
    pub alert_existing_on_first_run: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct NotifyCfg {
    /// 启用的推送渠道 id，例如 ["toast"]
    pub enabled: Vec<String>,
}

impl Default for PollCfg {
    fn default() -> Self {
        Self { interval_minutes: 3, jitter_percent: 20, overlap_minutes: 120, first_run_lookback_hours: 48, max_pages: 5 }
    }
}
impl Default for RulesCfg {
    fn default() -> Self {
        Self { meituan_max_star: 3.5, dianping_max_star: 3.5, platforms: Platform::ALL.to_vec(),
               only_unreplied: false, alert_existing_on_first_run: false }
    }
}
impl Default for NotifyCfg {
    fn default() -> Self {
        Self { enabled: vec!["toast".into()] }
    }
}

/// 已知的推送渠道 id（新增渠道时在这里登记）
pub const KNOWN_NOTIFIERS: &[&str] = &["toast"];

impl Config {
    pub fn validate(&self) -> Result<(), Vec<String>> {
        let mut e = Vec::new();
        let p = &self.poll;
        if !(1..=120).contains(&p.interval_minutes) { e.push("轮询间隔必须在 1–120 分钟之间".into()); }
        if p.jitter_percent > 50 { e.push("随机抖动必须在 0–50% 之间".into()); }
        if p.overlap_minutes > 24 * 60 { e.push("重叠回看不能超过 1440 分钟".into()); }
        if !(1..=24 * 30).contains(&p.first_run_lookback_hours) { e.push("首次回看必须在 1–720 小时之间".into()); }
        if !(1..=50).contains(&p.max_pages) { e.push("最大翻页数必须在 1–50 之间".into()); }
        let r = &self.rules;
        for (name, v) in [("美团", r.meituan_max_star), ("点评", r.dianping_max_star)] {
            if !(0.5..=5.0).contains(&v) { e.push(format!("{name}差评阈值必须在 0.5–5 星之间")); }
        }
        if r.platforms.is_empty() { e.push("至少要监控一个平台".into()); }
        for n in &self.notify.enabled {
            if !KNOWN_NOTIFIERS.contains(&n.as_str()) { e.push(format!("未知的推送渠道：{n}")); }
        }
        if e.is_empty() { Ok(()) } else { Err(e) }
    }

    pub fn rules(&self) -> Rules {
        Rules {
            meituan_max_star: self.rules.meituan_max_star,
            dianping_max_star: self.rules.dianping_max_star,
            platforms: self.rules.platforms.clone(),
            only_unreplied: self.rules.only_unreplied,
        }
    }

    /// 读配置；文件不存在 → 默认值；损坏或不合法 → 默认值 + 警告，并把原文件备份为 `.bad`
    pub fn load_or_default(path: &Path) -> (Config, Option<String>) {
        let Ok(text) = std::fs::read_to_string(path) else { return (Config::default(), None) };
        let parsed = toml::from_str::<Config>(&text).map_err(|e| e.to_string()).and_then(|c| {
            c.validate().map(|_| c).map_err(|errs| errs.join("；"))
        });
        match parsed {
            Ok(c) => (c, None),
            Err(why) => {
                let bak = path.with_extension("toml.bad");
                let _ = std::fs::copy(path, &bak);
                (Config::default(), Some(format!("配置文件有误，已改用默认配置（原文件备份为 {}）：{why}", bak.display())))
            }
        }
    }

    /// 校验后原子写入（先写临时文件再改名）
    pub fn save(&self, path: &Path) -> Result<()> {
        self.validate().map_err(|e| anyhow::anyhow!(e.join("；")))?;
        if let Some(dir) = path.parent() { std::fs::create_dir_all(dir)?; }
        let tmp = path.with_extension("toml.tmp");
        std::fs::write(&tmp, toml::to_string_pretty(self)?)?;
        std::fs::rename(&tmp, path)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_match_spec() {
        let c = Config::default();
        assert_eq!((c.poll.interval_minutes, c.poll.jitter_percent, c.poll.overlap_minutes), (3, 20, 120));
        assert_eq!((c.poll.first_run_lookback_hours, c.poll.max_pages), (48, 5));
        assert_eq!((c.rules.meituan_max_star, c.rules.dianping_max_star), (3.5, 3.5));
        assert_eq!(c.rules.platforms, vec![Platform::Meituan, Platform::Dianping]);
        assert!(!c.rules.only_unreplied && !c.rules.alert_existing_on_first_run);
        assert_eq!(c.notify.enabled, vec!["toast".to_string()]);
        assert!(c.validate().is_ok());
    }

    #[test]
    fn validate_rejects() {
        let mut c = Config::default();
        c.poll.interval_minutes = 0;
        c.poll.jitter_percent = 60;
        c.rules.dianping_max_star = 6.0;
        let e = c.validate().unwrap_err();
        assert_eq!(e.len(), 3);
        assert!(e.iter().any(|m| m.contains("轮询间隔")));
        assert!(e.iter().any(|m| m.contains("点评差评阈值")));
    }

    #[test]
    fn corrupt_file_falls_back() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("config.toml");
        std::fs::write(&p, "this is = = not toml").unwrap();
        let (c, warn) = Config::load_or_default(&p);
        assert_eq!(c, Config::default());
        assert!(warn.unwrap().contains("默认配置"));
        assert!(d.path().join("config.toml.bad").exists());
    }

    #[test]
    fn invalid_values_in_file_fall_back() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("config.toml");
        std::fs::write(&p, "[poll]\ninterval_minutes = 999\n").unwrap();
        let (c, warn) = Config::load_or_default(&p);
        assert_eq!(c, Config::default());
        assert!(warn.unwrap().contains("轮询间隔"));
    }

    #[test]
    fn partial_file_uses_defaults_for_rest() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("config.toml");
        std::fs::write(&p, "[poll]\ninterval_minutes = 5\n").unwrap();
        let (c, warn) = Config::load_or_default(&p);
        assert!(warn.is_none());
        assert_eq!(c.poll.interval_minutes, 5);
        assert_eq!(c.poll.jitter_percent, 20);
    }

    #[test]
    fn save_roundtrip() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("sub").join("config.toml");
        let mut c = Config::default();
        c.rules.meituan_max_star = 2.0;
        c.rules.platforms = vec![Platform::Dianping];
        c.save(&p).unwrap();
        assert_eq!(Config::load_or_default(&p), (c.clone(), None));
        c.poll.interval_minutes = 0;
        assert!(c.save(&p).is_err(), "非法配置不写盘");
        assert_eq!(Config::load_or_default(&p).0.poll.interval_minutes, 3);
    }
}
