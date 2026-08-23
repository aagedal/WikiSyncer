#!/usr/bin/env python3

from __future__ import annotations

import hashlib
import importlib.util
import json
import os
from pathlib import Path
import shutil
import struct
import subprocess
import sys
import tarfile
import tempfile
import unittest
from unittest import mock


REPO_ROOT = Path(__file__).resolve().parents[2]
RELEASE = REPO_ROOT / "packaging" / "scripts" / "release.py"
RELEASE_SPEC = importlib.util.spec_from_file_location("wikisync_release", RELEASE)
assert RELEASE_SPEC is not None and RELEASE_SPEC.loader is not None
RELEASE_MODULE = importlib.util.module_from_spec(RELEASE_SPEC)
RELEASE_SPEC.loader.exec_module(RELEASE_MODULE)


def minimal_macho(cpu_type: int) -> bytes:
    segment = struct.pack(
        "<II16sQQQQIIII",
        0x19,  # LC_SEGMENT_64
        72,
        b"",
        0,
        0,
        0,
        0,
        0,
        0,
        0,
        0,
    )
    header = struct.pack(
        "<8I",
        0xFEEDFACF,  # MH_MAGIC_64
        cpu_type,
        0,
        2,  # MH_EXECUTE
        1,
        len(segment),
        0,
        0,
    )
    return header + segment


