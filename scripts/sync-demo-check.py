#!/usr/bin/env python3
"""Append-only acceptance checks for the real tsk sync prototype.

The smoke profile avoids service restarts and failure injection. The full profile preserves
all 16 historical checks and is disruptive; prefer ``scripts/sync-demo test`` so its
failure-injection drop-in is always cleaned up.

The checker deliberately uses unique task titles and operation UUIDs. It never clears an
S2 stream, and it never prints bearer values. It expects an already running API at
http://127.0.0.1:8000 backed by S2 Lite.
"""

from __future__ import annotations

import argparse
import concurrent.futures
import copy
import json
import os
import shutil
import socket
import stat
import subprocess
import sys
import tempfile
import time
import urllib.error
import urllib.parse
import urllib.request
import uuid
from dataclasses import dataclass
from pathlib import Path
from typing import Any, Callable

DEFAULT_BASE_URL = "http://127.0.0.1:8000/"
EXPECTED_S2_VERSION = "0.42.12"
EXPECTED_S2_DIGEST = "sha256:75fb787f3d1b27572100ddcca704dc0cbc85936d899f0c88f66a2549f216fdc6"
EXPECTED_SDK_VERSION = "0.34.8"


class CheckFailure(RuntimeError):
    pass


class CheckSkip(RuntimeError):
    pass


@dataclass
class HttpResult:
    status: int
    body: Any


@dataclass
class Result:
    name: str
    outcome: str
    detail: str = ""


