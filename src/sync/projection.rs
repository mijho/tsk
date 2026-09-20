use std::collections::{BTreeSet, HashMap};

use uuid::Uuid;

use crate::domain::DomainState;

use super::protocol::{
    AssignedNumberKind, EventEnvelope, MutationResponse, SequencedEvent, SyncEventType,
    TaskChangeKind, ENVELOPE_VERSION, EVENT_SCHEMA_VERSION,
};
use super::validation::validate_replayed_state;

#[derive(Debug)]
pub struct ProjectionError(pub String);

impl std::fmt::Display for ProjectionError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl std::error::Error for ProjectionError {}

struct DedupeEntry {
    actor_id: String,
    content_hash: String,
    event_index: usize,
}

pub enum DedupeLookup {
    Missing,
    Match(Box<MutationResponse>),
    Reuse,
}

pub struct WorkspaceProjection {
    workspace: String,
    state: DomainState,
    next_sequence: u64,
    events: Vec<SequencedEvent>,
    dedupe: HashMap<Uuid, DedupeEntry>,
}

impl WorkspaceProjection {
    pub fn new(workspace: impl Into<String>) -> Self {
        Self {
            workspace: workspace.into(),
            state: DomainState::new(),
            next_sequence: 0,
            events: Vec::new(),
            dedupe: HashMap::new(),
        }
    }

    pub fn state(&self) -> &DomainState {
        &self.state
    }

    pub fn next_sequence(&self) -> u64 {
        self.next_sequence
    }

    pub fn apply_all(
        &mut self,
        events: impl IntoIterator<Item = SequencedEvent>,
    ) -> Result<(), ProjectionError> {
        for event in events {
            self.apply(event)?;
        }
        Ok(())
    }

    pub fn apply(&mut self, sequenced: SequencedEvent) -> Result<(), ProjectionError> {
        let event = &sequenced.event;
        if sequenced.sequence != self.next_sequence {
            return Err(ProjectionError(format!(
                "workspace {} expected sequence {}, found {}",
                self.workspace, self.next_sequence, sequenced.sequence
            )));
        }
        if event.envelope_version != ENVELOPE_VERSION
            || event.schema_version != EVENT_SCHEMA_VERSION
            || event.event_type != SyncEventType::StateMutation
        {
            return Err(ProjectionError(format!(
                "unsupported event version at sequence {}",
                sequenced.sequence
            )));
        }
        if event.workspace != self.workspace {
            return Err(ProjectionError(format!(
                "event workspace {} does not match stream workspace {}",
                event.workspace, self.workspace
            )));
        }
        if event.operation_id.is_nil() || event.actor_id.is_empty() || event.timestamp_unix_ms == 0
        {
            return Err(ProjectionError(format!(
                "event at sequence {} has invalid identity",
                sequenced.sequence
            )));
        }
        if event.content_hash.len() != 64
            || !event
                .content_hash
                .bytes()
                .all(|byte| byte.is_ascii_hexdigit())
        {
            return Err(ProjectionError(format!(
                "event at sequence {} has an invalid content hash",
                sequenced.sequence
            )));
        }
        if event.result.sequence != sequenced.sequence
            || event.result.next_sequence != sequenced.sequence.saturating_add(1)
        {
            return Err(ProjectionError(format!(
                "event at sequence {} has inconsistent result metadata",
                sequenced.sequence
            )));
        }
        if self.dedupe.contains_key(&event.operation_id) {
            return Err(ProjectionError(format!(
                "operation {} occurs more than once in the log",
                event.operation_id
            )));
        }
        validate_replayed_state(&event.state).map_err(|error| {
            ProjectionError(format!("invalid resulting state: {}", error.message))
        })?;
        self.validate_transition(event)?;

        let event_index = self.events.len();
        self.state = event.state.clone();
        self.next_sequence = event.result.next_sequence;
        self.dedupe.insert(
            event.operation_id,
            DedupeEntry {
                actor_id: event.actor_id.clone(),
                content_hash: event.content_hash.clone(),
                event_index,
            },
        );
        self.events.push(sequenced);
        Ok(())
    }

