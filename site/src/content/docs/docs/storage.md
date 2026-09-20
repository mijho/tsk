---
title: Storage
description: Task data, backups, deleted tasks, and update checks.
---

The board, CLI, and Herdr plugin share `~/.tsk/tsk.json`. The current store format is v5.

## Location

| Setting | Default | Purpose |
| --- | --- | --- |
| `TSK_STATE_DIR` | `~/.tsk` | Task data, backups, trash, release-check cache, and optional sync cache |
| `TSK_SYNC_CONFIG` | `<state-dir>/sync.json` when that file exists | Select an explicit shared-board client configuration |
| `--state-dir <dir>` | State directory | Override storage for a data command |

Use a local disk. NFS and synced folders such as Dropbox or iCloud Drive are unsupported. Directory roots must be real directories, not symlinks.

Herdr's plugin-specific state/config directories do not override these locations. Removing tsk leaves its task data intact.

## Shared-board client

A `sync.json` file in the state directory selects the authenticated remote backend. Set `TSK_SYNC_CONFIG` to select a config elsewhere. On Unix the config must be a regular file with mode `0600`; it contains a bearer secret and this shape:

```json
{
  "endpoint": "https://sync.example.test/",
  "workspace": "team",
  "bearer": "secret-token",
  "projects": [
    {
      "id": "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa",
      "name": "atlas",
      "path": "/home/alice/src/atlas"
    }
  ]
}
```

Project IDs and names are shared identities; `path` is this machine's absolute checkout path. Local paths are translated to `shared://<id>/<name>` before upload and are never sent to the service.

Reads fetch an authenticated snapshot and cache its wire state and cursor in `sync-cache.json`. Transport failures may use that cache and write `sync-stale.json` with its age; authentication, authorization, and protocol failures never fall back. Writes always require a live snapshot. An uncertain write remains in `sync-pending.json` and is resent with the same operation ID before another write, so a lost response cannot duplicate the operation. The open board polls the remote cursor every few seconds.

Shared undo, project archive/unarchive, starter/release-note delivery, and `trash.jsonl` restore are not supported by this protocol version. Deleted shared tasks remain tombstones in the shared snapshot and can be restored through normal task actions. Local-only mode is unchanged when no sync config is selected.

## Local sync demo

On a Linux systemd host with Docker, the repository's supported prototype onboarding is:

```sh
scripts/sync-demo setup
scripts/sync-demo doctor
scripts/sync-demo test --profile smoke
```

Setup creates or adopts private `0600` server and two-client configs without printing or
replacing credentials, maps two git directories to one stable shared project UUID, keeps
S2 and the API on loopback by default, preserves the named Docker volume, and installs
machine-specific managed units. Doctor is read-only. Smoke is append-only and does not
restart services; unexpected skips fail.

The complete 16-check run is intentionally disruptive and must be explicit:

```sh
scripts/sync-demo test --profile full --yes-disruptive
```

It temporarily enables failure injection through a `/run` systemd drop-in, exercises API
crash/restart and S2 persistence, then removes the drop-in and requires a healthy restart.
Use `scripts/sync-demo test --profile tui` for the real PTY check where an already-open
client receives another client's create. See `README-SYNC.md` for environment overrides,
unit-collision rules, backup/recovery, and operational commands.

## Backups

| File | Contains |
| --- | --- |
| `tsk.json` | Current tasks and archived-project records |
| `tsk.json.1` | Previous valid task document |
| `tsk.json.v<N>` | Backup made when migrating an older store format, such as `tsk.json.v4` for the v4 → v5 migration |
| `delivery.json` | Which starter tasks this install has received or dismissed, and the newest release note it has seen |

An older binary refuses a newer or unversioned store instead of rewriting it. Use a compatible tsk version to open it. On first save, v4 stores migrate to v5 so one undo entry can cover a marked completion or deletion; the original document is saved as `tsk.json.v4`. Earlier stores still run through each migration in order, including the v3 to v4 move from ready to open.

Archived tasks stay in the task document with their existing status. [Archive and restore](/docs/board/#archive).

## Starter guides and release notes

The four starter tasks (`N1`… on your desk) are seeded once per state directory on the first board open. Mark one done, archive it, or delete it and it never returns. Deleting `delivery.json` seeds any starter catalog id the task document no longer holds.

After an upgrade, the first board open adds one `What's new in tsk` task to your desk (in `review`, under NEEDS YOU) when the new version bundles release notes you have not seen. That includes upgrading from a build that only had starter tasks. Every missed release lands in that one task, newest first, with its changelog link in the notes. The notes ship inside the binary; a blank first install records them as seen and shows none. Clear it like any starter task.

## Deleted tasks

Deleted tasks move to `trash.jsonl` once undo can no longer restore them, or after seven days. They are purged 30 days after deletion.

```sh
tsk list --deleted --all
tsk trash restore T12
```

The list includes both recently deleted tasks still in the main store and tasks in trash. Restore reads trash; use board undo for a recent deletion that has not moved there yet.

## Update check

On launch, tsk checks for a newer release if its cached check is older than 24 hours. A newer version appears on the board's idle status row as `vX.Y.Z available, run tsk update`.

| Setting or file | Purpose |
| --- | --- |
| `TSK_NO_UPDATE_CHECK` | Set to disable the check and notice |
| `TSK_UPDATE_CURL=/absolute/path/to/curl` | Use a nonstandard, explicit curl path for the check and for `tsk update` (default `/usr/bin/curl`) |
| `update.json` | Cached release check in the state directory |

The check requests the latest release tag from GitHub over HTTPS only. It does not upload task data. Failures are silent.

[Upgrade tsk](/docs/install/#upgrade).
