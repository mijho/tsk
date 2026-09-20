# tsk S2 sync prototype operations

Last verified on the `tsk-mijho` VM: **2026-09-20**.

This is a **prototype runbook**, not a production deployment guide. The managed demo runs
one `tsk-sync` API and one S2 Lite process, keeps both listeners on loopback by default,
and creates two independent local clients that share one stable project UUID. Acceptance
checks are append-only: they use unique tasks and operation IDs and never clear S2 history.

## Onboarding: three commands

From the repository root:

```sh
scripts/sync-demo setup
scripts/sync-demo doctor
scripts/sync-demo test --profile smoke
```

`setup` performs preflight checks, installs the pinned S2 CLI when needed, builds locked
release binaries, creates or adopts private server/client configuration, initializes two
distinct git project directories, preserves an existing Docker volume, renders
machine-specific managed systemd units, starts the services, and runs `doctor`. It may ask
for `sudo`; privileged operations use argument arrays and atomic file helpers rather than a
shell.

The setup is idempotent. Existing valid configuration files are adopted byte-for-byte.
It never prints token values and never replaces an existing config. A symlink or symlinked
managed-path component, placeholder, wrong owner, mode other than `0600`, client/server
mismatch, path mismatch, actor mismatch, bind mismatch, unknown systemd unit/drop-in, or
unrelated Docker container-name collision is a refusal. Review an old colliding unit before
opting in to replacement:

```sh
scripts/sync-demo setup --replace-managed-units
```

Use `--no-build` only when both release binaries already exist, and `--no-start` to install
configuration and units without starting them. A `--no-start` setup does not run the live
`doctor`; run it after starting the services.

## Defaults and overrides

| Setting | Default | Purpose |
| --- | --- | --- |
| `TSK_SYNC_SERVER_CONFIG` | `/etc/tsk-sync/config.json` | Private server configuration |
| `TSK_SYNC_DEMO_ROOT` | `~/.local/share/tsk-sync-demo` | Client state and project git directories |
| `TSK_SYNC_WORKSPACE` | `demo` | Shared workspace identifier |
| `TSK_SYNC_ACTOR_A` | `user-a` | First authorized actor |
| `TSK_SYNC_ACTOR_B` | `user-b-agent` | Second authorized actor |
| `TSK_SYNC_OUTSIDER_ACTOR` | `unauthorized` | Actor used to prove authorization denial |
| `TSK_SYNC_API_BIND` | `127.0.0.1:8000` | API listener |
| `TSK_SYNC_ENDPOINT` | `http://127.0.0.1:8000/` | Client/checker base URL |
| `TSK_SYNC_API_SERVICE` | `tsk-sync.service` | API systemd unit name |
| `TSK_SYNC_S2_SERVICE` | `tsk-s2.service` | S2 systemd unit name |

Set overrides consistently on every `setup`, `doctor`, `test`, `status`, `restart`, and
`logs` invocation. Setup treats an existing file that disagrees with the requested values
as a mismatch rather than rewriting it. Plain HTTP client endpoints are supported only on
loopback by `tsk` itself.

The managed layout is:

```text
$TSK_SYNC_DEMO_ROOT/
  client-a/sync.json       # 0600, actor A bearer
  client-b/sync.json       # 0600, actor B bearer
  project-a/.git/          # local path for shared project UUID
  project-b/.git/          # different local path for the same UUID
```

The server config is also `0600`. All three generated tokens and the local S2 token are
independent random values. Project A and B both map to
`11111111-1111-4111-8111-111111111111` / `tsk-demo`, but retain their own absolute local
paths.

## What doctor proves

`doctor` is read-only. Human output contains one `PASS`/`FAIL` line per check; automation
can use:

```sh
scripts/sync-demo doctor --json
```

It checks:

- required tools, executable release binaries, the S2 CLI/revision, immutable Lite image
  digest, and exact Rust SDK pin;
