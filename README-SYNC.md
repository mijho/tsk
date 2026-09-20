# tsk S2 sync prototype operations

Last verified on the `tsk-mijho` VM: **2026-09-20**.

This is a **prototype runbook**, not a production deployment guide. It operates one
`tsk-sync` API and one S2 Lite process on the `tsk-mijho` exe.dev VM. The API is
available locally at `http://127.0.0.1:8000`; the acceptance checker always uses that
loopback address.

The checker and all commands below are append-only. They use unique UUIDs/titles and do
not clear old S2 history.

## Pinned components

| Component | Pin |
| --- | --- |
| Rust | repository `rust-toolchain.toml` (`1.96.0`) |
| S2 CLI/Lite | `0.42.12`, release revision `52b6e2e8fb7b8b7b13a34a8b7b149429ea9e726a` |
| S2 Lite image | `ghcr.io/s2-streamstore/s2@sha256:75fb787f3d1b27572100ddcca704dc0cbc85936d899f0c88f66a2549f216fdc6` |
| S2 Rust SDK | exactly `0.34.8` in `Cargo.toml`/`Cargo.lock` |

Do not replace the image digest with a mutable tag. Verify the image and SDK pin:

```sh
cd /home/exedev/tsk
docker pull ghcr.io/s2-streamstore/s2@sha256:75fb787f3d1b27572100ddcca704dc0cbc85936d899f0c88f66a2549f216fdc6
docker run --rm ghcr.io/s2-streamstore/s2@sha256:75fb787f3d1b27572100ddcca704dc0cbc85936d899f0c88f66a2549f216fdc6 --version
cargo tree -i s2-sdk --depth 0
```

Expected version lines include `s2 0.42.12` and `s2-sdk v0.34.8`.

## Install and build

The VM needs Docker, systemd, Python 3.11 or newer, and the repository-pinned Rust
toolchain. Install the pinned host-side S2 CLI (used for administrative smoke checks),
then build TSK:

```sh
cd /home/exedev/tsk
scripts/install-s2-lite.sh
$HOME/.local/bin/s2 --version
rustup show active-toolchain
cargo build --release --locked
./target/release/tsk --version
```

If the base VM does not already provide prerequisites, install its distribution packages
first (package names vary by distribution). The build produces `target/release/tsk` and
`target/release/tsk-sync`. The exact SDK pin is already in the manifest; `--locked`
refuses an unreviewed lockfile change.

## Server configuration

`docs/sync-server.example.json` and `docs/sync-client.example.json` contain placeholders,
never usable secrets. Generate independent random values without putting them in shell
history, then edit the server copy:

```sh
cd /home/exedev/tsk
python3 - <<'PY'
import secrets
for name in ("local S2 token", "client A bearer", "client B bearer", "outsider bearer"):
    print(f"{name}: {secrets.token_urlsafe(32)}")
PY
sudo install -d -o exedev -g exedev -m 0700 /etc/tsk-sync
sudo install -o exedev -g exedev -m 0600 docs/sync-server.example.json /etc/tsk-sync/config.json
sudo -u exedev "${EDITOR:-vi}" /etc/tsk-sync/config.json
stat -c '%a %U:%G %n' /etc/tsk-sync/config.json
```

The final `stat` must report mode `600`. The service refuses a symlink, a non-regular
file, or any mode other than exactly `0600`. Keep `/etc/tsk-sync/config.json` outside Git,
do not paste it into logs, and do not reuse any bearer as the S2 token. The outsider
identity belongs to `other`, not `demo`; the checker uses it to prove authorization
denials without printing it.

## Install and start the services

Install the checked-in, pinned systemd units:

```sh
cd /home/exedev/tsk
sudo install -m 0644 deploy/systemd/tsk-s2.service /etc/systemd/system/tsk-s2.service
sudo install -m 0644 deploy/systemd/tsk-sync.service /etc/systemd/system/tsk-sync.service
sudo systemctl daemon-reload
sudo systemctl enable --now tsk-s2.service
sudo systemctl enable --now tsk-sync.service
curl -fsS http://127.0.0.1:8000/health
systemctl --no-pager --full status tsk-s2.service tsk-sync.service
```

