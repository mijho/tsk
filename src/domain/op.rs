//! Serializable domain intents. Applying an intent never runs a host action.

use std::time::SystemTime;

use serde::{Deserialize, Serialize};
use uuid::Uuid;

use super::events::with_mutation_inputs;
use super::{
    Assignee, DispatchIntent, DomainError, DomainState, HumanStatus, ProvenanceOrigin, RoundIntent,
    TaskScope,
};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Op {
    pub id: Uuid,
    pub actor: String,
    pub at: SystemTime,
    pub intent: Intent,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "kind", deny_unknown_fields)]
pub enum Intent {
    /// An ordered save of already validated intents, with their original undo boundaries.
    /// Use typed marked verbs (not this wrapper) for one bulk board undo.
    Transaction {
        intents: Vec<Intent>,
    },
    Create {
        task: Uuid,
        title: String,
        notes: Option<String>,
        scope: TaskScope,
        provenance: ProvenanceOrigin,
        thread: Option<String>,
        assignee: Option<Assignee>,
        base: Option<String>,
        due: Option<String>,
        after: Vec<Uuid>,
        steps: Vec<NewStep>,
    },
    SetStatus {
        task: Uuid,
        from: HumanStatus,
        to: HumanStatus,
        prerequisite_policy: PrerequisitePolicy,
        undo: bool,
    },
    StatusBatch {
        tasks: Vec<StatusSource>,
        to: HumanStatus,
        prerequisite_policy: PrerequisitePolicy,
    },
    EditTask {
        task: Uuid,
        patch: Box<TaskPatch>,
    },
    SetAssignee {
        tasks: Vec<Uuid>,
        assignee: Option<Assignee>,
    },
    SetBase {
        tasks: Vec<Uuid>,
        base: Option<String>,
    },
    SetDue {
        task: Uuid,
        due: Option<String>,
    },
    SetAfter {
        tasks: Vec<Uuid>,
        after: Vec<Uuid>,
        undo: bool,
    },
    ChainAfter {
        tasks: Vec<Uuid>,
    },
    SetArchived {
        task: Uuid,
        archived: bool,
    },
    ToggleArchived {
        tasks: Vec<Uuid>,
        marked: bool,
    },
    SoftDelete {
        tasks: Vec<Uuid>,
        marked: bool,
    },
    Restore {
        task: Uuid,
    },
    Step {
        task: Uuid,
        change: StepChange,
        undo: bool,
    },
    SetProjectTitle {
        project: String,
        title: Option<String>,
    },
    SetProjectArchived {
        project: String,
        archived: bool,
    },
    HideAssignee {
        name: String,
    },
    Round {
        change: Box<RoundIntent>,
    },
    Dispatch {
        change: Box<DispatchIntent>,
    },
    Undo,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NewStep {
    pub id: Uuid,
    pub text: String,
}

/// Distinguishes a clear from an omitted patch field.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Field<T> {
    Set(T),
    Clear,
}

