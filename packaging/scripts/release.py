#!/usr/bin/env python3
"""Build and authenticate deterministic WikiSyncer release-candidate archives."""

from __future__ import annotations

import argparse
import base64
import binascii
import gzip
import hashlib
import io
import json
import os
from pathlib import Path, PurePosixPath
import re
import shutil
import stat
import struct
import subprocess
import sys
import tarfile
import tempfile
import uuid
from typing import BinaryIO


BINARIES = ("wikisync", "wikisyncd", "wikisync-gui")
VERSION_RE = re.compile(r"[0-9A-Za-z][0-9A-Za-z.+_-]*\Z")
CHECKSUM_RE = re.compile(r"([0-9a-f]{64})  ([0-9A-Za-z][0-9A-Za-z.+_-]*\.tar\.gz)\Z")
SIGNATURE_NAMESPACE = "wikisync-release"
MAX_ARCHIVE_BYTES = 4 * 1024 * 1024 * 1024
MAX_MEMBER_BYTES = 1024 * 1024 * 1024
MAX_TOTAL_UNCOMPRESSED_BYTES = 4 * 1024 * 1024 * 1024
MAX_ARCHIVE_MEMBERS = 128
MAX_CHECKSUM_MANIFEST_BYTES = 64 * 1024
MAX_SIGNATURE_BYTES = 64 * 1024
MAX_ALLOWED_SIGNERS_BYTES = 1024 * 1024
MAX_NOTARIZATION_RECEIPT_BYTES = 64 * 1024
MACOS_TEAM_RE = re.compile(r"[A-Z0-9]{10}\Z")
MACOS_CERTIFICATE_SHA1_RE = re.compile(r"[0-9A-F]{40}\Z")
MACOS_IDENTITY_RE = re.compile(
    r"Developer ID Application: [^\r\n]{1,180} \(([A-Z0-9]{10})\)\Z"
)
MACOS_IDENTIFIERS = {
    "wikisync": "org.wikisync.WikiSyncer.cli",
    "wikisyncd": "org.wikisync.WikiSyncer.daemon",
    "wikisync-gui": "org.wikisync.WikiSyncer.gui",
}
MACOS_CPU_TYPES = {
    0x01000007: "x86_64",
    0x0100000C: "aarch64",
}
LINUX_ELF_MACHINES = {
    62: "x86_64",  # EM_X86_64
    183: "aarch64",  # EM_AARCH64
}
MAX_MACHO_ARCHITECTURES = 32
MAX_MACHO_LOAD_COMMANDS = 4_096
MAX_MACHO_ALIGNMENT_EXPONENT = 30
CURRENT_REQUIRED_RELATIVE_FILES = {
    "RELEASE.txt",
    "LICENSE",
    "README.md",
    "docs/security/linux-package-repository-trust.md",
    "docs/security/macos-signing-notarization.md",
}


class ReleaseError(Exception):
    pass


def absolute_path(path: Path) -> Path:
    """Make a path absolute without resolving away a final-component symlink."""
    return Path(os.path.abspath(path))


def regular_file(path: Path, description: str) -> os.stat_result:
    try:
        metadata = path.lstat()
    except FileNotFoundError as error:
        raise ReleaseError(f"missing {description}: {path}") from error
    if not stat.S_ISREG(metadata.st_mode):
        raise ReleaseError(f"{description} must be a regular, non-symlink file: {path}")
    return metadata


