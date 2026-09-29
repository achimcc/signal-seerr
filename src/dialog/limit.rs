//! How fast one member may write (Audit 3, B127).
//!
//! A token bucket per ACI: a burst of `BURST` messages, refilled at
//! `PER_MINUTE`. The first message over the limit gets one answer asking to
//! slow down; after that the bot stays silent until a token is back. Without
//! it, one member -- taken over or just annoyed -- could drive Seerr and TMDB
//! as fast as they can type, and push Signal into rate-limiting the bot's
//! account for everybody.
//!
//! Strangers never get here: they are answered once an hour at most anyway
//! (`STRANGER_QUIET`), so this map is bounded by the size of the household.

use crate::model::Aci;
use std::collections::HashMap;
use std::time::Instant;

/// Messages a member may send in one go.
pub const BURST: f64 = 10.0;
/// And how many come back per minute after that.
pub const PER_MINUTE: f64 = 10.0;

#[derive(Debug, PartialEq, Eq)]
pub enum Verdict {
    /// Within the limit: handle the message.
    Pass,
    /// The first message over the limit: say so, once.
    SlowDown,
    /// Still over the limit, already told: say nothing.
    Silent,
}

struct Bucket {
    tokens: f64,
    at: Instant,
    told: bool,
}

#[derive(Default)]
pub struct Limiter {
    buckets: HashMap<Aci, Bucket>,
}

impl Limiter {
    /// `now` is a parameter so a test can move the clock without sleeping.
    pub fn admit(&mut self, from: &Aci, now: Instant) -> Verdict {
        let bucket = self.buckets.entry(from.clone()).or_insert(Bucket {
            tokens: BURST,
            at: now,
            told: false,
        });
        let elapsed = now.saturating_duration_since(bucket.at).as_secs_f64();
        bucket.tokens = (bucket.tokens + elapsed * PER_MINUTE / 60.0).min(BURST);
        bucket.at = now;
        if bucket.tokens >= 1.0 {
            bucket.tokens -= 1.0;
            bucket.told = false;
            Verdict::Pass
        } else if !bucket.told {
            bucket.told = true;
            Verdict::SlowDown
        } else {
            Verdict::Silent
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn a_burst_passes_the_next_is_told_and_then_silence() {
        let mut l = Limiter::default();
        let a = Aci("a".into());
        let t = Instant::now();
        for _ in 0..10 {
            assert_eq!(l.admit(&a, t), Verdict::Pass);
        }
        assert_eq!(l.admit(&a, t), Verdict::SlowDown);
        assert_eq!(l.admit(&a, t), Verdict::Silent);
        assert_eq!(l.admit(&a, t), Verdict::Silent);
    }

    #[test]
    fn a_token_comes_back_after_six_seconds() {
        let mut l = Limiter::default();
        let a = Aci("a".into());
        let t = Instant::now();
        for _ in 0..10 {
            l.admit(&a, t);
        }
        assert_eq!(l.admit(&a, t + Duration::from_secs(3)), Verdict::SlowDown);
        assert_eq!(l.admit(&a, t + Duration::from_secs(6)), Verdict::Pass);
        // Told again next time it runs dry -- a new episode, a new warning.
        assert_eq!(l.admit(&a, t + Duration::from_secs(6)), Verdict::SlowDown);
    }

    #[test]
    fn one_member_s_flood_does_not_cost_another_member_anything() {
        let mut l = Limiter::default();
        let t = Instant::now();
        for _ in 0..20 {
            l.admit(&Aci("a".into()), t);
        }
        assert_eq!(l.admit(&Aci("b".into()), t), Verdict::Pass);
    }
}
