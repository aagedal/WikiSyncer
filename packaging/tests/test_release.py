#!/usr/bin/env python3

from __future__ import annotations

import hashlib
import os
from pathlib import Path
import shutil
import subprocess
import sys
import tarfile
import tempfile
import unittest


REPO_ROOT = Path(__file__).resolve().parents[2]
RELEASE = REPO_ROOT / "packaging" / "scripts" / "release.py"


class ReleasePackagingTests(unittest.TestCase):
    def setUp(self) -> None:
        self.temporary = tempfile.TemporaryDirectory(prefix="wikisync-release-test-")
        self.root = Path(self.temporary.name)
        self.binaries = self.root / "bin"
        self.binaries.mkdir()
        for index, name in enumerate(("wikisync", "wikisyncd", "wikisync-gui"), 1):
            path = self.binaries / name
            path.write_bytes(f"test executable {index}\n".encode())
            path.chmod(0o755)

    def tearDown(self) -> None:
        self.temporary.cleanup()

    def run_release(self, *arguments: str, succeed: bool = True) -> subprocess.CompletedProcess[str]:
        result = subprocess.run(
            [sys.executable, str(RELEASE), *map(str, arguments)],
            cwd=REPO_ROOT,
            text=True,
            capture_output=True,
            check=False,
        )
        if succeed and result.returncode != 0:
            self.fail(f"release.py failed:\nstdout: {result.stdout}\nstderr: {result.stderr}")
        if not succeed and result.returncode == 0:
            self.fail(f"release.py unexpectedly succeeded:\nstdout: {result.stdout}")
        return result

    def package(self, destination: Path) -> Path:
        self.run_release(
            "package", "--input-dir", self.binaries, "--output-dir", destination,
            "--version", "0.1.0-beta.1", "--target-os", "linux", "--target-arch", "x86_64",
            "--source-date-epoch", "1700000000",
        )
        return destination / "wikisync-0.1.0-beta.1-linux-x86_64.tar.gz"

    def test_archive_is_reproducible_and_has_expected_layout(self) -> None:
        first = self.package(self.root / "first")
        for path in self.binaries.iterdir():
            os.utime(path, (1800000000, 1800000000))
        second = self.package(self.root / "second")
        self.assertEqual(hashlib.sha256(first.read_bytes()).digest(), hashlib.sha256(second.read_bytes()).digest())

        self.run_release("verify-archive", "--archive", first)
        with tarfile.open(first, "r:gz") as archive:
            names = {member.name: member for member in archive}
        prefix = "wikisync-0.1.0-beta.1-linux-x86_64"
        for binary in ("wikisync", "wikisyncd", "wikisync-gui"):
            self.assertEqual(names[f"{prefix}/bin/{binary}"].mode, 0o755)
        self.assertIn(f"{prefix}/service/wikisyncd.service.in", names)
        self.assertEqual({member.mtime for member in names.values()}, {1700000000})

    def test_checksums_detect_tampering(self) -> None:
        archive = self.package(self.root / "release")
        checksum = archive.parent / "SHA256SUMS"
        self.run_release("checksums", "--output-dir", archive.parent)
        self.run_release("verify-checksums", "--checksum-file", checksum)
        archive.write_bytes(archive.read_bytes() + b"tampered")
        result = self.run_release("verify-checksums", "--checksum-file", checksum, succeed=False)
        self.assertIn("checksum mismatch", result.stderr)

    def test_symlink_binary_is_rejected(self) -> None:
        target = self.binaries / "wikisync"
        target.unlink()
        target.symlink_to(self.binaries / "wikisyncd")
        result = self.run_release(
            "package", "--input-dir", self.binaries, "--output-dir", self.root / "release",
            "--version", "0.1.0", "--target-os", "macos", "--target-arch", "aarch64",
            "--source-date-epoch", "1700000000", succeed=False,
        )
        self.assertIn("regular, non-symlink", result.stderr)

    def test_noncanonical_archive_path_is_rejected(self) -> None:
        archive_path = self.root / "unsafe.tar.gz"
        with tarfile.open(archive_path, "w:gz") as archive:
            member = tarfile.TarInfo("wikisync-0.1.0-linux-x86_64//bin/wikisync")
            member.size = 0
            archive.addfile(member)
        result = self.run_release("verify-archive", "--archive", archive_path, succeed=False)
        self.assertIn("unsafe archive path", result.stderr)

    @unittest.skipUnless(shutil.which("ssh-keygen"), "ssh-keygen unavailable")
    def test_detached_signature_and_private_key_permissions(self) -> None:
        archive = self.package(self.root / "release")
        checksum = archive.parent / "SHA256SUMS"
        self.run_release("checksums", "--output-dir", archive.parent)
        key = self.root / "release-key"
        generated = subprocess.run(
            ["ssh-keygen", "-q", "-t", "ed25519", "-N", "", "-f", str(key)],
            text=True, capture_output=True, check=False,
        )
        if generated.returncode != 0:
            self.skipTest(f"cannot generate Ed25519 SSH key: {generated.stderr}")
        allowed = self.root / "allowed_signers"
        allowed.write_text(f"release@wikisync {key.with_suffix('.pub').read_text()}", encoding="ascii")

        key.chmod(0o644)
        rejected = self.run_release(
            "sign-checksums", "--checksum-file", checksum, "--private-key", key, succeed=False,
        )
        self.assertIn("0600 or stricter", rejected.stderr)
        key.chmod(0o600)
        self.run_release("sign-checksums", "--checksum-file", checksum, "--private-key", key)
        signature = checksum.with_name("SHA256SUMS.sig")
        self.assertTrue(signature.is_file())
        self.run_release(
            "verify-signature", "--checksum-file", checksum, "--signature", signature,
            "--allowed-signers", allowed, "--signer-identity", "release@wikisync",
        )


if __name__ == "__main__":
    unittest.main()