    pub fn lookup(&self, operation_id: Uuid, actor_id: &str, content_hash: &str) -> DedupeLookup {
        let Some(entry) = self.dedupe.get(&operation_id) else {
            return DedupeLookup::Missing;
        };
        if entry.actor_id != actor_id || entry.content_hash != content_hash {
            return DedupeLookup::Reuse;
        }
        let event = &self.events[entry.event_index].event;
        DedupeLookup::Match(Box::new(MutationResponse {
            workspace: event.workspace.clone(),
            operation_id: event.operation_id,
            content_hash: event.content_hash.clone(),
            result: event.result.clone(),
            state: event.state.clone(),
        }))
    }

    pub fn events_from(
        &self,
        from: u64,
        limit: usize,
    ) -> Result<Vec<SequencedEvent>, ProjectionError> {
        if from > self.next_sequence {
            return Err(ProjectionError(format!(
                "cursor {from} is ahead of next sequence {}",
                self.next_sequence
            )));
        }
        let start = usize::try_from(from)
            .map_err(|_| ProjectionError("event cursor does not fit this server".to_string()))?;
        Ok(self.events[start..].iter().take(limit).cloned().collect())
    }

    fn validate_transition(&self, event: &EventEnvelope) -> Result<(), ProjectionError> {
        let mut changed = BTreeSet::new();
        let mut created = BTreeSet::new();
        for change in &event.changes {
            if !changed.insert(change.task_id) {
                return Err(ProjectionError(format!(
                    "event repeats task {} in changes",
                    change.task_id
                )));
            }
            let before = self.state.get(change.task_id);
            let after = event.state.get(change.task_id).ok_or_else(|| {
                ProjectionError(format!(
                    "event result omits changed task {}",
                    change.task_id
                ))
            })?;
            if after.revision != change.result_revision
                || after.soft_deleted != change.result_tombstone
            {
                return Err(ProjectionError(format!(
                    "event result does not match change metadata for task {}",
                    change.task_id
                )));
            }
            match change.kind {
                TaskChangeKind::Create => {
                    if before.is_some()
                        || change.base_revision.is_some()
                        || change.base_tombstone
                        || change.result_tombstone
                    {
                        return Err(ProjectionError(format!(
                            "invalid create transition for task {}",
                            change.task_id
                        )));
                    }
                    created.insert(change.task_id);
                }
                TaskChangeKind::Update | TaskChangeKind::Delete | TaskChangeKind::Restore => {
                    let before = before.ok_or_else(|| {
                        ProjectionError(format!("event changes unknown task {}", change.task_id))
                    })?;
                    if change.base_revision != Some(before.revision)
                        || change.base_tombstone != before.soft_deleted
                        || change.result_revision == before.revision
                        || after.number != before.number
                        || after.notice != before.notice
                    {
                        return Err(ProjectionError(format!(
                            "event base/result does not match prior task {}",
                            change.task_id
                        )));
                    }
                    let valid_tombstones = match change.kind {
                        TaskChangeKind::Update => {
                            !change.base_tombstone && !change.result_tombstone
                        }
                        TaskChangeKind::Delete => !change.base_tombstone && change.result_tombstone,
                        TaskChangeKind::Restore => {
                            change.base_tombstone && !change.result_tombstone
                        }
                        TaskChangeKind::Create => unreachable!("handled above"),
                    };
                    if !valid_tombstones {
                        return Err(ProjectionError(format!(
                            "event has an invalid tombstone transition for task {}",
                            change.task_id
                        )));
                    }
                }
            }
        }
        for previous in self.state.tasks() {
            if !changed.contains(&previous.id) {
                let resulting = event.state.get(previous.id).ok_or_else(|| {
                    ProjectionError(format!("event dropped unchanged task {}", previous.id))
                })?;
                if resulting != previous {
                    return Err(ProjectionError(format!(
                        "event modified undeclared task {}",
                        previous.id
                    )));
                }
            }
        }
        for resulting in event.state.tasks() {
            if self.state.get(resulting.id).is_none() && !changed.contains(&resulting.id) {
                return Err(ProjectionError(format!(
                    "event added undeclared task {}",
                    resulting.id
                )));
            }
        }
        self.validate_assignments(event, &created)?;
        if event.state.projects() != self.state.projects() {
            return Err(ProjectionError(
                "project-record mutations are not supported by this protocol version".to_string(),
            ));
        }
        Ok(())
    }

