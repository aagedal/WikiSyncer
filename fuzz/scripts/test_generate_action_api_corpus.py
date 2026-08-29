#!/usr/bin/env python3

from __future__ import annotations

import hashlib
import json
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest


SCRIPT = Path(__file__).with_name("generate_action_api_corpus.py")
TEST_BODY_SIZE = 4096


def strings_in(value: object) -> list[str]:
    if isinstance(value, str):
        return [value]
    if isinstance(value, list):
        return [item for child in value for item in strings_in(child)]
    if isinstance(value, dict):
        return [item for child in value.values() for item in strings_in(child)]
    return []


class GenerateActionApiCorpusTests(unittest.TestCase):
    def generate(
        self, output: Path, body_size: int = TEST_BODY_SIZE
    ) -> subprocess.CompletedProcess[str]:
        return subprocess.run(
            [
                sys.executable,
                str(SCRIPT),
                "--output",
                str(output),
                "--body-size",
                str(body_size),
                "--quiet",
            ],
            check=False,
            capture_output=True,
            text=True,
        )

    def test_generates_all_kinds_at_exact_size_and_structural_maxima(self) -> None:
        with tempfile.TemporaryDirectory() as temporary_directory:
            output = Path(temporary_directory) / "corpus"
            result = self.generate(output)
            self.assertEqual(result.returncode, 0, result.stderr)

            files = {path.name: path.read_bytes() for path in output.iterdir()}
            near_ceiling = {
                name: data for name, data in files.items() if name.startswith("near-ceiling-")
            }
            self.assertEqual(len(files), 11)
            self.assertEqual(len(near_ceiling), 7)
            self.assertEqual(
                {data[:1] for data in near_ceiling.values()},
                {bytes([selector]) for selector in b"THBCMIN"},
            )
            for data in near_ceiling.values():
                self.assertEqual(len(data), TEST_BODY_SIZE + 1)
                payload = json.loads(data[1:])
                self.assertGreater(max(map(len, strings_in(payload))), TEST_BODY_SIZE // 2)

            title_pages = json.loads(files["maximum-title-pages.input"][1:])
            revision_batch = json.loads(files["maximum-revision-batch.input"][1:])
            category_members = json.loads(files["maximum-category-members.input"][1:])
            revision_images = json.loads(files["maximum-revision-images.input"][1:])
            self.assertEqual(len(title_pages["query"]["pages"]), 50)
            self.assertEqual(len(revision_batch["query"]["pages"][0]["revisions"]), 500)
            self.assertEqual(len(category_members["query"]["categorymembers"]), 500)
            self.assertEqual(len(revision_images["parse"]["images"]), 4096)

    def test_generation_is_reproducible(self) -> None:
        with tempfile.TemporaryDirectory() as temporary_directory:
            first = Path(temporary_directory) / "first"
            second = Path(temporary_directory) / "second"
            self.assertEqual(self.generate(first).returncode, 0)
            self.assertEqual(self.generate(second).returncode, 0)
            first_hashes = {
                path.name: hashlib.sha256(path.read_bytes()).digest()
                for path in first.iterdir()
            }
            second_hashes = {
                path.name: hashlib.sha256(path.read_bytes()).digest()
                for path in second.iterdir()
            }
            self.assertEqual(first_hashes, second_hashes)

    def test_rejects_a_body_above_the_production_ceiling(self) -> None:
        with tempfile.TemporaryDirectory() as temporary_directory:
            result = self.generate(Path(temporary_directory) / "corpus", 8 * 1024 * 1024 + 1)
            self.assertEqual(result.returncode, 2)
            self.assertIn("body size must be between", result.stderr)

    def test_rejects_expected_names_that_are_not_regular_files(self) -> None:
        with tempfile.TemporaryDirectory() as temporary_directory:
            root = Path(temporary_directory)
            for entry_kind in ("symlink", "directory"):
                with self.subTest(entry_kind=entry_kind):
                    output = root / entry_kind
                    output.mkdir()
                    named_entry = output / "near-ceiling-title-resolution.input"
                    if entry_kind == "symlink":
                        victim = root / "victim"
                        victim.write_text("must remain unchanged")
                        named_entry.symlink_to(victim)
                    else:
                        named_entry.mkdir()

                    result = self.generate(output)
                    self.assertEqual(result.returncode, 1)
                    self.assertIn("non-regular generated entries", result.stderr)
                    if entry_kind == "symlink":
                        self.assertEqual(victim.read_text(), "must remain unchanged")

    def test_rejects_a_symlink_output_directory(self) -> None:
        with tempfile.TemporaryDirectory() as temporary_directory:
            root = Path(temporary_directory)
            actual_output = root / "actual"
            actual_output.mkdir()
            output_symlink = root / "corpus"
            output_symlink.symlink_to(actual_output, target_is_directory=True)

            result = self.generate(output_symlink)
            self.assertEqual(result.returncode, 1)
            self.assertIn("output path must be a real directory", result.stderr)
            self.assertEqual(list(actual_output.iterdir()), [])


if __name__ == "__main__":
    unittest.main()
