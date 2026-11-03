use futures_util::future::join_all;
use leaky_bucket::RateLimiter;
use std::sync::Arc;
use std::{error::Error, fmt::Display};

use crate::{
    tiers::{Tier, TierRate},
    Consumer, State,
};

#[derive(Debug)]
pub enum LimiterError {
    PortDeleted,
    InvalidTier,
}
impl Display for LimiterError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            LimiterError::PortDeleted => f.write_str("Port was deleted"),
            LimiterError::InvalidTier => f.write_str("Tier is invalid"),
        }
    }
}
impl Error for LimiterError {}

async fn has_limiter(state: &State, consumer: &Consumer) -> bool {
    let rate_limiter_map = state.limiter.read().await;
    rate_limiter_map.get(&consumer.key).is_some()
}

/// Build the rate limiter for one tier rate.
///
/// Extracted so the shipped configuration is the thing under test — a test that
/// rebuilds the builder itself would pin a copy that can silently drift.
///
/// `max` is set explicitly. Left unset, leaky-bucket defaults it to
/// `10 * max(refill, initial)` — unreachable while the 1.0.1 refill bug meant
/// the bucket never accumulated, but live now that it does. Ten intervals of
/// banked budget dischargeable at once is not a burst we can size: the backend
/// ceiling is still unmeasured (plans/ogmios-throughput-profiling.md), the
/// bucket is shared across all of a consumer's connections, and tier 3 permits
/// 450 of them. Two intervals allows a short catch-up after idle without
/// unbounded accumulation.
pub(crate) fn build_rate_limiter(rate: &TierRate) -> RateLimiter {
    RateLimiter::builder()
        .initial(rate.limit)
        .interval(rate.interval)
        .refill(rate.limit)
        .max(rate.limit * 2)
        .build()
}

async fn add_limiter(state: &State, consumer: &Consumer, tier: &Tier) {
    let rates = tier
        .rates
        .iter()
        .map(|r| Arc::new(build_rate_limiter(r)))
        .collect();

    state
        .limiter
        .write()
        .await
        .insert(consumer.key.clone(), rates);
}

