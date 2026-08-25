# Milestone 0 API and disk characterization

This record closes the historical Milestone 0 benchmark artifact with reproducible,
fixture-backed evidence. It is a characterization of bounded behavior, not a release
performance promise. Routine runs use no Wikimedia or other external service.

## Environment

- Date: 2026-08-25
- Revision: `387b8d0`
- Host: macOS 27.0, arm64
- Toolchain used for this rerun: `rustc 1.95.0`, `cargo 1.95.0`
- Build profile: default test profile, warmed build cache

## Bounded 100-title API spike

Command:

```sh
/usr/bin/time -p cargo test -p wikisync-mediawiki --test client \
  one_hundred_title_spike_is_split_into_bounded_requests
```

Observed wall time was 0.27 seconds (`user 0.05`, `sys 0.07`). The loopback fixture
test resolved a 100-title input as exactly two requests of 50 titles, and checked that
neither request crossed its batch boundary. Process and test-harness startup dominate
this small local timing; the durable conclusion is the explicit request bound and the
absence of a one-request-per-title pattern.

## Loose-object and pack characterization

Command:

```sh
/usr/bin/time -p cargo test -p wikisync-store \
  pack_tuning_groups_page_history_and_reduces_fixture_storage
```

Observed wall time was 1.61 seconds (`user 0.29`, `sys 0.27`). The fixture creates 20
pages with five revisions each, for 100 logical wikitext objects. The test requires at
least one complete anchor per page, at least 75 bounded delta entries, and total pack
plus index bytes below half of the verified loose compressed bytes. The recorded
stable-v1 tuning run for this same fixture was 820,100 loose compressed bytes versus
222,563 pack-plus-index bytes (27.1%). The rerun passed the invariant and verified that
physical pack ordering keeps each page history contiguous.

Compaction is intentionally outside capture's critical path. These figures therefore
support the storage-model decision—durable loose capture first, verified pack tuning
later—without establishing a synchronization latency target.

## Interpretation and follow-up

- The Action API client has an enforced title batch size and fixture evidence at the
  planned 100-title spike size.
- Logical object identity is independent of loose/full/delta representation, and the
  representative revision fixture demonstrates useful bounded compaction.
- Results are machine- and fixture-specific. Before changing the search engine,
  concurrency defaults, dump strategy, or pack heuristics, rerun purpose-built release
  benchmarks at the 10,000-page validation target and record peak memory, source bytes,
  wall time, database size, and query latency on both supported platforms.
