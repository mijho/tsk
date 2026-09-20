use std::collections::{BTreeMap, BTreeSet};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::Serialize;
use sha2::{Digest, Sha256};
use uuid::Uuid;

use crate::domain::{DomainState, Task, TaskEventKind, STORE_FORMAT_VERSION};

use super::protocol::{
    AssignedNumber, AssignedNumberKind, EntityChange, ExpectedTask, ExpectedTaskVersion,
    MutationRequest, ProposedTaskChange, TaskChangeKind,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ValidationKind {
    Invalid,
    Conflict,
}

#[derive(Debug)]
pub struct ValidationError {
    pub kind: ValidationKind,
    pub message: String,
}

impl ValidationError {
    fn invalid(message: impl Into<String>) -> Self {
        Self {
            kind: ValidationKind::Invalid,
            message: message.into(),
        }
    }

    fn conflict(message: impl Into<String>) -> Self {
        Self {
            kind: ValidationKind::Conflict,
            message: message.into(),
        }
    }
}

#[derive(Debug)]
pub struct ValidatedMutation {
    pub state: DomainState,
    pub changes: Vec<EntityChange>,
    pub assigned_numbers: Vec<AssignedNumber>,
}

#[derive(Serialize)]
struct HashInput<'a> {
    workspace: &'a str,
    actor_id: &'a str,
    expected: Vec<&'a ExpectedTask>,
    changes: Vec<&'a ProposedTaskChange>,
    proposed_state: &'a DomainState,
}

pub fn mutation_content_hash(
    workspace: &str,
    actor_id: &str,
    request: &MutationRequest,
) -> Result<String, ValidationError> {
    let mut expected = request.expected.iter().collect::<Vec<_>>();
    expected.sort_by_key(|guard| guard.task_id);
    let mut changes = request.changes.iter().collect::<Vec<_>>();
    changes.sort_by_key(|change| change.task_id);
    let bytes = serde_json::to_vec(&HashInput {
        workspace,
        actor_id,
        expected,
        changes,
        proposed_state: &request.proposed_state,
    })
    .map_err(|error| ValidationError::invalid(format!("cannot hash mutation: {error}")))?;
    let digest = Sha256::digest(bytes);
    Ok(digest.iter().map(|byte| format!("{byte:02x}")).collect())
}

