#!/usr/bin/env python3

from __future__ import annotations

import os
import plistlib
from pathlib import Path
import platform
import shutil
import subprocess
import sys
import tempfile
import unittest


REPO_ROOT = Path(__file__).resolve().parents[2]
LAUNCHD = REPO_ROOT / "packaging" / "launchd"
SYSTEMD = REPO_ROOT / "packaging" / "systemd"
OPERATIONS = REPO_ROOT / "docs" / "operations"
RELEASE = REPO_ROOT / "packaging" / "scripts" / "release.py"


class ServiceLogRetentionTests(unittest.TestCase):
    def test_launchd_companion_and_newsyslog_policy_are_bounded(self) -> None:
        companion = plistlib.loads(
            (LAUNCHD / "org.wikisync.WikiSyncer-log-maintenance.plist.in").read_bytes()
        )
        self.assertEqual(companion["Label"], "org.wikisync.WikiSyncer-log-maintenance")
        self.assertEqual(companion["StartInterval"], 3600)
        self.assertEqual(
            companion["ProgramArguments"],
            [
                "/bin/sh",
                "@LOG_MAINTENANCE_SCRIPT@",
                "@WIKISYNCD@",
                "@LIBRARY@",
                "@NEWSYSLOG_CONFIG@",
                "@SERVICE_PLIST@",
                "@UID@",
                "@LOG_DIRECTORY@/wikisyncd.log",
                "@LOG_DIRECTORY@/wikisyncd-error.log",
            ],
        )

        policy_lines = [
            line.split()
            for line in (LAUNCHD / "wikisync-newsyslog.conf.in").read_text().splitlines()
            if line and not line.startswith("#")
        ]
        self.assertEqual(len(policy_lines), 2)
        self.assertEqual(
            {fields[0] for fields in policy_lines},
            {
                "@LOG_DIRECTORY@/wikisyncd.log",
                "@LOG_DIRECTORY@/wikisyncd-error.log",
            },
        )
        for fields in policy_lines:
            self.assertEqual(fields[1:], ["@UID@:@GID@", "600", "4", "10240", "*", "BNZ"])

    @unittest.skipUnless(Path("/usr/sbin/newsyslog").is_file(), "newsyslog unavailable")
    def test_rendered_newsyslog_policy_is_accepted_without_mutation(self) -> None:
        with tempfile.TemporaryDirectory(prefix="wikisync-newsyslog-") as temporary:
            root = Path(temporary)
            stdout_log = root / "wikisyncd.log"
            stderr_log = root / "wikisyncd-error.log"
            stdout_log.touch(mode=0o600)
            stderr_log.touch(mode=0o600)
            rendered = (
                (LAUNCHD / "wikisync-newsyslog.conf.in")
                .read_text(encoding="utf-8")
                .replace("@LOG_DIRECTORY@", str(root))
                .replace("@UID@", str(os.getuid()))
                .replace("@GID@", str(os.getgid()))
            )
            config = root / "newsyslog.conf"
            config.write_text(rendered, encoding="utf-8")
            dry_run = subprocess.run(
                [
                    "/usr/sbin/newsyslog",
                    "-n",
                    "-r",
                    "-s",
                    "-f",
                    str(config),
                    str(stdout_log),
                    str(stderr_log),
                ],
                text=True,
                capture_output=True,
                check=False,
            )
            self.assertEqual(dry_run.returncode, 0, dry_run.stderr)

    def test_launchd_helper_is_syntactically_valid_and_stops_before_rotation(self) -> None:
        helper = LAUNCHD / "wikisync-log-maintenance.sh"
        syntax = subprocess.run(
            ["/bin/sh", "-n", str(helper)], text=True, capture_output=True, check=False
        )
        self.assertEqual(syntax.returncode, 0, syntax.stderr)
        source = helper.read_text(encoding="utf-8")
        bootout = source.index('/bin/launchctl bootout "$domain/$label"')
        wait = source.index('while "$daemon" --library "$library" status', bootout)
        rotate = source.index('/usr/sbin/newsyslog -r -s -f', wait)
        bootstrap = source.index('/bin/launchctl bootstrap "$domain" "$service_plist"', rotate)
        self.assertLess(bootout, wait)
        self.assertLess(wait, rotate)
        self.assertLess(rotate, bootstrap)
        self.assertIn("trap restore_service EXIT", source)
        self.assertIn("trap 'exit 75' HUP INT TERM", source)
        self.assertIn("rotation_bytes=10485760", source)

        with tempfile.TemporaryDirectory(prefix="wikisync-log-policy-") as temporary:
            root = Path(temporary)
            config = root / "newsyslog.conf"
            service = root / "service.plist"
            config.write_text("# fixture\n", encoding="utf-8")
            service.write_text("fixture\n", encoding="utf-8")
            below_threshold = subprocess.run(
                [
                    "/bin/sh",
                    str(helper),
                    str(root / "wikisyncd"),
                    str(root / "library"),
                    str(config),
                    str(service),
                    "501",
                    str(root / "stdout.log"),
                    str(root / "stderr.log"),
                ],
                text=True,
                capture_output=True,
                check=False,
            )
            self.assertEqual(below_threshold.returncode, 0, below_threshold.stderr)

    def test_systemd_uses_journal_rate_limits_without_claiming_a_unit_quota(self) -> None:
        for name, burst in (("wikisyncd.service.in", "1000"), ("wikisyncd-health.service.in", "100")):
            source = (SYSTEMD / name).read_text(encoding="utf-8")
            self.assertIn("StandardOutput=journal", source)
            self.assertIn("StandardError=journal", source)
            self.assertIn("LogRateLimitIntervalSec=30s", source)
            self.assertIn(f"LogRateLimitBurst={burst}", source)
            for global_setting in ("SystemMaxUse=", "RuntimeMaxUse=", "MaxRetentionSec="):
                self.assertNotIn(global_setting, source)

        management = (OPERATIONS / "service-management.md").read_text(encoding="utf-8")
        diagnostics = (OPERATIONS / "diagnostics.md").read_text(encoding="utf-8")
        for setting in ("SystemMaxUse", "RuntimeMaxUse", "MaxRetentionSec"):
            self.assertIn(setting, management)
            self.assertIn(setting, diagnostics)
        self.assertIn("whole system journal", management)
        self.assertIn("administrator", management)

    @unittest.skipUnless(
        platform.system() == "Linux" and shutil.which("systemd-analyze") is not None,
        "native systemd-analyze verification is Linux-only",
    )
    def test_systemd_user_units_pass_native_verification(self) -> None:
        completed = subprocess.run(
            [sys.executable, str(RELEASE), "verify-systemd-units", "--repo-root", str(REPO_ROOT)],
            text=True,
            capture_output=True,
            check=False,
        )
        self.assertEqual(completed.returncode, 0, completed.stdout + completed.stderr)
        self.assertIn("native systemd-analyze verification passed", completed.stdout)


if __name__ == "__main__":
    unittest.main()
