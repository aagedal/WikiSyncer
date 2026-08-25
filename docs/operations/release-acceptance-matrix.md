# Release acceptance matrix

This matrix records platform evidence for a specific candidate revision. A workflow
definition is not a passing result: each platform cell must name the candidate,
native environment, command or CI run, and outcome. Credential-free checks cannot be
used as evidence for Developer ID, notarization, repository signing, clean-system
installation, or publication.

Candidate under review: working tree based on `387b8d0` (2026-08-25).

| Evidence | macOS arm64 | Ubuntu x86_64 | Stable-v1 requirement |
| --- | --- | --- | --- |
| Format, warning-denied Clippy, workspace tests | Pass: local macOS 27.0 arm64 integrated run, 2026-08-25 | Pending native CI result for this candidate | Required |
| Representative multi-language GUI/daemon lifecycle | Pass: `en` direct writer plus `nb` daemon writer in full workspace run | Pending native CI result | Required |
| Older beta schema-11 whole-library migration fixture | Pass in full workspace run | Pending native CI result | Required |
| Release-candidate archive and layout verification | Pass: 24 credential-free packaging and service-policy tests | Pending native CI result | Required |
| Release-mode default-path outbound audit | Pass: CLI, idle daemon/IPC, six reader routes, and bounded packaged-Iced-GUI default launch in an Aqua session; zero outbound attempts | Pending native CI result; a headless run must report the GUI launch as not audited | Required |
| Maintained fuzz targets build and bounded smoke corpus | Pass: five bins compile and each corpus completed a short direct smoke run; `cargo-fuzz` unavailable | Pending native CI/sustained campaign evidence | Required before release; sustained runs remain separate |
| Signed artifact verification with production identity | Not authorized/provisioned | Not authorized/provisioned | Required for a signed beta |
| Native install/service/upgrade on a clean supported host | Not recorded | Not recorded | Required |
| Gatekeeper/notarization assessment | Not authorized/provisioned | Not applicable | Required on macOS |
| Independent public-key/repository trust distribution | Not recorded | Not recorded | Required for published packages |

## Credential-free native commands

Run these from an immutable candidate checkout on each supported platform and attach
the output or CI job URL to the release record:

```sh
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace --locked
python3 -m unittest discover -s packaging/tests -p 'test_*.py'
python3 scripts/release_offline_audit.py
```

The offline-audit result must say that the packaged Iced GUI launch was observed to
count as GUI evidence. A successful headless run remains valid evidence for the CLI,
daemon, and reader portions only; retain its explicit “GUI launch not audited” reason
and rerun in a native graphical session before accepting the GUI portion.

Also build every maintained fuzz target and run the bounded smoke commands documented
in `fuzz/README.md`. Longer sanitizer/fuzz campaigns must record target, seed corpus
identity, toolchain, duration, and final outcome; a short smoke run proves only that
the harness and corpus execute.

## Release-record fields

For every `Pass`, retain:

- exact commit and whether the tree was clean;
- OS release, architecture, Rust toolchain, and lockfile identity;
- command or immutable CI run URL;
- start/end time and exit status;
- candidate archive/checksum identity when packaging is involved; and
- any skipped test or environmental limitation.

Do not promote the candidate while a required cell is pending, failed, or supported
only by a different revision.
