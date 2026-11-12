//! An adaptive (AIMD) token bucket, one per endpoint.
//!
//! The bucket starts unlimited. The first 429 sets a rate from the traffic
//! the endpoint was actually seeing and halves it (multiplicative decrease);
//! every success afterwards raises the rate a little (additive increase)
//! until it passes [`AimdLimiter::MAX_RATE`], where the bucket becomes
//! unlimited again.
//!
//! Concurrent requests that hit the same limit come back as a burst of
//! 429s: they are one signal, not many. At most one decrease is applied per
//! [`AimdLimiter::DECREASE_INTERVAL`]; later 429s in that interval only
//! drain the burst.
//!
//! Time is passed in explicitly so the limiter is deterministic under test.

use std::sync::Mutex;
use std::time::{Duration, Instant};

/// An adaptive token bucket. Cheap to call from many tasks.
#[derive(Debug)]
pub(crate) struct AimdLimiter {
    state: Mutex<State>,
}

#[derive(Debug)]
struct State {
    /// Tokens per second; `None` means unlimited.
    rate: Option<f64>,
    /// May go negative: each granted reservation takes one token.
    tokens: f64,
    refilled: Instant,
    /// Start and count of the current one-second observation window.
    window_start: Instant,
    window_count: u32,
    /// Count of the previous complete window.
    prev_count: u32,
    /// When the rate was last decreased.
    decreased: Option<Instant>,
}

/// Traffic is counted per window this long: the rate the first 429 sets
/// comes from the requests seen in the current or the previous window.
const OBSERVE_WINDOW: Duration = Duration::from_secs(1);

impl AimdLimiter {
    /// The lowest rate a run of 429s can push the bucket to, in requests
    /// per second.
    const MIN_RATE: f64 = 1.0;
    /// The least traffic the first 429 is assumed to have come from, in
    /// requests per second. A 429 at a trickle says little about the
    /// endpoint's real limit, so the first step never goes below half this.
    const FIRST_RATE: f64 = 4.0;
    /// Above this rate the bucket stops limiting.
    const MAX_RATE: f64 = 500.0;
    /// Requests per second added per second of successful traffic.
    const INCREASE: f64 = 1.0;
    /// At most one decrease per this interval. It is never shorter than
    /// one request at the current rate, because the rate never drops below
    /// [`MIN_RATE`](Self::MIN_RATE) (1 request per second).
    const DECREASE_INTERVAL: Duration = Duration::from_secs(1);