pub fn validate_mutation(
    authoritative: &DomainState,
    request: &MutationRequest,
) -> Result<ValidatedMutation, ValidationError> {
    if request.operation_id.is_nil() {
        return Err(ValidationError::invalid("operation_id must not be nil"));
    }
    validate_state_shape(&request.proposed_state, false)?;
    let accepted_at = SystemTime::now();

    let expected = unique_expected(&request.expected)?;
    let changes = unique_changes(&request.changes)?;
    if changes.is_empty() {
        return Err(ValidationError::invalid(
            "mutation must declare at least one task change",
        ));
    }
    let expected_ids = expected.keys().copied().collect::<BTreeSet<_>>();
    let changed_ids = changes.keys().copied().collect::<BTreeSet<_>>();
    if expected_ids != changed_ids {
        return Err(ValidationError::invalid(
            "expected and changes must contain exactly the same task IDs",
        ));
    }

    for proposed in request.proposed_state.tasks() {
        if authoritative.get(proposed.id).is_none() && !changed_ids.contains(&proposed.id) {
            return Err(ValidationError::invalid(format!(
                "new task {} is not declared in changes",
                proposed.id
            )));
        }
    }

    let mut replacements = Vec::with_capacity(changes.len());
    let mut entity_changes = Vec::with_capacity(changes.len());
    for (task_id, metadata) in changes {
        let guard = expected
            .get(&task_id)
            .expect("guard and change key sets matched");
        let proposed = request.proposed_state.get(task_id).ok_or_else(|| {
            ValidationError::invalid(format!(
                "changed task {task_id} is absent from proposed_state"
            ))
        })?;
        if proposed.revision != metadata.result_revision {
            return Err(ValidationError::invalid(format!(
                "result_revision does not match proposed task {task_id}"
            )));
        }

        let current = authoritative.get(task_id);
        let entity = match (current, &guard.version) {
            (None, ExpectedTaskVersion::Absent) => {
                validate_creation(proposed, metadata)?;
                EntityChange {
                    task_id,
                    kind: TaskChangeKind::Create,
                    base_revision: None,
                    result_revision: proposed.revision,
                    base_tombstone: false,
                    result_tombstone: proposed.soft_deleted,
                }
            }
            (None, ExpectedTaskVersion::Revision { .. }) => {
                return Err(ValidationError::conflict(format!(
                    "task {task_id} no longer exists"
                )));
            }
            (Some(_), ExpectedTaskVersion::Absent) => {
                return Err(ValidationError::conflict(format!(
                    "task {task_id} already exists"
                )));
            }
            (
                Some(current),
                ExpectedTaskVersion::Revision {
                    revision,
                    tombstone,
                },
            ) => validate_existing(current, proposed, metadata, *revision, *tombstone)?,
        };
        let mut replacement = proposed.clone();
        normalize_timestamps(&mut replacement, current, accepted_at);
        replacements.push(replacement);
        entity_changes.push(entity);
    }

    let new_ids = entity_changes
        .iter()
        .filter(|change| change.kind == TaskChangeKind::Create)
        .map(|change| change.task_id)
        .collect::<Vec<_>>();
    ensure_number_capacity(authoritative, &new_ids, &request.proposed_state)?;
    let mut state = authoritative.clone();
    state.apply_sync_tasks(replacements);
    state.finalize_for_sync();

    let mut assigned_numbers = Vec::new();
    for task_id in new_ids {
        let task = state
            .get(task_id)
            .expect("validated replacement remains in resulting state");
        if let Some(notice) = &task.notice {
            assigned_numbers.push(AssignedNumber {
                task_id,
                kind: AssignedNumberKind::Notice,
                number: notice
                    .number
                    .expect("sync finalization assigns notice numbers"),
            });
        } else {
            assigned_numbers.push(AssignedNumber {
                task_id,
                kind: AssignedNumberKind::Task,
                number: task.number.expect("sync finalization assigns task numbers"),
            });
        }
    }
    entity_changes.sort_by_key(|change| change.task_id);
    assigned_numbers.sort_by_key(|assignment| assignment.task_id);
    Ok(ValidatedMutation {
        state,
        changes: entity_changes,
        assigned_numbers,
    })
}

pub fn validate_replayed_state(state: &DomainState) -> Result<(), ValidationError> {
    validate_state_shape(state, true)
}

fn unique_expected(
    values: &[ExpectedTask],
) -> Result<BTreeMap<Uuid, &ExpectedTask>, ValidationError> {
    let mut map = BTreeMap::new();
    for value in values {
        if map.insert(value.task_id, value).is_some() {
            return Err(ValidationError::invalid(format!(
                "duplicate expected guard for task {}",
                value.task_id
            )));
        }
    }
    Ok(map)
}

fn unique_changes(
    values: &[ProposedTaskChange],
) -> Result<BTreeMap<Uuid, &ProposedTaskChange>, ValidationError> {
    let mut map = BTreeMap::new();
    for value in values {
        if map.insert(value.task_id, value).is_some() {
            return Err(ValidationError::invalid(format!(
                "duplicate change metadata for task {}",
                value.task_id
            )));
        }
    }
    Ok(map)
}

fn validate_creation(
    proposed: &Task,
    metadata: &ProposedTaskChange,
) -> Result<(), ValidationError> {
    if metadata.kind != TaskChangeKind::Create || metadata.base_revision.is_some() {
        return Err(ValidationError::invalid(format!(
            "new task {} must use create metadata with no base revision",
            proposed.id
        )));
    }
    if proposed.merge_base_revision.is_some() {
        return Err(ValidationError::invalid(format!(
            "new task {} cannot have a merge base",
            proposed.id
        )));
    }
    if proposed.soft_deleted {
        return Err(ValidationError::invalid(format!(
            "new task {} cannot begin as a tombstone",
            proposed.id
        )));
    }
    if proposed.number.is_some()
        || proposed
            .notice
            .as_ref()
            .is_some_and(|notice| notice.number.is_some())
    {
        return Err(ValidationError::invalid(format!(
            "new task {} must not choose its own number",
            proposed.id
        )));
    }
    if proposed
        .history
        .first()
        .is_none_or(|event| event.kind != TaskEventKind::Created)
    {
        return Err(ValidationError::invalid(format!(
            "new task {} must begin with a created event",
            proposed.id
        )));
    }
    Ok(())
}

