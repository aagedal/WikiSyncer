#!/usr/bin/env python3
"""Build and authenticate deterministic WikiSyncer release-candidate archives."""

from __future__ import annotations

import argparse
import gzip
import hashlib
import io
import os
from pathlib import Path, PurePosixPath
import re
import shutil
import stat
import subprocess
import sys
import tarfile
import tempfile


BINARIES = ("wikisync", "wikisyncd", "wikisync-gui")
VERSION_RE = re.compile(r"[0-9A-Za-z][0-9A-Za-z.+_-]*\Z")
CHECKSUM_RE = re.compile(r"([0-9a-f]{64})  ([0-9A-Za-z][0-9A-Za-z.+_-]*\.tar\.gz)\Z")
SIGNATURE_NAMESPACE = "wikisync-release"
MAX_ARCHIVE_BYTES = 4 * 1024 * 1024 * 1024
MAX_MEMBER_BYTES = 1024 * 1024 * 1024
MAX_TOTAL_UNCOMPRESSED_BYTES = 4 * 1024 * 1024 * 1024
MAX_ARCHIVE_MEMBERS = 128


class ReleaseError(Exception):
    pass


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


def checked_token(value: str, description: str) -> str:
    if not VERSION_RE.fullmatch(value):
        raise ReleaseError(f"invalid {description}: {value!r}")
    return value


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
    ]
    operations = repo_root / "docs" / "operations"
    for path in sorted(operations.glob("*.md")):
        sources.append((f"docs/operations/{path.name}", path))
    if target_os == "macos":
        sources.append(
            (
                "service/org.wikisync.WikiSyncer.plist.in",
                repo_root / "packaging" / "launchd" / "org.wikisync.WikiSyncer.plist.in",
            )
        )
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


def checksum_entries(checksum_file: Path) -> list[tuple[str, Path]]:
    regular_file(checksum_file, "checksum manifest")
    try:
        lines = checksum_file.read_text(encoding="ascii").splitlines()
    except (UnicodeDecodeError, OSError) as error:
        raise ReleaseError(f"cannot read checksum manifest: {error}") from error
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


def verify_checksums(args: argparse.Namespace) -> None:
    for expected, archive in checksum_entries(args.checksum_file.resolve()):
        metadata = regular_file(archive, "checksummed archive")
        if metadata.st_size > MAX_ARCHIVE_BYTES:
            raise ReleaseError(f"checksummed archive exceeds the {MAX_ARCHIVE_BYTES}-byte limit: {archive}")
        actual = sha256(archive)
        if actual != expected:
            raise ReleaseError(f"checksum mismatch for {archive.name}: expected {expected}, got {actual}")
        print(f"{archive.name}: OK")


def verify_archive(args: argparse.Namespace) -> None:
    archive_path = args.archive.resolve()
    archive_metadata = regular_file(archive_path, "release archive")
    if archive_metadata.st_size > MAX_ARCHIVE_BYTES:
        raise ReleaseError(f"release archive exceeds the {MAX_ARCHIVE_BYTES}-byte limit")
    seen: set[str] = set()
    roots: set[str] = set()
    binaries: set[str] = set()
    mtimes: set[int] = set()
    member_count = 0
    total_size = 0
    with tarfile.open(archive_path, mode="r:gz") as archive:
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
    if archive_path.name != f"{root}.tar.gz":
        raise ReleaseError("archive filename must match its top-level directory")
    required = {f"{root}/RELEASE.txt", f"{root}/LICENSE", f"{root}/README.md"}
    missing = sorted(required - seen)
    if missing:
        raise ReleaseError(f"archive is missing required files: {', '.join(missing)}")
    print(f"{archive_path.name}: layout OK")


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


def sign_checksums(args: argparse.Namespace) -> None:
    checksum_file = args.checksum_file.resolve()
    checksum_entries(checksum_file)
    key = private_key(args.private_key.resolve())
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


def verify_signature(args: argparse.Namespace) -> None:
    checksum_file = args.checksum_file.resolve()
    checksum_entries(checksum_file)
    signature = args.signature.resolve()
    allowed_signers = args.allowed_signers.resolve()
    regular_file(signature, "detached signature")
    regular_file(allowed_signers, "allowed signers file")
    command = [
        ssh_keygen(), "-Y", "verify", "-f", str(allowed_signers), "-I", args.signer_identity,
        "-n", SIGNATURE_NAMESPACE, "-s", str(signature),
    ]
    result = subprocess.run(command, input=checksum_file.read_bytes(), capture_output=True, check=False)
    if result.returncode != 0:
        detail = result.stderr.decode(errors="replace").strip()
        raise ReleaseError(f"detached signature verification failed: {detail}")
    print(f"{signature.name}: signature OK for {args.signer_identity}")


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