    /// An unlimited bucket.
    pub fn new(now: Instant) -> Self {
        AimdLimiter {
            state: Mutex::new(State {
                rate: None,
                tokens: 0.0,
                refilled: now,
                window_start: now,
                window_count: 0,
                prev_count: 0,
                decreased: None,
            }),
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Reserve one request slot. `Ok(wait)` means the slot is reserved and
    /// the caller must wait `wait` (often zero) before sending. `Err(wait)`
    /// means the wait would exceed `max_wait`; nothing was reserved.
    pub fn try_acquire(&self, now: Instant, max_wait: Duration) -> Result<Duration, Duration> {
        let mut s = self.lock();
        s.observe(now);
        let Some(rate) = s.rate else {
            s.window_count += 1;
            return Ok(Duration::ZERO);
        };
        s.refill(now, rate);
        let wait = if s.tokens >= 1.0 {
            Duration::ZERO
        } else {
            Duration::from_secs_f64((1.0 - s.tokens) / rate)
        };
        if wait > max_wait {
            return Err(wait);
        }
        s.tokens -= 1.0;
        s.window_count += 1;
        Ok(wait)
    }

    /// A request succeeded: additive increase.
    pub fn on_success(&self) {
        let mut s = self.lock();
        if let Some(rate) = s.rate {
            // +INCREASE req/s for every `rate` successes, i.e. per second of
            // traffic at the full rate.
            let next = rate + Self::INCREASE / rate.max(1.0);
            s.rate = (next <= Self::MAX_RATE).then_some(next);
        }
    }

    /// The endpoint answered 429: multiplicative decrease, at most once
    /// per [`DECREASE_INTERVAL`](Self::DECREASE_INTERVAL). An unlimited
    /// bucket first adopts the rate it was observed sending at.
    pub fn on_rate_limited(&self, now: Instant) {
        let mut s = self.lock();
        s.observe(now);
        // No burst left after a 429.
        s.tokens = s.tokens.min(0.0);
        if s.rate.is_some()
            && s.decreased
                .is_some_and(|at| now.saturating_duration_since(at) < Self::DECREASE_INTERVAL)
        {
            return;
        }
        s.decreased = Some(now);
        let observed = f64::from(s.window_count.max(s.prev_count)).max(1.0);
        let base = match s.rate {
            Some(rate) => rate.min(observed.max(Self::MIN_RATE)),
            None => observed.max(Self::FIRST_RATE),
        };
        let rate = (base / 2.0).clamp(Self::MIN_RATE, Self::MAX_RATE);
        if s.rate.is_none() {
            s.refilled = now;
        }
        s.rate = Some(rate);
    }

    #[cfg(test)]
    /// The current rate in requests per second; `None` when unlimited.
    pub fn rate(&self) -> Option<f64> {
        self.lock().rate
    }
}

impl State {
    fn refill(&mut self, now: Instant, rate: f64) {
        let elapsed = now.saturating_duration_since(self.refilled).as_secs_f64();
        self.refilled = now;
        // Burst: one second's worth, at least one request.
        let burst = rate.max(1.0);
        self.tokens = (self.tokens + elapsed * rate).min(burst);
    }

    fn observe(&mut self, now: Instant) {
        let elapsed = now.saturating_duration_since(self.window_start);
        if elapsed >= 2 * OBSERVE_WINDOW {
            // The previous window had no traffic either.
            self.prev_count = 0;
            self.window_count = 0;
            self.window_start = now;
        } else if elapsed >= OBSERVE_WINDOW {
            self.prev_count = self.window_count;
            self.window_count = 0;
            self.window_start = now;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const NO_WAIT: Duration = Duration::ZERO;
    const TWO_S: Duration = Duration::from_secs(2);

    #[test]
    fn starts_unlimited() {
        let t = Instant::now();
        let l = AimdLimiter::new(t);
        for _ in 0..10_000 {
            assert_eq!(l.try_acquire(t, NO_WAIT), Ok(Duration::ZERO));
        }
        assert_eq!(l.rate(), None);
    }

    #[test]
    fn a_429_halves_the_observed_rate_and_spaces_requests() {
        let t = Instant::now();
        let l = AimdLimiter::new(t);
        for _ in 0..20 {
            l.try_acquire(t, NO_WAIT).unwrap();
        }
        l.on_rate_limited(t);
        assert_eq!(l.rate(), Some(10.0));
        // No burst right after a 429: the next request waits 1/rate.
        let wait = l.try_acquire(t, TWO_S).unwrap();
        assert_eq!(wait, Duration::from_millis(100));
        let wait = l.try_acquire(t, TWO_S).unwrap();
        assert_eq!(wait, Duration::from_millis(200));
        // A second 429 a second later halves again.
        l.on_rate_limited(t + Duration::from_secs(1));
        assert_eq!(l.rate(), Some(5.0));
    }

    #[test]
    fn a_wait_past_the_limit_reserves_nothing() {
        let t = Instant::now();
        let l = AimdLimiter::new(t);
        l.try_acquire(t, NO_WAIT).unwrap();
        l.on_rate_limited(t);
        assert_eq!(l.rate(), Some(AimdLimiter::FIRST_RATE / 2.0)); // 2 req/s
        for n in 1..=4 {
            assert_eq!(l.try_acquire(t, TWO_S), Ok(Duration::from_millis(500 * n)));
        }
        let refused = l.try_acquire(t, TWO_S).unwrap_err();
        assert_eq!(refused, Duration::from_millis(2500));
        // Refusals don't dig the hole deeper.
        assert_eq!(l.try_acquire(t, TWO_S).unwrap_err(), refused);
        // Time heals.
        let later = t + Duration::from_millis(2500);
        assert_eq!(l.try_acquire(later, TWO_S), Ok(Duration::ZERO));
    }

    #[test]
    fn successes_raise_the_rate_until_unlimited() {
        let t = Instant::now();
        let l = AimdLimiter::new(t);
        for _ in 0..8 {
            l.try_acquire(t, NO_WAIT).unwrap();
        }
        l.on_rate_limited(t);
        assert_eq!(l.rate(), Some(4.0));
        for _ in 0..4 {
            l.on_success();
        }
        let rate = l.rate().unwrap();
        assert!(rate > 4.9 && rate < 5.0, "{rate}");
        for _ in 0..200_000 {
            l.on_success();
        }
        assert_eq!(l.rate(), None, "past MAX_RATE the bucket is unlimited");
    }

    #[test]
    fn rate_never_drops_below_the_floor() {
        let t = Instant::now();
        let l = AimdLimiter::new(t);
        for i in 0..10 {
            l.on_rate_limited(t + AimdLimiter::DECREASE_INTERVAL * i);
        }
        assert_eq!(l.rate(), Some(AimdLimiter::MIN_RATE));
    }

    #[test]
    fn a_burst_of_429s_decreases_once() {
        let t = Instant::now();
        let l = AimdLimiter::new(t);
        for _ in 0..32 {
            l.try_acquire(t, NO_WAIT).unwrap();
        }
        // 32 concurrent requests all come back 429.
        for i in 0..32 {
            l.on_rate_limited(t + Duration::from_millis(i));
        }
        assert_eq!(l.rate(), Some(16.0), "one halving, not 32");
        // The burst is still drained by the later 429s.
        assert!(l.try_acquire(t + Duration::from_millis(40), TWO_S).unwrap() > NO_WAIT);
        // Once the interval has passed, a 429 is a new signal.
        l.on_rate_limited(t + AimdLimiter::DECREASE_INTERVAL);
        assert_eq!(l.rate(), Some(8.0));
    }
}
