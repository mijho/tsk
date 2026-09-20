# S2 multi-user synchronisation prototype plan

Date: 2026-09-20
Starting fork commit: `7daa68ba0b8044548fe4daa6734a0b4342a091cb`
Working branch: `feature/s2-multi-user-sync`

## Verified baseline

- Rust terminal application with a versioned local JSON `DomainState` (`format_version = 5`).
- Stable task and step UUIDs; task revisions and merge-base stale-write checks.
- Task numbers are assigned while holding the local store lock.
- CLI mutations use locked store transitions; TUI mutations use revision-aware merge/save.
- TUI refresh polls a store signature and reloads changed state.
- Project identity is currently a canonical local filesystem path.
- Existing per-task history is not a complete replay log, so it will not be treated as one.

Pinned prototype dependencies:

- Rust `1.96.0` (repository pin).
- S2 CLI/Lite `0.42.12`, release commit `52b6e2e8fb7b8b7b13a34a8b7b149429ea9e726a`.
- S2 Rust SDK `0.34.8`.
- S2 storage on local disk through `s2 lite --local-root`; both account and basin endpoints point to the private Lite listener.

## Architecture

1. Add a `tsk-sync` Rust service binary. It authenticates static bearer tokens, authorizes workspace membership, serializes each workspace through a mutex, and exposes snapshot, mutation, and resumable event endpoints.
2. Use one S2 stream per workspace as the authoritative log. Each versioned envelope records workspace, operation ID, actor, content hash, event kind, affected IDs/revisions, resulting authoritative state, and result metadata. Full accepted state in each event makes deterministic replay and projection rebuild explicit for this prototype.
3. Rebuild the in-memory projection and operation-deduplication index from S2 at startup. Conditional append uses the expected S2 tail. A tail race triggers replay/revalidation. Ambiguous append outcomes reconcile by operation ID before retrying.
4. Add a remote backend inside `TaskStore`, selected by an explicit per-client config file. Keep the existing filesystem adapter and local-only behavior unchanged. Remote transitions load a snapshot, run the same domain methods, and submit revision-guarded changes. A private cache provides visibly stale offline reads; shared writes require connectivity.
5. Keep shared project identity independent of checkout paths: remote config maps a stable project UUID/name to each client’s local path, translating only at the backend boundary.
6. Make TUI polling use the remote stream cursor as its store signature so an open board refreshes when another actor writes. Report connection/conflict/unknown outcomes through typed store errors. Disable shared-mode undo where revision-safe semantics are not yet guaranteed.
7. Run S2 and the API under systemd. S2 stays on loopback; the API is reached through exe.dev’s authenticated TLS proxy or an SSH tunnel. Secrets live in root-readable runtime configuration outside Git.

## Milestones

1. Install pinned Rust and S2; prove S2 create/append/replay/follow/conditional append and restart persistence.
2. Implement protocol, S2 adapter, authenticated service, replay projection, dedupe, conflict/tombstone checks, and failure injection.
3. Implement remote client configuration/cache/backend integration for CLI and TUI.
4. Add automated real-S2 two-client acceptance tests and local-mode regressions.
5. Install services, generate three identities, create a demo workspace, run checks, restart/rebuild demonstrations, and perform a TUI smoke test.
6. Document connection, operations, logs, restart, backup/recovery, security, and deferred limitations.

## Acceptance checks

- Live A→B and B→A propagation without client restart.
- Unique concurrent task numbers.
- Explicit same-task conflict.
- Idempotent retry across API restart and lost response.
- Cursor catch-up after disconnect.
- S2/API restart persistence.
- Projection deletion plus replay equivalence.
- Deleted-task stale-write rejection.
- Read/write/subscribe denial for unauthorized identity.
- Different local checkout paths map to one shared project.
- Existing local-only CLI/TUI behavior and repository green bar remain intact.
