#!/usr/bin/env python3
"""Assess a native WikiSyncer archive without root or persistent installation."""

from __future__ import annotations

import argparse
import contextlib
import hashlib
import importlib.util
import io
import json
import os
from pathlib import Path, PurePosixPath
import platform
import plistlib
import re
import selectors
import shutil
import signal
import stat
import subprocess
import sys
import tarfile
import tempfile
import threading
import time
from typing import BinaryIO


SCRIPT_DIRECTORY = Path(__file__).resolve().parent
RELEASE_PATH = SCRIPT_DIRECTORY / "release.py"
RELEASE_SPEC = importlib.util.spec_from_file_location("wikisync_release", RELEASE_PATH)
if RELEASE_SPEC is None or RELEASE_SPEC.loader is None:  # pragma: no cover - installation error
    raise RuntimeError(f"cannot load release helpers from {RELEASE_PATH}")
RELEASE = importlib.util.module_from_spec(RELEASE_SPEC)
RELEASE_SPEC.loader.exec_module(RELEASE)

MAX_COMMAND_OUTPUT_BYTES = 1024 * 1024
MAX_REPORT_BYTES = 1024 * 1024
TOKEN_RE = re.compile(rb"@[A-Z][A-Z_]*@")
LEGACY_REQUIRED_RELATIVE_FILES = {
    "RELEASE.txt",
    "LICENSE",
    "README.md",
    "packaging/README.md",
}


class AssessmentError(Exception):
    pass


def native_target() -> tuple[str, str]:
    operating_system = {"Darwin": "macos", "Linux": "linux"}.get(platform.system())
    architecture = {"arm64": "aarch64", "aarch64": "aarch64", "x86_64": "x86_64"}.get(
        platform.machine().lower()
    )
    if operating_system is None or architecture is None:
        raise AssessmentError(
            f"unsupported assessment host: {platform.system()} {platform.machine()}"
        )
    return operating_system, architecture


