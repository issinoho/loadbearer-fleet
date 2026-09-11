//! How fast one credential may write to the collection folder.
//!
//! **This is a safety valve, not a defence.** What it is for is the loop: a
//! deployment script retrying on a timer somebody misread, or a credential
//! that got out and is filling a disk 40 KB at a time. What it is not for is
//! stopping somebody determined — they hold a real credential and have all the
//! time in the world — or absorbing a flood of *unauthenticated* requests,
//! which is the reverse proxy's job and which never reaches this anyway,
//! because a request with no principal is refused before there is anything to
//! count it against.
//!
//! Three decisions worth knowing:
//!
//! **It is keyed on the principal, never on the address.** Behind a reverse
//! proxy every request arrives from the proxy, and `X-Forwarded-For` is
//! whatever the client wrote unless the proxy is trusted to overwrite it — so
//! an address-keyed limit is either one bucket for the whole world or a limit
//! each caller picks for themselves. A subject comes from a credential that
//! has already been verified.
//!
//! **A refused request costs nothing.** A token bucket that charged for the
//! refusals would let a client hammering flat out hold itself at zero for
//! ever, which turns a misconfigured cron job into an outage for the
//! credential it shares. Rejections take no allowance; only work taken on
//! does.
//!
//! **Idle time banks, up to a minute of it.** A fleet does not submit evenly:
//! a rollout is ten thousand machines at 09:00 and nothing until tomorrow. A
//! full bucket absorbs a minute's worth of that at once, and the rest arrive
//! at the sustained rate with a `Retry-After` telling them when — which is
//! safe to act on, because ingest is idempotent by content and a resend of
//! something already stored is a question already answered.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// When to sweep the idle entries out.
///
/// Housekeeping rather than a bound: the keys are verified principals — a
/// staff list and a handful of tokens — not something a caller chooses, so the
/// map cannot be grown on purpose. The sweep exists because a server that runs
/// for a year should not still be holding a bucket for somebody who left.
const SWEEP_ABOVE: usize = 1024;

/// A cap on submissions per principal, refilled continuously.
pub struct Limiter {
    per_minute: u32,
    buckets: Mutex<HashMap<String, Bucket>>,
}

#[derive(Clone, Copy)]
struct Bucket {
    /// Submissions still available. Fractional because the refill is
    /// continuous — a bucket does not have to wait for a tick boundary.
    allowance: f64,
    last: Instant,
}

impl Limiter {
    /// `per_minute` of zero switches the limit off entirely.
    pub fn new(per_minute: u32) -> Self {
        Self {
            per_minute,
            buckets: Mutex::new(HashMap::new()),
        }
    }

    /// Take one submission's worth of allowance, or say how long until there
    /// is some.
    pub fn check(&self, key: &str) -> Result<(), Duration> {
        self.check_at(key, Instant::now())
    }

    /// The same, at a stated moment, so the refill is testable without
    /// sleeping through it.
    fn check_at(&self, key: &str, now: Instant) -> Result<(), Duration> {
        if self.per_minute == 0 {
            return Ok(());
        }
        let capacity = f64::from(self.per_minute);
        let per_second = capacity / 60.0;

        let mut buckets = self.buckets.lock().expect("rate limiter lock");
        if buckets.len() > SWEEP_ABOVE {
            // A bucket that has refilled to capacity is indistinguishable from
            // one that was never created, so dropping it changes no answer
            // this can give.
            buckets.retain(|_, b| refilled(b, now, per_second, capacity) < capacity);
        }

        let bucket = buckets.entry(key.to_string()).or_insert(Bucket {
            allowance: capacity,
            last: now,
        });
        bucket.allowance = refilled(bucket, now, per_second, capacity);
        bucket.last = now;

        if bucket.allowance >= 1.0 {
            bucket.allowance -= 1.0;
            return Ok(());
        }
        // Rounded up and never zero: a `Retry-After: 0` invites the caller
        // straight back into another refusal.
        let wait = ((1.0 - bucket.allowance) / per_second).ceil().max(1.0);
        Err(Duration::from_secs(wait as u64))
    }
}

