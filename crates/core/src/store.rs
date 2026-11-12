//! SQLite：评价表（去重）+ 推送发件箱（重试）+ 状态键值。
use crate::model::{Platform, Review};
use crate::rules::Rules;
use anyhow::Result;
use chrono::{DateTime, Local, TimeZone};
use rusqlite::{params, Connection, OptionalExtension};
use std::path::Path;

/// 失败后下一次重试的等待（分钟），第 5 次失败即放弃
const RETRY_MIN: [i64; 4] = [1, 5, 15, 15];
pub const MAX_ATTEMPTS: u32 = 5;

#[derive(Debug, Clone, PartialEq)]
pub struct OutboxItem {
    pub id: i64,
    pub review_key: String,
    pub notifier: String,
    pub attempts: u32,
}

#[derive(Debug, Clone, PartialEq)]
pub struct StoredReview {
    pub review: Review,
    pub is_bad: bool,
    pub read: bool,
    pub first_seen: DateTime<Local>,
}

pub struct Store {
    conn: Connection,
}

impl Store {
    pub fn open(path: &Path) -> Result<Self> {
        Self::init(Connection::open(path)?)
    }

    pub fn open_in_memory() -> Result<Self> {
        Self::init(Connection::open_in_memory()?)
    }

    fn init(conn: Connection) -> Result<Self> {
        conn.execute_batch(
            "PRAGMA journal_mode=WAL;
             CREATE TABLE IF NOT EXISTS reviews(
               key TEXT PRIMARY KEY, platform TEXT NOT NULL, feedback_id INTEGER NOT NULL,
               add_time INTEGER NOT NULL, star REAL NOT NULL, content TEXT NOT NULL,
               replied INTEGER NOT NULL, source TEXT NOT NULL, poi_name TEXT NOT NULL,
               user_name TEXT NOT NULL, is_bad INTEGER NOT NULL, read INTEGER NOT NULL DEFAULT 0,
               first_seen INTEGER NOT NULL);
             CREATE INDEX IF NOT EXISTS reviews_time ON reviews(add_time DESC);
             CREATE TABLE IF NOT EXISTS outbox(
               id INTEGER PRIMARY KEY AUTOINCREMENT, review_key TEXT NOT NULL, notifier TEXT NOT NULL,
               status TEXT NOT NULL DEFAULT 'pending', attempts INTEGER NOT NULL DEFAULT 0,
               next_at INTEGER NOT NULL, last_error TEXT,
               UNIQUE(review_key, notifier));
             CREATE TABLE IF NOT EXISTS state(k TEXT PRIMARY KEY, v TEXT NOT NULL);",
        )?;
        Ok(Self { conn })
    }

