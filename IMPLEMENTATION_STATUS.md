# WikiSyncer implementation status

Updated: 2026-08-21

## Completed backlog items

1. Cargo workspace, formatting and lint policy, macOS/Linux CI, and dependency
   license/source policy.
2. Validated core identities, explicit-title and category collection rules, and all
   planned history-policy variants.
3. Bounded MediaWiki Action API client, 100-title batching spike, resumable revision
   metadata pagination, retry classification, and offline loopback fixture harness.
4. SQLite migration 1 and a bounded atomic loose-object store with domain-separated
   BLAKE3 identities, Zstandard compression, durable installation, deduplication, and
   verified reads. Logical content identity is isolated from physical locations.
5. Explicit-title resolution and current-revision capture, including exact revision
   source retrieval, content-model/size/MediaWiki SHA-1 validation, schema migration
   2 for wiki/page/revision/collection metadata, missing-title observations, and
   idempotent durable capture.
6. Deterministic normalized plain text and minimal Markdown with output-specific
   transformer versions, conservative structural/inline wikitext handling, readable
   placeholders for unsupported templates, malformed-input bounds, and golden article
   fixtures.
7. Contentless SQLite FTS5 indexing for selected current revisions with separately
   weighted title, alias, heading, body, category, and caption fields; atomic derived
   index replacement during capture; wiki filters and bounded ranked queries; and CLI
   `search`/`show` commands with human-readable or JSON output and exact-source mode.
8. Bounded complete-history enumeration with opaque MediaWiki continuation, reuse of
   already captured revision bodies, immutable metadata consistency checks, and
   durable historical inserts that do not move the current page head or search index;
   newest-first local history queries; deterministic exact-source and normalized
   reading diffs with line alignment and word-level spans; and CLI `history`, `diff`,
   and `show --revision` commands with human-readable or JSON output.
9. Durable, idempotent synchronization runs and jobs with crash recovery of claimed
   and retryable-failed work; structured error history and aggregate job counts;
   whole-source and collection-scoped RecentChanges checkpoints with configurable
   overlap windows; checkpoint advancement gated on successful durable jobs; and CLI
   `status` output in human-readable and JSON forms.
10. A loopback-only Axum reader with home, search, article, history, revision, diff,
    changes, collections, and source/integrity routes; bundled styling, restrictive
    Content Security Policy and response headers, sanitized derived HTML, same-wiki
    local article links, provenance notices, CLI `serve`, and an in-process crawl
    proving every loaded resource remains local or embedded.
11. An end-to-end offline vertical-slice test that captures a current revision and its
    complete fixture history, shuts down the MediaWiki source, proves that it is no
    longer reachable, and then exercises the real CLI search, current and historical
    show, history, and reading-diff commands plus the local reader from only the
    durable library.
12. Verified, restartable immutable packfiles with separate checksummed indexes;
    bounded full and prefix/suffix-delta entries; maximum reconstruction depth and
    pack-input limits; hash verification before atomic SQLite activation; transparent
    packed reads with loose-copy fallback; safe loose pruning; tamper detection for
    pack payloads, indexes, and database pointers; and repacking that retains the old
    generation until every replacement object is verified.
13. Category recursion, long-gap recovery, and the Iced GUI through the Milestone 3
    gate, including:

    - bounded deterministic title/category previews and explicit schema-backed commit;
      durable inclusion reasons, title normalization, retained-history category
      removal, estimates, and hard page/canonical-byte budgets;
    - policy-aware, resumable per-page bootstrap for current-and-future, last-N,
      since-date, and complete history plus stable-page-ID long-gap reconciliation
      that captures every intermediate revision before advancing its checkpoint;
    - an Iced workflow for privacy-aware library setup, title-list/category selection,
      preview and estimates, history and budget configuration, create-and-sync,
      per-collection update, synchronization status, full logical-object verification,
      and managed offline-reader launch;
    - a responsive offline-only reader with semantic references, accessible tables and
      diffs, article/history/revision navigation, accurate capture/integrity language,
      and graceful ephemeral loopback lifecycle; and
    - a fixture-backed headless gate test that creates and synchronizes a collection,
      captures a multi-revision update, verifies the complete logical-object catalog,
      and starts/stops the reader without using the CLI.

## Milestone 3 gate

Passed on 2026-08-21. A nontechnical user can create, preview, synchronize, browse,
verify, and update a collection from Iced without using the CLI. Routine tests remain
offline and fixture-backed. Scheduling and the remaining daemon hardening are
Milestone 4 work; optional media and periodic dynamic-category reconciliation remain
stable-v1 work as specified by the plan.

## Milestone 4 progress

The first daemon checkpoint is implemented and validated:

- a versioned, bounded local Unix-socket contract with private library-local daemon
  and cooperative writer-lease sockets;
- race-safe writer discovery for GUI/CLI callers, with fail-closed handling of stale
  or unexpected socket paths;
- a single-threaded application dispatcher for collection reconciliation, bounded
  quick/full logical-object verification, and immutable-object compaction;
- CLI `sync`, `verify`, and `compact` commands that forward to the daemon when it owns
  the library or hold a short exclusive writer lease when it is absent;
- GUI collection updates using the same forwarding/direct-writer contract, while
  collection creation fails clearly if the daemon owns the writer because that
  mutation is not yet in protocol version 1;
- parameterized launchd and hardened systemd user-service templates, including a
  health-only systemd timer that does not pretend to schedule synchronization; and
- a pre-beta threat model plus service, backup/restore/migration, and redacted manual
  diagnostics documentation.

Focused daemon, CLI, GUI, and CLI-to-daemon forwarding tests pass. This is partial
Milestone 4 behavior, not its gate: interval/daily scheduling, GUI schedule controls,
sleep/wake and cancellation gates, graceful `SIGTERM`/`SIGINT`, safe stale-socket
recovery, signed packages/installers, and an automated `doctor` bundle remain.

## Plan audit notes

The ordered first implementation backlog (items 1–13) is complete. The broader
milestone delivery lists are not all closed: predecessor-linked integrity manifests,
signatures/trusted rollback anchors, full graph/manifest/search verification, the
planned export and administrative CLI surface, source/redirect allowlisting, and the
remaining Milestone 4/5 release work are still outstanding. Full verification at this
checkpoint means a stable scan of every logical canonical object; it must not be
described as manifest-chain or whole-archive trust verification.

## Next checkpoint

Add durable interval/daily schedule configuration with jitter and GUI controls, then
exercise restart and sleep/wake recovery through the real daemon. Add graceful Unix
signal handling before treating launchd termination as cooperative shutdown.

Milestone gates remain tracked in `IMPLEMENTATION_PLAN.md`; an item being complete
means its initial implementation is present, not that its later milestone hardening
is finished.
