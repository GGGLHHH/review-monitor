//! 轮询节奏：间隔 ± 抖动、失败退避、查询时间窗口。
use rand::Rng;
use std::time::Duration;

/// 下一轮等待：interval × (1 ± jitter%)
pub fn next_delay(interval_min: u32, jitter_pct: u32, rng: &mut impl Rng) -> Duration {
    let base = interval_min as f64 * 60.0;
    let j = (jitter_pct.min(100) as f64) / 100.0;
    let factor = if j > 0.0 { rng.random_range(1.0 - j..=1.0 + j) } else { 1.0 };
    Duration::from_secs_f64((base * factor).max(30.0))
}

/// 连续失败 n 次后的等待：min(interval × 2^n, 60 分钟)
pub fn backoff(consecutive_failures: u32, interval_min: u32) -> Duration {
    let mins = (interval_min as u64).saturating_mul(1u64 << consecutive_failures.min(10));
    Duration::from_secs(mins.min(60) * 60)
}

/// 本轮查询窗口 [start_ms, end_ms]
pub fn query_window(last_success_ms: Option<i64>, now_ms: i64, overlap_min: u32, first_lookback_h: u32) -> (i64, i64) {
    let start = match last_success_ms {
        Some(t) => t.min(now_ms) - overlap_min as i64 * 60_000,
        None => now_ms - first_lookback_h as i64 * 3_600_000,
    };
    (start, now_ms)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn jitter_within_bounds() {
        let mut rng = rand::rng();
        for _ in 0..1000 {
            let d = next_delay(10, 20, &mut rng).as_secs_f64();
            assert!((480.0..=720.0).contains(&d), "{d}");
        }
        assert_eq!(next_delay(10, 0, &mut rng), Duration::from_secs(600));
    }

    #[test]
    fn window_first_run() {
        let now = 1_800_000_000_000;
        assert_eq!(query_window(None, now, 120, 48), (now - 48 * 3_600_000, now));
    }

    #[test]
    fn window_overlap() {
        let now = 1_800_000_000_000;
        let last = now - 600_000;
        assert_eq!(query_window(Some(last), now, 120, 48), (last - 120 * 60_000, now));
    }

    #[test]
    fn backoff_caps_at_60() {
        assert_eq!(backoff(1, 10), Duration::from_secs(20 * 60));
        assert_eq!(backoff(3, 10), Duration::from_secs(60 * 60));
        assert_eq!(backoff(30, 10), Duration::from_secs(60 * 60));
    }
}