fn validate_existing(
    current: &Task,
    proposed: &Task,
    metadata: &ProposedTaskChange,
    expected_revision: Uuid,
    expected_tombstone: bool,
) -> Result<EntityChange, ValidationError> {
    if current.revision != expected_revision || current.soft_deleted != expected_tombstone {
        return Err(ValidationError::conflict(format!(
            "task {} changed since the client snapshot",
            current.id
        )));
    }
    if metadata.base_revision != Some(current.revision) {
        return Err(ValidationError::invalid(format!(
            "base_revision does not match task {}",
            current.id
        )));
    }
    if proposed.revision == current.revision || proposed.revision.is_nil() {
        return Err(ValidationError::invalid(format!(
            "task {} must advance to a new non-nil revision",
            current.id
        )));
    }
    if proposed.merge_base_revision.is_some()
        && proposed.merge_base_revision != Some(current.revision)
    {
        return Err(ValidationError::invalid(format!(
            "task {} has an unrelated merge base",
            current.id
        )));
    }
    if proposed.number != current.number || proposed.notice != current.notice {
        return Err(ValidationError::invalid(format!(
            "task {} cannot change its assigned number or notice identity",
            current.id
        )));
    }
    if proposed.created_at != current.created_at || proposed.provenance != current.provenance {
        return Err(ValidationError::invalid(format!(
            "task {} cannot change creation identity",
            current.id
        )));
    }
    if proposed.history.len() <= current.history.len()
        || !proposed.history.starts_with(&current.history)
    {
        return Err(ValidationError::invalid(format!(
            "task {} history must append to the authoritative history",
            current.id
        )));
    }

    let derived_kind = match (current.soft_deleted, proposed.soft_deleted) {
        (false, true) => TaskChangeKind::Delete,
        (true, false) => TaskChangeKind::Restore,
        (false, false) => TaskChangeKind::Update,
        (true, true) => {
            return Err(ValidationError::conflict(format!(
                "task {} is deleted and must be restored before editing",
                current.id
            )));
        }
    };
    if metadata.kind != derived_kind {
        return Err(ValidationError::invalid(format!(
            "change kind for task {} does not match its tombstone transition",
            current.id
        )));
    }
    if matches!(
        derived_kind,
        TaskChangeKind::Delete | TaskChangeKind::Restore
    ) && !only_tombstone_changed(current, proposed)
    {
        return Err(ValidationError::invalid(format!(
            "delete/restore for task {} cannot edit other task fields",
            current.id
        )));
    }
    let required_event = match derived_kind {
        TaskChangeKind::Delete => Some(TaskEventKind::SoftDeleted),
        TaskChangeKind::Restore => Some(TaskEventKind::Restored),
        TaskChangeKind::Create | TaskChangeKind::Update => None,
    };
    if required_event.is_some_and(|kind| {
        !proposed.history[current.history.len()..]
            .iter()
            .any(|event| event.kind == kind)
    }) {
        return Err(ValidationError::invalid(format!(
            "task {} history does not describe its tombstone transition",
            current.id
        )));
    }

    Ok(EntityChange {
        task_id: current.id,
        kind: derived_kind,
        base_revision: Some(current.revision),
        result_revision: proposed.revision,
        base_tombstone: current.soft_deleted,
        result_tombstone: proposed.soft_deleted,
    })
}

fn only_tombstone_changed(current: &Task, proposed: &Task) -> bool {
    let mut normalized = proposed.clone();
    normalized.revision = current.revision;
    normalized.merge_base_revision = current.merge_base_revision;
    normalized.soft_deleted = current.soft_deleted;
    normalized.updated_at = current.updated_at;
    normalized.history.clone_from(&current.history);
    normalized == *current
}

fn normalize_timestamps(task: &mut Task, current: Option<&Task>, accepted_at: SystemTime) {
    match current {
        Some(current) => {
            let accepted_at = accepted_at.max(current.updated_at);
            for event in &mut task.history[current.history.len()..] {
                event.at = accepted_at;
            }
            task.updated_at = accepted_at;
        }
        None => {
            task.created_at = accepted_at;
            task.updated_at = accepted_at;
            for event in &mut task.history {
                event.at = accepted_at;
            }
        }
    }
}

