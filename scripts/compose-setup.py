#!/usr/bin/env python3
"""Prepare Compose secrets; optionally export an already parked local campaign."""
import argparse
import fcntl
import hashlib
import importlib.util
import os
from pathlib import Path
import re
import secrets
import shutil
import subprocess
import sys

ROOT = Path(__file__).resolve().parents[1]
LOCAL = ROOT / "docker/local"
spec = importlib.util.spec_from_file_location("campaign", ROOT / "scripts/sandbox-campaign.py")
campaign = importlib.util.module_from_spec(spec)
spec.loader.exec_module(campaign)


def initialize():
    os.umask(0o077)
    for directory in ["secrets", "ca", "import/db", "import/state", "ai"]:
        (LOCAL / directory).mkdir(parents=True, exist_ok=True)
    token_path = LOCAL / "secrets/tinkoff_token"
    if not token_path.exists():
        token_path.write_text(campaign.token() + "\n")
    password = LOCAL / "secrets/postgres_password"
    if not password.exists():
        password.write_text(secrets.token_hex(32) + "\n")
    production = sync_production_token()
    # AI model profiles: a working copy of the example; tokens are added by the operator.
    models = LOCAL / "ai/models.toml"
    if not models.exists():
        models.parent.mkdir(parents=True, exist_ok=True)
        shutil.copyfile(ROOT / "examples/ai-models.example.toml", models)
        print("AI model profiles created: docker/local/ai/models.toml — set provider, url, model and token there")
    # Document sources for the AI model: feeds, retention and optional embeddings.
    sources = LOCAL / "ai/sources.toml"
    if not sources.exists():
        shutil.copyfile(ROOT / "examples/sources.example.toml", sources)
    for path in [token_path, password, production, models, sources]:
        path.chmod(0o600)
    if sys.platform == "darwin":
        # Search the user's configured keychains, including login and System.
        # Export only the public T-Invest CA certificates, never identities.
        result = subprocess.run(["/usr/bin/security", "find-certificate", "-a", "-c", "Russian Trusted", "-p"], capture_output=True, text=True, check=True)
        certificates = re.findall(r"-----BEGIN CERTIFICATE-----.*?-----END CERTIFICATE-----", result.stdout, re.S)
        for certificate in certificates:
            name = hashlib.sha256(certificate.encode()).hexdigest()[:16]
            (LOCAL / "ca" / (name + ".crt")).write_text(certificate + "\n")
        if not certificates:
            print("No Russian Trusted CA found in accessible keychains; add trusted PEM files to docker/local/ca before starting Compose", file=sys.stderr)
    print("Compose secrets and public CA certificates prepared in docker/local (ignored by Git)")


def export():
    state = campaign.load()
    if state.get("phase") != "parked" or campaign.process_alive(state.get("supervisor_pid"), "sandbox-campaign.py supervise"):
        raise RuntimeError("First park the campaign: python3 scripts/sandbox-campaign.py park")
    with (campaign.WORK / "supervisor.lock").open("a") as lock:
        fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
        owners = campaign.run_sql(state, "SELECT count(*) FROM pg_locks WHERE locktype='advisory' AND granted AND database=(SELECT oid FROM pg_database WHERE datname=current_database())")
        if owners != "0":
            raise RuntimeError("Active account leases remain; export refused")
        destination = LOCAL / "import/state"
        if (destination / "campaign.json").exists():
            raise RuntimeError("Export already exists; keep it or move docker/local/import aside before a new export")
        dump = LOCAL / "import/db/10-restore.sql"
        with dump.with_suffix(".tmp").open("wb") as output:
            subprocess.run(["docker", "exec", state["container"], "pg_dump", "-U", "tinkoff_campaign", "-d", "tinkoff_campaign", "--no-owner", "--no-privileges"], stdout=output, check=True)
        dump.with_suffix(".tmp").replace(dump)
        for directory in ["configs", "logs", "reports"]:
            if (campaign.WORK / directory).exists():
                shutil.copytree(campaign.WORK / directory, destination / directory, dirs_exist_ok=True)
        history = campaign.WORK / "dashboard-equity.jsonl"
        if history.exists():
            shutil.copyfile(history, destination / history.name)
        for key in ["database_url", "database_password", "database_port", "container", "supervisor_pid"]:
            state.pop(key, None)
        for case in state["cases"].values():
            case.pop("pid", None)
        state["database_mode"] = "external"
        campaign.save(destination / "campaign.json", state)
        if (campaign.WORK / "STOP").exists():
            (destination / "STOP").touch()
        (campaign.WORK / "MOVED_TO_COMPOSE").write_text("Exported to docker/local/import; do not run this local copy alongside Compose.\n")
    print("Export complete. Accounts and request IDs preserved; local trading copy is disabled.")


def sync_production_token():
    """Copy TINKOFF_TOKEN_PRODUCTION (environment or literal fish assignment) into the
    Compose secret; without the variable the secret is empty: real accounts are off."""
    path = LOCAL / "secrets/tinkoff_token_production"
    value = os.environ.get("TINKOFF_TOKEN_PRODUCTION", "").strip() or campaign.fish_literal("TINKOFF_TOKEN_PRODUCTION") or ""
    if any(c.isspace() for c in value):
        raise SystemExit("TINKOFF_TOKEN_PRODUCTION must be one line without spaces; nothing was changed")
    temporary = path.with_suffix(".tmp")
    temporary.write_text(value + "\n" if value else "")
    temporary.chmod(0o600)
    temporary.replace(path)
    print("Real-account token: " + ("connected from TINKOFF_TOKEN_PRODUCTION" if value else "not set (TINKOFF_TOKEN_PRODUCTION is empty)"))
    return path


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("action", choices=["init", "export"])
    args = parser.parse_args()
    initialize()
    if args.action == "export":
        export()
