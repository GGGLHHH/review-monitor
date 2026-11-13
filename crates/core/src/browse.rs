//! 评价浏览：按经营宝网页版的筛选条件查评价列表（只读），给应用自己的评价页用。
//!
//! 筛选项和取值照网页版评价管理页（2026-11-13 核对 `evaluation.*.js`）：
//! 网页把每组筛选的 `filterKey = 选中的值` 原样放进列表请求体，时间范围另算成 `startTime / endTime`。
//! 网页的选项本来由 `getTagsNew` 下发，但那个接口直连会被网关 403（要页面签名），所以这里写死；
//! 网页改版改了选项名时，改 [`filter_groups`] 即可。
use crate::model::{Platform, Review};
use crate::parse::{clean_text, ParseError};
use chrono::{DateTime, Datelike, Days, Local, TimeZone};
use serde_json::Value;
use std::collections::BTreeMap;

/// 一组筛选（如「回复状态」），options 里的字符串就是发给接口的值
#[derive(Debug, Clone, PartialEq)]
pub struct FilterGroup {
    pub key: &'static str,
    pub label: &'static str,
    pub options: Vec<&'static str>,
    pub default: &'static str,
}

fn g(key: &'static str, label: &'static str, options: &[&'static str], default: &'static str) -> FilterGroup {
    FilterGroup { key, label, options: options.to_vec(), default }
}

/// 该平台网页版上能选的筛选（顺序即网页上的顺序）。美团的「用户」只有「全部」，不显示
pub fn filter_groups(p: Platform) -> Vec<FilterGroup> {
    let reply = g("replyType", "回复状态", &["全部", "未回复", "已回复"], "未回复");
    let time = g("timeType", "时间", &["近7天", "近30天", "全部"], "近30天");
    match p {
        Platform::Meituan => vec![
            reply,
            g("starType", "星级", &["全部", "中差评", "好评"], "全部"),
            g("sourceType", "来源", &["全部", "消费后评价"], "全部"),
            time,
        ],
        Platform::Dianping => vec![
            reply,
            g("starType", "星级", &["全部", "好评", "中评", "差评"], "全部"),
            g("sourceType", "来源", &["全部", "免费试评价", "打卡后评价", "消费后评价"], "全部"),
            g("userType", "用户", &["全部", "回头客", "当地人", "游客"], "全部"),
            time,
        ],
    }
}

/// 网页版的时间换算：近7天 = 6 天前 0 点 ~ 今天 24 点；近30天 = 29 天前 0 点起；全部 = -1 / -1
pub fn time_range(time_type: &str, now: DateTime<Local>) -> (i64, i64) {
    let back = match time_type {
        "近7天" => 6,
        "近30天" => 29,
        _ => return (-1, -1),
    };
    let day = |d: chrono::NaiveDate, h, m, s, ms| {
        let t = d.and_hms_milli_opt(h, m, s, ms).unwrap_or_default();
        Local.from_local_datetime(&t).earliest().map(|t| t.timestamp_millis()).unwrap_or(-1)
    };
    let today = now.date_naive();
    let start = today.checked_sub_days(Days::new(back)).unwrap_or(today);
    (day(start, 0, 0, 0, 0), day(today, 23, 59, 59, 999))
}

/// 列表请求体。`picked` 里没给或不认识的值按默认；平台上没有的组（美团的用户）也照网页发「全部」
pub fn list_body(p: Platform, poi_id: &str, page_no: u32, page_size: u32, picked: &BTreeMap<String, String>, now: DateTime<Local>) -> String {
    let mut body = serde_json::Map::new();
    for k in ["replyType", "starType", "sourceType", "userType"] { body.insert(k.into(), "全部".into()); }
    body.insert("timeType".into(), "近30天".into());
    for grp in filter_groups(p) {
        let v = picked.get(grp.key).map(String::as_str).filter(|v| grp.options.contains(v)).unwrap_or(grp.default);
        body.insert(grp.key.into(), v.into());
    }
    let (start, end) = time_range(body["timeType"].as_str().unwrap_or(""), now);
    body.insert("pageNo".into(), page_no.max(1).into());
    body.insert("pageSize".into(), page_size.into());
    body.insert("poiId".into(), poi_id.into());
    body.insert("startTime".into(), start.into());
    body.insert("endTime".into(), end.into());
    Value::Object(body).to_string()
}

// ---- 按「几星」细筛（网页版没有，接口也不认；在网页的星级档位里拉回来后本地再筛） ----

/// 这条评价算「几星」：半星归到低一档（2.5★ 算 2 星，3.5★ 算 3 星），0.5★ 算 1 星。
/// 这样美团的「中差评」正好是 1~3 星、「好评」正好是 4~5 星。
pub fn star_bucket(star: f32) -> u8 {
    (star + 1e-4).floor().clamp(1.0, 5.0) as u8
}

/// 网页星级档位的分数范围（照网页说明：美团中差评 0.5~3.5、好评 4~5；点评差评 0.5~2、中评 2.5~3.5、好评 4~5）
fn band_range(p: Platform, band: &str) -> (f32, f32) {
    match (p, band) {
        (Platform::Meituan, "中差评") => (0.5, 3.5),
        (_, "好评") => (4.0, 5.0),
        (Platform::Dianping, "中评") => (2.5, 3.5),
        (Platform::Dianping, "差评") => (0.5, 2.0),
        _ => (0.5, 5.0),
    }
}

/// 所有可能的分数（0.5 一档）
fn all_scores() -> impl Iterator<Item = f32> {
    (1..=10).map(|i| i as f32 * 0.5)
}

/// 网页的某个星级档位里可能出现哪些「几星」（界面上只给这些选）
pub fn stars_in_band(p: Platform, band: &str) -> Vec<u8> {
    let (lo, hi) = band_range(p, band);
    let mut v: Vec<u8> = all_scores().filter(|s| *s >= lo && *s <= hi).map(star_bucket).collect();
    v.dedup();
    v
}

/// 细筛时实际发给接口的星级档位：用户自己选了档位就照用；选的是「全部」，
/// 就挑一个**完整装下**所选星数的最窄档位（只看 1 星 → 发「中差评」），少翻很多页。
/// 一个星数跨两个档位时（点评的 2 星 = 2.0 差评 + 2.5 中评）只能发「全部」，不然会漏
pub fn star_band_for(p: Platform, band: &str, stars: &[u8]) -> String {
    let opts = filter_groups(p).into_iter().find(|g| g.key == "starType").map(|g| g.options).unwrap_or_default();
    if stars.is_empty() || band != "全部" || !opts.contains(&band) {
        return band.to_string();
    }
    let wanted: Vec<f32> = all_scores().filter(|s| stars.contains(&star_bucket(*s))).collect();
    opts.into_iter()
        .filter(|b| *b != "全部")
        .map(|b| (b, band_range(p, b)))
        .filter(|(_, (lo, hi))| wanted.iter().all(|s| s >= lo && s <= hi))
        .min_by(|(_, a), (_, b)| (a.1 - a.0).total_cmp(&(b.1 - b.0)))
        .map_or("全部", |(b, _)| b)
        .to_string()
}

/// 评价下面的一条回复（商家回复，或用户的追评 / 回复）
#[derive(Debug, Clone, PartialEq)]
pub struct ReplyItem {
    pub reply_id: i64,
    /// 这条是回给哪条回复的（0 表示直接回评价）
    pub follow_id: i64,
    pub from_merchant: bool,
    pub name: String,
    pub content: String,
    pub time: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Feedback {
    pub key: String,
    pub platform: Platform,
    pub feedback_id: i64,
    /// 匿名用户显示「匿名用户」
    pub user_name: String,
    pub star: f32,
    /// 「口味 5.0 · 服务 5.0」，没有为空
    pub sub_scores: String,
    pub time: String,
    pub content: String,
    /// 图片缩略图地址（公开 CDN，不用 cookie）
    pub pictures: Vec<String>,
    /// 同一组图片的大图地址（和 pictures 一一对应），点开放大时用
    pub pictures_full: Vec<String>,
    pub video_count: u32,
    pub source: String,
    pub deal_name: String,
    pub returned_customer: bool,
    pub replied: bool,
    pub replies: Vec<ReplyItem>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct FeedbackPage {
    pub total: u32,
    pub items: Vec<Feedback>,
}

/// 发布时间：今年的只写月日
pub fn fmt_time(ms: i64, now: DateTime<Local>) -> String {
    match Local.timestamp_millis_opt(ms).single() {
        Some(t) if t.year() == now.year() => t.format("%m-%d %H:%M").to_string(),
        Some(t) => t.format("%Y-%m-%d %H:%M").to_string(),
        None => String::new(),
    }
}

fn s(v: &Value, k: &str) -> String {
    v.get(k).and_then(Value::as_str).unwrap_or("").to_string()
}

fn i(v: &Value, k: &str) -> i64 {
    v.get(k).and_then(|x| x.as_i64().or_else(|| x.as_str().and_then(|s| s.parse().ok()))).unwrap_or(0)
}

/// 美团的图片地址给的是 `http://`（点评是 https），安卓默认不让走明文 http；美团图床 https 也能取（2026-11-13 实测）
fn https(u: &str) -> String {
    if let Some(rest) = u.strip_prefix("http://") { format!("https://{rest}") }
    else if let Some(rest) = u.strip_prefix("//") { format!("https://{rest}") }
    else { u.to_string() }
}

/// 回复列表可能是树（children）：按出现顺序摊平
fn collect_replies(v: Option<&Value>, now: DateTime<Local>, out: &mut Vec<ReplyItem>) {
    let Some(Value::Array(a)) = v else { return };
    for r in a {
        let content = clean_text(&s(r, "content"));
        let from_merchant = !r.get("isUserReply").and_then(Value::as_bool).unwrap_or(false);
        let id = i(r, "replyId");
        if !content.is_empty() && !out.iter().any(|x| x.reply_id == id && id != 0) {
            let time = Some(s(r, "replyTimeStr")).filter(|t| !t.is_empty())
                .unwrap_or_else(|| fmt_time(i(r, "replyTime"), now));
            let name = Some(s(r, "userName")).filter(|n| !n.is_empty())
                .unwrap_or_else(|| if from_merchant { "商家".into() } else { "用户".into() });
            out.push(ReplyItem { reply_id: id, follow_id: i(r, "followId"), from_merchant, name, content, time });
        }
        collect_replies(r.get("children"), now, out);
    }
}

pub fn parse_page(p: Platform, body: &str, now: DateTime<Local>) -> Result<FeedbackPage, ParseError> {
    let v: Value = serde_json::from_str(body).map_err(|_| ParseError::Json)?;
    let inner = v.get("data").and_then(|d| d.get("data")).ok_or(ParseError::Shape("data.data"))?;
    let total = inner.get("total").and_then(Value::as_u64).unwrap_or(0) as u32;
    let list = match inner.get("feedbacks") {
        Some(Value::Array(a)) => a.as_slice(),
        Some(Value::Null) | None => &[][..],
        _ => return Err(ParseError::Shape("data.data.feedbacks")),
    };
    let mut items = Vec::with_capacity(list.len());
    for f in list {
        let id = f.get("feedbackId").and_then(Value::as_i64).ok_or(ParseError::Shape("feedbackId"))?;
        let star = f.get("accurateScore").and_then(Value::as_f64).or_else(|| f.get("score").and_then(Value::as_f64)).unwrap_or(0.0) as f32;
        let anonymous = f.get("anonymous").and_then(Value::as_bool).unwrap_or(false);
        let user_name = Some(s(f, "userName")).filter(|n| !n.is_empty() && !anonymous).unwrap_or_else(|| "匿名用户".into());
        let sub_scores = f.get("subScoreList").and_then(Value::as_array).map(|a| {
            a.iter().filter_map(|x| {
                let n = s(x, "name");
                let sc = x.get("score").map(|v| v.as_str().map(String::from).unwrap_or_else(|| v.to_string())).unwrap_or_default();
                (!n.is_empty() && !sc.is_empty()).then(|| format!("{n} {sc}"))
            }).collect::<Vec<_>>().join(" · ")
        }).unwrap_or_default();
        // 每张图取一对地址：列表里显示缩略图（thumbUrl），点开看大图（url / bigUrl）；缺哪个就用另一个顶上
        let pick = |x: &Value, keys: &[&str]| -> Option<String> {
            x.as_str().map(String::from).or_else(|| keys.iter().find_map(|k| x.get(*k).and_then(Value::as_str).filter(|u| !u.is_empty()).map(String::from)))
        };
        let (pictures, pictures_full): (Vec<String>, Vec<String>) = f.get("pictures").and_then(Value::as_array).map(|a| {
            a.iter().filter_map(|x| {
                let thumb = pick(x, &["thumbUrl", "url", "bigUrl"])?;
                let full = pick(x, &["bigUrl", "url", "thumbUrl"]).unwrap_or_else(|| thumb.clone());
                Some((https(&thumb), https(&full)))
            }).unzip()
        }).unwrap_or_default();
        let video_count = f.get("videos").and_then(Value::as_array).map_or(0, |a| a.len() as u32);
        let mut replies = Vec::new();
        collect_replies(f.get("merchantReplyList"), now, &mut replies);
        collect_replies(f.get("userReplyList"), now, &mut replies);
        let replied = i(f, "replyCount") > 0 || replies.iter().any(|r| r.from_merchant);
        items.push(Feedback {
            key: Review::make_key(p, id),
            platform: p,
            feedback_id: id,
            user_name,
            star,
            sub_scores,
            time: fmt_time(i(f, "addTime"), now),
            content: clean_text(&s(f, "content")),
            pictures,
            pictures_full,
            video_count,
            source: s(f, "sourceDesc"),
            deal_name: s(f, "dealName"),
            returned_customer: f.get("returnedCustomer").and_then(Value::as_bool).unwrap_or(false),
            replied,
            replies,
        });
    }
    Ok(FeedbackPage { total, items })
}

// ---------------- 回复（写操作，公开可见）----------------

/// 网页版回复框的上限
pub const REPLY_MAX_CHARS: usize = 500;

/// 网页版「回复」评价的请求体（`followReplyPCNew`）。
/// 网页直接回复评价时调用 `reply(feedback.feedbackId, feedback.replyId, 内容)`，评价对象上没有 replyId，
/// 所以 followId 是 undefined、JSON 里就没有这个字段；platform 是数字（美团 0、点评 1）。
pub fn reply_body(p: Platform, feedback_id: i64, content: &str) -> Result<String, String> {
    let c = content.trim();
    if c.is_empty() { return Err("回复内容不能为空".into()); }
    let n = c.chars().count();
    if n > REPLY_MAX_CHARS { return Err(format!("最多 {REPLY_MAX_CHARS} 字，现在 {n} 字")); }
    let platform = match p { Platform::Meituan => 0, Platform::Dianping => 1 };
    Ok(serde_json::json!({ "platform": platform, "feedbackId": feedback_id, "content": c }).to_string())
}

/// 网页版判断成败的办法：`data.resp.code == 200` 算成功，否则把 `data.resp.msg` 给用户看
pub fn reply_outcome(status: Option<u16>, body: &str) -> Result<(), String> {
    if status != Some(200) { return Err(format!("经营宝返回错误（HTTP {}）", status.unwrap_or(0))); }
    let v: Value = serde_json::from_str(body).map_err(|_| "经营宝返回的不是 JSON（可能被拦截了）".to_string())?;
    let resp = v.get("data").and_then(|d| d.get("resp"));
    let code = resp.and_then(|r| r.get("code")).and_then(Value::as_i64);
    if code == Some(200) { return Ok(()); }
    let msg = resp.and_then(|r| r.get("msg")).and_then(Value::as_str).unwrap_or("").trim();
    Err(if msg.is_empty() { format!("回复失败（code {}）", code.map_or("无".into(), |c| c.to_string())) } else { format!("回复失败：{msg}") })
}

/// 列表接口说「没登录」：401，或正文里 status/code 是 303、401
pub fn is_login_lost(status: Option<u16>, body: &str) -> bool {
    if status == Some(401) { return true; }
    let Ok(v) = serde_json::from_str::<Value>(body) else { return false };
    ["status", "code"].iter().any(|k| v.get(*k).and_then(Value::as_i64).is_some_and(|c| c == 303 || c == 401))
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::NaiveDate;

    fn at(y: i32, m: u32, d: u32, h: u32) -> DateTime<Local> {
        Local.from_local_datetime(&NaiveDate::from_ymd_opt(y, m, d).unwrap().and_hms_opt(h, 0, 0).unwrap()).unwrap()
    }

    #[test]
    fn groups_match_web() {
        let mt = filter_groups(Platform::Meituan);
        assert_eq!(mt.iter().map(|g| g.key).collect::<Vec<_>>(), ["replyType", "starType", "sourceType", "timeType"]);
        assert!(mt[1].options.contains(&"中差评"));
        let dp = filter_groups(Platform::Dianping);
        assert_eq!(dp.len(), 5);
        assert_eq!(dp[3].options, ["全部", "回头客", "当地人", "游客"]);
        for g in mt.iter().chain(&dp) { assert!(g.options.contains(&g.default), "{} 的默认值要在选项里", g.key); }
    }

    #[test]
    fn time_range_like_web() {
        let now = at(2026, 11, 13, 15);
        let (s, e) = time_range("近7天", now);
        assert_eq!(s, at(2026, 11, 7, 0).timestamp_millis());
        assert_eq!(e, at(2026, 11, 14, 0).timestamp_millis() - 1);
        assert_eq!(time_range("近30天", now).0, at(2026, 10, 15, 0).timestamp_millis());
        assert_eq!(time_range("全部", now), (-1, -1));
    }

    #[test]
    fn body_uses_picked_or_default() {
        let now = at(2026, 11, 13, 15);
        let mut picked = BTreeMap::new();
        picked.insert("starType".to_string(), "差评".to_string());
        picked.insert("userType".to_string(), "回头客".to_string());
        picked.insert("timeType".to_string(), "全部".to_string());
        let dp: Value = serde_json::from_str(&list_body(Platform::Dianping, "72", 2, 20, &picked, now)).unwrap();
        assert_eq!((dp["starType"].as_str(), dp["userType"].as_str(), dp["replyType"].as_str()), (Some("差评"), Some("回头客"), Some("未回复")));
        assert_eq!((dp["pageNo"].as_u64(), dp["poiId"].as_str(), dp["startTime"].as_i64()), (Some(2), Some("72"), Some(-1)));
        // 美团没有「差评」这个值、也没有用户筛选：按默认，用户发「全部」
        let mt: Value = serde_json::from_str(&list_body(Platform::Meituan, "72", 0, 20, &picked, now)).unwrap();
        assert_eq!((mt["starType"].as_str(), mt["userType"].as_str(), mt["pageNo"].as_u64()), (Some("全部"), Some("全部"), Some(1)));
    }

    #[test]
    fn star_bucket_rounds_half_down() {
        let got: Vec<u8> = [0.5, 1.0, 1.5, 2.0, 2.5, 3.0, 3.5, 4.0, 4.5, 5.0, 0.0].iter().map(|s| star_bucket(*s)).collect();
        assert_eq!(got, [1, 1, 1, 2, 2, 3, 3, 4, 4, 5, 1]);
    }

    #[test]
    fn stars_in_each_band() {
        let (mt, dp) = (Platform::Meituan, Platform::Dianping);
        assert_eq!(stars_in_band(mt, "全部"), [1, 2, 3, 4, 5]);
        assert_eq!(stars_in_band(mt, "中差评"), [1, 2, 3]);
        assert_eq!(stars_in_band(mt, "好评"), [4, 5]);
        assert_eq!(stars_in_band(dp, "差评"), [1, 2]);
        assert_eq!(stars_in_band(dp, "中评"), [2, 3], "2.5 归 2 星");
        assert_eq!(stars_in_band(dp, "好评"), [4, 5]);
    }

    #[test]
    fn band_for_picks_narrowest() {
        let mt = Platform::Meituan;
        let dp = Platform::Dianping;
        assert_eq!(star_band_for(mt, "全部", &[1]), "中差评");
        assert_eq!(star_band_for(mt, "全部", &[4, 5]), "好评");
        assert_eq!(star_band_for(mt, "全部", &[3, 4]), "全部", "跨档位只能全拉");
        assert_eq!(star_band_for(dp, "全部", &[1]), "差评");
        assert_eq!(star_band_for(dp, "全部", &[3]), "中评");
        assert_eq!(star_band_for(dp, "全部", &[2]), "全部", "点评 2 星 = 2.0 差评 + 2.5 中评，只发一个档位会漏");
        assert_eq!(star_band_for(dp, "全部", &[4]), "好评");
        assert_eq!(star_band_for(dp, "全部", &[1, 3]), "全部");
        assert_eq!(star_band_for(dp, "中评", &[3]), "中评", "用户选的档位照用");
        assert_eq!(star_band_for(mt, "全部", &[]), "全部", "没细筛不改");
    }

    #[test]
    fn parse_page_with_replies() {
        let now = at(2026, 11, 13, 15);
        let ms = at(2026, 11, 10, 19).timestamp_millis();
        let body = serde_json::json!({"data": {"data": {"total": 41, "hasMore": true, "feedbacks": [
            {"feedbackId": 1, "addTime": ms, "accurateScore": 2.5, "score": 2, "anonymous": false, "userName": "小王",
             "content": "<img src=\"x\">菜凉了", "subScoreList": [{"name": "口味", "score": "2.0"}, {"name": "服务", "score": "3.0"}],
             "pictures": ["https://p/1.jpg", {"thumbUrl": "http://p/2s.jpg", "url": "http://p/2.jpg"}], "videos": [{}], "sourceDesc": "消费后评价",
             "returnedCustomer": true, "replyCount": 1,
             "merchantReplyList": [{"replyId": 9, "followId": 0, "isUserReply": false, "userName": "商家", "content": "抱歉", "replyTimeStr": "11/10 20:20",
                 "children": [{"replyId": 10, "followId": 9, "isUserReply": true, "userName": "小王", "content": "好吧", "replyTime": ms}]}],
             "userReplyList": []},
            {"feedbackId": 2, "addTime": 1_600_000_000_000i64, "score": 1, "anonymous": true, "userName": "x", "content": "", "replyCount": 0,
             "merchantReplyList": [], "subScoreList": null}
        ]}}}).to_string();
        let p = parse_page(Platform::Dianping, &body, now).unwrap();
        assert_eq!(p.total, 41);
        let a = &p.items[0];
        assert_eq!((a.key.as_str(), a.star, a.content.as_str(), a.user_name.as_str()), ("dp:1", 2.5, "菜凉了", "小王"));
        assert_eq!(a.sub_scores, "口味 2.0 · 服务 3.0");
        assert_eq!((a.pictures.len(), a.video_count, a.returned_customer, a.replied), (2, 1, true, true));
        assert_eq!(a.pictures[1], "https://p/2s.jpg", "优先缩略图，http 升成 https");
        assert_eq!(a.pictures_full, ["https://p/1.jpg", "https://p/2.jpg"], "大图和缩略图一一对应");
        assert_eq!(a.time, "11-10 19:00");
        assert_eq!(a.replies.len(), 2);
        assert!(a.replies[0].from_merchant && !a.replies[1].from_merchant);
        assert_eq!((a.replies[1].follow_id, a.replies[1].time.as_str()), (9, "11-10 19:00"));
        let b = &p.items[1];
        assert_eq!((b.user_name.as_str(), b.star, b.replied), ("匿名用户", 1.0, false));
        assert!(b.time.starts_with("2020-"), "往年的带年份");
    }

    /// 拿真实接口返回核对解析：`RM_BROWSE_SAMPLES=mt:a.json;dp:b.json cargo test -p rm-core real_samples -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn real_samples() {
        let list = std::env::var("RM_BROWSE_SAMPLES").expect("设置 RM_BROWSE_SAMPLES");
        for item in list.split(';') {
            let (plat, path) = item.split_once(':').unwrap();
            let p = if plat == "mt" { Platform::Meituan } else { Platform::Dianping };
            let body = std::fs::read_to_string(path).unwrap();
            let page = parse_page(p, &body, Local::now()).unwrap();
            println!("{path}: total {} 本页 {}", page.total, page.items.len());
            for f in page.items.iter().take(3) {
                println!("  {} {}★ {} 图{} 回复{}条 子评分[{}] 内容{}字", f.key, f.star, f.time, f.pictures.len(), f.replies.len(), f.sub_scores, f.content.chars().count());
            }
        }
    }

    #[test]
    fn reply_body_like_web() {
        let b: Value = serde_json::from_str(&reply_body(Platform::Dianping, 355, "  谢谢支持  ").unwrap()).unwrap();
        assert_eq!(b, serde_json::json!({"platform": 1, "feedbackId": 355, "content": "谢谢支持"}));
        assert!(b.get("followId").is_none(), "直接回评价不带 followId");
        assert_eq!(reply_body(Platform::Meituan, 1, " \n ").unwrap_err(), "回复内容不能为空");
        assert!(reply_body(Platform::Meituan, 1, &"好".repeat(500)).is_ok());
        assert!(reply_body(Platform::Meituan, 1, &"好".repeat(501)).unwrap_err().contains("501"));
    }

    #[test]
    fn reply_outcome_like_web() {
        assert!(reply_outcome(Some(200), r#"{"data":{"data":true,"resp":{"code":200,"msg":"成功"}}}"#).is_ok());
        assert_eq!(reply_outcome(Some(200), r#"{"data":{"resp":{"code":500,"msg":"含有敏感词"}}}"#).unwrap_err(), "回复失败：含有敏感词");
        assert!(reply_outcome(Some(403), "<html>").unwrap_err().contains("403"));
        assert!(reply_outcome(Some(200), "<html>").is_err());
    }

    #[test]
    fn login_lost_detection() {
        assert!(is_login_lost(Some(401), ""));
        assert!(is_login_lost(Some(200), r#"{"status":303,"message":"您尚未登录"}"#));
        assert!(!is_login_lost(Some(200), r#"{"data":{"data":{"total":0}}}"#));
        assert!(!is_login_lost(Some(403), "<html>"));
    }
}
