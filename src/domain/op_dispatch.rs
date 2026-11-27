//! Replay the durable results of dispatch, cleanup, and completion-driven release.
//! Launching agents, inspecting worktrees, and deciding which releases belong to a
//! completion transaction are the caller's responsibility.

use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};
use uuid::Uuid;

use super::op::{ApplyError, ApplyOutcome, PrerequisitePolicy, StatusConflict};
use super::{CleanupOutcome, Dispatch, DomainError, DomainState, HumanStatus, Task, TaskEventKind};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "kind", deny_unknown_fields)]
pub enum DispatchIntent {
    RecordDispatch {
        task: Uuid,
        expected: DispatchGuard,
        /// The successful launch record, including its host and launch time.
        record: Box<Dispatch>,
        /// None records the agent without changing the human status.
        start: Option<DispatchStart>,
    },
    RecordCleanup {
        task: Uuid,
        cleanup: CleanupMarker,
        keep_undo: bool,
    },
    CompleteAfterCleanup {
        targets: Vec<CleanupCompletion>,
        /// The marked board path keeps a batch undo even for one task.
        marked: bool,
    },
    ReleaseStart {
        task: Uuid,
        prerequisite: Uuid,
    },
    ResolveReleasedLaunch {
        task: Uuid,
        prerequisite: Uuid,
        expected_revision: Uuid,
        outcome: ReleasedLaunchOutcome,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "kind", deny_unknown_fields)]
pub enum DispatchGuard {
    /// `None` explicitly expects a task with no dispatch.
    Dispatch {
        expected: Option<Box<Dispatch>>,
    },
    Revision {
        expected: Uuid,
    },
}

