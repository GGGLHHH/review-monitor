//! 解析 queryMTFeedbackPCNew / queryDPFeedbackPCNew 的响应。字段说明见 docs/接口调研.md。
use crate::model::{Platform, Review};
use chrono::{Local, TimeZone};
use serde_json::Value;

#[derive(Debug, thiserror::Error, PartialEq)]
pub enum ParseError {
    #[error("响应不是合法 JSON")]
    Json,
    #[error("响应结构不符（缺少 {0}），接口可能已变更")]
    Shape(&'static str),
}

#[derive(Debug, Clone, PartialEq)]
pub struct ListPage {
    pub total: u32,
    pub reviews: Vec<Review>,
}

pub fn parse_list(platform: Platform, body: &str) -> Result<ListPage, ParseError> {
    let v: Value = serde_json::from_str(body).map_err(|_| ParseError::Json)?;
    let inner = v
        .get("data")
        .and_then(|d| d.get("data"))
        .ok_or(ParseError::Shape("data.data"))?;
    let total = inner.get("total").and_then(Value::as_u64).unwrap_or(0) as u32;
    let list = match inner.get("feedbacks") {
        Some(Value::Array(a)) => a.as_slice(),
        Some(Value::Null) | None if total == 0 => &[][..],
        _ => return Err(ParseError::Shape("data.data.feedbacks")),
    };
    let mut reviews = Vec::with_capacity(list.len());
    for f in list {
        let Some(id) = f.get("feedbackId").and_then(Value::as_i64) else {
            return Err(ParseError::Shape("feedbackId"));
        };
        let star = f
            .get("accurateScore")
            .and_then(Value::as_f64)
            .or_else(|| f.get("score").and_then(Value::as_f64))
            .ok_or(ParseError::Shape("score"))? as f32;
        let add_ms = f.get("addTime").and_then(Value::as_i64).ok_or(ParseError::Shape("addTime"))?;
        let add_time = Local.timestamp_millis_opt(add_ms).single().ok_or(ParseError::Shape("addTime"))?;
        let non_empty = |k: &str| f.get(k).and_then(Value::as_array).is_some_and(|a| !a.is_empty());
        let replied = f.get("replyCount").and_then(Value::as_i64).unwrap_or(0) > 0
            || non_empty("merchantReplyList")
            || non_empty("replyList");
        let s = |k: &str| f.get(k).and_then(Value::as_str).unwrap_or("").to_string();
        reviews.push(Review {
            key: Review::make_key(platform, id),
            platform,
            feedback_id: id,
            add_time,
            star,
            content: clean_text(&s("content")),
            replied,
            source: s("sourceDesc"),
            poi_name: s("poiName"),
            user_name: s("userName"),
        });
    }
    Ok(ListPage { total, reviews })
}

/// 去掉 HTML 标签、解码常见实体、把连续空白合并成一个空格
pub fn clean_text(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut in_tag = false;
    for ch in s.chars() {
        match (in_tag, ch) {
            (false, '<') => { in_tag = true; out.push(' '); }
            (true, '>') => in_tag = false,
            (true, _) => {}
            (false, c) => out.push(c),
        }
    }
    let decoded = out
        .replace("&nbsp;", " ")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&amp;", "&");
    decoded.split_whitespace().collect::<Vec<_>>().join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;
    const MT: &str = include_str!("../tests/fixtures/mt_list.json");
    const DP: &str = include_str!("../tests/fixtures/dp_list.json");

    #[test]
    fn parse_mt_fixture() {
        let p = parse_list(Platform::Meituan, MT).unwrap();
        assert_eq!(p.total, 7);
        let r = &p.reviews[0];
        assert_eq!(r.key, "mt:10566010284");
        assert_eq!(r.star, 1.0);
        assert!(!r.replied);
        assert_eq!(r.content, "");
        assert_eq!(p.reviews[1].star, 0.5, "优先 accurateScore");
        assert!(p.reviews[2].replied, "merchantReplyList 非空算已回复");
    }

    #[test]
    fn parse_dp_fixture() {
        let p = parse_list(Platform::Dianping, DP).unwrap();
        assert_eq!(p.total, 24);
        let r = &p.reviews[0];
        assert_eq!(r.key, "dp:3550718756");
        assert_eq!(r.star, 2.5);
        assert!(!r.content.contains("<img"));
        assert_eq!(r.content, "感觉真的好难吃啊 贵就贵在风景好");
        assert_eq!(p.reviews[1].star, 4.0, "无 accurateScore 时回退 score");
        assert!(p.reviews[1].replied);
    }

    #[test]
    fn clean_text_strips_html_and_empty() {
        assert_eq!(clean_text("<img src=\"x\">好吃\n\n "), "好吃");
        assert_eq!(clean_text(""), "");
        assert_eq!(clean_text("a&amp;b &lt;c&gt;"), "a&b <c>");
    }

    #[test]
    fn parse_rejects_non_json_and_bad_shape() {
        assert_eq!(parse_list(Platform::Meituan, "<html>").unwrap_err(), ParseError::Json);
        assert_eq!(parse_list(Platform::Meituan, r#"{"code":401}"#).unwrap_err(), ParseError::Shape("data.data"));
    }

    #[test]
    fn parse_empty_list() {
        let p = parse_list(Platform::Dianping, r#"{"data":{"data":{"total":0,"feedbacks":null}}}"#).unwrap();
        assert_eq!(p.total, 0);
        assert!(p.reviews.is_empty());
    }
}
