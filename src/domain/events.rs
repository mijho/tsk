//! Provenance origin and append-only task event history.

use std::time::SystemTime;

use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// How a task was created. Allowed set includes at least these three.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProvenanceOrigin {
    Manual,
    Capture,
    Selection,
}

/// Kind of domain mutation recorded on a task's event history.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskEventKind {
    Created,
    Edited,
    StatusSet,
    Completed,
    Reopened,
    SoftDeleted,
    Restored,
    Archived,
    Unarchived,
    StepAdded,
    StepChecked,
    StepUnchecked,
    StepRenamed,
    StepRemoved,
    Assigned,
    BaseSet,
    Dispatched,
    Cleaned,
    BlockEdited,
    Replied,
    ReplyEdited,
    ReplyDeleted,
    ReviewEdited,
    CheckSet,
    /// Who the open block or review round waits on changed, with nothing else.
    OnSet,
}

/// One append-only history record on a task: what happened, when, who did it, and what
/// changed. Events from a v9 or older store carry no `by` and no `detail`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TaskEvent {
    pub kind: TaskEventKind,
    #[serde(with = "super::time_serde")]
    pub at: SystemTime,
    /// `you`, or the agent profile named by `TSK_AGENT` that made the change.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub by: Option<String>,
    /// What changed, for the kinds that carry it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<EventDetail>,
}

impl TaskEvent {
    /// An event with no author or detail, as an older store recorded it.
    pub fn new(kind: TaskEventKind, at: SystemTime) -> Self {
        Self {
            kind,
            at,
            by: None,
            detail: None,
        }
    }
}

/// The task field an `edited` event changed. Only which field, never the text.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EditedField {
    Title,
    Notes,
    Thread,
    Project,
    After,
    Due,
}

/// What a cleanup did with the dispatch's worktree and branch.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CleanupOutcome {
    /// The worktree and its merged branch were removed.
    Removed,
    /// The worktree was removed and the branch kept.
    BranchKept,
    /// The worktree was already gone; the record converged to cleaned.
    Missing,
}

/// Per-kind detail of one event. Every field is optional, so a kind fills only its own.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EventDetail {
    /// Status before and after a status change.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub from: Option<super::HumanStatus>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub to: Option<super::HumanStatus>,
    /// The assignee an `assigned` event set; absent on an `assigned` event means unassigned.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub assignee: Option<String>,
    /// The base a `base_set` event set (absent: cleared to the default), or the ref a dispatch
    /// started from.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base: Option<String>,
    /// The dispatch branch.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub branch: Option<String>,
    /// The short commit a dispatch started from.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sha: Option<String>,
    /// A dispatch that replaced an earlier one.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub relaunch: bool,
    /// What a cleanup did.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub outcome: Option<CleanupOutcome>,
    /// The fields an `edited` event changed.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub fields: Vec<EditedField>,
    /// The prerequisite whose completion started this task (a start or dispatch it caused).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub after: Option<u64>,
    /// Who an `on_set` event's record waited on before and after.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub on_from: Option<super::BlockOn>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub on_to: Option<super::BlockOn>,
}

impl EventDetail {
    /// A status change from `from` to `to`.
    pub fn status(from: super::HumanStatus, to: super::HumanStatus) -> Self {
        Self {
            from: Some(from),
            to: Some(to),
            ..Self::default()
        }
    }
}

thread_local! {
    static ACTOR: std::cell::RefCell<Option<String>> = const { std::cell::RefCell::new(None) };
    static MUTATION_INPUTS: std::cell::Cell<Option<MutationInputs>> = const { std::cell::Cell::new(None) };
}

#[derive(Clone, Copy)]
struct MutationInputs {
    at: SystemTime,
    seed: Uuid,
    next: u64,
}

/// Run a synchronous domain mutation with repeatable time, actor and generated IDs.
/// Use a distinct seed per replayed operation; outside the scope local mutations use
/// the wall clock and random IDs as usual.
pub(super) fn with_mutation_inputs<T>(
    at: SystemTime,
    seed: Uuid,
    actor: &str,
    run: impl FnOnce() -> T,
) -> T {
    struct Restore(Option<MutationInputs>);
    impl Drop for Restore {
        fn drop(&mut self) {
            MUTATION_INPUTS.set(self.0);
        }
    }
    let _restore = Restore(MUTATION_INPUTS.replace(Some(MutationInputs { at, seed, next: 0 })));
    acting_as(actor, run)
}

