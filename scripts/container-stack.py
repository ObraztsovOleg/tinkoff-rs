#!/usr/bin/env python3
"""Foreground Compose process manager. SIGTERM parks; explicit stop liquidates."""
import importlib.util
import json
import os
from pathlib import Path
import shutil
import signal
import subprocess
import sys
import time

ROOT = Path(__file__).resolve().parents[1]
spec = importlib.util.spec_from_file_location("campaign", ROOT / "scripts/sandbox-campaign.py")
campaign = importlib.util.module_from_spec(spec)
spec.loader.exec_module(campaign)


def main():
    work = campaign.WORK
    if "--health" in sys.argv:
        state = campaign.load()
        supervisor = campaign.process_alive(state.get("supervisor_pid"), "sandbox-campaign.py supervise")
        dashboard = json.loads((work / "dashboard.json").read_text())
        good = campaign.process_alive(dashboard["pid"], "dashboard/server.py serve")
        raise SystemExit(0 if good and (supervisor or state.get("phase") == "complete") else 1)
    os.umask(0o077)
    work.mkdir(parents=True, exist_ok=True)
    seed = Path("/seed")
    if not (work / "campaign.json").exists() and (seed / "campaign.json").exists():
        shutil.copytree(seed, work, dirs_exist_ok=True)
    # Do not resurrect stale PIDs from a previous PID namespace. Container exit
    # stops every child; durable request IDs and DB leases handle recovery.
    if (work / "campaign.json").exists():
        state = campaign.load()
        state.pop("supervisor_pid", None)
        for case in state["cases"].values():
            case.pop("pid", None)
        campaign.persist(state)
    (work / "PARK").unlink(missing_ok=True)
    campaign.prepare(os.environ.get("SANDBOX_ALLOW_ACCOUNT_REUSE") == "1",
                     os.environ.get("SANDBOX_ADOPT_IDLE_ACCOUNTS") == "1")
    state = campaign.load()
    result = campaign.bot(state, next(iter(state["cases"].values())), "migrate")
    if result.returncode:
        raise RuntimeError("Database migration failed; check the PostgreSQL service")
    children = []
    stopping = False

    def park(*_):
        nonlocal stopping
        stopping = True
        (work / "PARK").touch()

    signal.signal(signal.SIGTERM, park)
    signal.signal(signal.SIGINT, park)
    try:
        for script, action in [("sandbox-campaign.py", "supervise"), ("dashboard/server.py", "serve")]:
            children.append(subprocess.Popen([sys.executable, str(ROOT / "scripts" / script), action]))
        while not stopping:
            if children[1].poll() is not None:
                raise RuntimeError("Dashboard exited unexpectedly")
            if children[0].poll() is not None:
                if children[0].returncode != 0 or campaign.load().get("phase") != "complete":
                    raise RuntimeError("Campaign supervisor exited unexpectedly")
                # Keep the final report available after all scenarios finish.
            time.sleep(1)
    finally:
        (work / "PARK").touch()
        if children:
            children[0].wait(timeout=240)
        for child in children[1:]:
            child.terminate()
            child.wait(timeout=15)


if __name__ == "__main__":
    main()