    /// 入库；返回本次**新出现**的差评。`baseline=true` 时只入库、不进发件箱。
    /// 已存在的评价只刷新「是否已回复」。
    pub fn ingest(&mut self, reviews: &[Review], rules: &Rules, notifiers: &[&str], baseline: bool, now_ms: i64) -> Result<Vec<Review>> {
        let tx = self.conn.transaction()?;
        let mut fresh = Vec::new();
        for r in reviews {
            let bad = rules.is_bad(r);
            let inserted = tx.execute(
                "INSERT OR IGNORE INTO reviews(key,platform,feedback_id,add_time,star,content,replied,source,poi_name,user_name,is_bad,first_seen)
                 VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12)",
                params![r.key, r.platform.key_prefix(), r.feedback_id, r.add_time.timestamp_millis(), r.star, r.content,
                        r.replied, r.source, r.poi_name, r.user_name, bad, now_ms],
            )?;
            if inserted == 0 {
                tx.execute("UPDATE reviews SET replied=?2 WHERE key=?1", params![r.key, r.replied])?;
                continue;
            }
            if bad {
                if !baseline {
                    for n in notifiers {
                        tx.execute(
                            "INSERT OR IGNORE INTO outbox(review_key, notifier, next_at) VALUES(?1,?2,?3)",
                            params![r.key, n, now_ms],
                        )?;
                    }
                }
                fresh.push(r.clone());
            }
        }
        tx.commit()?;
        Ok(fresh)
    }

    pub fn due_outbox(&self, now_ms: i64) -> Result<Vec<OutboxItem>> {
        let mut st = self.conn.prepare(
            "SELECT id, review_key, notifier, attempts FROM outbox WHERE status='pending' AND next_at<=?1 ORDER BY id",
        )?;
        let rows = st.query_map([now_ms], |r| {
            Ok(OutboxItem { id: r.get(0)?, review_key: r.get(1)?, notifier: r.get(2)?, attempts: r.get(3)? })
        })?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    pub fn mark_sent(&self, id: i64) -> Result<()> {
        self.conn.execute("UPDATE outbox SET status='sent', attempts=attempts+1, last_error=NULL WHERE id=?1", [id])?;
        Ok(())
    }

    /// 记一次失败：按 1/5/15/15 分钟排下一次，第 5 次失败标记 gave_up
    pub fn mark_failed(&self, id: i64, now_ms: i64, err: &str) -> Result<()> {
        let attempts: u32 = self.conn.query_row("SELECT attempts FROM outbox WHERE id=?1", [id], |r| r.get(0))?;
        let attempts = attempts + 1;
        if attempts >= MAX_ATTEMPTS {
            self.conn.execute(
                "UPDATE outbox SET status='gave_up', attempts=?2, last_error=?3 WHERE id=?1",
                params![id, attempts, err],
            )?;
        } else {
            let next = now_ms + RETRY_MIN[(attempts - 1) as usize] * 60_000;
            self.conn.execute(
                "UPDATE outbox SET attempts=?2, next_at=?3, last_error=?4 WHERE id=?1",
                params![id, attempts, next, err],
            )?;
        }
        Ok(())
    }

    pub fn outbox_status(&self, id: i64) -> Result<(String, u32, i64)> {
        Ok(self.conn.query_row("SELECT status, attempts, next_at FROM outbox WHERE id=?1", [id], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?))
        })?)
    }

    /// 放弃发送的条数（界面上提示）
    pub fn gave_up_count(&self) -> Result<u32> {
        Ok(self.conn.query_row("SELECT COUNT(*) FROM outbox WHERE status='gave_up'", [], |r| r.get(0))?)
    }

    pub fn get_review(&self, key: &str) -> Result<Option<Review>> {
        Ok(self
            .conn
            .query_row(&format!("SELECT {COLS} FROM reviews WHERE key=?1"), [key], row_to_stored)
            .optional()?
            .map(|s| s.review))
    }

    /// 最新的评价（按发布时间倒序）
    pub fn recent(&self, limit: u32, only_bad: bool) -> Result<Vec<StoredReview>> {
        let sql = format!(
            "SELECT {COLS} FROM reviews {} ORDER BY add_time DESC LIMIT ?1",
            if only_bad { "WHERE is_bad=1" } else { "" }
        );
        let mut st = self.conn.prepare(&sql)?;
        let rows = st.query_map([limit], row_to_stored)?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    pub fn unread_bad_count(&self) -> Result<u32> {
        Ok(self.conn.query_row("SELECT COUNT(*) FROM reviews WHERE is_bad=1 AND read=0", [], |r| r.get(0))?)
    }

    pub fn mark_read(&self, key: &str) -> Result<()> {
        self.set_read(key, true)
    }

    pub fn set_read(&self, key: &str, read: bool) -> Result<()> {
        self.conn.execute("UPDATE reviews SET read=?2 WHERE key=?1", rusqlite::params![key, read as i32])?;
        Ok(())
    }

    pub fn mark_all_read(&self) -> Result<()> {
        self.conn.execute("UPDATE reviews SET read=1 WHERE read=0", [])?;
        Ok(())
    }

    /// 阈值改了以后重新标记 is_bad（不补发通知）
    pub fn reclassify(&mut self, rules: &Rules) -> Result<()> {
        let all = self.recent(u32::MAX, false)?;
        let tx = self.conn.transaction()?;
        for s in all {
            tx.execute("UPDATE reviews SET is_bad=?2 WHERE key=?1", params![s.review.key, rules.is_bad(&s.review)])?;
        }
        tx.commit()?;
        Ok(())
    }

    pub fn get_state(&self, k: &str) -> Result<Option<String>> {
        Ok(self.conn.query_row("SELECT v FROM state WHERE k=?1", [k], |r| r.get(0)).optional()?)
    }

    pub fn set_state(&self, k: &str, v: &str) -> Result<()> {
        self.conn.execute("INSERT INTO state(k,v) VALUES(?1,?2) ON CONFLICT(k) DO UPDATE SET v=excluded.v", [k, v])?;
        Ok(())
    }
}

