//! A token bucket for the provider API client (spec §7.2).
//!
//! The Webshare client keeps its own request rate below the documented
//! limits and stays away after a 429. The bucket runs on tokio's timer, so
//! tests drive it with a paused clock.

use std::sync::{Mutex, PoisonError};
use std::time::Duration;

use tokio::time::Instant;

/// A token bucket: `burst` requests may go at once, then one per refill
/// period. After a 429 the bucket can be blocked for a while.
#[derive(Debug)]
pub struct TokenBucket {
    capacity: f64,
    refill_per_second: f64,
    state: Mutex<State>,
}

#[derive(Debug)]
struct State {
    tokens: f64,
    updated: Instant,
    blocked_until: Option<Instant>,
}

impl TokenBucket {
    /// A full bucket that refills at `per_minute` tokens a minute and holds
    /// at most `burst`. Both are raised to 1 if they are 0.
    pub fn per_minute(per_minute: u32, burst: u32) -> Self {
        let capacity = f64::from(burst.max(1));
        Self {
            capacity,
            refill_per_second: f64::from(per_minute.max(1)) / 60.0,
            state: Mutex::new(State {
                tokens: capacity,
                updated: Instant::now(),
                blocked_until: None,
            }),
        }
    }

    /// A bucket for a per-minute ceiling with ten seconds' worth of burst.
    pub fn for_rate(per_minute: u32) -> Self {
        Self::per_minute(per_minute, (per_minute / 6).max(1))
    }

    /// Takes one token, waiting until one is available and the bucket is
    /// not blocked.
    pub async fn acquire(&self) {
        loop {
            match self.try_acquire() {
                Ok(()) => return,
                Err(wait) => tokio::time::sleep(wait).await,
            }
        }
    }

    /// Takes one token if one is available now.
    ///
    /// # Errors
    ///
    /// When none is available, the time to wait before trying again.
    pub fn try_acquire(&self) -> Result<(), Duration> {
        let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        let now = Instant::now();

        if let Some(until) = state.blocked_until {
            if until > now {
                return Err(until - now);
            }
            state.blocked_until = None;
        }

        let elapsed = now.saturating_duration_since(state.updated).as_secs_f64();
        state.tokens = elapsed
            .mul_add(self.refill_per_second, state.tokens)
            .min(self.capacity);
        state.updated = now;

        if state.tokens >= 1.0 {
            state.tokens -= 1.0;
            Ok(())
        } else {
            let missing = 1.0 - state.tokens;
            // At least a millisecond, so a caller never spins.
            Err(Duration::from_secs_f64(missing / self.refill_per_second)
                .max(Duration::from_millis(1)))
        }
    }

    /// Blocks the bucket for `duration`: the server said to stay away. A
    /// longer block already in force is kept. When the block ends exactly
    /// one request may go, and the bucket refills from there: the time
    /// spent blocked earns no burst.
    pub fn block_for(&self, duration: Duration) {
        let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        let until = Instant::now() + duration;
        let until = state
            .blocked_until
            .map_or(until, |current| current.max(until));
        state.blocked_until = Some(until);
        state.tokens = 1.0;
        state.updated = until;
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::time::Duration;

    use tokio::time::Instant;

    use super::TokenBucket;

    #[tokio::test(start_paused = true)]
    async fn the_burst_goes_at_once_and_then_the_rate_holds() {
        let bucket = TokenBucket::per_minute(60, 5);
        let start = Instant::now();

        for _ in 0..5 {
            bucket.acquire().await;
        }
        assert_eq!(start.elapsed(), Duration::ZERO);

        // One token a second from here on.
        for _ in 0..10 {
            bucket.acquire().await;
        }
        let elapsed = start.elapsed();
        assert!(
            (Duration::from_secs(10)..Duration::from_millis(10_100)).contains(&elapsed),
            "{elapsed:?}"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn no_minute_ever_sees_more_than_rate_plus_burst() {
        let bucket = TokenBucket::for_rate(120);
        let start = Instant::now();
        let mut grants = Vec::new();

        // Ask as fast as possible for five minutes.
        while start.elapsed() < Duration::from_mins(5) {
            bucket.acquire().await;
            grants.push(start.elapsed());
        }

        for (index, first) in grants.iter().enumerate() {
            let in_window = grants[index..]
                .iter()
                .take_while(|at| at.saturating_sub(*first) < Duration::from_mins(1))
                .count();
            // 120 a minute plus the burst of 20: still far below 240.
            assert!(in_window <= 141, "{in_window} requests within one minute");
        }
        assert!(grants.len() > 550, "{}", grants.len());
    }

    #[tokio::test(start_paused = true)]
    async fn an_idle_bucket_refills_to_its_burst_and_no_further() {
        let bucket = TokenBucket::per_minute(60, 3);
        for _ in 0..3 {
            bucket.try_acquire().unwrap();
        }
        assert!(bucket.try_acquire().is_err());

        tokio::time::advance(Duration::from_hours(1)).await;

        for _ in 0..3 {
            bucket.try_acquire().unwrap();
        }
        let wait = bucket.try_acquire().unwrap_err();
        assert!(
            (Duration::from_millis(900)..=Duration::from_secs(1)).contains(&wait),
            "{wait:?}"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_block_keeps_everyone_out_until_it_ends() {
        let bucket = Arc::new(TokenBucket::per_minute(600, 10));
        bucket.block_for(Duration::from_mins(1));
        assert_eq!(bucket.try_acquire().unwrap_err(), Duration::from_mins(1));

        // A shorter block does not shorten the one in force.
        bucket.block_for(Duration::from_secs(5));
        assert_eq!(bucket.try_acquire().unwrap_err(), Duration::from_mins(1));

        let start = Instant::now();
        let waiter = tokio::spawn({
            let bucket = Arc::clone(&bucket);
            async move {
                bucket.acquire().await;
                start.elapsed()
            }
        });
        let blocked_for = waiter.await.unwrap();
        // Exactly the block: one request may go when it ends.
        assert_eq!(blocked_for, Duration::from_mins(1));
        // The minute spent blocked earned no burst: the next request waits
        // one refill period like any other.
        assert_eq!(
            bucket.try_acquire().unwrap_err(),
            Duration::from_millis(100)
        );

        // A longer block replaces a shorter one.
        bucket.block_for(Duration::from_secs(10));
        bucket.block_for(Duration::from_secs(30));
        assert_eq!(bucket.try_acquire().unwrap_err(), Duration::from_secs(30));
    }

    #[tokio::test(start_paused = true)]
    async fn zero_rates_are_raised_to_one() {
        let bucket = TokenBucket::per_minute(0, 0);

        bucket.try_acquire().unwrap();
        assert_eq!(bucket.try_acquire().unwrap_err(), Duration::from_mins(1));
        assert!(format!("{:?}", TokenBucket::for_rate(1)).starts_with("TokenBucket"));
    }
}
