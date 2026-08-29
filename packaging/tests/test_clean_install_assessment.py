#!/usr/bin/env python3

from __future__ import annotations

import gzip
import json
import os
from pathlib import Path
import platform
import subprocess
import sys
import tempfile
import textwrap
import tarfile
import unittest


REPO_ROOT = Path(__file__).resolve().parents[2]
RELEASE = REPO_ROOT / "packaging" / "scripts" / "release.py"
ASSESS = REPO_ROOT / "packaging" / "scripts" / "assess_install.py"


FAKE_EXECUTABLE = textwrap.dedent(
    """\
    #!/usr/bin/env python3
    import json
    import os
    from pathlib import Path
    import signal
    import sys
    import time

    VERSION = "__VERSION__"
    name = Path(sys.argv[0]).name
    arguments = sys.argv[1:]
    if name == "wikisync":
        if arguments == ["--help"]:
            print("wikisync --library <path> init")
            print("wikisync --library <path> source list [--json]")
            print("wikisync --library <path> collection list [--all] [--json]")
            print("wikisync --library <path> status [--json]")
            raise SystemExit(0)
        if arguments == ["--version"]:
            print("wikisync " + VERSION)
            raise SystemExit(0)
        if len(arguments) < 3 or arguments[0] != "--library":
            raise SystemExit(2)
        library = Path(arguments[1])
        command = arguments[2:]
        if command == ["init"]:
            library.mkdir(mode=0o700)
            (library / "library.sqlite3").write_text(VERSION + "\\n")
            print("initialized")
            raise SystemExit(0)
        if not (library / "library.sqlite3").is_file():
            raise SystemExit(2)
        stored_version = (library / "library.sqlite3").read_text().strip()
        if stored_version != VERSION and command[:1] != ["status"]:
            print("read-only command reached before writable migration", file=sys.stderr)
            raise SystemExit(2)
        if command[:1] == ["status"]:
            (library / "library.sqlite3").write_text(VERSION + "\\n")
        if command[:2] == ["source", "list"]:
            value = {"schema_version": 1, "sources": []}
        elif command[:3] == ["collection", "list", "--all"]:
            value = {"schema_version": 1, "includes_tombstones": True, "collections": []}
        elif command[:1] == ["status"]:
            value = {"schema_version": 1, "runs": [], "jobs": []}
        elif command[:1] == ["doctor"]:
            value = {"bundle_schema_version": 1, "sections": []}
        else:
            raise SystemExit(2)
        print(json.dumps(value, sort_keys=True))
        raise SystemExit(0)

    if name == "wikisyncd":
        if len(arguments) != 3 or arguments[0] != "--library":
            raise SystemExit(2)
        library = Path(arguments[1])
        command = arguments[2]
        marker = library / ".fake-daemon"
        stop = library / ".fake-stop"
        if command == "run":
            stop.unlink(missing_ok=True)
            marker.write_text(str(os.getpid()))
            def requested_stop(_signal, _frame):
                stop.touch()
            signal.signal(signal.SIGTERM, requested_stop)
            signal.signal(signal.SIGINT, requested_stop)
            while not stop.exists():
                time.sleep(0.01)
            marker.unlink(missing_ok=True)
            stop.unlink(missing_ok=True)
            raise SystemExit(0)
        if command in {"health", "status"}:
            if not marker.is_file():
                raise SystemExit(2)
            print("healthy")
            raise SystemExit(0)
        if command == "shutdown":
            if not marker.is_file():
                raise SystemExit(2)
            stop.touch()
            print("stopping")
            raise SystemExit(0)
        raise SystemExit(2)

    if name == "wikisync-gui" and arguments in (["--help"], ["--version"]):
        print("fake GUI")
        raise SystemExit(0)
    raise SystemExit(2)
    """
).encode()


def native_target() -> tuple[str, str]:
    target_os = {"Darwin": "macos", "Linux": "linux"}[platform.system()]
    target_arch = {"arm64": "aarch64", "aarch64": "aarch64", "x86_64": "x86_64"}[
        platform.machine().lower()
    ]
    return target_os, target_arch