    fn validate_assignments(
        &self,
        event: &EventEnvelope,
        created: &BTreeSet<Uuid>,
    ) -> Result<(), ProjectionError> {
        let mut assigned = BTreeSet::new();
        for assignment in &event.result.assigned_numbers {
            if !assigned.insert(assignment.task_id) || !created.contains(&assignment.task_id) {
                return Err(ProjectionError(format!(
                    "invalid number assignment for task {}",
                    assignment.task_id
                )));
            }
            let task = event
                .state
                .get(assignment.task_id)
                .expect("created task was checked in resulting state");
            let matches = match assignment.kind {
                AssignedNumberKind::Task => {
                    task.notice.is_none() && task.number == Some(assignment.number)
                }
                AssignedNumberKind::Notice => {
                    task.number.is_none()
                        && task
                            .notice
                            .as_ref()
                            .is_some_and(|notice| notice.number == Some(assignment.number))
                }
            };
            if !matches {
                return Err(ProjectionError(format!(
                    "number assignment does not match task {}",
                    assignment.task_id
                )));
            }
        }
        if assigned != *created {
            return Err(ProjectionError(
                "every created task must have exactly one assigned number".to_string(),
            ));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::{ProvenanceOrigin, TaskScope};
    use crate::sync::protocol::{AssignedNumber, EntityChange, MutationResult};
    use std::time::{SystemTime, UNIX_EPOCH};

    fn creation_event(workspace: &str, operation_id: Uuid, hash: char) -> SequencedEvent {
        let mut state = DomainState::new();
        let task_id = state
            .create(
                "replayed",
                None,
                TaskScope::Global,
                ProvenanceOrigin::Manual,
                None,
            )
            .expect("create");
        let revision = state.get(task_id).unwrap().revision;
        state.finalize_for_sync();
        let number = state.get(task_id).unwrap().number.unwrap();
        SequencedEvent {
            sequence: 0,
            event: EventEnvelope {
                envelope_version: ENVELOPE_VERSION,
                workspace: workspace.to_string(),
                operation_id,
                actor_id: "alice".to_string(),
                event_type: SyncEventType::StateMutation,
                schema_version: EVENT_SCHEMA_VERSION,
                changes: vec![EntityChange {
                    task_id,
                    kind: TaskChangeKind::Create,
                    base_revision: None,
                    result_revision: revision,
                    base_tombstone: false,
                    result_tombstone: false,
                }],
                content_hash: std::iter::repeat_n(hash, 64).collect(),
                state,
                result: MutationResult {
                    sequence: 0,
                    next_sequence: 1,
                    assigned_numbers: vec![AssignedNumber {
                        task_id,
                        kind: AssignedNumberKind::Task,
                        number,
                    }],
                },
                timestamp_unix_ms: SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap()
                    .as_millis() as u64,
            },
        }
    }

    #[test]
    fn replay_rebuilds_projection_and_dedupe_result() {
        let operation_id = Uuid::new_v4();
        let event = creation_event("demo", operation_id, 'a');
        let expected_state = serde_json::to_value(&event.event.state).unwrap();
        let mut projection = WorkspaceProjection::new("demo");
        projection.apply(event).expect("replay");
        assert_eq!(projection.next_sequence(), 1);
        match projection.lookup(operation_id, "alice", &"a".repeat(64)) {
            DedupeLookup::Match(response) => {
                assert_eq!(
                    serde_json::to_value(response.state).unwrap(),
                    expected_state
                )
            }
            _ => panic!("expected dedupe hit"),
        }
    }

    #[test]
    fn replay_rejects_noncanonical_number_counters() {
        let mut event = creation_event("demo", Uuid::new_v4(), 'a');
        event.event.state.next_task_number = 99;
        let mut projection = WorkspaceProjection::new("demo");
        assert!(projection.apply(event).is_err());
    }

    #[test]
    fn operation_id_reuse_with_different_content_is_rejected() {
        let operation_id = Uuid::new_v4();
        let mut projection = WorkspaceProjection::new("demo");
        projection
            .apply(creation_event("demo", operation_id, 'a'))
            .expect("replay");
        assert!(matches!(
            projection.lookup(operation_id, "alice", &"b".repeat(64)),
            DedupeLookup::Reuse
        ));
    }
}