def sha256(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as source:
        while chunk := source.read(1024 * 1024):
            digest.update(chunk)
    return digest.hexdigest()


def bounded_file_bytes(path: Path, description: str) -> bytes:
    metadata = regular_file(path, description)
    if metadata.st_size > MAX_MEMBER_BYTES:
        raise ReleaseError(f"{description} exceeds the {MAX_MEMBER_BYTES}-byte limit: {path}")
    return path.read_bytes()


def bounded_trust_file_bytes(path: Path, description: str, maximum: int) -> bytes:
    metadata = regular_file(path, description)
    if metadata.st_size > maximum:
        raise ReleaseError(f"{description} exceeds the {maximum}-byte limit: {path}")
    try:
        return path.read_bytes()
    except OSError as error:
        raise ReleaseError(f"cannot read {description}: {error}") from error


def checked_token(value: str, description: str) -> str:
    if not VERSION_RE.fullmatch(value):
        raise ReleaseError(f"invalid {description}: {value!r}")
    return value


def checked_macos_identity(identity: str, team_id: str) -> None:
    if MACOS_TEAM_RE.fullmatch(team_id) is None:
        raise ReleaseError("macOS Team ID must be 10 uppercase ASCII letters or digits")
    match = MACOS_IDENTITY_RE.fullmatch(identity)
    if match is None:
        raise ReleaseError(
            "macOS signing identity must be an exact Developer ID Application authority"
        )
    if match.group(1) != team_id:
        raise ReleaseError("macOS signing identity Team ID does not match --team-id")


def opened_regular_file(
    path: Path, description: str, maximum: int | None = None
) -> tuple[BinaryIO, os.stat_result]:
    flags = os.O_RDONLY | getattr(os, "O_CLOEXEC", 0) | getattr(os, "O_NOFOLLOW", 0)
    try:
        descriptor = os.open(path, flags)
    except OSError as error:
        raise ReleaseError(f"cannot securely open {description}: {path}: {error}") from error
    source = os.fdopen(descriptor, "rb")
    try:
        metadata = os.fstat(descriptor)
        if not stat.S_ISREG(metadata.st_mode):
            raise ReleaseError(f"{description} must be a regular, non-symlink file: {path}")
        if maximum is not None and metadata.st_size > maximum:
            raise ReleaseError(f"{description} exceeds the {maximum}-byte limit: {path}")
        return source, metadata
    except Exception:
        source.close()
        raise


def read_exact_at(
    source: BinaryIO, offset: int, size: int, path: Path, description: str
) -> bytes:
    source.seek(offset)
    payload = source.read(size)
    if len(payload) != size:
        raise ReleaseError(f"macOS release binary has a truncated {description}: {path}")
    return payload


def linux_elf_architecture(path: Path) -> str:
    source, metadata = opened_regular_file(path, "Linux release binary", MAX_MEMBER_BYTES)
    with source:
        if metadata.st_mode & 0o111 == 0:
            raise ReleaseError(f"Linux release binary is not executable: {path}")
        header = source.read(64)
    if len(header) < 64:
        raise ReleaseError(f"Linux release binary has a truncated ELF header: {path}")
    if header[:4] != b"\x7fELF":
        raise ReleaseError(f"Linux release binary is not ELF: {path}")
    if header[4] != 2:
        raise ReleaseError(f"Linux release binary is not 64-bit ELF: {path}")
    byte_order = {1: "<", 2: ">"}.get(header[5])
    if byte_order is None or header[6] != 1:
        raise ReleaseError(f"Linux release binary has an unsupported ELF encoding: {path}")
    file_type, machine, version = struct.unpack_from(f"{byte_order}HHI", header, 16)
    if file_type not in {2, 3}:  # ET_EXEC or ET_DYN (PIE)
        raise ReleaseError(f"Linux release binary is not an executable ELF file: {path}")
    if version != 1:
        raise ReleaseError(f"Linux release binary has an unsupported ELF version: {path}")
    architecture = LINUX_ELF_MACHINES.get(machine)
    if architecture is None:
        raise ReleaseError(f"unsupported ELF machine {machine}: {path}")
    header_size = struct.unpack_from(f"{byte_order}H", header, 52)[0]
    if header_size < 64 or header_size > metadata.st_size:
        raise ReleaseError(f"Linux release binary has an invalid ELF header size: {path}")
    return architecture


def verify_linux_binaries(args: argparse.Namespace) -> None:
    input_dir = absolute_path(args.input_dir)
    target_arch = checked_token(args.target_arch, "target architecture")
    if target_arch not in set(LINUX_ELF_MACHINES.values()):
        raise ReleaseError(f"unsupported Linux target architecture: {target_arch}")
    for binary in BINARIES:
        path = input_dir / binary
        architecture = linux_elf_architecture(path)
        if architecture != target_arch:
            raise ReleaseError(
                f"Linux release binary {binary} is {architecture}, expected {target_arch}"
            )
    print(f"Linux release binaries: ELF {target_arch} verified")


def verify_systemd_units(args: argparse.Namespace) -> None:
    systemd_analyze = shutil.which("systemd-analyze")
    if systemd_analyze is None:
        raise ReleaseError("systemd-analyze is required for native Linux unit verification")
    true_executable = shutil.which("true")
    if true_executable is None:
        raise ReleaseError("a native true executable is required for systemd unit verification")
    repo_root = args.repo_root.resolve()
    source_directory = repo_root / "packaging" / "systemd"
    documentation_directory = repo_root / "docs" / "operations"
    with tempfile.TemporaryDirectory(prefix="wikisync-systemd-verify-") as temporary:
        root = Path(temporary)
        library = root / "library"
        library.mkdir()
        rendered = []
        for name in (
            "wikisyncd.service",
            "wikisyncd-health.service",
            "wikisyncd-health.timer",
        ):
            template = source_directory / f"{name}.in"
            payload = bounded_file_bytes(template, "systemd unit template").decode("utf-8")
            payload = (
                payload.replace("@WIKISYNCD@", true_executable)
                .replace("@LIBRARY@", str(library))
                .replace("@DOCUMENTATION_DIRECTORY@", str(documentation_directory))
            )
            if re.search(r"@[A-Z][A-Z_]*@", payload):
                raise ReleaseError(f"unresolved token in rendered systemd unit: {template}")
            destination = root / name
            destination.write_text(payload, encoding="utf-8")
            rendered.append(destination)
        command = [systemd_analyze, "--user", "verify", *(str(path) for path in rendered)]
        completed = subprocess.run(command, text=True, capture_output=True, check=False)
        if completed.stdout:
            print(completed.stdout, end="")
        if completed.stderr:
            print(completed.stderr, file=sys.stderr, end="")
        if completed.returncode != 0:
            raise ReleaseError(
                f"systemd-analyze rejected the rendered user units (exit {completed.returncode})"
            )
    print("Linux systemd user units: native systemd-analyze verification passed")


def thin_macho_architecture(
    source: BinaryIO, path: Path, slice_offset: int, slice_size: int
) -> str:
    magic = read_exact_at(source, slice_offset, 4, path, "Mach-O header")
    thin_format = {
        b"\xce\xfa\xed\xfe": ("<", False, 28),
        b"\xcf\xfa\xed\xfe": ("<", True, 32),
        b"\xfe\xed\xfa\xce": (">", False, 28),
        b"\xfe\xed\xfa\xcf": (">", True, 32),
    }.get(magic)
    if thin_format is None:
        raise ReleaseError(f"macOS release binary slice is not Mach-O: {path}")
    endian, is_64_bit, header_size = thin_format
    if slice_size < header_size:
        raise ReleaseError(f"macOS release binary has a truncated Mach-O header: {path}")
    header = read_exact_at(source, slice_offset, header_size, path, "Mach-O header")
    fields = struct.unpack(f"{endian}{'8I' if is_64_bit else '7I'}", header)
    cpu_type = fields[1]
    architecture = MACOS_CPU_TYPES.get(cpu_type)
    if architecture is None:
        raise ReleaseError(f"unsupported Mach-O CPU type 0x{cpu_type:08x}: {path}")
    if not is_64_bit:
        raise ReleaseError(f"supported macOS CPU type requires a 64-bit Mach-O header: {path}")
    file_type, command_count, command_bytes = fields[3:6]
    if file_type != 2:  # MH_EXECUTE
        raise ReleaseError(f"macOS release binary is not an MH_EXECUTE Mach-O file: {path}")
    if command_count > MAX_MACHO_LOAD_COMMANDS:
        raise ReleaseError(f"Mach-O load-command count exceeds the limit: {path}")
    if command_bytes > slice_size - header_size:
        raise ReleaseError(f"Mach-O load commands exceed their containing slice: {path}")

    command_offset = slice_offset + header_size
    remaining = command_bytes
    for _ in range(command_count):
        if remaining < 8:
            raise ReleaseError(f"Mach-O load-command table is truncated: {path}")
        command_header = read_exact_at(
            source, command_offset, 8, path, "Mach-O load-command header"
        )
        _, command_size = struct.unpack(f"{endian}II", command_header)
        alignment = 8 if is_64_bit else 4
        if command_size < 8 or command_size % alignment != 0 or command_size > remaining:
            raise ReleaseError(f"Mach-O load command has an invalid size: {path}")
        command_offset += command_size
        remaining -= command_size
    if remaining != 0:
        raise ReleaseError(f"Mach-O load-command count and size disagree: {path}")
    return architecture


def macho_architectures(path: Path) -> list[str]:
    source, metadata = opened_regular_file(path, "macOS release binary", MAX_MEMBER_BYTES)
    with source:
        if metadata.st_mode & 0o111 == 0:
            raise ReleaseError(f"macOS release binary is not executable: {path}")
        header = read_exact_at(source, 0, 8, path, "Mach-O header")
        magic = header[:4]
        if magic in {
            b"\xce\xfa\xed\xfe",
            b"\xcf\xfa\xed\xfe",
            b"\xfe\xed\xfa\xce",
            b"\xfe\xed\xfa\xcf",
        }:
            return [thin_macho_architecture(source, path, 0, metadata.st_size)]

        fat_format = {
            b"\xca\xfe\xba\xbe": (">", 20, False),
            b"\xbe\xba\xfe\xca": ("<", 20, False),
            b"\xca\xfe\xba\xbf": (">", 32, True),
            b"\xbf\xba\xfe\xca": ("<", 32, True),
        }.get(magic)
        if fat_format is None:
            raise ReleaseError(f"macOS release binary is not Mach-O: {path}")
        endian, record_size, fat_64_bit = fat_format
        architecture_count = struct.unpack(f"{endian}I", header[4:8])[0]
        if architecture_count == 0 or architecture_count > MAX_MACHO_ARCHITECTURES:
            raise ReleaseError(f"invalid Mach-O architecture count {architecture_count}: {path}")
        table_size = architecture_count * record_size
        table_end = 8 + table_size
        if table_end > metadata.st_size:
            raise ReleaseError(f"macOS release binary has a truncated fat header: {path}")
        table = read_exact_at(source, 8, table_size, path, "fat architecture table")
        architectures = []
        slices: list[tuple[int, int]] = []
        for index in range(architecture_count):
            record = table[index * record_size : (index + 1) * record_size]
            if fat_64_bit:
                cpu_type, _, offset, size, alignment, reserved = struct.unpack(
                    f"{endian}IIQQII", record
                )
                if reserved != 0:
                    raise ReleaseError(f"fat Mach-O architecture has nonzero reserved data: {path}")
            else:
                cpu_type, _, offset, size, alignment = struct.unpack(f"{endian}IIIII", record)
            architecture = MACOS_CPU_TYPES.get(cpu_type)
            if architecture is None:
                raise ReleaseError(f"unsupported Mach-O CPU type 0x{cpu_type:08x}: {path}")
            if architecture in architectures:
                raise ReleaseError(f"duplicate Mach-O architecture {architecture}: {path}")
            if alignment > MAX_MACHO_ALIGNMENT_EXPONENT or offset % (1 << alignment) != 0:
                raise ReleaseError(f"fat Mach-O architecture has invalid alignment: {path}")
            slice_end = offset + size
            if offset < table_end or size == 0 or slice_end > metadata.st_size:
                raise ReleaseError(f"fat Mach-O architecture slice is outside the file: {path}")
            if any(
                offset < other_end and other_offset < slice_end
                for other_offset, other_end in slices
            ):
                raise ReleaseError(f"fat Mach-O architecture slices overlap: {path}")
            slice_architecture = thin_macho_architecture(source, path, offset, size)
            if slice_architecture != architecture:
                raise ReleaseError(f"fat Mach-O architecture record does not match its slice: {path}")
            architectures.append(architecture)
            slices.append((offset, slice_end))
        return sorted(architectures)


def write_atomic_json(destination: Path, value: object) -> None:
    destination = absolute_path(destination)
    if destination.exists() and not stat.S_ISREG(destination.lstat().st_mode):
        raise ReleaseError(f"JSON output must replace only a regular file: {destination}")
    destination.parent.mkdir(parents=True, exist_ok=True)
    payload = (json.dumps(value, sort_keys=True, separators=(",", ":")) + "\n").encode()
    descriptor, temporary_name = tempfile.mkstemp(prefix=f".{destination.name}.", dir=destination.parent)
    temporary = Path(temporary_name)
    try:
        with os.fdopen(descriptor, "wb") as output:
            output.write(payload)
            output.flush()
            os.fsync(output.fileno())
        temporary.chmod(0o644)
        os.replace(temporary, destination)
    finally:
        if temporary.exists():
            temporary.unlink()
    print(destination)


def macos_signing_plan(args: argparse.Namespace) -> None:
    version = checked_token(args.version, "version")
    target_arch = checked_token(args.target_arch, "target architecture")
    if target_arch not in {"aarch64", "x86_64"}:
        raise ReleaseError("macOS target architecture must be aarch64 or x86_64")
    checked_macos_identity(args.signing_identity, args.team_id)
    certificate_sha1 = args.certificate_sha1.upper()
    if MACOS_CERTIFICATE_SHA1_RE.fullmatch(certificate_sha1) is None:
        raise ReleaseError("macOS certificate SHA-1 must be exactly 40 hexadecimal digits")
    placeholder = certificate_sha1 == "0" * 40
    if placeholder != args.credential_free_dry_run:
        raise ReleaseError(
            "the all-zero certificate fingerprint is allowed only with "
            "--credential-free-dry-run, which must not use a production fingerprint"
        )
    epoch = args.source_date_epoch
    if epoch is None:
        raw_epoch = os.environ.get("SOURCE_DATE_EPOCH")
        if raw_epoch is None:
            raise ReleaseError("set SOURCE_DATE_EPOCH or pass --source-date-epoch")
        try:
            epoch = int(raw_epoch)
        except ValueError as error:
            raise ReleaseError("SOURCE_DATE_EPOCH must be a non-negative integer") from error
    if epoch < 0:
        raise ReleaseError("source date epoch must be non-negative")

    input_dir = args.input_dir.resolve()
    artifacts = []
    signing_steps = []
    verification_steps = []
    for binary in BINARIES:
        path = input_dir / binary
        architectures = macho_architectures(path)
        if target_arch not in architectures:
            raise ReleaseError(
                f"macOS release binary {binary} does not contain {target_arch}: {architectures}"
            )
        relative = binary
        artifacts.append(
            {
                "architectures": architectures,
                "identifier": MACOS_IDENTIFIERS[binary],
                "name": binary,
                "sha256_before_signing": sha256(path),
                "size_before_signing": path.stat().st_size,
            }
        )
        signing_steps.append(
            {
                "arguments": [
                    "--force", "--sign", certificate_sha1, "--options", "runtime",
                    "--timestamp", "--identifier", MACOS_IDENTIFIERS[binary], relative,
                ],
                "tool": "/usr/bin/codesign",
            }
        )
        verification_steps.append(
            {
                "arguments": [
                    "--verify", "--all-architectures", "--strict", "--verbose=4", relative,
                ],
                "tool": "/usr/bin/codesign",
            }
        )
    plan = {
        "artifacts": artifacts,
        "credential_free_dry_run": args.credential_free_dry_run,
        "notarization": {
            "accepted_status": "Accepted",
            "credentials": "protected keychain profile supplied only in an authorized release run",
            "stapling": "not supported by the current tar.gz distribution container",
            "submission_formats": ["zip", "pkg", "dmg"],
            "tool": "xcrun notarytool",
        },
        "schema_version": 1,
        "signer": {
            "certificate_sha1": certificate_sha1,
            "identity": args.signing_identity,
            "team_id": args.team_id,
        },
        "signing_steps": signing_steps,
        "source_date_epoch": epoch,
        "target_arch": target_arch,
        "target_os": "macos",
        "verification_steps": verification_steps,
        "version": version,
    }
    write_atomic_json(args.output_file, plan)


def parse_codesign_details(details: str, identity: str, team_id: str, identifier: str) -> None:
    values: dict[str, list[str]] = {}
    code_directory_lines = []
    for line in details.splitlines():
        if line.startswith("CodeDirectory "):
            code_directory_lines.append(line)
        if "=" in line:
            key, value = line.split("=", 1)
            values.setdefault(key, []).append(value)
    authorities = values.get("Authority", [])
    if not authorities or authorities[0] != identity:
        raise ReleaseError("macOS signature has an unexpected leaf authority")
    if authorities[1:] != ["Developer ID Certification Authority", "Apple Root CA"]:
        raise ReleaseError("macOS signature has an unexpected Developer ID authority chain")
    if values.get("TeamIdentifier") != [team_id]:
        raise ReleaseError("macOS signature has an unexpected TeamIdentifier")
    if values.get("Identifier") != [identifier]:
        raise ReleaseError("macOS signature has an unexpected code identifier")
    if len(code_directory_lines) != 1 or "(runtime)" not in code_directory_lines[0]:
        raise ReleaseError("macOS signature does not enable the hardened runtime")
    if len(values.get("Timestamp", [])) != 1 or "Signed Time" in values:
        raise ReleaseError("macOS signature does not contain exactly one secure timestamp")


def verify_macos_signatures(args: argparse.Namespace) -> None:
    checked_macos_identity(args.signing_identity, args.team_id)
    target_arch = checked_token(args.target_arch, "target architecture")
    if target_arch not in {"aarch64", "x86_64"}:
        raise ReleaseError("macOS target architecture must be aarch64 or x86_64")
    codesign = Path("/usr/bin/codesign")
    regular_file(codesign, "Apple codesign tool")
    input_dir = args.input_dir.resolve()
    for binary in BINARIES:
        path = input_dir / binary
        architectures = macho_architectures(path)
        if target_arch not in architectures:
            raise ReleaseError(f"macOS release binary {binary} does not contain {target_arch}")
        verified = subprocess.run(
            [
                str(codesign), "--verify", "--all-architectures", "--strict",
                "--verbose=4", str(path),
            ],
            stdin=subprocess.DEVNULL,
            capture_output=True,
            text=True,
            check=False,
        )
        if verified.returncode != 0:
            raise ReleaseError(
                f"codesign verification failed for {binary}: {verified.stderr.strip()}"
            )
        for architecture in architectures:
            codesign_architecture = "arm64" if architecture == "aarch64" else architecture
            displayed = subprocess.run(
                [
                    str(codesign), "--display", "--verbose=4", "--architecture",
                    codesign_architecture, str(path),
                ],
                stdin=subprocess.DEVNULL,
                capture_output=True,
                text=True,
                check=False,
            )
            if displayed.returncode != 0:
                raise ReleaseError(
                    f"cannot inspect code signature for {binary}/{architecture}: "
                    f"{displayed.stderr.strip()}"
                )
            parse_codesign_details(
                displayed.stderr + displayed.stdout,
                args.signing_identity,
                args.team_id,
                MACOS_IDENTIFIERS[binary],
            )
        print(f"{binary}: Developer ID signature OK")


def verify_archive_binary_payload(archive_path: Path, input_dir: Path) -> None:
    expected = {}
    for binary in BINARIES:
        path = input_dir / binary
        metadata = regular_file(path, "signed macOS release binary")
        if metadata.st_size > MAX_MEMBER_BYTES:
            raise ReleaseError(f"signed macOS release binary exceeds size limit: {path}")
        expected[binary] = sha256(path)
    seen = set()
    macos_release_metadata = False
    with tarfile.open(archive_path, mode="r:gz") as archive:
        for member in archive:
            path = PurePosixPath(member.name)
            if len(path.parts) == 2 and path.parts[1] == "RELEASE.txt":
                extracted_metadata = archive.extractfile(member)
                if extracted_metadata is None:
                    raise ReleaseError("cannot read macOS RELEASE.txt")
                metadata = extracted_metadata.read(MAX_MEMBER_BYTES + 1)
                if len(metadata) > MAX_MEMBER_BYTES:
                    raise ReleaseError("macOS RELEASE.txt exceeds size limit")
                try:
                    metadata_lines = metadata.decode("utf-8").splitlines()
                except UnicodeDecodeError as error:
                    raise ReleaseError("macOS RELEASE.txt is not UTF-8") from error
                macos_release_metadata = "target-os: macos" in metadata_lines
            if len(path.parts) != 3 or path.parts[1] != "bin":
                continue
            binary = path.parts[2]
            if binary not in expected:
                continue
            extracted = archive.extractfile(member)
            if extracted is None:
                raise ReleaseError(f"cannot read archived macOS release binary: {binary}")
            digest = hashlib.sha256()
            size = 0
            while chunk := extracted.read(1024 * 1024):
                size += len(chunk)
                if size > MAX_MEMBER_BYTES:
                    raise ReleaseError(f"archived macOS release binary exceeds size limit: {binary}")
                digest.update(chunk)
            if digest.hexdigest() != expected[binary]:
                raise ReleaseError(
                    f"archived macOS release binary does not match signed input: {binary}"
                )
            seen.add(binary)
    if seen != set(BINARIES):
        raise ReleaseError("macOS release archive does not contain the exact signed binary set")
    if not macos_release_metadata:
        raise ReleaseError("release archive metadata does not identify target-os: macos")


def verify_macos_release_archive(args: argparse.Namespace) -> None:
    verify_macos_signatures(args)
    archive_path = absolute_path(args.archive)
    verify_archive_path(archive_path)
    verify_archive_binary_payload(archive_path, args.input_dir.resolve())
    print(f"{archive_path.name}: signed macOS payload matches archive")


def validate_notarization_receipt(args: argparse.Namespace) -> None:
    receipt_path = absolute_path(args.receipt)
    payload = bounded_trust_file_bytes(
        receipt_path, "notarization receipt", MAX_NOTARIZATION_RECEIPT_BYTES
    )
    try:
        receipt = json.loads(payload)
    except (UnicodeDecodeError, json.JSONDecodeError) as error:
        raise ReleaseError(f"cannot parse notarization receipt JSON: {error}") from error
    if not isinstance(receipt, dict):
        raise ReleaseError("notarization receipt must be a JSON object")
    receipt_id = receipt.get("id")
    try:
        parsed_id = uuid.UUID(receipt_id) if isinstance(receipt_id, str) else None
    except ValueError as error:
        raise ReleaseError("notarization receipt has an invalid submission ID") from error
    if parsed_id is None:
        raise ReleaseError("notarization receipt is missing its submission ID")
    try:
        expected_id = uuid.UUID(args.submission_id)
    except ValueError as error:
        raise ReleaseError("--submission-id must be a UUID") from error
    if parsed_id != expected_id:
        raise ReleaseError("notarization receipt submission ID does not match the expected request")
    if receipt.get("status") != "Accepted":
        raise ReleaseError(
            f"notarization receipt status is not Accepted: {receipt.get('status')!r}"
        )
    print(f"notarization submission {parsed_id}: Accepted")


def archive_entry(name: str, data: bytes | None, mode: int, epoch: int) -> tuple[tarfile.TarInfo, bytes | None]:
    info = tarfile.TarInfo(name)
    info.uid = 0
    info.gid = 0
    info.uname = "root"
    info.gname = "root"
    info.mtime = epoch
    info.mode = mode
    if data is None:
        info.type = tarfile.DIRTYPE
        info.size = 0
    else:
        info.type = tarfile.REGTYPE
        info.size = len(data)
    return info, data


def repository_files(repo_root: Path, target_os: str) -> list[tuple[str, bytes, int]]:
    sources = [
        ("LICENSE", repo_root / "LICENSE"),
        ("README.md", repo_root / "README.md"),
        ("packaging/README.md", repo_root / "packaging" / "README.md"),
        (
            "docs/security/linux-package-repository-trust.md",
            repo_root / "docs" / "security" / "linux-package-repository-trust.md",
        ),
        (
            "docs/security/macos-signing-notarization.md",
            repo_root / "docs" / "security" / "macos-signing-notarization.md",
        ),
    ]
    operations = repo_root / "docs" / "operations"
    for path in sorted(operations.glob("*.md")):
        sources.append((f"docs/operations/{path.name}", path))
    if target_os == "macos":
        launchd = repo_root / "packaging" / "launchd"
        for path in sorted(launchd.iterdir()):
            if path.suffix in {".in", ".sh"}:
                sources.append((f"service/{path.name}", path))
    else:
        systemd = repo_root / "packaging" / "systemd"
        for path in sorted(systemd.glob("*.in")):
            sources.append((f"service/{path.name}", path))

    result = []
    for destination, source in sources:
        regular_file(source, "release documentation")
        result.append((destination, bounded_file_bytes(source, "release documentation"), 0o644))
    return result


def build_archive(args: argparse.Namespace) -> None:
    version = checked_token(args.version, "version")
    target_arch = checked_token(args.target_arch, "target architecture")
    repo_root = args.repo_root.resolve()
    input_dir = args.input_dir.resolve()
    output_dir = args.output_dir.resolve()
    epoch = args.source_date_epoch
    if epoch is None:
        raw_epoch = os.environ.get("SOURCE_DATE_EPOCH")
        if raw_epoch is None:
            raise ReleaseError("set SOURCE_DATE_EPOCH or pass --source-date-epoch")
        try:
            epoch = int(raw_epoch)
        except ValueError as error:
            raise ReleaseError("SOURCE_DATE_EPOCH must be a non-negative integer") from error
    if epoch < 0:
        raise ReleaseError("source date epoch must be non-negative")

    root = f"wikisync-{version}-{args.target_os}-{target_arch}"
    entries: list[tuple[str, bytes | None, int]] = [
        (root, None, 0o755),
        (f"{root}/bin", None, 0o755),
    ]
    for binary in BINARIES:
        source = input_dir / binary
        metadata = regular_file(source, "release binary")
        if metadata.st_mode & 0o111 == 0:
            raise ReleaseError(f"release binary is not executable: {source}")
        entries.append((f"{root}/bin/{binary}", bounded_file_bytes(source, "release binary"), 0o755))

    release_text = (
        "WikiSyncer release candidate\n"
        f"version: {version}\n"
        f"target-os: {args.target_os}\n"
        f"target-arch: {target_arch}\n"
        f"source-date-epoch: {epoch}\n\n"
        "This archive is not, by itself, proof of publisher identity. Verify its entry in\n"
        "SHA256SUMS and the detached SHA256SUMS.sig before installation. Platform code\n"
        "signing/notarization status must be established separately by the releaser.\n"
    ).encode()
    entries.append((f"{root}/RELEASE.txt", release_text, 0o644))
    for destination, data, mode in repository_files(repo_root, args.target_os):
        entries.append((f"{root}/{destination}", data, mode))

    directories = {root}
    for name, _, _ in entries:
        path = PurePosixPath(name)
        directories.update(str(parent) for parent in path.parents if str(parent) != ".")
    existing = {name for name, _, _ in entries}
    for directory in sorted(directories - existing):
        entries.append((directory, None, 0o755))
    entries.sort(key=lambda entry: (entry[0].count("/"), entry[0]))
    if len(entries) > MAX_ARCHIVE_MEMBERS:
        raise ReleaseError(f"archive would exceed the {MAX_ARCHIVE_MEMBERS}-member limit")
    total_size = sum(len(data) for _, data, _ in entries if data is not None)
    if total_size > MAX_TOTAL_UNCOMPRESSED_BYTES:
        raise ReleaseError(
            f"archive would exceed the {MAX_TOTAL_UNCOMPRESSED_BYTES}-byte uncompressed limit"
        )

    output_dir.mkdir(parents=True, exist_ok=True)
    archive_path = output_dir / f"{root}.tar.gz"
    descriptor, temporary_name = tempfile.mkstemp(prefix=f".{archive_path.name}.", dir=output_dir)
    temporary_path = Path(temporary_name)
    try:
        with os.fdopen(descriptor, "wb") as raw:
            with gzip.GzipFile(filename="", mode="wb", fileobj=raw, compresslevel=9, mtime=epoch) as compressed:
                with tarfile.open(fileobj=compressed, mode="w", format=tarfile.GNU_FORMAT) as archive:
                    for name, data, mode in entries:
                        info, payload = archive_entry(name, data, mode, epoch)
                        archive.addfile(info, None if payload is None else io.BytesIO(payload))
            raw.flush()
            os.fsync(raw.fileno())
        temporary_path.chmod(0o644)
        os.replace(temporary_path, archive_path)
    finally:
        if temporary_path.exists():
            temporary_path.unlink()
    print(archive_path)


def write_checksums(args: argparse.Namespace) -> None:
    output_dir = args.output_dir.resolve()
    archives = sorted(output_dir.glob("wikisync-*.tar.gz"), key=lambda path: path.name)
    if not archives:
        raise ReleaseError(f"no WikiSyncer archives in {output_dir}")
    for archive in archives:
        metadata = regular_file(archive, "release archive")
        if metadata.st_size > MAX_ARCHIVE_BYTES:
            raise ReleaseError(f"release archive exceeds the {MAX_ARCHIVE_BYTES}-byte limit: {archive}")
    payload = "".join(f"{sha256(archive)}  {archive.name}\n" for archive in archives).encode()
    destination = output_dir / "SHA256SUMS"
    descriptor, temporary_name = tempfile.mkstemp(prefix=".SHA256SUMS.", dir=output_dir)
    temporary = Path(temporary_name)
    try:
        with os.fdopen(descriptor, "wb") as output:
            output.write(payload)
            output.flush()
            os.fsync(output.fileno())
        temporary.chmod(0o644)
        os.replace(temporary, destination)
    finally:
        if temporary.exists():
            temporary.unlink()
    print(destination)


def checksum_entries_from_bytes(checksum_file: Path, payload: bytes) -> list[tuple[str, Path]]:
    try:
        text = payload.decode("ascii")
    except UnicodeDecodeError as error:
        raise ReleaseError(f"cannot read checksum manifest: {error}") from error
    if not text.endswith("\n"):
        raise ReleaseError("checksum manifest must end with a newline")
    lines = text.splitlines()
    if not lines:
        raise ReleaseError("checksum manifest is empty")
    seen: set[str] = set()
    entries = []
    for line_number, line in enumerate(lines, 1):
        match = CHECKSUM_RE.fullmatch(line)
        if match is None:
            raise ReleaseError(f"invalid checksum manifest line {line_number}")
        digest, filename = match.groups()
        if filename in seen:
            raise ReleaseError(f"duplicate checksum entry: {filename}")
        seen.add(filename)
        entries.append((digest, checksum_file.parent / filename))
    return entries


def checksum_entries(checksum_file: Path) -> list[tuple[str, Path]]:
    payload = bounded_trust_file_bytes(
        checksum_file, "checksum manifest", MAX_CHECKSUM_MANIFEST_BYTES
    )
    return checksum_entries_from_bytes(checksum_file, payload)


def verify_checksums(args: argparse.Namespace) -> None:
    verify_checksum_entries(checksum_entries(absolute_path(args.checksum_file)))


def verify_checksum_entries(entries: list[tuple[str, Path]]) -> None:
    for expected, archive in entries:
        with snapshot_archive(archive, expected):
            pass
        print(f"{archive.name}: OK")


def snapshot_archive(archive_path: Path, expected_sha256: str | None = None) -> BinaryIO:
    source, before = opened_regular_file(
        archive_path, "release archive", MAX_ARCHIVE_BYTES
    )
    snapshot = tempfile.TemporaryFile(prefix="wikisync-verified-archive-", mode="w+b")
    digest = hashlib.sha256()
    copied = 0
    try:
        with source:
            while chunk := source.read(1024 * 1024):
                copied += len(chunk)
                if copied > MAX_ARCHIVE_BYTES:
                    raise ReleaseError(
                        f"release archive exceeds the {MAX_ARCHIVE_BYTES}-byte limit"
                    )
                snapshot.write(chunk)
                digest.update(chunk)
            after = os.fstat(source.fileno())
        if (
            before.st_dev != after.st_dev
            or before.st_ino != after.st_ino
            or before.st_size != after.st_size
            or before.st_mtime_ns != after.st_mtime_ns
            or copied != before.st_size
        ):
            raise ReleaseError(f"release archive changed while being snapshotted: {archive_path}")
        actual = digest.hexdigest()
        if expected_sha256 is not None and actual != expected_sha256:
            raise ReleaseError(
                f"checksum mismatch for {archive_path.name}: "
                f"expected {expected_sha256}, got {actual}"
            )
        snapshot.flush()
        snapshot.seek(0)
        return snapshot
    except Exception:
        snapshot.close()
        raise


def verify_archive_file(
    archive_source: BinaryIO,
    archive_name: str,
    *,
    required_relative_files: set[str] | None = None,
) -> None:
    seen: set[str] = set()
    roots: set[str] = set()
    binaries: set[str] = set()
    mtimes: set[int] = set()
    member_count = 0
    total_size = 0
    archive_source.seek(0)
    with tarfile.open(fileobj=archive_source, mode="r:gz") as archive:
        for member in archive:
            member_count += 1
            if member_count > MAX_ARCHIVE_MEMBERS:
                raise ReleaseError(f"archive exceeds the {MAX_ARCHIVE_MEMBERS}-member limit")
            path = PurePosixPath(member.name)
            if (
                path.is_absolute()
                or ".." in path.parts
                or not path.parts
                or str(path) != member.name
                or "\\" in member.name
            ):
                raise ReleaseError(f"unsafe archive path: {member.name}")
            if member.name in seen:
                raise ReleaseError(f"duplicate archive path: {member.name}")
            seen.add(member.name)
            roots.add(path.parts[0])
            mtimes.add(member.mtime)
            if not (member.isdir() or member.isfile()):
                raise ReleaseError(f"links and special files are forbidden: {member.name}")
            if member.size > MAX_MEMBER_BYTES:
                raise ReleaseError(f"archive member exceeds the {MAX_MEMBER_BYTES}-byte limit: {member.name}")
            total_size += member.size
            if total_size > MAX_TOTAL_UNCOMPRESSED_BYTES:
                raise ReleaseError(
                    f"archive exceeds the {MAX_TOTAL_UNCOMPRESSED_BYTES}-byte uncompressed limit"
                )
            expected_mode = 0o755 if member.isdir() or (len(path.parts) == 3 and path.parts[1] == "bin") else 0o644
            if member.mode != expected_mode:
                raise ReleaseError(f"unexpected mode {member.mode:o} for {member.name}")
            if member.uid != 0 or member.gid != 0:
                raise ReleaseError(f"unexpected ownership metadata for {member.name}")
            if len(path.parts) == 3 and path.parts[1] == "bin":
                binaries.add(path.parts[2])
    if len(roots) != 1:
        raise ReleaseError("archive must have exactly one top-level directory")
    if binaries != set(BINARIES):
        raise ReleaseError(f"archive binaries are {sorted(binaries)}, expected {list(BINARIES)}")
    if len(mtimes) != 1:
        raise ReleaseError("archive contains non-deterministic timestamps")
    root = next(iter(roots))
    if archive_name != f"{root}.tar.gz":
        raise ReleaseError("archive filename must match its top-level directory")
    required_relative_files = (
        CURRENT_REQUIRED_RELATIVE_FILES
        if required_relative_files is None
        else required_relative_files
    )
    required = {f"{root}/{path}" for path in required_relative_files}
    missing = sorted(required - seen)
    if missing:
        raise ReleaseError(f"archive is missing required files: {', '.join(missing)}")
    print(f"{archive_name}: layout OK")


def verify_archive_path(archive_path: Path) -> None:
    with snapshot_archive(archive_path) as snapshot:
        verify_archive_file(snapshot, archive_path.name)


def verify_signed_archive(expected_sha256: str, archive_path: Path) -> None:
    with snapshot_archive(archive_path, expected_sha256) as snapshot:
        print(f"{archive_path.name}: OK")
        verify_archive_file(snapshot, archive_path.name)


def verify_archive(args: argparse.Namespace) -> None:
    verify_archive_path(absolute_path(args.archive))


def private_key(path: Path) -> Path:
    metadata = regular_file(path, "private signing key")
    if metadata.st_mode & 0o077:
        raise ReleaseError(f"private signing key permissions must be 0600 or stricter: {path}")
    if hasattr(os, "getuid") and metadata.st_uid != os.getuid():
        raise ReleaseError(f"private signing key must be owned by the current user: {path}")
    return path


def ssh_keygen() -> str:
    executable = shutil.which("ssh-keygen")
    if executable is None:
        raise ReleaseError("ssh-keygen with SSH signature support is required")
    return executable


def require_ed25519_private_key(path: Path) -> None:
    result = subprocess.run(
        [ssh_keygen(), "-y", "-f", str(path)],
        stdin=subprocess.DEVNULL,
        capture_output=True,
        check=False,
    )
    if result.returncode != 0:
        detail = result.stderr.decode(errors="replace").strip()
        raise ReleaseError(f"cannot inspect private signing key: {detail}")
    if not result.stdout.startswith(b"ssh-ed25519 "):
        raise ReleaseError("private signing key must be an Ed25519 SSH key")


def ssh_string(payload: bytes, offset: int, description: str) -> tuple[bytes, int]:
    if offset + 4 > len(payload):
        raise ReleaseError(f"malformed detached signature: missing {description} length")
    size = int.from_bytes(payload[offset : offset + 4], "big")
    offset += 4
    if size > len(payload) - offset:
        raise ReleaseError(f"malformed detached signature: truncated {description}")
    return payload[offset : offset + size], offset + size


def require_ed25519_signature(payload: bytes) -> None:
    try:
        lines = payload.decode("ascii").splitlines()
    except UnicodeDecodeError as error:
        raise ReleaseError("detached signature is not ASCII armored") from error
    if (
        len(lines) < 3
        or lines[0] != "-----BEGIN SSH SIGNATURE-----"
        or lines[-1] != "-----END SSH SIGNATURE-----"
    ):
        raise ReleaseError("detached signature is not an OpenSSH SSHSIG document")
    try:
        decoded = base64.b64decode("".join(lines[1:-1]), validate=True)
    except (ValueError, binascii.Error) as error:
        raise ReleaseError("detached signature has invalid base64") from error
    if not decoded.startswith(b"SSHSIG") or len(decoded) < 10:
        raise ReleaseError("detached signature has an invalid SSHSIG header")
    version = int.from_bytes(decoded[6:10], "big")
    if version != 1:
        raise ReleaseError(f"unsupported SSHSIG version: {version}")
    offset = 10
    public_key, offset = ssh_string(decoded, offset, "public key")
    namespace, offset = ssh_string(decoded, offset, "namespace")
    _, offset = ssh_string(decoded, offset, "reserved field")
    _, offset = ssh_string(decoded, offset, "hash algorithm")
    signature, offset = ssh_string(decoded, offset, "signature")
    if offset != len(decoded):
        raise ReleaseError("malformed detached signature: trailing data")
    public_algorithm, _ = ssh_string(public_key, 0, "public-key algorithm")
    signature_algorithm, _ = ssh_string(signature, 0, "signature algorithm")
    if namespace != SIGNATURE_NAMESPACE.encode():
        raise ReleaseError(f"detached signature namespace must be {SIGNATURE_NAMESPACE}")
    if public_algorithm != b"ssh-ed25519" or signature_algorithm != b"ssh-ed25519":
        raise ReleaseError("detached signature must use an Ed25519 SSH key")


def sign_checksums(args: argparse.Namespace) -> None:
    checksum_file = absolute_path(args.checksum_file)
    checksum_entries(checksum_file)
    key = private_key(absolute_path(args.private_key))
    require_ed25519_private_key(key)
    signature = checksum_file.with_name(f"{checksum_file.name}.sig")
    if signature.exists() and not args.force:
        raise ReleaseError(f"signature already exists (use --force to replace it): {signature}")
    with tempfile.TemporaryDirectory(prefix="wikisync-sign-") as temporary:
        staged = Path(temporary) / checksum_file.name
        shutil.copyfile(checksum_file, staged)
        command = [ssh_keygen(), "-Y", "sign", "-f", str(key), "-n", SIGNATURE_NAMESPACE, str(staged)]
        result = subprocess.run(command, stdin=subprocess.DEVNULL, capture_output=True, text=True, check=False)
        if result.returncode != 0:
            raise ReleaseError(f"ssh-keygen signing failed: {result.stderr.strip()}")
        staged_signature = staged.with_name(f"{staged.name}.sig")
        regular_file(staged_signature, "detached signature")
        os.replace(staged_signature, signature)
    print(signature)


def verify_signature_bytes(
    checksum_payload: bytes,
    signature: Path,
    allowed_signers: Path,
    signer_identity: str,
) -> None:
    signature_payload = bounded_trust_file_bytes(
        signature, "detached signature", MAX_SIGNATURE_BYTES
    )
    require_ed25519_signature(signature_payload)
    bounded_trust_file_bytes(
        allowed_signers, "allowed signers file", MAX_ALLOWED_SIGNERS_BYTES
    )
    command = [
        ssh_keygen(), "-Y", "verify", "-f", str(allowed_signers), "-I", signer_identity,
        "-n", SIGNATURE_NAMESPACE, "-s", str(signature),
    ]
    result = subprocess.run(command, input=checksum_payload, capture_output=True, check=False)
    if result.returncode != 0:
        detail = result.stderr.decode(errors="replace").strip()
        raise ReleaseError(f"detached signature verification failed: {detail}")
    print(f"{signature.name}: signature OK for {signer_identity}")


def verify_signature(args: argparse.Namespace) -> None:
    checksum_file = absolute_path(args.checksum_file)
    checksum_payload = bounded_trust_file_bytes(
        checksum_file, "checksum manifest", MAX_CHECKSUM_MANIFEST_BYTES
    )
    checksum_entries_from_bytes(checksum_file, checksum_payload)
    verify_signature_bytes(
        checksum_payload,
        absolute_path(args.signature),
        absolute_path(args.allowed_signers),
        args.signer_identity,
    )


def verify_release(args: argparse.Namespace) -> None:
    checksum_file = absolute_path(args.checksum_file)
    checksum_payload = bounded_trust_file_bytes(
        checksum_file, "checksum manifest", MAX_CHECKSUM_MANIFEST_BYTES
    )

    # Authentication deliberately precedes every interpretation of attacker-controlled
    # archive bytes. The same in-memory manifest bytes are then parsed and verified.
    verify_signature_bytes(
        checksum_payload,
        absolute_path(args.signature),
        absolute_path(args.allowed_signers),
        args.signer_identity,
    )
    entries = checksum_entries_from_bytes(checksum_file, checksum_payload)
    signed_names = {archive.name for _, archive in entries}
    present_names = {
        archive.name for archive in checksum_file.parent.glob("wikisync-*.tar.gz")
    }
    unsigned_names = sorted(present_names - signed_names)
    if unsigned_names:
        raise ReleaseError(
            "release directory contains archives not covered by the signed manifest: "
            + ", ".join(unsigned_names)
        )
    missing_names = sorted(signed_names - present_names)
    if missing_names:
        raise ReleaseError(
            "signed manifest names archives missing from the release directory: "
            + ", ".join(missing_names)
        )
    for expected, archive in entries:
        verify_signed_archive(expected, archive)
    print(f"release set: {len(entries)} signed archive(s) verified")


def parser() -> argparse.ArgumentParser:
    result = argparse.ArgumentParser(description=__doc__)
    subcommands = result.add_subparsers(dest="command", required=True)

    package = subcommands.add_parser("package", help="build one deterministic tar.gz archive")
    package.add_argument("--input-dir", type=Path, required=True)
    package.add_argument("--output-dir", type=Path, required=True)
    package.add_argument("--repo-root", type=Path, default=Path(__file__).resolve().parents[2])
    package.add_argument("--version", required=True)
    package.add_argument("--target-os", choices=("macos", "linux"), required=True)
    package.add_argument("--target-arch", required=True)
    package.add_argument("--source-date-epoch", type=int)
    package.set_defaults(function=build_archive)

    linux_binaries = subcommands.add_parser(
        "verify-linux-binaries",
        help="verify that the release inputs are executable 64-bit Linux ELF binaries",
    )
    linux_binaries.add_argument("--input-dir", type=Path, required=True)
    linux_binaries.add_argument("--target-arch", required=True)
    linux_binaries.set_defaults(function=verify_linux_binaries)

    systemd_units = subcommands.add_parser(
        "verify-systemd-units",
        help="render and natively validate the Linux systemd user units",
    )
    systemd_units.add_argument(
        "--repo-root", type=Path, default=Path(__file__).resolve().parents[2]
    )
    systemd_units.set_defaults(function=verify_systemd_units)

    macos_plan = subcommands.add_parser(
        "macos-signing-plan",
        help="validate Mach-O inputs and write a deterministic credential-free signing plan",
    )
    macos_plan.add_argument("--input-dir", type=Path, required=True)
    macos_plan.add_argument("--output-file", type=Path, required=True)
    macos_plan.add_argument("--version", required=True)
    macos_plan.add_argument("--target-arch", required=True)
    macos_plan.add_argument("--signing-identity", required=True)
    macos_plan.add_argument("--team-id", required=True)
    macos_plan.add_argument("--certificate-sha1", required=True)
    macos_plan.add_argument("--source-date-epoch", type=int)
    macos_plan.add_argument("--credential-free-dry-run", action="store_true")
    macos_plan.set_defaults(function=macos_signing_plan)

    macos_verify = subcommands.add_parser(
        "verify-macos-signatures",
        help="verify the exact Developer ID identity and signature policy of Mach-O inputs",
    )
    macos_verify.add_argument("--input-dir", type=Path, required=True)
    macos_verify.add_argument("--target-arch", required=True)
    macos_verify.add_argument("--signing-identity", required=True)
    macos_verify.add_argument("--team-id", required=True)
    macos_verify.set_defaults(function=verify_macos_signatures)

    macos_archive = subcommands.add_parser(
        "verify-macos-release-archive",
        help="verify Developer ID signatures and their exact bytes in a release archive",
    )
    macos_archive.add_argument("--input-dir", type=Path, required=True)
    macos_archive.add_argument("--archive", type=Path, required=True)
    macos_archive.add_argument("--target-arch", required=True)
    macos_archive.add_argument("--signing-identity", required=True)
    macos_archive.add_argument("--team-id", required=True)
    macos_archive.set_defaults(function=verify_macos_release_archive)

    notarization_receipt = subcommands.add_parser(
        "validate-notarization-receipt",
        help="validate an Apple notarytool JSON receipt against an expected submission ID",
    )
    notarization_receipt.add_argument("--receipt", type=Path, required=True)
    notarization_receipt.add_argument("--submission-id", required=True)
    notarization_receipt.set_defaults(function=validate_notarization_receipt)

    checksums = subcommands.add_parser("checksums", help="write SHA256SUMS for release archives")
    checksums.add_argument("--output-dir", type=Path, required=True)
    checksums.set_defaults(function=write_checksums)

    verify_sums = subcommands.add_parser("verify-checksums", help="verify every SHA256SUMS entry")
    verify_sums.add_argument("--checksum-file", type=Path, required=True)
    verify_sums.set_defaults(function=verify_checksums)

    inspect = subcommands.add_parser("verify-archive", help="validate a release archive's safe layout")
    inspect.add_argument("--archive", type=Path, required=True)
    inspect.set_defaults(function=verify_archive)

    sign = subcommands.add_parser("sign-checksums", help="SSH-sign SHA256SUMS with an explicit private key")
    sign.add_argument("--checksum-file", type=Path, required=True)
    sign.add_argument("--private-key", type=Path, required=True)
    sign.add_argument("--force", action="store_true")
    sign.set_defaults(function=sign_checksums)

    verify_sig = subcommands.add_parser("verify-signature", help="verify SHA256SUMS.sig against allowed signers")
    verify_sig.add_argument("--checksum-file", type=Path, required=True)
    verify_sig.add_argument("--signature", type=Path, required=True)
    verify_sig.add_argument("--allowed-signers", type=Path, required=True)
    verify_sig.add_argument("--signer-identity", required=True)
    verify_sig.set_defaults(function=verify_signature)

    verify_set = subcommands.add_parser(
        "verify-release", help="authenticate and validate an exact signed release archive set"
    )
    verify_set.add_argument("--checksum-file", type=Path, required=True)
    verify_set.add_argument("--signature", type=Path, required=True)
    verify_set.add_argument("--allowed-signers", type=Path, required=True)
    verify_set.add_argument("--signer-identity", required=True)
    verify_set.set_defaults(function=verify_release)
    return result


def main() -> int:
    try:
        args = parser().parse_args()
        args.function(args)
    except (ReleaseError, OSError, tarfile.TarError) as error:
        print(f"release.py: error: {error}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
