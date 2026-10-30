//! The publishing worker's decisions (ARCHITECTURE.md §10), as pure functions: what a send's outcome means for
//! the instance's state, the attempt's record, the retry delay and the spend ledger.
//!
//! ```text
//! publisher says          instance            attempt outcome     ledger    retry
//! Posted                  sending → posted     success             settle    —
//! Refused (validation)    sending → failed     validation_error    release   —
//! Refused (other)         sending → failed     failed              release   —
//! Retry, attempts left    sending → queued     rate_limited/failed keep      max(hint, backoff) + jitter
//! Retry, none left        sending → failed     rate_limited/failed release   —
//! Unknown                 sending → unknown    unknown             keep      never (an operator resolves it)
//! dry run (at claim)      queued → manual_required  dry_run        release   —
//! stopped (at claim)      queued → manual_required  (no attempt)   release   —
//! ```
//!
//! `Retry` is only ever reported when the request provably did not take effect (a refused connection, a 429, a
//! failed login before anything was posted, or an endpoint the platform documents as idempotent with the same
//! key). Anything else that is not a clear answer is `Unknown`.

use std::time::Duration;

use chrono::{DateTime, Days, TimeZone, Utc};

use crate::model::{DeliveryOutcome, InstanceStatus};

/// What one send produced, as its publisher classified it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Sent {
    /// Accepted: the platform's URL or id for the post.
    Posted { url_or_external_id: String },
    /// Refused by the platform: it provably did not take effect. `validation` marks a refusal of the content
    /// itself (too long, malformed) rather than of the credentials or the destination.
    Refused { reason: String, validation: bool },
    /// Provably not sent and safe to try again; `after` is the platform's own hint (Retry-After and the like).
    Retry { reason: String, after: Option<Duration>, rate_limited: bool },
    /// It may or may not have gone out. Recorded as `unknown` and never retried automatically.
    Unknown { reason: String },
}

/// Retry bounds: at most `max_attempts` sends per instance (the claim counts each), exponential backoff from
/// `base`, never more than `cap`, plus up to 20 % jitter.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RetryPolicy {
    pub max_attempts: u32,
    pub base: Duration,
    pub cap: Duration,
}

impl Default for RetryPolicy {
    fn default() -> Self {
        RetryPolicy { max_attempts: 5, base: Duration::from_secs(30), cap: Duration::from_secs(3600) }
    }
}

/// The longest platform hint honoured (a longer one is clamped; the attempt count still bounds the total).
pub const MAX_HINT: Duration = Duration::from_secs(24 * 3600);

/// What happens to the instance's spend reservation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Ledger {
    /// Record the cost as spent (and release the reservation it replaces).
    Settle,
    /// Give the reservation back: nothing was spent.
    Release,
    /// Keep the reservation (a retry is pending, or the outcome is unknown).
    Keep,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Decision {
    pub to: InstanceStatus,
    /// `None` when no attempt is recorded (a stop at claim time).
    pub outcome: Option<DeliveryOutcome>,
    pub retry_in: Option<Duration>,
    pub ledger: Ledger,
    pub last_error: Option<String>,
    pub url_or_external_id: Option<String>,
}

/// The database's limit on `last_error`, `response_excerpt` and the like.
pub const TEXT_LIMIT: usize = 2000;

/// Cut `s` to at most `n` characters.
pub fn clip(s: &str, n: usize) -> String {
    match s.char_indices().nth(n) {
        Some((i, _)) => s[..i].to_owned(),
        None => s.to_owned(),
    }
}

/// The backoff before attempt `attempts + 1`: `base · 2^(attempts − 1)`, at most `cap`.
pub fn backoff(policy: &RetryPolicy, attempts: u32) -> Duration {
    let exp = attempts.saturating_sub(1).min(20);
    policy.base.saturating_mul(1u32 << exp).min(policy.cap)
}