pub(super) fn mutation_time() -> SystemTime {
    MUTATION_INPUTS
        .get()
        .map_or_else(SystemTime::now, |inputs| inputs.at)
}

pub(super) fn mutation_id() -> Uuid {
    let Some(mut inputs) = MUTATION_INPUTS.get() else {
        return Uuid::new_v4();
    };
    let id = Uuid::new_v5(&inputs.seed, &inputs.next.to_be_bytes());
    inputs.next = inputs
        .next
        .checked_add(1)
        .expect("mutation ID counter exhausted");
    MUTATION_INPUTS.set(Some(inputs));
    id
}

/// Who this thread's domain mutations are recorded as: `you` unless a caller is acting as an
/// agent through [`acting_as`].
pub fn current_actor() -> String {
    ACTOR
        .with(|actor| actor.borrow().clone())
        .unwrap_or_else(|| super::OWNER.to_string())
}

/// Run `work` with this thread's mutations recorded as made by `by`. The CLI wraps every verb in
/// it with [`super::actor_from_env`]; the board never does, so its changes are always `you`.
pub fn acting_as<T>(by: &str, work: impl FnOnce() -> T) -> T {
    struct Restore(Option<String>);
    impl Drop for Restore {
        fn drop(&mut self) {
            ACTOR.with(|actor| *actor.borrow_mut() = self.0.take());
        }
    }
    let _restore = Restore(ACTOR.with(|actor| actor.borrow_mut().replace(by.to_string())));
    work()
}

#[cfg(test)]
mod replay_tests {
    use super::*;
    use crate::domain::{DomainState, HumanStatus, TaskScope};
    use std::time::{Duration, UNIX_EPOCH};
    use uuid::Uuid;

    #[test]
    fn domain_replay_reproduces_ids_times_history_and_undo() {
        let at = UNIX_EPOCH + Duration::from_secs(42);
        let run = || {
            with_mutation_inputs(at, Uuid::from_u128(1), "builder", || {
                let mut state = DomainState::new();
                let id = state
                    .create(
                        "task",
                        None,
                        TaskScope::Global,
                        ProvenanceOrigin::Manual,
                        None,
                    )
                    .unwrap();
                let step = state.add_step(id, "step").unwrap();
                state.toggle_step(id, step).unwrap();
                state
                    .change_status(&[id], HumanStatus::Ready, false)
                    .unwrap();
                let task = state.get(id).unwrap();
                assert_eq!(task.created_at, at);
                assert_eq!(task.updated_at, at);
                assert_ne!(id, step);
                assert_ne!(task.merge_base_revision, Some(task.revision));
                assert!(task
                    .history
                    .iter()
                    .all(|event| event.by.as_deref() == Some("builder")));
                assert!(state.last_undo().is_some());
                serde_json::to_value(state).unwrap()
            })
        };

        assert_eq!(run(), run());
        assert_eq!(current_actor(), crate::domain::OWNER);
    }

    #[test]
    fn nested_mutation_scope_restores_actor_clock_and_id_sequence_after_unwind() {
        let seed = Uuid::from_u128(2);
        let expected =
            with_mutation_inputs(UNIX_EPOCH, seed, "outer", || (mutation_id(), mutation_id()));
        with_mutation_inputs(UNIX_EPOCH, seed, "outer", || {
            assert_eq!(mutation_id(), expected.0);
            let failure = std::panic::catch_unwind(|| {
                with_mutation_inputs(
                    UNIX_EPOCH + Duration::from_secs(1),
                    Uuid::from_u128(3),
                    "inner",
                    || {
                        assert_eq!(current_actor(), "inner");
                        assert_ne!(mutation_time(), UNIX_EPOCH);
                        assert_ne!(mutation_id(), expected.1);
                        panic!("test unwind");
                    },
                );
            });
            assert!(failure.is_err());
            assert_eq!(current_actor(), "outer");
            assert_eq!(mutation_time(), UNIX_EPOCH);
            assert_eq!(mutation_id(), expected.1);
        });
        assert_eq!(current_actor(), crate::domain::OWNER);
    }
}
