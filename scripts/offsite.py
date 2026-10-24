"""Off-site copies of the stand's database in an S3 bucket (Cloud.ru Object Storage
or any S3-compatible storage), encrypted before they leave the computer.

The backup service writes nightly dumps to /data/backups. With a configuration
file (OFFSITE_CONFIG, KEY=VALUE lines) the supervisor encrypts each new dump with
`openssl enc` and the operator's passphrase, uploads it with an AWS Signature V4
request and keeps the last OFFSITE_KEEP_DAYS days in the bucket. Credentials stay
in that file: never in argv, logs or the dashboard. Standard library only.
"""
import datetime as dt
import hashlib
import hmac
import json
import os
from pathlib import Path
import re
import subprocess
import tempfile
import urllib.error
import urllib.parse
import urllib.request
import xml.etree.ElementTree as ET

REQUIRED = ("S3_ENDPOINT", "S3_BUCKET", "S3_ACCESS_KEY", "S3_SECRET_KEY", "BACKUP_PASSPHRASE")
DUMP_DATE = re.compile(r"-(\d{8})\.dump")


def load_config(path):
    """KEY=VALUE lines; None when the file is absent (off-site copies are off)."""
    try:
        text = Path(path).read_text()
    except OSError:
        return None
    config = {}
    for line in text.splitlines():
        line = line.strip()
        if not line or line.startswith("#") or "=" not in line:
            continue
        key, value = line.split("=", 1)
        config[key.strip()] = value.strip().strip('"').strip("'")
    missing = [key for key in REQUIRED if not config.get(key)]
    if missing:
        raise ValueError("off-site backup settings miss " + ", ".join(missing))
    config.setdefault("S3_REGION", "ru-central-1")
    config.setdefault("S3_PREFIX", "tinkoff-rs/")
    config.setdefault("OFFSITE_KEEP_DAYS", "30")
    return config


def _sign(key, message):
    return hmac.new(key, message.encode(), hashlib.sha256).digest()


def sigv4_headers(method, url, region, access_key, secret_key, payload_hash, now, headers=None):
    """Headers of an AWS Signature V4 request to S3 (path-style URL)."""
    parts = urllib.parse.urlsplit(url)
    amz_date = now.strftime("%Y%m%dT%H%M%SZ")
    day = now.strftime("%Y%m%d")
    signed = {"host": parts.netloc, "x-amz-content-sha256": payload_hash, "x-amz-date": amz_date}
    signed.update({k.lower(): v for k, v in (headers or {}).items()})
    names = sorted(signed)
    query = "&".join(f"{urllib.parse.quote(k, safe='-_.~')}={urllib.parse.quote(v, safe='-_.~')}"
                     for k, v in sorted(urllib.parse.parse_qsl(parts.query, keep_blank_values=True)))
    canonical = "\n".join([method, urllib.parse.quote(parts.path or "/", safe="/-_.~"), query,
                           "".join(f"{k}:{str(signed[k]).strip()}\n" for k in names), ";".join(names), payload_hash])
    scope = f"{day}/{region}/s3/aws4_request"
    to_sign = "\n".join(["AWS4-HMAC-SHA256", amz_date, scope, hashlib.sha256(canonical.encode()).hexdigest()])
    key = _sign(_sign(_sign(_sign(("AWS4" + secret_key).encode(), day), region), "s3"), "aws4_request")
    signature = hmac.new(key, to_sign.encode(), hashlib.sha256).hexdigest()
    result = {k: v for k, v in signed.items() if k != "host"}
    result["Authorization"] = (f"AWS4-HMAC-SHA256 Credential={access_key}/{scope}, "
                               f"SignedHeaders={';'.join(names)}, Signature={signature}")
    return result


