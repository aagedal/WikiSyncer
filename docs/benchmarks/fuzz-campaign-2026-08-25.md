# Instrumented fuzz campaign: 2026-08-25

This is a bounded local sanitizer campaign, not the sustained multi-hour evidence
required for a stable release. It proves that every maintained target builds and ran
cleanly under libFuzzer/AddressSanitizer on the current macOS candidate, while recording
enough resource data to size later native campaigns.

## Candidate and environment

- base commit: `69389a18c36ae228806ff94353b48fa821365fbc`
- tree state: modified by the release-acceptance work in progress
- host: macOS Darwin 27.0.0, arm64
- default Rust: `rustc 1.95.0 (59807616e 2026-04-14)`
- fuzz Rust: `rustc 1.100.0-nightly (e7769602a 2026-08-24)`
- cargo-fuzz: `0.13.2`
- maintained seed-corpus aggregate SHA-256: `ad3a43fb142824a71d78c3e7e14917207cc1c1f1e023f7c5475f5b62280f6fbd`
- completed: `2026-08-25T17:48:05+0200`

The commands copied each maintained corpus to `/private/tmp` before running, so new
coverage inputs did not mutate the repository. Each target used a 60-second limit, a
20-second per-input timeout, a 2 GiB RSS ceiling, its documented maximum input length,
and an isolated artifact directory.

## Outcomes

| Target | Executions | Final coverage/features | Final corpus | Peak RSS | Outcome |
| --- | ---: | ---: | ---: | ---: | --- |
| `wikitext_markdown` | 73,148 | 2,607 / 12,169 | 1,754 / 190 KiB | 798 MiB | exit 0; no crash, timeout, OOM, or artifact |
| `trusted_head_json` | 702,975 | 1,250 / 2,895 | 1,004 / 69 KiB | 531 MiB | exit 0; no crash, timeout, OOM, or artifact |
| `dump_parser` | 499,154 | 1,034 / 3,170 | 367 / 30 KiB | 187 MiB | exit 0; no crash, timeout, OOM, or artifact |
| `loose_object` | 1,143 | 1,197 / 1,328 | 9 / 171 bytes | 288 MiB | exit 0; no crash, timeout, OOM, or artifact |
| `pack_roundtrip` | 381 | 2,097 / 2,324 | 41 / 1,669 bytes | 152 MiB | exit 0; no crash, timeout, OOM, or artifact |

The macOS external symbolizer did not start reliably, so libFuzzer printed raw binary
offsets while discovering coverage. This did not disable sanitizer instrumentation or
change the clean exit outcomes, but a release campaign should fix symbolizer startup
so any future finding has immediately useful stack symbols.

## Remaining release evidence

Run longer campaigns independently per target on native macOS and Ubuntu runners,
retain their exact candidate and corpus identities, and record wall-clock/CPU/RSS/disk
outcomes plus every artifact. Storage targets are intentionally much slower than the
pure parser/transform targets and should use separately sized campaign durations.

The implementation plan also asks for fuzz/property coverage of untrusted Action API
inputs. Malformed Action API responses currently have fixture-backed integration tests,
but there is no direct Action API JSON fuzz target because the response decoder is a
private client boundary. Closing that gap requires a deliberately bounded public test
harness or an in-crate property target; it must not make a live source part of routine
fuzzing.
