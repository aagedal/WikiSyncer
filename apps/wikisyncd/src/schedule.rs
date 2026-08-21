//! Pure schedule calculation and restart/sleep recovery.
//!
//! Scheduled timestamps include deterministic, delay-only jitter. Keeping jitter
//! below one cadence period preserves occurrence order, while deriving it from the
//! collection and nominal timestamp makes restart calculations reproducible.

use wikisync_store::ScheduleCadence;

const SECONDS_PER_DAY: u64 = 24 * 60 * 60;
const JITTER_DOMAIN: u64 = 0x5753_4a49_5454_4552;

/// A recovery calculation for one durable collection schedule.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RecoveryDecision {
    /// The persisted occurrence that should be claimed once, if it is due.
    pub due_at: Option<u64>,
    /// The occurrence to persist after a successful claim, or the initialized
    /// occurrence when the schedule did not previously have one.
    pub next_run_at: Option<u64>,
}

/// Returns the next nominal cadence boundary strictly after `timestamp`.
///
/// Interval schedules are anchored at the Unix epoch. Daily schedules use UTC
/// days and their configured number of seconds after midnight. `None` means the
/// cadence is manual or that the next boundary cannot be represented by `u64`.
#[must_use]
pub fn next_nominal_after(cadence: ScheduleCadence, timestamp: u64) -> Option<u64> {
    match cadence {
        ScheduleCadence::Manual => None,
        ScheduleCadence::Interval(interval) => {
            let seconds = u64::from(interval.seconds());
            debug_assert!(seconds > 0);
            timestamp
                .checked_div(seconds)?
                .checked_add(1)?
                .checked_mul(seconds)
        }
        ScheduleCadence::DailyUtc(time) => {
            let seconds_after_midnight = u64::from(time.seconds_after_midnight());
            debug_assert!(seconds_after_midnight < SECONDS_PER_DAY);
            let day_start = timestamp - timestamp % SECONDS_PER_DAY;
            let today = day_start.checked_add(seconds_after_midnight)?;
            if today > timestamp {
                Some(today)
            } else {
                day_start
                    .checked_add(SECONDS_PER_DAY)?
                    .checked_add(seconds_after_midnight)
            }
        }
    }
}

/// Applies deterministic bounded jitter to a nominal occurrence.
///
/// The returned timestamp is never earlier than `nominal`. Jitter is capped at
/// one less than the cadence period even if a larger value is supplied, so two
/// consecutive nominal occurrences cannot be reordered. `None` is returned for
/// manual cadence or timestamp overflow.
#[must_use]
pub fn jittered_occurrence(
    cadence: ScheduleCadence,
    collection_id: u64,
    nominal: u64,
    jitter_seconds: u32,
) -> Option<u64> {
    let maximum = effective_jitter(cadence, jitter_seconds)?;
    nominal.checked_add(deterministic_jitter(collection_id, nominal, maximum))
}

/// Returns the earliest jittered occurrence strictly after `timestamp`.
///
/// This calculation is stable across process restarts and does not iterate over
/// every missed interval. It jumps to the small window in which jitter can affect
/// whether an occurrence is still pending.
#[must_use]
pub fn next_occurrence_after(
    cadence: ScheduleCadence,
    collection_id: u64,
    jitter_seconds: u32,
    timestamp: u64,
) -> Option<u64> {
    let maximum = effective_jitter(cadence, jitter_seconds)?;
    let lower_bound = timestamp.saturating_sub(maximum);
    let mut nominal = nominal_at_or_after(cadence, lower_bound)?;

    loop {
        let occurrence = jittered_occurrence(cadence, collection_id, nominal, jitter_seconds)?;
        if occurrence > timestamp {
            return Some(occurrence);
        }
        nominal = next_nominal_after(cadence, nominal)?;
    }
}

