"""One-shot T3 provider configuration change, deferred until Codex runs are idle."""
import json
import os
import plistlib
import sqlite3
import signal
import subprocess
import sys
import time
from datetime import datetime, timezone
from contextlib import closing
from pathlib import Path


def atomic_write(path, data):
    temporary = path.with_name(path.name + ".piaz-tmp")
    with open(temporary, "wb") as output:
        os.chmod(temporary, 0o600)
        output.write(data)
        output.flush()
        os.fsync(output.fileno())
    os.replace(temporary, path)


def active_work(base):
    database = base / "userdata/statev2.sqlite"
    with closing(sqlite3.connect(database.as_uri() + "?mode=ro", uri=True, timeout=5)) as connection:
        runs = connection.execute(
            "SELECT count(*) FROM orchestration_v2_projection_runs "
            "WHERE provider='codex' AND status NOT IN ('completed','failed','cancelled','interrupted','rolled_back')"
        ).fetchone()[0]
        children = connection.execute(
            "SELECT count(*) FROM orchestration_v2_projection_subagents WHERE provider='codex' "
            "AND status NOT IN ('idle','completed','failed','cancelled','interrupted')"
        ).fetchone()[0]
        background = sum(bool(json.loads(row[0]).get("pendingBackgroundTasks")) for row in connection.execute(
            "SELECT payload_json FROM orchestration_v2_projection_provider_threads WHERE provider='codex'"
        ))
        return runs + children + background


def apply_settings(base, expected, desired, backup):
    path = base / "userdata/settings.json"
    if path.is_symlink():
        raise RuntimeError("Settings is a symlink; automatic activation stopped")
    before = path.read_bytes()
    settings = json.loads(before)
    providers = settings.get("providerInstances", {}).copy()
    if providers.get("codex") != expected:
        raise RuntimeError("Codex settings changed since installation; nothing overwritten")
    providers["codex"] = desired
    settings["providerInstances"] = providers
    if active_work(base) or path.read_bytes() != before:
        return False
    if not backup.exists():
        atomic_write(backup, before)
    atomic_write(path, (json.dumps(settings, indent=2, ensure_ascii=False) + "\n").encode())
    return True


def process_signature(pid):
    return subprocess.run(["/bin/ps", "-p", str(pid), "-o", "ppid=,lstart=,comm="],
                          capture_output=True, text=True, check=False).stdout.strip()


def retire_processes(base, processes):
    for process in processes:
        signature = process_signature(process["pid"])
        if not signature or signature != process["signature"]:
            continue  # Exited or PID reused; never signal the replacement.
        if active_work(base):
            return False
        os.kill(process["pid"], signal.SIGTERM)
        for _ in range(50):
            if process_signature(process["pid"]) != signature:
                break
            time.sleep(0.1)
        else:
            raise RuntimeError("Old idle Codex process did not exit; no force kill attempted")
    return True


def runtime_proof(base, root):
    try:
        server_pid = json.loads((base / "userdata/server-runtime.json").read_text())["pid"]
        for path in (root / "runtime").glob("*.json"):
            record = json.loads(path.read_text())
            if record.get("bridgeRevision") != 2 or not record.get("sawTurn") or record.get("parentPid") != server_pid:
                continue
            command = subprocess.run(["/bin/ps", "-p", str(record["pid"]), "-o", "command="],
                                     capture_output=True, text=True, check=False).stdout
            native_parent = subprocess.run(["/bin/ps", "-p", str(record["nativePid"]), "-o", "ppid="],
                                           capture_output=True, text=True, check=False).stdout.strip()
            if str(root / "codex-proxy.mjs") in command and native_parent == str(record["pid"]):
                return record
    except (OSError, ValueError, KeyError):
        pass
    return None


def main(request_path):
    request = json.loads(request_path.read_text())
    root = request_path.parent
    base = Path(request["baseDir"])
    status_path = root / "activation-status.json"

    def status(state, **details):
        atomic_write(status_path, (json.dumps({
            "state": state, "updatedAt": datetime.now(timezone.utc).isoformat(), **details,
        }, indent=2) + "\n").encode())

    try:
        idle_since = None
        deadline = time.monotonic() + 43200
        while time.monotonic() < deadline:
            count = active_work(base)
            if count:
                idle_since = None
                status("waitingForIdle", activeCodexWork=count)
            elif idle_since is None:
                idle_since = time.monotonic()
                status("waitingForStableIdle", activeCodexWork=0)
            elif time.monotonic() - idle_since >= 10:
                with open(request["t3InfoPlist"], "rb") as source:
                    version = plistlib.load(source)["CFBundleShortVersionString"]
                if version != "0.1.7":
                    raise RuntimeError("T3 version changed; revalidate the continuation adapter")
                if apply_settings(base, request["previousCodex"], request["desiredCodex"], root / "settings-before.json"):
                    if not retire_processes(base, request.get("retireProcesses", [])):
                        idle_since = None
                        continue
                    status("awaitingRuntime", reason="Old Codex session retired; waiting for a real T3 turn through the bridge")
                    # Provider probes also say ready; only a live T3 child that handled a turn proves migration.
                    while time.monotonic() < deadline:
                        proof = runtime_proof(base, root)
                        if proof:
                            status("active", provider="codex", **proof)
                            return 0
                        time.sleep(2)
                    status("configuredAwaitingVerification", reason="No real bridged T3 turn observed within 12 hours")
                    return 0
                idle_since = None
            time.sleep(2)
        status("expired", reason="No stable idle window within 12 hours; settings unchanged")
        return 1
    except Exception as error:
        # Do not put settings or provider responses into logs.
        reason = str(error) if isinstance(error, RuntimeError) else type(error).__name__
        status("failed", reason=reason)
        return 1


if __name__ == "__main__":
    sys.exit(main(Path(sys.argv[1]) if len(sys.argv) > 1 else Path(__file__).with_name("activation-request.json")))