`deploy/systemd/tsk-s2.service` runs the immutable S2 image with
`lite --local-root /data --path tsk --no-cors`, maps S2 only to
`127.0.0.1:8080`, and persists `/data` in the `tsk-s2-lite` Docker volume. If a new
volume is owned by root rather than the image's UID, initialize it once before starting:

```sh
docker volume create tsk-s2-lite
docker run --rm -v tsk-s2-lite:/data alpine:3.22 \
  sh -c 'chown -R 65532:65532 /data && chmod 700 /data'
```

The API unit enables failure injection only so the acceptance checker can exercise
ambiguous outcomes; remove that environment line outside an isolated prototype. The health
response is `{"status":"ok"}`. A healthy endpoint proves only that the HTTP process is
running; the authenticated snapshot and restart checks below also exercise S2.

To smoke the S2 routing and durable log directly, set **both** endpoint overrides:

```sh
export PATH="$HOME/.local/bin:$PATH"
export S2_ACCOUNT_ENDPOINT=http://127.0.0.1:8080
export S2_BASIN_ENDPOINT=http://127.0.0.1:8080
export S2_ACCESS_TOKEN=ignored
s2 create-basin tsk-sync-demo --create-stream-on-append --create-stream-on-read
s2 create-stream s2://tsk-sync-demo/probe
printf '%s\n' '{"probe":"one"}' | s2 append s2://tsk-sync-demo/probe
s2 read --seq-num 0 --count 1 s2://tsk-sync-demo/probe
# Omit --count to replay and then follow live records.
s2 read --seq-num 1 s2://tsk-sync-demo/probe
```

A conditional append uses `s2 append --match-seq-num <tail>`. Restart
`tsk-s2.service` and repeat the bounded read to prove acknowledged data remains available.

## Connect two clients on the VM

The following creates two external state directories and two different checkout paths
that map to the same shared project identity. It reads bearers from the external server
file and writes them directly to owner-only files; it does not echo them.

```sh
install -d -m 0700 \
  /home/exedev/.local/share/tsk-sync-demo/client-a \
  /home/exedev/.local/share/tsk-sync-demo/client-b \
  /home/exedev/.local/share/tsk-sync-demo/project-a \
  /home/exedev/.local/share/tsk-sync-demo/project-b
git -C /home/exedev/.local/share/tsk-sync-demo/project-a init -q
git -C /home/exedev/.local/share/tsk-sync-demo/project-b init -q

python3 - <<'PY'
import json, os
from pathlib import Path

server = json.loads(Path("/etc/tsk-sync/config.json").read_text())
tokens = {record["actor_id"]: record["bearer"] for record in server["tokens"]}
# Match these actor IDs to the two demo members in the installed server config.
clients = (
    ("user-a", Path("/home/exedev/.local/share/tsk-sync-demo/client-a"),
     "/home/exedev/.local/share/tsk-sync-demo/project-a"),
    ("user-b-agent", Path("/home/exedev/.local/share/tsk-sync-demo/client-b"),
     "/home/exedev/.local/share/tsk-sync-demo/project-b"),
)
for actor, state_dir, project_path in clients:
    document = {
        "endpoint": "http://127.0.0.1:8000/",
        "workspace": "demo",
        "bearer": tokens[actor],
        "projects": [{
            "id": "11111111-1111-4111-8111-111111111111",
            "name": "tsk-demo",
            "path": project_path,
        }],
    }
    path = state_dir / "sync.json"
    fd = os.open(path, os.O_WRONLY | os.O_CREAT | os.O_TRUNC, 0o600)
    with os.fdopen(fd, "w") as output:
        json.dump(document, output, indent=2)
        output.write("\n")
    os.chmod(path, 0o600)
PY

cd /home/exedev/tsk
./target/release/tsk add --state-dir /home/exedev/.local/share/tsk-sync-demo/client-a \
  --project /home/exedev/.local/share/tsk-sync-demo/project-a \
  --title "two-client smoke" --json
./target/release/tsk list --state-dir /home/exedev/.local/share/tsk-sync-demo/client-b \
  --all --json
```

