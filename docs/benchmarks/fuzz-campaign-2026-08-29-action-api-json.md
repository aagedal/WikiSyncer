# Action API JSON fuzz campaign: 2026-08-29

This is the initial bounded sanitizer campaign for the direct untrusted MediaWiki
Action API JSON boundary added after the five-target campaign recorded on 2026-08-25.
It is resource-sizing and regression evidence, not the sustained multi-hour macOS and
Ubuntu evidence required for a stable release.

## Candidate and environment

- base commit: `03e14aacdeaea9552b378bd148ef194304bc1f47`
- tree state: modified by the Action API robustness work in progress
- host: macOS 27.0 (`26A5421a`), Darwin 27.0.0, arm64
- default Rust: `rustc 1.95.0 (59807616e 2026-04-14)`
- fuzz Rust: `cargo 1.100.0-nightly (e8cb624d5 2026-08-22)`
- cargo-fuzz: `0.13.2`
- `Cargo.lock` SHA-256: `1b92b28675f821247f170831e99a2373669f731d543297071ea83926af368c34`
- `fuzz/Cargo.lock` SHA-256: `4094cbf0310bb72c0f1d26de70623f1861e3542886f4906adfeab7e05414fc28`
- Action API seed-corpus aggregate SHA-256: `30708c0559e564f2fb5b2d58e3525ee9022738e50950b41bdc04b9de387c8c54`
- all maintained seed-corpora aggregate SHA-256: `a110bf56379daa96cfa3384ef6c5af4060c3bb378b13a094ec51da2add52c3f3`
- completed: `2026-08-29T17:20:33+0200`

The 13 maintained seeds cover all seven production response shapes plus structured
API error, empty, short, malformed, nested, Unicode, and incomplete inputs. The
campaign copied those seeds to `/private/tmp` and used a separate artifact directory,
so discovered coverage did not mutate the repository corpus.

## Command and outcome

```sh
cargo +nightly fuzz run action_api_json <copied-corpus> -- \
  -max_total_time=60 -timeout=20 -rss_limit_mb=2048 -max_len=262144 \
  -artifact_prefix=<isolated-artifacts>/
```

| Target | Executions | Final coverage/features | Final generated corpus | Peak RSS | Outcome |
| --- | ---: | ---: | ---: | ---: | --- |
| `action_api_json` | 875,058 | 2,713 / 5,572 | 1,597 / 139 KiB | 667 MiB | exit 0; no crash, timeout, OOM, or artifact |

The harness uses a 256 KiB campaign maximum while its feature-gated decoder accepts
the default production 8 MiB response ceiling. The decoder shares the production
operation validators for title resolution, stable page heads, revision metadata and
content, category members, revision image discovery, and thumbnail attribution.
Deserialization additionally rejects response cardinalities above 50 pages, 500
revisions, 500 category members, 4,096 raw image references, or the single-result
image-info contract before accepting those collections.

Nightly Rust emitted a deprecation warning for `AtomicUsize::fetch_update`; stable Rust
1.95 does not warn and the workspace's Rust 1.85 compatibility floor precludes using
the newer spelling unconditionally. The macOS external symbolizer again failed to
start reliably. Sanitizer instrumentation remained active and the run exited cleanly,
but a release campaign should repair symbolization before longer runs.

## Remaining release evidence

Run this target for sustained durations independently on native macOS and Ubuntu,
including inputs between the initial 256 KiB campaign cap and the 8 MiB production
boundary. Retain exact clean candidate, corpus, CPU/RSS/disk, symbolizer, and artifact
records. The other five targets' earlier bounded outcomes remain in
[`fuzz-campaign-2026-08-25.md`](fuzz-campaign-2026-08-25.md); neither short campaign
substitutes for the stable-release sustained fuzz gate.