- regular non-symlink `0600` configs with no placeholders;
- distinct actor credentials, client state paths, and git roots mapped to one project UUID;
- active API and S2 systemd units;
- S2 endpoints and Docker port publishing restricted to loopback;
- API bind and client endpoint restricted to loopback by default;
- failure injection absent from the running service and rejected by the API;
- health, both authorized snapshots, missing/invalid bearer `401`, and outsider
  read/write/events `403` behavior.

For a deliberately reviewed remote API deployment, `doctor --allow-non-loopback` relaxes
only the API bind/endpoint check. It never permits non-loopback S2 exposure. Doctor never
prints token values.

## Test profiles

### Smoke (normal verification)

```sh
scripts/sync-demo test --profile smoke
```

Smoke runs the non-service-restarting acceptance subset: two-way propagation, concurrent
number allocation, stale conflict handling, cursor catch-up, tombstones, authorization,
project path translation, local-only isolation, pins, and preflight. An unexpected `SKIP`
is a failure. Use `--allow-skips` only when deliberately accepting reduced coverage.

### Full (explicitly disruptive)

```sh
scripts/sync-demo test --profile full --yes-disruptive
```

Full preserves all 16 acceptance checks and additionally restarts S2/API and deliberately
crashes the API around durable append/projection boundaries. The wrapper creates a temporary
managed drop-in under `/run/systemd/system`, enables failure injection, runs the checker,
and removes the drop-in in a `finally` path. It then reloads systemd, restarts the API, and
requires a healthy response even when the checker failed. Failure injection is not present
in the persistent unit.

The command refuses to run without `--yes-disruptive`. Unexpected skips fail unless
`--allow-skips` is explicit.

### TUI PTY

```sh
scripts/sync-demo test --profile tui
```

This is a real pseudo-terminal test. It opens client B's TUI first, creates a project task
through client A, and waits for the already-open TUI to render the new title through normal
background sync polling.

The lower-level checker remains available for debugging:

```sh
python3 scripts/sync-demo-check.py \
  --profile smoke \
  --fail-on-skip \
  --server-config /etc/tsk-sync/config.json \
  --client-a-dir "$HOME/.local/share/tsk-sync-demo/client-a" \
  --client-b-dir "$HOME/.local/share/tsk-sync-demo/client-b" \
  --tsk "$PWD/target/release/tsk" \
  --api-service tsk-sync.service \
  --s2-service tsk-s2.service
```

`--profile full` is the historical 16-check set. Prefer the `sync-demo test` wrapper for a
full run because it owns the temporary failure-injection lifecycle.

## Pinned components

| Component | Pin |
| --- | --- |
| Rust | repository `rust-toolchain.toml` (`1.96.0`) |
| S2 CLI/Lite | `0.42.12`, release revision `52b6e2e8fb7b8b7b13a34a8b7b149429ea9e726a` |
| S2 Lite image | `ghcr.io/s2-streamstore/s2@sha256:75fb787f3d1b27572100ddcca704dc0cbc85936d899f0c88f66a2549f216fdc6` |
| S2 Rust SDK | exactly `0.34.8` in `Cargo.toml`/`Cargo.lock` |

Do not replace the image digest with a mutable tag. The host-side CLI installer also pins
and verifies its release archive checksum. The Docker volume is named `tsk-s2-lite`; setup
creates and initializes it only when absent and never deletes or recreates an existing
volume.

## Routine operations

```sh
scripts/sync-demo status
scripts/sync-demo restart api
scripts/sync-demo restart s2
scripts/sync-demo restart all
scripts/sync-demo logs api
scripts/sync-demo logs s2 --follow
```

After rebuilding, restart the API so systemd does not retain the old executable:

```sh
cargo build --release --locked
scripts/sync-demo restart api
```

The API projection and operation-deduplication index are rebuilt from S2 whenever
`tsk-sync` starts. Restarting S2 alone does not prove projection replay; the full profile
restarts both in the required order and compares the reconstructed snapshot.