/// Selects at most one due run and advances past all other missed occurrences.
///
/// `persisted_next_run_at` is the compare-and-swap token used by durable storage.
/// When it is due, the caller should atomically claim that exact timestamp while
/// storing `next_run_at`. This produces one recovery run after sleep or restart
/// instead of a catch-up storm. When no occurrence was persisted, this initializes
/// the schedule without treating historical boundaries as missed work.
#[must_use]
pub fn recover(
    cadence: ScheduleCadence,
    collection_id: u64,
    jitter_seconds: u32,
    persisted_next_run_at: Option<u64>,
    now: u64,
) -> RecoveryDecision {
    if matches!(cadence, ScheduleCadence::Manual) {
        return RecoveryDecision {
            due_at: None,
            next_run_at: None,
        };
    }

    match persisted_next_run_at {
        Some(next_run_at) if next_run_at <= now => RecoveryDecision {
            due_at: Some(next_run_at),
            next_run_at: next_occurrence_after(cadence, collection_id, jitter_seconds, now),
        },
        Some(next_run_at) => RecoveryDecision {
            due_at: None,
            next_run_at: Some(next_run_at),
        },
        None => RecoveryDecision {
            due_at: None,
            next_run_at: next_occurrence_after(cadence, collection_id, jitter_seconds, now),
        },
    }
}

fn nominal_at_or_after(cadence: ScheduleCadence, timestamp: u64) -> Option<u64> {
    match cadence {
        ScheduleCadence::Manual => None,
        ScheduleCadence::Interval(interval) => {
            let seconds = u64::from(interval.seconds());
            debug_assert!(seconds > 0);
            let remainder = timestamp % seconds;
            if remainder == 0 {
                Some(timestamp)
            } else {
                timestamp.checked_add(seconds - remainder)
            }
        }
        ScheduleCadence::DailyUtc(time) => {
            let seconds_after_midnight = u64::from(time.seconds_after_midnight());
            debug_assert!(seconds_after_midnight < SECONDS_PER_DAY);
            let day_start = timestamp - timestamp % SECONDS_PER_DAY;
            let today = day_start.checked_add(seconds_after_midnight)?;
            if today >= timestamp {
                Some(today)
            } else {
                day_start
                    .checked_add(SECONDS_PER_DAY)?
                    .checked_add(seconds_after_midnight)
            }
        }
    }
}

fn effective_jitter(cadence: ScheduleCadence, configured_seconds: u32) -> Option<u64> {
    let maximum_for_cadence = match cadence {
        ScheduleCadence::Manual => return None,
        ScheduleCadence::Interval(interval) => u64::from(interval.seconds()).saturating_sub(1),
        ScheduleCadence::DailyUtc(_) => SECONDS_PER_DAY - 1,
    };
    Some(u64::from(configured_seconds).min(maximum_for_cadence))
}

fn deterministic_jitter(collection_id: u64, nominal: u64, maximum: u64) -> u64 {
    if maximum == 0 {
        return 0;
    }

    let mixed =
        splitmix64(JITTER_DOMAIN ^ splitmix64(collection_id) ^ splitmix64(nominal.rotate_left(29)));
    mixed % (maximum + 1)
}