Client config is selected from `<state-dir>/sync.json`, or explicitly with
`TSK_SYNC_CONFIG=/absolute/path/to/client.json`. Client config must also be a regular,
non-symlink `0600` file. Cache, pending-operation, stale, and error markers are written
inside the selected client state directory.

## Run the real-service acceptance checker

Build first, leave both units running, and run from the repository:

```sh
cd /home/exedev/tsk
python3 scripts/sync-demo-check.py \
  --server-config /etc/tsk-sync/config.json \
  --client-a-dir /home/exedev/.local/share/tsk-sync-demo/client-a \
  --client-b-dir /home/exedev/.local/share/tsk-sync-demo/client-b \
  --tsk /home/exedev/tsk/target/release/tsk \
  --api-service tsk-sync.service \
  --s2-service tsk-s2.service
```

The script checks, against the running API and real S2 Lite:

- A create/B receive and B status/A receive;
- concurrent number allocation;
- direct stale same-task rejection;
- durable idempotency after a lost response and after API restart;
- event-cursor catch-up;
- S2 plus API restart persistence and exact projection replay;
- stale update rejection after a tombstone;
- unauthorized snapshot, mutation, and event access;
- two local paths mapped to one shared project identity;
- an ordinary local-only `tsk.json` store;
- injected error and process crash after append but before projection update.

It prints one `PASS`, `SKIP`, or `FAIL` line per check and a final count. It never prints
bearers. `SKIP` is used when systemd/passwordless restart or failure injection is not
available; any `FAIL` makes the process exit nonzero. The process-crash check intentionally
aborts `tsk-sync`; systemd must restart it before the exact operation is retried.

## Access through exe.dev TLS or SSH

The VM's authenticated TLS URL is:

```text
https://tsk-mijho.exe.xyz/
```

After signing in through exe.dev, the browser health URL is
`https://tsk-mijho.exe.xyz/health`. The outer exe.dev proxy authentication is separate
from the API's workspace bearer. The current `tsk` client sends only the workspace
`Authorization: Bearer …` header and does not implement the exe.dev browser login/cookie
flow or a second proxy authorization header. For command-line clients on another machine,
use an SSH tunnel and keep the client endpoint on loopback:

```sh
# Run on the remote client machine and leave it open.
ssh -N -L 18000:127.0.0.1:8000 tsk-mijho.exe.xyz
```

Then set that machine's client config endpoint to:

```json
"endpoint": "http://127.0.0.1:18000/"
```

Use a new client state directory, its own bearer identity, and that machine's own absolute
project path. Never expose S2 port 8080; only tunnel the API port.

A two-client test on this VM proves separate identities, caches, cursors, and local path
translation, but both clients still share one kernel, filesystem host, and network path.
A real cross-machine test is still required to validate SSH/proxy setup, actual network
disconnects, machine-specific paths, and clock/network behavior.

## Logs, status, and restart commands

```sh
systemctl is-active tsk-s2.service tsk-sync.service
journalctl -u tsk-s2.service -n 200 --no-pager
journalctl -u tsk-sync.service -n 200 --no-pager
journalctl -fu tsk-sync.service

docker ps --filter name=tsk-s2-lite
docker inspect tsk-s2-lite --format '{{.Config.Image}} {{json .Mounts}}'

sudo systemctl restart tsk-sync.service
curl -fsS http://127.0.0.1:8000/health

sudo systemctl restart tsk-s2.service
sudo systemctl restart tsk-sync.service
curl -fsS http://127.0.0.1:8000/health
```

The API projection and operation-deduplication index exist only in memory and are rebuilt
from S2 whenever `tsk-sync` starts. Restarting S2 alone does not prove replay; restart the
API afterwards as shown.

After rebuilding the binary, restart the API so systemd does not keep the old executable:

```sh
cd /home/exedev/tsk
cargo build --release --locked
sudo systemctl restart tsk-sync.service
```

## Backup

A consistent backup stops writers, archives the named Docker volume, and archives the
secret server configuration. Store the backup outside the repository.

