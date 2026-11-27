//! Replayable block, review, check, and reply mutations. No delivery or host calls.
//!
//! Legacy keys are scoped to a task/round: rounds use `BlockKey`, replies use exact
//! `(by, at)`, and checks use exact text. Never serialize UI positions. Migrate these
//! selectors (and creation) to persistent IDs when the store gains round/reply/check
//! IDs; until then an ambiguous legacy selector must not pick a first match.

use std::collections::BTreeSet;
use std::time::SystemTime;

use serde::{Deserialize, Serialize};
use uuid::Uuid;

use super::events::mutation_time;
use super::op::ApplyError;
use super::{
    block_text, Block, BlockDraft, BlockField, BlockKey, BlockKind, BlockOn, BlockPatch,
    CheckState, DomainError, DomainState, HumanStatus, Reply, ReviewDraft, ReviewPatch, Task,
    OWNER,
};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "kind", deny_unknown_fields)]
pub enum RoundIntent {
    /// Precondition for a reply-box transaction (even when there is no reply).
    Guard {
        task: Uuid,
        record: BlockKey,
        require_failed_check: bool,
    },
    OpenBlock {
        targets: Vec<OpenRoundTarget>,
        mode: OpenRoundMode,
        draft: BlockDraft,
    },
    OpenReview {
        targets: Vec<OpenRoundTarget>,
        mode: OpenRoundMode,
        draft: ReviewDraft,
    },
    EditBlock {
        task: Uuid,
        record: BlockKey,
        patch: BlockIntentPatch,
    },
    EditReview {
        task: Uuid,
        record: BlockKey,
        patch: ReviewIntentPatch,
    },
    SetOn {
        changes: Vec<WaitingOnChange>,
    },
    SetCheck {
        task: Uuid,
        record: BlockKey,
        check: String,
        /// Which check with this exact text, counting from 0 in round order.
        occurrence: usize,
        state: CheckState,
    },
    AddReply {
        task: Uuid,
        record: BlockKey,
        text: String,
    },
    EditReply {
        task: Uuid,
        record: BlockKey,
        reply: ReplyKey,
        text: String,
    },
    DeleteReply {
        task: Uuid,
        record: BlockKey,
        reply: ReplyKey,
    },
    MarkReplySent {
        task: Uuid,
        record: BlockKey,
        reply: ReplyKey,
        /// Exact stored text that was delivered, not a new draft to normalize.
        sent_text: String,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OpenRoundMode {
    Cli,
    Board,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OpenRoundTarget {
    pub task: Uuid,
    pub from: HumanStatus,
    /// The open record the caller observed, if any. A matching `from` alone
    /// cannot authorize closing a different round of that same status.
    pub expected_record: Option<BlockKey>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WaitingOnChange {
    pub task: Uuid,
    pub record: BlockKey,
    pub on: BlockOn,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReplyKey {
    pub by: String,
    pub at: SystemTime,
}

impl ReplyKey {
    pub fn of(reply: &Reply) -> Self {
        Self {
            by: reply.by.clone(),
            at: reply.at,
        }
    }

    fn matches(&self, reply: &Reply) -> bool {
        self.by == reply.by && self.at == reply.at
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum TextPatch {
    #[default]
    Keep,
    Clear,
    Set(String),
}

impl TextPatch {
    fn input(&self) -> Option<&str> {
        match self {
            Self::Keep | Self::Clear => None,
            Self::Set(value) => Some(value),
        }
    }

    fn patch(&self, validated: Option<String>) -> Option<Option<String>> {
        match self {
            Self::Keep => None,
            Self::Clear | Self::Set(_) => Some(validated),
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BlockIntentPatch {
    #[serde(default)]
    pub why: TextPatch,
    #[serde(default)]
    pub needs: TextPatch,
    pub options: Option<Vec<String>>,
    pub on: Option<BlockOn>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReviewIntentPatch {
    #[serde(default)]
    pub done: TextPatch,
    /// Replacement text list. Existing matching checks keep their state;
    /// newly introduced checks start open. An empty list clears the checks.
    pub checks: Option<Vec<String>>,
    #[serde(default)]
    pub next: TextPatch,
    pub on: Option<BlockOn>,
}

impl RoundIntent {
    /// Apply within the enclosing op's `with_mutation_inputs(at, id, actor, ...)`.
    /// `actor` is the resolved domain author (`you` for the board owner), not an
    /// arbitrary payload author. New legacy identities use that actor and `at`.
    pub(super) fn apply(&self, domain: &mut DomainState, actor: &str) -> Result<bool, ApplyError> {
        match self {
            Self::Guard {
                task,
                record,
                require_failed_check,
            } => {
                let round = require_round(domain, *task, record)?;
                if *require_failed_check && (!round.is_review() || round.failed_checks().is_empty())
                {
                    return Err(ApplyError::InvalidIntent(
                        "empty send-back needs a failed check",
                    ));
                }
                Ok(false)
            }
            Self::OpenBlock {
                targets,
                mode,
                draft,
            } => {
                let draft = BlockDraft::from_input(
                    draft.why.as_deref(),
                    draft.needs.as_deref(),
                    &draft.options,
                    draft.on.clone(),
                )
                .map_err(DomainError::TextTooLong)?;
                let ids = open_targets(domain, targets, *mode, BlockKind::Blocked, actor)?;
                match mode {
                    OpenRoundMode::Cli => domain.block(ids[0], draft, actor).map(|()| true),
                    OpenRoundMode::Board => domain.block_batch(&ids, &draft, actor),
                }
                .map_err(Into::into)
            }
            Self::OpenReview {
                targets,
                mode,
                draft,
            } => {
                let checks: Vec<_> = draft
                    .checks
                    .iter()
                    .map(|check| check.text.clone())
                    .collect();
                let draft = ReviewDraft::from_input(
                    draft.done.as_deref(),
                    &checks,
                    draft.next.as_deref(),
                    draft.on.clone(),
                )
                .map_err(DomainError::TextTooLong)?;
                let ids = open_targets(domain, targets, *mode, BlockKind::Review, actor)?;
                match mode {
                    OpenRoundMode::Cli => domain.review(ids[0], draft, actor).map(|()| true),
                    OpenRoundMode::Board => domain.review_batch(&ids, &draft, actor),
                }
                .map_err(Into::into)
            }
            Self::EditBlock {
                task,
                record,
                patch,
            } => {
                let block = require_round(domain, *task, record)?;
                if block.is_review() {
                    return Err(DomainError::NotBlocked(*task).into());
                }
                let validated = BlockDraft::from_input(
                    patch.why.input(),
                    patch.needs.input(),
                    patch.options.as_deref().unwrap_or_default(),
                    patch.on.clone().unwrap_or_default(),
                )
                .map_err(DomainError::TextTooLong)?;
                Ok(domain.edit_block(
                    *task,
                    BlockPatch {
                        why: patch.why.patch(validated.why),
                        needs: patch.needs.patch(validated.needs),
                        options: patch.options.as_ref().map(|_| validated.options),
                        on: patch.on.as_ref().map(|_| validated.on),
                    },
                )?)
            }
            Self::EditReview {
                task,
                record,
                patch,
            } => {
                let block = require_round(domain, *task, record)?;
                if !block.is_review() {
                    return Err(DomainError::NotInReview(*task).into());
                }
                let validated = ReviewDraft::from_input(
                    patch.done.input(),
                    patch.checks.as_deref().unwrap_or_default(),
                    patch.next.input(),
                    patch.on.clone().unwrap_or_default(),
                )
                .map_err(DomainError::TextTooLong)?;
                Ok(domain.edit_review(
                    *task,
                    ReviewPatch {
                        done: patch.done.patch(validated.done),
                        checks: patch.checks.as_ref().map(|_| validated.checks),
                        next: patch.next.patch(validated.next),
                        on: patch.on.as_ref().map(|_| validated.on),
                    },
                )?)
            }
            Self::SetOn { changes } => {
                let changes = changes
                    .iter()
                    .map(|change| {
                        require_round(domain, change.task, &change.record)?;
                        Ok((change.task, validate_on(&change.on)?))
                    })
                    .collect::<Result<Vec<_>, ApplyError>>()?;
                Ok(domain.set_on_batch(&changes)?)
            }
            Self::SetCheck {
                task,
                record,
                check,
                occurrence,
                state,
            } => {
                let block = require_round(domain, *task, record)?;
                if !block.is_review() {
                    return Err(DomainError::NotInReview(*task).into());
                }
                let index = block
                    .checks
                    .iter()
                    .enumerate()
                    .filter(|(_, item)| item.text == *check)
                    .nth(*occurrence)
                    .map(|(index, _)| index)
                    .ok_or(ApplyError::InvalidIntent("unknown check"))?;
                Ok(domain.set_check(*task, index, *state)?)
            }
            Self::AddReply { task, record, text } => {
                let block = require_round(domain, *task, record)?;
                let at = mutation_time();
                if block
                    .replies
                    .iter()
                    .any(|reply| reply.by == actor && reply.at == at)
                {
                    return Err(ApplyError::InvalidIntent("duplicate legacy reply key"));
                }
                domain.reply(*task, text, actor)?;
                Ok(true)
            }
            Self::EditReply {
                task,
                record,
                reply,
                text,
            } => {
                let block = require_round(domain, *task, record)?;
                let index = owner_reply_index(block, reply, actor)?;
                let revision = live_task(domain, *task)?.revision;
                domain.edit_reply(*task, index, text)?;
                Ok(live_task(domain, *task)?.revision != revision)
            }
            Self::DeleteReply {
                task,
                record,
                reply,
            } => {
                let block = require_round(domain, *task, record)?;
                let index = owner_reply_index(block, reply, actor)?;
                domain.delete_reply(*task, index)?;
                Ok(true)
            }
            Self::MarkReplySent {
                task,
                record,
                reply,
                sent_text,
            } => {
                require_owner(actor)?;
                let Some(block) = matching_round(live_task(domain, *task)?, record) else {
                    return Ok(false);
                };
                let Some(index) = reply_index(block, reply)? else {
                    return Ok(false);
                };
                Ok(domain.mark_reply_sent(*task, record, index, sent_text)?)
            }
        }
    }
}

fn live_task(domain: &DomainState, task: Uuid) -> Result<&Task, DomainError> {
    let target = domain.get(task).ok_or(DomainError::UnknownId(task))?;
    if target.soft_deleted {
        return Err(DomainError::SoftDeleted(task));
    }
    Ok(target)
}

fn matching_round<'a>(task: &'a Task, key: &BlockKey) -> Option<&'a Block> {
    if task
        .past_blocks
        .iter()
        .any(|block| block.by == key.by && block.at == key.at)
    {
        return None;
    }
    task.block
        .as_ref()
        .filter(|block| block.closed_at.is_none() && block.by == key.by && block.at == key.at)
}

fn require_round<'a>(
    domain: &'a DomainState,
    task: Uuid,
    key: &BlockKey,
) -> Result<&'a Block, ApplyError> {
    matching_round(live_task(domain, task)?, key).ok_or(ApplyError::StaleRound(task))
}

fn open_targets(
    domain: &DomainState,
    targets: &[OpenRoundTarget],
    mode: OpenRoundMode,
    kind: BlockKind,
    actor: &str,
) -> Result<Vec<Uuid>, ApplyError> {
    match mode {
        OpenRoundMode::Cli if targets.len() != 1 => {
            return Err(ApplyError::InvalidIntent("CLI round open needs one task"));
        }
        OpenRoundMode::Board => require_owner(actor)?,
        OpenRoundMode::Cli => {}
    }
    let at = mutation_time();
    let mut seen = BTreeSet::new();
    let mut ids = Vec::with_capacity(targets.len());
    for target in targets {
        if !seen.insert(target.task) {
            continue; // Board batches retain the first occurrence.
        }
        let task = live_task(domain, target.task)?;
        if task.status != target.from
            || task.block.as_ref().map(Block::key) != target.expected_record
        {
            return Err(ApplyError::StaleRound(target.task));
        }
        let same_kind = task.block.as_ref().is_some_and(|block| block.kind == kind);
        if !same_kind
            && task
                .block
                .iter()
                .chain(&task.past_blocks)
                .any(|block| block.by == actor && block.at == at)
        {
            return Err(ApplyError::InvalidIntent("duplicate legacy round key"));
        }
        ids.push(target.task);
    }
    Ok(ids)
}

fn validate_on(on: &BlockOn) -> Result<BlockOn, DomainError> {
    match on {
        BlockOn::Other(text) => Ok(block_text(Some(text), BlockField::On)
            .map_err(DomainError::TextTooLong)?
            .map_or(BlockOn::You, BlockOn::Other)),
        on => Ok(on.clone()),
    }
}

fn require_owner(actor: &str) -> Result<(), ApplyError> {
    if actor != OWNER {
        return Err(ApplyError::InvalidIntent("round action needs the owner"));
    }
    Ok(())
}

fn reply_index(block: &Block, key: &ReplyKey) -> Result<Option<usize>, ApplyError> {
    // Include tombstones in uniqueness checks: hiding one must never retarget an op.
    unique_index(
        block.replies.iter().map(|reply| key.matches(reply)),
        "ambiguous reply key",
    )
}

fn owner_reply_index(block: &Block, key: &ReplyKey, actor: &str) -> Result<usize, ApplyError> {
    require_owner(actor)?;
    let index = reply_index(block, key)?.ok_or(ApplyError::InvalidIntent("unknown reply key"))?;
    let reply = &block.replies[index];
    if !reply.is_owner() || reply.deleted {
        return Err(DomainError::UnknownReply(index).into());
    }
    Ok(index)
}

fn unique_index(
    matches: impl Iterator<Item = bool>,
    ambiguous: &'static str,
) -> Result<Option<usize>, ApplyError> {
    let mut indices = matches
        .enumerate()
        .filter_map(|(index, matches)| matches.then_some(index));
    let first = indices.next();
    if indices.next().is_some() {
        return Err(ApplyError::InvalidIntent(ambiguous));
    }
    Ok(first)
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, UNIX_EPOCH};

    use super::*;
    use crate::domain::events::with_mutation_inputs;
    use crate::domain::{Check, ProvenanceOrigin, TaskScope, UndoEntry};

    fn at(tick: u64) -> SystemTime {
        UNIX_EPOCH + Duration::from_secs(tick)
    }

    fn apply(
        domain: &mut DomainState,
        tick: u64,
        actor: &str,
        intent: &RoundIntent,
    ) -> Result<bool, ApplyError> {
        with_mutation_inputs(at(tick), Uuid::from_u128(tick.into()), actor, || {
            intent.apply(domain, actor)
        })
    }

    fn task(domain: &mut DomainState, id: u128) -> Uuid {
        let id = Uuid::from_u128(id);
        with_mutation_inputs(at(1), id, OWNER, || {
            domain.create_with_id(
                id,
                "task",
                None,
                TaskScope::Global,
                ProvenanceOrigin::Manual,
                None,
            )
        })
        .unwrap()
    }

    fn open_target(task: Uuid) -> OpenRoundTarget {
        OpenRoundTarget {
            task,
            from: HumanStatus::Open,
            expected_record: None,
        }
    }

    fn round(domain: &DomainState, task: Uuid) -> &Block {
        domain.get(task).unwrap().block.as_ref().unwrap()
    }

    #[test]
    fn edit_block_and_delete_reply_target_only_the_open_record() {
        let mut domain = DomainState::new();
        let task = task(&mut domain, 101);
        apply(
            &mut domain,
            2,
            OWNER,
            &RoundIntent::OpenBlock {
                targets: vec![open_target(task)],
                mode: OpenRoundMode::Cli,
                draft: BlockDraft::default(),
            },
        )
        .unwrap();
        let record = round(&domain, task).key();
        let edit = RoundIntent::EditBlock {
            task,
            record: record.clone(),
            patch: BlockIntentPatch {
                why: TextPatch::Set("waiting for a reply".into()),
                ..BlockIntentPatch::default()
            },
        };
        assert!(apply(&mut domain, 3, OWNER, &edit).unwrap());
        assert_eq!(
            round(&domain, task).why.as_deref(),
            Some("waiting for a reply")
        );
        assert!(!apply(&mut domain, 4, OWNER, &edit).unwrap());
        apply(
            &mut domain,
            5,
            OWNER,
            &RoundIntent::AddReply {
                task,
                record: record.clone(),
                text: "ready".into(),
            },
        )
        .unwrap();
        let reply = ReplyKey::of(&round(&domain, task).replies[0]);
        assert!(apply(
            &mut domain,
            6,
            OWNER,
            &RoundIntent::DeleteReply {
                task,
                record,
                reply,
            }
        )
        .unwrap());
        assert!(round(&domain, task).replies[0].deleted);
        assert_eq!(domain.undo_len(), 0);
    }

    #[test]
    fn board_open_and_set_on_prevalidate_and_keep_batch_undo() {
        let mut domain = DomainState::new();
        let first = task(&mut domain, 101);
        let second = task(&mut domain, 102);
        let mut open = RoundIntent::OpenBlock {
            targets: vec![
                open_target(first),
                OpenRoundTarget {
                    from: HumanStatus::Started,
                    ..open_target(second)
                },
            ],
            mode: OpenRoundMode::Board,
            draft: BlockDraft::default(),
        };
        assert_eq!(
            apply(&mut domain, 2, OWNER, &open),
            Err(ApplyError::StaleRound(second))
        );
        assert!(domain.get(first).unwrap().block.is_none());
        assert!(domain.last_undo().is_none());
        if let RoundIntent::OpenBlock { targets, .. } = &mut open {
            targets[1].from = HumanStatus::Open;
        }
        assert!(apply(&mut domain, 3, OWNER, &open).unwrap());
        assert!(
            matches!(domain.last_undo(), Some(UndoEntry::Batch { entries }) if entries.len() == 2)
        );

        let mut changes = [first, second]
            .map(|task| WaitingOnChange {
                task,
                record: round(&domain, task).key(),
                on: BlockOn::Other("  reviewer  ".into()),
            })
            .to_vec();
        changes[1].record.at = at(2);
        let set_on = RoundIntent::SetOn {
            changes: changes.clone(),
        };
        assert_eq!(
            apply(&mut domain, 4, OWNER, &set_on),
            Err(ApplyError::StaleRound(second))
        );
        assert_eq!(round(&domain, first).on, BlockOn::You);
        changes[1].record.at = at(3);
        assert!(apply(&mut domain, 5, OWNER, &RoundIntent::SetOn { changes }).unwrap());
        assert!(
            matches!(domain.last_undo(), Some(UndoEntry::Batch { entries }) if entries.len() == 2)
        );
        assert_eq!(round(&domain, first).on, BlockOn::Other("reviewer".into()));
        with_mutation_inputs(at(6), Uuid::from_u128(6), OWNER, || domain.undo()).unwrap();
        assert_eq!(round(&domain, first).on, BlockOn::You);
        assert_eq!(round(&domain, second).on, BlockOn::You);
    }

    #[test]
    fn review_wire_patch_retains_check_state_and_ambiguous_text_refuses() {
        let mut domain = DomainState::new();
        let task = task(&mut domain, 101);
        let open = RoundIntent::OpenReview {
            targets: vec![open_target(task)],
            mode: OpenRoundMode::Cli,
            draft: ReviewDraft {
                done: Some("summary".into()),
                next: Some("next".into()),
                checks: vec![Check {
                    text: "lint".into(),
                    state: CheckState::Passed,
                }],
                ..ReviewDraft::default()
            },
        };
        assert!(apply(&mut domain, 2, "builder", &open).unwrap());
        assert!(domain.last_undo().is_none());
        assert_eq!(round(&domain, task).checks[0].state, CheckState::Open);
        let record = round(&domain, task).key();
        let check = RoundIntent::SetCheck {
            task,
            record: record.clone(),
            check: "lint".into(),
            occurrence: 0,
            state: CheckState::Failed,
        };
        assert!(apply(&mut domain, 3, OWNER, &check).unwrap());
        let patch = RoundIntent::EditReview {
            task,
            record,
            patch: ReviewIntentPatch {
                done: TextPatch::Clear,
                checks: Some(vec![" test ".into(), "lint".into(), "test".into()]),
                ..ReviewIntentPatch::default()
            },
        };
        let decoded: RoundIntent =
            serde_json::from_str(&serde_json::to_string(&patch).unwrap()).unwrap();
        assert_eq!(patch, decoded);
        assert!(apply(&mut domain, 4, "builder", &decoded).unwrap());
        let block = round(&domain, task);
        assert_eq!(block.done, None);
        assert_eq!(block.next.as_deref(), Some("next"));
        assert_eq!(block.checks[1].state, CheckState::Failed);
        let key = block.key();
        let repeated = |occurrence| RoundIntent::SetCheck {
            task,
            record: key.clone(),
            check: "test".into(),
            occurrence,
            state: CheckState::Passed,
        };
        assert_eq!(
            apply(&mut domain, 5, OWNER, &repeated(2)),
            Err(ApplyError::InvalidIntent("unknown check"))
        );
        assert!(apply(&mut domain, 6, OWNER, &repeated(1)).unwrap());
        let checks = &round(&domain, task).checks;
        assert_eq!(checks[0].state, CheckState::Open);
        assert_eq!(checks[2].state, CheckState::Passed);
    }

    #[test]
    fn reply_delivery_follows_exact_text_and_retargets_undo_without_event() {
        let mut domain = DomainState::new();
        let task = task(&mut domain, 101);
        let open = RoundIntent::OpenBlock {
            targets: vec![open_target(task)],
            mode: OpenRoundMode::Board,
            draft: BlockDraft::default(),
        };
        assert!(apply(&mut domain, 2, OWNER, &open).unwrap());
        let record = round(&domain, task).key();
        let add = RoundIntent::AddReply {
            task,
            record: record.clone(),
            text: " old ".into(),
        };
        assert!(apply(&mut domain, 3, OWNER, &add).unwrap());
        let reply = ReplyKey::of(&round(&domain, task).replies[0]);
        let edit = RoundIntent::EditReply {
            task,
            record: record.clone(),
            reply: reply.clone(),
            text: "new".into(),
        };
        assert!(apply(&mut domain, 4, "builder", &edit).is_err());
        assert!(apply(&mut domain, 5, OWNER, &edit).unwrap());
        let marker = |sent_text: &str| RoundIntent::MarkReplySent {
            task,
            record: record.clone(),
            reply: reply.clone(),
            sent_text: sent_text.into(),
        };
        assert!(!apply(&mut domain, 6, OWNER, &marker("old")).unwrap());
        let set_on = RoundIntent::SetOn {
            changes: vec![WaitingOnChange {
                task,
                record: record.clone(),
                on: BlockOn::Other("reviewer".into()),
            }],
        };
        assert!(apply(&mut domain, 7, OWNER, &set_on).unwrap());
        let history_len = domain.get(task).unwrap().history.len();
        assert!(apply(&mut domain, 8, OWNER, &marker("new")).unwrap());
        assert_eq!(domain.get(task).unwrap().history.len(), history_len);
        assert!(round(&domain, task).replies[0].sent);
        with_mutation_inputs(at(9), Uuid::from_u128(9), OWNER, || domain.undo()).unwrap();
        assert_eq!(round(&domain, task).on, BlockOn::You);
        assert!(round(&domain, task).replies[0].sent);
    }
}