/// Decide what a send's outcome means. `attempts` counts this one; `jitter` is uniform in `[0, 1)`.
pub fn decide(sent: &Sent, attempts: u32, policy: &RetryPolicy, jitter: f64) -> Decision {
    match sent {
        Sent::Posted { url_or_external_id } => Decision {
            to: InstanceStatus::Posted,
            outcome: Some(DeliveryOutcome::Success),
            retry_in: None,
            ledger: Ledger::Settle,
            last_error: None,
            url_or_external_id: Some(clip(url_or_external_id, TEXT_LIMIT)),
        },
        Sent::Refused { reason, validation } => Decision {
            to: InstanceStatus::Failed,
            outcome: Some(if *validation { DeliveryOutcome::ValidationError } else { DeliveryOutcome::Failed }),
            retry_in: None,
            ledger: Ledger::Release,
            last_error: Some(clip(reason, TEXT_LIMIT)),
            url_or_external_id: None,
        },
        Sent::Retry { reason, after, rate_limited } => {
            let outcome = if *rate_limited { DeliveryOutcome::RateLimited } else { DeliveryOutcome::Failed };
            if attempts >= policy.max_attempts {
                return Decision {
                    to: InstanceStatus::Failed,
                    outcome: Some(outcome),
                    retry_in: None,
                    ledger: Ledger::Release,
                    last_error: Some(clip(
                        &format!("gave up after {attempts} attempts, none of which was sent: {reason}"),
                        TEXT_LIMIT,
                    )),
                    url_or_external_id: None,
                };
            }
            let hint = after.unwrap_or_default().min(MAX_HINT);
            let delay = hint.max(backoff(policy, attempts));
            let extra = delay.mul_f64(0.2 * jitter.clamp(0.0, 1.0));
            Decision {
                to: InstanceStatus::Queued,
                outcome: Some(outcome),
                retry_in: Some(delay + extra),
                ledger: Ledger::Keep,
                last_error: Some(clip(reason, TEXT_LIMIT)),
                url_or_external_id: None,
            }
        }
        Sent::Unknown { reason } => Decision {
            to: InstanceStatus::Unknown,
            outcome: Some(DeliveryOutcome::Unknown),
            retry_in: None,
            ledger: Ledger::Keep,
            last_error: Some(clip(reason, TEXT_LIMIT)),
            url_or_external_id: None,
        },
    }
}

/// A dry run at claim time: the exact request is recorded, nothing is sent, a person takes over.
pub fn dry_run(reason: &str) -> Decision {
    Decision {
        to: InstanceStatus::ManualRequired,
        outcome: Some(DeliveryOutcome::DryRun),
        retry_in: None,
        ledger: Ledger::Release,
        last_error: Some(clip(&format!("dry run, nothing was sent: {reason}"), TEXT_LIMIT)),
        url_or_external_id: None,
    }
}

/// A stop at claim time (the platform or target was switched off, no publisher exists): no attempt.
pub fn stopped(reason: &str) -> Decision {
    Decision {
        to: InstanceStatus::ManualRequired,
        outcome: None,
        retry_in: None,
        ledger: Ledger::Release,
        last_error: Some(clip(&format!("not sent: {reason}"), TEXT_LIMIT)),
        url_or_external_id: None,
    }
}

