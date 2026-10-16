"""Container-only secret and CA setup; never put credentials in argv or state."""
import os
from pathlib import Path
import shutil
import subprocess
import sys
from urllib.parse import quote

os.umask(0o077)
extra = Path("/run/extra-ca")
certificates = sorted(extra.glob("*.crt")) if extra.exists() else []
if certificates:
    directory = Path("/usr/local/share/ca-certificates/sandbox")
    directory.mkdir(parents=True, exist_ok=True)
    for certificate in certificates:
        shutil.copyfile(certificate, directory / certificate.name)
    subprocess.run(["update-ca-certificates"], check=True, stdout=subprocess.DEVNULL)

if os.environ.get("POSTGRES_PASSWORD_FILE"):
    password = Path(os.environ["POSTGRES_PASSWORD_FILE"]).read_text().strip()
    if not password:
        raise SystemExit("PostgreSQL secret is empty")
    host = os.environ.get("POSTGRES_HOST", "postgres")
    os.environ["DATABASE_URL"] = f"postgres://tinkoff_campaign:{quote(password, safe='')}@{host}:5432/tinkoff_campaign"

os.execvp(sys.argv[1], sys.argv[1:])
