# Shared workspace: plan

One tsk workspace shared by many hosts: agent VMs write offline, humans edit the same
tasks at once, a PWA reads it from a phone. Design only; the work items live on the tsk
board (thread `sync-*`), not here. Process: `docs/sync/PROCESS.md`.

## Decisions

| Question | Decision |
| --- | --- |
| Shared unit | The whole workspace, desk included. Each host's `tsk.json` becomes a replica. |
| Offline | Agents write offline. Every client keeps an outbox. |
| Authority | One server, `tsk-sync`, SQLite, on an exe.dev VM. It owns order and numbers. |
| Concurrency | Humans edit the same task at once. Merge by meaning, per field, not per task. |
| Wire model | Clients send ops (intents), never proposed state. |
| Prior art | `feature/s2-multi-user-sync`: keep config, tokens, idempotency, validation, project UUIDs. Drop S2, full-state events, proposed-state mutations. |

### S2 prototype inventory

Use the prototype at `5b65fcd` as reference, not as a branch to merge.

| Prototype files (`src/sync/`) | Carry forward |
| --- | --- |
| `config.rs`, `client.rs` | Strict private config for endpoint, workspace, and credentials. Map each host's local project paths to shared project UUIDs, not wire-level absolute paths. |
| `server.rs` | Authenticate bearer tokens and derive the actor server-side. Store token hashes and scopes in SQLite, not plaintext bearer values in server config. |
| `client.rs`, `projection.rs` | Persist pending writes before sending. Retry with the same op id. Return the original result for the same actor and op content, and refuse reuse with different content or actor. |
| `validation.rs` | Keep state and ID invariants. Validate each op and the state it produces through domain functions, not a client-supplied proposed state. |
| `s2log.rs`, `protocol.rs`, `mod.rs` | Drop the S2 adapter, full-state events, and proposed-state requests. Use the SQLite op log and protocol below. |

## Model

An **op** is one named domain mutation with an id: `SetStatus`, `ToggleStep`, `SetTitle`,
`AddReply`, `Create`, … The domain functions already exist; an op is their serialisable
call. One op is one undo-entry boundary; a bulk verb is one `Batch` op.

- **Client.** Replica = server snapshot at cursor `N` + pending ops replayed on top.
  Outbox is durable and ordered; op ids make resend safe.
- **Push.** `POST /ops` with a batch. The server replays each op against current state,
  assigns `seq` and task numbers, returns accepted/rejected per op. Body
  `{"ops": [Op…]}`; answer `{"results": [{"status": "accepted", "op_id", "seq", "numbers":
  {task id: number}} | {"status": "rejected", "op_id", "reason"}]}` in order. A resent op
  (same id, actor, body) gets its stored result; an id reused for anything else is
  rejected `op-id-reused`. A batch commits as one SQLite transaction with the projection.
- **Pull.** `GET /ops?after=N` (or the `/events` stream). Client applies, drops confirmed
  ops from the outbox, replays the rest.
- **Snapshot.** `GET /snapshot` for cold start and catch-up.

## Conflict rules

| Field | Rule |
| --- | --- |
| Steps, replies, blocks, review rounds | Stable ids or append-only: union. |
| Title, notes, thread, assignee, due, base | Last op to reach the server wins; the loser lands in the paper trail. |
| Status | Op carries `from`. If current ≠ `from`, accept and flag a conflict for a human. Never pick silently. |
| `after` | Stored by task id. Numbers are display only. |
| Create | Server assigns the number. Offline creates show `T~<idprefix>` until confirmed. |
| Done → release | Decided by the server when the done op lands, never by clients. |
| Delete / restore | Tombstone; restore is an op. |
| Undo | Per actor: a compensating op for that actor's last op. No shared undo stack. |
| Dispatch record | Host-tagged. Never interpreted on another host. |

## Claims

`tsk claim T12` is a server-side compare-and-set: open/ready → started with assignee =
actor. Online only. A held claim lets that actor keep writing the task offline. A second
claim is refused. A stale claim is flagged, never auto-released (human status is truth).

## Server

- Binary `tsk-sync`, same crate, behind cargo feature `sync-server`, so the default `tsk`
  build and CI matrix pull no server deps. Client HTTP uses `ureq` (blocking); the server
  uses `tiny_http` (blocking, no async runtime). Config is `[server] listen, database`,
  every key required and known; the schema version is SQLite's `user_version`.
- SQLite (`rusqlite`, bundled). Tables: `ops(seq, op_id UNIQUE, actor, at, body, numbers)`,
  `rejections(op_id, actor, body, reason)`, `projection(seq, state)` (the authority's
  `DomainState`), `counters`, `tokens(hash, actor, scope)`, `meta(protocol_version)`.
- The server takes no `Undo` or `Dispatch` op until per-actor undo (T21) and host-tagged
  dispatch (T22) define them; both are refused `not-on-server`.
- Endpoints: `POST /ops`, `GET /snapshot`, `GET /ops?after=N`, `GET /events` (SSE),
  `GET /` (PWA). Bearer token per actor; actor lands in every history event.
- Protocol version in every request; the server refuses below its minimum. The strict
  unknown-field rule moves from the file to the protocol.
- Runs as a systemd unit on the VM; exe.dev's proxy fronts HTTPS. Backup: SQLite
  `.backup` on a timer to the VM disk plus off-VM copy.

## Phases

| # | Ships | Gate |
| --- | --- | --- |
| 0 | Ops locally: every mutation is an op; `after` by id (store migration) | Replay-from-empty equals final state, for every verb |
| 1 | Server + online writes | Two clients on one workspace, CLI only |
| 2 | Outbox, provisional numbers, rejection notices, field merges and status-conflict flags, release, undo, host-tagged dispatch | Agent VM edits offline, reconnects, converges |
| 3 | `/events`, board subscribes, read-only PWA | Phone sees an agent's status change within a second |
| 4 | Claims and stale-claim flags | Two agents race one task; exactly one wins |
| 5 | PWA writes: status verbs, reply, quick-add | Unblock an agent from the phone |

Dogfood: after phase 2, this board moves onto the shared workspace.

## Holds from AGENTS.md

tsk has users. Store changes keep migrations and `deny_unknown_fields`; local-only mode
stays the default and unchanged. Docs ship with each user-visible step. Inside
`src/sync/` (new code) there is no compatibility to keep: one way, no fallbacks.
