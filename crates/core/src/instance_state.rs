//! The platform-instance state machine, the core of the at-most-once guarantee (ARCHITECTURE.md §10).
//!
//! Who may make each change:
//!
//! ```text
//! from             to                actor
//! planned          queued            operator (enqueue; the payload is validated and spend reserved first)
//! planned          manual_required   operator (enqueue on a manual platform)
//! planned          cancelled         operator
//! queued           sending           worker   (claim; committed BEFORE the external call)
//! queued           manual_required   worker   (a cap or a platform switch stops it at claim time)
//! queued           planned           operator (unqueue)
//! queued           cancelled         operator
//! sending          posted            worker
//! sending          failed            worker   (the platform refused it: it provably did not take effect)
//! sending          queued            worker   (retryable and provably not sent: refused connection, 429)
//! sending          unknown           worker   (timeout after sending, or any other doubt)
//! sending          unknown           janitor  (a `sending` row older than the claim lease)
//! failed           queued            operator (retry)
//! failed           manual_required   operator
//! failed           cancelled         operator
//! unknown          posted            operator (confirmed it went out; records the URL)
//! unknown          failed            operator (confirmed it did not)
//! manual_required  posted            operator (mark posted)
//! manual_required  planned           operator
//! manual_required  cancelled         operator
//! cancelled        planned           operator (reopen)
//! posted           —                 terminal
//! ```
//!
//! The invariants the tests pin: only the worker enters `sending`; nothing leaves `unknown` except an
//! operator's resolution to `posted` or `failed` (never an automatic retry); `posted` is terminal; the
//! janitor can do exactly one thing.

use crate::model::InstanceStatus;

/// Who asks for the change.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Actor {
    /// A person, through the admin API.
    Operator,
    /// The publishing worker.
    Worker,
    /// The worker's periodic scan for stale `sending` rows.
    Janitor,
}

impl Actor {
    /// Every actor, for the tests that walk the whole table.
    pub const ALL: &'static [Actor] = &[Actor::Operator, Actor::Worker, Actor::Janitor];
}

/// The refusal of a move the table does not allow.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("{actor:?} cannot move an instance from {from} to {to}")]
pub struct InstanceTransitionError {
    /// Who tried to make the move.
    pub actor: Actor,
    /// The status the instance was in.
    pub from: InstanceStatus,
    /// The status it was asked to move to.
    pub to: InstanceStatus,
}

/// Whether `actor` may move an instance from `from` to `to`.
pub const fn allowed(actor: Actor, from: InstanceStatus, to: InstanceStatus) -> bool {
    use Actor::*;
    use InstanceStatus::*;
    match actor {
        Operator => matches!(
            (from, to),
            (Planned, Queued | ManualRequired | Cancelled)
                | (Queued, Planned | Cancelled)
                | (Failed, Queued | ManualRequired | Cancelled)
                | (Unknown, Posted | Failed)
                | (ManualRequired, Posted | Planned | Cancelled)
                | (Cancelled, Planned)
        ),
        Worker => {
            matches!((from, to), (Queued, Sending | ManualRequired) | (Sending, Posted | Failed | Queued | Unknown))
        }
        Janitor => matches!((from, to), (Sending, Unknown)),
    }
}

/// Check one transition.
pub fn check(actor: Actor, from: InstanceStatus, to: InstanceStatus) -> Result<(), InstanceTransitionError> {
    if allowed(actor, from, to) { Ok(()) } else { Err(InstanceTransitionError { actor, from, to }) }
}

/// Unit tests for this module.
#[cfg(test)]
mod tests {
    use super::*;
    use Actor::*;
    use InstanceStatus::*;

    /// The table in the module documentation.
    const TABLE: &[(InstanceStatus, InstanceStatus, Actor)] = &[
        (Planned, Queued, Operator),
        (Planned, ManualRequired, Operator),
        (Planned, Cancelled, Operator),
        (Queued, Sending, Worker),
        (Queued, ManualRequired, Worker),
        (Queued, Planned, Operator),
        (Queued, Cancelled, Operator),
        (Sending, Posted, Worker),
        (Sending, Failed, Worker),
        (Sending, Queued, Worker),
        (Sending, Unknown, Worker),
        (Sending, Unknown, Janitor),
        (Failed, Queued, Operator),
        (Failed, ManualRequired, Operator),
        (Failed, Cancelled, Operator),
        (Unknown, Posted, Operator),
        (Unknown, Failed, Operator),
        (ManualRequired, Posted, Operator),
        (ManualRequired, Planned, Operator),
        (ManualRequired, Cancelled, Operator),
        (Cancelled, Planned, Operator),
    ];

    /// Every triple matches the documented table.
    #[test]
    fn every_triple_matches_the_documented_table() {
        for &actor in Actor::ALL {
            for &from in InstanceStatus::ALL {
                for &to in InstanceStatus::ALL {
                    let documented = TABLE.contains(&(from, to, actor));
                    assert_eq!(allowed(actor, from, to), documented, "{actor:?}: {from} -> {to}");
                }
            }
        }
    }

    /// Only the worker enters sending.
    #[test]
    fn only_the_worker_enters_sending() {
        for &actor in Actor::ALL {
            for &from in InstanceStatus::ALL {
                assert_eq!(allowed(actor, from, Sending), actor == Worker && from == Queued);
            }
        }
    }

    /// Unknown is left only by an operator resolution.
    #[test]
    fn unknown_is_left_only_by_an_operator_resolution() {
        for &actor in Actor::ALL {
            for &to in InstanceStatus::ALL {
                let ok = allowed(actor, Unknown, to);
                assert_eq!(ok, actor == Operator && matches!(to, Posted | Failed), "{actor:?}: unknown -> {to}");
            }
        }
        // In particular nothing, ever, retries an unknown send automatically.
        for &actor in Actor::ALL {
            assert!(!allowed(actor, Unknown, Queued));
        }
    }

    /// Posted is terminal.
    #[test]
    fn posted_is_terminal() {
        for &actor in Actor::ALL {
            for &to in InstanceStatus::ALL {
                assert!(!allowed(actor, Posted, to));
            }
        }
    }

    /// The janitor has exactly one move.
    #[test]
    fn the_janitor_has_exactly_one_move() {
        let moves: Vec<_> = InstanceStatus::ALL
            .iter()
            .flat_map(|&f| InstanceStatus::ALL.iter().map(move |&t| (f, t)))
            .filter(|&(f, t)| allowed(Janitor, f, t))
            .collect();
        assert_eq!(moves, vec![(Sending, Unknown)]);
    }

    /// Check reports the refused triple.
    #[test]
    fn check_reports_the_refused_triple() {
        assert_eq!(
            check(Operator, Queued, Sending),
            Err(InstanceTransitionError { actor: Operator, from: Queued, to: Sending })
        );
        assert_eq!(check(Worker, Queued, Sending), Ok(()));
    }
}
