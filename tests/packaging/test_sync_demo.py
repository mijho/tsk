from __future__ import annotations

import contextlib
import importlib.machinery
import importlib.util
import io
import json
import os
import pathlib
import sys
import tempfile
import types
import unittest
from unittest import mock


ROOT = pathlib.Path(__file__).resolve().parents[2]


def load_script(name: str, path: pathlib.Path):
    loader = importlib.machinery.SourceFileLoader(name, str(path))
    spec = importlib.util.spec_from_loader(name, loader)
    assert spec is not None
    module = importlib.util.module_from_spec(spec)
    sys.modules[name] = module
    loader.exec_module(module)
    return module


SYNC_DEMO = load_script("sync_demo", ROOT / "scripts/sync-demo")
SYNC_CHECK = load_script("sync_demo_check", ROOT / "scripts/sync-demo-check.py")


class SyncDemoTests(unittest.TestCase):
    def settings(self, root: pathlib.Path):
        environment = {
            "TSK_SYNC_SERVER_CONFIG": str(root / "server/config.json"),
            "TSK_SYNC_DEMO_ROOT": str(root / "demo"),
            "TSK_SYNC_SYSTEMD_DIR": str(root / "systemd"),
        }
        return SYNC_DEMO.Settings.from_environment(ROOT, environment)

    def test_existing_valid_server_and_clients_are_adopted_byte_for_byte(self):
        with tempfile.TemporaryDirectory(prefix="tsk-sync-demo-test-") as directory:
            settings = self.settings(pathlib.Path(directory))
            SYNC_DEMO.ensure_private_directory(settings.demo_root)
            SYNC_DEMO.ensure_private_directory(settings.project_a_dir)
            SYNC_DEMO.ensure_private_directory(settings.project_b_dir)

            server = SYNC_DEMO.ensure_server_config(settings)
            client_a = SYNC_DEMO.ensure_client_config(
                settings,
                server,
                settings.actor_a,
                settings.client_a_dir,
                settings.project_a_dir,
                "client A config",
            )
            SYNC_DEMO.ensure_client_config(
                settings,
                server,
                settings.actor_b,
                settings.client_b_dir,
                settings.project_b_dir,
                "client B config",
            )

            server_bytes = json.dumps(server, separators=(",", ":")).encode() + b"\n"
            client_bytes = json.dumps(client_a, separators=(",", ":")).encode() + b"\n"
            settings.server_config.write_bytes(server_bytes)
            os.chmod(settings.server_config, 0o600)
            client_a_path = settings.client_a_dir / "sync.json"
            client_a_path.write_bytes(client_bytes)
            os.chmod(client_a_path, 0o600)

            adopted = SYNC_DEMO.ensure_server_config(settings)
            SYNC_DEMO.ensure_client_config(
                settings,
                adopted,
                settings.actor_a,
                settings.client_a_dir,
                settings.project_a_dir,
                "client A config",
            )
            self.assertEqual(settings.server_config.read_bytes(), server_bytes)
            self.assertEqual(client_a_path.read_bytes(), client_bytes)

    def test_config_creation_and_reporting_never_print_secret_values(self):
        with tempfile.TemporaryDirectory(prefix="tsk-sync-demo-secret-test-") as directory:
            settings = self.settings(pathlib.Path(directory))
            SYNC_DEMO.ensure_private_directory(settings.demo_root)
            SYNC_DEMO.ensure_private_directory(settings.project_a_dir)
            output = io.StringIO()
            with contextlib.redirect_stdout(output), contextlib.redirect_stderr(output):
                server = SYNC_DEMO.ensure_server_config(settings)
                SYNC_DEMO.ensure_client_config(
                    settings,
                    server,
                    settings.actor_a,
                    settings.client_a_dir,
                    settings.project_a_dir,
                    "client A config",
                )
            rendered = output.getvalue()
            redactor = SYNC_DEMO.SecretRedactor()
            redactor.collect(server)
            for secret in redactor.secrets:
                self.assertNotIn(secret, rendered)
                self.assertNotIn(secret, redactor.sanitize(f"failure: {secret}"))

    @unittest.skipUnless(hasattr(os, "symlink"), "symlinks are required")
    def test_private_json_rejects_symlinks(self):
        with tempfile.TemporaryDirectory(prefix="tsk-sync-demo-link-test-") as directory:
            root = pathlib.Path(directory)
            target = root / "target.json"
            target.write_text("{}\n", encoding="utf-8")
            os.chmod(target, 0o600)
            link = root / "sync.json"
            link.symlink_to(target)
            with self.assertRaisesRegex(SYNC_DEMO.DemoError, "symlink"):
                SYNC_DEMO.read_private_json(link, "client config")

    def test_acceptance_profiles_keep_full_at_16_and_smoke_non_disruptive(self):
        names = [
            "check_preflight",
            "check_pins",
            "check_a_create_b_receive",
            "check_b_update_a_receive",
            "check_concurrent_numbers",
            "check_stale_conflict",
            "check_lost_response_and_retry",
            "check_error_before_projection",
            "check_crash_before_projection",
            "check_cursor_catch_up",
            "check_s2_api_restart_persistence",
            "check_projection_replay",
            "check_tombstone_stale_update",
            "check_unauthorized",
            "check_project_mapping",
            "check_local_only",
        ]
        acceptance = types.SimpleNamespace(**{name: (lambda: None) for name in names})
        smoke = SYNC_CHECK.checks_for_profile(acceptance, "smoke")
        full = SYNC_CHECK.checks_for_profile(acceptance, "full")
        self.assertEqual(len(smoke), 11)
        self.assertEqual(len(full), 16)
        smoke_names = "\n".join(name for name, _ in smoke).lower()
        self.assertNotIn("restart", smoke_names)
        self.assertNotIn("projection update", smoke_names)
        self.assertIn("api restart", "\n".join(name for name, _ in full).lower())
    def test_rendered_api_unit_is_loopback_safe_and_failure_injection_off(self):
        with tempfile.TemporaryDirectory(prefix="tsk-sync-demo-unit-test-") as directory:
            settings = self.settings(pathlib.Path(directory))
            unit = SYNC_DEMO.render_api_unit(settings)
            self.assertIn(f"WorkingDirectory={ROOT}", unit)
            self.assertNotIn(f'WorkingDirectory="{ROOT}"', unit)
            self.assertIn("Environment=TSK_SYNC_ENABLE_FAILURE_INJECTION=0", unit)
            self.assertNotIn("TSK_SYNC_ENABLE_FAILURE_INJECTION=1", unit)

    def test_start_services_restarts_rebuilt_api(self):
        settings = types.SimpleNamespace(
            s2_service="s2.service",
            api_service="api.service",
        )
        with (
            mock.patch.object(SYNC_DEMO, "_systemctl_root") as systemctl,
            mock.patch.object(SYNC_DEMO, "_wait_for_tcp"),
            mock.patch.object(SYNC_DEMO, "restart_api_and_wait") as restart_api,
            mock.patch.object(SYNC_DEMO, "wait_for_health") as wait_health,
        ):
            SYNC_DEMO.start_services(settings, units_changed=False, binaries_rebuilt=True)
        restart_api.assert_called_once_with(settings)
        wait_health.assert_not_called()
        self.assertIn(
            mock.call(["enable", "--now", "api.service"], timeout=180),
            systemctl.mock_calls,
        )

    def test_full_profile_cleans_dropin_when_checker_raises(self):
        settings = object()
        dropin = pathlib.Path("/run/systemd/system/test.service.d/failure.conf")
        with (
            mock.patch.object(SYNC_DEMO, "_install_failure_dropin", return_value=dropin),
            mock.patch.object(SYNC_DEMO, "_systemctl_root"),
            mock.patch.object(SYNC_DEMO, "restart_api_and_wait"),
            mock.patch.object(
                SYNC_DEMO, "run_checker", side_effect=RuntimeError("checker failed")
            ),
            mock.patch.object(SYNC_DEMO, "_remove_failure_dropin") as remove,
        ):
            with self.assertRaisesRegex(RuntimeError, "checker failed"):
                SYNC_DEMO.run_full_test(settings, allow_skips=False)
        remove.assert_called_once_with(settings, dropin)


if __name__ == "__main__":
    unittest.main()
