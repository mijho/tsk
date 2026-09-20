//! Public JSON protocol shared by `tsk-sync` and future remote clients.

use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::domain::DomainState;

pub const ENVELOPE_VERSION: u32 = 1;
pub const EVENT_SCHEMA_VERSION: u32 = 1;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SnapshotResponse {
    pub workspace: String,
    /// Exact sequence number at which the next event will be appended.
    pub next_sequence: u64,
    pub state: DomainState,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MutationRequest {
    pub operation_id: Uuid,
    /// One guard for every task declared in `changes`.
    pub expected: Vec<ExpectedTask>,
    /// Explicit metadata for every task this operation intends to change.
    pub changes: Vec<ProposedTaskChange>,
    /// The client's complete proposed state. The server extracts only declared, validated
    /// changes and overlays them on the latest authoritative state.
    pub proposed_state: DomainState,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExpectedTask {
    pub task_id: Uuid,
    pub version: ExpectedTaskVersion,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum ExpectedTaskVersion {
    Absent,
    Revision { revision: Uuid, tombstone: bool },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProposedTaskChange {
    pub task_id: Uuid,
    pub kind: TaskChangeKind,
    pub base_revision: Option<Uuid>,
    pub result_revision: Uuid,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskChangeKind {
    Create,
    Update,
    Delete,
    Restore,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MutationResponse {
    pub workspace: String,
    pub operation_id: Uuid,
    pub content_hash: String,
    pub result: MutationResult,
    pub state: DomainState,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MutationResult {
    /// Sequence number occupied by the accepted event.
    pub sequence: u64,
    /// Exact sequence number at which the next event will be appended.
    pub next_sequence: u64,
    pub assigned_numbers: Vec<AssignedNumber>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AssignedNumber {
    pub task_id: Uuid,
    pub kind: AssignedNumberKind,
    pub number: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AssignedNumberKind {
    Task,
    Notice,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EventEnvelope {
    pub envelope_version: u32,
    pub workspace: String,
    pub operation_id: Uuid,
    pub actor_id: String,
    pub event_type: SyncEventType,
    pub schema_version: u32,
    pub changes: Vec<EntityChange>,
    /// SHA-256 of the authenticated workspace, actor, and mutation content.
    pub content_hash: String,
    /// Full authoritative state after this event, for deterministic replay.
    pub state: DomainState,
    pub result: MutationResult,
    pub timestamp_unix_ms: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SyncEventType {
    StateMutation,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EntityChange {
    pub task_id: Uuid,
    pub kind: TaskChangeKind,
    pub base_revision: Option<Uuid>,
    pub result_revision: Uuid,
    pub base_tombstone: bool,
    pub result_tombstone: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SequencedEvent {
    pub sequence: u64,
    pub event: EventEnvelope,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EventsResponse {
    pub workspace: String,
    pub events: Vec<SequencedEvent>,
    /// Resume from this exact sequence on the next request.
    pub next_sequence: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorCode {
    Unauthorized,
    Forbidden,
    InvalidRequest,
    Conflict,
    OperationReuse,
    Unknown,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ErrorBody {
    pub code: ErrorCode,
    pub message: String,
    pub retryable: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HealthResponse {
    pub status: String,
}
