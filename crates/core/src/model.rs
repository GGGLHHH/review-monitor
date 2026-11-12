use chrono::{DateTime, Local};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Platform {
    Meituan,
    Dianping,
}

impl Platform {
    pub const ALL: [Platform; 2] = [Platform::Meituan, Platform::Dianping];

    /// 去重键前缀
    pub fn key_prefix(self) -> &'static str {
        match self {
            Platform::Meituan => "mt",
            Platform::Dianping => "dp",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Platform::Meituan => "美团",
            Platform::Dianping => "点评",
        }
    }

    /// 与 web/poller.js 中 LIST_URL 的键一致
    pub fn js_name(self) -> &'static str {
        match self {
            Platform::Meituan => "meituan",
            Platform::Dianping => "dianping",
        }
    }

    pub fn from_prefix(s: &str) -> Option<Platform> {
        match s {
            "mt" => Some(Platform::Meituan),
            "dp" => Some(Platform::Dianping),
            _ => None,
        }
    }
}

/// 一条评价（已清洗）
#[derive(Debug, Clone, PartialEq)]
pub struct Review {
    /// 去重键：`mt:<feedbackId>` / `dp:<feedbackId>`
    pub key: String,
    pub platform: Platform,
    pub feedback_id: i64,
    pub add_time: DateTime<Local>,
    pub star: f32,
    pub content: String,
    pub replied: bool,
    pub source: String,
    pub poi_name: String,
    pub user_name: String,
}

impl Review {
    pub fn make_key(platform: Platform, feedback_id: i64) -> String {
        format!("{}:{}", platform.key_prefix(), feedback_id)
    }
}