```sh
BACKUP=/home/exedev/backups/tsk-sync-$(date -u +%Y%m%dT%H%M%SZ)
umask 077
mkdir -p "$BACKUP"
sudo systemctl stop tsk-sync.service
sudo systemctl stop tsk-s2.service

VOLUME_PATH=$(docker volume inspect tsk-s2-lite --format '{{.Mountpoint}}')
sudo tar -C "$VOLUME_PATH" -czf "$BACKUP/tsk-s2-lite.tgz" .
sudo tar -C / -czf "$BACKUP/etc-tsk-sync.tgz" etc/tsk-sync
sudo cp -a /etc/systemd/system/tsk-s2.service /etc/systemd/system/tsk-sync.service "$BACKUP/"
sudo chown -R "$(id -u):$(id -g)" "$BACKUP"
chmod -R go-rwx "$BACKUP"

sudo systemctl start tsk-s2.service
sudo systemctl start tsk-sync.service
curl -fsS http://127.0.0.1:8000/health
```

The `/etc/tsk-sync` archive contains every bearer and the S2 token. Encrypt and restrict
the backup as secret material.

## Recovery

Recovery replaces the entire prototype volume and `/etc/tsk-sync` configuration. Check
the paths before running the destructive `find` command.

```sh
BACKUP=/home/exedev/backups/REPLACE_WITH_BACKUP_DIRECTORY
sudo systemctl stop tsk-sync.service
sudo systemctl stop tsk-s2.service

docker volume create tsk-s2-lite >/dev/null
VOLUME_PATH=$(docker volume inspect tsk-s2-lite --format '{{.Mountpoint}}')
sudo find "$VOLUME_PATH" -mindepth 1 -maxdepth 1 -exec rm -rf -- {} +
sudo tar -C "$VOLUME_PATH" -xzf "$BACKUP/tsk-s2-lite.tgz"
sudo tar -C / -xzf "$BACKUP/etc-tsk-sync.tgz"
sudo chown -R exedev:exedev /etc/tsk-sync
sudo chmod 0700 /etc/tsk-sync
sudo chmod 0600 /etc/tsk-sync/config.json
sudo install -m 0644 "$BACKUP/tsk-s2.service" /etc/systemd/system/tsk-s2.service
sudo install -m 0644 "$BACKUP/tsk-sync.service" /etc/systemd/system/tsk-sync.service

sudo systemctl daemon-reload
sudo systemctl start tsk-s2.service
sudo systemctl start tsk-sync.service
curl -fsS http://127.0.0.1:8000/health
cd /home/exedev/tsk
python3 scripts/sync-demo-check.py \
  --server-config /etc/tsk-sync/config.json \
  --client-a-dir /home/exedev/.local/share/tsk-sync-demo/client-a \
  --client-b-dir /home/exedev/.local/share/tsk-sync-demo/client-b
```

Client cache directories are not authoritative and do not need to be restored. Existing
client pending-operation files should be retained until that exact operation has been
reconciled; do not delete one merely because a previous response was lost.

## Prototype limitations

- Every accepted event stores a full `DomainState`; there is no compaction or retention
  policy, so log and replay cost grow with every operation.
- Static bearer tokens and workspace membership come from one local JSON file. There is no
  rotation endpoint, expiry, audit UI, or dynamic membership; changes require API restart.
- Workspace serialization is an in-process mutex. Running multiple API replicas is not a
  supported deployment even though S2 conditional append detects tail races.
- Shared writes require connectivity. Offline reads use the last private cache and are
  explicitly stale; there is no offline write queue beyond retrying one persisted ambiguous
  operation.
- Shared undo, shared project archive/unarchive, hard removal, and trash-file restore are
  disabled. Soft-deleted tasks remain authoritative tombstones.
- Every shared project identity must have a local absolute-path mapping in every client.
  Project names/UUIDs are configuration, not a discovery protocol.
- Failure-injection headers can terminate the API process. Keep
  `TSK_SYNC_ENABLE_FAILURE_INJECTION` off anywhere that is not an isolated acceptance VM.
- The API has application bearer authorization but no native TLS. Use the exe.dev proxy for
  browser access or an SSH tunnel for current CLI clients; never publish loopback S2 directly.
- This prototype has not established production backup SLAs, schema evolution across mixed
  client versions, observability/alerting, rate limiting, or a disaster-recovery objective.