fn refilled(bucket: &Bucket, now: Instant, per_second: f64, capacity: f64) -> f64 {
    let elapsed = now.saturating_duration_since(bucket.last).as_secs_f64();
    (bucket.allowance + elapsed * per_second).min(capacity)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_burst_the_size_of_the_bucket_goes_through_and_the_next_one_waits() {
        let limiter = Limiter::new(60);
        let t0 = Instant::now();
        for i in 0..60 {
            assert!(
                limiter.check_at("token:deploy", t0).is_ok(),
                "submission {i} of a full minute's worth"
            );
        }
        let wait = limiter
            .check_at("token:deploy", t0)
            .expect_err("the sixty-first in the same instant");
        assert_eq!(wait, Duration::from_secs(1), "one per second refills");
    }

    #[test]
    fn allowance_comes_back_continuously_rather_than_on_a_tick() {
        let limiter = Limiter::new(60);
        let t0 = Instant::now();
        for _ in 0..60 {
            limiter.check_at("token:deploy", t0).expect("the burst");
        }
        assert!(limiter.check_at("token:deploy", t0).is_err());

        // Half a second is half a submission at 60/minute: still short.
        let half = t0 + Duration::from_millis(500);
        assert!(limiter.check_at("token:deploy", half).is_err());

        assert!(
            limiter
                .check_at("token:deploy", t0 + Duration::from_secs(1))
                .is_ok(),
            "a second buys exactly one"
        );
        assert!(
            limiter
                .check_at("token:deploy", t0 + Duration::from_secs(1))
                .is_err(),
            "and only one"
        );
    }

    /// The failure this is meant to prevent is a client hammering flat out.
    /// Charging it for its refusals would hold it at zero for as long as it
    /// kept trying, and take the credential's other users down with it.
    #[test]
    fn a_refused_submission_does_not_spend_anything() {
        let limiter = Limiter::new(60);
        let t0 = Instant::now();
        for _ in 0..60 {
            limiter.check_at("token:deploy", t0).expect("the burst");
        }
        for _ in 0..1000 {
            assert!(limiter.check_at("token:deploy", t0).is_err());
        }
        assert!(
            limiter
                .check_at("token:deploy", t0 + Duration::from_secs(1))
                .is_ok(),
            "a thousand refusals later, one second is still worth one submission"
        );
    }

    #[test]
    fn the_bucket_never_fills_past_its_capacity() {
        let limiter = Limiter::new(60);
        let t0 = Instant::now();
        limiter.check_at("token:deploy", t0).expect("one");
        // An hour of idleness is not an hour of credit.
        let later = t0 + Duration::from_secs(3600);
        for _ in 0..60 {
            limiter.check_at("token:deploy", later).expect("a minute");
        }
        assert!(limiter.check_at("token:deploy", later).is_err());
    }

    #[test]
    fn one_credential_running_hot_does_not_slow_another() {
        let limiter = Limiter::new(60);
        let t0 = Instant::now();
        for _ in 0..60 {
            limiter.check_at("token:noisy", t0).expect("the burst");
        }
        assert!(limiter.check_at("token:noisy", t0).is_err());
        assert!(
            limiter.check_at("token:quiet", t0).is_ok(),
            "the limit is the credential's, not the server's"
        );
    }

    #[test]
    fn zero_is_off() {
        let limiter = Limiter::new(0);
        let t0 = Instant::now();
        for _ in 0..10_000 {
            limiter
                .check_at("token:deploy", t0)
                .expect("no limit at all");
        }
    }

    /// A server that has been up for a year should not still hold a bucket for
    /// somebody who left. Dropping a *full* one is free, because a full bucket
    /// and an absent one give the same answer to every question.
    #[test]
    fn idle_buckets_are_swept_without_changing_any_answer() {
        let limiter = Limiter::new(60);
        let t0 = Instant::now();
        for i in 0..=SWEEP_ABOVE {
            limiter
                .check_at(&format!("user:{i}"), t0)
                .expect("one each");
        }
        assert_eq!(limiter.buckets.lock().expect("lock").len(), SWEEP_ABOVE + 1);

        // A minute later they have all refilled, so the next call sweeps them.
        let later = t0 + Duration::from_secs(60);
        limiter.check_at("user:new", later).expect("one");
        assert_eq!(
            limiter.buckets.lock().expect("lock").len(),
            1,
            "only the caller that just arrived is still worth remembering"
        );

        // And the swept-away caller is exactly where it was: full.
        for _ in 0..60 {
            limiter.check_at("user:0", later).expect("a full minute");
        }
        assert!(limiter.check_at("user:0", later).is_err());
    }

    /// Somebody still mid-burst is *not* swept, in the same sweep that drops
    /// the idle ones — otherwise the sweep would hand back allowance nobody
    /// had waited for, and the limit could be reset at will by anyone able to
    /// trigger one.
    #[test]
    fn a_caller_still_spending_survives_the_sweep_that_drops_the_idle() {
        let limiter = Limiter::new(60);
        let t0 = Instant::now();
        for _ in 0..60 {
            limiter.check_at("token:deploy", t0).expect("the burst");
        }
        for i in 0..=SWEEP_ABOVE {
            limiter
                .check_at(&format!("user:{i}"), t0)
                .expect("one each");
        }

        // Half a minute on, the callers that spent one each are full again;
        // the one that emptied its bucket is only half way back.
        let half = t0 + Duration::from_secs(30);
        limiter.check_at("user:trigger", half).expect("sweeps");
        assert_eq!(
            limiter.buckets.lock().expect("lock").len(),
            2,
            "the one still spending, and the one that just arrived"
        );

        for i in 0..30 {
            limiter
                .check_at("token:deploy", half)
                .unwrap_or_else(|_| panic!("thirty seconds is worth thirty, not {i}"));
        }
        assert!(
            limiter.check_at("token:deploy", half).is_err(),
            "and not a sixtieth: being swept past must not refill a bucket"
        );
    }
}