class S3:
    def __init__(self, config):
        self.endpoint = config["S3_ENDPOINT"].rstrip("/")
        self.bucket = config["S3_BUCKET"]
        self.region = config["S3_REGION"]
        self.access_key = config["S3_ACCESS_KEY"]
        self.secret_key = config["S3_SECRET_KEY"]

    def request(self, method, key="", query="", body=b""):
        url = f"{self.endpoint}/{self.bucket}/{urllib.parse.quote(key)}" + (f"?{query}" if query else "")
        headers = sigv4_headers(method, url, self.region, self.access_key, self.secret_key,
                                hashlib.sha256(body).hexdigest(), dt.datetime.now(dt.timezone.utc))
        request = urllib.request.Request(url, data=body if method == "PUT" else None, method=method, headers=headers)
        try:
            with urllib.request.urlopen(request, timeout=120) as response:
                return response.read()
        except urllib.error.HTTPError as error:
            # The storage answers with an XML error code; the request itself holds no secret.
            raise RuntimeError(f"S3 {method} {key or self.bucket}: HTTP {error.code} {error.read()[:200]!r}") from None

    def put(self, key, body):
        self.request("PUT", key, body=body)

    def keys(self, prefix):
        out, token = [], None
        while True:
            query = "list-type=2&prefix=" + urllib.parse.quote(prefix, safe="")
            if token:
                query += "&continuation-token=" + urllib.parse.quote(token, safe="")
            root = ET.fromstring(self.request("GET", query=query))
            ns = {"s3": root.tag.split("}")[0].strip("{")} if root.tag.startswith("{") else {}
            find = (lambda e, t: e.findall(f"s3:{t}", ns)) if ns else (lambda e, t: e.findall(t))
            out += [c.findtext(f"{{{ns['s3']}}}Key" if ns else "Key") for c in find(root, "Contents")]
            truncated = (root.findtext(f"{{{ns['s3']}}}IsTruncated" if ns else "IsTruncated") or "").lower() == "true"
            token = root.findtext(f"{{{ns['s3']}}}NextContinuationToken" if ns else "NextContinuationToken")
            if not truncated or not token:
                return out

    def delete(self, key):
        self.request("DELETE", key)


def encrypt(source, passphrase):
    """AES-256 with a PBKDF2 key: `openssl enc -d -aes-256-cbc -pbkdf2 -iter 200000` restores it."""
    with tempfile.NamedTemporaryFile("w", delete=False) as secret:
        os.chmod(secret.name, 0o600)
        secret.write(passphrase)
    try:
        return subprocess.run(["openssl", "enc", "-aes-256-cbc", "-pbkdf2", "-iter", "200000", "-salt",
                               "-in", str(source), "-pass", f"file:{secret.name}"],
                              capture_output=True, check=True, timeout=600).stdout
    finally:
        os.unlink(secret.name)


def sync(backups, config, now, storage=None):
    """Uploads dumps not yet in the bucket and removes copies older than the keep
    window. Returns the status written to <backups>/offsite.json."""
    backups = Path(backups)
    status_path = backups / "offsite.json"
    try:
        status = json.loads(status_path.read_text())
    except (OSError, ValueError):
        status = {}
    uploaded = status.get("uploaded", {})
    storage = storage or S3(config)
    prefix = config["S3_PREFIX"]
    try:
        for dump in sorted(backups.glob("*.dump")):
            if dump.name in uploaded:
                continue
            storage.put(prefix + dump.name + ".enc", encrypt(dump, config["BACKUP_PASSPHRASE"]))
            uploaded[dump.name] = now.isoformat()
        keep = dt.timedelta(days=int(config["OFFSITE_KEEP_DAYS"]))
        removed = 0
        for key in storage.keys(prefix):
            match = DUMP_DATE.search(key or "")
            if match and dt.datetime.strptime(match.group(1), "%Y%m%d").replace(tzinfo=dt.timezone.utc) < now - keep:
                storage.delete(key)
                removed += 1
        status = {"ok": True, "at": now.isoformat(), "uploaded": uploaded, "removed": removed,
                  "bucket": config["S3_BUCKET"], "prefix": prefix,
                  "latest": max(uploaded, default=None)}
    except (RuntimeError, OSError, subprocess.SubprocessError, ValueError, ET.ParseError) as error:
        status = dict(status, ok=False, at=now.isoformat(), uploaded=uploaded, error=str(error)[:300])
    status_path.write_text(json.dumps(status, ensure_ascii=False))
    return status