def fat_macho(slices: list[tuple[int, bytes]]) -> bytes:
    table_end = 8 + 20 * len(slices)
    offsets = []
    cursor = table_end
    for _, payload in slices:
        cursor = (cursor + 15) & ~15
        offsets.append(cursor)
        cursor += len(payload)
    records = b"".join(
        struct.pack(">IIIII", cpu_type, 0, offset, len(payload), 4)
        for (cpu_type, payload), offset in zip(slices, offsets, strict=True)
    )
    result = bytearray(b"\xca\xfe\xba\xbe" + struct.pack(">I", len(slices)) + records)
    for (_, payload), offset in zip(slices, offsets, strict=True):
        result.extend(b"\0" * (offset - len(result)))
        result.extend(payload)
    return bytes(result)


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

    def macos_binaries(self) -> Path:
        destination = self.root / "macos-bin"
        destination.mkdir()
        fixture = minimal_macho(0x0100000C)
        for name in ("wikisync", "wikisyncd", "wikisync-gui"):
            path = destination / name
            path.write_bytes(fixture)
            path.chmod(0o755)
        return destination

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
        self.assertIn(f"{prefix}/docs/security/linux-package-repository-trust.md", names)
        self.assertIn(f"{prefix}/docs/security/macos-signing-notarization.md", names)
        self.assertEqual({member.mtime for member in names.values()}, {1700000000})

    def test_checksums_detect_tampering(self) -> None:
        archive = self.package(self.root / "release")
        checksum = archive.parent / "SHA256SUMS"
        self.run_release("checksums", "--output-dir", archive.parent)
        self.run_release("verify-checksums", "--checksum-file", checksum)
        archive.write_bytes(archive.read_bytes() + b"tampered")
        result = self.run_release("verify-checksums", "--checksum-file", checksum, succeed=False)
        self.assertIn("checksum mismatch", result.stderr)

    def test_macos_signing_plan_is_deterministic_and_credential_free(self) -> None:
        binaries = self.macos_binaries()
        first = self.root / "first-plan.json"
        second = self.root / "second-plan.json"
        arguments = (
            "macos-signing-plan", "--input-dir", binaries,
            "--version", "0.1.0-beta.1", "--target-arch", "aarch64",
            "--signing-identity",
            "Developer ID Application: UNPROVISIONED WIKISYNC RELEASE (AAAAAAAAAA)",
            "--team-id", "AAAAAAAAAA", "--certificate-sha1", "0" * 40,
            "--source-date-epoch", "1700000000", "--credential-free-dry-run",
        )
        self.run_release(*arguments, "--output-file", first)
        for path in binaries.iterdir():
            os.utime(path, (1800000000, 1800000000))
        self.run_release(*arguments, "--output-file", second)
        self.assertEqual(first.read_bytes(), second.read_bytes())
        plan = json.loads(first.read_bytes())
        self.assertTrue(plan["credential_free_dry_run"])
        self.assertEqual([item["name"] for item in plan["artifacts"]], [
            "wikisync", "wikisyncd", "wikisync-gui",
        ])
        self.assertEqual(
            {step["arguments"][4] for step in plan["signing_steps"]}, {"runtime"}
        )
        self.assertTrue(all("--timestamp" in step["arguments"] for step in plan["signing_steps"]))
        self.assertEqual(
            plan["notarization"]["stapling"],
            "not supported by the current tar.gz distribution container",
        )

        production_with_placeholder = self.run_release(
            *arguments[:-1], "--output-file", self.root / "unsafe-plan.json", succeed=False,
        )
        self.assertIn("all-zero certificate fingerprint", production_with_placeholder.stderr)

    def test_macos_signing_plan_rejects_wrong_architecture_and_identity(self) -> None:
        binaries = self.macos_binaries()
        base = (
            "macos-signing-plan", "--input-dir", binaries,
            "--output-file", self.root / "plan.json", "--version", "0.1.0",
            "--certificate-sha1", "1" * 40, "--source-date-epoch", "1700000000",
        )
        wrong_architecture = self.run_release(
            *base, "--target-arch", "x86_64",
            "--signing-identity", "Developer ID Application: WikiSyncer (AAAAAAAAAA)",
            "--team-id", "AAAAAAAAAA", succeed=False,
        )
        self.assertIn("does not contain x86_64", wrong_architecture.stderr)
        wrong_team = self.run_release(
            *base, "--target-arch", "aarch64",
            "--signing-identity", "Developer ID Application: WikiSyncer (AAAAAAAAAA)",
            "--team-id", "BBBBBBBBBB", succeed=False,
        )
        self.assertIn("Team ID does not match", wrong_team.stderr)

    def test_macho_validation_rejects_truncated_or_invalid_thin_and_fat_files(self) -> None:
        candidate = self.root / "candidate-macho"

        def write_candidate(payload: bytes) -> None:
            candidate.write_bytes(payload)
            candidate.chmod(0o755)

        write_candidate(b"\xcf\xfa\xed\xfe\x0c\x00\x00\x01")
        with self.assertRaisesRegex(RELEASE_MODULE.ReleaseError, "truncated Mach-O header"):
            RELEASE_MODULE.macho_architectures(candidate)

        not_executable = bytearray(minimal_macho(0x0100000C))
        struct.pack_into("<I", not_executable, 12, 1)  # MH_OBJECT
        write_candidate(bytes(not_executable))
        with self.assertRaisesRegex(RELEASE_MODULE.ReleaseError, "not an MH_EXECUTE"):
            RELEASE_MODULE.macho_architectures(candidate)

        invalid_command = bytearray(minimal_macho(0x0100000C))
        struct.pack_into("<I", invalid_command, 36, 4)
        write_candidate(bytes(invalid_command))
        with self.assertRaisesRegex(RELEASE_MODULE.ReleaseError, "invalid size"):
            RELEASE_MODULE.macho_architectures(candidate)

        write_candidate(b"\xca\xfe\xba\xbe" + struct.pack(">I", 1))
        with self.assertRaisesRegex(RELEASE_MODULE.ReleaseError, "truncated fat header"):
            RELEASE_MODULE.macho_architectures(candidate)

        outside_record = struct.pack(">IIIII", 0x0100000C, 0, 4096, 104, 4)
        write_candidate(b"\xca\xfe\xba\xbe" + struct.pack(">I", 1) + outside_record)
        with self.assertRaisesRegex(RELEASE_MODULE.ReleaseError, "slice is outside"):
            RELEASE_MODULE.macho_architectures(candidate)

        valid_fat = fat_macho(
            [
                (0x01000007, minimal_macho(0x01000007)),
                (0x0100000C, minimal_macho(0x0100000C)),
            ]
        )
        write_candidate(valid_fat)
        self.assertEqual(
            RELEASE_MODULE.macho_architectures(candidate), ["aarch64", "x86_64"]
        )

        mismatched = bytearray(valid_fat)
        first_slice_offset = struct.unpack_from(">I", mismatched, 16)[0]
        struct.pack_into("<I", mismatched, first_slice_offset + 4, 0x0100000C)
        write_candidate(bytes(mismatched))
        with self.assertRaisesRegex(RELEASE_MODULE.ReleaseError, "does not match"):
            RELEASE_MODULE.macho_architectures(candidate)

    def test_macos_archive_payload_matches_the_selected_inputs(self) -> None:
        destination = self.root / "macos-release"
        self.run_release(
            "package", "--input-dir", self.binaries, "--output-dir", destination,
            "--version", "0.1.0", "--target-os", "macos", "--target-arch", "aarch64",
            "--source-date-epoch", "1700000000",
        )
        archive = destination / "wikisync-0.1.0-macos-aarch64.tar.gz"
        RELEASE_MODULE.verify_archive_path(archive)
        RELEASE_MODULE.verify_archive_binary_payload(archive, self.binaries)
        (self.binaries / "wikisyncd").write_bytes(b"different signed payload\n")
        with self.assertRaisesRegex(RELEASE_MODULE.ReleaseError, "does not match signed input"):
            RELEASE_MODULE.verify_archive_binary_payload(archive, self.binaries)

    def test_codesign_policy_and_notarization_receipt_are_fail_closed(self) -> None:
        identity = "Developer ID Application: WikiSyncer Release (AAAAAAAAAA)"
        details = "\n".join((
            "Identifier=org.wikisync.WikiSyncer.cli",
            "CodeDirectory v=20500 size=100 flags=0x10000(runtime) hashes=1+2 location=embedded",
            f"Authority={identity}",
            "Authority=Developer ID Certification Authority",
            "Authority=Apple Root CA",
            "Timestamp=22 Aug 2026 at 12:00:00",
            "TeamIdentifier=AAAAAAAAAA",
        ))
        RELEASE_MODULE.parse_codesign_details(
            details, identity, "AAAAAAAAAA", "org.wikisync.WikiSyncer.cli"
        )
        with self.assertRaisesRegex(RELEASE_MODULE.ReleaseError, "hardened runtime"):
            RELEASE_MODULE.parse_codesign_details(
                details.replace("(runtime)", "(none)"),
                identity, "AAAAAAAAAA", "org.wikisync.WikiSyncer.cli",
            )
        with self.assertRaisesRegex(RELEASE_MODULE.ReleaseError, "secure timestamp"):
            RELEASE_MODULE.parse_codesign_details(
                details.replace("Timestamp=", "Signed Time="),
                identity, "AAAAAAAAAA", "org.wikisync.WikiSyncer.cli",
            )

        submission_id = "8d96bd1d-37af-45d1-9dcd-27d12db9cc12"
        receipt = self.root / "notarization.json"
        receipt.write_text(
            json.dumps({"id": submission_id, "message": "Successfully uploaded file", "status": "Accepted"}),
            encoding="utf-8",
        )
        self.run_release(
            "validate-notarization-receipt", "--receipt", receipt,
            "--submission-id", submission_id,
        )
        receipt.write_text(
            json.dumps({"id": submission_id, "message": "Archive contains critical errors", "status": "Invalid"}),
            encoding="utf-8",
        )
        rejected = self.run_release(
            "validate-notarization-receipt", "--receipt", receipt,
            "--submission-id", submission_id, succeed=False,
        )
        self.assertIn("status is not Accepted", rejected.stderr)

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

    def test_signed_archive_layout_uses_the_checksummed_immutable_snapshot(self) -> None:
        archive = self.package(self.root / "release")
        expected = hashlib.sha256(archive.read_bytes()).hexdigest()
        replacement = self.root / "replacement.tar.gz"
        replacement.write_bytes(b"not the signed archive")
        original_verify = RELEASE_MODULE.verify_archive_file

        def substitute_path_then_verify(snapshot, archive_name):
            os.replace(replacement, archive)
            return original_verify(snapshot, archive_name)

        with mock.patch.object(
            RELEASE_MODULE,
            "verify_archive_file",
            side_effect=substitute_path_then_verify,
        ):
            RELEASE_MODULE.verify_signed_archive(expected, archive)

        self.assertEqual(archive.read_bytes(), b"not the signed archive")

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
        linked_key = self.root / "linked-release-key"
        linked_key.symlink_to(key)
        linked = self.run_release(
            "sign-checksums", "--checksum-file", checksum,
            "--private-key", linked_key, succeed=False,
        )
        self.assertIn("regular, non-symlink", linked.stderr)
        self.run_release("sign-checksums", "--checksum-file", checksum, "--private-key", key)
        signature = checksum.with_name("SHA256SUMS.sig")
        self.assertTrue(signature.is_file())
        self.run_release(
            "verify-signature", "--checksum-file", checksum, "--signature", signature,
            "--allowed-signers", allowed, "--signer-identity", "release@wikisync",
        )
        verified = self.run_release(
            "verify-release", "--checksum-file", checksum, "--signature", signature,
            "--allowed-signers", allowed, "--signer-identity", "release@wikisync",
        )
        self.assertIn("release set: 1 signed archive(s) verified", verified.stdout)

        wrong_identity = self.run_release(
            "verify-release", "--checksum-file", checksum, "--signature", signature,
            "--allowed-signers", allowed, "--signer-identity", "other@wikisync",
            succeed=False,
        )
        self.assertIn("detached signature verification failed", wrong_identity.stderr)

        unsigned = archive.parent / "wikisync-9.9.9-linux-x86_64.tar.gz"
        shutil.copyfile(archive, unsigned)
        extra = self.run_release(
            "verify-release", "--checksum-file", checksum, "--signature", signature,
            "--allowed-signers", allowed, "--signer-identity", "release@wikisync",
            succeed=False,
        )
        self.assertIn("archives not covered by the signed manifest", extra.stderr)

    @unittest.skipUnless(shutil.which("ssh-keygen"), "ssh-keygen unavailable")
    def test_release_verification_authenticates_manifest_before_archive(self) -> None:
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
        self.run_release("sign-checksums", "--checksum-file", checksum, "--private-key", key)

        checksum.write_text("0" * 64 + f"  {archive.name}\n", encoding="ascii")
        rejected = self.run_release(
            "verify-release", "--checksum-file", checksum,
            "--signature", checksum.with_name("SHA256SUMS.sig"),
            "--allowed-signers", allowed, "--signer-identity", "release@wikisync",
            succeed=False,
        )
        self.assertIn("detached signature verification failed", rejected.stderr)
        self.assertNotIn("checksum mismatch", rejected.stderr)

    @unittest.skipUnless(shutil.which("ssh-keygen"), "ssh-keygen unavailable")
    def test_non_ed25519_signing_key_is_rejected(self) -> None:
        archive = self.package(self.root / "release")
        checksum = archive.parent / "SHA256SUMS"
        self.run_release("checksums", "--output-dir", archive.parent)
        key = self.root / "rsa-release-key"
        generated = subprocess.run(
            ["ssh-keygen", "-q", "-t", "rsa", "-b", "2048", "-N", "", "-f", str(key)],
            text=True, capture_output=True, check=False,
        )
        if generated.returncode != 0:
            self.skipTest(f"cannot generate RSA SSH key: {generated.stderr}")
        rejected = self.run_release(
            "sign-checksums", "--checksum-file", checksum, "--private-key", key,
            succeed=False,
        )
        self.assertIn("must be an Ed25519 SSH key", rejected.stderr)

        externally_signed = subprocess.run(
            [
                "ssh-keygen", "-Y", "sign", "-f", str(key),
                "-n", "wikisync-release", str(checksum),
            ],
            text=True, capture_output=True, check=False,
        )
        if externally_signed.returncode != 0:
            self.skipTest(f"cannot create RSA SSH signature: {externally_signed.stderr}")
        allowed = self.root / "rsa-allowed-signers"
        allowed.write_text(
            f"release@wikisync {key.with_suffix('.pub').read_text()}", encoding="ascii"
        )
        rejected_verification = self.run_release(
            "verify-release", "--checksum-file", checksum,
            "--signature", checksum.with_name("SHA256SUMS.sig"),
            "--allowed-signers", allowed, "--signer-identity", "release@wikisync",
            succeed=False,
        )
        self.assertIn("signature must use an Ed25519 SSH key", rejected_verification.stderr)


if __name__ == "__main__":
    unittest.main()