class CleanInstallAssessmentTests(unittest.TestCase):
    def setUp(self) -> None:
        self.temporary = tempfile.TemporaryDirectory(prefix="wikisync-install-assessment-test-")
        self.root = Path(self.temporary.name)
        self.binaries = self.root / "bin"
        self.binaries.mkdir()
        for name in ("wikisync", "wikisyncd", "wikisync-gui"):
            path = self.binaries / name
            path.write_bytes(FAKE_EXECUTABLE)
            path.chmod(0o755)

    def tearDown(self) -> None:
        self.temporary.cleanup()

    def run_command(
        self, command: list[object], *, succeed: bool = True
    ) -> subprocess.CompletedProcess[str]:
        completed = subprocess.run(
            [sys.executable, *map(str, command)],
            cwd=REPO_ROOT,
            text=True,
            capture_output=True,
            check=False,
        )
        if succeed and completed.returncode != 0:
            self.fail(f"command failed:\nstdout: {completed.stdout}\nstderr: {completed.stderr}")
        if not succeed and completed.returncode == 0:
            self.fail(f"command unexpectedly succeeded:\nstdout: {completed.stdout}")
        return completed

    def package(self, version: str, target_os: str | None = None) -> Path:
        native_os, native_arch = native_target()
        target_os = target_os or native_os
        executable = FAKE_EXECUTABLE.replace(b"__VERSION__", version.encode())
        for name in ("wikisync", "wikisyncd", "wikisync-gui"):
            path = self.binaries / name
            path.write_bytes(executable)
            path.chmod(0o755)
        output = self.root / f"release-{version}-{target_os}"
        self.run_command(
            [
                RELEASE,
                "package",
                "--input-dir",
                self.binaries,
                "--output-dir",
                output,
                "--version",
                version,
                "--target-os",
                target_os,
                "--target-arch",
                native_arch,
                "--source-date-epoch",
                "1700000000",
            ]
        )
        return output / f"wikisync-{version}-{target_os}-{native_arch}.tar.gz"

    def legacy_archive(self, source: Path) -> Path:
        destination_directory = self.root / "legacy"
        destination_directory.mkdir(exist_ok=True)
        destination = destination_directory / source.name
        excluded_suffixes = {
            "/docs/security/linux-package-repository-trust.md",
            "/docs/security/macos-signing-notarization.md",
            "/service/org.wikisync.WikiSyncer-log-maintenance.plist.in",
            "/service/wikisync-log-maintenance.sh",
            "/service/wikisync-newsyslog.conf.in",
        }
        with source.open("rb") as raw_source, tarfile.open(
            fileobj=raw_source, mode="r:gz"
        ) as source_archive, destination.open("wb") as raw_destination, gzip.GzipFile(
            filename="", mode="wb", fileobj=raw_destination, mtime=1700000000
        ) as compressed, tarfile.open(
            fileobj=compressed, mode="w", format=tarfile.GNU_FORMAT
        ) as destination_archive:
            for member in source_archive:
                if any(member.name.endswith(suffix) for suffix in excluded_suffixes):
                    continue
                destination_archive.addfile(member, source_archive.extractfile(member))
        return destination

    def test_clean_install_report_is_bounded_and_path_independent(self) -> None:
        archive = self.package("0.2.0")
        first = self.run_command([ASSESS, "--archive", archive, "--timeout", "3"])
        second = self.run_command([ASSESS, "--archive", archive, "--timeout", "3"])
        self.assertEqual(first.stdout, second.stdout)
        report = json.loads(first.stdout)
        self.assertEqual(report["result"], "pass")
        self.assertTrue(report["clean_install"]["fresh_library_initialized"])
        self.assertTrue(report["clean_install"]["daemon_lifecycle"])
        self.assertTrue(report["clean_install"]["service_assets"]["private_modes"])
        self.assertTrue(report["evidence_scope"]["rootless_archive_rehearsal"])
        self.assertFalse(report["evidence_scope"]["clean_system_certification"])
        self.assertFalse(report["evidence_scope"]["service_manager_install_or_enable"])
        self.assertIsNone(report["upgrade"])
        self.assertNotIn(str(self.root), first.stdout)

    def test_previous_candidate_upgrade_preserves_stable_empty_state(self) -> None:
        previous = self.legacy_archive(self.package("0.1.0"))
        candidate = self.package("0.2.0")
        strict = self.run_command(
            [RELEASE, "verify-archive", "--archive", previous], succeed=False
        )
        self.assertIn("missing required files", strict.stderr)
        completed = self.run_command(
            [
                ASSESS,
                "--archive",
                candidate,
                "--previous-archive",
                previous,
                "--timeout",
                "3",
            ]
        )
        report = json.loads(completed.stdout)
        self.assertEqual(report["previous_candidate"]["version"], "0.1.0")
        self.assertEqual(
            report["previous_candidate"]["layout_policy"],
            "bounded-legacy-upgrade-input-v1",
        )
        self.assertTrue(report["upgrade"]["logical_state_preserved"])
        self.assertTrue(report["upgrade"]["daemon_after_upgrade"])

    def test_non_native_candidate_is_rejected_before_execution(self) -> None:
        native_os, _ = native_target()
        other_os = "linux" if native_os == "macos" else "macos"
        archive = self.package("0.2.0", other_os)
        completed = self.run_command(
            [ASSESS, "--archive", archive, "--timeout", "1"], succeed=False
        )
        self.assertIn("but this host is", completed.stderr)

    def test_report_is_create_new_and_private(self) -> None:
        archive = self.package("0.2.0")
        report_path = self.root / "assessment.json"
        self.run_command([ASSESS, "--archive", archive, "--output", report_path, "--timeout", "3"])
        self.assertEqual(report_path.stat().st_mode & 0o777, 0o600)
        repeated = self.run_command(
            [ASSESS, "--archive", archive, "--output", report_path, "--timeout", "3"],
            succeed=False,
        )
        self.assertIn("refusing to overwrite", repeated.stderr)


if __name__ == "__main__":
    unittest.main()