impl DispatchGuard {
    fn matches(&self, task: &Task) -> bool {
        match self {
            Self::Dispatch { expected } => task.dispatch.as_ref() == expected.as_deref(),
            Self::Revision { expected } => task.revision == *expected,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DispatchStart {
    pub from: HumanStatus,
    pub prerequisite_policy: PrerequisitePolicy,
    /// Only a single board launch gets status-only undo.
    pub undo: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CleanupMarker {
    pub expected_dispatch: Box<Dispatch>,
    pub outcome: CleanupOutcome,
}

impl CleanupMarker {
    fn matches(&self, task: &Task) -> bool {
        task.dispatch
            .as_ref()
            .is_some_and(|record| !record.cleaned && record == self.expected_dispatch.as_ref())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CleanupCompletion {
    pub task: Uuid,
    pub from: HumanStatus,
    pub cleanup: Option<CleanupMarker>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "kind", deny_unknown_fields)]
pub enum ReleasedLaunchOutcome {
    Succeeded { record: Box<Dispatch> },
    Failed,
}

impl DispatchIntent {
    /// Call within the envelope's `with_mutation_inputs` scope for deterministic replay.
    pub(super) fn apply(&self, state: &mut DomainState) -> Result<ApplyOutcome, ApplyError> {
        let mut result = ApplyOutcome::default();
        result.changed = match self {
            Self::RecordDispatch {
                task,
                expected,
                record,
                start,
            } => {
                let target = live(state, *task)?;
                if !expected.matches(target) {
                    return Err(ApplyError::StaleDispatch(*task));
                }
                let previous = target.status;
                if let Some(start) = start {
                    if start.prerequisite_policy == PrerequisitePolicy::Check {
                        check_waiting(state, target)?;
                    }
                    result.status_conflicts.extend(conflict(target, start.from));
                }
                state.record_dispatch_with_status(
                    *task,
                    record.as_ref().clone(),
                    start.is_some(),
                )?;
                if start.is_some_and(|start| start.undo) && previous != HumanStatus::Started {
                    state.push_start_undo(*task, previous)?;
                }
                true
            }
            Self::RecordCleanup {
                task,
                cleanup,
                keep_undo,
            } => {
                if !cleanup.matches(live(state, *task)?) {
                    false
                } else {
                    if *keep_undo {
                        state.record_dispatch_cleaned_keeping_undo(*task, cleanup.outcome)?;
                    } else {
                        state.record_dispatch_cleaned(*task, cleanup.outcome)?;
                    }
                    true
                }
            }
            Self::CompleteAfterCleanup { targets, marked } => {
                return complete_after_cleanup(state, targets, *marked);
            }
            Self::ReleaseStart { task, prerequisite } => {
                let target = live(state, *task)?;
                let prerequisite = live(state, *prerequisite)?;
                let number = number_of(prerequisite)?;
                if target.status != HumanStatus::Ready || target.archived || target.is_notice() {
                    return Err(ApplyError::InvalidIntent(
                        "release target must be a live, unarchived ready task",
                    ));
                }
                if prerequisite.id == *task
                    || prerequisite.is_notice()
                    || !target.after.contains(&number)
                {
                    return Err(ApplyError::InvalidIntent(
                        "release prerequisite is not linked to the target",
                    ));
                }
                check_waiting(state, target)?;
                if prerequisite.status != HumanStatus::Done {
                    return Err(ApplyError::InvalidIntent(
                        "release prerequisite must be done",
                    ));
                }
                state.start_released(*task, number)?;
                true
            }
            Self::ResolveReleasedLaunch {
                task,
                prerequisite,
                expected_revision,
                outcome,
            } => {
                let target = live(state, *task)?;
                if target.revision != *expected_revision
                    || target.status != HumanStatus::Started
                    || target.dispatch.is_some()
                    || target.archived
                    || target.is_notice()
                {
                    return Err(ApplyError::StaleDispatch(*task));
                }
                // A later reopen of the prerequisite does not undo the release. Only
                // the target's revision and release provenance govern this outcome.
                let source = state
                    .get(*prerequisite)
                    .ok_or(DomainError::UnknownId(*prerequisite))?;
                let number = number_of(source)?;
                // The release must be the task's latest status change. An unrelated edit
                // since then (a title, a step) keeps the launch attributed to it.
                let last_status = target.history.iter().rev().find(|event| {
                    matches!(
                        event.kind,
                        TaskEventKind::StatusSet
                            | TaskEventKind::Completed
                            | TaskEventKind::Reopened
                    )
                });
                let released_by_source = last_status.is_some_and(|event| {
                    event.kind == TaskEventKind::StatusSet
                        && event.detail.as_ref().is_some_and(|detail| {
                            detail.from == Some(HumanStatus::Ready)
                                && detail.to == Some(HumanStatus::Started)
                                && detail.after == Some(number)
                        })
                });
                if !released_by_source {
                    return Err(ApplyError::InvalidIntent(
                        "launch result must refer to the target's release start",
                    ));
                }
                match outcome {
                    ReleasedLaunchOutcome::Succeeded { record } => {
                        state.record_dispatch_with_status(*task, record.as_ref().clone(), false)?;
                        state.note_released_dispatch(*task, number, *expected_revision)?;
                    }
                    ReleasedLaunchOutcome::Failed => {
                        state.return_released_to_ready(*task, *expected_revision)?;
                    }
                }
                true
            }
        };
        Ok(result)
    }
}

fn live(state: &DomainState, id: Uuid) -> Result<&Task, DomainError> {
    let task = state.get(id).ok_or(DomainError::UnknownId(id))?;
    if task.soft_deleted {
        return Err(DomainError::SoftDeleted(id));
    }
    Ok(task)
}

fn number_of(task: &Task) -> Result<u64, ApplyError> {
    task.number
        .ok_or(ApplyError::UnnumberedPrerequisite(task.id))
}

fn check_waiting(state: &DomainState, task: &Task) -> Result<(), ApplyError> {
    let prerequisites = state.waiting_on(task);
    if !prerequisites.is_empty() {
        return Err(ApplyError::WaitingOn {
            task: task.id,
            prerequisites,
        });
    }
    Ok(())
}

fn conflict(task: &Task, expected: HumanStatus) -> Option<StatusConflict> {
    (task.status != expected).then_some(StatusConflict {
        task: task.id,
        expected,
        actual: task.status,
    })
}

fn complete_after_cleanup(
    state: &mut DomainState,
    targets: &[CleanupCompletion],
    marked: bool,
) -> Result<ApplyOutcome, ApplyError> {
    if !marked && targets.len() != 1 {
        return Err(ApplyError::InvalidIntent(
            "single CompleteAfterCleanup needs one task",
        ));
    }
    let mut seen = BTreeSet::new();
    let mut ids = Vec::new();
    let mut markers = Vec::new();
    let mut status_conflicts = Vec::new();
    for completion in targets.iter().filter(|entry| seen.insert(entry.task)) {
        let task = live(state, completion.task)?;
        status_conflicts.extend(conflict(task, completion.from));
        if task.status != HumanStatus::Done {
            ids.push(task.id);
        }
        if let Some(marker) = completion
            .cleanup
            .as_ref()
            .filter(|marker| marker.matches(task))
        {
            markers.push((task.id, marker.outcome));
        }
    }
    let changed = !ids.is_empty() || !markers.is_empty();
    if changed {
        let mut staged = state.clone();
        for (id, outcome) in markers {
            staged.record_dispatch_cleaned(id, outcome)?;
        }
        if marked {
            staged.complete_batch_after_cleanup(&ids)?;
        } else if let [id] = ids.as_slice() {
            staged.complete_after_cleanup(*id)?;
        }
        *state = staged;
    }
    Ok(ApplyOutcome {
        changed,
        status_conflicts,
    })
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, UNIX_EPOCH};

    use super::*;
    use crate::domain::events::with_mutation_inputs;
    use crate::domain::{AgentHost, ProvenanceOrigin, TaskScope};

    fn task(state: &mut DomainState, title: &str) -> Uuid {
        state
            .create(
                title,
                None,
                TaskScope::Global,
                ProvenanceOrigin::Manual,
                None,
            )
            .unwrap()
    }

    fn dispatch(session: &str) -> Dispatch {
        Dispatch {
            argv: vec!["agent".into()],
            worktree: "/work/tree".into(),
            branch: "task-branch".into(),
            base: None,
            base_ref: None,
            base_commit: None,
            base_remote: None,
            host: AgentHost::Herdr,
            session: session.into(),
            at: UNIX_EPOCH + Duration::from_secs(42),
            cleaned: false,
        }
    }

    #[test]
    fn parses_guarded_dispatch_and_undo_restores_only_status() {
        let mut state = DomainState::new();
        let id = task(&mut state, "launch");
        let intent = DispatchIntent::RecordDispatch {
            task: id,
            expected: DispatchGuard::Dispatch { expected: None },
            record: Box::new(dispatch("first")),
            start: Some(DispatchStart {
                from: HumanStatus::Open,
                prerequisite_policy: PrerequisitePolicy::Check,
                undo: true,
            }),
        };
        let json = serde_json::to_string(&intent).unwrap();
        let replay: DispatchIntent = serde_json::from_str(&json).unwrap();
        assert_eq!(replay, intent);
        let at = UNIX_EPOCH + Duration::from_secs(42);
        let mut second = state.clone();
        for (state, intent) in [(&mut state, &intent), (&mut second, &replay)] {
            assert!(
                with_mutation_inputs(at, Uuid::from_u128(1), "agent", || { intent.apply(state) })
                    .unwrap()
                    .changed
            );
        }
        assert_eq!(
            serde_json::to_value(&state).unwrap(),
            serde_json::to_value(&second).unwrap()
        );
        assert_eq!(replay.apply(&mut state), Err(ApplyError::StaleDispatch(id)));
        state.undo().unwrap();
        let task = state.get(id).unwrap();
        assert_eq!(task.status, HumanStatus::Open);
        assert_eq!(task.dispatch.as_ref(), Some(&dispatch("first")));
    }

    #[test]
    fn marked_completion_skips_stale_cleanup_and_keeps_current_cleanup_on_undo() {
        let mut state = DomainState::new();
        let first = task(&mut state, "first");
        let second = task(&mut state, "second");
        state
            .record_dispatch(first, dispatch("relaunched"))
            .unwrap();
        state.record_dispatch(second, dispatch("current")).unwrap();
        let mut intent = DispatchIntent::CompleteAfterCleanup {
            marked: true,
            targets: vec![
                CleanupCompletion {
                    task: first,
                    from: HumanStatus::Started,
                    cleanup: Some(CleanupMarker {
                        expected_dispatch: Box::new(dispatch("old")),
                        outcome: CleanupOutcome::Missing,
                    }),
                },
                CleanupCompletion {
                    task: second,
                    from: HumanStatus::Started,
                    cleanup: Some(CleanupMarker {
                        expected_dispatch: Box::new(dispatch("current")),
                        outcome: CleanupOutcome::Removed,
                    }),
                },
                CleanupCompletion {
                    task: Uuid::nil(),
                    from: HumanStatus::Started,
                    cleanup: None,
                },
            ],
        };
        let before = serde_json::to_value(&state).unwrap();
        assert!(intent.apply(&mut state).is_err());
        assert_eq!(before, serde_json::to_value(&state).unwrap());
        if let DispatchIntent::CompleteAfterCleanup { targets, .. } = &mut intent {
            targets.pop();
        }
        assert!(intent.apply(&mut state).unwrap().changed);
        assert!(!state.get(first).unwrap().dispatch.as_ref().unwrap().cleaned);
        assert!(
            state
                .get(second)
                .unwrap()
                .dispatch
                .as_ref()
                .unwrap()
                .cleaned
        );
        assert_eq!(state.completed_ids(), [first, second]);
        state.undo().unwrap();
        for id in [first, second] {
            assert_eq!(state.get(id).unwrap().status, HumanStatus::Started);
        }
        assert!(
            state
                .get(second)
                .unwrap()
                .dispatch
                .as_ref()
                .unwrap()
                .cleaned
        );
    }

    #[test]
    fn standalone_cleanup_retargets_undo_only_when_requested() {
        let mut original = DomainState::new();
        let id = task(&mut original, "completed");
        original.record_dispatch(id, dispatch("current")).unwrap();
        original.complete(id).unwrap();
        for keep_undo in [true, false] {
            let mut state = original.clone();
            let intent = DispatchIntent::RecordCleanup {
                task: id,
                cleanup: CleanupMarker {
                    expected_dispatch: Box::new(dispatch("current")),
                    outcome: CleanupOutcome::BranchKept,
                },
                keep_undo,
            };
            assert!(intent.apply(&mut state).unwrap().changed);
            assert!(!intent.apply(&mut state).unwrap().changed);
            assert_eq!(state.top_undo_is_live(), keep_undo);
        }
    }

    #[test]
    fn release_outcomes_are_guarded_and_maintain_completion_undo() {
        let mut original = DomainState::new();
        let prerequisite = task(&mut original, "prerequisite");
        let id = task(&mut original, "waiting");
        original.assign_numbers_for_persistence();
        let number = original.get(prerequisite).unwrap().number.unwrap();
        original.set_after(id, &[number]).unwrap();
        original.set_status(id, HumanStatus::Ready).unwrap();
        let start = DispatchIntent::ReleaseStart {
            task: id,
            prerequisite,
        };
        assert_eq!(
            start.apply(&mut original),
            Err(ApplyError::WaitingOn {
                task: id,
                prerequisites: vec![number],
            })
        );
        original.complete(prerequisite).unwrap();
        assert!(start.apply(&mut original).unwrap().changed);
        let expected_revision = original.get(id).unwrap().revision;
        for outcome in [
            ReleasedLaunchOutcome::Succeeded {
                record: Box::new(dispatch("released")),
            },
            ReleasedLaunchOutcome::Failed,
        ] {
            let mut state = original.clone();
            let success = matches!(&outcome, ReleasedLaunchOutcome::Succeeded { .. });
            let resolve = DispatchIntent::ResolveReleasedLaunch {
                task: id,
                prerequisite,
                expected_revision,
                outcome,
            };
            assert!(resolve.apply(&mut state).unwrap().changed);
            assert_eq!(
                resolve.apply(&mut state),
                Err(ApplyError::StaleDispatch(id))
            );
            let task = state.get(id).unwrap();
            if success {
                assert_eq!(task.status, HumanStatus::Started);
                let detail = task.history.last().unwrap().detail.as_ref().unwrap();
                assert_eq!(detail.after, Some(number));
            } else {
                assert_eq!(task.status, HumanStatus::Ready);
            }
            assert!(state.top_undo_is_live());
            state.undo().unwrap();
            assert_eq!(state.get(prerequisite).unwrap().status, HumanStatus::Open);
            let task = state.get(id).unwrap();
            assert_eq!(task.status, HumanStatus::Ready);
            assert_eq!(task.dispatch.is_some(), success);
        }
    }

    /// A write between the release's save and its launch, such as a title edit, does not
    /// stop the outcome landing: a failed launch still goes back to ready.
    #[test]
    fn a_release_outcome_survives_an_unrelated_edit_since_the_release() {
        let mut state = DomainState::new();
        let prerequisite = task(&mut state, "prerequisite");
        let id = task(&mut state, "waiting");
        state.assign_numbers_for_persistence();
        let number = state.get(prerequisite).unwrap().number.unwrap();
        state.set_after(id, &[number]).unwrap();
        state.set_status(id, HumanStatus::Ready).unwrap();
        state.complete(prerequisite).unwrap();
        let start = DispatchIntent::ReleaseStart {
            task: id,
            prerequisite,
        };
        assert!(start.apply(&mut state).unwrap().changed);
        state.add_step(id, "added elsewhere").unwrap();
        let expected_revision = state.get(id).unwrap().revision;
        for outcome in [
            ReleasedLaunchOutcome::Failed,
            ReleasedLaunchOutcome::Succeeded {
                record: Box::new(dispatch("released")),
            },
        ] {
            let mut state = state.clone();
            let success = matches!(&outcome, ReleasedLaunchOutcome::Succeeded { .. });
            let resolve = DispatchIntent::ResolveReleasedLaunch {
                task: id,
                prerequisite,
                expected_revision,
                outcome,
            };
            assert!(resolve.apply(&mut state).unwrap().changed);
            let task = state.get(id).unwrap();
            let expected = if success {
                HumanStatus::Started
            } else {
                HumanStatus::Ready
            };
            assert_eq!(task.status, expected);
            if success {
                let detail = task.history.last().unwrap().detail.as_ref().unwrap();
                assert_eq!(detail.after, Some(number));
            }
        }
    }
}