fn ensure_number_capacity(
    authoritative: &DomainState,
    new_ids: &[Uuid],
    proposed: &DomainState,
) -> Result<(), ValidationError> {
    let notices = new_ids
        .iter()
        .filter(|id| proposed.get(**id).is_some_and(Task::is_notice))
        .count();
    let tasks = new_ids.len() - notices;
    let tasks = u64::try_from(tasks)
        .map_err(|_| ValidationError::invalid("too many new tasks in one mutation"))?;
    let notices = u64::try_from(notices)
        .map_err(|_| ValidationError::invalid("too many new notices in one mutation"))?;
    authoritative
        .next_task_number
        .checked_add(tasks)
        .ok_or_else(|| ValidationError::invalid("task number space is exhausted"))?;
    authoritative
        .next_notice_number
        .checked_add(notices)
        .ok_or_else(|| ValidationError::invalid("notice number space is exhausted"))?;
    Ok(())
}

fn validate_state_shape(
    state: &DomainState,
    require_assigned_numbers: bool,
) -> Result<(), ValidationError> {
    if state.format_version() != STORE_FORMAT_VERSION {
        return Err(ValidationError::invalid(format!(
            "unsupported DomainState format version {}",
            state.format_version()
        )));
    }
    let mut task_ids = BTreeSet::new();
    let mut task_numbers = BTreeSet::new();
    let mut notice_numbers = BTreeSet::new();
    for task in state.tasks() {
        if task.id.is_nil() || !task_ids.insert(task.id) {
            return Err(ValidationError::invalid(
                "task IDs must be unique and non-nil",
            ));
        }
        if task.revision.is_nil() {
            return Err(ValidationError::invalid(format!(
                "task {} has a nil revision",
                task.id
            )));
        }
        if require_assigned_numbers && task.merge_base_revision.is_some() {
            return Err(ValidationError::invalid(format!(
                "task {} retained a client merge base",
                task.id
            )));
        }
        if task.title.is_empty() || task.title.trim() != task.title {
            return Err(ValidationError::invalid(format!(
                "task {} title must be non-empty and trimmed",
                task.id
            )));
        }
        if require_assigned_numbers && task.updated_at < task.created_at {
            return Err(ValidationError::invalid(format!(
                "task {} updated_at predates created_at",
                task.id
            )));
        }
        validate_history(task, require_assigned_numbers)?;
        let mut step_ids = BTreeSet::new();
        for step in &task.steps {
            if step.id.is_nil() || !step_ids.insert(step.id) {
                return Err(ValidationError::invalid(format!(
                    "task {} step IDs must be unique and non-nil",
                    task.id
                )));
            }
            if step.text.is_empty() || step.text.trim() != step.text {
                return Err(ValidationError::invalid(format!(
                    "task {} step text must be non-empty and trimmed",
                    task.id
                )));
            }
        }
        match &task.notice {
            Some(notice) => {
                if task.number.is_some() {
                    return Err(ValidationError::invalid(format!(
                        "notice task {} cannot also have a task number",
                        task.id
                    )));
                }
                if let Some(number) = notice.number {
                    if number == 0 || !notice_numbers.insert(number) {
                        return Err(ValidationError::invalid(
                            "notice numbers must be positive and unique",
                        ));
                    }
                } else if require_assigned_numbers {
                    return Err(ValidationError::invalid(format!(
                        "notice task {} has no assigned number",
                        task.id
                    )));
                }
            }
            None => {
                if let Some(number) = task.number {
                    if number == 0 || !task_numbers.insert(number) {
                        return Err(ValidationError::invalid(
                            "task numbers must be positive and unique",
                        ));
                    }
                } else if require_assigned_numbers {
                    return Err(ValidationError::invalid(format!(
                        "task {} has no assigned number",
                        task.id
                    )));
                }
            }
        }
    }
    if require_assigned_numbers {
        validate_sync_metadata(state, &task_numbers, &notice_numbers)?;
    }
    Ok(())
}