impl<T: Clone> Field<T> {
    fn value(&self) -> Option<T> {
        match self {
            Self::Set(value) => Some(value.clone()),
            Self::Clear => None,
        }
    }
    fn merge(field: &Option<Self>, previous: &Option<T>) -> Option<T> {
        field.as_ref().map_or_else(|| previous.clone(), Self::value)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NotesEdit {
    Replace(Field<String>),
    Append(String),
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TaskPatch {
    pub title: Option<String>,
    pub notes: Option<NotesEdit>,
    pub scope: Option<TaskScope>,
    pub thread: Option<Field<String>>,
    pub assignee: Option<Field<Assignee>>,
    pub base: Option<Field<String>>,
    pub due: Option<Field<String>>,
    pub after: Option<Vec<Uuid>>,
    pub board_steps: Option<FormSteps>,
}

/// Present for the board form, even when no steps were edited.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FormSteps {
    pub renames: Vec<(Uuid, String)>,
    pub removals: Vec<Uuid>,
    pub additions: Vec<NewStep>,
    pub after: Option<Uuid>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StepAnchor {
    Start,
    End,
    After(Uuid),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "kind", deny_unknown_fields)]
pub enum StepChange {
    Insert { step: NewStep, anchor: StepAnchor },
    Move { step: Uuid, anchor: StepAnchor },
    Rename { step: Uuid, text: String },
    Toggle { step: Uuid },
    Remove { step: Uuid },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PrerequisitePolicy {
    Check,
    Override,
    NotApplicable,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StatusSource {
    pub task: Uuid,
    pub from: HumanStatus,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ApplyOutcome {
    pub changed: bool,
    pub status_conflicts: Vec<StatusConflict>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StatusConflict {
    pub task: Uuid,
    pub expected: HumanStatus,
    pub actual: HumanStatus,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ApplyError {
    Domain(DomainError),
    WaitingOn { task: Uuid, prerequisites: Vec<u64> },
    DuplicateId(Uuid),
    UnnumberedPrerequisite(Uuid),
    InvalidIntent(&'static str),
    StaleRound(Uuid),
    StaleDispatch(Uuid),
}

impl From<DomainError> for ApplyError {
    fn from(error: DomainError) -> Self {
        Self::Domain(error)
    }
}

impl std::fmt::Display for ApplyError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Domain(error) => error.fmt(f),
            Self::WaitingOn {
                task,
                prerequisites,
            } => write!(f, "task {task} still waits on {prerequisites:?}"),
            Self::DuplicateId(id) => write!(f, "id {id} already exists"),
            Self::UnnumberedPrerequisite(id) => write!(f, "prerequisite {id} has no task number"),
            Self::InvalidIntent(reason) => f.write_str(reason),
            Self::StaleRound(id) => write!(f, "the open round on task {id} changed"),
            Self::StaleDispatch(id) => write!(f, "the dispatch on task {id} changed"),
        }
    }
}

impl std::error::Error for ApplyError {}

impl DomainState {
    /// Apply an accepted domain intent with the envelope's time and actor.
    pub fn apply(&mut self, op: &Op) -> Result<ApplyOutcome, ApplyError> {
        with_mutation_inputs(op.at, op.id, &op.actor, || {
            self.apply_intent(&op.intent, &op.actor)
        })
    }

    fn apply_intent(&mut self, intent: &Intent, actor: &str) -> Result<ApplyOutcome, ApplyError> {
        let mut status_conflicts = Vec::new();
        let changed = match intent {
            Intent::Transaction { intents } => {
                let mut staged = self.clone();
                let mut changed = false;
                for intent in intents {
                    let result = staged.apply_intent(intent, actor)?;
                    changed |= result.changed;
                    status_conflicts.extend(result.status_conflicts);
                }
                *self = staged;
                changed
            }
            Intent::Create {
                task,
                title,
                notes,
                scope,
                provenance,
                thread,
                assignee,
                base,
                due,
                after,
                steps,
            } => {
                if self.get(*task).is_some() {
                    return Err(ApplyError::DuplicateId(*task));
                }
                let after = self.prerequisite_numbers(after)?;
                self.check_new_after(&after)?;
                let mut staged = self.clone();
                staged.create_with_id(
                    *task,
                    title,
                    notes.clone(),
                    scope.clone(),
                    *provenance,
                    thread.clone(),
                )?;
                staged.configure_created(*task, assignee.clone(), base.clone())?;
                staged.set_due_on_create(*task, due.clone())?;
                staged.set_after_on_create(*task, &after)?;
                for step in steps {
                    staged.apply_step(
                        *task,
                        &StepChange::Insert {
                            step: step.clone(),
                            anchor: StepAnchor::End,
                        },
                    )?;
                }
                *self = staged;
                true
            }
            Intent::SetStatus {
                task,
                from,
                to,
                prerequisite_policy,
                undo,
            } => {
                let target = self.get(*task).ok_or(DomainError::UnknownId(*task))?;
                if target.soft_deleted {
                    return Err(DomainError::SoftDeleted(*task).into());
                }
                let actual = target.status;
                if actual != *from {
                    status_conflicts.push(StatusConflict {
                        task: *task,
                        expected: *from,
                        actual,
                    });
                }
                if *to == HumanStatus::Started && *prerequisite_policy == PrerequisitePolicy::Check
                {
                    let prerequisites = self.waiting_on(target);
                    if !prerequisites.is_empty() {
                        return Err(ApplyError::WaitingOn {
                            task: *task,
                            prerequisites,
                        });
                    }
                }
                if actual == *to {
                    false
                } else {
                    match (*to, *undo) {
                        (HumanStatus::Done, true) => self.complete(*task)?,
                        (_, true) => {
                            self.change_status(&[*task], *to, false)?;
                        }
                        (_, false) => self.set_status_by(*task, *to, actor)?,
                    }
                    true
                }
            }
            Intent::StatusBatch {
                tasks,
                to,
                prerequisite_policy,
            } => {
                let ids: Vec<_> = tasks.iter().map(|target| target.task).collect();
                self.prevalidate_batch_ids(&ids)?;
                let mut changed = false;
                for target in tasks {
                    let task = self
                        .get(target.task)
                        .ok_or(DomainError::UnknownId(target.task))?;
                    if task.status != target.from {
                        status_conflicts.push(StatusConflict {
                            task: task.id,
                            expected: target.from,
                            actual: task.status,
                        });
                    }
                    if *to == HumanStatus::Started
                        && *prerequisite_policy == PrerequisitePolicy::Check
                    {
                        let prerequisites = self.waiting_on(task);
                        if !prerequisites.is_empty() {
                            return Err(ApplyError::WaitingOn {
                                task: task.id,
                                prerequisites,
                            });
                        }
                    }
                    changed |= task.status != *to;
                }
                if *to == HumanStatus::Done {
                    self.complete_batch(&ids)?;
                    changed
                } else {
                    self.change_status(&ids, *to, true)?
                }
            }
            Intent::EditTask { task, patch } => self.apply_patch(*task, patch)?,
            Intent::SetAssignee { tasks, assignee } => {
                self.assign_batch(tasks, assignee.clone())?
            }
            Intent::SetBase { tasks, base } => self.set_base_batch(tasks, base.clone())?,
            Intent::SetDue { task, due } => self.edit_due(*task, due.clone())?,
            Intent::SetAfter { tasks, after, undo } => {
                let after = self.prerequisite_numbers(after)?;
                if *undo {
                    self.set_after_batch(tasks, &after)?
                } else {
                    let [task] = tasks.as_slice() else {
                        return Err(ApplyError::InvalidIntent("plain SetAfter needs one task"));
                    };
                    self.edit_after(*task, &after)?
                }
            }
            Intent::ChainAfter { tasks } => self.chain_after(tasks)?,
            Intent::SetArchived {
                task,
                archived: true,
            } => self.archive_task(*task)?,
            Intent::SetArchived {
                task,
                archived: false,
            } => self.unarchive_task(*task)?,
            Intent::ToggleArchived { tasks, marked } => self.file_tasks(tasks, *marked)?,
            Intent::SoftDelete { tasks, marked } => {
                if *marked {
                    self.soft_delete_batch(tasks)?;
                    !tasks.is_empty()
                } else {
                    let [task] = tasks.as_slice() else {
                        return Err(ApplyError::InvalidIntent("plain SoftDelete needs one task"));
                    };
                    self.soft_delete(*task)?;
                    true
                }
            }
            Intent::Restore { task } => {
                self.restore(*task)?;
                true
            }
            Intent::Step { task, change, undo } => {
                if *undo {
                    // Compute stable anchors before wrapping the legacy step mutation.
                    let index = self.step_position(*task, change)?;
                    self.with_steps_undo(*task, |state| state.mutate_step(*task, change, index))?
                } else {
                    self.apply_step(*task, change)?
                }
            }
            Intent::SetProjectTitle { project, title } => {
                self.set_project_title(project, title.as_deref().unwrap_or(""))?
            }
            Intent::SetProjectArchived {
                project,
                archived: true,
            } => self.archive_project(project)?,
            Intent::SetProjectArchived {
                project,
                archived: false,
            } => self.unarchive_project(project)?,
            Intent::HideAssignee { name } => self.hide_assignee(name),
            Intent::Round { change } => change.apply(self, actor)?,
            Intent::Dispatch { change } => return change.apply(self),
            Intent::Undo => {
                let changed = self.last_undo().is_some();
                self.undo()?;
                changed
            }
        };
        Ok(ApplyOutcome {
            changed,
            status_conflicts,
        })
    }

    fn apply_patch(&mut self, task: Uuid, patch: &TaskPatch) -> Result<bool, ApplyError> {
        let before = self.get(task).ok_or(DomainError::UnknownId(task))?;
        if before.soft_deleted {
            return Err(DomainError::SoftDeleted(task).into());
        }
        let title = patch.title.as_deref().unwrap_or(&before.title).trim();
        let notes = match &patch.notes {
            None => before.notes.clone(),
            Some(NotesEdit::Replace(value)) => value.value(),
            Some(NotesEdit::Append(text)) => Some(match before.notes.as_deref() {
                Some(notes) if !notes.trim().is_empty() => {
                    format!("{}\n\n{text}", notes.trim_end())
                }
                _ => text.clone(),
            }),
        };
        let scope = patch.scope.clone().unwrap_or_else(|| before.scope.clone());
        let thread = Field::merge(&patch.thread, &before.thread);
        let assignee = Field::merge(&patch.assignee, &before.assigned());
        let base = Field::merge(&patch.base, &before.base);
        let after = patch
            .after
            .as_ref()
            .map(|ids| self.prerequisite_numbers(ids))
            .transpose()?;
        let due = Field::merge(&patch.due, &before.due);
        let mut staged = self.clone();
        let mut changed = title != before.title
            || notes != before.notes
            || scope != before.scope
            || thread != before.thread
            || assignee != before.assigned()
            || base != before.base;
        if let Some(steps) = &patch.board_steps {
            let mut ids: std::collections::BTreeSet<_> =
                before.steps.iter().map(|step| step.id).collect();
            for step in &steps.additions {
                if !ids.insert(step.id) {
                    return Err(ApplyError::DuplicateId(step.id));
                }
            }
            let additions: Vec<_> = steps
                .additions
                .iter()
                .map(|step| (Some(step.id), step.text.as_str()))
                .collect();
            if let Some(after) = &after {
                self.check_after(task, after)?;
            }
            staged.with_steps_undo(task, |state| {
                state.edit_with_identified_steps(
                    task,
                    title,
                    notes,
                    scope,
                    thread,
                    assignee,
                    base,
                    &steps.renames,
                    &steps.removals,
                    &additions,
                    steps.after,
                )?;
                if let Some(after) = &after {
                    state.stage_after_in_edit(task, after)?;
                }
                if patch.due.is_some() {
                    state.stage_due_in_edit(task, due)?;
                }
                Ok(())
            })?;
            *self = staged;
            return Ok(true);
        }
        if changed {
            staged
                .edit_with_assignee_and_base(task, title, notes, scope, thread, assignee, base)?;
        }
        if let Some(after) = &after {
            changed |= staged.edit_after(task, after)?;
        }
        if patch.due.is_some() {
            changed |= staged.edit_due(task, due)?;
        }
        if changed {
            *self = staged;
        }
        Ok(changed)
    }

    fn prerequisite_numbers(&self, ids: &[Uuid]) -> Result<Vec<u64>, ApplyError> {
        ids.iter()
            .map(|id| {
                self.get(*id)
                    .ok_or(DomainError::UnknownId(*id))?
                    .number
                    .ok_or(ApplyError::UnnumberedPrerequisite(*id))
            })
            .collect()
    }

    fn apply_step(&mut self, task: Uuid, change: &StepChange) -> Result<bool, ApplyError> {
        let index = self.step_position(task, change)?;
        Ok(self.mutate_step(task, change, index)?)
    }

    fn step_position(&self, task: Uuid, change: &StepChange) -> Result<usize, ApplyError> {
        let task = self.get(task).ok_or(DomainError::UnknownId(task))?;
        let (anchor, moving) = match change {
            StepChange::Insert { step, anchor } => {
                if task.steps.iter().any(|existing| existing.id == step.id) {
                    return Err(ApplyError::DuplicateId(step.id));
                }
                (*anchor, None)
            }
            StepChange::Move { step, anchor } => (*anchor, Some(*step)),
            _ => return Ok(0),
        };
        let ids = || {
            task.steps
                .iter()
                .map(|step| step.id)
                .filter(|id| Some(*id) != moving)
        };
        match anchor {
            StepAnchor::Start => Ok(0),
            StepAnchor::End => Ok(ids().count()),
            StepAnchor::After(id) => ids()
                .position(|candidate| candidate == id)
                .map(|index| index + 1)
                .ok_or_else(|| DomainError::UnknownStep(id).into()),
        }
    }

    fn mutate_step(
        &mut self,
        task: Uuid,
        change: &StepChange,
        index: usize,
    ) -> Result<bool, DomainError> {
        let revision = self.get(task).ok_or(DomainError::UnknownId(task))?.revision;
        match change {
            StepChange::Insert { step, .. } => {
                self.insert_step_with_id(task, step.id, &step.text, index)?;
            }
            StepChange::Move { step, .. } => {
                self.move_step(task, *step, index)?;
            }
            StepChange::Rename { step, text } => self.rename_step(task, *step, text)?,
            StepChange::Toggle { step } => self.toggle_step(task, *step)?,
            StepChange::Remove { step } => {
                self.remove_step(task, *step)?;
            }
        }
        Ok(self.get(task).is_some_and(|task| task.revision != revision))
    }
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, UNIX_EPOCH};

    use super::*;
    use crate::domain::{ProvenanceOrigin, TaskScope};

    fn envelope(intent: Intent) -> Op {
        Op {
            id: Uuid::from_u128(1),
            actor: "builder".into(),
            at: UNIX_EPOCH + Duration::from_secs(42),
            intent,
        }
    }

    #[test]
    fn serialized_status_op_replays_deterministically_and_reports_conflict() {
        let mut state = DomainState::new();
        let task = state
            .create(
                "task",
                None,
                TaskScope::Global,
                ProvenanceOrigin::Manual,
                None,
            )
            .unwrap();
        let op = envelope(Intent::SetStatus {
            task,
            from: HumanStatus::Ready,
            to: HumanStatus::Done,
            prerequisite_policy: PrerequisitePolicy::NotApplicable,
            undo: true,
        });
        let encoded = serde_json::to_string(&op).unwrap();
        let decoded: Op = serde_json::from_str(&encoded).unwrap();
        assert_eq!(decoded, op);
        let mut replay = state.clone();
        let result = state.apply(&op).unwrap();
        assert_eq!(
            result,
            ApplyOutcome {
                changed: true,
                status_conflicts: vec![StatusConflict {
                    task,
                    expected: HumanStatus::Ready,
                    actual: HumanStatus::Open,
                }],
            }
        );
        assert_eq!(replay.apply(&decoded).unwrap(), result);
        assert_eq!(
            serde_json::to_value(&state).unwrap(),
            serde_json::to_value(&replay).unwrap()
        );
        assert_eq!(state.get(task).unwrap().updated_at, op.at);
        assert_eq!(state.completed_ids(), &[task]);
        assert!(state.last_undo().is_some());
    }

    fn apply(
        state: &mut DomainState,
        id: u128,
        intent: Intent,
    ) -> Result<ApplyOutcome, ApplyError> {
        let mut op = envelope(intent);
        op.id = Uuid::from_u128(id);
        let wire = serde_json::to_vec(&op).unwrap();
        state.apply(&serde_json::from_slice(&wire).unwrap())
    }

    fn create(task: Uuid, after: Vec<Uuid>, steps: Vec<NewStep>) -> Intent {
        Intent::Create {
            task,
            title: "task".into(),
            notes: None,
            scope: TaskScope::Project {
                path: "/tmp/tsk-op-demo".into(),
            },
            provenance: ProvenanceOrigin::Selection,
            thread: Some("sync".into()),
            assignee: None,
            base: None,
            due: None,
            after,
            steps,
        }
    }

    #[test]
    fn create_preserves_intent_ids_and_rejects_partial_create() {
        let task = Uuid::from_u128(10);
        let step = NewStep {
            id: Uuid::from_u128(11),
            text: "first".into(),
        };
        let mut state = DomainState::new();
        assert_eq!(
            apply(
                &mut state,
                2,
                create(task, vec![], vec![step.clone(), step.clone()])
            ),
            Err(ApplyError::DuplicateId(step.id))
        );
        assert!(state.tasks().is_empty());
        assert!(
            apply(&mut state, 2, create(task, vec![], vec![step.clone()]))
                .unwrap()
                .changed
        );
        let created = state.get(task).unwrap();
        assert_eq!(created.provenance, ProvenanceOrigin::Selection);
        assert_eq!(created.steps[0].id, step.id);
        assert_eq!(state.undo_len(), 0);
    }

    #[test]
    fn transaction_rolls_back_prior_writes_and_undo_on_refusal() {
        let task = Uuid::from_u128(10);
        let mut state = DomainState::new();
        apply(&mut state, 2, create(task, vec![], vec![])).unwrap();
        let before = serde_json::to_value(&state).unwrap();
        let result = apply(
            &mut state,
            3,
            Intent::Transaction {
                intents: vec![
                    Intent::ToggleArchived {
                        tasks: vec![task],
                        marked: false,
                    },
                    Intent::Step {
                        task,
                        change: StepChange::Toggle { step: Uuid::nil() },
                        undo: true,
                    },
                ],
            },
        );
        assert_eq!(result, Err(DomainError::UnknownStep(Uuid::nil()).into()));
        assert_eq!(serde_json::to_value(&state).unwrap(), before);
    }

    #[test]
    fn steps_use_anchors_and_undo_restores_original_order() {
        let task = Uuid::from_u128(10);
        let steps: Vec<_> = (11..14)
            .map(|id| NewStep {
                id: Uuid::from_u128(id),
                text: id.to_string(),
            })
            .collect();
        let [a, b, c] = [steps[0].id, steps[1].id, steps[2].id];
        let mut state = DomainState::new();
        apply(&mut state, 2, create(task, vec![], steps)).unwrap();
        let order = |state: &DomainState| {
            state
                .get(task)
                .unwrap()
                .steps
                .iter()
                .map(|step| step.id)
                .collect::<Vec<_>>()
        };
        apply(
            &mut state,
            3,
            Intent::Step {
                task,
                change: StepChange::Move {
                    step: a,
                    anchor: StepAnchor::After(b),
                },
                undo: true,
            },
        )
        .unwrap();
        assert_eq!(order(&state), [b, a, c]);
        apply(&mut state, 4, Intent::Undo).unwrap();
        assert_eq!(order(&state), [a, b, c]);
    }

    #[test]
    fn explicit_prerequisite_override_does_not_change_normal_start_validation() {
        let mut state = DomainState::new();
        let prerequisite = Uuid::from_u128(10);
        let task = Uuid::from_u128(11);
        apply(&mut state, 2, create(prerequisite, vec![], vec![])).unwrap();
        state.assign_numbers_for_persistence();
        apply(&mut state, 3, create(task, vec![prerequisite], vec![])).unwrap();
        let start = |policy| Intent::SetStatus {
            task,
            from: HumanStatus::Open,
            to: HumanStatus::Started,
            prerequisite_policy: policy,
            undo: false,
        };
        assert!(matches!(
            apply(&mut state, 4, start(PrerequisitePolicy::Check)),
            Err(ApplyError::WaitingOn { .. })
        ));
        apply(&mut state, 5, start(PrerequisitePolicy::Override)).unwrap();
        assert_eq!(state.get(task).unwrap().status, HumanStatus::Started);
        assert_eq!(state.undo_len(), 0);
    }

    #[test]
    fn board_form_undo_restores_steps_without_reverting_task_fields() {
        let mut state = DomainState::new();
        let task = Uuid::from_u128(10);
        let original = NewStep {
            id: Uuid::from_u128(11),
            text: "old".into(),
        };
        let addition = NewStep {
            id: Uuid::from_u128(12),
            text: "new".into(),
        };
        apply(&mut state, 2, create(task, vec![], vec![original.clone()])).unwrap();
        apply(
            &mut state,
            3,
            Intent::EditTask {
                task,
                patch: Box::new(TaskPatch {
                    title: Some("renamed".into()),
                    notes: Some(NotesEdit::Append("kept".into())),
                    board_steps: Some(FormSteps {
                        removals: vec![original.id],
                        additions: vec![addition.clone()],
                        ..FormSteps::default()
                    }),
                    ..TaskPatch::default()
                }),
            },
        )
        .unwrap();
        assert_eq!(state.get(task).unwrap().steps[0].id, addition.id);
        apply(&mut state, 4, Intent::Undo).unwrap();
        let task = state.get(task).unwrap();
        assert_eq!(task.title, "renamed");
        assert_eq!(task.notes.as_deref(), Some("kept"));
        assert_eq!(task.steps[0].id, original.id);
    }

    #[test]
    fn local_lifecycle_and_project_intents_round_trip() {
        let mut state = DomainState::new();
        let task = Uuid::from_u128(10);
        let other = Uuid::from_u128(11);
        let project = "/tmp/tsk-op-demo".to_string();
        let assignee = Assignee::agent("builder");
        apply(&mut state, 2, create(task, vec![], vec![])).unwrap();
        apply(&mut state, 3, create(other, vec![], vec![])).unwrap();
        state.assign_numbers_for_persistence();
        let intents = [
            Intent::SetProjectTitle {
                project: project.clone(),
                title: Some("Shared".into()),
            },
            Intent::SetProjectArchived {
                project: project.clone(),
                archived: true,
            },
            Intent::SetProjectArchived {
                project: project.clone(),
                archived: false,
            },
            Intent::SetAssignee {
                tasks: vec![task],
                assignee: Some(assignee.clone()),
            },
            Intent::SetBase {
                tasks: vec![task],
                base: Some("main".into()),
            },
            Intent::SetDue {
                task,
                due: Some("2026-12-01".into()),
            },
            Intent::SetArchived {
                task,
                archived: true,
            },
            Intent::ToggleArchived {
                tasks: vec![task],
                marked: false,
            },
            Intent::SetAfter {
                tasks: vec![task],
                after: vec![other],
                undo: false,
            },
            Intent::ChainAfter {
                tasks: vec![other, task],
            },
            Intent::SoftDelete {
                tasks: vec![task],
                marked: true,
            },
            Intent::Restore { task },
        ];
        let mut replica = state.clone();
        for (index, intent) in intents.into_iter().enumerate() {
            let op = index as u128 + 4;
            let first = apply(&mut state, op, intent.clone()).unwrap();
            assert_eq!(first, apply(&mut replica, op, intent).unwrap());
        }
        assert_eq!(
            serde_json::to_value(&state).unwrap(),
            serde_json::to_value(replica).unwrap()
        );
        let task = state.get(task).unwrap();
        assert_eq!(task.assigned(), Some(assignee));
        assert_eq!(task.base.as_deref(), Some("main"));
        assert_eq!(task.due.as_deref(), Some("2026-12-01"));
        assert!(!task.archived && !task.soft_deleted);
        assert_eq!(state.project_name(&project), "Shared");
    }

    #[test]
    fn marked_status_is_atomic_with_one_undo_and_reports_each_conflict() {
        let mut state = DomainState::new();
        let a = Uuid::from_u128(10);
        let b = Uuid::from_u128(11);
        apply(&mut state, 2, create(a, vec![], vec![])).unwrap();
        apply(&mut state, 3, create(b, vec![], vec![])).unwrap();
        let source = |task| StatusSource {
            task,
            from: HumanStatus::Ready,
        };
        let batch = |tasks| Intent::StatusBatch {
            tasks,
            to: HumanStatus::Done,
            prerequisite_policy: PrerequisitePolicy::NotApplicable,
        };
        assert!(apply(&mut state, 4, batch(vec![source(a), source(Uuid::nil())])).is_err());
        assert!(state.completed_ids().is_empty());
        let result = apply(&mut state, 5, batch(vec![source(a), source(b)])).unwrap();
        assert_eq!(result.status_conflicts.len(), 2);
        assert_eq!(state.undo_len(), 1);
        assert_eq!(state.completed_ids(), &[a, b]);
        apply(&mut state, 6, Intent::Undo).unwrap();
        assert_eq!(state.get(a).unwrap().status, HumanStatus::Open);
        assert_eq!(state.get(b).unwrap().status, HumanStatus::Open);
    }
}
