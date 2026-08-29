# Release acceptance matrix

This matrix records platform evidence for a specific candidate revision. A workflow
definition is not a passing result: each platform cell must name the candidate,
native environment, command or CI run, and outcome. Credential-free checks cannot be
used as evidence for Developer ID, notarization, repository signing, clean-system
installation, or publication.

Candidate under review: working tree based on `03e14aa` (2026-08-29).

| Evidence | macOS arm64 | Ubuntu x86_64 | Stable-v1 requirement |
| --- | --- | --- | --- |
| Format, warning-denied Clippy, workspace tests | Pass: local macOS 27.0 arm64 integrated run, 2026-08-29 | Pending native CI result for this candidate | Required |
| Representative multi-language GUI/daemon lifecycle | Pass: `en` direct writer plus `nb` daemon writer in full workspace run | Pending native CI result | Required |
| Older beta schema-11 whole-library migration fixture | Pass in full workspace run | Pending native CI result | Required |
| Release-candidate archive and layout verification | Pass: 26 credential-free packaging/service-policy tests executed; native-Linux systemd test skipped as designed | Pending native CI result; the release job verifies ELF architecture, systemd units, checksum, and layout | Required |
| Release-mode CLI, daemon, and reader outbound audit | Pass: CLI, idle daemon/IPC, and six reader routes; zero outbound attempts | Pending native CI result | Required |
| Packaged GUI default-launch outbound audit | Pass: bounded no-action launch in an Aqua session; zero outbound attempts | Pending native graphical Ubuntu result; a headless pass must remain explicitly incomplete | Required |
| Maintained fuzz targets build and bounded smoke corpus | Pass: six bins compile; all six maintained corpora completed bounded direct smoke runs, with the new Action API target rechecked after integration | Pending native Ubuntu result | Required before release |
| Sustained instrumented fuzz campaigns | Partial: five clean 60-second target runs are [recorded](../benchmarks/fuzz-campaign-2026-08-25.md), and the sixth Action API target has a clean [61-second follow-up](../benchmarks/fuzz-campaign-2026-08-29-action-api-json.md); longer campaigns remain | Not recorded | Required before release; bounded runs do not close this row |
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
cargo build --workspace --bins --release --locked
python3 scripts/release_offline_audit.py --skip-build
```

On native Ubuntu x86_64, also run the candidate-format and service-template checks:

```sh
python3 packaging/scripts/release.py verify-linux-binaries \
  --input-dir target/release --target-arch x86_64
python3 packaging/scripts/release.py verify-systemd-units
```

`.github/workflows/release.yml` runs this credential-free sequence on a pinned
`ubuntu-24.04` runner. After every preceding command and candidate archive check
succeeds, it writes a job summary containing the commit, clean-tree check, runner and
OS identity, exact Rust tools, `Cargo.lock` SHA-256, timestamps, immutable run URL,
offline-audit result, and archive checksum. Retain that summary or equivalent native
output with the release record. The workflow definition alone, a cancelled job, or a
summary from another commit is not evidence.

The offline-audit result must say that the packaged Iced GUI launch was observed to
count as GUI evidence. A successful headless run remains valid evidence for the CLI,
daemon, and reader portions only; retain its explicit “GUI launch not audited” reason
and rerun in a native graphical session before accepting the GUI portion.

Separately build every maintained fuzz target and run the bounded smoke commands documented
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
