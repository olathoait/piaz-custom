import importlib.util
import json
import sqlite3
import subprocess
import sys
import tempfile
import unittest
from contextlib import closing
from pathlib import Path

spec = importlib.util.spec_from_file_location("activation", Path(__file__).with_name("activate-when-idle.py"))
activation = importlib.util.module_from_spec(spec)
spec.loader.exec_module(activation)


class ActivationTest(unittest.TestCase):
    def test_waits_for_all_codex_runs_and_preserves_other_settings(self):
        with tempfile.TemporaryDirectory() as directory:
            base = Path(directory)
            (base / "userdata").mkdir()
            (base / "caches").mkdir()
            (base / "caches/codex.json").write_text(json.dumps({"status": "ready", "installed": True}))
            self.assertIsNone(activation.runtime_proof(base, base))
            with closing(sqlite3.connect(base / "userdata/statev2.sqlite")) as database, database:
                database.execute("CREATE TABLE orchestration_v2_projection_runs(provider TEXT,status TEXT)")
                database.execute("CREATE TABLE orchestration_v2_projection_subagents(provider TEXT,status TEXT)")
                database.execute("CREATE TABLE orchestration_v2_projection_provider_threads(provider TEXT,payload_json TEXT)")
                database.executemany("INSERT INTO orchestration_v2_projection_runs VALUES(?,?)", [("codex", "running"), ("codex", "queued"), ("claudeAgent", "running")])
            original = {"favorites": ["keep"], "providerInstances": {"other": {"driver": "other"}}}
            settings = base / "userdata/settings.json"
            settings.write_text(json.dumps(original))
            backup = base / "before.json"
            target = {"driver": "codex", "config": {"binaryPath": "/bridge"}}
            self.assertEqual(activation.active_work(base), 2)
            self.assertFalse(activation.apply_settings(base, None, target, backup))
            self.assertFalse(backup.exists())
            self.assertEqual(json.loads(settings.read_text()), original)
            child = subprocess.Popen([sys.executable, "-c", "import time; print('ready', flush=True); time.sleep(30)"], stdout=subprocess.PIPE)
            try:
                child.stdout.readline()
                process = {"pid": child.pid, "signature": activation.process_signature(child.pid)}
                self.assertFalse(activation.retire_processes(base, [process]))
                self.assertIsNone(child.poll())
                # A reused PID is never signalled.
                self.assertTrue(activation.retire_processes(base, [{**process, "signature": "different"}]))
                self.assertIsNone(child.poll())
            finally:
                child.terminate()
                child.wait(timeout=5)
                child.stdout.close()
            with closing(sqlite3.connect(base / "userdata/statev2.sqlite")) as database, database:
                database.execute("UPDATE orchestration_v2_projection_runs SET status='interrupted' WHERE provider='codex'")
                database.execute("INSERT INTO orchestration_v2_projection_subagents VALUES('codex','running')")
                database.execute("INSERT INTO orchestration_v2_projection_provider_threads VALUES('codex',?)", (json.dumps({"pendingBackgroundTasks": [{"taskId": "test"}]}),))
            self.assertEqual(activation.active_work(base), 2)
            self.assertFalse(activation.apply_settings(base, None, target, backup))
            with closing(sqlite3.connect(base / "userdata/statev2.sqlite")) as database, database:
                database.execute("UPDATE orchestration_v2_projection_subagents SET status='idle'")
                database.execute("UPDATE orchestration_v2_projection_provider_threads SET payload_json='{}'")
            self.assertTrue(activation.apply_settings(base, None, target, backup))
            child = subprocess.Popen([sys.executable, "-c", "import time; print('ready', flush=True); time.sleep(30)"], stdout=subprocess.PIPE)
            try:
                child.stdout.readline()
                self.assertTrue(activation.retire_processes(base, [{"pid": child.pid, "signature": activation.process_signature(child.pid)}]))
                child.wait(timeout=5)
                self.assertIsNotNone(child.returncode)
            finally:
                if child.poll() is None:
                    child.terminate()
                    child.wait(timeout=5)
                child.stdout.close()
            changed = json.loads(settings.read_text())
            self.assertEqual(changed["favorites"], original["favorites"])
            self.assertEqual(changed["providerInstances"]["other"], original["providerInstances"]["other"])
            self.assertEqual(changed["providerInstances"]["codex"], target)
            self.assertEqual(json.loads(backup.read_text()), original)
            self.assertEqual(settings.stat().st_mode & 0o777, 0o600)
            with self.assertRaises(RuntimeError):
                activation.apply_settings(base, None, target, backup)


if __name__ == "__main__":
    unittest.main()
