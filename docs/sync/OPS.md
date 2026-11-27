# Domain op catalogue

Each op records intent, not a proposed task or store. This is the phase-0 inventory; conflict rules follow [PLAN.md](PLAN.md). Names here are for the domain, not new CLI verbs.

## Replay contract

- An envelope carries an op ID, actor, and time. Authoritative apply must supply identities, timestamps, and revisions to domain code instead of generating them during replay. A create carries its task ID; new steps and records need stable IDs too.
- Resolve task numbers, short step IDs, check and reply indexes, project paths, and agent names at the input boundary. Target by stable ID in the op. For order changes carry an anchor ID (or start/end), not an index into a possibly changed list.
- A patch distinguishes omitted, set, and cleared fields. An append to notes appends to the current notes; do not turn it into a stale replacement. Compare status with `from` and flag mismatches; keep overwritten scalar values in the paper trail.
- `Batch(ordered intents)` preserves the marked board verb's prevalidation, atomicity, and one undo entry. A dispatching start batch is the exception: each successful external launch lands individually. A transaction with no undo today does not gain local undo in phase 0.

## Tasks and lifecycle

| Op and payload | Current entry points | Conflict and undo boundary |
| --- | --- | --- |
| `Create(task, scope, provenance, title, notes, thread, assignee, base, due, after, steps)` | CLI add; board quick add and expanded capture: `create_configured`, `set_due_on_create`, `set_after_on_create`, `add_step` | Distinct IDs coexist; number is assigned at save/server, not chosen by caller. Preserve capture provenance and include initial fields and steps in the create. Neither board capture path adds undo. |
| `EditTask(task, field patch, step intents)` | CLI edit: `edit_with_assignee_and_base`, `edit_due`, `edit_after`, `set_on`; board form: `edit_with_step_changes_and_base`, staged due and after | Field-level last arrival for title, notes, thread, assignee, due, base. Stable step IDs. Only staged step changes get board form undo. Keep CLI edits atomic and preserve `SetOn` undo as described below. Scope is checked at the boundary, not silently remapped. |
| `SetAssignee`, `SetBase`, `SetDue` (task, value or clear) | Board actions and CLI edit; `assign_batch`, `set_base_batch`, `edit_due` | Scalar last arrival with history. Marked actions keep a single batch undo. Any automatic unhide on assignment stays in the same change. |
| `SetAfter(task, prerequisite IDs)`, `ChainAfter(ordered task IDs)` | CLI edit and create; board after picker and chain; `set_after_batch`, `chain_after` | Validate existence, self-links and cycles against current state. Keep chain edges and undo atomic. Local code still resolves numbers until T7. |
| `SetStatus(task, from, to)`, `Complete(task, from)` | CLI status, board status/start/done paths; `set_status_by`, `change_status`, `start_batch`, `complete`, `complete_batch` | Flag a stale `from`; preserve close-record effects. Done and resulting ready-task releases are one save and one undo boundary. Do not infer done from host state. |
| `SoftDelete(task)`, `Restore(task)` | Board delete and undo; `soft_delete`, `soft_delete_batch`, `restore` | Tombstone by ID. Clearing prerequisites that point to deleted tasks belongs to that same delete and undo. |
| `SetArchived(task, archived)` | CLI archive/unarchive; board `file_tasks`, `unarchive_task` | Keep archive separate from status. Preserve the board toggle as intent and its single/batch undo; CLI explicit archive/unarchive is idempotent and has no undo. |
| `RestoreTrash(task)` | CLI trash restore via `TaskStore::restore_from_trash` and `insert_restored` | Resolve the tombstone by ID; the store/authority decides whether a display number is still free. Reject an already-live ID. No undo is added. |

CLI JSON-plan add is partial success, not an atomic `Batch`: refused items and existing
matches emit no op. Record successful creates in input order under the existing single save.

## Rounds and steps

