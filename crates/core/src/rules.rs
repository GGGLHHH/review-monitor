use crate::model::{Platform, Review};

#[derive(Debug, Clone, PartialEq)]
pub struct Rules {
    pub meituan_max_star: f32,
    pub dianping_max_star: f32,
    pub platforms: Vec<Platform>,
    pub only_unreplied: bool,
}

impl Rules {
    pub fn max_star(&self, p: Platform) -> f32 {
        match p {
            Platform::Meituan => self.meituan_max_star,
            Platform::Dianping => self.dianping_max_star,
        }
    }

    pub fn is_bad(&self, r: &Review) -> bool {
        self.platforms.contains(&r.platform)
            && r.star <= self.max_star(r.platform) + 1e-4
            && !(self.only_unreplied && r.replied)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Local;

    fn rv(p: Platform, star: f32, replied: bool) -> Review {
        Review { key: Review::make_key(p, 1), platform: p, feedback_id: 1, add_time: Local::now(), star,
                 content: String::new(), replied, source: String::new(), poi_name: String::new(), user_name: String::new() }
    }
    fn rules() -> Rules {
        Rules { meituan_max_star: 3.5, dianping_max_star: 3.5, platforms: Platform::ALL.to_vec(), only_unreplied: false }
    }

    #[test]
    fn threshold_boundary() {
        assert!(rules().is_bad(&rv(Platform::Meituan, 3.5, false)));
        assert!(!rules().is_bad(&rv(Platform::Meituan, 3.6, false)));
        let r = Rules { dianping_max_star: 2.0, ..rules() };
        assert!(!r.is_bad(&rv(Platform::Dianping, 2.5, false)), "平台阈值独立");
    }

    #[test]
    fn platform_disabled() {
        let r = Rules { platforms: vec![Platform::Meituan], ..rules() };
        assert!(!r.is_bad(&rv(Platform::Dianping, 1.0, false)));
    }

    #[test]
    fn only_unreplied() {
        let r = Rules { only_unreplied: true, ..rules() };
        assert!(!r.is_bad(&rv(Platform::Meituan, 1.0, true)));
        assert!(r.is_bad(&rv(Platform::Meituan, 1.0, false)));
    }
}