/// The next midnight (UTC) after `now`: when a target's daily post limit resets.
pub fn next_utc_midnight(now: DateTime<Utc>) -> DateTime<Utc> {
    let tomorrow = now.date_naive().checked_add_days(Days::new(1)).unwrap_or(now.date_naive());
    Utc.from_utc_datetime(&tomorrow.and_time(chrono::NaiveTime::MIN))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::instance_state::{Actor, allowed};

    fn p() -> RetryPolicy {
        RetryPolicy::default()
    }

    fn retry(after: Option<u64>, rate_limited: bool) -> Sent {
        Sent::Retry { reason: "r".into(), after: after.map(Duration::from_secs), rate_limited }
    }

    #[test]
    fn every_decision_is_a_move_the_worker_may_make() {
        let outcomes = [
            Sent::Posted { url_or_external_id: "u".into() },
            Sent::Refused { reason: "x".into(), validation: true },
            Sent::Refused { reason: "x".into(), validation: false },
            retry(None, false),
            retry(Some(5), true),
            Sent::Unknown { reason: "t".into() },
        ];
        for s in &outcomes {
            for attempts in 1..=6 {
                let d = decide(s, attempts, &p(), 0.5);
                assert!(allowed(Actor::Worker, InstanceStatus::Sending, d.to), "{s:?} -> {:?}", d.to);
            }
        }
        for d in [dry_run("r"), stopped("r")] {
            assert!(allowed(Actor::Worker, InstanceStatus::Queued, d.to));
        }
    }

    #[test]
    fn the_table() {
        let d = decide(&Sent::Posted { url_or_external_id: "https://x/1".into() }, 1, &p(), 0.0);
        assert_eq!(
            (d.to, d.outcome, d.ledger, d.url_or_external_id.as_deref()),
            (InstanceStatus::Posted, Some(DeliveryOutcome::Success), Ledger::Settle, Some("https://x/1"))
        );
        let d = decide(&Sent::Refused { reason: "too long".into(), validation: true }, 1, &p(), 0.0);
        assert_eq!(
            (d.to, d.outcome, d.ledger),
            (InstanceStatus::Failed, Some(DeliveryOutcome::ValidationError), Ledger::Release)
        );
        let d = decide(&Sent::Refused { reason: "401".into(), validation: false }, 1, &p(), 0.0);
        assert_eq!(d.outcome, Some(DeliveryOutcome::Failed));
        let d = decide(&Sent::Unknown { reason: "timeout after sending".into() }, 1, &p(), 0.0);
        assert_eq!(
            (d.to, d.outcome, d.ledger, d.retry_in),
            (InstanceStatus::Unknown, Some(DeliveryOutcome::Unknown), Ledger::Keep, None)
        );
        let d = dry_run("target not live");
        assert_eq!(
            (d.to, d.outcome, d.ledger),
            (InstanceStatus::ManualRequired, Some(DeliveryOutcome::DryRun), Ledger::Release)
        );
        let d = stopped("platform disabled");
        assert_eq!((d.to, d.outcome, d.ledger), (InstanceStatus::ManualRequired, None, Ledger::Release));
    }

    #[test]
    fn unknown_is_never_retried_whatever_the_attempt_count() {
        for attempts in 0..20 {
            let d = decide(&Sent::Unknown { reason: "?".into() }, attempts, &p(), 0.9);
            assert_eq!((d.to, d.retry_in), (InstanceStatus::Unknown, None));
        }
    }

    #[test]
    fn retries_back_off_honour_hints_and_stop_at_the_bound() {
        let d = decide(&retry(None, false), 1, &p(), 0.0);
        assert_eq!((d.to, d.retry_in, d.ledger), (InstanceStatus::Queued, Some(Duration::from_secs(30)), Ledger::Keep));
        assert_eq!(decide(&retry(None, false), 3, &p(), 0.0).retry_in, Some(Duration::from_secs(120)));
        // The platform's hint wins when it is longer; jitter adds at most 20 %.
        let d = decide(&retry(Some(600), true), 1, &p(), 0.0);
        assert_eq!((d.retry_in, d.outcome), (Some(Duration::from_secs(600)), Some(DeliveryOutcome::RateLimited)));
        let d = decide(&retry(Some(600), true), 1, &p(), 0.999_999);
        assert!(d.retry_in.expect("delay") <= Duration::from_secs(720));
        // A huge hint is clamped; the backoff is capped.
        assert_eq!(decide(&retry(Some(10 * 86_400), true), 1, &p(), 0.0).retry_in, Some(MAX_HINT));
        assert_eq!(backoff(&p(), 30), Duration::from_secs(3600));
        // The fifth attempt is the last.
        let d = decide(&retry(Some(1), true), 5, &p(), 0.0);
        assert_eq!((d.to, d.ledger, d.retry_in), (InstanceStatus::Failed, Ledger::Release, None));
        assert!(d.last_error.expect("why").starts_with("gave up after 5 attempts"));
    }

    #[test]
    fn texts_are_clipped_to_the_column_limit() {
        let long = "é".repeat(5000);
        let d = decide(&Sent::Unknown { reason: long }, 1, &p(), 0.0);
        assert_eq!(d.last_error.expect("e").chars().count(), TEXT_LIMIT);
        assert_eq!(clip("abc", 5), "abc");
    }

    #[test]
    fn the_daily_limit_resets_at_midnight_utc() {
        let now = Utc.with_ymd_and_hms(2026, 10, 31, 23, 59, 59).single().expect("t");
        assert_eq!(next_utc_midnight(now), Utc.with_ymd_and_hms(2026, 11, 1, 0, 0, 0).single().expect("t"));
    }
}