pub async fn limiter(state: Arc<State>, consumer: &Consumer) -> Result<(), LimiterError> {
    if !has_limiter(&state, consumer).await {
        let consumers = state.consumers.read().await.clone();
        let refreshed_consumer = match consumers.get(&consumer.key) {
            Some(consumer) => consumer,
            None => return Err(LimiterError::PortDeleted),
        };
        let tiers = state.tiers.read().await.clone();
        let tier = match tiers.get(&refreshed_consumer.tier) {
            Some(tier) => tier,
            None => return Err(LimiterError::InvalidTier),
        };
        add_limiter(&state, refreshed_consumer, tier).await;
    }

    let rate_limiter_map = state.limiter.read().await.clone();
    let rates = rate_limiter_map.get(&consumer.key).unwrap();

    join_all(rates.iter().map(|r| async { r.acquire_one().await })).await;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;
    use tokio::time::{advance, timeout, Instant};

    /// Long enough that it never fires under correct behaviour; short enough
    /// that a limiter waiting on a timer the runtime cannot see fails by name
    /// instead of hanging CI forever. Costs nothing under paused time.
    const HANG: Duration = Duration::from_secs(3600);

    fn rate(limit: usize, interval_secs: u64) -> TierRate {
        TierRate {
            limit,
            interval: Duration::from_secs(interval_secs),
        }
    }

    /// Virtual time is an exact oracle: an acquire that did not block consumes
    /// exactly `Duration::ZERO`, one that blocked consumes exactly the time to
    /// the next deadline. So every assertion here is a `Duration` equality --
    /// no sleeps, no tolerances, no flake.
    async fn elapsed_acquiring(limiter: &RateLimiter) -> Duration {
        let started = Instant::now();
        timeout(HANG, limiter.acquire_one())
            .await
            .expect("acquire hung: virtual clock did not advance");
        started.elapsed()
    }

    async fn assert_all_instant(limiter: &RateLimiter, n: usize, what: &str) {
        for i in 0..n {
            assert_eq!(
                elapsed_acquiring(limiter).await,
                Duration::ZERO,
                "{what}: acquire {} of {n} blocked",
                i + 1
            );
        }
    }

    /// REGRESSION GATE. leaky-bucket 1.0.1 has two refill paths and only one
    /// works: the blocked path adds `refill` tokens, but the inline path taken
    /// by an acquire that finds tokens still available credits
    /// `elapsed_intervals` -- a COUNT -- instead of `elapsed_intervals * refill`.
    /// So the bucket does not refill while it is serving. Fails on 1.0.1.
    #[tokio::test(start_paused = true)]
    async fn refill_credits_a_full_limit_not_one_token_per_interval() {
        let limiter = build_rate_limiter(&rate(10, 60));

        assert_all_instant(&limiter, 10, "initial grant").await;
        advance(Duration::from_secs(60)).await;
        assert_all_instant(&limiter, 10, "after one full interval").await;
    }

    /// REGRESSION GATE, stated as the product promise: a client inside its tier
    /// is not throttled. 30 msg/min against a 60/min budget, for five
    /// intervals. Fails on 1.0.1, where the initial grant runs out after two
    /// intervals and never meaningfully refills.
    #[tokio::test(start_paused = true)]
    async fn sustained_rate_below_refill_never_blocks() {
        let limiter = build_rate_limiter(&rate(60, 60));

        for i in 0..150 {
            assert_eq!(
                elapsed_acquiring(&limiter).await,
                Duration::ZERO,
                "a client at half its tier rate was throttled at acquire {}",
                i + 1
            );
            advance(Duration::from_secs(2)).await;
        }
    }

    /// The stall is intentional above the tier ceiling. Equality on 60s pins
    /// the boundary alignment, which is what production exhibited.
    #[tokio::test(start_paused = true)]
    async fn draining_the_budget_blocks_until_the_next_interval_boundary() {
        let limiter = build_rate_limiter(&rate(10, 60));

        assert_all_instant(&limiter, 10, "initial grant").await;
        assert_eq!(
            elapsed_acquiring(&limiter).await,
            Duration::from_secs(60),
            "expected a wait to the next interval boundary"
        );
    }

    /// Pins the explicit `.max()`. The crate defaults `max` to
    /// `10 * max(refill, initial)`, which is unreachable under the 1.0.1 bug
    /// and becomes live once refill works -- an idle port could otherwise bank
    /// ten intervals and discharge them at once.
    ///
    /// The numbers discriminate all three worlds after three idle intervals:
    /// 1.0.1 credits ~3, 1.1.2 uncapped credits 30, 1.1.2 capped credits 20.
    #[tokio::test(start_paused = true)]
    async fn idle_does_not_bank_more_than_the_burst_cap() {
        let limiter = build_rate_limiter(&rate(10, 60));

        assert_all_instant(&limiter, 10, "initial grant").await;
        advance(Duration::from_secs(180)).await;

        assert_all_instant(&limiter, 20, "banked burst").await;
        assert!(
            elapsed_acquiring(&limiter).await > Duration::ZERO,
            "burst was not capped at 2x the limit"
        );
    }

    /// Guards `initial`, and pins the `initial <= max` corner. Note this is
    /// what a consumer sees on a *fresh* limiter -- `state.limiter` is keyed on
    /// `consumer.key` and survives reconnects, so it is only reached after a
    /// tier reload or a proxy restart.
    #[tokio::test(start_paused = true)]
    async fn a_fresh_limiter_grants_exactly_one_interval_immediately() {
        let limiter = build_rate_limiter(&rate(10, 60));

        assert_all_instant(&limiter, 10, "initial grant").await;
        assert!(
            elapsed_acquiring(&limiter).await > Duration::ZERO,
            "granted more than `initial` on a fresh limiter"
        );
    }
}
