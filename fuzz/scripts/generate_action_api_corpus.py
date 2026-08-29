#!/usr/bin/env python3
"""Generate deterministic production-scale Action API fuzz inputs.

The checked-in corpus stays deliberately small. This generator derives from its
valid response seeds and materializes large inputs only for a campaign that needs
to exercise the production response ceiling.
"""

from __future__ import annotations

import argparse
import copy
import hashlib
import json
import os
from pathlib import Path
import sys
import tempfile
from typing import Any


PRODUCTION_BODY_CEILING = 8 * 1024 * 1024
MINIMUM_PADDED_BODY_SIZE = 1024
SCRIPT_PATH = Path(__file__).resolve()
REPOSITORY_ROOT = SCRIPT_PATH.parents[2]
SEED_DIRECTORY = REPOSITORY_ROOT / "fuzz" / "corpus" / "action_api_json"

SEEDS: tuple[tuple[str, bytes, str], ...] = (
    ("title-resolution", b"T", "valid-title-resolution.json"),
    ("page-head", b"H", "valid-page-head.json"),
    ("revision-batch", b"B", "valid-revision-batch.json"),
    ("revision-content", b"C", "valid-revision-content.json"),
    ("category-members", b"M", "valid-category-members.json"),
    ("revision-images", b"I", "valid-revision-images.json"),
    ("thumbnail-metadata", b"N", "valid-thumbnail-metadata.json"),
)

# Every near-ceiling case grows a field consumed by its production decoder, rather
# than relying on an ignored top-level padding member.
LARGE_STRING_PATHS: dict[str, tuple[str | int, ...]] = {
    "title-resolution": ("query", "pages", 0, "title"),
    "page-head": ("query", "pages", 0, "title"),
    "revision-batch": ("query", "pages", 0, "revisions", 0, "comment"),
    "revision-content": (
        "query",
        "pages",
        0,
        "revisions",
        0,
        "slots",
        "main",
        "content",
    ),
    "category-members": ("query", "categorymembers", 0, "title"),
    "revision-images": ("parse", "images", 0),
    "thumbnail-metadata": (
        "query",
        "pages",
        0,
        "imageinfo",
        0,
        "extmetadata",
        "Artist",
        "value",
    ),
}


def compact_json(value: Any) -> bytes:
    return json.dumps(
        value,
        ensure_ascii=True,
        separators=(",", ":"),
        sort_keys=False,
    ).encode("ascii")


def load_seed(selector: bytes, filename: str) -> dict[str, Any]:
    data = (SEED_DIRECTORY / filename).read_bytes()
    if not data.startswith(selector):
        raise ValueError(f"{filename} does not start with selector {selector!r}")
    parsed = json.loads(data[1:])
    if not isinstance(parsed, dict):
        raise ValueError(f"{filename} is not a top-level JSON object")
    return parsed


def grow_string_field_to_size(
    value: dict[str, Any], path: tuple[str | int, ...], body_size: int
) -> bytes:
    """Grow one consumed string field so the encoded body is exactly body_size."""

    result = copy.deepcopy(value)
    cursor: Any = result
    try:
        for component in path[:-1]:
            cursor = cursor[component]
        final_component = path[-1]
        if not isinstance(cursor[final_component], str):
            raise ValueError("selected large-response field is not a string")
        cursor[final_component] = ""
    except (IndexError, KeyError, TypeError) as error:
        raise ValueError(f"seed does not contain large-response path {path!r}") from error

    encoded = compact_json(result)
    padding_size = body_size - len(encoded)
    if padding_size < 0:
        raise ValueError(
            f"requested body size {body_size} is smaller than a structured seed"
        )
    cursor[final_component] = "x" * padding_size
    body = compact_json(result)
    if len(body) != body_size:
        raise AssertionError("padding calculation did not reach the requested size")
    return body


def maximum_title_pages(seed: dict[str, Any]) -> dict[str, Any]:
    result = copy.deepcopy(seed)
    template = result["query"]["pages"][0]
    pages = []
    for index in range(50):
        page = copy.deepcopy(template)
        page["pageid"] = 42 + index
        page["title"] = f"Fuzz title {index:02d}"
        page["revisions"][0]["revid"] = 100 + index
        pages.append(page)
    result["query"]["pages"] = pages
    return result