fn splitmix64(mut value: u64) -> u64 {
    value = value.wrapping_add(0x9e37_79b9_7f4a_7c15);
    value = (value ^ (value >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    value = (value ^ (value >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    value ^ (value >> 31)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn interval(seconds: u32) -> ScheduleCadence {
        ScheduleCadence::interval(seconds).expect("valid interval")
    }

    fn daily(seconds_after_midnight: u32) -> ScheduleCadence {
        ScheduleCadence::daily_utc(seconds_after_midnight).expect("valid daily time")
    }

    #[test]
    fn manual_cadence_has_no_occurrences() {
        assert_eq!(next_nominal_after(ScheduleCadence::Manual, 0), None);
        assert_eq!(
            next_occurrence_after(ScheduleCadence::Manual, 1, 0, 0),
            None
        );
        assert_eq!(
            recover(ScheduleCadence::Manual, 1, 0, Some(10), 10),
            RecoveryDecision {
                due_at: None,
                next_run_at: None,
            }
        );
    }

    #[test]
    fn interval_boundaries_are_epoch_anchored_and_strict() {
        let cadence = interval(60);
        assert_eq!(next_nominal_after(cadence, 0), Some(60));
        assert_eq!(next_nominal_after(cadence, 59), Some(60));
        assert_eq!(next_nominal_after(cadence, 60), Some(120));
    }

    #[test]
    fn daily_boundaries_cross_utc_midnight() {
        let cadence = daily(3_600);
        assert_eq!(next_nominal_after(cadence, 0), Some(3_600));
        assert_eq!(next_nominal_after(cadence, 3_600), Some(90_000));
        assert_eq!(next_nominal_after(cadence, 86_399), Some(90_000));
    }

    #[test]
    fn jitter_is_deterministic_bounded_and_delay_only() {
        let cadence = interval(300);
        let nominal = 12_000;
        let first = jittered_occurrence(cadence, 41, nominal, 45).expect("occurrence");
        let second = jittered_occurrence(cadence, 41, nominal, 45).expect("occurrence");
        assert_eq!(first, second);
        assert!((nominal..=nominal + 45).contains(&first));

        let differs_for_a_key = (42..100).any(|collection_id| {
            jittered_occurrence(cadence, collection_id, nominal, 45) != Some(first)
        });
        assert!(differs_for_a_key);
    }

    #[test]
    fn jitter_is_capped_below_interval_to_preserve_order() {
        let cadence = interval(60);
        for nominal in [0, 60, 120, 180] {
            let occurrence =
                jittered_occurrence(cadence, 7, nominal, u32::MAX).expect("occurrence");
            assert!((nominal..nominal + 60).contains(&occurrence));
        }
    }

    #[test]
    fn next_occurrence_is_strict_and_can_use_a_pending_jitter_window() {
        let cadence = interval(300);
        let occurrence = next_occurrence_after(cadence, 99, 120, 10_000).expect("next");
        assert!(occurrence > 10_000);

        let nominal = occurrence - occurrence % 300;
        assert!(nominal <= 10_200);
        assert_eq!(
            jittered_occurrence(cadence, 99, nominal, 120),
            Some(occurrence)
        );
    }

    #[test]
    fn missing_cursor_is_initialized_without_a_due_run() {
        let decision = recover(interval(60), 5, 10, None, 1_000);
        assert_eq!(decision.due_at, None);
        assert!(
            decision
                .next_run_at
                .is_some_and(|timestamp| timestamp > 1_000)
        );
    }

    #[test]
    fn a_future_cursor_is_preserved_exactly() {
        assert_eq!(
            recover(interval(60), 5, 10, Some(1_010), 1_000),
            RecoveryDecision {
                due_at: None,
                next_run_at: Some(1_010),
            }
        );
    }

    #[test]
    fn long_gap_selects_one_due_run_and_jumps_to_the_future() {
        let cadence = interval(60);
        let now = 20 * 365 * SECONDS_PER_DAY;
        let decision = recover(cadence, 5, 20, Some(60), now);
        assert_eq!(decision.due_at, Some(60));
        assert!(
            decision
                .next_run_at
                .is_some_and(|timestamp| timestamp > now)
        );

        let after_claim = recover(cadence, 5, 20, decision.next_run_at, now);
        assert_eq!(after_claim.due_at, None);
        assert_eq!(after_claim.next_run_at, decision.next_run_at);
    }

    #[test]
    fn due_at_now_is_claimed_once() {
        let decision = recover(daily(0), 8, 0, Some(SECONDS_PER_DAY), SECONDS_PER_DAY);
        assert_eq!(decision.due_at, Some(SECONDS_PER_DAY));
        assert_eq!(decision.next_run_at, Some(2 * SECONDS_PER_DAY));
    }

    #[test]
    fn timestamp_overflow_returns_no_later_occurrence() {
        let cadence = interval(60);
        assert_eq!(next_nominal_after(cadence, u64::MAX), None);
        assert_eq!(next_occurrence_after(cadence, 1, 10, u64::MAX), None);

        let decision = recover(cadence, 1, 10, Some(u64::MAX - 1), u64::MAX);
        assert_eq!(decision.due_at, Some(u64::MAX - 1));
        assert_eq!(decision.next_run_at, None);
    }
}