const COLS: &str = "key,platform,feedback_id,add_time,star,content,replied,source,poi_name,user_name,is_bad,read,first_seen";

fn row_to_stored(r: &rusqlite::Row) -> rusqlite::Result<StoredReview> {
    let ms = |i: usize| -> rusqlite::Result<DateTime<Local>> {
        let v: i64 = r.get(i)?;
        Ok(Local.timestamp_millis_opt(v).single().unwrap_or_else(Local::now))
    };
    let p: String = r.get(1)?;
    Ok(StoredReview {
        review: Review {
            key: r.get(0)?,
            platform: Platform::from_prefix(&p).unwrap_or(Platform::Meituan),
            feedback_id: r.get(2)?,
            add_time: ms(3)?,
            star: r.get(4)?,
            content: r.get(5)?,
            replied: r.get(6)?,
            source: r.get(7)?,
            poi_name: r.get(8)?,
            user_name: r.get(9)?,
        },
        is_bad: r.get(10)?,
        read: r.get(11)?,
        first_seen: ms(12)?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rv(p: Platform, id: i64, star: f32) -> Review {
        Review { key: Review::make_key(p, id), platform: p, feedback_id: id,
                 add_time: Local.timestamp_millis_opt(1_794_000_000_000 + id).unwrap(), star,
                 content: format!("c{id}"), replied: false, source: String::new(), poi_name: "店".into(), user_name: "u".into() }
    }
    fn rules() -> Rules {
        Rules { meituan_max_star: 3.5, dianping_max_star: 3.5, platforms: Platform::ALL.to_vec(), only_unreplied: false }
    }
    const NOW: i64 = 1_800_000_000_000;

    #[test]
    fn dedupe_same_key_twice() {
        let mut s = Store::open_in_memory().unwrap();
        let batch = [rv(Platform::Meituan, 1, 1.0), rv(Platform::Meituan, 2, 5.0)];
        assert_eq!(s.ingest(&batch, &rules(), &["toast"], false, NOW).unwrap().len(), 1, "只有差评算新差评");
        assert!(s.ingest(&batch, &rules(), &["toast"], false, NOW).unwrap().is_empty());
        assert_eq!(s.due_outbox(NOW).unwrap().len(), 1);
        assert_eq!(s.recent(10, false).unwrap().len(), 2);
    }

    #[test]
    fn same_id_different_platform_is_distinct() {
        let mut s = Store::open_in_memory().unwrap();
        let batch = [rv(Platform::Meituan, 7, 1.0), rv(Platform::Dianping, 7, 1.0)];
        assert_eq!(s.ingest(&batch, &rules(), &["toast"], false, NOW).unwrap().len(), 2);
    }

    #[test]
    fn set_read_toggles_and_counts() {
        let mut s = Store::open_in_memory().unwrap();
        s.ingest(&[rv(Platform::Dianping, 1, 1.0), rv(Platform::Dianping, 2, 2.0)], &rules(), &["toast"], false, NOW).unwrap();
        assert_eq!(s.unread_bad_count().unwrap(), 2);
        s.set_read("dp:1", true).unwrap();
        assert_eq!(s.unread_bad_count().unwrap(), 1);
        let read_of = |s: &Store, k: &str| s.recent(10, true).unwrap().into_iter().find(|r| r.review.key == k).unwrap().read;
        assert!(read_of(&s, "dp:1"));
        s.set_read("dp:1", false).unwrap();
        assert!(!read_of(&s, "dp:1"));
        assert_eq!(s.unread_bad_count().unwrap(), 2);
    }

    #[test]
    fn baseline_no_outbox() {
        let mut s = Store::open_in_memory().unwrap();
        s.ingest(&[rv(Platform::Dianping, 1, 1.0)], &rules(), &["toast"], true, NOW).unwrap();
        assert!(s.due_outbox(NOW).unwrap().is_empty());
        assert_eq!(s.recent(10, true).unwrap().len(), 1);
    }

    #[test]
    fn outbox_retry_schedule() {
        let mut s = Store::open_in_memory().unwrap();
        s.ingest(&[rv(Platform::Meituan, 1, 1.0)], &rules(), &["toast"], false, NOW).unwrap();
        let id = s.due_outbox(NOW).unwrap()[0].id;
        let mut t = NOW;
        for (i, wait) in [1i64, 5, 15, 15].iter().enumerate() {
            s.mark_failed(id, t, "boom").unwrap();
            let (st, attempts, next) = s.outbox_status(id).unwrap();
            assert_eq!((st.as_str(), attempts), ("pending", i as u32 + 1));
            assert_eq!(next, t + wait * 60_000);
            assert!(s.due_outbox(next - 1).unwrap().is_empty(), "未到时间不取");
            t = next;
        }
        s.mark_failed(id, t, "boom").unwrap();
        assert_eq!(s.outbox_status(id).unwrap().0, "gave_up");
        assert!(s.due_outbox(i64::MAX).unwrap().is_empty());
        assert_eq!(s.gave_up_count().unwrap(), 1);
    }

    #[test]
    fn outbox_survives_reopen() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("d.db");
        {
            let mut s = Store::open(&p).unwrap();
            s.ingest(&[rv(Platform::Meituan, 1, 1.0)], &rules(), &["toast"], false, NOW).unwrap();
        }
        let s = Store::open(&p).unwrap();
        assert_eq!(s.due_outbox(NOW).unwrap().len(), 1);
    }

    #[test]
    fn multi_notifier_rows() {
        let mut s = Store::open_in_memory().unwrap();
        s.ingest(&[rv(Platform::Meituan, 1, 1.0)], &rules(), &["toast", "x"], false, NOW).unwrap();
        let due = s.due_outbox(NOW).unwrap();
        assert_eq!(due.len(), 2);
        s.mark_sent(due[0].id).unwrap();
        assert_eq!(s.due_outbox(NOW).unwrap().len(), 1);
    }

    #[test]
    fn existing_review_updates_replied_and_read() {
        let mut s = Store::open_in_memory().unwrap();
        let mut r = rv(Platform::Meituan, 1, 1.0);
        s.ingest(&[r.clone()], &rules(), &[], false, NOW).unwrap();
        r.replied = true;
        s.ingest(&[r.clone()], &rules(), &[], false, NOW).unwrap();
        assert!(s.get_review(&r.key).unwrap().unwrap().replied);
        assert_eq!(s.unread_bad_count().unwrap(), 1);
        s.mark_read(&r.key).unwrap();
        assert_eq!(s.unread_bad_count().unwrap(), 0);
    }

    #[test]
    fn state_kv() {
        let s = Store::open_in_memory().unwrap();
        assert_eq!(s.get_state("a").unwrap(), None);
        s.set_state("a", "1").unwrap();
        s.set_state("a", "2").unwrap();
        assert_eq!(s.get_state("a").unwrap().as_deref(), Some("2"));
    }
}