def maximum_revision_batch(seed: dict[str, Any]) -> dict[str, Any]:
    result = copy.deepcopy(seed)
    template = result["query"]["pages"][0]["revisions"][0]
    revisions = []
    for index in range(500):
        revision = copy.deepcopy(template)
        revision["revid"] = 100 + index
        revision["parentid"] = 0 if index == 0 else 99 + index
        revision["comment"] = f"Deterministic fuzz revision {index:03d}"
        revisions.append(revision)
    result["query"]["pages"][0]["revisions"] = revisions
    return result


def maximum_category_members(seed: dict[str, Any]) -> dict[str, Any]:
    result = copy.deepcopy(seed)
    template = result["query"]["categorymembers"][0]
    members = []
    for index in range(500):
        member = copy.deepcopy(template)
        member["pageid"] = 42 + index
        member["title"] = f"Fuzz category member {index:03d}"
        members.append(member)
    result["query"]["categorymembers"] = members
    return result


def maximum_revision_images(seed: dict[str, Any]) -> dict[str, Any]:
    result = copy.deepcopy(seed)
    result["parse"]["images"] = [f"Fuzz-image-{index:04d}.png" for index in range(4096)]
    return result


def generated_cases(body_size: int) -> dict[str, bytes]:
    parsed = {
        name: (selector, load_seed(selector, filename))
        for name, selector, filename in SEEDS
    }
    cases = {
        f"near-ceiling-{name}.input": selector
        + grow_string_field_to_size(seed, LARGE_STRING_PATHS[name], body_size)
        for name, (selector, seed) in parsed.items()
    }
    cases.update(
        {
            "maximum-title-pages.input": b"T"
            + compact_json(maximum_title_pages(parsed["title-resolution"][1])),
            "maximum-revision-batch.input": b"B"
            + compact_json(maximum_revision_batch(parsed["revision-batch"][1])),
            "maximum-category-members.input": b"M"
            + compact_json(maximum_category_members(parsed["category-members"][1])),
            "maximum-revision-images.input": b"I"
            + compact_json(maximum_revision_images(parsed["revision-images"][1])),
        }
    )
    return cases


def write_cases(output: Path, cases: dict[str, bytes]) -> None:
    output.mkdir(parents=True, exist_ok=True)
    if output.is_symlink() or not output.is_dir():
        raise ValueError("output path must be a real directory, not a symlink")
    expected = set(cases)
    entries = list(output.iterdir())
    unexpected = {entry.name for entry in entries} - expected
    if unexpected:
        names = ", ".join(sorted(unexpected))
        raise ValueError(
            f"output directory contains non-generator entries ({names}); use an empty directory"
        )
    unsafe = [
        entry.name for entry in entries if entry.is_symlink() or not entry.is_file()
    ]
    if unsafe:
        names = ", ".join(sorted(unsafe))
        raise ValueError(
            f"output directory contains non-regular generated entries ({names})"
        )
    for filename, data in sorted(cases.items()):
        file_descriptor, temporary_name = tempfile.mkstemp(
            dir=output, prefix=f".{filename}."
        )
        temporary_path = Path(temporary_name)
        try:
            with os.fdopen(file_descriptor, "wb") as temporary_file:
                temporary_file.write(data)
            temporary_path.replace(output / filename)
        except BaseException:
            temporary_path.unlink(missing_ok=True)
            raise


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "--output",
        type=Path,
        required=True,
        help="empty or previously generated corpus directory",
    )
    parser.add_argument(
        "--body-size",
        type=int,
        default=PRODUCTION_BODY_CEILING,
        help=(
            "exact body size for each large structured response "
            f"(default and maximum: {PRODUCTION_BODY_CEILING})"
        ),
    )
    parser.add_argument("--quiet", action="store_true", help="suppress the digest listing")
    return parser.parse_args()


def main() -> int:
    args = parse_args()
    if not MINIMUM_PADDED_BODY_SIZE <= args.body_size <= PRODUCTION_BODY_CEILING:
        print(
            "body size must be between "
            f"{MINIMUM_PADDED_BODY_SIZE} and {PRODUCTION_BODY_CEILING} bytes",
            file=sys.stderr,
        )
        return 2
    try:
        cases = generated_cases(args.body_size)
        write_cases(args.output, cases)
    except (OSError, ValueError, json.JSONDecodeError) as error:
        print(f"failed to generate Action API corpus: {error}", file=sys.stderr)
        return 1

    if not args.quiet:
        for filename, data in sorted(cases.items()):
            digest = hashlib.sha256(data).hexdigest()
            print(f"{digest}  {len(data):>8}  {filename}")
        print(f"generated {len(cases)} inputs in {args.output}")
        maximum_input_size = max(map(len, cases.values()))
        print(f"libFuzzer -max_len must be at least {maximum_input_size}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