| Op and payload | Current entry points | Conflict and undo boundary |
| --- | --- | --- |
| `OpenBlock(task, from, record, why, needs, options, on)`, `OpenReview(task, from, record, done, checks, next, on)` | CLI status; board block/review cards; `block`, `review`, `block_batch`, `review_batch` | Status conflict check plus distinct round identity; do not replace a newer round silently. Marked actions keep one undo entry; CLI single opens add none. |
| `EditBlock`, `EditReview`, `SetOn` (task, record, patch) | CLI status and edit; board round cards and selector; `edit_block`, `edit_review`, `set_on`, `set_on_batch` | Patch only the addressed open record. Preserve retained check state when editing the list. Single and marked `SetOn` have undo. CLI `edit --on` also adds that entry, which a later due change in the same edit can stale. |
| `SetCheck(task, record, check, state)` | CLI checks; board review checklist | Address the check in its original round. No undo today; carry explicit state rather than a toggle. |
| `AddReply(task, record, reply, text)`, `EditReply`, `DeleteReply` (same IDs, optional text), `MarkReplySent(task, record, reply, sent version)` | CLI reply; board reply editor; `reply`, `edit_reply`, `delete_reply`, `mark_reply_sent` | Union inserts by ID; edits and tombstones target one reply. Delivery marks only the still-current round and text. A board reply saved with its round edit stays one transaction (`as_one_change`), not a new undo entry. |
| `InsertStep(task, step, text, anchor)`, `MoveStep(task, step, anchor)`, `ToggleStep(task, step)`, `RenameStep(task, step, text)`, `RemoveStep(task, step)` | CLI steps; board task page and form; `insert_step`, `move_step`, `toggle_step`, `rename_step`, `remove_step` | Union new IDs. Order, edits, and removals target the addressed step. Apply toggle once per op ID; concurrent moves are ordered by the authority. Board `with_steps_undo` or form undo owns the boundary; CLI step verbs add none. |

## Project, dispatch, and maintenance

| Op and payload | Current entry points | Conflict and undo boundary |
| --- | --- | --- |
| `SetProjectArchived(project, archived)`, `SetProjectTitle(project, title or clear)` | CLI project verbs; board project picker and title dialog | Project UUID on the wire; update the addressed field. Keep board project-title undo and existing archive semantics. |
| `HideAssignee(name)` | Board assignee header; `hide_assignee` | Keep the hidden-name flag and its existing undo. An assign that shows the same assignee updates it in that assign transaction. |
| `RecordDispatch(task, expected dispatch or revision, host, record, status change if any)` | CLI/board start, dispatch and relaunch via `dispatch.rs`; `record_dispatch`, `record_dispatch_with_status`, `push_start_undo` | Host-tag the record and gate replacement by the expected dispatch. Record only after launch succeeds; replay never launches. Single dispatched start undoes status only. |
| `RecordCleanup(task, expected dispatch, outcome)`, `CompleteAfterCleanup(task, from, outcome)` | CLI clean and done, board cleanup card; `record_dispatch_cleaned` variants and `complete_after_cleanup` variants | Ignore stale outcomes for replaced dispatches. Preserve completion and release/store boundaries; record what happened, not an instruction to run cleanup on replay. |
| `ReleaseStart(task, prerequisite)`, `ResolveReleasedLaunch(task, expected revision, outcome)` | Save/release path in `dispatch.rs`; `start_released`, `note_released_dispatch`, `return_released_to_ready` | Release decision is part of completion; returned launch results are guarded, not independent user status choices. Take/hold queues are orchestration, not wire ops. |
| `Undo(target change)` | Board undo, including archive and project title | Apply the existing validated local inverse in phase 0. Shared undo is a compensating op for the actor's own change; preserve a batch as one boundary. |

Store merge/reload, number allocation, undo pruning, backup/restore, and trash purge are persistence or operator work, not ordinary user intents. Starter guides and announcements are local notice delivery. A replay harness should compare state after the authoritative effects of ops, not replay file writes, Git calls, or agent launches.