fn validate_sync_metadata(
    state: &DomainState,
    task_numbers: &BTreeSet<u64>,
    notice_numbers: &BTreeSet<u64>,
) -> Result<(), ValidationError> {
    let next_task = task_numbers
        .last()
        .copied()
        .unwrap_or(0)
        .checked_add(1)
        .ok_or_else(|| ValidationError::invalid("task number space is exhausted"))?;
    let next_notice = notice_numbers
        .last()
        .copied()
        .unwrap_or(0)
        .checked_add(1)
        .ok_or_else(|| ValidationError::invalid("notice number space is exhausted"))?;
    if state.next_task_number != next_task || state.next_notice_number != next_notice {
        return Err(ValidationError::invalid(
            "next-number counters are not canonical",
        ));
    }
    let document = serde_json::to_value(state)
        .map_err(|error| ValidationError::invalid(format!("cannot inspect state: {error}")))?;
    if document
        .get("undo_stack")
        .and_then(serde_json::Value::as_array)
        .is_none_or(|entries| !entries.is_empty())
    {
        return Err(ValidationError::invalid(
            "shared authoritative state must not contain local undo entries",
        ));
    }
    Ok(())
}

fn validate_history(
    task: &Task,
    require_canonical_timestamps: bool,
) -> Result<(), ValidationError> {
    if task.history.is_empty() || task.history[0].kind != TaskEventKind::Created {
        return Err(ValidationError::invalid(format!(
            "task {} history must begin with created",
            task.id
        )));
    }
    if task
        .history
        .iter()
        .skip(1)
        .any(|event| event.kind == TaskEventKind::Created)
    {
        return Err(ValidationError::invalid(format!(
            "task {} history repeats the created event",
            task.id
        )));
    }
    if !require_canonical_timestamps {
        return Ok(());
    }
    let mut previous = UNIX_EPOCH;
    for event in &task.history {
        if event.at < task.created_at || event.at < previous || event.at > task.updated_at {
            return Err(ValidationError::invalid(format!(
                "task {} history timestamps are inconsistent",
                task.id
            )));
        }
        previous = event.at;
    }
    Ok(())
}

