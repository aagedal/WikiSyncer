from __future__ import annotations

from pathlib import Path
import re
import unittest


REPO_ROOT = Path(__file__).resolve().parents[2]
WORKFLOW = REPO_ROOT / ".github" / "workflows" / "release.yml"
UPLOAD_ARTIFACT_V4_6_2 = "ea165f8d65b6e75b540449e92b4886f43607fa02"


class ReleaseWorkflowTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls) -> None:
        cls.source = WORKFLOW.read_text(encoding="utf-8")

    def test_native_ubuntu_candidate_is_bound_to_its_evidence(self) -> None:
        source = self.source
        self.assertIn("runner: ubuntu-24.04", source)
        self.assertIn('echo "RELEASE_CANDIDATE_ARCHIVE=$archive" >> "$GITHUB_ENV"', source)
        self.assertIn('echo "RELEASE_CANDIDATE_SHA256=$archive_sha256" >> "$GITHUB_ENV"', source)
        self.assertIn(
            'test "$observed_candidate_sha256" = "$RELEASE_CANDIDATE_SHA256"',
            source,
        )
        self.assertIn('- Candidate commit: \\`$GITHUB_SHA\\`', source)
        self.assertIn('- Candidate archive: \\`$(basename "$RELEASE_CANDIDATE_ARCHIVE")\\`', source)
        self.assertIn('- Candidate archive SHA-256: \\`$RELEASE_CANDIDATE_SHA256\\`', source)
        self.assertIn("python3 packaging/scripts/assess_install.py", source)
        self.assertIn(
            "--output target/release-candidate/install-assessment.json",
            source,
        )
        self.assertIn("cat target/release-candidate/install-assessment.json", source)

        build = source.index("- name: Build and verify deterministic release candidate")
        evidence = source.index("- name: Record successful native evidence")
        preserve = source.index("- name: Preserve candidate-bound native evidence")
        self.assertLess(build, evidence)
        self.assertLess(evidence, preserve)

    def test_candidate_and_evidence_are_retained_without_release_authority(self) -> None:
        source = self.source
        self.assertIn(f"uses: actions/upload-artifact@{UPLOAD_ARTIFACT_V4_6_2}", source)
        self.assertIn("path: target/release-candidate/", source)
        self.assertIn("if-no-files-found: error", source)
        self.assertIn("retention-days: 30", source)
        self.assertIn("compression-level: 0", source)
        self.assertIn("overwrite: false", source)
        self.assertIn("include-hidden-files: false", source)
        self.assertRegex(
            source,
            re.compile(
                r"name: wikisync-\$\{\{ matrix\.target_os \}\}-native-candidate-"
                r"\$\{\{ github\.sha \}\}-\$\{\{ github\.run_attempt \}\}"
            ),
        )
        self.assertIn(
            "EVIDENCE_ARTIFACT_DIGEST: "
            "${{ steps.preserve-native-evidence.outputs.artifact-digest }}",
            source,
        )
        self.assertIn("permissions:\n  contents: read", source)
        self.assertIn("persist-credentials: false", source)
        self.assertNotIn("contents: write", source)
        self.assertNotRegex(source, re.compile(r"^\s*release:\s*$", re.MULTILINE))


if __name__ == "__main__":
    unittest.main()
