"""Container configuration and migration guardrails; no broker mutations."""
import importlib.util
import json
import os
from pathlib import Path
import signal
import tempfile
import unittest
from unittest.mock import Mock, patch

spec = importlib.util.spec_from_file_location("campaign", Path(__file__).resolve().parents[1] / "scripts/sandbox-campaign.py")
campaign = importlib.util.module_from_spec(spec)
spec.loader.exec_module(campaign)


class ComposeTests(unittest.TestCase):
    def test_token_secret_file_takes_precedence_over_fish(self):
        with tempfile.TemporaryDirectory() as temporary:
            path = Path(temporary) / "secret"
            path.write_text("test-token\n")
            with patch.dict(os.environ, {"TINKOFF_TOKEN_FILE": str(path)}):
                self.assertEqual(campaign.token(), "test-token")
                path.write_text("line1\nline2")
                with self.assertRaises(RuntimeError):
                    campaign.token()

    def test_external_database_needs_no_docker_and_password_is_not_in_argv(self):
        url = "postgres://user:TEST_PASSWORD@postgres/db"
        with patch.dict(os.environ, {"DATABASE_URL": url}), patch.object(campaign, "command", return_value="1") as command:
            campaign.database({"database_mode": "external"})
            args, kwargs = command.call_args
            self.assertEqual(args[0][0], "psql")
            self.assertNotIn("TEST_PASSWORD", str(args))
            self.assertEqual(kwargs["env"]["PGDATABASE"], "db")
            self.assertEqual(kwargs["env"]["PGHOST"], "postgres")
            self.assertEqual(kwargs["env"]["PGUSER"], "user")
            self.assertEqual(kwargs["env"]["PGPASSWORD"], "TEST_PASSWORD")

    def test_psql_environment_decodes_credentials_and_tls_options(self):
        url = "postgresql://user:p%40ss%2Fword@postgres:5433/campaign?sslmode=verify-full&connect_timeout=10"
        with patch.dict(os.environ, {"DATABASE_URL": url}):
            env = campaign.psql_environment({})
        self.assertEqual(env["PGPASSWORD"], "p@ss/word")
        self.assertEqual(env["PGPORT"], "5433")
        self.assertEqual(env["PGSSLMODE"], "verify-full")
        self.assertEqual(env["PGCONNECT_TIMEOUT"], "10")

    def test_park_stops_runtimes_before_terminating_scheduler_without_cleanup(self):
        case = {"kind": "example", "config": "example.toml", "pid": 10}
        state = {"supervisor_pid": 123, "cases": {"example": case}}
        events = []
        with tempfile.TemporaryDirectory() as temporary, patch.object(campaign, "WORK", Path(temporary)), patch.object(campaign, "load", return_value=state), patch.object(campaign, "persist"), patch.object(campaign, "process_alive", return_value=True), patch.object(campaign, "stop_process", side_effect=lambda _: events.append("runtime stopped")), patch.object(campaign, "run_sql", return_value="0"), patch.object(campaign.os, "kill", side_effect=lambda _, sig: events.append(sig)), patch.object(campaign, "cleanup_case") as cleanup:
            campaign.park_local()
            self.assertEqual(events, [signal.SIGSTOP, "runtime stopped", signal.SIGKILL])
            self.assertEqual(state["phase"], "parked")
            self.assertFalse((Path(temporary) / "STOP").exists())
            cleanup.assert_not_called()

    def test_remaining_lease_aborts_park_and_resumes_scheduler(self):
        state = {"supervisor_pid": 123, "cases": {}}
        with patch.object(campaign, "load", return_value=state), patch.object(campaign, "process_alive", return_value=True), patch.object(campaign, "run_sql", return_value="1"), patch.object(campaign.os, "kill") as kill:
            with self.assertRaises(RuntimeError):
                campaign.park_local()
            self.assertEqual([c.args[1] for c in kill.call_args_list], [signal.SIGSTOP, signal.SIGCONT])

    def test_exported_local_copy_cannot_start_even_if_old_state_remains(self):
        with tempfile.TemporaryDirectory() as temporary, patch.object(campaign, "WORK", Path(temporary)), patch.object(campaign, "load") as load:
            (Path(temporary) / "MOVED_TO_COMPOSE").touch()
            with self.assertRaises(RuntimeError):
                campaign.start()
            with self.assertRaises(RuntimeError):
                campaign.supervise()
            load.assert_not_called()


if __name__ == "__main__":
    unittest.main()
