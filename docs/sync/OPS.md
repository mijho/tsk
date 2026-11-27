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
| `EditTask(task, field patch, step intents)` | CLI edit: `edit_with_assignee_and_base`, `edit_due`, `edit_after`, `set_on`; board form: `edit_with_step_changes_and_base`, staged due and after | Scalar fields use last arrival, including an explicit board scope patch (desk or project UUID). CLI edit keeps scope. Only staged step changes get board form undo. Keep CLI edits atomic and preserve `SetOn` undo as described below. |
| `SetAssignee`, `SetBase`, `SetDue` (task, value or clear) | Board actions and CLI edit; `assign_batch`, `set_base_batch`, `edit_due` | Scalar last arrival with history. Marked actions keep a single batch undo. Any automatic unhide on assignment stays in the same change. |
| `SetAfter(task, prerequisite IDs)`, `ChainAfter(ordered task IDs)` | CLI edit and create; board after picker and chain; `set_after_batch`, `chain_after` | Validate existence, self-links and cycles against current state. The picker replaces prerequisites; chain appends each predecessor to its successor's existing list if missing. Both board actions have one atomic undo; CLI `edit --after` adds none. Local code still resolves numbers until T7. |
| `SetStatus(task, from, to)`, `Complete(task, from)` | CLI status, board status/start/done paths; `set_status_by`, `change_status`, `start_batch`, `complete`, `complete_batch` | Flag a stale `from`; preserve close-record effects. Done and its releases share a save. Board status adds undo, and board completion undo includes release effects. CLI ordinary status and done via `set_status_by` add no undo. |
| `SoftDelete(task)`, `Restore(task)` | Board delete and undo; `soft_delete`, `soft_delete_batch`, `restore` | Tombstone by ID. Clearing prerequisites that point to deleted tasks belongs to that same delete and undo. |
| `SetArchived(task, archived)`, `ToggleArchived(task)` | CLI archive/unarchive; board `file_tasks`, `unarchive_task` | Keep archive separate from status. Board toggle has single/batch undo. CLI explicit archive/unarchive is idempotent and has no undo. On an archived task with no live archive entry on top, board `ctrl+u` explicitly unarchives without pushing or popping undo. |
| `RestoreTrash(task)` | CLI trash restore via `TaskStore::restore_from_trash` and `insert_restored` | Resolve the tombstone by ID; the store/authority decides whether a display number is still free. Reject an already-live ID. No undo is added. |

CLI JSON-plan add is partial success, not an atomic `Batch`: refused items and existing
matches emit no op. Record successful creates in input order under the existing single save.

## Rounds and steps

| Op and payload | Current entry points | Conflict and undo boundary |
| --- | --- | --- |
| `OpenBlock(task, from, record, why, needs, options, on)`, `OpenReview(task, from, record, done, checks, next, on)` | CLI status; board block/review cards; `block`, `review`, `block_batch`, `review_batch` | Status conflict check plus distinct round identity; do not replace a newer round silently. Board opens add one batch undo even for one target. CLI opens and in-place edits add none. |
| `EditBlock`, `EditReview`, `SetOn` (task, record, patch) | CLI status and edit; board round cards and selector; `edit_block`, `edit_review`, `set_on`, `set_on_batch` | Patch only the addressed open record. Preserve retained check state when editing the list. Single and marked `SetOn` have undo. CLI `edit --on` also adds that entry, which a later due change in the same edit can stale. |
| `SetCheck(task, record, check, state)` | CLI checks; board review checklist | Address the check in its original round. No undo today; carry explicit state rather than a toggle. |
| `AddReply(task, record, reply, text)`, `EditReply`, `DeleteReply` (same IDs, optional text), `MarkReplySent(task, record, reply, sent version)` | CLI reply; board reply editor; `reply`, `edit_reply`, `delete_reply`, `mark_reply_sent` | Union inserts by ID; edits and tombstones target one reply. Delivery marks only the still-current round and text. Reply-only saves add no undo. |
| `InsertStep(task, step, text, anchor)`, `MoveStep(task, step, anchor)`, `ToggleStep(task, step)`, `RenameStep(task, step, text)`, `RemoveStep(task, step)` | CLI steps; board task page and form; `insert_step`, `move_step`, `toggle_step`, `rename_step`, `remove_step` | Union new IDs. Order, edits, and removals target the addressed step. Apply toggle once per op ID; concurrent moves are ordered by the authority. Board `with_steps_undo` or form undo owns the boundary; CLI step verbs add none. |

For board unblock and review send-back, apply the optional reply add or edit before
`SetStatus` closes that round, in one `as_one_change` transaction. Only status is
undoable; the reply survives the inverse. Review send-back always targets `started`.

## Project, dispatch, and maintenance

| Op and payload | Current entry points | Conflict and undo boundary |
| --- | --- | --- |
| `SetProjectArchived(project, archived)`, `SetProjectTitle(project, title or clear)` | CLI project verbs; board project picker and title dialog | Use project UUID on the wire. Refuse a title another project already uses. A changed title pushes one undo from either entry point; project archive/unarchive adds none. |
| `HideAssignee(name)` | Board assignee header; `hide_assignee` | Keep the hidden-name flag and its existing undo. An assign that shows the same assignee updates it in that assign transaction. |
| `RecordDispatch(task, expected dispatch or revision, host, record, status change if any)` | CLI/board start, dispatch and relaunch via `dispatch.rs`; `record_dispatch`, `record_dispatch_with_status`, `push_start_undo` | Host-tag the record and gate replacement by the expected dispatch. Record only after launch succeeds; replay never launches. A single board launch adds status-only undo if status changes; CLI dispatch and CLI start with dispatch add none. |
| `RecordCleanup(task, expected dispatch, outcome)`, `CompleteAfterCleanup(task, from, outcome)` | CLI clean and done, board cleanup card; `record_dispatch_cleaned` variants and `complete_after_cleanup` variants | Ignore stale outcomes for replaced dispatches. If cleanup and board completion save together, only completion and its releases are undoable. Later async board cleanup retargets matching undo revisions; standalone CLI clean does not. Replay records the outcome without running cleanup. |
| `ReleaseStart(task, prerequisite)`, `ResolveReleasedLaunch(task, expected revision, outcome)` | Save/release path in `dispatch.rs`; `start_released`, `note_released_dispatch`, `return_released_to_ready` | Release is part of completion. Success attributes the dispatch to its prerequisite and retargets the release undo to the new revision. A failed or refused launch returns a still-current start to ready and removes only its matching start undo leaf. Take/hold queues are orchestration, not wire ops. |
| `Undo(target change)` | Board undo, including archive and project title | Apply the existing validated local inverse in phase 0. Shared undo is a compensating op for the actor's own change; preserve a batch as one boundary. |

Store merge/reload, number allocation, undo pruning, backup/restore, and trash purge are persistence or operator work, not ordinary user intents. Starter guides and announcements are local notice delivery. A replay harness should compare state after the authoritative effects of ops, not replay file writes, Git calls, or agent launches.