def digest_file(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as source:
        while chunk := source.read(1024 * 1024):
            digest.update(chunk)
    return digest.hexdigest()


def archive_metadata(snapshot: BinaryIO, archive_name: str) -> dict[str, str]:
    snapshot.seek(0)
    with tarfile.open(fileobj=snapshot, mode="r:gz") as archive:
        release_members = []
        mtimes: set[int] = set()
        for member_count, member in enumerate(archive, 1):
            if member_count > RELEASE.MAX_ARCHIVE_MEMBERS:
                raise AssessmentError("candidate metadata exceeds the archive member bound")
            mtimes.add(member.mtime)
            path = PurePosixPath(member.name)
            if path.name == "RELEASE.txt" and len(path.parts) == 2:
                if not member.isfile() or member.size > RELEASE.MAX_MEMBER_BYTES:
                    raise AssessmentError(
                        "candidate RELEASE.txt is not a bounded regular file"
                    )
                release_members.append(member)
        if len(release_members) != 1:
            raise AssessmentError("candidate archive must contain exactly one RELEASE.txt")
        extracted = archive.extractfile(release_members[0])
        if extracted is None:
            raise AssessmentError("cannot read candidate RELEASE.txt")
        payload = extracted.read(RELEASE.MAX_MEMBER_BYTES + 1)
    if len(payload) > RELEASE.MAX_MEMBER_BYTES:
        raise AssessmentError("candidate RELEASE.txt exceeds its size bound")
    try:
        lines = payload.decode("utf-8").splitlines()
    except UnicodeDecodeError as error:
        raise AssessmentError("candidate RELEASE.txt is not UTF-8") from error
    metadata: dict[str, str] = {}
    for line in lines:
        if ": " in line:
            key, value = line.split(": ", 1)
            if key in metadata:
                raise AssessmentError(f"duplicate RELEASE.txt field: {key}")
            metadata[key] = value
    required = {"version", "target-os", "target-arch", "source-date-epoch"}
    if set(metadata) != required:
        raise AssessmentError(
            f"RELEASE.txt fields are {sorted(metadata)}, expected {sorted(required)}"
        )
    RELEASE.checked_token(metadata["version"], "release version")
    if metadata["target-os"] not in {"linux", "macos"}:
        raise AssessmentError("RELEASE.txt has an unsupported target OS")
    if metadata["target-arch"] not in {"aarch64", "x86_64"}:
        raise AssessmentError("RELEASE.txt has an unsupported target architecture")
    try:
        source_date_epoch = int(metadata["source-date-epoch"])
    except ValueError as error:
        raise AssessmentError("RELEASE.txt source-date-epoch is not an integer") from error
    if source_date_epoch < 0 or str(source_date_epoch) != metadata["source-date-epoch"]:
        raise AssessmentError("RELEASE.txt source-date-epoch is not canonical")
    if mtimes != {source_date_epoch}:
        raise AssessmentError("RELEASE.txt source-date-epoch does not match archive timestamps")
    expected_root = (
        f"wikisync-{metadata['version']}-{metadata['target-os']}-{metadata['target-arch']}"
    )
    if archive_name != f"{expected_root}.tar.gz":
        raise AssessmentError("RELEASE.txt metadata does not match the archive filename")
    return metadata


def install_snapshot(
    snapshot: BinaryIO, destination: Path, *, previous_layout: bool
) -> tuple[Path, dict[str, str]]:
    """Validate and manually extract one immutable snapshot into a private directory."""
    archive_name = destination.name + ".tar.gz"
    if previous_layout:
        with contextlib.redirect_stdout(io.StringIO()):
            RELEASE.verify_archive_file(
                snapshot,
                archive_name,
                required_relative_files=LEGACY_REQUIRED_RELATIVE_FILES,
            )
        metadata = archive_metadata(snapshot, archive_name)
        service_baseline = (
            "service/org.wikisync.WikiSyncer.plist.in"
            if metadata["target-os"] == "macos"
            else "service/wikisyncd.service.in"
        )
        with contextlib.redirect_stdout(io.StringIO()):
            RELEASE.verify_archive_file(
                snapshot,
                archive_name,
                required_relative_files=LEGACY_REQUIRED_RELATIVE_FILES
                | {service_baseline},
            )
    else:
        with contextlib.redirect_stdout(io.StringIO()):
            RELEASE.verify_archive_file(snapshot, archive_name)
        metadata = archive_metadata(snapshot, archive_name)
    destination.mkdir(mode=0o700)
    snapshot.seek(0)
    root: Path | None = None
    with tarfile.open(fileobj=snapshot, mode="r:gz") as archive:
        for member in archive:
            relative = PurePosixPath(member.name)
            target = destination.joinpath(*relative.parts)
            if root is None:
                root = destination / relative.parts[0]
            if member.isdir():
                target.mkdir(mode=0o755, parents=True, exist_ok=True)
                target.chmod(0o755)
                continue
            target.parent.mkdir(mode=0o755, parents=True, exist_ok=True)
            extracted = archive.extractfile(member)
            if extracted is None:
                raise AssessmentError(f"cannot read archive member: {member.name}")
            flags = os.O_WRONLY | os.O_CREAT | os.O_EXCL | getattr(os, "O_NOFOLLOW", 0)
            descriptor = os.open(target, flags, member.mode)
            written = 0
            try:
                with os.fdopen(descriptor, "wb") as output:
                    while chunk := extracted.read(1024 * 1024):
                        written += len(chunk)
                        if written > member.size:
                            raise AssessmentError(
                                f"archive member grew while extracting: {member.name}"
                            )
                        output.write(chunk)
                if written != member.size:
                    raise AssessmentError(f"archive member is truncated: {member.name}")
                target.chmod(member.mode)
            except Exception:
                target.unlink(missing_ok=True)
                raise
    if root is None or not root.is_dir():
        raise AssessmentError("candidate archive has no installation root")
    return root, metadata


def install_archive(
    archive_path: Path, install_parent: Path, *, previous_layout: bool = False
) -> tuple[Path, dict[str, str], str]:
    archive_path = Path(os.path.abspath(archive_path))
    archive_digest = digest_file(archive_path)
    with RELEASE.snapshot_archive(archive_path, archive_digest) as snapshot:
        expected_name = archive_path.name.removesuffix(".tar.gz")
        destination = install_parent / expected_name
        root, metadata = install_snapshot(
            snapshot, destination, previous_layout=previous_layout
        )
    return root, metadata, archive_digest


def checked_native(metadata: dict[str, str], description: str) -> None:
    expected_os, expected_arch = native_target()
    observed = (metadata["target-os"], metadata["target-arch"])
    if observed != (expected_os, expected_arch):
        raise AssessmentError(
            f"{description} targets {observed[0]}/{observed[1]}, "
            f"but this host is {expected_os}/{expected_arch}"
        )


def command_environment(home: Path) -> dict[str, str]:
    environment = {
        "HOME": str(home),
        "LANG": "C",
        "LC_ALL": "C",
        "PATH": os.environ.get("PATH", "/usr/bin:/bin"),
        "TMPDIR": str(home / "tmp"),
        "XDG_CACHE_HOME": str(home / ".cache"),
        "XDG_CONFIG_HOME": str(home / ".config"),
        "XDG_DATA_HOME": str(home / ".local" / "share"),
    }
    for path in (
        home / "tmp",
        home / ".cache",
        home / ".config",
        home / ".local",
        home / ".local" / "share",
    ):
        path.mkdir(mode=0o700, parents=True, exist_ok=True)
        path.chmod(0o700)
    return environment


def run_bounded(
    command: list[str], environment: dict[str, str], cwd: Path, timeout: int
) -> tuple[str, str]:
    process = subprocess.Popen(
        command,
        cwd=cwd,
        env=environment,
        stdin=subprocess.DEVNULL,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        start_new_session=True,
    )
    assert process.stdout is not None and process.stderr is not None
    streams = {process.stdout: bytearray(), process.stderr: bytearray()}
    selector = selectors.DefaultSelector()
    for stream in streams:
        os.set_blocking(stream.fileno(), False)
        selector.register(stream, selectors.EVENT_READ)
    deadline = time.monotonic() + timeout
    try:
        while selector.get_map():
            remaining = deadline - time.monotonic()
            if remaining <= 0:
                os.killpg(process.pid, signal.SIGKILL)
                process.wait()
                raise AssessmentError(
                    f"command timed out after {timeout}s: {Path(command[0]).name}"
                )
            for key, _ in selector.select(min(remaining, 0.1)):
                stream = key.fileobj
                chunk = os.read(stream.fileno(), 64 * 1024)
                if not chunk:
                    selector.unregister(stream)
                    continue
                output = streams[stream]
                output.extend(chunk)
                if len(output) > MAX_COMMAND_OUTPUT_BYTES:
                    os.killpg(process.pid, signal.SIGKILL)
                    process.wait()
                    raise AssessmentError(
                        f"command output exceeds {MAX_COMMAND_OUTPUT_BYTES} bytes"
                    )
        return_code = process.wait()
    finally:
        selector.close()
        for stream in streams:
            stream.close()
        if process.poll() is None:
            os.killpg(process.pid, signal.SIGKILL)
            process.wait()
    out = streams[process.stdout].decode("utf-8", errors="replace")
    err = streams[process.stderr].decode("utf-8", errors="replace")
    if return_code != 0:
        detail = err.strip().splitlines()[-1] if err.strip() else "no diagnostic"
        raise AssessmentError(
            f"{Path(command[0]).name} exited {return_code}: {detail[:500]}"
        )
    return out, err


def cli_json(
    binary: Path,
    library: Path,
    command: list[str],
    environment: dict[str, str],
    cwd: Path,
    timeout: int,
) -> object:
    output, _ = run_bounded(
        [str(binary), "--library", str(library), *command, "--json"],
        environment,
        cwd,
        timeout,
    )
    try:
        return json.loads(output)
    except json.JSONDecodeError as error:
        raise AssessmentError(
            f"{binary.name} returned invalid JSON for {' '.join(command)}"
        ) from error


def render_services(
    install: Path, library: Path, home: Path, target_os: str
) -> dict[str, object]:
    service_source = install / "service"
    service_install = home / "service"
    service_install.mkdir(mode=0o700)
    daemon = install / "bin" / "wikisyncd"
    documentation = install / "docs" / "operations"
    rendered: list[Path] = []
    if target_os == "linux":
        replacements = {
            b"@WIKISYNCD@": os.fsencode(daemon),
            b"@LIBRARY@": os.fsencode(library),
            b"@DOCUMENTATION_DIRECTORY@": os.fsencode(documentation),
        }
        for template in sorted(service_source.glob("*.in")):
            payload = template.read_bytes()
            for token, value in replacements.items():
                payload = payload.replace(token, value)
            if TOKEN_RE.search(payload):
                raise AssessmentError(f"unresolved service token in {template.name}")
            destination = service_install / template.name.removesuffix(".in")
            destination.write_bytes(payload)
            destination.chmod(0o600)
            rendered.append(destination)
        parser = shutil.which("systemd-analyze")
        native_parser = parser is not None
        if parser is not None:
            run_bounded(
                [parser, "--user", "verify", *(str(path) for path in rendered)],
                command_environment(home),
                home,
                15,
            )
    else:
        logs = home / "logs"
        logs.mkdir(mode=0o700)
        helper = service_install / "wikisync-log-maintenance.sh"
        helper.write_bytes((service_source / helper.name).read_bytes())
        helper.chmod(0o700)
        plist_path = service_install / "org.wikisync.WikiSyncer.plist"
        maintenance_path = service_install / "org.wikisync.WikiSyncer-log-maintenance.plist"
        newsyslog_path = service_install / "newsyslog.conf"
        replacements = {
            b"@WIKISYNCD@": os.fsencode(daemon),
            b"@LIBRARY@": os.fsencode(library),
            b"@LOG_DIRECTORY@": os.fsencode(logs),
            b"@LOG_MAINTENANCE_SCRIPT@": os.fsencode(helper),
            b"@NEWSYSLOG_CONFIG@": os.fsencode(newsyslog_path),
            b"@SERVICE_PLIST@": os.fsencode(plist_path),
            b"@UID@": str(os.getuid()).encode(),
            b"@GID@": str(os.getgid()).encode(),
        }
        destinations = {
            "org.wikisync.WikiSyncer.plist.in": plist_path,
            "org.wikisync.WikiSyncer-log-maintenance.plist.in": maintenance_path,
            "wikisync-newsyslog.conf.in": newsyslog_path,
        }
        for name, destination in destinations.items():
            payload = (service_source / name).read_bytes()
            for token, value in replacements.items():
                payload = payload.replace(token, value)
            if TOKEN_RE.search(payload):
                raise AssessmentError(f"unresolved service token in {name}")
            if destination.suffix == ".plist":
                plistlib.loads(payload)
            destination.write_bytes(payload)
            destination.chmod(0o600)
            rendered.append(destination)
        native_parser = True
    if not rendered or any(stat.S_IMODE(path.stat().st_mode) != 0o600 for path in rendered):
        raise AssessmentError("rendered service files were not privately installed")
    return {
        "installed_file_count": len(rendered) + (1 if target_os == "macos" else 0),
        "native_parser_available": native_parser,
        "private_modes": True,
        "tokens_resolved": True,
    }


def daemon_lifecycle(
    install: Path,
    library: Path,
    environment: dict[str, str],
    cwd: Path,
    timeout: int,
) -> None:
    daemon = install / "bin" / "wikisyncd"
    process = subprocess.Popen(
        [str(daemon), "--library", str(library), "run"],
        cwd=cwd,
        env=environment,
        stdin=subprocess.DEVNULL,
        stdout=subprocess.PIPE,
        stderr=subprocess.STDOUT,
        start_new_session=True,
    )
    assert process.stdout is not None
    daemon_output = bytearray()
    output_overflow = threading.Event()

    def collect_output() -> None:
        while chunk := process.stdout.read(64 * 1024):
            if len(daemon_output) + len(chunk) > MAX_COMMAND_OUTPUT_BYTES:
                output_overflow.set()
                if process.poll() is None:
                    os.killpg(process.pid, signal.SIGKILL)
                return
            daemon_output.extend(chunk)

    collector = threading.Thread(target=collect_output, daemon=True)
    collector.start()

    def daemon_diagnostic() -> str:
        text = daemon_output.decode("utf-8", errors="replace").strip()
        return text.splitlines()[-1][:500] if text else "no diagnostic"

    try:
        deadline = time.monotonic() + timeout
        while True:
            if process.poll() is not None:
                collector.join(timeout=1)
                if output_overflow.is_set():
                    raise AssessmentError(
                        f"daemon output exceeds {MAX_COMMAND_OUTPUT_BYTES} bytes"
                    )
                raise AssessmentError(
                    f"{daemon.name} exited before becoming healthy: {daemon_diagnostic()}"
                )
            try:
                run_bounded(
                    [str(daemon), "--library", str(library), "health"],
                    environment,
                    cwd,
                    2,
                )
                break
            except AssessmentError:
                if time.monotonic() >= deadline:
                    raise AssessmentError(
                        f"{daemon.name} did not become healthy within {timeout}s"
                    )
                time.sleep(0.05)
        run_bounded(
            [str(daemon), "--library", str(library), "status"],
            environment,
            cwd,
            timeout,
        )
        run_bounded(
            [str(daemon), "--library", str(library), "shutdown"],
            environment,
            cwd,
            timeout,
        )
        try:
            process.wait(timeout=timeout)
        except subprocess.TimeoutExpired as error:
            raise AssessmentError(f"{daemon.name} did not stop after shutdown") from error
        if process.returncode != 0:
            collector.join(timeout=1)
            raise AssessmentError(
                f"{daemon.name} exited {process.returncode} after shutdown: "
                f"{daemon_diagnostic()}"
            )
        collector.join(timeout=1)
        if collector.is_alive():
            raise AssessmentError("daemon output collector did not stop")
        if output_overflow.is_set():
            raise AssessmentError(f"daemon output exceeds {MAX_COMMAND_OUTPUT_BYTES} bytes")
    finally:
        if process.poll() is None:
            os.killpg(process.pid, signal.SIGKILL)
            process.wait()
        collector.join(timeout=1)
        process.stdout.close()


def library_snapshot(
    install: Path,
    library: Path,
    environment: dict[str, str],
    cwd: Path,
    timeout: int,
) -> dict[str, object]:
    cli = install / "bin" / "wikisync"
    # Status intentionally goes first: unlike the following read-only inspection
    # commands, it opens the library through the writable migration path.
    status = cli_json(cli, library, ["status"], environment, cwd, timeout)
    return {
        "collections": cli_json(
            cli,
            library,
            ["collection", "list", "--all"],
            environment,
            cwd,
            timeout,
        ),
        "sources": cli_json(cli, library, ["source", "list"], environment, cwd, timeout),
        "status": status,
    }


def initialize_and_check(
    install: Path,
    library: Path,
    environment: dict[str, str],
    cwd: Path,
    timeout: int,
) -> dict[str, object]:
    cli = install / "bin" / "wikisync"
    help_output, _ = run_bounded([str(cli), "--help"], environment, cwd, timeout)
    required_commands = (
        "wikisync --library <path> init",
        "wikisync --library <path> source list [--json]",
        "wikisync --library <path> collection list [--all] [--json]",
        "wikisync --library <path> status [--json]",
    )
    if any(command not in help_output for command in required_commands):
        raise AssessmentError(
            "candidate CLI predates the stable administration surface required "
            "for this install/upgrade rehearsal"
        )
    version, _ = run_bounded([str(cli), "--version"], environment, cwd, timeout)
    run_bounded(
        [str(cli), "--library", str(library), "init"], environment, cwd, timeout
    )
    if not (library / "library.sqlite3").is_file():
        raise AssessmentError("library initialization did not create library.sqlite3")
    snapshot = library_snapshot(install, library, environment, cwd, timeout)
    cli_json(cli, library, ["doctor"], environment, cwd, timeout)
    daemon_lifecycle(install, library, environment, cwd, timeout)
    return {"cli_version": version.strip(), "state": snapshot}


def assess(args: argparse.Namespace) -> dict[str, object]:
    archive = Path(os.path.abspath(args.archive))
    previous = (
        Path(os.path.abspath(args.previous_archive)) if args.previous_archive else None
    )
    # Keep the library root short enough for its platform Unix-socket leaf names.
    # The randomly named directory is private before any candidate process runs.
    with tempfile.TemporaryDirectory(prefix="wsia-", dir="/tmp") as temporary:
        root = Path(temporary)
        home = root / "home"
        home.mkdir(mode=0o700)
        environment = command_environment(home)
        installs = root / "installs"
        installs.mkdir(mode=0o700)
        current_installs = installs / "current"
        current_installs.mkdir(mode=0o700)
        current, metadata, archive_sha256 = install_archive(archive, current_installs)
        checked_native(metadata, "candidate archive")

        fresh_library = home / "fresh-library"
        fresh = initialize_and_check(current, fresh_library, environment, home, args.timeout)
        if fresh["cli_version"] != f"wikisync {metadata['version']}":
            raise AssessmentError("candidate CLI version does not match RELEASE.txt")
        services = render_services(current, fresh_library, home, metadata["target-os"])

        upgrade: dict[str, object] | None = None
        previous_summary: dict[str, str] | None = None
        if previous is not None:
            previous_installs = installs / "previous"
            previous_installs.mkdir(mode=0o700)
            old, old_metadata, old_sha256 = install_archive(
                previous, previous_installs, previous_layout=True
            )
            checked_native(old_metadata, "previous candidate archive")
            if (old_metadata["target-os"], old_metadata["target-arch"]) != (
                metadata["target-os"], metadata["target-arch"]
            ):
                raise AssessmentError(
                    "candidate and previous candidate target different platforms"
                )
            upgrade_library = home / "upgrade-library"
            old_result = initialize_and_check(
                old, upgrade_library, environment, home, args.timeout
            )
            if old_result["cli_version"] != f"wikisync {old_metadata['version']}":
                raise AssessmentError(
                    "previous candidate CLI version does not match RELEASE.txt"
                )
            before = old_result["state"]
            after = library_snapshot(
                current, upgrade_library, environment, home, args.timeout
            )
            if before != after:
                raise AssessmentError(
                    "stable empty-library state changed across candidate upgrade"
                )
            daemon_lifecycle(current, upgrade_library, environment, home, args.timeout)
            previous_summary = {
                "sha256": old_sha256,
                "target_arch": old_metadata["target-arch"],
                "target_os": old_metadata["target-os"],
                "version": old_metadata["version"],
                "layout_policy": "bounded-legacy-upgrade-input-v1",
            }
            upgrade = {
                "daemon_after_upgrade": True,
                "logical_state_preserved": True,
                "migration_opened_by_current_candidate": True,
            }

    result: dict[str, object] = {
        "assessment_schema_version": 1,
        "candidate": {
            "sha256": archive_sha256,
            "target_arch": metadata["target-arch"],
            "target_os": metadata["target-os"],
            "version": metadata["version"],
        },
        "clean_install": {
            "cli_help_and_version": True,
            "daemon_lifecycle": True,
            "fresh_library_initialized": True,
            "offline_command_set_exercised": True,
            "service_assets": services,
        },
        "evidence_scope": {
            "clean_system_certification": False,
            "current_host_only": True,
            "network_isolation_enforced": False,
            "rootless_archive_rehearsal": True,
            "service_manager_install_or_enable": False,
        },
        "previous_candidate": previous_summary,
        "result": "pass",
        "upgrade": upgrade,
    }
    if not isinstance(fresh["cli_version"], str):
        raise AssessmentError("candidate version output is invalid")
    return result


def write_report(path: Path | None, report: dict[str, object]) -> None:
    payload = (json.dumps(report, sort_keys=True, separators=(",", ":")) + "\n").encode()
    if len(payload) > MAX_REPORT_BYTES:
        raise AssessmentError("assessment report exceeds its size bound")
    if path is None:
        sys.stdout.buffer.write(payload)
        return
    destination = Path(os.path.abspath(path))
    if destination.exists():
        raise AssessmentError(f"refusing to overwrite assessment report: {destination}")
    destination.parent.mkdir(parents=True, exist_ok=True)
    descriptor = os.open(destination, os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o600)
    with os.fdopen(descriptor, "wb") as output:
        output.write(payload)
        output.flush()
        os.fsync(output.fileno())


def parser() -> argparse.ArgumentParser:
    result = argparse.ArgumentParser(description=__doc__)
    result.add_argument("--archive", type=Path, required=True)
    result.add_argument("--previous-archive", type=Path)
    result.add_argument("--output", type=Path)
    result.add_argument("--timeout", type=int, default=15, choices=range(1, 61), metavar="SECONDS")
    return result


def main() -> int:
    try:
        args = parser().parse_args()
        write_report(args.output, assess(args))
    except (AssessmentError, RELEASE.ReleaseError, OSError, tarfile.TarError) as error:
        print(f"assess_install.py: error: {error}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