## Direct client use

The generated configs are selected automatically from each state directory:

```sh
./target/release/tsk add \
  --state-dir "$HOME/.local/share/tsk-sync-demo/client-a" \
  --project "$HOME/.local/share/tsk-sync-demo/project-a" \
  --title "two-client smoke" --json

./target/release/tsk list \
  --state-dir "$HOME/.local/share/tsk-sync-demo/client-b" \
  --all --json
```

Client config can also be selected explicitly with
`TSK_SYNC_CONFIG=/absolute/path/to/client.json`. Cache, pending-operation, stale, and error
markers live inside that client state directory.

## Remote access

The API has application bearer authorization but no native TLS. Keep the default loopback
bind and use an SSH tunnel for another machine:

```sh
ssh -N -L 18000:127.0.0.1:8000 tsk-mijho.exe.xyz
```

The remote client's endpoint is then `http://127.0.0.1:18000/`. Give that machine a new
state directory, its own bearer identity, and its own absolute project path. Never expose
S2 port 8080.

A two-client test on one VM proves separate identities, caches, cursors, and local path
translation, but not cross-machine networking, proxy behavior, or disconnection behavior.

## Backup

A consistent backup stops writers, archives the named Docker volume, and archives the
secret server configuration. Store the result outside the repository and protect it as
secret material.

```sh
BACKUP="$HOME/backups/tsk-sync-$(date -u +%Y%m%dT%H%M%SZ)"
umask 077
mkdir -p "$BACKUP"
sudo systemctl stop tsk-sync.service tsk-s2.service

VOLUME_PATH=$(docker volume inspect tsk-s2-lite --format '{{.Mountpoint}}')
sudo tar -C "$VOLUME_PATH" -czf "$BACKUP/tsk-s2-lite.tgz" .
sudo tar -C / -czf "$BACKUP/etc-tsk-sync.tgz" etc/tsk-sync
sudo cp -a /etc/systemd/system/tsk-s2.service \
  /etc/systemd/system/tsk-sync.service "$BACKUP/"
sudo chown -R "$(id -u):$(id -g)" "$BACKUP"
chmod -R go-rwx "$BACKUP"

scripts/sync-demo restart all
```

Client caches are not authoritative and do not need backup. Retain any
`sync-pending.json` until that exact operation has been reconciled.

## Recovery

Recovery is intentionally manual because it replaces durable state. Stop both units,
restore the entire volume and server configuration from one matching backup, restore the
units, then run:

```sh
sudo systemctl daemon-reload
scripts/sync-demo restart all
scripts/sync-demo doctor
scripts/sync-demo test --profile smoke
```

Do not clear or recreate an existing volume as an onboarding or troubleshooting step.

## Prototype limitations

- Every accepted event stores a full `DomainState`; there is no compaction or retention
  policy, so log and replay cost grow with every operation.
- Static bearer tokens and workspace membership come from one local JSON file. There is no
  rotation endpoint, expiry, audit UI, or dynamic membership; changes require API restart.
- Workspace serialization is an in-process mutex. Multiple API replicas are unsupported.
- Shared writes require connectivity. Offline reads may use the last private cache; there
  is no general offline write queue beyond retrying one persisted ambiguous operation.
- Shared undo, project archive/unarchive, hard removal, and trash-file restore are disabled.
  Soft-deleted tasks remain authoritative tombstones.
- Shared project IDs/names are configured, not discovered. Every client needs its own
  absolute-path mapping.
- Failure-injection headers can terminate the API. They are enabled only by the temporary
  full-test drop-in and must remain disabled in persistent units.
- The API has no native TLS. Use a trusted authenticated proxy or SSH tunnel and never
  publish S2 directly.
- This prototype has no production backup SLA, mixed-version schema policy, rate limiting,
  alerting, or disaster-recovery objective.