class Acceptance:
    def __init__(self, args: argparse.Namespace) -> None:
        self.args = args
        self.repo = Path(__file__).resolve().parents[1]
        self.tsk = Path(args.tsk).resolve()
        self.server_config_path = Path(args.server_config).resolve()
        self.client_a_dir = Path(args.client_a_dir).resolve()
        self.client_b_dir = Path(args.client_b_dir).resolve()
        self.base_url = args.base_url.rstrip("/") + "/"
        self.run_id = f"{int(time.time())}-{uuid.uuid4().hex[:10]}"
        self.results: list[Result] = []
        self.secrets: set[str] = set()

        self.server_config = self._read_private_json(self.server_config_path, "server config")
        self.client_a = self._read_private_json(self.client_a_dir / "sync.json", "client A config")
        self.client_b = self._read_private_json(self.client_b_dir / "sync.json", "client B config")
        self._collect_secrets()
        self.workspace = self._common_workspace()
        self.token_a = self._string(self.client_a, "bearer", "client A config")
        self.token_b = self._string(self.client_b, "bearer", "client B config")
        self.unauthorized_token = self._find_unauthorized_token()

    def _collect_secrets(self) -> None:
        for document in (self.server_config, self.client_a, self.client_b):
            self._collect_document_secrets(document)

    def _collect_document_secrets(self, value: Any, key: str = "") -> None:
        if isinstance(value, dict):
            for child_key, child in value.items():
                self._collect_document_secrets(child, child_key)
        elif isinstance(value, list):
            for child in value:
                self._collect_document_secrets(child, key)
        elif key in {"bearer", "access_token"} and isinstance(value, str) and value:
            self.secrets.add(value)

    def sanitize(self, message: object) -> str:
        text = str(message).replace("\n", " ").strip()
        for secret in sorted(self.secrets, key=len, reverse=True):
            text = text.replace(secret, "<redacted>")
        return text

    @staticmethod
    def _string(document: dict[str, Any], key: str, label: str) -> str:
        value = document.get(key)
        if not isinstance(value, str) or not value:
            raise CheckFailure(f"{label} must contain a non-empty {key}")
        return value

    def _read_private_json(self, path: Path, label: str) -> dict[str, Any]:
        try:
            metadata = path.lstat()
        except OSError as error:
            raise CheckFailure(f"cannot inspect {label} {path}: {error}") from error
        if not stat.S_ISREG(metadata.st_mode) or path.is_symlink():
            raise CheckFailure(f"{label} must be a regular file, not a symlink: {path}")
        if os.name == "posix" and stat.S_IMODE(metadata.st_mode) != 0o600:
            raise CheckFailure(f"{label} permissions must be exactly 0600: {path}")
        try:
            value = json.loads(path.read_text(encoding="utf-8"))
        except (OSError, json.JSONDecodeError) as error:
            raise CheckFailure(f"cannot read {label} {path}: {error}") from error
        if not isinstance(value, dict):
            raise CheckFailure(f"{label} must be a JSON object: {path}")
        return value

    def _common_workspace(self) -> str:
        a = self._string(self.client_a, "workspace", "client A config")
        b = self._string(self.client_b, "workspace", "client B config")
        if a != b:
            raise CheckFailure(f"client workspaces differ: {a!r} != {b!r}")
        if a not in self._server_workspaces():
            raise CheckFailure(f"workspace {a!r} is not present in the server config")
        return a

    def _server_workspaces(self) -> set[str]:
        found: set[str] = set()
        for record in self.server_config.get("tokens", []):
            if isinstance(record, dict) and isinstance(record.get("workspaces"), list):
                found.update(value for value in record["workspaces"] if isinstance(value, str))
        return found

    def _find_unauthorized_token(self) -> str | None:
        for record in self.server_config.get("tokens", []):
            if not isinstance(record, dict):
                continue
            bearer = record.get("bearer")
            workspaces = record.get("workspaces")
            if (
                isinstance(bearer, str)
                and bearer
                and isinstance(workspaces, list)
                and self.workspace not in workspaces
            ):
                self.secrets.add(bearer)
                return bearer
        return None

    def run(self, name: str, function: Callable[[], str | None]) -> None:
        try:
            detail = function() or ""
        except CheckSkip as error:
            self.results.append(Result(name, "SKIP", self.sanitize(error)))
        except Exception as error:  # keep independent acceptance checks running
            self.results.append(Result(name, "FAIL", self.sanitize(error)))
        else:
            self.results.append(Result(name, "PASS", self.sanitize(detail)))

    def print_summary(self) -> int:
        print("\ntsk sync acceptance summary")
        print("=" * 28)
        for result in self.results:
            suffix = f" — {result.detail}" if result.detail else ""
            print(f"{result.outcome:4} {result.name}{suffix}")
        counts = {outcome: sum(result.outcome == outcome for result in self.results) for outcome in ("PASS", "SKIP", "FAIL")}
        print(f"\nPASS={counts['PASS']} SKIP={counts['SKIP']} FAIL={counts['FAIL']}")
        return 1 if counts["FAIL"] or (getattr(self.args, "fail_on_skip", False) and counts["SKIP"]) else 0

    def url(self, relative: str) -> str:
        return urllib.parse.urljoin(self.base_url, relative.lstrip("/"))

    def http(
        self,
        method: str,
        relative: str,
        token: str | None = None,
        body: Any | None = None,
        headers: dict[str, str] | None = None,
        timeout: float = 12.0,
    ) -> HttpResult:
        request_headers = {"Accept": "application/json"}
        if token is not None:
            request_headers["Authorization"] = f"Bearer {token}"
        if headers:
            request_headers.update(headers)
        data = None
        if body is not None:
            data = json.dumps(body, separators=(",", ":")).encode("utf-8")
            request_headers["Content-Type"] = "application/json"
        request = urllib.request.Request(
            self.url(relative), data=data, headers=request_headers, method=method
        )
        try:
            with urllib.request.urlopen(request, timeout=timeout) as response:
                raw = response.read()
                return HttpResult(response.status, self._decode_body(raw))
        except urllib.error.HTTPError as error:
            return HttpResult(error.code, self._decode_body(error.read()))

    @staticmethod
    def _decode_body(raw: bytes) -> Any:
        if not raw:
            return None
        try:
            return json.loads(raw)
        except json.JSONDecodeError:
            return raw.decode("utf-8", errors="replace")

    @staticmethod
    def require_status(result: HttpResult, expected: int, context: str) -> Any:
        if result.status != expected:
            detail = result.body.get("message") if isinstance(result.body, dict) else result.body
            raise CheckFailure(f"{context}: expected HTTP {expected}, got {result.status}: {detail}")
        return result.body

    def snapshot(self, token: str | None = None) -> dict[str, Any]:
        result = self.http(
            "GET",
            f"v1/workspaces/{urllib.parse.quote(self.workspace)}/snapshot",
            self.token_a if token is None else token,
        )
        body = self.require_status(result, 200, "snapshot")
        if not isinstance(body, dict) or not isinstance(body.get("state"), dict):
            raise CheckFailure("snapshot response is not a state document")
        return body

    def events(self, from_sequence: int, token: str | None = None) -> dict[str, Any]:
        query = urllib.parse.urlencode(
            {"from": from_sequence, "limit": 100, "follow": "false"}
        )
        result = self.http(
            "GET",
            f"v1/workspaces/{urllib.parse.quote(self.workspace)}/events?{query}",
            self.token_a if token is None else token,
        )
        body = self.require_status(result, 200, "events")
        if not isinstance(body, dict) or not isinstance(body.get("events"), list):
            raise CheckFailure("events response is not an event document")
        return body

    def mutation(self, request: dict[str, Any], token: str | None = None, failure: str | None = None) -> HttpResult:
        headers = {"x-tsk-sync-failure": failure} if failure else None
        return self.http(
            "POST",
            f"v1/workspaces/{urllib.parse.quote(self.workspace)}/mutations",
            self.token_a if token is None else token,
            request,
            headers,
        )

    def tsk_command(self, state_dir: Path, arguments: list[str], expected: int = 0) -> subprocess.CompletedProcess[str]:
        env = os.environ.copy()
        env.pop("TSK_SYNC_CONFIG", None)
        command = [str(self.tsk), *arguments, "--state-dir", str(state_dir)]
        completed = subprocess.run(
            command,
            cwd=self.repo,
            env=env,
            text=True,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            timeout=35,
            check=False,
        )
        if completed.returncode != expected:
            raise CheckFailure(
                f"tsk command exited {completed.returncode}, expected {expected}: "
                f"{completed.stderr.strip() or completed.stdout.strip()}"
            )
        return completed

    def tsk_json(self, state_dir: Path, arguments: list[str]) -> Any:
        completed = self.tsk_command(state_dir, arguments)
        try:
            return json.loads(completed.stdout)
        except json.JSONDecodeError as error:
            raise CheckFailure(f"tsk returned invalid JSON: {completed.stdout.strip()}") from error

    def add(self, state_dir: Path, title: str, project: str | None = None) -> dict[str, Any]:
        arguments = ["add", "--title", title]
        if project is None:
            arguments.append("--desk")
        else:
            arguments.extend(["--project", project])
        arguments.append("--json")
        value = self.tsk_json(state_dir, arguments)
        if not isinstance(value, dict) or value.get("outcome") != "created":
            raise CheckFailure(f"expected a newly created task, got {value!r}")
        return value

    def task(self, state_dir: Path, task_id: str) -> dict[str, Any]:
        value = self.tsk_json(state_dir, ["list", task_id, "--json"])
        if isinstance(value, list):
            if len(value) != 1:
                raise CheckFailure(f"direct task lookup returned {len(value)} rows")
            value = value[0]
        if not isinstance(value, dict) or value.get("id") != task_id:
            raise CheckFailure(f"direct task lookup did not return {task_id}")
        return value

    def unique_title(self, label: str) -> str:
        return f"sync-check {self.run_id} {label}"

    @staticmethod
    def find_task(state: dict[str, Any], task_id: str) -> dict[str, Any]:
        for task in state.get("tasks", []):
            if isinstance(task, dict) and task.get("id") == task_id:
                return task
        raise CheckFailure(f"authoritative state omitted task {task_id}")

    @staticmethod
    def timestamp() -> list[int]:
        nanoseconds = time.time_ns()
        return [nanoseconds // 1_000_000_000, nanoseconds % 1_000_000_000]

    def update_request(
        self,
        snapshot: dict[str, Any],
        task_id: str,
        title: str,
        operation_id: str | None = None,
    ) -> dict[str, Any]:
        state = copy.deepcopy(snapshot["state"])
        task = self.find_task(state, task_id)
        base_revision = task["revision"]
        result_revision = str(uuid.uuid4())
        at = self.timestamp()
        task["merge_base_revision"] = base_revision
        task["revision"] = result_revision
        task["title"] = title
        task["updated_at"] = at
        task.setdefault("history", []).append({"kind": "edited", "at": at})
        return {
            "operation_id": operation_id or str(uuid.uuid4()),
            "expected": [
                {
                    "task_id": task_id,
                    "version": {
                        "kind": "revision",
                        "revision": base_revision,
                        "tombstone": bool(task.get("soft_deleted", False)),
                    },
                }
            ],
            "changes": [
                {
                    "task_id": task_id,
                    "kind": "update",
                    "base_revision": base_revision,
                    "result_revision": result_revision,
                }
            ],
            "proposed_state": state,
        }

    def delete_request(
        self, snapshot: dict[str, Any], task_id: str, operation_id: str | None = None
    ) -> dict[str, Any]:
        state = copy.deepcopy(snapshot["state"])
        task = self.find_task(state, task_id)
        base_revision = task["revision"]
        result_revision = str(uuid.uuid4())
        at = self.timestamp()
        task["merge_base_revision"] = base_revision
        task["revision"] = result_revision
        task["soft_deleted"] = True
        task["updated_at"] = at
        task.setdefault("history", []).append({"kind": "soft_deleted", "at": at})
        return {
            "operation_id": operation_id or str(uuid.uuid4()),
            "expected": [
                {
                    "task_id": task_id,
                    "version": {
                        "kind": "revision",
                        "revision": base_revision,
                        "tombstone": False,
                    },
                }
            ],
            "changes": [
                {
                    "task_id": task_id,
                    "kind": "delete",
                    "base_revision": base_revision,
                    "result_revision": result_revision,
                }
            ],
            "proposed_state": state,
        }

    @staticmethod
    def mutation_sequence(body: Any) -> int:
        try:
            return int(body["result"]["sequence"])
        except (KeyError, TypeError, ValueError) as error:
            raise CheckFailure("mutation response omitted its sequence") from error

    def assert_one_operation_event(self, sequence: int, operation_id: str) -> None:
        document = self.events(sequence)
        occurrences = sum(
            event.get("event", {}).get("operation_id") == operation_id
            for event in document["events"]
            if isinstance(event, dict)
        )
        if occurrences != 1:
            raise CheckFailure(
                f"operation {operation_id} appeared {occurrences} times at/after sequence {sequence}"
            )

    def systemctl_prefix(self) -> list[str]:
        if shutil.which("systemctl") is None:
            raise CheckSkip("systemctl is unavailable")
        if os.geteuid() == 0:
            return ["systemctl"]
        if shutil.which("sudo") is None:
            raise CheckSkip("passwordless service restart is unavailable")
        probe = subprocess.run(
            ["sudo", "-n", "true"], stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL
        )
        if probe.returncode != 0:
            raise CheckSkip("passwordless service restart is unavailable")
        return ["sudo", "-n", "systemctl"]

    def unit_is_active(self, unit: str) -> bool:
        return subprocess.run(
            ["systemctl", "--quiet", "is-active", unit],
            stdout=subprocess.DEVNULL,
            stderr=subprocess.DEVNULL,
        ).returncode == 0

    def reset_unit_counter(self, unit: str) -> None:
        subprocess.run(
            [*self.systemctl_prefix(), "reset-failed", unit],
            stdout=subprocess.DEVNULL,
            stderr=subprocess.DEVNULL,
            timeout=20,
        )

    def restart_unit(self, unit: str) -> None:
        loaded = subprocess.run(
            ["systemctl", "show", unit, "--property=LoadState", "--value"],
            text=True,
            stdout=subprocess.PIPE,
            stderr=subprocess.DEVNULL,
        ).stdout.strip()
        if loaded != "loaded":
            raise CheckSkip(f"{unit} is not loaded")
        prefix = self.systemctl_prefix()
        # The acceptance run deliberately restarts and crashes the API several times.
        # Clear systemd's start-rate counter before each controlled restart so a valid
        # later replay check is not mistaken for a crash loop.
        self.reset_unit_counter(unit)
        action = "restart" if self.unit_is_active(unit) else "start"
        completed = subprocess.run(
            [*prefix, action, unit],
            text=True,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            timeout=90,
        )
        if completed.returncode != 0:
            self.reset_unit_counter(unit)
            completed = subprocess.run(
                [*prefix, "start", unit],
                text=True,
                stdout=subprocess.PIPE,
                stderr=subprocess.PIPE,
                timeout=90,
            )
        if completed.returncode != 0:
            raise CheckSkip(f"cannot restart {unit}: {completed.stderr.strip()}")

    @staticmethod
    def unit_pid(unit: str) -> int | None:
        completed = subprocess.run(
            ["systemctl", "show", unit, "--property=MainPID", "--value"],
            text=True,
            stdout=subprocess.PIPE,
            stderr=subprocess.DEVNULL,
        )
        try:
            value = int(completed.stdout.strip())
        except ValueError:
            return None
        return value or None

    def wait_for_health(self, timeout: float = 45.0) -> None:
        deadline = time.monotonic() + timeout
        last_error = "service did not answer"
        while time.monotonic() < deadline:
            try:
                result = self.http("GET", "health", timeout=2)
                if result.status == 200 and isinstance(result.body, dict) and result.body.get("status") == "ok":
                    return
                last_error = f"HTTP {result.status}"
            except OSError as error:
                last_error = str(error)
            time.sleep(0.4)
        raise CheckFailure(f"sync API did not become healthy: {last_error}")

    @staticmethod
    def wait_for_tcp(host: str, port: int, timeout: float = 30.0) -> None:
        deadline = time.monotonic() + timeout
        last_error = "endpoint did not answer"
        while time.monotonic() < deadline:
            try:
                with socket.create_connection((host, port), timeout=1):
                    # S2 Lite binds only after its local database fencing delay.
                    time.sleep(0.4)
                    return
            except OSError as error:
                last_error = str(error)
                time.sleep(0.3)
        raise CheckFailure(f"S2 did not become reachable at {host}:{port}: {last_error}")

    def check_preflight(self) -> str:
        for label, path in (
            ("server config", self.server_config_path),
            ("client A state directory", self.client_a_dir),
            ("client B state directory", self.client_b_dir),
        ):
            if path == self.repo or self.repo in path.parents:
                raise CheckFailure(f"{label} must be external to the repository: {path}")
        if not self.tsk.is_file() or not os.access(self.tsk, os.X_OK):
            raise CheckFailure(f"tsk binary is not executable: {self.tsk}")
        if self.client_a_dir == self.client_b_dir:
            raise CheckFailure("client A and B state directories must be different")
        if self.token_a == self.token_b:
            raise CheckFailure("client A and B must use different bearer identities")
        for label, document, token in (
            ("A", self.client_a, self.token_a),
            ("B", self.client_b, self.token_b),
        ):
            endpoint = self._string(document, "endpoint", f"client {label} config").rstrip("/") + "/"
            if endpoint != self.base_url:
                raise CheckFailure(
                    f"client {label} endpoint {endpoint!r} does not match checker endpoint {self.base_url!r}"
                )
            matching = [
                record
                for record in self.server_config.get("tokens", [])
                if isinstance(record, dict) and record.get("bearer") == token
            ]
            if len(matching) != 1 or self.workspace not in matching[0].get("workspaces", []):
                raise CheckFailure(f"client {label} bearer is not authorized for {self.workspace}")
        self.wait_for_health()
        snapshot = self.snapshot()
        if snapshot.get("workspace") != self.workspace:
            raise CheckFailure("live API returned the wrong workspace")
        s2 = self.server_config.get("s2")
        if not isinstance(s2, dict) or not s2.get("account_endpoint") or not s2.get("basin_endpoint"):
            raise CheckFailure("server config does not name real S2 endpoints")
        return f"workspace={self.workspace}, cursor={snapshot.get('next_sequence')}"

    def check_pins(self) -> str:
        cargo = (self.repo / "Cargo.toml").read_text(encoding="utf-8")
        if f's2-sdk = {{ version = "={EXPECTED_SDK_VERSION}"' not in cargo:
            raise CheckFailure(f"Cargo.toml does not pin s2-sdk {EXPECTED_SDK_VERSION}")
        details = [f"SDK {EXPECTED_SDK_VERSION}"]
        s2 = shutil.which("s2")
        if s2:
            completed = subprocess.run(
                [s2, "--version"], text=True, stdout=subprocess.PIPE, stderr=subprocess.STDOUT
            )
            if EXPECTED_S2_VERSION not in completed.stdout:
                raise CheckFailure(f"s2 CLI is not {EXPECTED_S2_VERSION}: {completed.stdout.strip()}")
            details.append(f"CLI {EXPECTED_S2_VERSION}")
        if shutil.which("docker") and subprocess.run(
            ["docker", "inspect", "tsk-s2-lite"],
            stdout=subprocess.DEVNULL,
            stderr=subprocess.DEVNULL,
        ).returncode == 0:
            image = subprocess.run(
                ["docker", "inspect", "tsk-s2-lite", "--format", "{{.Config.Image}}"],
                text=True,
                stdout=subprocess.PIPE,
                stderr=subprocess.PIPE,
            ).stdout.strip()
            if EXPECTED_S2_DIGEST not in image:
                raise CheckFailure(f"running tsk-s2-lite image is not pinned to {EXPECTED_S2_DIGEST}")
            details.append("Lite digest pinned")
        return ", ".join(details)

    def check_a_create_b_receive(self) -> str:
        title = self.unique_title("A-create-B-receive")
        created = self.add(self.client_a_dir, title)
        received = self.task(self.client_b_dir, created["id"])
        if received.get("title") != title:
            raise CheckFailure("client B received a different title")
        return f"T{created['number']} propagated A→B"

    def check_b_update_a_receive(self) -> str:
        title = self.unique_title("B-status-A-receive")
        created = self.add(self.client_a_dir, title)
        self.tsk_command(self.client_b_dir, ["status", created["id"], "started"])
        received = self.task(self.client_a_dir, created["id"])
        if received.get("status") != "started":
            raise CheckFailure("client A did not observe B's started status")
        return f"T{created['number']} propagated B→A"

    def check_concurrent_numbers(self) -> str:
        title_a = self.unique_title("concurrent-A")
        title_b = self.unique_title("concurrent-B")
        with concurrent.futures.ThreadPoolExecutor(max_workers=2) as executor:
            future_a = executor.submit(self.add, self.client_a_dir, title_a)
            future_b = executor.submit(self.add, self.client_b_dir, title_b)
            created_a = future_a.result()
            created_b = future_b.result()
        if created_a["id"] == created_b["id"] or created_a["number"] == created_b["number"]:
            raise CheckFailure("concurrent creates did not receive distinct IDs and task numbers")
        return f"assigned T{created_a['number']} and T{created_b['number']}"

    def check_stale_conflict(self) -> str:
        created = self.add(self.client_a_dir, self.unique_title("stale-conflict-base"))
        base = self.snapshot()
        winner = self.update_request(base, created["id"], self.unique_title("stale-winner"))
        stale = self.update_request(base, created["id"], self.unique_title("stale-loser"))
        self.require_status(self.mutation(winner, self.token_a), 200, "winning direct update")
        rejected = self.mutation(stale, self.token_b)
        body = self.require_status(rejected, 409, "stale direct update")
        if not isinstance(body, dict) or body.get("code") != "conflict":
            raise CheckFailure("stale update was not classified as a conflict")
        authoritative = self.find_task(self.snapshot()["state"], created["id"])
        expected = self.find_task(winner["proposed_state"], created["id"])["title"]
        if authoritative["title"] != expected:
            raise CheckFailure("stale conflict changed the authoritative winner")
        return "HTTP 409 preserved the winning revision"

    def check_lost_response_and_retry(self) -> str:
        created = self.add(self.client_a_dir, self.unique_title("lost-response-base"))
        request = self.update_request(
            self.snapshot(),
            created["id"],
            self.unique_title("lost-response-accepted"),
        )
        first = self.mutation(request, failure="lost_response_after_append")
        if first.status == 403:
            raise CheckSkip("x-tsk-sync-failure is disabled")
        self.require_status(first, 503, "lost-response injection")
        retry = self.mutation(request)
        body = self.require_status(retry, 200, "exact retry after lost response")
        sequence = self.mutation_sequence(body)
        self.assert_one_operation_event(sequence, request["operation_id"])

        self.restart_unit(self.args.api_service)
        self.wait_for_health()
        after_restart = self.require_status(
            self.mutation(request), 200, "exact retry after API restart"
        )
        if self.mutation_sequence(after_restart) != sequence:
            raise CheckFailure("API restart changed the deduplicated operation sequence")
        self.assert_one_operation_event(sequence, request["operation_id"])
        return f"operation remained single at sequence {sequence} across API restart"

    def check_error_before_projection(self) -> str:
        created = self.add(self.client_a_dir, self.unique_title("projection-error-base"))
        request = self.update_request(
            self.snapshot(), created["id"], self.unique_title("projection-error-accepted")
        )
        first = self.mutation(request, failure="error_before_projection_update")
        if first.status == 403:
            raise CheckSkip("x-tsk-sync-failure is disabled")
        self.require_status(first, 503, "projection-error injection")
        self.snapshot()  # forces catch-up from S2 before retry
        body = self.require_status(self.mutation(request), 200, "projection-error retry")
        sequence = self.mutation_sequence(body)
        self.assert_one_operation_event(sequence, request["operation_id"])
        return f"projection caught up to durable sequence {sequence}"

    def check_crash_before_projection(self) -> str:
        if not self.unit_is_active(self.args.api_service):
            raise CheckSkip(f"{self.args.api_service} is not active")
        self.reset_unit_counter(self.args.api_service)
        old_pid = self.unit_pid(self.args.api_service)
        created = self.add(self.client_a_dir, self.unique_title("projection-crash-base"))
        request = self.update_request(
            self.snapshot(), created["id"], self.unique_title("projection-crash-accepted")
        )
        try:
            first = self.mutation(
                request, failure="crash_before_projection_update"
            )
        except (urllib.error.URLError, ConnectionError, TimeoutError, OSError):
            first = None
        if first is not None and first.status == 403:
            raise CheckSkip("x-tsk-sync-failure is disabled")
        if first is not None and first.status < 500:
            raise CheckFailure(f"crash injection unexpectedly returned HTTP {first.status}")
        self.wait_for_health()
        new_pid = self.unit_pid(self.args.api_service)
        if old_pid is not None and new_pid == old_pid:
            raise CheckFailure("API PID did not change after crash injection")
        body = self.require_status(self.mutation(request), 200, "crash retry")
        sequence = self.mutation_sequence(body)
        self.assert_one_operation_event(sequence, request["operation_id"])
        return f"systemd recovered and operation remained single at sequence {sequence}"

    def check_cursor_catch_up(self) -> str:
        cursor = int(self.snapshot()["next_sequence"])
        title = self.unique_title("cursor-catch-up")
        self.add(self.client_a_dir, title)
        document = self.events(cursor, self.token_b)
        titles = {
            task.get("title")
            for sequenced in document["events"]
            if isinstance(sequenced, dict)
            for task in sequenced.get("event", {}).get("state", {}).get("tasks", [])
            if isinstance(task, dict)
        }
        if title not in titles or int(document.get("next_sequence", cursor)) <= cursor:
            raise CheckFailure("resumed cursor did not catch up the disconnected client")
        return f"cursor advanced from {cursor} to {document['next_sequence']}"

    def check_s2_api_restart_persistence(self) -> str:
        created = self.add(self.client_a_dir, self.unique_title("S2-API-restart"))
        before = self.snapshot()
        self.restart_unit(self.args.s2_service)
        s2_endpoint = urllib.parse.urlparse(self.server_config["s2"]["basin_endpoint"])
        if not s2_endpoint.hostname:
            raise CheckFailure("server S2 basin endpoint has no host")
        s2_port = s2_endpoint.port or (443 if s2_endpoint.scheme == "https" else 80)
        self.wait_for_tcp(s2_endpoint.hostname, s2_port)
        self.restart_unit(self.args.api_service)
        self.wait_for_health(timeout=60)
        after = self.snapshot()
        task = self.find_task(after["state"], created["id"])
        if task.get("title") != created["title"]:
            raise CheckFailure("task did not survive the S2/API restart")
        if int(after["next_sequence"]) < int(before["next_sequence"]):
            raise CheckFailure("stream cursor moved backwards after S2/API restart")
        return f"T{created['number']} survived both service restarts"

    def check_projection_replay(self) -> str:
        before = self.snapshot()
        self.restart_unit(self.args.api_service)
        self.wait_for_health()
        after = self.snapshot()
        if before["next_sequence"] != after["next_sequence"] or before["state"] != after["state"]:
            raise CheckFailure("projection replay did not reproduce the exact snapshot")
        return f"replayed {after['next_sequence']} events to an identical projection"

    def check_tombstone_stale_update(self) -> str:
        created = self.add(self.client_a_dir, self.unique_title("tombstone-base"))
        base = self.snapshot()
        deletion = self.delete_request(base, created["id"])
        stale = self.update_request(base, created["id"], self.unique_title("must-not-resurrect"))
        self.require_status(self.mutation(deletion), 200, "direct tombstone")
        rejected = self.mutation(stale, self.token_b)
        body = self.require_status(rejected, 409, "stale update after tombstone")
        if not isinstance(body, dict) or body.get("code") != "conflict":
            raise CheckFailure("stale resurrection was not classified as a conflict")
        task = self.find_task(self.snapshot()["state"], created["id"])
        stale_title = self.find_task(stale["proposed_state"], created["id"])["title"]
        if not task.get("soft_deleted") or task.get("title") == stale_title:
            raise CheckFailure("stale update resurrected or changed the tombstone")
        return f"T{created['number']} remained tombstoned"

    def check_unauthorized(self) -> str:
        if self.unauthorized_token is None:
            raise CheckSkip(
                "server config has no bearer identity outside the checked workspace"
            )
        workspace = urllib.parse.quote(self.workspace)
        requests = [
            ("read", self.http("GET", f"v1/workspaces/{workspace}/snapshot", self.unauthorized_token)),
            (
                "write",
                self.http(
                    "POST",
                    f"v1/workspaces/{workspace}/mutations",
                    self.unauthorized_token,
                    {},
                ),
            ),
            (
                "events",
                self.http(
                    "GET",
                    f"v1/workspaces/{workspace}/events?from=0&limit=1",
                    self.unauthorized_token,
                ),
            ),
        ]
        for label, result in requests:
            body = self.require_status(result, 403, f"unauthorized {label}")
            if not isinstance(body, dict) or body.get("code") != "forbidden":
                raise CheckFailure(f"unauthorized {label} did not return forbidden")
        invalid = self.http("GET", f"v1/workspaces/{workspace}/snapshot", f"invalid-{uuid.uuid4()}")
        self.require_status(invalid, 401, "invalid bearer")
        return "read/write/events returned 403; invalid bearer returned 401"

    def project_mapping(self, document: dict[str, Any], label: str) -> dict[str, Any]:
        projects = document.get("projects")
        if not isinstance(projects, list) or not projects or not isinstance(projects[0], dict):
            raise CheckFailure(f"client {label} config needs at least one project mapping")
        return projects[0]

    def check_project_mapping(self) -> str:
        mapping_a = self.project_mapping(self.client_a, "A")
        mapping_b = self.project_mapping(self.client_b, "B")
        for key in ("id", "name", "path"):
            if not isinstance(mapping_a.get(key), str) or not isinstance(mapping_b.get(key), str):
                raise CheckFailure(f"project mappings must contain string {key} values")
        if (mapping_a["id"], mapping_a["name"]) != (mapping_b["id"], mapping_b["name"]):
            raise CheckFailure("client project mappings do not name the same shared project")
        if Path(mapping_a["path"]).resolve() == Path(mapping_b["path"]).resolve():
            raise CheckFailure("client project mappings must use different local paths")
        for mapping in (mapping_a, mapping_b):
            if not Path(mapping["path"]).is_dir():
                raise CheckFailure(f"mapped local project path does not exist: {mapping['path']}")
        title = self.unique_title("mapped-project")
        created = self.add(self.client_a_dir, title, mapping_a["path"])
        received = self.task(self.client_b_dir, created["id"])
        if received.get("project") != mapping_b["path"]:
            raise CheckFailure(
                f"client B saw project {received.get('project')!r}, expected its local mapping"
            )
        return f"shared project {mapping_a['name']} translated A path→B path"

    def check_local_only(self) -> str:
        title = self.unique_title("local-only")
        before_ids = {task.get("id") for task in self.snapshot()["state"].get("tasks", [])}
        with tempfile.TemporaryDirectory(prefix="tsk-sync-local-only-") as directory:
            state_dir = Path(directory) / "state"
            created = self.add(state_dir, title)
            local = self.task(state_dir, created["id"])
            if local.get("title") != title or not (state_dir / "tsk.json").is_file():
                raise CheckFailure("local-only tsk did not persist to tsk.json")
            if (state_dir / "sync-cache.json").exists():
                raise CheckFailure("local-only tsk unexpectedly created a sync cache")
            after_ids = {task.get("id") for task in self.snapshot()["state"].get("tasks", [])}
            if created["id"] in after_ids or before_ids != after_ids:
                raise CheckFailure("local-only mutation leaked into the shared workspace")
        return "isolated tsk.json stayed outside S2"


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(
        description="Run append-only acceptance checks against the real local tsk-sync service."
    )
    parser.add_argument("--server-config", required=True, help="external 0600 tsk-sync server JSON")
    parser.add_argument("--client-a-dir", required=True, help="external client A state directory")
    parser.add_argument("--client-b-dir", required=True, help="external client B state directory")
    parser.add_argument("--tsk", default="target/release/tsk", help="built tsk binary")
    parser.add_argument("--base-url", default=DEFAULT_BASE_URL)
    parser.add_argument("--api-service", default="tsk-sync.service")
    parser.add_argument("--s2-service", default="tsk-s2.service")
    parser.add_argument(
        "--profile",
        choices=("smoke", "full"),
        default="full",
        help="smoke avoids service restarts and failure injection; full runs all 16 checks",
    )
    parser.add_argument(
        "--fail-on-skip",
        action="store_true",
        help="return nonzero when any selected check is skipped",
    )
    return parser.parse_args()


def checks_for_profile(
    acceptance: Acceptance, profile: str
) -> list[tuple[str, Callable[[], str | None]]]:
    smoke: list[tuple[str, Callable[[], str | None]]] = [
        ("preflight: live API, S2 config, two external clients", acceptance.check_preflight),
        ("pinned S2 0.42.12/digest and SDK 0.34.8", acceptance.check_pins),
        ("A create → B receive", acceptance.check_a_create_b_receive),
        ("B status → A receive", acceptance.check_b_update_a_receive),
        ("concurrent creates receive distinct numbers", acceptance.check_concurrent_numbers),
        ("direct stale same-task conflict", acceptance.check_stale_conflict),
        ("disconnect/cursor catch-up", acceptance.check_cursor_catch_up),
        ("stale update cannot resurrect tombstone", acceptance.check_tombstone_stale_update),
        ("unauthorized read/write/events", acceptance.check_unauthorized),
        ("two local paths map to one shared project", acceptance.check_project_mapping),
        ("local-only tsk remains local", acceptance.check_local_only),
    ]
    if profile == "smoke":
        return smoke
    if profile != "full":
        raise ValueError(f"unknown acceptance profile {profile!r}")
    disruptive = [
        ("lost response + exact retry + API restart dedupe", acceptance.check_lost_response_and_retry),
        ("error before projection update", acceptance.check_error_before_projection),
        ("crash before projection update + systemd retry", acceptance.check_crash_before_projection),
        ("S2 + API restart persistence", acceptance.check_s2_api_restart_persistence),
        ("projection replay equivalence after API restart", acceptance.check_projection_replay),
    ]
    # Keep the full profile in the historical 16-check order.
    return [*smoke[:6], *disruptive[:3], smoke[6], *disruptive[3:], *smoke[7:]]


def main() -> int:
    args = parse_args()
    try:
        acceptance = Acceptance(args)
    except Exception as error:
        print(f"FAIL setup — {error}", file=sys.stderr)
        print("\nPASS=0 SKIP=0 FAIL=1")
        return 1

    for name, function in checks_for_profile(acceptance, args.profile):
        acceptance.run(name, function)
    return acceptance.print_summary()


if __name__ == "__main__":
    raise SystemExit(main())
