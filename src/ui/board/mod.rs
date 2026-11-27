//! Queue-board presentation, split by state, commands, reduction, chrome, and drawing.

mod apply;
mod block;
mod chrome;
mod cleanup_card;
mod commands;
mod dispatch_card;
mod draw;
mod launch_card;
mod model;
mod project_title;
mod selector;

pub use apply::{
    apply_intent, board_intent_may_persist, ctrl_s_starts, ROW_DOUBLE_CLICK_WINDOW,
    SHIFT_ENTER_AS_CTRL_J_HINT,
};
pub use block::{BlockCard, BlockCardField, BlockTarget, OnKind};
#[cfg(test)]
pub(crate) use block::{BLOCK_REPLACED, NOTHING_TO_SEND_BACK};
pub use chrome::DELETE_NOTICE_UNDO;
pub use cleanup_card::{CleanupChoice, CleanupOption, RowVerdict};
pub use commands::{resolve_board_command, BoardCommand, CommandSurface};
pub use dispatch_card::DispatchChoice;
pub use draw::{board_hit_map, board_verb_items, draw_board};
pub use launch_card::{LaunchChoice, LaunchPrompt};
pub use model::{
    nothing_to_dispatch, project_option_label, BoardInputMode, BoardLayout, BoardModel, BoardTab,
    BulkCleanup, BulkDispatchRun, CleanupPr, CleanupPrompt, CleanupRow, CleanupRowState,
    CleanupRun, CleanupRunRow, DispatchPrompt, FilterTab, IntentOutcome, ListPickerKind, MergedPr,
    PendingLaunch, PickerTab, ProjectScopeOption, ProjectsView, RelaunchPrompt, SaveResolution,
    StartAnywayPrompt, BOARD_TITLE, CLEANUP_BUSY, FINISHING_CLEANUP, REFRESHING_BRANCHES,
};
pub use selector::{CardChoice, CardOption, OptionCard, OptionSelector, CARD_ENTER_GUARD};

/// Apply one board intent as a local op by this thread's actor (the board is always `you`).
/// Returns whether it changed anything.
pub(crate) fn apply_op(
    domain: &mut crate::domain::DomainState,
    intent: crate::domain::Intent,
) -> Result<bool, crate::domain::DomainError> {
    Ok(domain
        .apply_local(&crate::domain::current_actor(), intent)?
        .changed)
}

/// A board status verb on `targets` as one op: a marked set (`bulk`) is one batch and one undo
/// entry, a single task one status undo. `start_card` is the start card's batch of starts.
pub(crate) fn status_op(
    domain: &mut crate::domain::DomainState,
    targets: &[uuid::Uuid],
    to: crate::domain::HumanStatus,
    bulk: bool,
    prerequisite_policy: crate::domain::PrerequisitePolicy,
) -> Result<bool, crate::domain::DomainError> {
    use crate::domain::{DomainError, Intent, StatusSource};
    let tasks = targets
        .iter()
        .map(|id| {
            let from = domain.get(*id).ok_or(DomainError::UnknownId(*id))?.status;
            Ok(StatusSource { task: *id, from })
        })
        .collect::<Result<Vec<_>, DomainError>>()?;
    let intent = match tasks.as_slice() {
        _ if bulk => Intent::StatusBatch {
            tasks,
            to,
            prerequisite_policy,
        },
        [] => return Ok(false),
        [source] => Intent::SetStatus {
            task: source.task,
            from: source.from,
            to,
            prerequisite_policy,
            undo: true,
        },
        _ => {
            return Err(DomainError::Refused(
                "an unmarked status change has one task".into(),
            ))
        }
    };
    apply_op(domain, intent)
}

/// The start card's marked set: one batch of start undo entries.
pub(crate) fn start_batch_op(
    domain: &mut crate::domain::DomainState,
    targets: &[uuid::Uuid],
    prerequisite_policy: crate::domain::PrerequisitePolicy,
) -> Result<bool, crate::domain::DomainError> {
    use crate::domain::{DomainError, Intent, StatusSource};
    let tasks = targets
        .iter()
        .map(|id| {
            let from = domain.get(*id).ok_or(DomainError::UnknownId(*id))?.status;
            Ok(StatusSource { task: *id, from })
        })
        .collect::<Result<Vec<_>, DomainError>>()?;
    apply_op(
        domain,
        Intent::StartBatch {
            tasks,
            prerequisite_policy,
        },
    )
}

/// How a board start treats prerequisites: checked, unless the start-anyway card said yes.
pub(crate) fn start_policy(model: &BoardModel) -> crate::domain::PrerequisitePolicy {
    if model.start_anyway {
        crate::domain::PrerequisitePolicy::Override
    } else {
        crate::domain::PrerequisitePolicy::Check
    }
}

/// The task ids of prerequisite numbers, as an op carries them.
pub(crate) fn after_ids(
    domain: &crate::domain::DomainState,
    after: &[u64],
) -> Result<Vec<uuid::Uuid>, crate::domain::DomainError> {
    after
        .iter()
        .map(|number| {
            domain
                .task_by_number(*number)
                .map(|task| task.id)
                .ok_or(crate::domain::DomainError::AfterUnknown(*number))
        })
        .collect()
}

/// One task-page step change as a board op, with its own steps undo entry.
pub(crate) fn step_op(
    domain: &mut crate::domain::DomainState,
    task: uuid::Uuid,
    change: crate::domain::StepChange,
) -> Result<bool, crate::domain::DomainError> {
    apply_op(
        domain,
        crate::domain::Intent::Step {
            task,
            change,
            undo: true,
        },
    )
}

/// The anchor that puts `moving` at `index` of `task`'s steps.
pub(crate) fn move_anchor(
    domain: &crate::domain::DomainState,
    task: uuid::Uuid,
    moving: uuid::Uuid,
    index: usize,
) -> crate::domain::StepAnchor {
    let others: Vec<uuid::Uuid> = domain
        .get(task)
        .map(|task| task.steps.iter().map(|step| step.id))
        .into_iter()
        .flatten()
        .filter(|id| *id != moving)
        .collect();
    match index.checked_sub(1) {
        None => crate::domain::StepAnchor::Start,
        Some(before) => others
            .get(before)
            .map_or(crate::domain::StepAnchor::End, |id| {
                crate::domain::StepAnchor::After(*id)
            }),
    }
}
