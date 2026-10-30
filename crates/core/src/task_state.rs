//! The task state machine: which status changes the admin API accepts.
//!
//! ```text
//! from \ to     planned  ready  in_progress  done  blocked
//! planned          -       ✓        ✓         ✗      ✓
//! ready            ✓       -        ✓         ✗      ✓
//! in_progress      ✗       ✓        -         ✓      ✓
//! done             ✗       ✗        ✓ (reopen) -     ✗
//! blocked          ✓       ✓        ✓         ✗      -
//! ```
//!
//! Also: `blocked` needs a reason (the database refuses a blocked row without one), and a task cannot be
//! started or finished while a task it depends on is unfinished. The seed import sets statuses directly;
//! it is not a transition.

use crate::model::TaskStatus;

/// What a transition needs to know beyond the two statuses.
#[derive(Debug, Clone, Copy, Default)]
pub struct TaskContext<'a> {
    /// Tasks this one depends on that are not `done`.
    pub unfinished_dependencies: usize,
    /// The reason given with a move to `blocked`.
    pub blocked_reason: Option<&'a str>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum TaskTransitionError {
    #[error("a task cannot move from {from} to {to}")]
    NotAllowed { from: TaskStatus, to: TaskStatus },
    #[error("a blocked task needs a reason (1 to 1000 characters)")]
    BlockedNeedsReason,
    #[error("{0} task(s) this one depends on are not done")]
    DependenciesUnfinished(usize),
}

/// Whether the edge `from -> to` exists in the table above (ignoring the context).
pub const fn edge_exists(from: TaskStatus, to: TaskStatus) -> bool {
    use TaskStatus::*;
    matches!(
        (from, to),
        (Planned, Ready | InProgress | Blocked)
            | (Ready, Planned | InProgress | Blocked)
            | (InProgress, Ready | Done | Blocked)
            | (Done, InProgress)
            | (Blocked, Planned | Ready | InProgress)
    )
}

/// Check one transition.
pub fn check(from: TaskStatus, to: TaskStatus, ctx: TaskContext<'_>) -> Result<(), TaskTransitionError> {
    if !edge_exists(from, to) {
        return Err(TaskTransitionError::NotAllowed { from, to });
    }
    if to == TaskStatus::Blocked {
        let ok = ctx.blocked_reason.is_some_and(|r| {
            let n = r.trim().chars().count();
            (1..=1000).contains(&n)
        });
        if !ok {
            return Err(TaskTransitionError::BlockedNeedsReason);
        }
    }
    if matches!(to, TaskStatus::InProgress | TaskStatus::Done) && ctx.unfinished_dependencies > 0 {
        return Err(TaskTransitionError::DependenciesUnfinished(ctx.unfinished_dependencies));
    }
    Ok(())
}

/// Whether the dependency edges `(task, depends_on)` contain a cycle (a task may not, directly or indirectly,
/// depend on itself). Iterative depth-first search; `O(V + E)`.
pub fn has_dependency_cycle<K: Ord + Clone>(edges: &[(K, K)]) -> bool {
    use std::collections::BTreeMap;
    let mut adj: BTreeMap<&K, Vec<&K>> = BTreeMap::new();
    for (a, b) in edges {
        adj.entry(a).or_default().push(b);
        adj.entry(b).or_default();
    }
    // 0 = unvisited, 1 = on the current path, 2 = done.
    let mut state: BTreeMap<&K, u8> = adj.keys().map(|k| (*k, 0)).collect();
    for start in adj.keys() {
        if state[start] != 0 {
            continue;
        }
        let mut stack: Vec<(&K, usize)> = vec![(start, 0)];
        state.insert(start, 1);
        while let Some((node, next)) = stack.pop() {
            let children = &adj[node];
            if next < children.len() {
                stack.push((node, next + 1));
                let child = children[next];
                match state[child] {
                    1 => return true,
                    0 => {
                        state.insert(child, 1);
                        stack.push((child, 0));
                    }
                    _ => {}
                }
            } else {
                state.insert(node, 2);
            }
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use TaskStatus::*;

    #[test]
    fn dependency_cycles_are_found() {
        assert!(!has_dependency_cycle::<i32>(&[]));
        assert!(!has_dependency_cycle(&[(1, 2), (2, 3), (1, 3)]));
        assert!(has_dependency_cycle(&[(1, 2), (2, 3), (3, 1)]));
        assert!(has_dependency_cycle(&[(1, 1)]));
        assert!(has_dependency_cycle(&[(5, 6), (1, 2), (2, 1)]));
    }

    /// The table in the module documentation, row by row.
    const TABLE: &[(TaskStatus, &[TaskStatus])] = &[
        (Planned, &[Ready, InProgress, Blocked]),
        (Ready, &[Planned, InProgress, Blocked]),
        (InProgress, &[Ready, Done, Blocked]),
        (Done, &[InProgress]),
        (Blocked, &[Planned, Ready, InProgress]),
    ];

    #[test]
    fn every_pair_matches_the_documented_table() {
        assert_eq!(TABLE.len(), TaskStatus::ALL.len());
        for &(from, allowed) in TABLE {
            for &to in TaskStatus::ALL {
                assert_eq!(edge_exists(from, to), allowed.contains(&to), "{from} -> {to}");
            }
        }
    }

    #[test]
    fn no_status_moves_to_itself() {
        for &s in TaskStatus::ALL {
            assert!(!edge_exists(s, s));
        }
    }

    #[test]
    fn blocking_needs_a_non_blank_bounded_reason() {
        let no = TaskContext::default();
        assert_eq!(check(Ready, Blocked, no), Err(TaskTransitionError::BlockedNeedsReason));
        let blank = TaskContext { blocked_reason: Some("   "), ..no };
        assert_eq!(check(Ready, Blocked, blank), Err(TaskTransitionError::BlockedNeedsReason));
        let long = "x".repeat(1001);
        let too_long = TaskContext { blocked_reason: Some(&long), ..no };
        assert_eq!(check(Ready, Blocked, too_long), Err(TaskTransitionError::BlockedNeedsReason));
        let ok = TaskContext { blocked_reason: Some("waiting for the entity"), ..no };
        assert_eq!(check(Ready, Blocked, ok), Ok(()));
    }

    #[test]
    fn unfinished_dependencies_stop_starting_and_finishing_only() {
        let deps = TaskContext { unfinished_dependencies: 2, ..TaskContext::default() };
        assert_eq!(check(Ready, InProgress, deps), Err(TaskTransitionError::DependenciesUnfinished(2)));
        assert_eq!(check(InProgress, Done, deps), Err(TaskTransitionError::DependenciesUnfinished(2)));
        assert_eq!(check(Planned, Ready, deps), Ok(()));
        assert_eq!(check(InProgress, Ready, deps), Ok(()));
    }

    #[test]
    fn a_missing_edge_is_reported_before_the_context() {
        assert_eq!(
            check(Planned, Done, TaskContext::default()),
            Err(TaskTransitionError::NotAllowed { from: Planned, to: Done })
        );
    }
}
