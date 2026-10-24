import datetime as dt
import importlib.util
import json
from pathlib import Path
import subprocess
import tempfile
import unittest

SPEC = importlib.util.spec_from_file_location("offsite", Path(__file__).resolve().parents[1] / "scripts/offsite.py")
offsite = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(offsite)


class FakeStorage:
    def __init__(self, keys=()):
        self.objects = {key: b"" for key in keys}

    def put(self, key, body):
        self.objects[key] = body

    def keys(self, prefix):
        return [k for k in self.objects if k.startswith(prefix)]

    def delete(self, key):
        del self.objects[key]


class OffsiteTests(unittest.TestCase):
    def test_signature_matches_the_aws_reference_example(self):
        # AWS S3 documentation, "Example: GET Object" (Signature Version 4).
        headers = offsite.sigv4_headers(
            "GET", "https://examplebucket.s3.amazonaws.com/test.txt", "us-east-1",
            "AKIAIOSFODNN7EXAMPLE", "wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY",
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855",
            dt.datetime(2013, 5, 24, tzinfo=dt.timezone.utc), {"Range": "bytes=0-9"})
        self.assertTrue(headers["Authorization"].endswith(
            "Signature=f0e8bdb87c964420e857bd35b5d6ed310bd44f0170aba48dd91039c6036bdb41"), headers["Authorization"])
        self.assertIn("SignedHeaders=host;range;x-amz-content-sha256;x-amz-date", headers["Authorization"])

    def test_new_dumps_are_encrypted_uploaded_once_and_old_copies_expire(self):
        with tempfile.TemporaryDirectory() as directory:
            backups = Path(directory)
            (backups / "tinkoff_campaign-20261021.dump").write_bytes(b"dump of the 21st")
            (backups / "tinkoff_campaign-20261022.dump").write_bytes(b"dump of the 22nd")
            config = {"S3_BUCKET": "stand", "S3_PREFIX": "tinkoff-rs/", "OFFSITE_KEEP_DAYS": "30", "BACKUP_PASSPHRASE": "correct horse"}
            storage = FakeStorage(["tinkoff-rs/tinkoff_campaign-20260901.dump.enc", "tinkoff-rs/notes.txt"])
            now = dt.datetime(2026, 10, 22, 12, tzinfo=dt.timezone.utc)
            status = offsite.sync(backups, config, now, storage)
            self.assertTrue(status["ok"], status)
            self.assertEqual(sorted(storage.objects), ["tinkoff-rs/notes.txt", "tinkoff-rs/tinkoff_campaign-20261021.dump.enc",
                                                       "tinkoff-rs/tinkoff_campaign-20261022.dump.enc"])
            body = storage.objects["tinkoff-rs/tinkoff_campaign-20261022.dump.enc"]
            self.assertNotIn(b"dump of the 22nd", body, "the copy leaves the computer encrypted")
            restored = subprocess.run(["openssl", "enc", "-d", "-aes-256-cbc", "-pbkdf2", "-iter", "200000", "-pass", "pass:correct horse"],
                                      input=body, capture_output=True, check=True).stdout
            self.assertEqual(restored, b"dump of the 22nd")
            # A second pass uploads nothing new.
            storage.objects.clear()
            offsite.sync(backups, config, now, storage)
            self.assertEqual(storage.objects, {})
            self.assertEqual(json.loads((backups / "offsite.json").read_text())["latest"], "tinkoff_campaign-20261022.dump")

    def test_settings_require_every_secret_and_absent_settings_turn_copies_off(self):
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "s3.env"
            self.assertIsNone(offsite.load_config(path))
            path.write_text("S3_ENDPOINT=https://s3.cloud.ru\nS3_BUCKET=stand\n")
            with self.assertRaisesRegex(ValueError, "S3_ACCESS_KEY"):
                offsite.load_config(path)
            path.write_text("# Cloud.ru\nS3_ENDPOINT=https://s3.cloud.ru\nS3_BUCKET=stand\nS3_ACCESS_KEY=tenant:key\n"
                            "S3_SECRET_KEY=secret\nBACKUP_PASSPHRASE='long phrase'\n")
            config = offsite.load_config(path)
            self.assertEqual((config["S3_REGION"], config["BACKUP_PASSPHRASE"], config["OFFSITE_KEEP_DAYS"]), ("ru-central-1", "long phrase", "30"))


if __name__ == "__main__":
    unittest.main()