pub fn unix_millis(now: SystemTime) -> Result<u64, ValidationError> {
    let millis = now
        .duration_since(UNIX_EPOCH)
        .map_err(|_| ValidationError::invalid("system clock is before Unix epoch"))?
        .as_millis();
    u64::try_from(millis).map_err(|_| ValidationError::invalid("timestamp is out of range"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::{ProvenanceOrigin, TaskScope};
    use std::time::Duration;

    fn numbered_state(title: &str) -> (DomainState, Uuid) {
        let mut state = DomainState::new();
        let id = state
            .create(
                title,
                None,
                TaskScope::Global,
                ProvenanceOrigin::Manual,
                None,
            )
            .expect("create");
        state.finalize_for_sync();
        (state, id)
    }

    fn request_for(
        base: &DomainState,
        id: Uuid,
        proposed: DomainState,
        kind: TaskChangeKind,
    ) -> MutationRequest {
        let current = base.get(id).expect("base task");
        let result = proposed.get(id).expect("proposed task");
        MutationRequest {
            operation_id: Uuid::new_v4(),
            expected: vec![ExpectedTask {
                task_id: id,
                version: ExpectedTaskVersion::Revision {
                    revision: current.revision,
                    tombstone: current.soft_deleted,
                },
            }],
            changes: vec![ProposedTaskChange {
                task_id: id,
                kind,
                base_revision: Some(current.revision),
                result_revision: result.revision,
            }],
            proposed_state: proposed,
        }
    }

    #[test]
    fn stale_sibling_state_is_ignored_without_losing_authoritative_changes() {
        let (base, first) = numbered_state("first");
        let mut authoritative = base.clone();
        let second = authoritative
            .create(
                "concurrent",
                None,
                TaskScope::Global,
                ProvenanceOrigin::Manual,
                None,
            )
            .expect("create concurrent");
        authoritative.finalize_for_sync();

        let mut proposed = base.clone();
        proposed
            .edit(first, "edited", None, TaskScope::Global, None)
            .expect("edit");
        let request = request_for(&base, first, proposed, TaskChangeKind::Update);
        let validated = validate_mutation(&authoritative, &request).expect("sibling merge");
        assert_eq!(validated.state.get(first).unwrap().title, "edited");
        assert_eq!(validated.state.get(second).unwrap().title, "concurrent");
    }

    #[test]
    fn stale_same_task_edit_is_a_conflict() {
        let (base, id) = numbered_state("base");
        let mut authoritative = base.clone();
        authoritative
            .edit(id, "winner", None, TaskScope::Global, None)
            .expect("winner");
        authoritative.finalize_for_sync();
        let mut proposed = base.clone();
        proposed
            .edit(id, "loser", None, TaskScope::Global, None)
            .expect("loser");
        let error = validate_mutation(
            &authoritative,
            &request_for(&base, id, proposed, TaskChangeKind::Update),
        )
        .expect_err("must conflict");
        assert_eq!(error.kind, ValidationKind::Conflict);
    }

    #[test]
    fn stale_resurrection_after_delete_is_a_conflict() {
        let (base, id) = numbered_state("base");
        let mut deleted = base.clone();
        deleted.soft_delete(id).expect("delete");
        deleted.finalize_for_sync();
        let mut stale = base.clone();
        stale
            .edit(id, "stale edit", None, TaskScope::Global, None)
            .expect("edit");
        let error = validate_mutation(
            &deleted,
            &request_for(&base, id, stale, TaskChangeKind::Update),
        )
        .expect_err("deleted revision wins");
        assert_eq!(error.kind, ValidationKind::Conflict);
    }

    #[test]
    fn server_normalizes_untrusted_client_timestamps() {
        let (base, id) = numbered_state("base");
        let mut proposed = base.clone();
        proposed
            .edit(id, "edited", None, TaskScope::Global, None)
            .expect("edit");
        let far_future = SystemTime::now() + Duration::from_secs(50 * 365 * 24 * 60 * 60);
        let far = far_future.duration_since(UNIX_EPOCH).unwrap();
        let mut document = serde_json::to_value(proposed).unwrap();
        let timestamp = serde_json::json!([far.as_secs(), far.subsec_nanos()]);
        document["tasks"][0]["updated_at"] = timestamp.clone();
        let history = document["tasks"][0]["history"].as_array_mut().unwrap();
        history.last_mut().unwrap()["at"] = timestamp;
        let proposed: DomainState = serde_json::from_value(document).unwrap();
        let validated = validate_mutation(
            &base,
            &request_for(&base, id, proposed, TaskChangeKind::Update),
        )
        .expect("timestamps are server-owned");
        let accepted = validated.state.get(id).unwrap();
        assert!(accepted.updated_at < far_future);
        assert_eq!(accepted.history.last().unwrap().at, accepted.updated_at);
    }

    #[test]
    fn exhausted_number_space_is_rejected_without_panicking() {
        let mut authoritative = DomainState::new();
        authoritative.next_task_number = u64::MAX;
        let mut proposed = authoritative.clone();
        let id = proposed
            .create(
                "cannot number",
                None,
                TaskScope::Global,
                ProvenanceOrigin::Manual,
                None,
            )
            .expect("create");
        let revision = proposed.get(id).unwrap().revision;
        let request = MutationRequest {
            operation_id: Uuid::new_v4(),
            expected: vec![ExpectedTask {
                task_id: id,
                version: ExpectedTaskVersion::Absent,
            }],
            changes: vec![ProposedTaskChange {
                task_id: id,
                kind: TaskChangeKind::Create,
                base_revision: None,
                result_revision: revision,
            }],
            proposed_state: proposed,
        };
        let error = validate_mutation(&authoritative, &request).expect_err("number exhaustion");
        assert!(error.message.contains("exhausted"));
    }

    #[test]
    fn server_allocates_new_numbers() {
        let authoritative = DomainState::new();
        let mut proposed = authoritative.clone();
        let id = proposed
            .create(
                "new",
                None,
                TaskScope::Global,
                ProvenanceOrigin::Manual,
                None,
            )
            .expect("create");
        let revision = proposed.get(id).unwrap().revision;
        let request = MutationRequest {
            operation_id: Uuid::new_v4(),
            expected: vec![ExpectedTask {
                task_id: id,
                version: ExpectedTaskVersion::Absent,
            }],
            changes: vec![ProposedTaskChange {
                task_id: id,
                kind: TaskChangeKind::Create,
                base_revision: None,
                result_revision: revision,
            }],
            proposed_state: proposed,
        };
        let validated = validate_mutation(&authoritative, &request).expect("valid create");
        assert_eq!(validated.state.get(id).unwrap().number, Some(1));
        assert_eq!(validated.assigned_numbers[0].number, 1);
    }
}
